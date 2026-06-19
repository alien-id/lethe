use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use genai::Client;
use genai::adapter::AdapterKind;
use genai::chat::{
    BinarySource, CacheControl, ChatMessage, ChatOptions, ChatRequest, ChatResponse, ChatRole,
    ChatStreamEvent, ContentPart, MessageContent, PromptTokensDetails, ToolCall, ToolResponse,
    Usage,
};
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{ModelIden, ServiceTarget};
use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{Mutex, Semaphore};

/// Callback for streamed assistant text. Invoked once per chunk in the
/// order the provider emits them; chunks may be a single token or
/// several. Implementations must be cheap and non-blocking.
pub type DeltaCallback<'a> = &'a (dyn Fn(&str) + Send + Sync);

use crate::config::Settings;

const OPENROUTER_ENDPOINT: &str = "https://openrouter.ai/api/v1/";
const OPENAI_ENDPOINT: &str = "https://api.openai.com/v1/";
const ANTHROPIC_ENDPOINT: &str = "https://api.anthropic.com/v1/";
const OPENCODE_GO_ENDPOINT: &str = "https://opencode.ai/zen/go/v1/";
pub(crate) const ANTHROPIC_OAUTH_TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
pub(crate) const ANTHROPIC_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_CODE_VERSION: &str = "2.1.117";
const CLAUDE_CODE_SALT: &str = "59cf53e54c78";
static LLM_DEBUG_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmRole {
    System,
    User,
    Assistant,
}

/// Cache hint that survives the LlmMessage abstraction. Mirrors the subset of
/// genai's CacheControl we actually use. Mapped at the boundary in
/// `into_chat_message`. Other providers ignore the hint.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheHint {
    /// Standard 5-minute ephemeral cache. Use for content that may shift
    /// turn-to-turn but should still cache for quick follow-ups.
    Ephemeral,
    /// 1-hour extended cache (Anthropic `{type: ephemeral, ttl: "1h"}`,
    /// via our vendored genai patch). Use for the long-stable prefix
    /// (identity, persona, instructions) so it survives gaps between user
    /// replies on an always-on assistant.
    Persistent,
}

/// A tool call recorded earlier (either in this turn's iterations or in a
/// previous turn loaded from history). Carries the id we use to pair with
/// the matching tool response when sending the next request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HistoricalToolCall {
    pub call_id: String,
    pub fn_name: String,
    pub fn_arguments: serde_json::Value,
    /// Reasoning continuation tokens emitted by thinking-capable models
    /// (currently Gemini). Round-tripped so multi-turn tool chains preserve
    /// the model's internal reasoning state. `None` for providers that
    /// don't use this mechanism.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signatures: Option<Vec<String>>,
}

/// Tool result associated with a previously emitted tool_use_id.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HistoricalToolResponse {
    pub call_id: String,
    pub content: String,
    /// Persistent message id of the tool result row, when known. Lets us
    /// archive the full text and leave a `conversation_get(message_id=...)`
    /// reference that the agent can resolve back to the original payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_message_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: LlmRole,
    pub content: String,
    #[serde(default)]
    pub attachments: Vec<LlmAttachment>,
    /// Tool calls emitted by an assistant message. Empty for plain replies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<HistoricalToolCall>,
    /// Tool results returned to the model. Carried on a user-role message so
    /// the wire format (Anthropic `tool_result` blocks / OpenAI `tool` role)
    /// pairs correctly with the preceding assistant message's tool_use blocks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_responses: Vec<HistoricalToolResponse>,
    /// Per-message cache hint. Set on system messages we want to mark as a
    /// cache breakpoint; ignored by providers that don't support caching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheHint>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LlmAttachment {
    pub content_type: String,
    pub base64_content: String,
    pub name: Option<String>,
}

impl LlmMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: LlmRole::System,
            content: content.into(),
            attachments: vec![],
            tool_calls: vec![],
            tool_responses: vec![],
            cache_control: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: LlmRole::User,
            content: content.into(),
            attachments: vec![],
            tool_calls: vec![],
            tool_responses: vec![],
            cache_control: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: LlmRole::Assistant,
            content: content.into(),
            attachments: vec![],
            tool_calls: vec![],
            tool_responses: vec![],
            cache_control: None,
        }
    }

    pub fn user_with_attachments(
        content: impl Into<String>,
        attachments: Vec<LlmAttachment>,
    ) -> Self {
        Self {
            role: LlmRole::User,
            content: content.into(),
            attachments,
            tool_calls: vec![],
            tool_responses: vec![],
            cache_control: None,
        }
    }

    /// Assistant message that emitted tool calls. `content` is the
    /// accompanying text (often empty when the model went straight to tools).
    pub fn assistant_with_tool_calls(
        content: impl Into<String>,
        tool_calls: Vec<HistoricalToolCall>,
    ) -> Self {
        Self {
            role: LlmRole::Assistant,
            content: content.into(),
            attachments: vec![],
            tool_calls,
            tool_responses: vec![],
            cache_control: None,
        }
    }

    /// User-role message carrying tool results that follow an
    /// `assistant_with_tool_calls`. Anthropic and OpenAI both expect tool
    /// results paired with their tool_use_id; the user role is the wrapper.
    pub fn tool_results(tool_responses: Vec<HistoricalToolResponse>) -> Self {
        Self {
            role: LlmRole::User,
            content: String::new(),
            attachments: vec![],
            tool_calls: vec![],
            tool_responses,
            cache_control: None,
        }
    }

    pub fn with_cache_control(mut self, hint: CacheHint) -> Self {
        self.cache_control = Some(hint);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LlmRouterConfig {
    pub model: String,
    pub aux_model: String,
    /// Optional dedicated model for tool chains. Empty = no mid-turn switch.
    pub tool_model: String,
    pub provider: String,
    pub api_base: String,
    pub max_output_tokens: u32,
    pub temperature_millidegrees: u32,
}

impl LlmRouterConfig {
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            model: settings.llm.llm_model.clone(),
            aux_model: settings.effective_aux_model().to_string(),
            tool_model: settings.effective_tool_model().to_string(),
            provider: settings.llm.llm_provider.clone(),
            api_base: settings.llm.llm_api_base.clone(),
            max_output_tokens: settings.llm.llm_max_output,
            temperature_millidegrees: 700,
        }
    }

    pub fn model_for(&self, use_aux: bool) -> &str {
        if use_aux && !self.aux_model.trim().is_empty() {
            &self.aux_model
        } else {
            &self.model
        }
    }

    /// The dedicated tool-chain model id (may be empty).
    pub fn tool_model(&self) -> &str {
        self.tool_model.trim()
    }

    /// Whether a dedicated tool-chain model is configured and actually differs
    /// from the primary model (otherwise switching to it would be a no-op).
    pub fn has_tool_model(&self) -> bool {
        let tool = self.tool_model.trim();
        !tool.is_empty() && tool != self.model.trim()
    }

    pub fn chat_options(&self) -> ChatOptions {
        ChatOptions::default()
            .with_max_tokens(self.max_output_tokens)
            .with_temperature(self.temperature_millidegrees as f64 / 1000.0)
    }
}

#[derive(Clone)]
pub struct LlmRouter {
    client: Client,
    config: LlmRouterConfig,
    anthropic_oauth: Option<AnthropicOAuthClient>,
    openai_oauth: Option<crate::llm::openai_oauth::OpenAiOAuthClient>,
}

impl LlmRouter {
    pub fn new(config: LlmRouterConfig) -> Self {
        let client = build_client(&config);
        let anthropic_oauth = AnthropicOAuthClient::from_env();
        let openai_oauth = crate::llm::openai_oauth::OpenAiOAuthClient::from_env();
        Self {
            client,
            config,
            anthropic_oauth,
            openai_oauth,
        }
    }

    pub fn config(&self) -> &LlmRouterConfig {
        &self.config
    }

    pub async fn complete(&self, messages: Vec<LlmMessage>, use_aux: bool) -> Result<String> {
        let response = self
            .exec_chat_request(build_chat_request(messages), use_aux)
            .await?;

        Ok(response.into_first_text().unwrap_or_default())
    }

    /// Streaming variant of `exec_chat_request`. Calls `on_delta` for each
    /// assistant text chunk as it arrives, then returns the complete
    /// `ChatResponse` (with any tool calls) once the stream finishes.
    ///
    /// Streams on Anthropic OAuth (`content_block_delta`/`text_delta`) and
    /// OpenAI OAuth (`response.output_text.delta`). The genai-native path
    /// falls back to non-streaming with a single replay delta, so clients
    /// still receive the text via the same channel.
    pub async fn exec_chat_request_stream(
        &self,
        request: ChatRequest,
        use_aux: bool,
        on_delta: DeltaCallback<'_>,
        on_reasoning: Option<DeltaCallback<'_>>,
    ) -> Result<ChatResponse> {
        let model = self.config.model_for(use_aux).to_string();
        self.exec_chat_request_stream_with_model(request, &model, on_delta, on_reasoning)
            .await
    }

