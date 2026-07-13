use crate::adapter::adapters::support::get_api_key;
use crate::adapter::openai::OpenAIStreamer;
use crate::adapter::openai::ToWebRequestCustom;
use crate::adapter::{Adapter, AdapterDispatcher, AdapterKind, ServiceType, WebRequestData};
use crate::chat::{
	BinarySource, CacheControl, ChatOptionsSet, ChatRequest, ChatResponse, ChatResponseFormat, ChatRole, ChatStream,
	ChatStreamResponse, ContentPart, MessageContent, ReasoningEffort, ToolCall, Usage,
};
use crate::resolver::{AuthData, Endpoint};
use crate::webc::{EventSourceStream, WebResponse};
use crate::{Error, Headers, Result};
use crate::{ModelIden, ServiceTarget};
use reqwest::RequestBuilder;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::error;
use tracing::warn;
use value_ext::JsonValueExt;

pub struct OpenAIAdapter;

// Latest models
const MODELS: &[&str] = &[
	//
	"gpt-5.2",
	"gpt-5.2-pro",
	"gpt-5-mini",
	"gpt-5-nano",
	"gpt-audio-mini",
	"gpt-audio",
];

impl OpenAIAdapter {
	pub const API_KEY_DEFAULT_ENV_NAME: &str = "OPENAI_API_KEY";
}

impl Adapter for OpenAIAdapter {
	fn default_auth() -> AuthData {
		AuthData::from_env(Self::API_KEY_DEFAULT_ENV_NAME)
	}

	fn default_endpoint() -> Endpoint {
		const BASE_URL: &str = "https://api.openai.com/v1/";
		Endpoint::from_static(BASE_URL)
	}

	/// Note: Currently returns the common models (see above)
	async fn all_model_names(_kind: AdapterKind) -> Result<Vec<String>> {
		Ok(MODELS.iter().map(|s| s.to_string()).collect())
	}

	fn get_service_url(model: &ModelIden, service_type: ServiceType, endpoint: Endpoint) -> Result<String> {
		Self::util_get_service_url(model, service_type, endpoint)
	}

	fn to_web_request_data(
		target: ServiceTarget,
		service_type: ServiceType,
		chat_req: ChatRequest,
		chat_options: ChatOptionsSet<'_, '_>,
	) -> Result<WebRequestData> {
		OpenAIAdapter::util_to_web_request_data(target, service_type, chat_req, chat_options, None)
	}

	fn to_chat_response(
		model_iden: ModelIden,
		web_response: WebResponse,
		options_set: ChatOptionsSet<'_, '_>,
	) -> Result<ChatResponse> {
		let WebResponse { mut body, .. } = web_response;

		// -- Capture the provider_model_iden
		let provider_model_name: Option<String> = body.x_remove("model").ok();
		let provider_model_iden = model_iden.from_optional_name(provider_model_name);

		// -- Capture the usage
		let usage = body
			.x_take("usage")
			.map(|value| OpenAIAdapter::into_usage(model_iden.adapter_kind, value))
			.unwrap_or_default();

		// -- Capture the content
		let mut content: MessageContent = MessageContent::default();
		let mut reasoning_content: Option<String> = None;

		if let Ok(Some(mut first_choice)) = body.x_take::<Option<Value>>("/choices/0") {
			// Check if reasoning is present
			// Can be in two places:
			// - /message/reasoning
			// - /message/reasoning_content
			// Extracted before content as some model can return reasoning without content
			reasoning_content = first_choice
				.x_take::<Option<String>>("/message/reasoning")
				.ok()
				.unwrap_or_else(|| {
					first_choice
						.x_take::<Option<String>>("/message/reasoning_content")
						.ok()
						.flatten()
				})
				.map(|s| s.trim().to_string());

			// -- Push eventual text message
			if let Ok(Some(mut text_content)) = first_choice.x_take::<Option<String>>("/message/content") {
				text_content = text_content.trim().to_string();
				// If not reasoning_content, but
				if reasoning_content.is_none() && options_set.normalize_reasoning_content().unwrap_or_default() {
					let (content_tmp, reasoning_content_tmp) = extract_think(text_content);
					reasoning_content = reasoning_content_tmp;
					text_content = content_tmp;
				}

				// After extracting reasoning_content, sometimes the content is empty.
				if !text_content.is_empty() {
					content.push(text_content);
				}
			}

			// -- Push eventual ToolCalls
			if let Some(tool_calls) = first_choice
				.x_take("/message/tool_calls")
				.ok()
				.map(parse_tool_calls)
				.transpose()?
				.map(MessageContent::from_tool_calls)
			{
				content.extend(tool_calls);
			}
		}

		Ok(ChatResponse {
			content,
			reasoning_content,
			model_iden,
			provider_model_iden,
			usage,
			captured_raw_body: None, // Set by the client exec_chat
		})
	}

