//! OpenAI ChatGPT Plus/Pro Codex OAuth client.
//!
//! Mirrors the v0.17.1 Python implementation: device-code flow against
//! auth.openai.com → tokens persisted under
//! `~/.lethe/credentials/openai_oauth_tokens.json` → chat calls posted to
//! `https://chatgpt.com/backend-api/codex/responses` (the Codex Responses
//! API, not the standard Chat Completions endpoint).
//!
//! The Responses API speaks a different shape than chat.completion:
//! system → `instructions`, messages → typed `input` items
//! (`input_text` / `output_text` / `function_call` / `function_call_output`),
//! and the wire response is SSE that we have to aggregate and translate
//! back into a genai `ChatResponse`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::{
    ChatOptions, ChatRequest, ChatResponse, ChatRole, ContentPart, MessageContent,
    PromptTokensDetails, ToolCall, Usage,
};
use genai::{ModelIden, chat::Tool};
use reqwest::StatusCode;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use thiserror::Error;
use tokio::sync::{Mutex, Semaphore};
use uuid::Uuid;

use crate::llm::client::DeltaCallback;

// --- Endpoints / constants ---------------------------------------------------

const OPENAI_OAUTH_ISSUER: &str = "https://auth.openai.com";
const OPENAI_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OPENAI_DEVICE_USERCODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const OPENAI_DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const OPENAI_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
/// Public client id used by Codex-compatible CLIs; same value the
/// Python predecessor and the official Codex CLI ship with.
const OPENAI_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const DEFAULT_INSTRUCTIONS: &str = "You are Lethe, a helpful and precise assistant.";
const DEVICE_AUTH_TIMEOUT_SECS: u64 = 900;
const DEVICE_POLL_SAFETY_MARGIN_SECS: u64 = 3;
const JWT_ACCOUNT_PATH: &str = "https://api.openai.com/auth";

// --- Client ------------------------------------------------------------------

#[derive(Clone)]
pub struct OpenAiOAuthClient {
    http: reqwest::Client,
    token_file: PathBuf,
    tokens: Arc<Mutex<OpenAiOAuthTokens>>,
    request_gate: Arc<Semaphore>,
    rate_limit_until: Arc<Mutex<Option<Instant>>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct OpenAiOAuthTokens {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_at: Option<f64>,
    account_id: Option<String>,
    #[serde(skip)]
    env_access_token: bool,
}

#[derive(Debug, Error)]
enum OpenAiOAuthError {
    #[error("{message}")]
    RateLimited {
        message: String,
        retry_after: Duration,
    },
    #[error("{message}")]
    Transient {
        message: String,
        retry_after: Duration,
    },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl OpenAiOAuthClient {
    pub fn from_env() -> Option<Self> {
        let token_file = openai_oauth_token_file();
        let tokens = if let Ok(access_token) = env::var("OPENAI_AUTH_TOKEN") {
            let access_token = access_token.trim().to_string();
            if access_token.is_empty() {
                None
            } else {
                let account_id = parse_jwt_claims(&access_token)
                    .as_ref()
                    .and_then(extract_account_id_from_claims);
                Some(OpenAiOAuthTokens {
                    access_token: Some(access_token),
                    account_id,
                    env_access_token: true,
                    ..Default::default()
                })
            }
        } else {
            read_openai_oauth_tokens(&token_file)
        }?;

        Some(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .build()
                .ok()?,
            token_file,
            tokens: Arc::new(Mutex::new(tokens)),
            request_gate: Arc::new(Semaphore::new(oauth_max_concurrency())),
            rate_limit_until: Arc::new(Mutex::new(None)),
        })
    }