    /// Like [`Self::exec_chat_request_stream`] but with an explicit model id,
    /// used by the agent loop's dynamic tool-model router so a single turn can
    /// switch models mid-flight.
    pub async fn exec_chat_request_stream_with_model(
        &self,
        request: ChatRequest,
        model: &str,
        on_delta: DeltaCallback<'_>,
        on_reasoning: Option<DeltaCallback<'_>>,
    ) -> Result<ChatResponse> {
        let model = model.trim();
        if model.is_empty() {
            bail!("LLM_MODEL is not set.");
        }
        let use_aux = model != self.config.model.trim();
        let options = self.config.chat_options();
        if let Some(oauth) = &self.openai_oauth
            && should_use_openai_oauth(model, &self.config)
        {
            return oauth
                .exec_chat_request_stream(model, request, &options, on_delta)
                .await
                .with_context(|| format!("LLM streaming chat request failed for model {model}"));
        }
        if let Some(oauth) = &self.anthropic_oauth
            && should_use_anthropic_oauth(model, &self.config)
        {
            return oauth
                .exec_chat_request_stream(model, request, &options, on_delta)
                .await
                .with_context(|| format!("LLM streaming chat request failed for model {model}"));
        }
        // genai-native path (OpenAI-compatible providers, incl. OpenRouter): stream
        // through the genai client so assistant text reaches `on_delta` token-by-token.
        // We turn on the StreamEnd captures so that, once the stream finishes, we can
        // rebuild the same ChatResponse (text + tool calls + usage) the non-streaming
        // path would have returned — the rest of the agent loop is unchanged.
        let options = options
            .with_capture_content(true)
            .with_capture_tool_calls(true)
            .with_capture_usage(true)
            .with_capture_reasoning_content(true);
        let request_log =
            llm_debug_request_payload("genai-stream", model, use_aux, &request, &options);
        let stream_response = match self
            .client
            .exec_chat_stream(model, request.clone(), Some(&options))
            .await
        {
            Ok(response) => response,
            Err(error) => {
                // Pre-stream failure (network / rate-limit blip before any bytes
                // arrived): fall back to the non-streaming path so the user still
                // gets the reply, surfaced via a single delta.
                let error = anyhow::Error::new(error).context(format!(
                    "LLM streaming chat request failed for model {model}"
                ));
                log_llm_interaction(
                    "chat_stream_error",
                    model,
                    request_log,
                    json!({ "ok": false, "error": format!("{error:#}") }),
                );
                let response = self.exec_chat_request_with_model(request, model).await?;
                if let Some(text) = response.first_text() {
                    on_delta(text);
                }
                return Ok(response);
            }
        };

        let model_iden = stream_response.model_iden.clone();
        let mut stream = stream_response.stream;
        let mut captured_content: Option<MessageContent> = None;
        let mut captured_usage: Option<Usage> = None;
        let mut reasoning_content: Option<String> = None;
        let mut stream_error: Option<anyhow::Error> = None;
        while let Some(event) = stream.next().await {
            match event {
                Ok(ChatStreamEvent::Chunk(chunk)) => {
                    if !chunk.content.is_empty() {
                        on_delta(&chunk.content);
                    }
                }
                Ok(ChatStreamEvent::ReasoningChunk(chunk)) => {
                    // Stream thinking tokens on the separate reasoning channel so
                    // clients can show a live "thinking…" indicator instead of
                    // dead-air while the model reasons before answering.
                    if let Some(on_reasoning) = on_reasoning
                        && !chunk.content.is_empty()
                    {
                        on_reasoning(&chunk.content);
                    }
                }
                Ok(ChatStreamEvent::End(end)) => {
                    captured_content = end.captured_content;
                    captured_usage = end.captured_usage;
                    reasoning_content = end.captured_reasoning_content;
                }
                // Start / ThoughtSignatureChunk / ToolCallChunk: tool calls are
                // recovered from the captured content at End.
                Ok(_) => {}
                Err(error) => {
                    // Mid-stream failure: some deltas may already be on screen, so
                    // we don't retry (that would duplicate text) — surface the error.
                    stream_error = Some(anyhow::Error::new(error).context(format!(
                        "LLM streaming chat request failed for model {model}"
                    )));
                    break;
                }
            }
        }

        if let Some(error) = stream_error {
            log_llm_interaction(
                "chat_stream_error",
                model,
                request_log,
                json!({ "ok": false, "error": format!("{error:#}") }),
            );
            return Err(error);
        }

        let response = ChatResponse {
            content: captured_content.unwrap_or_else(|| MessageContent::from_text(String::new())),
            reasoning_content,
            model_iden: model_iden.clone(),
            provider_model_iden: model_iden,
            usage: captured_usage.unwrap_or_default(),
            captured_raw_body: None,
        };
        log_llm_interaction(
            "chat",
            model,
            request_log,
            json!({ "ok": true, "response": &response }),
        );
        Ok(response)
    }

    pub async fn exec_chat_request(
        &self,
        request: ChatRequest,
        use_aux: bool,
    ) -> Result<ChatResponse> {
        let model = self.config.model_for(use_aux).to_string();
        self.exec_chat_request_with_model(request, &model).await
    }