	fn to_chat_stream(
		model_iden: ModelIden,
		reqwest_builder: RequestBuilder,
		options_sets: ChatOptionsSet<'_, '_>,
	) -> Result<ChatStreamResponse> {
		let event_source = EventSourceStream::new(reqwest_builder);
		let openai_stream = OpenAIStreamer::new(event_source, model_iden.clone(), options_sets);
		let chat_stream = ChatStream::from_inter_stream(openai_stream);

		Ok(ChatStreamResponse {
			model_iden,
			stream: chat_stream,
		})
	}

	fn to_embed_request_data(
		service_target: ServiceTarget,
		embed_req: crate::embed::EmbedRequest,
		options_set: crate::embed::EmbedOptionsSet<'_, '_>,
	) -> Result<WebRequestData> {
		super::embed::to_embed_request_data(service_target, embed_req, options_set)
	}

	fn to_embed_response(
		model_iden: ModelIden,
		web_response: WebResponse,
		options_set: crate::embed::EmbedOptionsSet<'_, '_>,
	) -> Result<crate::embed::EmbedResponse> {
		super::embed::to_embed_response(model_iden, web_response, options_set)
	}
}

/// Support functions for other adapters that share OpenAI APIs
impl OpenAIAdapter {
	pub(in crate::adapter::adapters) fn util_get_service_url(
		_model: &ModelIden,
		service_type: ServiceType,
		// -- utility arguments
		default_endpoint: Endpoint,
	) -> Result<String> {
		let base_url = default_endpoint.base_url();
		// Parse into URL and query-params
		let base_url = reqwest::Url::parse(base_url)
			.map_err(|err| Error::Internal(format!("Cannot parse url: {base_url}. Cause:\n{err}")))?;
		let original_query_params = base_url.query().to_owned();

		let suffix = match service_type {
			ServiceType::Chat | ServiceType::ChatStream => "chat/completions",
			ServiceType::Embed => "embeddings",
		};
		let mut full_url = base_url.join(suffix).map_err(|err| {
			Error::Internal(format!(
				"Cannot joing suffix '{suffix}' for url: {base_url}. Cause:\n{err}"
			))
		})?;
		full_url.set_query(original_query_params);
		Ok(full_url.to_string())
	}