    pub async fn exec_chat_request(
        &self,
        model: &str,
        request: ChatRequest,
        options: &ChatOptions,
    ) -> Result<ChatResponse> {
        let mut last_error = None;
        for attempt in 0..3 {
            match self
                .call_messages_once(model, request.clone(), options)
                .await
            {
                Ok(response) => return Ok(response),
                Err(error @ OpenAiOAuthError::RateLimited { retry_after, .. }) => {
                    last_error = Some(error);
                    tokio::time::sleep(retry_after).await;
                }
                Err(error @ OpenAiOAuthError::Transient { retry_after, .. }) => {
                    last_error = Some(error);
                    let wait = retry_after.max(Duration::from_secs(5 * (attempt + 1) as u64));
                    tokio::time::sleep(wait).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(last_error
            .map(anyhow::Error::from)
            .unwrap_or_else(|| anyhow!("OpenAI OAuth call failed")))
    }

    /// Streaming variant: forwards `response.output_text.delta` chunks to
    /// `on_delta` as they arrive. On pre-stream errors falls back to the
    /// non-streaming retry path so the user still gets a complete response.
    pub async fn exec_chat_request_stream(
        &self,
        model: &str,
        request: ChatRequest,
        options: &ChatOptions,
        on_delta: DeltaCallback<'_>,
    ) -> Result<ChatResponse> {
        match self
            .call_messages_stream(model, request.clone(), options, on_delta)
            .await
        {
            Ok(response) => Ok(response),
            Err(OpenAiOAuthError::RateLimited { .. } | OpenAiOAuthError::Transient { .. }) => {
                let response = self.exec_chat_request(model, request, options).await?;
                if let Some(text) = response.first_text() {
                    on_delta(text);
                }
                Ok(response)
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn call_messages_once(
        &self,
        model: &str,
        request: ChatRequest,
        options: &ChatOptions,
    ) -> Result<ChatResponse, OpenAiOAuthError> {
        self.ensure_access().await?;

        let (access_token, account_id) = {
            let tokens = self.tokens.lock().await;
            let access = tokens
                .access_token
                .clone()
                .ok_or_else(|| anyhow!("OpenAI OAuth access token is missing"))?;
            (access, tokens.account_id.clone())
        };
        let body = openai_responses_body(model, request, options);
        let request_log = json!({
            "auth": "openai_oauth",
            "endpoint": OPENAI_RESPONSES_URL,
            "model": model,
            "body": body.clone(),
        });
        let headers = openai_oauth_headers(&access_token, account_id.as_deref());

        let _permit = self
            .request_gate
            .acquire()
            .await
            .map_err(|error| anyhow!("OpenAI OAuth request gate closed: {error}"))?;
        self.wait_for_rate_limit().await;

        let response = self
            .http
            .post(OPENAI_RESPONSES_URL)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                crate::llm::client::log_llm_interaction(
                    "chat_openai_oauth_error",
                    model,
                    request_log.clone(),
                    json!({
                        "ok": false,
                        "error": format!("OpenAI OAuth request failed: {error}"),
                    }),
                );
                OpenAiOAuthError::Transient {
                    message: format!("OpenAI OAuth request failed: {error}"),
                    retry_after: Duration::from_secs(5),
                }
            })?;

        let status = response.status();
        let resp_headers = response.headers().clone();
        let text = response
            .text()
            .await
            .map_err(|error| anyhow!("OpenAI OAuth response read failed: {error}"))?;

        let response_log_body = if let Ok(value) = serde_json::from_str::<Value>(&text) {
            value
        } else {
            json!({"raw_text": truncate_log(&text)})
        };
        crate::llm::client::log_llm_interaction(
            if status.is_success() {
                "chat_openai_oauth"
            } else {
                "chat_openai_oauth_error"
            },
            model,
            request_log,
            json!({
                "ok": status.is_success(),
                "status": status.as_u16(),
                "body": response_log_body,
            }),
        );

        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry_after = retry_after_from_headers(&resp_headers);
            self.set_rate_limit(retry_after).await;
            return Err(OpenAiOAuthError::RateLimited {
                message: format!("OpenAI OAuth rate limited (429) - {}", truncate_err(&text)),
                retry_after,
            });
        }

        if status.as_u16() == 529 || status.as_u16() == 503 {
            return Err(OpenAiOAuthError::Transient {
                message: format!(
                    "OpenAI OAuth overloaded ({}) - {}",
                    status.as_u16(),
                    truncate_err(&text)
                ),
                retry_after: Duration::from_secs(5),
            });
        }

        if !status.is_success() {
            return Err(anyhow!(
                "OpenAI OAuth API error: {} - {}",
                status.as_u16(),
                truncate_err(&text)
            )
            .into());
        }

        let content_type = resp_headers
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();

        let data = if content_type.contains("application/json") {
            serde_json::from_str(&text)
                .with_context(|| format!("invalid OpenAI OAuth JSON: {}", truncate_err(&text)))?
        } else {
            parse_streamed_response(&text).map_err(OpenAiOAuthError::Other)?
        };

        openai_response_to_chat_response(data, model).map_err(Into::into)
    }

    /// Stream-and-aggregate: same endpoint as `call_messages_once`, but we
    /// consume the SSE incrementally. Text deltas land in `on_delta`
    /// immediately; tool_call deltas and item metadata are aggregated so
    /// the eventual `ChatResponse` matches what the non-streaming path
    /// produces.
    async fn call_messages_stream(
        &self,
        model: &str,
        request: ChatRequest,
        options: &ChatOptions,
        on_delta: DeltaCallback<'_>,
    ) -> Result<ChatResponse, OpenAiOAuthError> {
        self.ensure_access().await?;

        let (access_token, account_id) = {
            let tokens = self.tokens.lock().await;
            let access = tokens
                .access_token
                .clone()
                .ok_or_else(|| anyhow!("OpenAI OAuth access token is missing"))?;
            (access, tokens.account_id.clone())
        };
        let body = openai_responses_body(model, request, options);
        let mut headers = openai_oauth_headers(&access_token, account_id.as_deref());
        headers.insert("accept", HeaderValue::from_static("text/event-stream"));

        let _permit = self
            .request_gate
            .acquire()
            .await
            .map_err(|error| anyhow!("OpenAI OAuth request gate closed: {error}"))?;
        self.wait_for_rate_limit().await;

        let response = self
            .http
            .post(OPENAI_RESPONSES_URL)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .map_err(|error| OpenAiOAuthError::Transient {
                message: format!("OpenAI OAuth stream request failed: {error}"),
                retry_after: Duration::from_secs(5),
            })?;

        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry_after = retry_after_from_headers(response.headers());
            self.set_rate_limit(retry_after).await;
            let text = response.text().await.unwrap_or_default();
            return Err(OpenAiOAuthError::RateLimited {
                message: format!(
                    "OpenAI OAuth stream rate limited (429) - {}",
                    truncate_err(&text)
                ),
                retry_after,
            });
        }
        if status.as_u16() == 529 || status.as_u16() == 503 {
            let text = response.text().await.unwrap_or_default();
            return Err(OpenAiOAuthError::Transient {
                message: format!(
                    "OpenAI OAuth overloaded ({}) - {}",
                    status.as_u16(),
                    truncate_err(&text)
                ),
                retry_after: Duration::from_secs(5),
            });
        }
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "OpenAI OAuth stream error: {} - {}",
                status.as_u16(),
                truncate_err(&text)
            )
            .into());
        }

        let mut state = OpenAiStreamState::new();
        let mut events = response.bytes_stream().eventsource();
        while let Some(event) = events.next().await {
            let event = event.map_err(|error| OpenAiOAuthError::Transient {
                message: format!("OpenAI OAuth stream decode failed: {error}"),
                retry_after: Duration::from_secs(2),
            })?;
            if event.data.is_empty() {
                continue;
            }
            let Ok(payload) = serde_json::from_str::<Value>(&event.data) else {
                continue;
            };
            state.apply(&event.event, &payload, on_delta);
            if state.done {
                break;
            }
        }
        let data = state.finalize().map_err(OpenAiOAuthError::Other)?;
        openai_response_to_chat_response(data, model).map_err(Into::into)
    }

    async fn ensure_access(&self) -> Result<(), OpenAiOAuthError> {
        let refresh_token = {
            let tokens = self.tokens.lock().await;
            if tokens.env_access_token || !tokens.needs_refresh() {
                return Ok(());
            }
            tokens.refresh_token.clone()
        };
        let Some(refresh_token) = refresh_token else {
            return Ok(());
        };

        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", OPENAI_OAUTH_CLIENT_ID),
        ];
        let response = self
            .http
            .post(OPENAI_OAUTH_TOKEN_URL)
            .header("content-type", "application/x-www-form-urlencoded")
            .form(&form)
            .send()
            .await
            .map_err(|error| OpenAiOAuthError::Transient {
                message: format!("OpenAI OAuth token refresh failed: {error}"),
                retry_after: Duration::from_secs(5),
            })?;

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| anyhow!("OpenAI OAuth token refresh response read failed: {error}"))?;
        if !status.is_success() {
            return Err(anyhow!(
                "OpenAI OAuth token refresh failed: {} {}",
                status.as_u16(),
                truncate_err(&text)
            )
            .into());
        }

        let data: Value = serde_json::from_str(&text).with_context(|| {
            format!("invalid OpenAI OAuth refresh JSON: {}", truncate_err(&text))
        })?;
        let access_token = data
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("OpenAI OAuth refresh response is missing access_token"))?
            .to_string();
        let refresh_token = data
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string);
        let expires_in = data
            .get("expires_in")
            .and_then(Value::as_f64)
            .unwrap_or(3600.0);
        let new_account_id = extract_account_id_from_token_payload(&data);

        let snapshot = {
            let mut tokens = self.tokens.lock().await;
            tokens.access_token = Some(access_token);
            if let Some(refresh_token) = refresh_token {
                tokens.refresh_token = Some(refresh_token);
            }
            tokens.expires_at = Some(unix_now_seconds() + expires_in);
            if let Some(account_id) = new_account_id {
                tokens.account_id = Some(account_id);
            }
            tokens.clone()
        };
        write_openai_oauth_tokens(&self.token_file, &snapshot)?;
        Ok(())
    }

    async fn wait_for_rate_limit(&self) {
        let wait = {
            let until = self.rate_limit_until.lock().await;
            until.and_then(|instant| instant.checked_duration_since(Instant::now()))
        };
        if let Some(wait) = wait {
            tokio::time::sleep(wait).await;
        }
    }

    async fn set_rate_limit(&self, wait: Duration) {
        let mut until = self.rate_limit_until.lock().await;
        *until = Some(Instant::now() + wait);
    }
}