    /// Like [`Self::exec_chat_request`] but with an explicit model id, used by
    /// the agent loop's dynamic tool-model router.
    pub async fn exec_chat_request_with_model(
        &self,
        request: ChatRequest,
        model: &str,
    ) -> Result<ChatResponse> {
        let model = model.trim();
        if model.is_empty() {
            bail!(
                "LLM_MODEL is not set. Run `lethe init` for guided setup, or \
                 set LLM_MODEL=<id> in your environment (see .env.example for \
                 known ids and provider keys)."
            );
        }
        let use_aux = model != self.config.model.trim();

        let options = self.config.chat_options();
        if let Some(oauth) = &self.openai_oauth
            && should_use_openai_oauth(model, &self.config)
        {
            return oauth
                .exec_chat_request(model, request, &options)
                .await
                .with_context(|| format!("LLM chat request failed for model {model}"));
        }
        if let Some(oauth) = &self.anthropic_oauth
            && should_use_anthropic_oauth(model, &self.config)
        {
            return oauth
                .exec_chat_request(model, request, &options)
                .await
                .with_context(|| format!("LLM chat request failed for model {model}"));
        }

        let request_log = llm_debug_request_payload("genai", model, use_aux, &request, &options);
        // Retry transient upstream failures (HTTP 429 rate limits, 5xx, network
        // blips). OpenRouter rate-limits popular shared-pool models, so a brief
        // spike should self-heal rather than surface as a hard error. Permanent
        // errors (400/401/403/404, malformed request) are returned immediately.
        let mut last_error: Option<anyhow::Error> = None;
        for attempt in 0..GENAI_MAX_ATTEMPTS {
            match self
                .client
                .exec_chat(model, request.clone(), Some(&options))
                .await
            {
                Ok(response) => {
                    log_llm_interaction(
                        "chat",
                        model,
                        request_log.clone(),
                        json!({ "ok": true, "response": &response }),
                    );
                    return Ok(response);
                }
                Err(error) => {
                    let retryable = genai_error_is_retryable(&error);
                    let error = anyhow::Error::new(error)
                        .context(format!("LLM chat request failed for model {model}"));
                    if retryable && attempt + 1 < GENAI_MAX_ATTEMPTS {
                        let wait = genai_retry_backoff(attempt);
                        tracing::warn!(
                            attempt = attempt + 1,
                            wait_ms = wait.as_millis() as u64,
                            error = %format!("{error:#}"),
                            "retryable LLM error — backing off and retrying"
                        );
                        last_error = Some(error);
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    log_llm_interaction(
                        "chat_error",
                        model,
                        request_log.clone(),
                        json!({ "ok": false, "error": format!("{error:#}") }),
                    );
                    return Err(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("LLM chat request failed for model {model}")))
    }
}

/// How many times to attempt a genai (OpenAI-compatible / OpenRouter) chat
/// request before giving up. Mirrors the Anthropic OAuth path's retry budget.
const GENAI_MAX_ATTEMPTS: u32 = 3;

/// Whether an HTTP status is worth retrying: rate limits (429) and transient
/// upstream failures (5xx). Permanent client errors (4xx other than 429) are not.
fn is_retryable_status(code: u16) -> bool {
    code == 429 || (500..=504).contains(&code)
}

/// Whether a genai error is transient and worth retrying. Matches the
/// structured `HttpError` status where available, and otherwise falls back to
/// the rendered message (covers `WebModelCall`/webc `ResponseFailedStatus` and
/// reqwest timeout/connection errors, plus OpenRouter's "rate-limited upstream").
fn genai_error_is_retryable(error: &genai::Error) -> bool {
    if let genai::Error::HttpError { status, .. } = error {
        return is_retryable_status(status.as_u16());
    }
    let s = format!("{error}").to_ascii_lowercase();
    if s.contains("429")
        || s.contains("too many requests")
        || s.contains("rate-limited")
        || s.contains("rate limit")
        || s.contains("rate_limit")
    {
        return true;
    }
    if [
        "'500'",
        "'502'",
        "'503'",
        "'504'",
        "internal server error",
        "bad gateway",
        "service unavailable",
        "gateway timeout",
        "overloaded",
    ]
    .iter()
    .any(|m| s.contains(m))
    {
        return true;
    }
    s.contains("timed out")
        || s.contains("timeout")
        || s.contains("connection reset")
        || s.contains("connection closed")
        || s.contains("connection refused")
        || s.contains("error sending request")
        || s.contains("dns error")
}

/// Capped exponential backoff between genai retry attempts: ~1s, then ~2s.
fn genai_retry_backoff(attempt: u32) -> Duration {
    let secs = 1u64 << attempt.min(2); // 1, 2, 4
    Duration::from_millis((secs * 1000).min(4000))
}

#[derive(Clone)]
struct AnthropicOAuthClient {
    http: reqwest::Client,
    token_file: PathBuf,
    tokens: Arc<Mutex<AnthropicOAuthTokens>>,
    request_gate: Arc<Semaphore>,
    rate_limit_until: Arc<Mutex<Option<Instant>>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct AnthropicOAuthTokens {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_at: Option<f64>,
    #[serde(skip)]
    env_access_token: bool,
}

#[derive(Debug, Error)]
enum AnthropicOAuthError {
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

impl AnthropicOAuthClient {
    fn from_env() -> Option<Self> {
        let token_file = anthropic_oauth_token_file();
        let tokens = if let Ok(access_token) = env::var("ANTHROPIC_AUTH_TOKEN") {
            let access_token = access_token.trim().to_string();
            if access_token.is_empty() {
                None
            } else {
                Some(AnthropicOAuthTokens {
                    access_token: Some(access_token),
                    env_access_token: true,
                    ..Default::default()
                })
            }
        } else {
            read_anthropic_oauth_tokens(&token_file)
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

    async fn exec_chat_request(
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
                Err(error @ AnthropicOAuthError::RateLimited { retry_after, .. }) => {
                    last_error = Some(error);
                    tokio::time::sleep(retry_after).await;
                }
                Err(error @ AnthropicOAuthError::Transient { retry_after, .. }) => {
                    last_error = Some(error);
                    let wait = retry_after.max(Duration::from_secs(5 * (attempt + 1) as u64));
                    tokio::time::sleep(wait).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(last_error
            .map(anyhow::Error::from)
            .unwrap_or_else(|| anyhow!("Anthropic OAuth call failed")))
    }

    /// Streams the response. We don't retry the streaming call — partial
    /// text may already be visible to the user, so re-running would
    /// duplicate. On retryable errors before the first byte we fall back
    /// to the non-streaming retry path.
    async fn exec_chat_request_stream(
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
            Err(
                AnthropicOAuthError::RateLimited { .. } | AnthropicOAuthError::Transient { .. },
            ) => {
                // Pre-stream failure (rate limit / network blip before we
                // got bytes back) — let the non-streaming path retry and
                // surface the full text in one delta.
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
    ) -> Result<ChatResponse, AnthropicOAuthError> {
        self.ensure_access().await?;

        let access_token = {
            let tokens = self.tokens.lock().await;
            tokens
                .access_token
                .clone()
                .ok_or_else(|| anyhow!("Anthropic OAuth access token is missing"))?
        };
        let body = anthropic_request_body(model, request, options);
        let request_log = json!({
            "auth": "anthropic_oauth",
            "endpoint": format!("{ANTHROPIC_MESSAGES_URL}?beta=true"),
            "model": model,
            "body": body.clone(),
        });
        let headers = anthropic_oauth_headers(&access_token);

        let _permit = self
            .request_gate
            .acquire()
            .await
            .map_err(|error| anyhow!("Anthropic OAuth request gate closed: {error}"))?;
        self.wait_for_rate_limit().await;

        let response = self
            .http
            .post(format!("{ANTHROPIC_MESSAGES_URL}?beta=true"))
            .headers(headers)
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                log_llm_interaction(
                    "chat_anthropic_oauth_error",
                    model,
                    request_log.clone(),
                    json!({
                        "ok": false,
                        "error": format!("Anthropic OAuth request failed: {error}"),
                    }),
                );
                AnthropicOAuthError::Transient {
                    message: format!("Anthropic OAuth request failed: {error}"),
                    retry_after: Duration::from_secs(5),
                }
            })?;

        let status = response.status();
        let headers = response.headers().clone();
        let text = response
            .text()
            .await
            .map_err(|error| anyhow!("Anthropic OAuth response read failed: {error}"))?;
        log_llm_interaction(
            if status.is_success() {
                "chat_anthropic_oauth"
            } else {
                "chat_anthropic_oauth_error"
            },
            model,
            request_log,
            json!({
                "ok": status.is_success(),
                "status": status.as_u16(),
                "body": serde_json::from_str::<Value>(&text)
                    .unwrap_or_else(|_| json!({"raw_text": text.clone()})),
            }),
        );

        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry_after = retry_after_from_headers(&headers);
            self.set_rate_limit(retry_after).await;
            return Err(AnthropicOAuthError::RateLimited {
                message: format!(
                    "Anthropic OAuth rate limited (429) - {}",
                    truncate_error(&text)
                ),
                retry_after,
            });
        }

        if status == StatusCode::FORBIDDEN
            && (text.contains("permission_error") || text.contains("OAuth authentication"))
        {
            let retry_after = retry_after_from_headers(&headers);
            self.set_rate_limit(retry_after).await;
            return Err(AnthropicOAuthError::RateLimited {
                message: format!(
                    "Anthropic OAuth throttled (403 permission_error) - {}",
                    truncate_error(&text)
                ),
                retry_after,
            });
        }

        if status.as_u16() == 529 {
            return Err(AnthropicOAuthError::Transient {
                message: format!(
                    "Anthropic OAuth overloaded (529) - {}",
                    truncate_error(&text)
                ),
                retry_after: Duration::from_secs(5),
            });
        }

        if !status.is_success() {
            return Err(anyhow!(
                "Anthropic OAuth API error: {} - {}",
                status.as_u16(),
                truncate_error(&text)
            )
            .into());
        }

        let data: Value = serde_json::from_str(&text).with_context(|| {
            format!(
                "invalid Anthropic OAuth response JSON: {}",
                truncate_error(&text)
            )
        })?;
        anthropic_response_to_chat_response(data, model).map_err(Into::into)
    }

    /// Issue a streaming Anthropic Messages call and forward text deltas
    /// to `on_delta`. Returns a fully-assembled `ChatResponse` matching
    /// what the non-streaming path produces (text + tool_use blocks).
    async fn call_messages_stream(
        &self,
        model: &str,
        request: ChatRequest,
        options: &ChatOptions,
        on_delta: DeltaCallback<'_>,
    ) -> Result<ChatResponse, AnthropicOAuthError> {
        self.ensure_access().await?;

        let access_token = {
            let tokens = self.tokens.lock().await;
            tokens
                .access_token
                .clone()
                .ok_or_else(|| anyhow!("Anthropic OAuth access token is missing"))?
        };

        // Same body as the non-streaming path, with stream:true. The API
        // returns SSE frames; everything else (system prompt cache, tools,
        // message merge) is unchanged.
        let mut body = anthropic_request_body(model, request, options);
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".to_string(), json!(true));
        }
        let mut headers = anthropic_oauth_headers(&access_token);
        headers.insert("accept", "text/event-stream".parse().unwrap());

        let _permit = self
            .request_gate
            .acquire()
            .await
            .map_err(|error| anyhow!("Anthropic OAuth request gate closed: {error}"))?;
        self.wait_for_rate_limit().await;

        let response = self
            .http
            .post(format!("{ANTHROPIC_MESSAGES_URL}?beta=true"))
            .headers(headers)
            .json(&body)
            .send()
            .await
            .map_err(|error| AnthropicOAuthError::Transient {
                message: format!("Anthropic OAuth stream request failed: {error}"),
                retry_after: Duration::from_secs(5),
            })?;

        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry_after = retry_after_from_headers(response.headers());
            self.set_rate_limit(retry_after).await;
            let text = response.text().await.unwrap_or_default();
            return Err(AnthropicOAuthError::RateLimited {
                message: format!(
                    "Anthropic OAuth stream rate limited (429) - {}",
                    truncate_error(&text)
                ),
                retry_after,
            });
        }
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Anthropic OAuth stream error: {} - {}",
                status.as_u16(),
                truncate_error(&text)
            )
            .into());
        }

        let mut state = AnthropicStreamState::new(model);
        let mut events = response.bytes_stream().eventsource();
        while let Some(event) = events.next().await {
            let event = event.map_err(|error| AnthropicOAuthError::Transient {
                message: format!("Anthropic OAuth stream decode failed: {error}"),
                retry_after: Duration::from_secs(2),
            })?;
            if event.data.is_empty() {
                continue;
            }
            let payload: Value = match serde_json::from_str(&event.data) {
                Ok(value) => value,
                Err(_) => continue,
            };
            state.apply(&payload, on_delta);
            if state.done {
                break;
            }
        }
        Ok(state.into_response())
    }

    async fn ensure_access(&self) -> Result<(), AnthropicOAuthError> {
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

        let response = self
            .http
            .post(ANTHROPIC_OAUTH_TOKEN_URL)
            .json(&json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "client_id": ANTHROPIC_OAUTH_CLIENT_ID,
            }))
            .send()
            .await
            .map_err(|error| AnthropicOAuthError::Transient {
                message: format!("Anthropic OAuth token refresh failed: {error}"),
                retry_after: Duration::from_secs(5),
            })?;

        let status = response.status();
        let text = response.text().await.map_err(|error| {
            anyhow!("Anthropic OAuth token refresh response read failed: {error}")
        })?;
        if !status.is_success() {
            return Err(anyhow!(
                "Anthropic OAuth token refresh failed: {} {}",
                status.as_u16(),
                truncate_error(&text)
            )
            .into());
        }

        let data: Value = serde_json::from_str(&text).with_context(|| {
            format!(
                "invalid Anthropic OAuth refresh JSON: {}",
                truncate_error(&text)
            )
        })?;
        let access_token = data
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Anthropic OAuth refresh response is missing access_token"))?
            .to_string();
        let refresh_token = data
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string);
        let expires_in = data
            .get("expires_in")
            .and_then(Value::as_f64)
            .unwrap_or(3600.0);

        let snapshot = {
            let mut tokens = self.tokens.lock().await;
            tokens.access_token = Some(access_token);
            if let Some(refresh_token) = refresh_token {
                tokens.refresh_token = Some(refresh_token);
            }
            tokens.expires_at = Some(unix_now_seconds() + expires_in);
            tokens.clone()
        };
        write_anthropic_oauth_tokens(&self.token_file, &snapshot)?;
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