	/// Shared OpenAI to_web_request_data for various OpenAI compatible adapters
	pub(in crate::adapter::adapters) fn util_to_web_request_data(
		target: ServiceTarget,
		service_type: ServiceType,
		chat_req: ChatRequest,
		options_set: ChatOptionsSet<'_, '_>,
		custom: Option<ToWebRequestCustom>,
	) -> Result<WebRequestData> {
		let ServiceTarget { model, auth, endpoint } = target;
		let (_, model_name) = model.model_name.namespace_and_name();
		let adapter_kind = model.adapter_kind;

		// -- api_key
		let api_key = get_api_key(auth, &model)?;

		// -- url
		let url = AdapterDispatcher::get_service_url(&model, service_type, endpoint)?;

		// -- headers
		let headers = Headers::from(("Authorization".to_string(), format!("Bearer {api_key}")));

		let stream = matches!(service_type, ServiceType::ChatStream);

		// -- compute reasoning_effort and eventual trimmed model_name
		// For now, just for openai AdapterKind
		let (reasoning_effort, model_name): (Option<ReasoningEffort>, &str) =
			if matches!(adapter_kind, AdapterKind::OpenAI) {
				let (reasoning_effort, model_name) = options_set
					.reasoning_effort()
					.cloned()
					.map(|v| (Some(v), model_name))
					.unwrap_or_else(|| ReasoningEffort::from_model_name(model_name));

				(reasoning_effort, model_name)
			} else {
				(None, model_name)
			};

		// -- Build the basic payload

		// Lethe fork: OpenAI reasoning-era models (o-series, gpt-5 family) have
		// stricter Chat Completions rules — `max_tokens` must be sent as
		// `max_completion_tokens`, and sampling params like `temperature`/`top_p`
		// are a hard 400 unless left at their defaults. Only applies to the
		// direct OpenAI adapter; compatible providers keep the classic fields.
		let (use_max_completion_tokens, reject_sampling_params) = if matches!(adapter_kind, AdapterKind::OpenAI) {
			openai_reasoning_era_rules(model_name)
		} else {
			(false, false)
		};

		let OpenAIRequestParts { messages, tools } = Self::into_openai_request_parts(&model, chat_req)?;
		let mut payload = json!({
			"model": model_name,
			"messages": messages,
			"stream": stream
		});

		// -- Set reasoning effort
		if let Some(reasoning_effort) = reasoning_effort
			&& let Some(keyword) = reasoning_effort.as_keyword()
		{
			payload.x_insert("reasoning_effort", keyword)?;
		}

		// -- Set verbosity
		if let Some(verbosity) = options_set.verbosity()
			&& let Some(keyword) = verbosity.as_keyword()
		{
			payload.x_insert("verbosity", keyword)?;
		}

		// -- Tools
		if let Some(tools) = tools {
			payload.x_insert("/tools", tools)?;
		}

		// -- Add options
		let response_format = if let Some(response_format) = options_set.response_format() {
			match response_format {
				ChatResponseFormat::JsonMode => Some(json!({"type": "json_object"})),
				ChatResponseFormat::JsonSpec(st_json) => {
					// "type": "json_schema", "json_schema": {...}

					let mut schema = st_json.schema.clone();
					schema.x_walk(|parent_map, name| {
						if name == "type" {
							let typ = parent_map.get("type").and_then(|v| v.as_str()).unwrap_or("");
							if typ == "object" {
								parent_map.insert("additionalProperties".to_string(), false.into());
							}
						}
						true
					});

					Some(json!({
						"type": "json_schema",
						"json_schema": {
							"name": st_json.name.clone(),
							"strict": true,
							// TODO: add description
							"schema": schema,
						}
					}))
				}
			}
		} else {
			None
		};

		if let Some(response_format) = response_format {
			payload["response_format"] = response_format;
		}

		// -- Add supported ChatOptions
		if stream & options_set.capture_usage().unwrap_or(false) {
			payload.x_insert("stream_options", json!({"include_usage": true}))?;
		}

		if let Some(temperature) = options_set.temperature()
			&& !reject_sampling_params
		{
			payload.x_insert("temperature", temperature)?;
		}

		if !options_set.stop_sequences().is_empty() {
			payload.x_insert("stop", options_set.stop_sequences())?;
		}

		let max_tokens_key = if use_max_completion_tokens {
			"max_completion_tokens"
		} else {
			"max_tokens"
		};
		if let Some(max_tokens) = options_set.max_tokens() {
			payload.x_insert(max_tokens_key, max_tokens)?;
		} else if let Some(custom) = custom.as_ref()
			&& let Some(max_tokens) = custom.default_max_tokens
		{
			payload.x_insert(max_tokens_key, max_tokens)?;
		}
		if let Some(top_p) = options_set.top_p()
			&& !reject_sampling_params
		{
			payload.x_insert("top_p", top_p)?;
		}
		if let Some(seed) = options_set.seed() {
			payload.x_insert("seed", seed)?;
		}
		if let Some(service_tier) = options_set.service_tier()
			&& let Some(keyword) = service_tier.as_keyword()
		{
			payload.x_insert("service_tier", keyword)?;
		}

		Ok(WebRequestData { url, headers, payload })
	}