impl OpenAiOAuthTokens {
    fn needs_refresh(&self) -> bool {
        let Some(refresh_token) = &self.refresh_token else {
            return false;
        };
        if refresh_token.trim().is_empty() {
            return false;
        }
        self.expires_at.unwrap_or(0.0) <= unix_now_seconds() + 60.0
    }
}

// --- Headers -----------------------------------------------------------------

fn openai_oauth_headers(access_token: &str, account_id: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert("accept", HeaderValue::from_static("application/json"));
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {access_token}"))
            .unwrap_or_else(|_| HeaderValue::from_static("Bearer invalid")),
    );
    headers.insert(
        "openai-beta",
        HeaderValue::from_static("responses=experimental"),
    );
    headers.insert("originator", HeaderValue::from_static("pi"));
    headers.insert(
        "user-agent",
        HeaderValue::from_str(&build_user_agent())
            .unwrap_or_else(|_| HeaderValue::from_static("pi (lethe)")),
    );
    if let Some(account_id) = account_id
        && let Ok(value) = HeaderValue::from_str(account_id)
    {
        headers.insert("chatgpt-account-id", value);
    }
    headers
}

fn build_user_agent() -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    format!("pi ({os}; {arch})")
}

// --- Body shaping ------------------------------------------------------------

const OPENAI_OAUTH_MAX_BODY_BYTES: usize = 500_000;

fn openai_body_size_bytes(body: &Value) -> usize {
    serde_json::to_vec(body)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
}

fn trim_openai_input_items(body: &mut Value) {
    loop {
        let needs_leading_trim = body
            .get("input")
            .and_then(Value::as_array)
            .and_then(|input| input.first())
            .is_some_and(|item| item.get("role").is_none());
        let too_large = openai_body_size_bytes(body) > OPENAI_OAUTH_MAX_BODY_BYTES;
        if !needs_leading_trim && !too_large {
            break;
        }

        let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
            break;
        };
        if input.len() <= 1 {
            break;
        }
        input.remove(0);
    }
}

fn openai_responses_body(model: &str, request: ChatRequest, _options: &ChatOptions) -> Value {
    // System → instructions; everything else → typed input items.
    // max_tokens is intentionally not forwarded — the Codex endpoint
    // rejects token-limit params.
    let mut instructions_parts: Vec<String> = Vec::new();
    if let Some(system) = request.system
        && !system.trim().is_empty()
    {
        instructions_parts.push(system.trim().to_string());
    }
    let mut input_items: Vec<Value> = Vec::new();
    for message in request.messages {
        match message.role {
            ChatRole::System => {
                for text in message.content.into_texts() {
                    let trimmed = text.trim().to_string();
                    if !trimmed.is_empty() {
                        instructions_parts.push(trimmed);
                    }
                }
            }
            ChatRole::User => input_items.extend(openai_user_input_items(message.content)),
            ChatRole::Assistant => {
                input_items.extend(openai_assistant_input_items(message.content));
            }
            ChatRole::Tool => {
                input_items.extend(openai_tool_result_input_items(message.content));
            }
        }
    }
    if input_items.is_empty() {
        input_items.push(json!({
            "role": "user",
            "content": [{"type": "input_text", "text": "[Continue]"}]
        }));
    }
    let instructions = if instructions_parts.is_empty() {
        DEFAULT_INSTRUCTIONS.to_string()
    } else {
        instructions_parts.join("\n\n")
    };

    let mut body = json!({
        "model": model,
        "instructions": instructions,
        "input": input_items,
        "store": false,
        "stream": true,
    });
    trim_openai_input_items(&mut body);
    if let Some(tools) = request.tools
        && !tools.is_empty()
    {
        let tools_json: Vec<Value> = tools.into_iter().map(openai_tool_schema).collect();
        body["tools"] = Value::Array(tools_json);
    }
    body
}

fn openai_user_input_items(content: MessageContent) -> Vec<Value> {
    let mut parts: Vec<Value> = Vec::new();
    let mut items: Vec<Value> = Vec::new();
    for part in content.into_parts() {
        match part {
            ContentPart::Text(text) => {
                if !text.is_empty() {
                    parts.push(json!({"type": "input_text", "text": text}));
                }
            }
            ContentPart::Binary(binary) => {
                if binary.is_image() {
                    let url = match binary.source {
                        genai::chat::BinarySource::Url(url) => url.to_string(),
                        genai::chat::BinarySource::Base64(data) => {
                            format!("data:{};base64,{}", binary.content_type, data.as_ref())
                        }
                    };
                    parts.push(json!({"type": "input_image", "image_url": url}));
                }
                // Non-image binaries are dropped — the Responses API on
                // chatgpt.com doesn't accept document uploads on this
                // path; the user-visible content stays text-only.
            }
            ContentPart::ToolResponse(response) => {
                items.push(json!({
                    "type": "function_call_output",
                    "call_id": response.call_id,
                    "output": response.content,
                }));
            }
            ContentPart::ToolCall(_) | ContentPart::ThoughtSignature(_) => {}
        }
    }
    if !parts.is_empty() {
        items.insert(
            0,
            json!({
                "role": "user",
                "content": parts,
            }),
        );
    }
    items
}