impl AnthropicOAuthTokens {
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

fn build_client(config: &LlmRouterConfig) -> Client {
    let resolver_config = config.clone();
    let target_resolver = ServiceTargetResolver::from_resolver_fn(
        move |service_target: ServiceTarget| -> genai::resolver::Result<ServiceTarget> {
            let raw_model = service_target.model.model_name.to_string();
            let Some(target) = router_target_for_model(&raw_model, &resolver_config) else {
                return Ok(service_target);
            };
            Ok(target.into_service_target())
        },
    );
    Client::builder()
        .with_service_target_resolver(target_resolver)
        .build()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RouterTarget {
    endpoint: String,
    auth_env: String,
    adapter: AdapterKind,
    model_name: String,
}

impl RouterTarget {
    fn into_service_target(self) -> ServiceTarget {
        ServiceTarget {
            endpoint: Endpoint::from_owned(self.endpoint),
            auth: AuthData::from_env(self.auth_env),
            model: ModelIden::new(self.adapter, self.model_name),
        }
    }
}

fn router_target_for_model(raw_model: &str, config: &LlmRouterConfig) -> Option<RouterTarget> {
    let model = raw_model.trim();
    if model.is_empty() {
        return None;
    }

    let api_base = normalize_api_base(&config.api_base);
    let slash_provider = slash_provider(model);
    let configured_provider = normalized_provider(&config.provider);
    let provider = slash_provider
        .or(configured_provider.as_deref())
        .unwrap_or(if api_base.is_some() { "openai" } else { "" });
    if provider.is_empty() {
        return None;
    }

    if let Some(endpoint) = api_base {
        return Some(RouterTarget {
            endpoint,
            auth_env: auth_env_for_provider(provider).to_string(),
            adapter: adapter_for_provider(provider).unwrap_or(AdapterKind::OpenAI),
            model_name: strip_slash_provider(model, provider).to_string(),
        });
    }

    if provider == "openrouter" || slash_provider == Some("openrouter") {
        return Some(RouterTarget {
            endpoint: OPENROUTER_ENDPOINT.to_string(),
            auth_env: "OPENROUTER_API_KEY".to_string(),
            adapter: AdapterKind::OpenAI,
            model_name: strip_slash_provider(model, "openrouter").to_string(),
        });
    }

    // OpenCode Go fast-path: single endpoint, per-model protocol from catalog.
    // Must come before the generic slash_provider branch because
    // adapter_for_provider returns None for "opencode-go" — the adapter
    // depends on the individual model's protocol, not the provider.
    if provider == "opencode-go" {
        let model_name = strip_slash_provider(model, "opencode-go").to_string();
        let protocol = crate::llm::models::protocol_for_model(model);
        let adapter = match protocol {
            "anthropic" => AdapterKind::Anthropic,
            _ => AdapterKind::OpenAI,
        };
        return Some(RouterTarget {
            endpoint: OPENCODE_GO_ENDPOINT.to_string(),
            auth_env: "OPENCODE_GO_API_KEY".to_string(),
            adapter,
            model_name,
        });
    }

    if let Some(slash_provider) = slash_provider
        && let Some(endpoint) = default_endpoint_for_provider(slash_provider)
    {
        return Some(RouterTarget {
            endpoint: endpoint.to_string(),
            auth_env: auth_env_for_provider(slash_provider).to_string(),
            adapter: adapter_for_provider(slash_provider)?,
            model_name: strip_slash_provider(model, slash_provider).to_string(),
        });
    }

    None
}

fn normalized_provider(provider: &str) -> Option<String> {
    let provider = provider.trim().to_ascii_lowercase();
    (!provider.is_empty()).then_some(provider)
}

fn normalize_api_base(api_base: &str) -> Option<String> {
    let api_base = api_base.trim();
    if api_base.is_empty() {
        return None;
    }
    if api_base.ends_with('/') {
        Some(api_base.to_string())
    } else {
        Some(format!("{api_base}/"))
    }
}

fn slash_provider(model: &str) -> Option<&str> {
    let (provider, rest) = model.split_once('/')?;
    if rest.is_empty() {
        return None;
    }
    match provider.to_ascii_lowercase().as_str() {
        "openrouter" => Some("openrouter"),
        "openai" => Some("openai"),
        "anthropic" => Some("anthropic"),
        "ollama" => Some("ollama"),
        "opencode-go" => Some("opencode-go"),
        _ => None,
    }
}

fn strip_slash_provider<'a>(model: &'a str, provider: &str) -> &'a str {
    let Some((prefix, rest)) = model.split_once('/') else {
        return model;
    };
    if prefix.eq_ignore_ascii_case(provider) && !rest.is_empty() {
        rest
    } else {
        model
    }
}

fn adapter_for_provider(provider: &str) -> Option<AdapterKind> {
    match provider {
        "openrouter" | "openai" => Some(AdapterKind::OpenAI),
        "anthropic" => Some(AdapterKind::Anthropic),
        "ollama" => Some(AdapterKind::Ollama),
        // "opencode-go" is NOT here — its adapter depends on the
        // per-model protocol field, resolved in the opencode-go fast-path
        // branch of router_target_for_model(), not here.
        _ => None,
    }
}

fn default_endpoint_for_provider(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" => Some(OPENAI_ENDPOINT),
        "anthropic" => Some(ANTHROPIC_ENDPOINT),
        "openrouter" => Some(OPENROUTER_ENDPOINT),
        "opencode-go" => Some(OPENCODE_GO_ENDPOINT),
        _ => None,
    }
}

fn auth_env_for_provider(provider: &str) -> &'static str {
    match provider {
        "openrouter" => "OPENROUTER_API_KEY",
        "anthropic" => "ANTHROPIC_API_KEY",
        "opencode-go" => "OPENCODE_GO_API_KEY",
        _ => "OPENAI_API_KEY",
    }
}

fn llm_debug_request_payload(
    auth: &str,
    model: &str,
    use_aux: bool,
    request: &ChatRequest,
    options: &ChatOptions,
) -> Value {
    json!({
        "auth": auth,
        "model": model,
        "use_aux": use_aux,
        "request": request,
        "options": options,
    })
}

pub(crate) fn log_llm_interaction(label: &str, model: &str, request: Value, response: Value) {
    if !llm_debug_enabled() {
        return;
    }

    let dir = llm_debug_dir();
    let result = (|| -> Result<PathBuf> {
        fs::create_dir_all(&dir)?;
        let sequence = LLM_DEBUG_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let timestamp = Utc::now();
        let file_name = format!(
            "{}_{:06}_{label}.json",
            timestamp.format("%Y%m%d_%H%M%S_%6f"),
            sequence
        );
        let path = dir.join(file_name);
        let payload = json!({
            "timestamp": timestamp.to_rfc3339(),
            "label": label,
            "model": model,
            "request": request,
            "response": response,
        });
        fs::write(&path, serde_json::to_string_pretty(&payload)?)?;
        Ok(path)
    })();

    match result {
        Ok(path) => tracing::debug!(path = %path.display(), label, model, "logged llm interaction"),
        Err(error) => tracing::warn!(error = %error, "failed to log llm interaction"),
    }
}