	/// Note: Needs to be called from super::streamer as well
	pub(super) fn into_usage(adapter: AdapterKind, usage_value: Value) -> Usage {
		// NOTE: here we make sure we do not fail since we do not want to break a response because usage parsing fail
		let usage = serde_json::from_value(usage_value).map_err(|err| {
			error!("Fail to deserialize usage. Cause: {err}");
			err
		});
		let mut usage: Usage = usage.unwrap_or_default();
		// Will set details to None if no values
		usage.compact_details();

		// Unfortunately, xAI grok-3 does not compute reasoning tokens correctly.
		// Example: completion_tokens: 35, completion_tokens_details.reasoning_tokens: 192
		// BUT completion_tokens should be 35 + 192.
		// TODO: We might want to do this for other token details as well.
		// TODO: We could check if the math adds up first with the total token count, and only change it if it does not.
		//       This will allow us to be forward compatible if/when they fix this bug (yes, it is a bug).
		if matches!(adapter, AdapterKind::Xai)
			&& let Some(reasoning_tokens) = usage.completion_tokens_details.as_ref().and_then(|d| d.reasoning_tokens)
		{
			let completion_tokens = usage.completion_tokens.unwrap_or(0);
			usage.completion_tokens = Some(completion_tokens + reasoning_tokens)
		}

		usage
	}