fn openai_assistant_input_items(content: MessageContent) -> Vec<Value> {
    let mut text_parts: Vec<Value> = Vec::new();
    let mut tool_items: Vec<Value> = Vec::new();
    for part in content.into_parts() {
        match part {
            ContentPart::Text(text) => {
                if !text.is_empty() {
                    text_parts.push(json!({"type": "output_text", "text": text}));
                }
            }
            ContentPart::ToolCall(call) => {
                let arguments = match &call.fn_arguments {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let call_id = if call.call_id.is_empty() {
                    format!("call_{}", short_uuid())
                } else {
                    call.call_id
                };
                tool_items.push(json!({
                    "type": "function_call",
                    "call_id": call_id,
                    "name": call.fn_name,
                    "arguments": arguments,
                }));
            }
            ContentPart::Binary(_)
            | ContentPart::ToolResponse(_)
            | ContentPart::ThoughtSignature(_) => {}
        }
    }
    let mut items: Vec<Value> = Vec::new();
    if !text_parts.is_empty() {
        items.push(json!({
            "role": "assistant",
            "content": text_parts,
        }));
    }
    items.extend(tool_items);
    items
}

fn openai_tool_result_input_items(content: MessageContent) -> Vec<Value> {
    let mut items: Vec<Value> = Vec::new();
    for part in content.into_parts() {
        if let ContentPart::ToolResponse(response) = part {
            items.push(json!({
                "type": "function_call_output",
                "call_id": response.call_id,
                "output": response.content,
            }));
        }
    }
    items
}

fn openai_tool_schema(tool: Tool) -> Value {
    let mut entry = Map::new();
    entry.insert("type".to_string(), Value::String("function".to_string()));
    entry.insert("name".to_string(), Value::String(tool.name));
    if let Some(description) = tool.description {
        entry.insert("description".to_string(), Value::String(description));
    }
    let schema = tool.schema.unwrap_or_else(|| {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })
    });
    entry.insert("parameters".to_string(), schema);
    Value::Object(entry)
}

// --- SSE / Responses → ChatResponse -----------------------------------------