fn llm_debug_enabled() -> bool {
    env::var("LLM_DEBUG")
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn llm_debug_dir() -> PathBuf {
    if let Some(path) = env::var_os("LLM_DEBUG_DIR") {
        return PathBuf::from(path);
    }
    if let Some(path) = env::var_os("LOGS_DIR") {
        return PathBuf::from(path).join("llm");
    }
    if let Some(path) = env::var_os("LETHE_HOME") {
        return PathBuf::from(path).join("logs").join("llm");
    }
    PathBuf::from("logs").join("llm")
}

fn should_use_anthropic_oauth(model: &str, config: &LlmRouterConfig) -> bool {
    if normalize_api_base(&config.api_base).is_some() {
        return false;
    }
    if slash_provider(model) == Some("openrouter") {
        return false;
    }
    if slash_provider(model) == Some("opencode-go") {
        return false;
    }
    if normalized_provider(&config.provider).as_deref() == Some("openrouter") {
        return false;
    }
    if normalized_provider(&config.provider).as_deref() == Some("opencode-go") {
        return false;
    }
    let model = strip_slash_provider(model, "anthropic").to_ascii_lowercase();
    slash_provider(model.as_str()) == Some("anthropic")
        || normalized_provider(&config.provider).as_deref() == Some("anthropic")
        || model.contains("claude")
}

fn should_use_openai_oauth(model: &str, config: &LlmRouterConfig) -> bool {
    if normalize_api_base(&config.api_base).is_some() {
        return false;
    }
    if slash_provider(model) == Some("openrouter") {
        return false;
    }
    if slash_provider(model) == Some("opencode-go") {
        return false;
    }
    if normalized_provider(&config.provider).as_deref() == Some("openrouter") {
        return false;
    }
    if normalized_provider(&config.provider).as_deref() == Some("opencode-go") {
        return false;
    }
    // Routes to OAuth when the user has explicitly selected the openai
    // provider (either via LLM_PROVIDER=openai or a model id prefixed
    // with `openai/`). API-key paths via genai's standard OpenAI adapter
    // still work — having an OAuth token doesn't override an explicit
    // OPENAI_API_KEY for openrouter or custom api_base targets, since
    // those branches return false above.
    slash_provider(model) == Some("openai")
        || normalized_provider(&config.provider).as_deref() == Some("openai")
}

pub fn llm_auth_mode_for_settings(settings: &Settings) -> String {
    let config = LlmRouterConfig::from_settings(settings);
    let main = auth_mode_for_model(config.model_for(false), &config);
    let aux = auth_mode_for_model(config.model_for(true), &config);
    if main == aux {
        main
    } else {
        format!("main={main}, aux={aux}")
    }
}

fn auth_mode_for_model(model: &str, config: &LlmRouterConfig) -> String {
    if should_use_openai_oauth(model, config) && crate::llm::openai_oauth::openai_oauth_available()
    {
        return "openai_oauth".to_string();
    }
    if should_use_anthropic_oauth(model, config) && anthropic_oauth_available() {
        return "anthropic_oauth".to_string();
    }
    let configured_provider = normalized_provider(&config.provider);
    let provider = slash_provider(model)
        .or(configured_provider.as_deref())
        .unwrap_or(if normalize_api_base(&config.api_base).is_some() {
            "openai"
        } else if model.to_ascii_lowercase().contains("claude") {
            "anthropic"
        } else {
            ""
        });
    match provider {
        "anthropic" => "anthropic_api_key".to_string(),
        "openrouter" => "openrouter_api_key".to_string(),
        "openai" => "openai_api_key".to_string(),
        "opencode-go" => "opencode_go_api_key".to_string(),
        other if !other.is_empty() => format!("{other}_auth"),
        _ => "auto".to_string(),
    }
}

pub fn anthropic_oauth_available() -> bool {
    env::var("ANTHROPIC_AUTH_TOKEN")
        .map(|token| !token.trim().is_empty())
        .unwrap_or(false)
        || read_anthropic_oauth_tokens(&anthropic_oauth_token_file()).is_some()
}

pub(crate) fn anthropic_oauth_token_file() -> PathBuf {
    if let Some(path) = env::var_os("LETHE_ANTHROPIC_OAUTH_TOKENS") {
        return PathBuf::from(path);
    }
    if let Some(path) = env::var_os("CREDENTIALS_DIR") {
        return PathBuf::from(path).join("anthropic_oauth_tokens.json");
    }
    let home = env::var_os("LETHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".lethe")))
        .unwrap_or_else(|| PathBuf::from(".lethe"));
    home.join("credentials").join("anthropic_oauth_tokens.json")
}

fn read_anthropic_oauth_tokens(path: &Path) -> Option<AnthropicOAuthTokens> {
    let text = fs::read_to_string(path).ok()?;
    let mut tokens: AnthropicOAuthTokens = serde_json::from_str(&text).ok()?;
    tokens.env_access_token = false;
    tokens
        .access_token
        .as_ref()
        .is_some_and(|token| !token.trim().is_empty())
        .then_some(tokens)
}

fn write_anthropic_oauth_tokens(path: &Path, tokens: &AnthropicOAuthTokens) -> Result<()> {
    let Some(parent) = path.parent() else {
        bail!(
            "Anthropic OAuth token path has no parent: {}",
            path.display()
        );
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

fn truncate_error(text: &str) -> String {
    const MAX: usize = 500;
    let text = text.trim();
    if text.len() <= MAX {
        return text.to_string();
    }
    format!("{}...", &text[..MAX])
}

fn anthropic_request_body(model: &str, request: ChatRequest, options: &ChatOptions) -> Value {
    let max_tokens = options.max_tokens.unwrap_or(8000);
    let mut system_blocks: Vec<Value> = Vec::new();
    if let Some(system) = request.system.filter(|system| !system.trim().is_empty()) {
        system_blocks.push(json!({"type": "text", "text": system}));
    }

    let mut messages = Vec::new();
    for message in request.messages {
        match message.role {
            ChatRole::System => {
                // Stamp cache_control on the LAST emitted block for this
                // message so the Persistent / Ephemeral breakpoints set by
                // apply_cache_markers in agent.rs survive the OAuth path.
                // Without this every heartbeat re-pays the full system
                // prompt as fresh input.
                let cache = message.options.as_ref().and_then(|o| o.cache_control);
                let prev_len = system_blocks.len();
                for text in message.content.texts() {
                    if !text.trim().is_empty() {
                        system_blocks.push(json!({"type": "text", "text": text}));
                    }
                }
                if let Some(control) = cache
                    && system_blocks.len() > prev_len
                    && let Some(obj) = system_blocks.last_mut().and_then(Value::as_object_mut)
                {
                    obj.insert("cache_control".into(), cache_control_value(control));
                }
            }
            ChatRole::User => messages.push(json!({
                "role": "user",
                "content": anthropic_user_content(message.content),
            })),
            ChatRole::Assistant => {
                let content = anthropic_assistant_content(message.content);
                if !content.is_empty() {
                    messages.push(json!({"role": "assistant", "content": content}));
                }
            }
            ChatRole::Tool => {
                let content = anthropic_tool_result_content(message.content);
                if !content.is_empty() {
                    messages.push(json!({"role": "user", "content": content}));
                }
            }
        }
    }

    let first_user_text = first_user_text(&messages);
    system_blocks.insert(
        0,
        json!({"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."}),
    );
    system_blocks.insert(
        0,
        json!({"type": "text", "text": billing_header(&first_user_text)}),
    );

    let mut messages = merge_anthropic_messages(messages);
    drop_leading_non_user_messages(&mut messages);
    while messages
        .last()
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
        == Some("assistant")
    {
        messages.pop();
    }
    if messages.is_empty() {
        messages.push(json!({"role": "user", "content": "[No user message provided.]"}));
    }
    let mut messages = clean_orphaned_tool_pairs(messages);
    drop_leading_non_user_messages(&mut messages);
    if messages.is_empty() {
        messages.push(json!({"role": "user", "content": "[No user message provided.]"}));
    }

    // Tools intentionally carry no cache_control: a 5m marker on the
    // last tool sits before the system 1h marker in Anthropic's
    // tools → system → messages processing order, and the API rejects
    // ttl=5m occurring before ttl=1h. The system breakpoint already
    // caches everything from the start of the prompt (tools included),
    // so the tools marker is redundant anyway. Mirrors what genai's
    // vendored anthropic adapter does on the non-OAuth path.
    let tools: Vec<Value> = request
        .tools
        .unwrap_or_default()
        .into_iter()
        .map(anthropic_tool_schema)
        .collect();

    json!({
        "model": normalize_anthropic_model(model),
        "max_tokens": max_tokens,
        "system": system_blocks,
        "messages": messages,
        "tools": tools,
    })
}

fn cache_control_value(control: CacheControl) -> Value {
    match control {
        CacheControl::Ephemeral => json!({"type": "ephemeral"}),
        CacheControl::Persistent => json!({"type": "ephemeral", "ttl": "1h"}),
    }
}

fn anthropic_user_content(content: MessageContent) -> Vec<Value> {
    let mut blocks = Vec::new();
    for part in content.into_parts() {
        match part {
            ContentPart::Text(text) => {
                if !text.trim().is_empty() {
                    blocks.push(json!({"type": "text", "text": text}));
                }
            }
            ContentPart::Binary(binary) => {
                let is_image = binary.is_image();
                let content_type = binary.content_type;
                match binary.source {
                    BinarySource::Base64(data) => {
                        if is_image {
                            blocks.push(json!({
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": content_type,
                                    "data": data.as_ref(),
                                }
                            }));
                        } else {
                            blocks.push(json!({
                                "type": "document",
                                "source": {
                                    "type": "base64",
                                    "media_type": content_type,
                                    "data": data.as_ref(),
                                }
                            }));
                        }
                    }
                    BinarySource::Url(url) => {
                        if is_image {
                            blocks.push(json!({
                                "type": "image",
                                "source": {"type": "url", "url": url}
                            }));
                        } else {
                            blocks.push(json!({
                                "type": "text",
                                "text": format!("[{} attachment: {url}]", content_type),
                            }));
                        }
                    }
                }
            }
            ContentPart::ToolResponse(response) => blocks.push(json!({
                "type": "tool_result",
                "tool_use_id": response.call_id,
                "content": response.content,
            })),
            ContentPart::ToolCall(call) => blocks.push(json!({
                "type": "text",
                "text": format!("[tool call {} omitted in user content]", call.fn_name),
            })),
            ContentPart::ThoughtSignature(_) => {}
        }
    }
    if blocks.is_empty() {
        blocks.push(json!({"type": "text", "text": ""}));
    }
    blocks
}

fn anthropic_assistant_content(content: MessageContent) -> Vec<Value> {
    let mut blocks = Vec::new();
    for part in content.into_parts() {
        match part {
            ContentPart::Text(text) => {
                if !text.trim().is_empty() {
                    blocks.push(json!({"type": "text", "text": text}));
                }
            }
            ContentPart::ToolCall(call) => blocks.push(json!({
                "type": "tool_use",
                "id": call.call_id,
                "name": map_tool_name_to_claude(&call.fn_name),
                "input": call.fn_arguments,
            })),
            ContentPart::ThoughtSignature(_) => {}
            ContentPart::Binary(_) | ContentPart::ToolResponse(_) => {}
        }
    }
    blocks
}

fn anthropic_tool_result_content(content: MessageContent) -> Vec<Value> {
    let mut blocks = Vec::new();
    for part in content.into_parts() {
        match part {
            ContentPart::ToolResponse(response) => blocks.push(json!({
                "type": "tool_result",
                "tool_use_id": response.call_id,
                "content": response.content,
            })),
            ContentPart::Text(text) => {
                if !text.trim().is_empty() {
                    blocks.push(json!({"type": "text", "text": text}));
                }
            }
            ContentPart::ToolCall(_)
            | ContentPart::Binary(_)
            | ContentPart::ThoughtSignature(_) => {}
        }
    }
    blocks
}

fn anthropic_tool_schema(tool: genai::chat::Tool) -> Value {
    json!({
        "name": map_tool_name_to_claude(&tool.name),
        "description": tool.description.unwrap_or_default(),
        "input_schema": tool.schema.unwrap_or_else(|| json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })),
    })
}

fn merge_anthropic_messages(messages: Vec<Value>) -> Vec<Value> {
    let mut merged: Vec<Value> = Vec::new();
    for message in messages {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        if let Some(previous) = merged.last_mut()
            && previous.get("role").and_then(Value::as_str) == Some(role)
        {
            merge_message_content(previous, &message);
            continue;
        }
        merged.push(message);
    }
    merged
}

fn drop_leading_non_user_messages(messages: &mut Vec<Value>) {
    // Anthropic requires the conversation to begin with a user turn, so we drop
    // leading non-user messages. But a leading `assistant` message may carry
    // `tool_use` blocks whose `tool_result`s are the *next* (user-role) message;
    // dropping that assistant orphans those results, and Anthropic 400s on a
    // leading `tool_result` with no preceding `tool_use`. So after draining to
    // the first user message, drop it too if it still carries a tool_result (its
    // producing assistant is gone), then re-scan. Each step removes at least one
    // message, so this terminates. (Without the loop, the second call site —
    // after clean_orphaned_tool_pairs has shifted an assistant_tool_use to the
    // front — would re-orphan a tool_result with nothing left to clean it up.)
    loop {
        match messages
            .iter()
            .position(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        {
            None => {
                messages.clear();
                return;
            }
            Some(index) => {
                if index > 0 {
                    messages.drain(0..index);
                }
                if message_contains_tool_result(&messages[0]) {
                    messages.remove(0);
                    continue;
                }
                return;
            }
        }
    }
}

fn message_contains_tool_result(message: &Value) -> bool {
    message
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|content| {
            content
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        })
}

fn merge_message_content(previous: &mut Value, next: &Value) {
    let previous_content = previous.get_mut("content");
    let next_content = next
        .get("content")
        .cloned()
        .unwrap_or(Value::String(String::new()));
    let Some(previous_content) = previous_content else {
        previous["content"] = next_content;
        return;
    };

    let merged = match (std::mem::take(previous_content), next_content) {
        (Value::String(mut previous), Value::String(next)) => {
            previous.push('\n');
            previous.push_str(&next);
            Value::String(previous)
        }
        (Value::Array(mut previous), Value::Array(next)) => {
            previous.extend(next);
            Value::Array(previous)
        }
        (Value::String(previous), Value::Array(mut next)) => {
            let mut merged = vec![json!({"type": "text", "text": previous})];
            merged.append(&mut next);
            Value::Array(merged)
        }
        (Value::Array(mut previous), Value::String(next)) => {
            previous.push(json!({"type": "text", "text": next}));
            Value::Array(previous)
        }
        (_, next) => next,
    };
    *previous_content = merged;
}

fn clean_orphaned_tool_pairs(messages: Vec<Value>) -> Vec<Value> {
    let mut cleaned = Vec::new();
    let mut previous_tool_use_ids: Vec<String> = Vec::new();
    for message in messages {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        let Some(content) = message.get("content").and_then(Value::as_array) else {
            previous_tool_use_ids.clear();
            cleaned.push(message);
            continue;
        };

        if role == "assistant" {
            previous_tool_use_ids = content
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
                .filter_map(|block| block.get("id").and_then(Value::as_str).map(str::to_string))
                .collect();
            cleaned.push(message);
            continue;
        }

        if role == "user"
            && content
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        {
            let filtered = content
                .iter()
                .filter(|block| {
                    block.get("type").and_then(Value::as_str) != Some("tool_result")
                        || block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .is_some_and(|id| {
                                previous_tool_use_ids.iter().any(|use_id| use_id == id)
                            })
                })
                .cloned()
                .collect::<Vec<_>>();
            if !filtered.is_empty() {
                let mut message = message;
                message["content"] = Value::Array(filtered);
                cleaned.push(message);
            }
        } else {
            previous_tool_use_ids.clear();
            cleaned.push(message);
        }
    }
    cleaned
}

fn first_user_text(messages: &[Value]) -> String {
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let Some(content) = message.get("content") else {
            return String::new();
        };
        if let Some(text) = content.as_str() {
            return text.to_string();
        }
        if let Some(blocks) = content.as_array() {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(text) = block.get("text").and_then(Value::as_str)
                {
                    return text.to_string();
                }
            }
        }
    }
    String::new()
}