	/// Takes the genai ChatMessages and builds the OpenAIChatRequestParts
	/// - `genai::ChatRequest.system`, if present, is added as the first message with role 'system'.
	/// - All messages get added with the corresponding roles (tools are not supported for now)
	fn into_openai_request_parts(_model_iden: &ModelIden, chat_req: ChatRequest) -> Result<OpenAIRequestParts> {
		let mut messages: Vec<Value> = Vec::new();

		// -- Process the system
		if let Some(system_msg) = chat_req.system {
			messages.push(json!({"role": "system", "content": system_msg}));
		}

		// -- Process the messages
		for msg in chat_req.messages {
			// Note: Will handle more types later
			// Lethe fork: capture cache_control off the message before
			// moving its content. Forwarding cache markers as a
			// content-parts array is OpenRouter's documented extension
			// for routing prompt caching to providers that support it
			// (Anthropic, Gemini, Moonshot/Kimi). Direct OpenAI silently
			// drops unknown fields, so this is safe on both paths.
			let msg_cache_control = msg.options.as_ref().and_then(|opts| opts.cache_control);
			match msg.role {
				// For now, system and tool messages go to the system
				ChatRole::System => {
					if let Some(content) = msg.content.into_joined_texts() {
						if let Some(cache_control) = msg_cache_control {
							messages.push(json!({
								"role": "system",
								"content": [{
									"type": "text",
									"text": content,
									"cache_control": cache_control_value(cache_control),
								}],
							}));
						} else {
							messages.push(json!({"role": "system", "content": content}));
						}
					}
					// TODO: Probably need to warn if it is a ToolCalls type of content
				}

				// User - For now support Text and Binary
				ChatRole::User => {
					// -- If we have only text, then, we jjust returned the joined_texts
					if msg.content.is_text_only() {
						// NOTE: for now, if no content, just return empty string (respect current logic)
						let content = json!(msg.content.joined_texts().unwrap_or_else(String::new));
						messages.push(json! ({"role": "user", "content": content}));
					} else {
						let mut values: Vec<Value> = Vec::new();
						for part in msg.content {
							match part {
								ContentPart::Text(content) => values.push(json!({"type": "text", "text": content})),
								ContentPart::Binary(binary) => {
									let is_audio = binary.is_audio();
									let is_image = binary.is_image();

									// let Binary {
									// 	content_type, source, ..
									// } = binary;

									if is_audio {
										match &binary.source {
											BinarySource::Url(_url) => {
												warn!(
													"OpenAI doesn't support audio from URL, need to handle it gracefully"
												);
											}
											BinarySource::Base64(content) => {
												let mut format =
													binary.content_type.split('/').next_back().unwrap_or("");
												if format == "mpeg" {
													format = "mp3";
												}
												values.push(json!({
													"type": "input_audio",
													"input_audio": {
														"data": content,
														"format": format
													}
												}));
											}
										}
									} else if is_image {
										let image_url = binary.into_url();
										values.push(json!({"type": "image_url", "image_url": {"url": image_url}}));
									} else if matches!(&binary.source, BinarySource::Url(_)) {
										// TODO: Need to return error
										warn!("OpenAI doesn't support file from URL, need to handle it gracefully");
									} else {
										let filename = binary.name.clone();
										let file_base64_url = binary.into_url();
										values.push(json!({"type": "file", "file": {
											"filename": filename,
											"file_data": file_base64_url
										}}))
									}
								}

								// Use `match` instead of `if let`. This will allow to future-proof this
								// implementation in case some new message content types would appear,
								// this way library would not compile if not all methods are implemented
								// continue would allow to gracefully skip pushing unserializable message
								// TODO: Probably need to warn if it is a ToolCalls type of content
								ContentPart::ToolCall(_) => (),
								ContentPart::ToolResponse(_) => (),
								ContentPart::ThoughtSignature(_) => (),
							}
						}
						messages.push(json! ({"role": "user", "content": values}));
					}
				}

				// Assistant - For now support Text and ToolCalls
				ChatRole::Assistant => {
					// -- If we have only text, then, we jjust returned the joined_texts
					let mut texts: Vec<String> = Vec::new();
					let mut tool_calls: Vec<Value> = Vec::new();
					for part in msg.content {
						match part {
							ContentPart::Text(text) => texts.push(text),
							ContentPart::ToolCall(tool_call) => {
								//
								tool_calls.push(json!({
									"type": "function",
									"id": tool_call.call_id,
									"function": {
										"name": tool_call.fn_name,
										"arguments": tool_call.fn_arguments.to_string(),
									}
								}))
							}

							// TODO: Probably need towarn on this one (probably need to add binary here)
							ContentPart::Binary(_) => (),
							ContentPart::ToolResponse(_) => (),
							ContentPart::ThoughtSignature(_) => {}
						}
					}
					let content = texts.join("\n\n");
					let mut message = json!({"role": "assistant", "content": content});
					if !tool_calls.is_empty() {
						message.x_insert("tool_calls", tool_calls)?;
					}
					messages.push(message);
				}

				// Tool - For now, support only tool responses
				ChatRole::Tool => {
					for part in msg.content {
						if let ContentPart::ToolResponse(tool_response) = part {
							messages.push(json!({
								"role": "tool",
								"content": tool_response.content,
								"tool_call_id": tool_response.call_id,
							}))
						}
					}

					// TODO: Probably need to trace/warn that this will be ignored
				}
			}
		}

		// -- Process the tools
		let tools = chat_req.tools.map(|tools| {
			tools
				.into_iter()
				.map(|tool| {
					// TODO: Need to handle the error correctly
					// TODO: Needs to have a custom serializer (tool should not have to match to a provider)
					// NOTE: Right now, low probability, so, we just return null if cannot convert to value.
					json!({
						"type": "function",
						"function": {
							"name": tool.name,
							"description": tool.description,
							"parameters": tool.schema,
							// TODO: If we need to support `strict: true` we need to add additionalProperties: false into the schema
							//       above (like structured output)
							"strict": false,
						}
					})
				})
				.collect::<Vec<Value>>()
		});

		Ok(OpenAIRequestParts { messages, tools })
	}
}