/// Parse OpenAI Responses SSE event blocks into (event_name, data) pairs.
/// Tolerant of missing trailing blank line.
fn iter_sse_events(raw: &str) -> Vec<(String, String)> {
    let mut events: Vec<(String, String)> = Vec::new();
    let mut event_name: Option<String> = None;
    let mut data_lines: Vec<String> = Vec::new();

    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("event: ") {
            if let Some(name) = event_name.take() {
                events.push((name, data_lines.join("\n")));
                data_lines.clear();
            }
            event_name = Some(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("data: ") {
            data_lines.push(rest.to_string());
            continue;
        }
        if line.trim().is_empty()
            && let Some(name) = event_name.take()
        {
            events.push((name, data_lines.join("\n")));
            data_lines.clear();
        }
    }
    if let Some(name) = event_name {
        events.push((name, data_lines.join("\n")));
    }
    events
}

fn openai_stream_error_message(
    event_name: &str,
    payload_type: &str,
    payload_obj: &Map<String, Value>,
) -> Option<String> {
    let error = if event_name == "error" || payload_type == "error" {
        payload_obj.get("error")
    } else if event_name == "response.failed" || payload_type == "response.failed" {
        payload_obj
            .get("response")
            .and_then(Value::as_object)
            .and_then(|response| response.get("error"))
    } else {
        None
    }?;

    if let Some(message) = error.get("message").and_then(Value::as_str) {
        if let Some(code) = error.get("code").and_then(Value::as_str) {
            return Some(format!("{code}: {message}"));
        }
        return Some(message.to_string());
    }
    error.as_str().map(str::to_string)
}

/// Streaming counterpart to `parse_streamed_response`. Holds the same
/// aggregator state (latest_response/output_items/output_text_deltas) but
/// is fed one SSE event at a time so text deltas can be forwarded to the
/// TUI without waiting for the whole response.
struct OpenAiStreamState {
    latest_response: Option<Map<String, Value>>,
    output_items: Vec<(String, Map<String, Value>)>,
    output_text_deltas: Vec<String>,
    error: Option<String>,
    done: bool,
}

impl OpenAiStreamState {
    fn new() -> Self {
        Self {
            latest_response: None,
            output_items: Vec::new(),
            output_text_deltas: Vec::new(),
            error: None,
            done: false,
        }
    }

    fn apply(&mut self, event_name: &str, payload: &Value, on_delta: DeltaCallback<'_>) {
        let Some(payload_obj) = payload.as_object() else {
            return;
        };
        let payload_type = payload_obj
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("");
        if let Some(message) = openai_stream_error_message(event_name, payload_type, payload_obj) {
            self.error = Some(message);
            self.done = true;
            return;
        }

        if let Some(response) = payload_obj.get("response").and_then(Value::as_object) {
            let target = self.latest_response.get_or_insert_with(Map::new);
            for (key, value) in response {
                if key == "output" {
                    if value.as_array().is_some_and(|arr| !arr.is_empty()) {
                        target.insert(key.clone(), value.clone());
                    } else if !target.contains_key("output") {
                        target.insert(key.clone(), value.clone());
                    }
                } else if !value.is_null() {
                    target.insert(key.clone(), value.clone());
                }
            }
        }

        if let Some(item) = payload_obj.get("item") {
            self.record_item(item);
        }
        if let Some(item) = payload_obj.get("output_item") {
            self.record_item(item);
        }

        let payload_type = payload_obj
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("");
        match payload_type {
            "response.output_text.delta" | "response.refusal.delta" => {
                if let Some(delta) = payload_obj.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    self.output_text_deltas.push(delta.to_string());
                    on_delta(delta);
                }
            }
            _ => {}
        }

        if event_name == "response.completed" || payload_type == "response.completed" {
            self.done = true;
        }
    }

    fn record_item(&mut self, item: &Value) {
        let Some(obj) = item.as_object() else {
            return;
        };
        let id = obj
            .get("id")
            .or_else(|| obj.get("call_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("item_{}", self.output_items.len()));
        if let Some((_, existing)) = self.output_items.iter_mut().find(|(key, _)| key == &id) {
            for (key, value) in obj {
                if key == "content" {
                    if value.as_array().is_some_and(|arr| !arr.is_empty()) {
                        existing.insert(key.clone(), value.clone());
                    }
                    continue;
                }
                if !value.is_null() {
                    existing.insert(key.clone(), value.clone());
                }
            }
        } else {
            self.output_items.push((id, obj.clone()));
        }
    }

    fn finalize(mut self) -> Result<Value> {
        if let Some(error) = self.error {
            return Err(anyhow!("OpenAI OAuth stream failed: {error}"));
        }
        if !self.done {
            return Err(anyhow!("OpenAI OAuth stream ended before completion"));
        }

        if !self.output_items.is_empty() {
            let assembled: Vec<Value> = self
                .output_items
                .drain(..)
                .map(|(_, obj)| Value::Object(obj))
                .collect();
            let target = self.latest_response.get_or_insert_with(Map::new);
            let needs_overwrite = target
                .get("output")
                .map(|value| value.as_array().is_none_or(|arr| arr.is_empty()))
                .unwrap_or(true);
            if needs_overwrite {
                target.insert("output".to_string(), Value::Array(assembled));
            }
        }

        if !self.output_text_deltas.is_empty() {
            let target = self.latest_response.get_or_insert_with(Map::new);
            let needs_overwrite = target
                .get("output")
                .map(|value| value.as_array().is_none_or(|arr| arr.is_empty()))
                .unwrap_or(true);
            if needs_overwrite {
                target.insert(
                    "output".to_string(),
                    json!([
                        {
                            "type": "message",
                            "content": [
                                {"type": "output_text", "text": self.output_text_deltas.join("")}
                            ]
                        }
                    ]),
                );
            }
        }

        self.latest_response
            .map(Value::Object)
            .ok_or_else(|| anyhow!("OpenAI OAuth stream ended without a response payload"))
    }
}

fn parse_streamed_response(raw: &str) -> Result<Value> {
    let mut latest_response: Option<Map<String, Value>> = None;
    // Insertion-ordered store: id → object. A Vec lookup is fine here
    // (output_items is bounded by the number of items in a single LLM
    // response, typically < 20) and avoids an indexmap dependency.
    let mut output_items: Vec<(String, Map<String, Value>)> = Vec::new();
    let mut output_text_deltas: Vec<String> = Vec::new();
    let mut completed = false;

    let mut record_item = |item: &Value| {
        let Some(obj) = item.as_object() else {
            return;
        };
        let id = obj
            .get("id")
            .or_else(|| obj.get("call_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("item_{}", output_items.len()));
        if let Some((_, existing)) = output_items.iter_mut().find(|(key, _)| key == &id) {
            for (key, value) in obj {
                if key == "content" {
                    if value.as_array().is_some_and(|arr| !arr.is_empty()) {
                        existing.insert(key.clone(), value.clone());
                    }
                    continue;
                }
                if !value.is_null() {
                    existing.insert(key.clone(), value.clone());
                }
            }
        } else {
            output_items.push((id, obj.clone()));
        }
    };

    for (event_name, data) in iter_sse_events(raw) {
        if data.is_empty() {
            continue;
        }
        let Ok(payload) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        let Some(payload_obj) = payload.as_object() else {
            continue;
        };
        let payload_type = payload_obj
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("");
        if let Some(message) = openai_stream_error_message(&event_name, payload_type, payload_obj) {
            return Err(anyhow!("OpenAI OAuth stream failed: {message}"));
        }

        if let Some(response) = payload_obj.get("response").and_then(Value::as_object) {
            let target = latest_response.get_or_insert_with(Map::new);
            for (key, value) in response {
                if key == "output" {
                    if value.as_array().is_some_and(|arr| !arr.is_empty()) {
                        target.insert(key.clone(), value.clone());
                    } else if !target.contains_key("output") {
                        target.insert(key.clone(), value.clone());
                    }
                } else if !value.is_null() {
                    target.insert(key.clone(), value.clone());
                }
            }
        }

        if let Some(item) = payload_obj.get("item") {
            record_item(item);
        }
        if let Some(item) = payload_obj.get("output_item") {
            record_item(item);
        }

        if matches!(
            payload_type,
            "response.output_text.delta" | "response.refusal.delta"
        ) && let Some(delta) = payload_obj.get("delta").and_then(Value::as_str)
            && !delta.is_empty()
        {
            output_text_deltas.push(delta.to_string());
        }

        if event_name == "response.completed" || payload_type == "response.completed" {
            completed = true;
            break;
        }
    }

    if !output_items.is_empty() {
        let assembled: Vec<Value> = output_items
            .into_iter()
            .map(|(_, obj)| Value::Object(obj))
            .collect();
        let target = latest_response.get_or_insert_with(Map::new);
        let needs_overwrite = target
            .get("output")
            .map(|value| value.as_array().is_none_or(|arr| arr.is_empty()))
            .unwrap_or(true);
        if needs_overwrite {
            target.insert("output".to_string(), Value::Array(assembled));
        }
    }

    if !output_text_deltas.is_empty() {
        let target = latest_response.get_or_insert_with(Map::new);
        let needs_overwrite = target
            .get("output")
            .map(|value| value.as_array().is_none_or(|arr| arr.is_empty()))
            .unwrap_or(true);
        if needs_overwrite {
            target.insert(
                "output".to_string(),
                json!([
                    {
                        "type": "message",
                        "content": [
                            {"type": "output_text", "text": output_text_deltas.join("")}
                        ]
                    }
                ]),
            );
        }
    }

    if !completed {
        return Err(anyhow!("OpenAI OAuth stream ended before completion"));
    }

    latest_response
        .map(Value::Object)
        .ok_or_else(|| anyhow!("OpenAI OAuth API error: could not parse streamed response"))
}

fn openai_response_to_chat_response(data: Value, requested_model: &str) -> Result<ChatResponse> {
    let response = data.get("response").cloned().unwrap_or(data);
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut parts: Vec<ContentPart> = Vec::new();
    let mut text_buf = String::new();
    for item in &output {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let item_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
        match item_type {
            "message" => {
                if let Some(blocks) = obj.get("content").and_then(Value::as_array) {
                    for block in blocks {
                        let block_obj = match block.as_object() {
                            Some(obj) => obj,
                            None => continue,
                        };
                        let block_type =
                            block_obj.get("type").and_then(Value::as_str).unwrap_or("");
                        if matches!(block_type, "output_text" | "text")
                            && let Some(text) = block_obj.get("text").and_then(Value::as_str)
                        {
                            text_buf.push_str(text);
                        }
                    }
                }
            }
            "function_call" => {
                let Some(name) = obj.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let call_id = obj
                    .get("call_id")
                    .or_else(|| obj.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("toolu_{}", short_uuid()));
                let arguments_raw = obj.get("arguments").cloned().unwrap_or_else(|| json!({}));
                let fn_arguments = match arguments_raw {
                    Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
                    other => other,
                };
                parts.push(ContentPart::ToolCall(ToolCall {
                    call_id,
                    fn_name: name.to_string(),
                    fn_arguments,
                    thought_signatures: None,
                }));
            }
            _ => {}
        }
    }

    if text_buf.is_empty()
        && let Some(text) = response.get("output_text").and_then(Value::as_str)
    {
        text_buf.push_str(text);
    }
    if !text_buf.is_empty() {
        parts.insert(0, ContentPart::Text(text_buf));
    }

    let provider_model = response
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| requested_model.to_string());

    let mut usage = Usage::default();
    if let Some(raw_usage) = response.get("usage") {
        let prompt = raw_usage
            .get("input_tokens")
            .and_then(Value::as_i64)
            .map(|v| v as i32);
        let completion = raw_usage
            .get("output_tokens")
            .and_then(Value::as_i64)
            .map(|v| v as i32);
        usage.prompt_tokens = prompt;
        usage.completion_tokens = completion;
        usage.total_tokens = match raw_usage.get("total_tokens").and_then(Value::as_i64) {
            Some(total) => Some(total as i32),
            None => prompt.zip(completion).map(|(p, c)| p + c),
        };
        if let Some(input_details) = raw_usage
            .get("input_tokens_details")
            .and_then(Value::as_object)
            && let Some(cached) = input_details.get("cached_tokens").and_then(Value::as_i64)
        {
            usage.prompt_tokens_details = Some(PromptTokensDetails {
                cache_creation_tokens: None,
                cached_tokens: Some(cached as i32),
                audio_tokens: None,
            });
        }
        usage.compact_details();
    }

    Ok(ChatResponse {
        content: MessageContent::from_parts(parts),
        reasoning_content: None,
        model_iden: ModelIden::new(AdapterKind::OpenAI, requested_model.to_string()),
        provider_model_iden: ModelIden::new(AdapterKind::OpenAI, provider_model),
        usage,
        captured_raw_body: None,
    })
}

// --- JWT / account id --------------------------------------------------------

fn decode_base64url(value: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value.as_bytes())
        .map_err(|error| anyhow!("base64url decode failed: {error}"))
}

fn parse_jwt_claims(token: &str) -> Option<Value> {
    let segments: Vec<&str> = token.split('.').collect();
    if segments.len() != 3 {
        return None;
    }
    let payload_bytes = decode_base64url(segments[1]).ok()?;
    let payload: Value = serde_json::from_slice(&payload_bytes).ok()?;
    payload.is_object().then_some(payload)
}

fn extract_account_id_from_claims(claims: &Value) -> Option<String> {
    if let Some(direct) = claims.get("chatgpt_account_id").and_then(Value::as_str)
        && !direct.is_empty()
    {
        return Some(direct.to_string());
    }
    if let Some(nested) = claims.get(JWT_ACCOUNT_PATH).and_then(Value::as_object)
        && let Some(id) = nested.get("chatgpt_account_id").and_then(Value::as_str)
        && !id.is_empty()
    {
        return Some(id.to_string());
    }
    if let Some(orgs) = claims.get("organizations").and_then(Value::as_array)
        && let Some(first) = orgs.first().and_then(Value::as_object)
        && let Some(id) = first.get("id").and_then(Value::as_str)
        && !id.is_empty()
    {
        return Some(id.to_string());
    }
    None
}

fn extract_account_id_from_token_payload(data: &Value) -> Option<String> {
    if let Some(id_token) = data.get("id_token").and_then(Value::as_str)
        && let Some(claims) = parse_jwt_claims(id_token)
        && let Some(account_id) = extract_account_id_from_claims(&claims)
    {
        return Some(account_id);
    }
    if let Some(access_token) = data.get("access_token").and_then(Value::as_str)
        && let Some(claims) = parse_jwt_claims(access_token)
    {
        return extract_account_id_from_claims(&claims);
    }
    None
}

// --- Token file persistence --------------------------------------------------

pub fn openai_oauth_token_file() -> PathBuf {
    if let Some(path) = env::var_os("LETHE_OPENAI_OAUTH_TOKENS") {
        return PathBuf::from(path);
    }
    if let Some(path) = env::var_os("CREDENTIALS_DIR") {
        return PathBuf::from(path).join("openai_oauth_tokens.json");
    }
    let home = env::var_os("LETHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".lethe")))
        .unwrap_or_else(|| PathBuf::from(".lethe"));
    home.join("credentials").join("openai_oauth_tokens.json")
}

fn read_openai_oauth_tokens(path: &Path) -> Option<OpenAiOAuthTokens> {
    let text = fs::read_to_string(path).ok()?;
    let mut tokens: OpenAiOAuthTokens = serde_json::from_str(&text).ok()?;
    tokens.env_access_token = false;
    tokens
        .access_token
        .as_ref()
        .is_some_and(|token| !token.trim().is_empty())
        .then_some(tokens)
}

fn write_openai_oauth_tokens(path: &Path, tokens: &OpenAiOAuthTokens) -> Result<()> {
    let Some(parent) = path.parent() else {
        bail!("OpenAI OAuth token path has no parent: {}", path.display());
    };
    fs::create_dir_all(parent)?;
    let text = serde_json::to_string_pretty(tokens)?;
    fs::write(path, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn openai_oauth_available() -> bool {
    env::var("OPENAI_AUTH_TOKEN")
        .map(|token| !token.trim().is_empty())
        .unwrap_or(false)
        || read_openai_oauth_tokens(&openai_oauth_token_file()).is_some()
}

// --- Device-code login flow --------------------------------------------------

// The .env-rewriting bookend lives in `crate::llm::oauth_env` so the
// `lethe login openai` dispatch can prompt for models between the
// device-login and the .env write. Callers go through
// `oauth_env::prompt_provider_models` + `update_env_after_oauth_login`.

/// Run the interactive ChatGPT Plus/Pro device-code login. Opens the
/// verification URL in a browser (best-effort), waits for the user to
/// approve, exchanges the resulting authorization code for tokens, and
/// writes them to the canonical token file. Does NOT touch the .env —
/// callers that want LLM_PROVIDER set automatically should call
/// `update_env_for_openai_oauth` after this returns.
pub async fn run_device_login() -> Result<()> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .context("building HTTP client for OpenAI device flow")?;

    println!();
    println!("OpenAI OAuth login (ChatGPT Plus/Pro Codex)");
    println!("──────────────────────────────────────────────");
    println!("Device-code flow. Approve in your browser, then return here.");
    println!();

    let device = start_device_flow(&http)
        .await
        .context("starting device flow")?;
    let device_auth_id = device
        .get("device_auth_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("device flow response missing device_auth_id"))?
        .to_string();
    let user_code = device
        .get("user_code")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("device flow response missing user_code"))?
        .to_string();
    let interval = device
        .get("interval")
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or(5);
    let verification_uri = device
        .get("verification_uri_complete")
        .or_else(|| device.get("verification_uri"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{OPENAI_OAUTH_ISSUER}/codex/device"));

    println!("Open this URL in a browser:");
    println!("    {verification_uri}");
    println!();
    println!("Enter this code if prompted: {user_code}");
    println!();
    best_effort_open(&verification_uri);
    println!("Waiting for authorization (Ctrl-C to cancel)...");

    let auth = poll_for_authorization_code(&http, &device_auth_id, &user_code, interval)
        .await
        .context("polling device auth endpoint")?;
    let authorization_code = auth
        .get("authorization_code")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("polling response missing authorization_code"))?
        .to_string();
    let code_verifier = auth
        .get("code_verifier")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("polling response missing code_verifier"))?
        .to_string();

    println!("Exchanging authorization code for tokens...");
    let token_data = exchange_authorization_code(&http, &authorization_code, &code_verifier)
        .await
        .context("token exchange")?;

    let access_token = token_data
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("token exchange response missing access_token"))?
        .to_string();
    let refresh_token = token_data
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_string);
    let expires_in = token_data
        .get("expires_in")
        .and_then(Value::as_f64)
        .unwrap_or(3600.0);
    let account_id = extract_account_id_from_token_payload(&token_data).or_else(|| {
        parse_jwt_claims(&access_token)
            .as_ref()
            .and_then(extract_account_id_from_claims)
    });

    let tokens = OpenAiOAuthTokens {
        access_token: Some(access_token.clone()),
        refresh_token: refresh_token.clone(),
        expires_at: Some(unix_now_seconds() + expires_in),
        account_id: account_id.clone(),
        env_access_token: false,
    };
    let token_file = openai_oauth_token_file();
    write_openai_oauth_tokens(&token_file, &tokens)
        .with_context(|| format!("writing OpenAI OAuth tokens to {}", token_file.display()))?;

    println!();
    println!("OAuth tokens saved to {}", token_file.display());
    println!(
        "Refresh token: {}",
        if refresh_token.is_some() { "yes" } else { "no" }
    );
    println!("Expires in: {expires_in:.0}s");
    println!(
        "Account id: {}",
        account_id.as_deref().unwrap_or("(not found)")
    );
    Ok(())
}

async fn start_device_flow(http: &reqwest::Client) -> Result<Value> {
    let response = http
        .post(OPENAI_DEVICE_USERCODE_URL)
        .header("content-type", "application/json")
        .header("user-agent", "lethe-oauth-login")
        .json(&json!({"client_id": OPENAI_OAUTH_CLIENT_ID}))
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        bail!(
            "device auth start failed: {} {}",
            status.as_u16(),
            truncate_err(&text)
        );
    }
    serde_json::from_str(&text)
        .with_context(|| format!("invalid device auth response JSON: {}", truncate_err(&text)))
}

async fn poll_for_authorization_code(
    http: &reqwest::Client,
    device_auth_id: &str,
    user_code: &str,
    interval_seconds: u64,
) -> Result<Value> {
    let wait = Duration::from_secs(interval_seconds.max(1) + DEVICE_POLL_SAFETY_MARGIN_SECS);
    let deadline = Instant::now() + Duration::from_secs(DEVICE_AUTH_TIMEOUT_SECS);
    loop {
        let response = http
            .post(OPENAI_DEVICE_TOKEN_URL)
            .header("content-type", "application/json")
            .header("user-agent", "lethe-oauth-login")
            .json(&json!({
                "device_auth_id": device_auth_id,
                "user_code": user_code,
            }))
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        if status.is_success() {
            return serde_json::from_str(&text).with_context(|| {
                format!("invalid device token response: {}", truncate_err(&text))
            });
        }
        // 403 and 404 are the pending-auth status codes in the current
        // OpenAI device endpoint.
        if matches!(status.as_u16(), 403 | 404) {
            let now = Instant::now();
            if now >= deadline {
                bail!("timed out waiting for OpenAI device authorization");
            }
            let sleep_for = wait.min(deadline.saturating_duration_since(now));
            if !sleep_for.is_zero() {
                tokio::time::sleep(sleep_for).await;
            }
            continue;
        }
        bail!(
            "device authorization polling failed: {} {}",
            status.as_u16(),
            truncate_err(&text)
        );
    }
}

async fn exchange_authorization_code(
    http: &reqwest::Client,
    authorization_code: &str,
    code_verifier: &str,
) -> Result<Value> {
    let form = [
        ("grant_type", "authorization_code"),
        ("code", authorization_code),
        ("redirect_uri", OPENAI_DEVICE_REDIRECT_URI),
        ("client_id", OPENAI_OAUTH_CLIENT_ID),
        ("code_verifier", code_verifier),
    ];
    let response = http
        .post(OPENAI_OAUTH_TOKEN_URL)
        .header("content-type", "application/x-www-form-urlencoded")
        .form(&form)
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        bail!(
            "token exchange failed: {} {}",
            status.as_u16(),
            truncate_err(&text)
        );
    }
    serde_json::from_str(&text)
        .with_context(|| format!("invalid token exchange response: {}", truncate_err(&text)))
}