/// Accumulator for Anthropic SSE streams. Mirrors the content-block model
/// Anthropic exposes: `content_block_start` opens a block at an index;
/// `content_block_delta` carries either `text_delta` (for text blocks) or
/// `input_json_delta` (for tool_use blocks, with `partial_json` to
/// concatenate); `message_delta` carries final usage; `message_stop`
/// closes the stream.
struct AnthropicStreamState {
    requested_model: String,
    provider_model: Option<String>,
    blocks: Vec<StreamBlock>,
    usage: Usage,
    done: bool,
}

enum StreamBlock {
    Text(String),
    Tool {
        id: String,
        name: String,
        json: String,
    },
}

impl AnthropicStreamState {
    fn new(model: &str) -> Self {
        Self {
            requested_model: model.to_string(),
            provider_model: None,
            blocks: Vec::new(),
            usage: Usage::default(),
            done: false,
        }
    }

    fn apply(&mut self, payload: &Value, on_delta: DeltaCallback<'_>) {
        let event_type = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match event_type {
            "message_start" => {
                if let Some(message) = payload.get("message") {
                    self.provider_model = message
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    if let Some(usage) = message.get("usage") {
                        self.merge_usage(usage);
                    }
                }
            }
            "content_block_start" => {
                let index = payload.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let block = payload.get("content_block").cloned().unwrap_or(Value::Null);
                let kind = block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.ensure_block(index);
                match kind {
                    "text" => {
                        let initial = block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        self.blocks[index] = StreamBlock::Text(initial.clone());
                        if !initial.is_empty() {
                            on_delta(&initial);
                        }
                    }
                    "tool_use" => {
                        self.blocks[index] = StreamBlock::Tool {
                            id: block
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            name: block
                                .get("name")
                                .and_then(Value::as_str)
                                .map(map_tool_name_from_claude)
                                .unwrap_or_default(),
                            json: String::new(),
                        };
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let index = payload.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let delta = payload.get("delta").cloned().unwrap_or(Value::Null);
                let delta_type = delta
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.ensure_block(index);
                match delta_type {
                    "text_delta" => {
                        let text = delta
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if text.is_empty() {
                            return;
                        }
                        if let StreamBlock::Text(buffer) = &mut self.blocks[index] {
                            buffer.push_str(text);
                        } else {
                            self.blocks[index] = StreamBlock::Text(text.to_string());
                        }
                        on_delta(text);
                    }
                    "input_json_delta" => {
                        let partial = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if let StreamBlock::Tool { json, .. } = &mut self.blocks[index] {
                            json.push_str(partial);
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(usage) = payload.get("usage") {
                    self.merge_usage(usage);
                }
            }
            "message_stop" => self.done = true,
            "error" => {
                tracing::warn!(payload = %payload, "anthropic stream error frame");
            }
            _ => {}
        }
    }

    fn ensure_block(&mut self, index: usize) {
        while self.blocks.len() <= index {
            self.blocks.push(StreamBlock::Text(String::new()));
        }
    }

    fn merge_usage(&mut self, usage: &Value) {
        let input = usage
            .get("input_tokens")
            .and_then(Value::as_i64)
            .map(|value| value as i32);
        let output = usage
            .get("output_tokens")
            .and_then(Value::as_i64)
            .map(|value| value as i32);
        if let Some(value) = input {
            self.usage.prompt_tokens = Some(value);
        }
        if let Some(value) = output {
            self.usage.completion_tokens = Some(value);
        }
        if let (Some(input), Some(output)) =
            (self.usage.prompt_tokens, self.usage.completion_tokens)
        {
            self.usage.total_tokens = Some(input + output);
        }
        let cache_creation = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_i64)
            .map(|value| value as i32);
        let cache_read = usage
            .get("cache_read_input_tokens")
            .and_then(Value::as_i64)
            .map(|value| value as i32);
        if cache_creation.is_some() || cache_read.is_some() {
            self.usage.prompt_tokens_details = Some(PromptTokensDetails {
                cache_creation_tokens: cache_creation,
                cached_tokens: cache_read,
                audio_tokens: None,
            });
        }
        self.usage.compact_details();
    }

    fn into_response(self) -> ChatResponse {
        let mut parts = Vec::new();
        for block in self.blocks {
            match block {
                StreamBlock::Text(text) => {
                    if !text.is_empty() {
                        parts.push(ContentPart::Text(text));
                    }
                }
                StreamBlock::Tool { id, name, json } => {
                    let fn_arguments = if json.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&json).unwrap_or(json!({}))
                    };
                    parts.push(ContentPart::ToolCall(ToolCall {
                        call_id: id,
                        fn_name: name,
                        fn_arguments,
                        thought_signatures: None,
                    }));
                }
            }
        }
        let requested = normalize_anthropic_model(&self.requested_model);
        let provider = self.provider_model.unwrap_or_else(|| requested.clone());
        ChatResponse {
            content: MessageContent::from_parts(parts),
            reasoning_content: None,
            model_iden: ModelIden::new(AdapterKind::Anthropic, requested),
            provider_model_iden: ModelIden::new(AdapterKind::Anthropic, provider),
            usage: self.usage,
            captured_raw_body: None,
        }
    }
}

fn anthropic_response_to_chat_response(data: Value, requested_model: &str) -> Result<ChatResponse> {
    let mut parts = Vec::new();
    if let Some(blocks) = data.get("content").and_then(Value::as_array) {
        for block in blocks {
            match block.get("type").and_then(Value::as_str).unwrap_or("") {
                "text" => {
                    if let Some(text) = block.get("text").and_then(Value::as_str)
                        && !text.is_empty()
                    {
                        parts.push(ContentPart::Text(text.to_string()));
                    }
                }
                "tool_use" => {
                    let call_id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let fn_name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .map(map_tool_name_from_claude)
                        .unwrap_or_default();
                    let fn_arguments = block.get("input").cloned().unwrap_or_else(|| json!({}));
                    parts.push(ContentPart::ToolCall(ToolCall {
                        call_id,
                        fn_name,
                        fn_arguments,
                        thought_signatures: None,
                    }));
                }
                _ => {}
            }
        }
    }

    let provider_model = data
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| normalize_anthropic_model(requested_model));
    let mut usage = Usage::default();
    if let Some(raw_usage) = data.get("usage") {
        let input_tokens = raw_usage
            .get("input_tokens")
            .and_then(Value::as_i64)
            .map(|value| value as i32);
        let output_tokens = raw_usage
            .get("output_tokens")
            .and_then(Value::as_i64)
            .map(|value| value as i32);
        usage.prompt_tokens = input_tokens;
        usage.completion_tokens = output_tokens;
        usage.total_tokens = input_tokens
            .zip(output_tokens)
            .map(|(input, output)| input + output);
        let cache_creation = raw_usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_i64)
            .map(|value| value as i32);
        let cache_read = raw_usage
            .get("cache_read_input_tokens")
            .and_then(Value::as_i64)
            .map(|value| value as i32);
        if cache_creation.is_some() || cache_read.is_some() {
            usage.prompt_tokens_details = Some(PromptTokensDetails {
                cache_creation_tokens: cache_creation,
                cached_tokens: cache_read,
                audio_tokens: None,
            });
        }
        usage.compact_details();
    }

    let requested_model = normalize_anthropic_model(requested_model);
    Ok(ChatResponse {
        content: MessageContent::from_parts(parts),
        reasoning_content: None,
        model_iden: ModelIden::new(AdapterKind::Anthropic, requested_model),
        provider_model_iden: ModelIden::new(AdapterKind::Anthropic, provider_model),
        usage,
        captured_raw_body: None,
    })
}

fn anthropic_oauth_headers(access_token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/json".parse().unwrap());
    headers.insert("accept", "application/json".parse().unwrap());
    headers.insert(
        "authorization",
        format!("Bearer {access_token}").parse().unwrap(),
    );
    headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
    headers.insert(
        "user-agent",
        format!("claude-cli/{CLAUDE_CODE_VERSION} (external, cli)")
            .parse()
            .unwrap(),
    );
    headers.insert("x-app", "cli".parse().unwrap());
    headers.insert(
        "anthropic-dangerous-direct-browser-access",
        "true".parse().unwrap(),
    );
    headers.insert("x-stainless-arch", "x64".parse().unwrap());
    headers.insert("x-stainless-lang", "js".parse().unwrap());
    headers.insert("x-stainless-os", "Linux".parse().unwrap());
    headers.insert("x-stainless-package-version", "0.70.0".parse().unwrap());
    headers.insert("x-stainless-runtime", "node".parse().unwrap());
    headers.insert("x-stainless-runtime-version", "v24.3.0".parse().unwrap());
    headers.insert("x-stainless-retry-count", "0".parse().unwrap());
    headers.insert("x-stainless-timeout", "600".parse().unwrap());
    headers.insert(
        "anthropic-beta",
        "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14"
            .parse()
            .unwrap(),
    );
    headers
}