// region:    --- Support

fn extract_think(content: String) -> (String, Option<String>) {
	let start_tag = "<think>";
	let end_tag = "</think>";

	if let Some(start) = content.find(start_tag)
		&& let Some(end) = content[start + start_tag.len()..].find(end_tag)
	{
		let start_pos = start;
		let end_pos = start + start_tag.len() + end;

		let think_content = &content[start_pos + start_tag.len()..end_pos];
		let think_content = think_content.trim();

		// Extract parts of the original content without cloning until necessary
		let before_think = &content[..start_pos];
		let after_think = &content[end_pos + end_tag.len()..];

		// Remove a leading newline in `after_think` if it starts with '\n'
		let after_think = after_think.trim_start();

		// Construct the final cleaned content in one allocation
		let cleaned_content = format!("{before_think}{after_think}");

		return (cleaned_content, Some(think_content.to_string()));
	}

	(content, None)
}

struct OpenAIRequestParts {
	messages: Vec<Value>,
	tools: Option<Vec<Value>>,
}

fn parse_tool_calls(raw_tool_calls: Value) -> Result<Vec<ToolCall>> {
	// Some backends (like sglang) return null if no tool calls are present.
	if raw_tool_calls.is_null() {
		return Ok(vec![]);
	}

	let Value::Array(raw_tool_calls) = raw_tool_calls else {
		return Err(Error::InvalidJsonResponseElement {
			info: "tool calls is not an array",
		});
	};

	let tool_calls = raw_tool_calls.into_iter().map(parse_tool_call).collect::<Result<Vec<_>>>()?;

	Ok(tool_calls)
}

fn parse_tool_call(raw_tool_call: Value) -> Result<ToolCall> {
	// Define a helper struct to match the original JSON structure.
	#[derive(Deserialize)]
	struct IterimToolFnCall {
		id: String,
		#[allow(unused)]
		#[serde(rename = "type")]
		r#type: String,
		function: IterimFunction,
	}

	#[derive(Deserialize)]
	struct IterimFunction {
		name: String,
		arguments: Value,
	}

	let iterim = serde_json::from_value::<IterimToolFnCall>(raw_tool_call)?;

	let fn_name = iterim.function.name;

	// For now, support Object only, and parse the eventual string as a json value.
	// Eventually, we might check pricing
	let fn_arguments = match iterim.function.arguments {
		Value::Object(obj) => Value::Object(obj),
		Value::String(txt) => serde_json::from_str(&txt)?,
		_ => {
			return Err(Error::InvalidJsonResponseElement {
				info: "tool call arguments is not an object",
			});
		}
	};

	// Then, map the fields of the helper struct to the flat structure.
	Ok(ToolCall {
		call_id: iterim.id,
		fn_name,
		fn_arguments,
		thought_signatures: None,
	})
}

// endregion: --- Support

// region:    --- Lethe fork cache_control helpers

/// Lethe fork: build the OpenRouter/Anthropic-compatible `cache_control`
/// object from a `CacheControl` marker. `Persistent` is the fork's
/// extension for Anthropic's 1h extended cache; OpenRouter forwards
/// the marker verbatim to upstream providers and drops it for ones
/// that don't recognize the field.
fn cache_control_value(control: CacheControl) -> Value {
	match control {
		CacheControl::Ephemeral => json!({"type": "ephemeral"}),
		CacheControl::Persistent => json!({"type": "ephemeral", "ttl": "1h"}),
	}
}

// endregion: --- Lethe fork cache_control helpers

// region:    --- Lethe fork reasoning-era param rules