fn best_effort_open(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    // Best-effort; if there's no display, silently skip.
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn();
}

// --- Helpers -----------------------------------------------------------------

fn oauth_max_concurrency() -> usize {
    env::var("LETHE_OAUTH_MAX_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1)
}

fn unix_now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

fn retry_after_from_headers(headers: &HeaderMap) -> Duration {
    headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<f64>().ok())
        .map(Duration::from_secs_f64)
        .unwrap_or_else(|| Duration::from_secs(30))
}

fn short_uuid() -> String {
    Uuid::new_v4().simple().to_string()[..12].to_string()
}

fn truncate_err(text: &str) -> String {
    const MAX: usize = 500;
    let text = text.trim();
    if text.len() <= MAX {
        text.to_string()
    } else {
        format!("{}...", &text[..MAX])
    }
}

fn truncate_log(text: &str) -> String {
    const MAX: usize = 4000;
    let text = text.trim();
    if text.len() <= MAX {
        text.to_string()
    } else {
        format!("{}...", &text[..MAX])
    }
}

// --- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use genai::chat::{ChatMessage, MessageContent, Tool};

    fn make_request() -> ChatRequest {
        let mut req = ChatRequest::new(vec![
            ChatMessage::system("be precise"),
            ChatMessage::user("hello"),
            ChatMessage::assistant("hi there"),
        ]);
        req = req.append_tool(
            Tool::new("lookup".to_string())
                .with_description("Find a value")
                .with_schema(json!({"type": "object", "properties": {}})),
        );
        req
    }

    #[test]
    fn body_extracts_instructions_and_input_items() {
        let body = openai_responses_body("gpt-5.2", make_request(), &ChatOptions::default());
        assert_eq!(body["instructions"], json!("be precise"));
        assert_eq!(body["store"], json!(false));
        assert_eq!(body["stream"], json!(true));
        let input = body["input"].as_array().unwrap();
        // user + assistant message → two items
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], json!("user"));
        assert_eq!(input[0]["content"][0]["type"], json!("input_text"));
        assert_eq!(input[0]["content"][0]["text"], json!("hello"));
        assert_eq!(input[1]["role"], json!("assistant"));
        assert_eq!(input[1]["content"][0]["type"], json!("output_text"));
        // tools rewritten without nested function wrapper
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], json!("function"));
        assert_eq!(tools[0]["name"], json!("lookup"));
        assert!(tools[0]["parameters"].is_object());
    }

    #[test]
    fn body_trims_oldest_input_items_when_too_large() {
        let huge = "x".repeat(30_000);
        let messages = (0..20)
            .map(|idx| ChatMessage::user(format!("msg-{idx}:{huge}")))
            .collect::<Vec<_>>();
        let req = ChatRequest::new(messages);
        let body = openai_responses_body("gpt-5.2", req, &ChatOptions::default());
        let body_bytes = serde_json::to_vec(&body).unwrap().len();
        let input = body["input"].as_array().unwrap();
        assert!(body_bytes <= OPENAI_OAUTH_MAX_BODY_BYTES);
        assert!(input.len() < 20);
        assert!(input.first().and_then(|item| item.get("role")).is_some());
        assert!(
            input
                .last()
                .and_then(|item| item.get("content"))
                .and_then(Value::as_array)
                .and_then(|content| content.first())
                .and_then(|part| part.get("text"))
                .and_then(Value::as_str)
                .unwrap()
                .contains("msg-19:")
        );
    }

    #[test]
    fn body_falls_back_to_default_instructions_when_no_system() {
        let req = ChatRequest::new(vec![ChatMessage::user("hi")]);
        let body = openai_responses_body("gpt-5.2", req, &ChatOptions::default());
        assert_eq!(body["instructions"], json!(DEFAULT_INSTRUCTIONS));
    }

    #[test]
    fn body_inserts_placeholder_when_input_is_empty() {
        let req = ChatRequest::new(vec![ChatMessage::system("only system")]);
        let body = openai_responses_body("gpt-5.2", req, &ChatOptions::default());
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], json!("user"));
        assert_eq!(input[0]["content"][0]["text"], json!("[Continue]"));
    }

    #[test]
    fn assistant_tool_call_becomes_flat_function_call_item() {
        let mut req = ChatRequest::new(vec![]);
        let parts = vec![
            ContentPart::Text("calling now".to_string()),
            ContentPart::ToolCall(ToolCall {
                call_id: "abc".to_string(),
                fn_name: "lookup".to_string(),
                fn_arguments: json!({"q": "x"}),
                thought_signatures: None,
            }),
        ];
        req = req.append_message(ChatMessage::assistant(MessageContent::from_parts(parts)));
        let body = openai_responses_body("gpt-5.2", req, &ChatOptions::default());
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2, "assistant text + function_call → 2 items");
        assert_eq!(input[0]["role"], json!("assistant"));
        assert_eq!(input[1]["type"], json!("function_call"));
        assert_eq!(input[1]["call_id"], json!("abc"));
        assert_eq!(input[1]["name"], json!("lookup"));
        // arguments must be a JSON string (Codex expects serialized JSON)
        assert!(input[1]["arguments"].is_string());
    }

    #[test]
    fn tool_role_message_becomes_function_call_output() {
        use genai::chat::ToolResponse;
        let mut req = ChatRequest::new(vec![]);
        let message = ChatMessage {
            role: ChatRole::Tool,
            content: MessageContent::from_parts(vec![ContentPart::ToolResponse(
                ToolResponse::new("abc".to_string(), "{\"ok\":true}".to_string()),
            )]),
            options: None,
        };
        req = req.append_message(message);
        let body = openai_responses_body("gpt-5.2", req, &ChatOptions::default());
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], json!("function_call_output"));
        assert_eq!(input[0]["call_id"], json!("abc"));
        assert_eq!(input[0]["output"], json!("{\"ok\":true}"));
    }

    #[test]
    fn sse_parser_handles_event_and_data_blocks() {
        let raw = "event: response.created\ndata: {\"type\":\"response.created\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\"}\n\n";
        let events = iter_sse_events(raw);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, "response.created");
        assert_eq!(events[1].0, "response.completed");
    }

    #[test]
    fn parse_streamed_response_assembles_text_deltas() {
        let raw = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hel\"}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\"}\n\n";
        let payload = parse_streamed_response(raw).unwrap();
        let output = payload["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["content"][0]["text"], json!("hello"));
    }

    #[test]
    fn parse_streamed_response_errors_on_failed_stream() {
        let raw = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"status\":\"in_progress\"}}\n\nevent: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"context_length_exceeded\"}}\n\nevent: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"message\":\"context_length_exceeded\"}}}\n\n";
        let err = parse_streamed_response(raw).unwrap_err().to_string();
        assert!(err.contains("context_length_exceeded"));
    }

    #[test]
    fn response_to_chat_response_extracts_text_and_tool_calls() {
        let payload = json!({
            "output": [
                {
                    "type": "message",
                    "content": [{"type": "output_text", "text": "Sure."}]
                },
                {
                    "type": "function_call",
                    "id": "fc-1",
                    "name": "lookup",
                    "arguments": "{\"q\":\"x\"}"
                }
            ],
            "usage": {"input_tokens": 100, "output_tokens": 20, "total_tokens": 120}
        });
        let response = openai_response_to_chat_response(payload, "gpt-5.2").unwrap();
        let parts = response.content.into_parts();
        assert!(matches!(parts[0], ContentPart::Text(ref t) if t == "Sure."));
        match &parts[1] {
            ContentPart::ToolCall(call) => {
                assert_eq!(call.call_id, "fc-1");
                assert_eq!(call.fn_name, "lookup");
                assert_eq!(call.fn_arguments, json!({"q": "x"}));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
        assert_eq!(response.usage.prompt_tokens, Some(100));
        assert_eq!(response.usage.completion_tokens, Some(20));
        assert_eq!(response.usage.total_tokens, Some(120));
    }

    #[test]
    fn jwt_account_id_extraction_handles_direct_and_org_paths() {
        let claims = json!({"chatgpt_account_id": "acct_123"});
        assert_eq!(
            extract_account_id_from_claims(&claims).as_deref(),
            Some("acct_123")
        );
        let nested = json!({JWT_ACCOUNT_PATH: {"chatgpt_account_id": "acct_nested"}});
        assert_eq!(
            extract_account_id_from_claims(&nested).as_deref(),
            Some("acct_nested")
        );
        let orgs = json!({"organizations": [{"id": "org_fallback"}]});
        assert_eq!(
            extract_account_id_from_claims(&orgs).as_deref(),
            Some("org_fallback")
        );
        assert_eq!(extract_account_id_from_claims(&json!({})), None);
    }

    // Env-rewriter tests live in `oauth_env::tests` — that's the shared
    // helper both login flows go through.

    #[test]
    fn token_file_round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("openai_oauth_tokens.json");
        let tokens = OpenAiOAuthTokens {
            access_token: Some("at".to_string()),
            refresh_token: Some("rt".to_string()),
            expires_at: Some(1_700_000_000.0),
            account_id: Some("acct".to_string()),
            env_access_token: false,
        };
        write_openai_oauth_tokens(&path, &tokens).unwrap();
        let loaded = read_openai_oauth_tokens(&path).unwrap();
        assert_eq!(loaded.access_token.as_deref(), Some("at"));
        assert_eq!(loaded.refresh_token.as_deref(), Some("rt"));
        assert_eq!(loaded.account_id.as_deref(), Some("acct"));
        assert_eq!(loaded.expires_at, Some(1_700_000_000.0));
    }
}