fn normalize_anthropic_model(model: &str) -> String {
    let model = strip_slash_provider(model.trim(), "anthropic");
    match model {
        "claude-opus-4-6" => "claude-opus-4-6",
        "claude-opus-4-5" => "claude-opus-4-5-20251101",
        "claude-sonnet-4-5" => "claude-sonnet-4-5-20250929",
        "claude-haiku-4-5" => "claude-haiku-4-5-20251001",
        other => other,
    }
    .to_string()
}

fn billing_header(first_user_text: &str) -> String {
    let chars = [4usize, 7, 20]
        .into_iter()
        .map(|index| first_user_text.chars().nth(index).unwrap_or('0'))
        .collect::<String>();
    let raw = format!("{CLAUDE_CODE_SALT}{chars}[object Object]");
    let hash = format!("{:x}", Sha256::digest(raw.as_bytes()));
    let entrypoint = env::var("CLAUDE_CODE_ENTRYPOINT").unwrap_or_else(|_| "unknown".to_string());
    format!(
        "x-anthropic-billing-header: cc_version={}.{}; cc_entrypoint={}; cch=00000;",
        CLAUDE_CODE_VERSION,
        &hash[..3],
        entrypoint
    )
}

fn map_tool_name_to_claude(name: &str) -> String {
    match name {
        "bash" => "Bash".to_string(),
        "read_file" => "Read".to_string(),
        "write_file" => "Write".to_string(),
        "edit_file" => "Edit".to_string(),
        "list_directory" => "Glob".to_string(),
        "grep_search" => "Grep".to_string(),
        "web_search" => "WebSearch".to_string(),
        "fetch_webpage" => "WebFetch".to_string(),
        "memory_read" => "mcp__lethe__MemoryRead".to_string(),
        "memory_update" => "mcp__lethe__MemoryUpdate".to_string(),
        "memory_append" => "mcp__lethe__MemoryAppend".to_string(),
        "archival_search" => "mcp__lethe__ArchivalSearch".to_string(),
        "archival_insert" => "mcp__lethe__ArchivalInsert".to_string(),
        "conversation_search" => "mcp__lethe__ConversationSearch".to_string(),
        other => format!("mcp__lethe__{}", to_pascal_case(other)),
    }
}

fn map_tool_name_from_claude(name: &str) -> String {
    match name {
        "Bash" => "bash".to_string(),
        "Read" => "read_file".to_string(),
        "Write" => "write_file".to_string(),
        "Edit" => "edit_file".to_string(),
        "Glob" => "list_directory".to_string(),
        "Grep" => "grep_search".to_string(),
        "WebSearch" => "web_search".to_string(),
        "WebFetch" => "fetch_webpage".to_string(),
        "mcp__lethe__MemoryRead" => "memory_read".to_string(),
        "mcp__lethe__MemoryUpdate" => "memory_update".to_string(),
        "mcp__lethe__MemoryAppend" => "memory_append".to_string(),
        "mcp__lethe__ArchivalSearch" => "archival_search".to_string(),
        "mcp__lethe__ArchivalInsert" => "archival_insert".to_string(),
        "mcp__lethe__ConversationSearch" => "conversation_search".to_string(),
        other if other.starts_with("mcp__") => other
            .split("__")
            .nth(2)
            .map(to_snake_case)
            .unwrap_or_else(|| to_snake_case(other)),
        other if other.starts_with("mcp_") => to_snake_case(&other[4..]),
        other => to_snake_case(other),
    }
}