/// Lethe fork: Chat Completions parameter rules for OpenAI reasoning-era
/// models. Returns `(use_max_completion_tokens, reject_sampling_params)`.
///
/// The o-series (o1/o3/o4…) and the whole gpt-5 family (gpt-5, gpt-5.x,
/// gpt-5-mini, …) reject `max_tokens` with a 400 and require
/// `max_completion_tokens` instead. The same models reject non-default
/// sampling params (`temperature`, `top_p`) — except the `gpt-5-chat*`
/// conversational variants, which still support sampling but share the
/// `max_completion_tokens` requirement.
fn openai_reasoning_era_rules(model_name: &str) -> (bool, bool) {
	let bytes = model_name.as_bytes();
	// "o1", "o3-mini", "o4-mini", … but not e.g. "olive" or "o200k".
	let o_series = matches!(bytes, [b'o', d, ..] if d.is_ascii_digit())
		&& (bytes.len() == 2 || bytes[2] == b'-' || bytes[2] == b'.');
	let gpt5 = model_name.starts_with("gpt-5");
	if !(o_series || gpt5) {
		return (false, false);
	}
	(true, !model_name.starts_with("gpt-5-chat"))
}

// endregion: --- Lethe fork reasoning-era param rules

#[cfg(test)]
mod cache_control_tests {
	use super::*;
	use crate::chat::ChatMessage;

	#[test]
	fn system_message_without_cache_marker_stays_a_plain_string() {
		let req = ChatRequest::new(vec![ChatMessage::system("you are helpful")]);
		let parts = OpenAIAdapter::into_openai_request_parts(
			&ModelIden::new(AdapterKind::OpenAI, "gpt-5.4".to_string()),
			req,
		)
		.unwrap();
		assert_eq!(parts.messages.len(), 1);
		assert_eq!(parts.messages[0]["role"], json!("system"));
		assert_eq!(parts.messages[0]["content"], json!("you are helpful"));
	}

	#[test]
	fn system_message_with_persistent_marker_emits_cache_control_parts() {
		let mut msg = ChatMessage::system("stable prefix");
		msg.options = Some(CacheControl::Persistent.into());
		let req = ChatRequest::new(vec![msg]);
		let parts = OpenAIAdapter::into_openai_request_parts(
			&ModelIden::new(AdapterKind::OpenAI, "gpt-5.4".to_string()),
			req,
		)
		.unwrap();
		assert_eq!(parts.messages.len(), 1);
		assert_eq!(parts.messages[0]["role"], json!("system"));
		let content = parts.messages[0]["content"].as_array().unwrap();
		assert_eq!(content.len(), 1);
		assert_eq!(content[0]["type"], json!("text"));
		assert_eq!(content[0]["text"], json!("stable prefix"));
		assert_eq!(
			content[0]["cache_control"],
			json!({"type": "ephemeral", "ttl": "1h"})
		);
	}

	#[test]
	fn system_message_with_ephemeral_marker_emits_5m_cache_control() {
		let mut msg = ChatMessage::system("volatile tail");
		msg.options = Some(CacheControl::Ephemeral.into());
		let req = ChatRequest::new(vec![msg]);
		let parts = OpenAIAdapter::into_openai_request_parts(
			&ModelIden::new(AdapterKind::OpenAI, "gpt-5.4".to_string()),
			req,
		)
		.unwrap();
		let content = parts.messages[0]["content"].as_array().unwrap();
		assert_eq!(
			content[0]["cache_control"],
			json!({"type": "ephemeral"})
		);
	}

	#[test]
	fn non_system_messages_keep_their_existing_shape() {
		let req = ChatRequest::new(vec![ChatMessage::user("hello")]);
		let parts = OpenAIAdapter::into_openai_request_parts(
			&ModelIden::new(AdapterKind::OpenAI, "gpt-5.4".to_string()),
			req,
		)
		.unwrap();
		assert_eq!(parts.messages[0]["role"], json!("user"));
		// User content stays a plain string when no parts split is needed.
		assert_eq!(parts.messages[0]["content"], json!("hello"));
	}
}

#[cfg(test)]
mod reasoning_era_param_tests {
	use super::*;
	use crate::chat::{ChatMessage, ChatOptions};

	fn openai_payload(model_name: &str, options: &ChatOptions) -> Value {
		let target = ServiceTarget {
			model: ModelIden::new(AdapterKind::OpenAI, model_name.to_string()),
			auth: AuthData::from_single("test-key"),
			endpoint: OpenAIAdapter::default_endpoint(),
		};
		let chat_req = ChatRequest::new(vec![ChatMessage::user("hi")]);
		let options_set = ChatOptionsSet::default().with_chat_options(Some(options));
		let data = OpenAIAdapter::util_to_web_request_data(
			target,
			ServiceType::ChatStream,
			chat_req,
			options_set,
			None,
		)
		.unwrap();
		data.payload
	}

	#[test]
	fn gpt5_models_get_max_completion_tokens_and_no_temperature() {
		let options = ChatOptions::default()
			.with_temperature(0.7)
			.with_max_tokens(8000)
			.with_top_p(0.9);
		for model in ["gpt-5.5", "gpt-5.4-mini", "gpt-5", "gpt-5-nano", "o3-mini", "o1"] {
			let payload = openai_payload(model, &options);
			assert_eq!(
				payload["max_completion_tokens"],
				json!(8000),
				"{model} must send max_completion_tokens"
			);
			assert!(payload.get("max_tokens").is_none(), "{model} must not send max_tokens");
			assert!(
				payload.get("temperature").is_none(),
				"{model} must not send temperature"
			);
			assert!(payload.get("top_p").is_none(), "{model} must not send top_p");
		}
	}

	#[test]
	fn reasoning_effort_suffix_still_triggers_the_rules() {
		let options = ChatOptions::default().with_temperature(0.7).with_max_tokens(8000);
		// "-high" is stripped into reasoning_effort; the rules must apply to the
		// trimmed name that actually goes on the wire.
		let payload = openai_payload("gpt-5.5-high", &options);
		assert_eq!(payload["model"], json!("gpt-5.5"));
		assert_eq!(payload["reasoning_effort"], json!("high"));
		assert_eq!(payload["max_completion_tokens"], json!(8000));
		assert!(payload.get("temperature").is_none());
	}

	#[test]
	fn classic_models_keep_max_tokens_and_temperature() {
		let options = ChatOptions::default().with_temperature(0.7).with_max_tokens(8000);
		for model in ["gpt-4o", "gpt-4.1-mini"] {
			let payload = openai_payload(model, &options);
			assert_eq!(payload["max_tokens"], json!(8000), "{model} keeps max_tokens");
			assert!(
				payload.get("max_completion_tokens").is_none(),
				"{model} must not send max_completion_tokens"
			);
			assert_eq!(payload["temperature"], json!(0.7), "{model} keeps temperature");
		}
	}

	#[test]
	fn gpt5_chat_variant_keeps_sampling_but_uses_max_completion_tokens() {
		let options = ChatOptions::default().with_temperature(0.7).with_max_tokens(8000);
		let payload = openai_payload("gpt-5-chat-latest", &options);
		assert_eq!(payload["max_completion_tokens"], json!(8000));
		assert!(payload.get("max_tokens").is_none());
		assert_eq!(payload["temperature"], json!(0.7));
	}

	#[test]
	fn rules_classify_model_families_correctly() {
		// (model, use_max_completion_tokens, reject_sampling_params)
		let cases = [
			("gpt-5.5", true, true),
			("gpt-5.4-mini", true, true),
			("gpt-5-mini", true, true),
			("gpt-5-chat-latest", true, false),
			("o1", true, true),
			("o3-mini", true, true),
			("o4-mini", true, true),
			("gpt-4o", false, false),
			("gpt-4.1", false, false),
			("olive-3", false, false),
			("o200k-fake", false, false),
		];
		for (model, mct, reject) in cases {
			assert_eq!(
				openai_reasoning_era_rules(model),
				(mct, reject),
				"classification for {model}"
			);
		}
	}
}