fn to_pascal_case(name: &str) -> String {
    name.split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

fn to_snake_case(name: &str) -> String {
    let mut output = String::new();
    let mut previous_lower_or_digit = false;
    for ch in name.chars() {
        if ch == '-' || ch == ' ' {
            if !output.ends_with('_') {
                output.push('_');
            }
            previous_lower_or_digit = false;
            continue;
        }
        if ch.is_ascii_uppercase() {
            if previous_lower_or_digit && !output.ends_with('_') {
                output.push('_');
            }
            output.push(ch.to_ascii_lowercase());
            previous_lower_or_digit = false;
        } else {
            output.push(ch.to_ascii_lowercase());
            previous_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        }
    }
    output.trim_matches('_').to_string()
}

pub fn build_chat_request(messages: Vec<LlmMessage>) -> ChatRequest {
    ChatRequest::new(messages.into_iter().map(into_chat_message).collect())
}

fn into_chat_message(message: LlmMessage) -> ChatMessage {
    use genai::chat::MessageOptions;
    let LlmMessage {
        role,
        content,
        attachments,
        tool_calls,
        tool_responses,
        cache_control,
    } = message;

    // Tool-result-only user message → genai expects a content with ToolResponse
    // parts. Mirrors what the in-turn tool loop emits via ToolResponse::new.
    // Role must be `Tool`, not `User`: genai's OpenAI adapter only emits the
    // required `role:"tool"` message (with tool_call_id) for ChatRole::Tool. As
    // ChatRole::User the strict OpenAI API rejects the turn ("an assistant
    // message with 'tool_calls' must be followed by tool messages ..."). The
    // Anthropic path treats both roles identically (tool_result block under a
    // user message), so this is a no-op there.
    if role == LlmRole::User && !tool_responses.is_empty() {
        let mut parts = Vec::new();
        for response in tool_responses {
            parts.push(ContentPart::ToolResponse(ToolResponse::new(
                response.call_id,
                response.content,
            )));
        }
        let mut chat = ChatMessage {
            role: ChatRole::Tool,
            content: MessageContent::from_parts(parts),
            options: None,
        };
        if let Some(hint) = cache_control {
            chat.options = Some(MessageOptions::from(cache_hint_to_genai(hint)));
        }
        return chat;
    }

    // Assistant message that includes tool calls → text part(s) + ToolCall parts.
    if role == LlmRole::Assistant && !tool_calls.is_empty() {
        let mut parts = Vec::new();
        if !content.trim().is_empty() {
            parts.push(ContentPart::Text(content));
        }
        for call in tool_calls {
            parts.push(ContentPart::ToolCall(ToolCall {
                call_id: call.call_id,
                fn_name: call.fn_name,
                fn_arguments: call.fn_arguments,
                thought_signatures: call.thought_signatures,
            }));
        }
        let mut chat = ChatMessage::assistant(MessageContent::from_parts(parts));
        if let Some(hint) = cache_control {
            chat.options = Some(MessageOptions::from(cache_hint_to_genai(hint)));
        }
        return chat;
    }

    let content = into_message_content(content, attachments);
    let mut chat = match role {
        LlmRole::System => ChatMessage::system(content),
        LlmRole::User => ChatMessage::user(content),
        LlmRole::Assistant => ChatMessage::assistant(content),
    };
    if let Some(hint) = cache_control {
        chat.options = Some(MessageOptions::from(cache_hint_to_genai(hint)));
    }
    chat
}

fn cache_hint_to_genai(hint: CacheHint) -> genai::chat::CacheControl {
    match hint {
        CacheHint::Ephemeral => genai::chat::CacheControl::Ephemeral,
        CacheHint::Persistent => genai::chat::CacheControl::Persistent,
    }
}

fn into_message_content(content: String, attachments: Vec<LlmAttachment>) -> MessageContent {
    if attachments.is_empty() {
        return MessageContent::from(content);
    }
    let mut parts = Vec::new();
    if !content.trim().is_empty() {
        parts.push(ContentPart::from_text(content));
    }
    parts.extend(attachments.into_iter().map(|attachment| {
        ContentPart::from_binary_base64(
            attachment.content_type,
            attachment.base64_content,
            attachment.name,
        )
    }));
    MessageContent::from_parts(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_leading_non_user_messages_drops_orphaned_tool_results() {
        // A leading assistant(tool_use) whose results are the next (user-role)
        // message: dropping the assistant to satisfy "start with user" must also
        // drop the now-orphaned tool_result, else Anthropic 400s on messages.0.
        let mut messages = vec![
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "X", "name": "bash", "input": {}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "X", "content": "out"}]}),
            json!({"role": "user", "content": "real question"}),
        ];
        drop_leading_non_user_messages(&mut messages);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], json!("real question"));
    }

    #[test]
    fn drop_leading_non_user_messages_drops_consecutive_orphans_but_keeps_real_user() {
        // Two orphaned tool_result turns in a row, then a real user message.
        let mut messages = vec![
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "A", "name": "f", "input": {}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "A", "content": "a"}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "B", "content": "b"}]}),
            json!({"role": "user", "content": "hello"}),
        ];
        drop_leading_non_user_messages(&mut messages);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["content"], json!("hello"));
    }

    #[test]
    fn drop_leading_non_user_messages_keeps_a_clean_leading_user() {
        let mut messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "hey"}),
        ];
        drop_leading_non_user_messages(&mut messages);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["content"], json!("hi"));
    }

    #[test]
    fn aux_model_falls_back_to_main_model() {
        let config = LlmRouterConfig {
            model: "gpt-5".to_string(),
            aux_model: String::new(),
            tool_model: String::new(),
            provider: String::new(),
            api_base: String::new(),
            max_output_tokens: 100,
            temperature_millidegrees: 500,
        };

        assert_eq!(config.model_for(false), "gpt-5");
        assert_eq!(config.model_for(true), "gpt-5");
    }

    #[test]
    fn has_tool_model_only_when_set_and_distinct() {
        let mut config = LlmRouterConfig {
            model: "gemma".to_string(),
            aux_model: String::new(),
            tool_model: String::new(),
            provider: String::new(),
            api_base: String::new(),
            max_output_tokens: 100,
            temperature_millidegrees: 500,
        };
        // Unset -> no switching.
        assert!(!config.has_tool_model());
        assert_eq!(config.tool_model(), "");
        // Same as primary -> switching would be a no-op, so report none.
        config.tool_model = "gemma".to_string();
        assert!(!config.has_tool_model());
        // Distinct -> switch.
        config.tool_model = "deepseek".to_string();
        assert!(config.has_tool_model());
        assert_eq!(config.tool_model(), "deepseek");
    }

    #[test]
    fn retryable_status_codes() {
        for code in [429u16, 500, 502, 503, 504] {
            assert!(is_retryable_status(code), "{code} should be retryable");
        }
        for code in [400u16, 401, 403, 404, 422, 200] {
            assert!(!is_retryable_status(code), "{code} should not be retryable");
        }
    }

    #[test]
    fn genai_http_error_retry_classification() {
        let mk = |code: u16| genai::Error::HttpError {
            status: reqwest::StatusCode::from_u16(code).unwrap(),
            canonical_reason: String::new(),
            body: String::new(),
        };
        assert!(genai_error_is_retryable(&mk(429)));
        assert!(genai_error_is_retryable(&mk(503)));
        assert!(!genai_error_is_retryable(&mk(400)));
        assert!(!genai_error_is_retryable(&mk(404)));
    }

    #[test]
    fn genai_backoff_is_bounded_and_increasing() {
        assert_eq!(genai_retry_backoff(0), Duration::from_secs(1));
        assert_eq!(genai_retry_backoff(1), Duration::from_secs(2));
        assert!(genai_retry_backoff(10) <= Duration::from_secs(4));
    }

    #[test]
    fn request_builder_accepts_core_roles() {
        let request = build_chat_request(vec![
            LlmMessage::system("system"),
            LlmMessage::user("hello"),
            LlmMessage::assistant("hi"),
        ]);

        assert_eq!(request.messages.len(), 3);
    }

    #[test]
    fn request_builder_preserves_user_binary_attachments() {
        let request = build_chat_request(vec![LlmMessage::user_with_attachments(
            "caption",
            vec![LlmAttachment {
                content_type: "image/png".to_string(),
                base64_content: "aGVsbG8=".to_string(),
                name: Some("photo.png".to_string()),
            }],
        )]);

        assert_eq!(request.messages.len(), 1);
        let parts = request.messages[0].content.parts();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].as_text(), Some("caption"));
        assert!(parts[1].is_image());
    }

    #[test]
    fn tool_results_serialize_as_tool_role() {
        // Regression: a reconstructed historical tool result must map to genai
        // ChatRole::Tool so the OpenAI adapter emits a `role:"tool"` message.
        // With ChatRole::User the strict OpenAI API 400s the whole turn with
        // "the following tool_call_ids did not have response messages".
        let request = build_chat_request(vec![
            LlmMessage::assistant_with_tool_calls(
                "",
                vec![HistoricalToolCall {
                    call_id: "call_1".to_string(),
                    fn_name: "calculator".to_string(),
                    fn_arguments: serde_json::json!({"expression": "2+2"}),
                    thought_signatures: None,
                }],
            ),
            LlmMessage::tool_results(vec![HistoricalToolResponse {
                call_id: "call_1".to_string(),
                content: "4".to_string(),
                source_message_id: None,
            }]),
        ]);

        assert_eq!(request.messages.len(), 2);
        assert!(matches!(request.messages[0].role, ChatRole::Assistant));
        assert!(
            matches!(request.messages[1].role, ChatRole::Tool),
            "historical tool results must serialize as ChatRole::Tool, not User"
        );
    }

    #[test]
    fn tool_results_still_render_as_anthropic_tool_result_block() {
        // The Anthropic path must be unchanged by the Tool-role mapping: tool
        // results still become a `tool_result` block under a user message.
        let body = anthropic_request_body(
            "claude-opus-4-6",
            build_chat_request(vec![
                LlmMessage::user("what is 2+2?"),
                LlmMessage::assistant_with_tool_calls(
                    "",
                    vec![HistoricalToolCall {
                        call_id: "call_1".to_string(),
                        fn_name: "calculator".to_string(),
                        fn_arguments: serde_json::json!({"expression": "2+2"}),
                        thought_signatures: None,
                    }],
                ),
                LlmMessage::tool_results(vec![HistoricalToolResponse {
                    call_id: "call_1".to_string(),
                    content: "4".to_string(),
                    source_message_id: None,
                }]),
            ]),
            &ChatOptions::default(),
        );
        let messages = body["messages"].as_array().unwrap();
        let tool_result = messages
            .iter()
            .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
            .find(|block| block["type"] == "tool_result")
            .expect("a tool_result block must be present");
        assert_eq!(tool_result["tool_use_id"], "call_1");
        assert_eq!(tool_result["content"], "4");
    }

    #[test]
    fn anthropic_body_drops_leading_assistant_instead_of_inserting_continue() {
        let request = build_chat_request(vec![
            LlmMessage::assistant("orphaned prior assistant"),
            LlmMessage::user("real user message"),
        ]);

        let body = anthropic_request_body("claude-opus-4-6", request, &ChatOptions::default());
        let messages = body["messages"].as_array().unwrap();

        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["text"], "real user message");
        assert!(
            !serde_json::to_string(messages)
                .unwrap()
                .contains("[Continue]")
        );
    }

    #[test]
    fn router_target_maps_openrouter_prefix_to_openai_compatible_endpoint() {
        let config = LlmRouterConfig {
            model: "openrouter/moonshotai/kimi-k2.6".to_string(),
            aux_model: String::new(),
            tool_model: String::new(),
            provider: String::new(),
            api_base: String::new(),
            max_output_tokens: 100,
            temperature_millidegrees: 500,
        };

        let target = router_target_for_model(&config.model, &config).unwrap();

        assert_eq!(target.endpoint, OPENROUTER_ENDPOINT);
        assert_eq!(target.auth_env, "OPENROUTER_API_KEY");
        assert_eq!(target.adapter, AdapterKind::OpenAI);
        assert_eq!(target.model_name, "moonshotai/kimi-k2.6");
    }

    #[test]
    fn router_target_uses_configured_openrouter_provider_without_prefix() {
        let config = LlmRouterConfig {
            model: "moonshotai/kimi-k2.6".to_string(),
            aux_model: String::new(),
            tool_model: String::new(),
            provider: "openrouter".to_string(),
            api_base: String::new(),
            max_output_tokens: 100,
            temperature_millidegrees: 500,
        };

        let target = router_target_for_model(&config.model, &config).unwrap();

        assert_eq!(target.endpoint, OPENROUTER_ENDPOINT);
        assert_eq!(target.auth_env, "OPENROUTER_API_KEY");
        assert_eq!(target.model_name, "moonshotai/kimi-k2.6");
    }

    #[test]
    fn router_target_normalizes_custom_openai_base_and_strips_prefix() {
        let config = LlmRouterConfig {
            model: "openai/gemma-4-31B-it-Q8_0.gguf".to_string(),
            aux_model: String::new(),
            tool_model: String::new(),
            provider: "openai".to_string(),
            api_base: "http://localhost:8090/v1".to_string(),
            max_output_tokens: 100,
            temperature_millidegrees: 500,
        };

        let target = router_target_for_model(&config.model, &config).unwrap();

        assert_eq!(target.endpoint, "http://localhost:8090/v1/");
        assert_eq!(target.auth_env, "OPENAI_API_KEY");
        assert_eq!(target.adapter, AdapterKind::OpenAI);
        assert_eq!(target.model_name, "gemma-4-31B-it-Q8_0.gguf");
    }

    #[test]
    fn router_target_strips_anthropic_slash_prefix() {
        let config = LlmRouterConfig {
            model: "anthropic/claude-sonnet-4-6".to_string(),
            aux_model: String::new(),
            tool_model: String::new(),
            provider: String::new(),
            api_base: String::new(),
            max_output_tokens: 100,
            temperature_millidegrees: 500,
        };

        let target = router_target_for_model(&config.model, &config).unwrap();

        assert_eq!(target.endpoint, ANTHROPIC_ENDPOINT);
        assert_eq!(target.auth_env, "ANTHROPIC_API_KEY");
        assert_eq!(target.adapter, AdapterKind::Anthropic);
        assert_eq!(target.model_name, "claude-sonnet-4-6");
    }

    #[test]
    fn openai_oauth_routes_when_provider_is_openai() {
        let mut config = LlmRouterConfig {
            model: "gpt-5.2".to_string(),
            aux_model: String::new(),
            tool_model: String::new(),
            provider: "openai".to_string(),
            api_base: String::new(),
            max_output_tokens: 100,
            temperature_millidegrees: 500,
        };
        assert!(should_use_openai_oauth(&config.model, &config));

        config.model = "openai/gpt-5.2".to_string();
        config.provider = String::new();
        assert!(should_use_openai_oauth(&config.model, &config));
    }

    #[test]
    fn openai_oauth_skips_openrouter_and_custom_api_base() {
        // openrouter slash-prefix wins regardless of provider
        let config = LlmRouterConfig {
            model: "openrouter/openai/gpt-5.2".to_string(),
            aux_model: String::new(),
            tool_model: String::new(),
            provider: "openai".to_string(),
            api_base: String::new(),
            max_output_tokens: 100,
            temperature_millidegrees: 500,
        };
        assert!(!should_use_openai_oauth(&config.model, &config));

        // custom api_base means the user wired their own gateway
        let config = LlmRouterConfig {
            model: "gpt-5.2".to_string(),
            aux_model: String::new(),
            tool_model: String::new(),
            provider: "openai".to_string(),
            api_base: "http://localhost:8080/v1/".to_string(),
            max_output_tokens: 100,
            temperature_millidegrees: 500,
        };
        assert!(!should_use_openai_oauth(&config.model, &config));
    }

    #[test]
    fn openai_oauth_skipped_when_provider_is_unrelated() {
        let config = LlmRouterConfig {
            model: "claude-opus-4-6".to_string(),
            aux_model: String::new(),
            tool_model: String::new(),
            provider: "anthropic".to_string(),
            api_base: String::new(),
            max_output_tokens: 100,
            temperature_millidegrees: 500,
        };
        assert!(!should_use_openai_oauth(&config.model, &config));
    }
}
