use std::collections::HashMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use async_stream::stream;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast, mpsc};
use uuid::Uuid;

use crate::actor::ActorEvent;
use crate::agent::{Agent, TurnRequest};
use crate::config::Settings;
use crate::conversation::{ConversationManager, ProcessCallback, ProcessContext};
use crate::llm::models::{available_providers, normalize_model_id, provider_for_model};
use crate::memory::StoredMessage;
use crate::scheduler::brainstem::{BrainstemEmission, BrainstemHandle};
use crate::todos::{TodoFilter, TodoPriority, TodoStatus};
use crate::tools::registry::{
    BoxToolFuture, ClientToolContext, SharedTurnObserver, ToolRuntime, TurnObserver,
};

const SESSION_QUEUE_DEPTH: usize = 32;
const PROACTIVE_QUEUE_DEPTH: usize = 64;

#[derive(Clone)]
pub struct ApiState {
    settings: Settings,
    agent: Arc<Agent>,
    conversations: ConversationManager,
    sessions: Arc<Mutex<ApiSessions>>,
    proactive_tx: broadcast::Sender<ApiEvent>,
    /// Server-wide stream that fans out actor.* lifecycle events to any
    /// /events subscriber (TUI clients). Populated when an actor runtime
    /// is installed (see `install_actor_broadcaster`).
    stream_tx: broadcast::Sender<ApiEvent>,
}

#[derive(Debug, Default)]
struct ApiSessions {
    by_id: HashMap<String, ApiSession>,
    by_chat: HashMap<i64, String>,
}

#[derive(Debug)]
struct ApiSession {
    chat_id: i64,
    sender: mpsc::Sender<ApiEvent>,
}

struct ApiStreamGuard {
    state: ApiState,
    chat_id: i64,
    session_id: String,
    finished: bool,
}

impl ApiStreamGuard {
    fn new(state: ApiState, chat_id: i64, session_id: String) -> Self {
        Self {
            state,
            chat_id,
            session_id,
            finished: false,
        }
    }

    async fn finish(&mut self) {
        self.finished = true;
        self.state.unregister_session(&self.session_id).await;
    }
}

impl Drop for ApiStreamGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }

        let state = self.state.clone();
        let chat_id = self.chat_id;
        let session_id = self.session_id.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if state.session_matches_chat(chat_id, &session_id).await {
                    state.conversations.cancel(chat_id).await;
                }
                state.unregister_session(&session_id).await;
            });
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApiEvent {
    pub event: String,
    pub data: Value,
}

impl ApiEvent {
    pub fn new(event: impl Into<String>, data: Value) -> Self {
        Self {
            event: event.into(),
            data,
        }
    }

    fn into_sse(self) -> Event {
        Event::default()
            .event(self.event)
            .data(self.data.to_string())
    }
}

impl ApiState {
    pub fn new(settings: Settings, agent: Agent) -> Self {
        Self::with_shared_agent(settings, Arc::new(agent))
    }

    /// Construct around a pre-built `Arc<Agent>` so multiple transports
    /// (HTTP API + Telegram poller) can share one agent, one memory
    /// store, and one actor registry in the same process.
    pub fn with_shared_agent(settings: Settings, agent: Arc<Agent>) -> Self {
        let (proactive_tx, _) = broadcast::channel(PROACTIVE_QUEUE_DEPTH);
        let (stream_tx, _) = broadcast::channel(PROACTIVE_QUEUE_DEPTH);
        Self {
            conversations: ConversationManager::new(Duration::from_secs_f64(
                settings.background.debounce_seconds,
            )),
            settings,
            agent,
            sessions: Arc::new(Mutex::new(ApiSessions::default())),
            proactive_tx,
            stream_tx,
        }
    }

    /// Subscribe the API to the agent's actor event bus and translate each
    /// internal `ActorEvent` into a public `actor.*` SSE event on the
    /// stream broadcast. Called once at server start.
    pub async fn install_actor_broadcaster(&self) -> Result<()> {
        let Some(runtime) = self.agent.actor_registry() else {
            return Ok(());
        };
        let mut rx = runtime
            .install_event_broadcaster(256)
            .await
            .map_err(|error| anyhow::anyhow!("install actor broadcaster: {error}"))?;
        let stream_tx = self.stream_tx.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if let Some(api_event) = actor_event_to_api(&event) {
                            let _ = stream_tx.send(api_event);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Ok(())
    }

    pub fn from_settings(settings: Settings) -> Result<Self> {
        let agent = Agent::from_settings(settings.clone())?;
        Ok(Self::new(settings, agent))
    }

    pub async fn send_proactive(&self, content: &str) -> bool {
        let content = content.trim();
        if content.is_empty() {
            return false;
        }
        self.proactive_tx
            .send(ApiEvent::new(
                "text",
                json!({
                    "content": content,
                    "parse_mode": "Markdown",
                    "message_id": 0,
                    "proactive": true,
                }),
            ))
            .is_ok()
    }

    async fn register_session(&self, chat_id: i64, sender: mpsc::Sender<ApiEvent>) -> String {
        let session_id = Uuid::new_v4().simple().to_string();
        let previous = {
            let mut sessions = self.sessions.lock().await;
            let previous_id = sessions.by_chat.insert(chat_id, session_id.clone());
            let previous = previous_id.and_then(|id| sessions.by_id.remove(&id));
            sessions
                .by_id
                .insert(session_id.clone(), ApiSession { chat_id, sender });
            previous
        };

        if let Some(previous) = previous {
            close_sender(previous.sender).await;
        }
        session_id
    }

    async fn unregister_session(&self, session_id: &str) {
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.by_id.remove(session_id)
            && sessions.by_chat.get(&session.chat_id) == Some(&session_id.to_string())
        {
            sessions.by_chat.remove(&session.chat_id);
        }
    }

    async fn close_chat_session(&self, chat_id: i64) -> bool {
        let session = {
            let mut sessions = self.sessions.lock().await;
            let Some(session_id) = sessions.by_chat.remove(&chat_id) else {
                return false;
            };
            sessions.by_id.remove(&session_id)
        };
        if let Some(session) = session {
            close_sender(session.sender).await;
            true
        } else {
            false
        }
    }

    async fn session_matches_chat(&self, chat_id: i64, session_id: &str) -> bool {
        let sessions = self.sessions.lock().await;
        sessions
            .by_chat
            .get(&chat_id)
            .is_some_and(|id| id == session_id)
            && sessions.by_id.contains_key(session_id)
    }

    async fn send_to_session(&self, session_id: &str, event: &str, data: Value) -> bool {
        let sender = {
            let sessions = self.sessions.lock().await;
            sessions
                .by_id
                .get(session_id)
                .map(|session| session.sender.clone())
        };
        let Some(sender) = sender else {
            return false;
        };
        sender.send(ApiEvent::new(event, data)).await.is_ok()
    }

    async fn client_tool_context(
        &self,
        session_id: &str,
        chat_id: i64,
        last_message_id: Option<i64>,
    ) -> Option<ClientToolContext> {
        let sender = {
            let sessions = self.sessions.lock().await;
            sessions
                .by_id
                .get(session_id)
                .map(|session| session.sender.clone())
        }?;
        Some(ClientToolContext::new(
            chat_id,
            last_message_id,
            move |event| {
                sender
                    .try_send(ApiEvent::new(event.event, event.data))
                    .is_ok()
            },
        ))
    }
}

async fn close_sender(sender: mpsc::Sender<ApiEvent>) {
    let _ = sender.send(ApiEvent::new("typing_stop", json!({}))).await;
    let _ = sender.send(ApiEvent::new("done", json!({}))).await;
}

/// Bridges the agent's tool-loop hooks into per-session SSE events. The
/// session sender is the same `mpsc::Sender` used for `text`/`typing_*`,
/// so tool cards appear inline with the assistant transcript on the TUI.
struct ApiTurnObserver {
    sender: mpsc::Sender<ApiEvent>,
}

impl ApiTurnObserver {
    fn new(sender: mpsc::Sender<ApiEvent>) -> Self {
        Self { sender }
    }
}

impl TurnObserver for ApiTurnObserver {
    fn wrap_tool_call<'a>(&'a self, _name: &'a str, inner: BoxToolFuture<'a>) -> BoxToolFuture<'a> {
        inner
    }

    fn on_tool_start(&self, name: &str, call_id: &str, args_preview: &str) {
        let _ = self.sender.try_send(ApiEvent::new(
            "tool.start",
            json!({
                "name": name,
                "call_id": call_id,
                "args_preview": args_preview,
            }),
        ));
    }

    fn on_tool_end(
        &self,
        name: &str,
        call_id: &str,
        success: bool,
        output_preview: &str,
        duration_ms: u128,
    ) {
        let _ = self.sender.try_send(ApiEvent::new(
            "tool.end",
            json!({
                "name": name,
                "call_id": call_id,
                "success": success,
                "output_preview": output_preview,
                "duration_ms": duration_ms as u64,
            }),
        ));
    }

    fn on_assistant_delta(&self, content: &str) {
        if content.is_empty() {
            return;
        }
        let _ = self.sender.try_send(ApiEvent::new(
            "assistant.delta",
            json!({"content": content}),
        ));
    }

    fn on_reasoning_delta(&self, content: &str) {
        if content.is_empty() {
            return;
        }
        let _ = self.sender.try_send(ApiEvent::new(
            "assistant.reasoning",
            json!({"content": content}),
        ));
    }
}

/// Translate an internal `ActorEvent` into the TUI-facing actor.* surface.
/// Returns `None` for events that the TUI doesn't render so the SSE stream
/// stays low-traffic.
fn actor_event_to_api(event: &ActorEvent) -> Option<ApiEvent> {
    let payload = serde_json::Value::Object(event.payload.clone());
    match event.event_type.as_str() {
        "actor_spawned" => Some(ApiEvent::new(
            "actor.spawned",
            json!({
                "actor_id": event.actor_id,
                "group": event.group,
                "payload": payload,
            }),
        )),
        "actor_terminated" | "actor_cycle_finished" => Some(ApiEvent::new(
            "actor.state",
            json!({
                "actor_id": event.actor_id,
                "kind": event.event_type,
                "payload": payload,
            }),
        )),
        "task_state_changed" => Some(ApiEvent::new(
            "actor.task",
            json!({
                "actor_id": event.actor_id,
                "payload": payload,
            }),
        )),
        "actor_message" => Some(ApiEvent::new(
            "actor.message",
            json!({
                "actor_id": event.actor_id,
                "payload": payload,
            }),
        )),
        _ => None,
    }
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/chat", post(chat))
        .route("/cancel", post(cancel))
        .route("/configure", post(configure))
        .route("/model", get(model_get).post(model_post))
        .route("/events", get(events))
        .route("/mini/{app_name}", get(static_mini_app))
        .route("/mini-apps/{slug}", get(mini_app_wrapper))
        .route("/telegram/mini-app/validate", post(validate_mini_app))
        .route("/file", get(serve_file))
        .route("/actors", get(list_actors))
        .route("/todos", get(list_todos))
        .route("/session/history", get(session_history))
        .with_state(state)
}

pub async fn serve(settings: Settings, port: u16) -> Result<()> {
    // Standalone API mode: spin up our own Brainstem since there's no
    // shared one. The combined api+telegram path in `cli::handlers`
    // passes its own handle so both transports share one Brainstem.
    let brainstem = BrainstemHandle::new();
    serve_with_agent(settings, port, None, brainstem).await
}

/// Run the API server with optional shared agent + shared Brainstem.
/// When `agent` is `None`, builds one from settings. The Brainstem's
/// emissions are bridged into the existing `/events` broadcast so
/// connected TUI clients see heartbeat-driven proactive messages with
/// no special-case logic.
pub async fn serve_with_agent(
    settings: Settings,
    port: u16,
    agent: Option<Arc<Agent>>,
    brainstem: BrainstemHandle,
) -> Result<()> {
    if settings.api.token.trim().is_empty() {
        bail!("LETHE_API_TOKEN must be set in API mode");
    }

    let state = match agent {
        Some(agent) => ApiState::with_shared_agent(settings.clone(), agent),
        None => ApiState::from_settings(settings.clone())?,
    };
    if let Err(error) = state.install_actor_broadcaster().await {
        tracing::warn!(error = %error, "actor broadcaster not installed");
    }
    let app = router(state.clone());
    let bind = format!("{}:{port}", settings.api.host);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!("Lethe Rust API listening on http://{bind}");

    let brainstem_bridge = {
        let state = state.clone();
        let mut rx = brainstem.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(BrainstemEmission { message, .. }) => {
                        state.send_proactive(&message).await;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    };

    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    brainstem_bridge.abort();
    let _ = brainstem_bridge.await;
    Ok(result?)
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ready"}))
}

async fn static_mini_app(
    State(state): State<ApiState>,
    AxumPath(app_name): AxumPath<String>,
) -> Response {
    let slug = crate::mini_app::static_mini_app_slug(&app_name);
    let path = crate::mini_app::static_mini_app_index_path(&state.settings.paths.workspace_dir, &slug);
    let html = match std::fs::read_to_string(&path) {
        Ok(html) => html,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return json_error(StatusCode::NOT_FOUND, &format!("mini app not found: {slug}"));
        }
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let mut response = html_response(html);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store, max-age=0"));
    response
}

#[derive(Debug, Deserialize)]
struct MiniAppOpenQuery {
    token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MiniAppValidateRequest {
    slug: String,
    token: String,
    #[serde(rename = "initData")]
    init_data: String,
}

async fn mini_app_wrapper(
    State(state): State<ApiState>,
    AxumPath(slug): AxumPath<String>,
    Query(query): Query<MiniAppOpenQuery>,
) -> Response {
    let Some(token) = query
        .token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return json_error(StatusCode::UNAUTHORIZED, "mini app token is required");
    };
    let store = crate::mini_app::MiniAppStore::from_settings(&state.settings);
    match store.verify_token(&slug, token) {
        Ok(true) => html_response(mini_app_wrapper_html(&slug, token)),
        Ok(false) => json_error(StatusCode::UNAUTHORIZED, "invalid mini app token"),
        Err(error) => json_error(StatusCode::NOT_FOUND, &error.to_string()),
    }
}

async fn validate_mini_app(
    State(state): State<ApiState>,
    Json(body): Json<MiniAppValidateRequest>,
) -> Response {
    let store = crate::mini_app::MiniAppStore::from_settings(&state.settings);
    let metadata = match store.load_metadata(&body.slug) {
        Ok(metadata) => metadata,
        Err(error) => return json_error(StatusCode::NOT_FOUND, &error.to_string()),
    };
    match store.verify_token(&body.slug, &body.token) {
        Ok(true) => {}
        Ok(false) => return json_error(StatusCode::UNAUTHORIZED, "invalid mini app token"),
        Err(error) => return json_error(StatusCode::NOT_FOUND, &error.to_string()),
    }
    if state.settings.telegram.bot_token.trim().is_empty() {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "TELEGRAM_BOT_TOKEN is required for Mini App validation",
        );
    }
    if let Err(error) = crate::mini_app::validate_telegram_init_data(
        &body.init_data,
        &state.settings.telegram.bot_token,
        24 * 60 * 60,
    ) {
        return json_error(StatusCode::UNAUTHORIZED, &error.to_string());
    }
    if let Some(expected_user_id) = metadata.telegram_user_id
        && let Some(actual_user_id) = crate::mini_app::telegram_init_data_user_id(&body.init_data)
        && actual_user_id != expected_user_id
    {
        return json_error(
            StatusCode::FORBIDDEN,
            "Mini App user does not match artifact owner",
        );
    }
    let html = match store.read_artifact_html(&body.slug) {
        Ok(html) => html,
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    Json(json!({"html": html, "title": metadata.title, "summary": metadata.summary}))
        .into_response()
}

fn mini_app_wrapper_html(slug: &str, token: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Lethe Mini App</title>
<script src="https://telegram.org/js/telegram-web-app.js"></script>
<style>body{{margin:0;font-family:system-ui,sans-serif;background:#111827;color:#f9fafb}}#status{{padding:1rem}}iframe{{border:0;width:100vw;height:100vh;background:white}}</style>
</head>
<body>
<div id="status">Opening Mini App…</div>
<script>
const slug = {slug_json};
const token = {token_json};
const tg = window.Telegram && window.Telegram.WebApp;
if (tg) tg.ready();
async function main() {{
  const initData = tg ? tg.initData : "";
  const response = await fetch('/telegram/mini-app/validate', {{
    method: 'POST',
    headers: {{'content-type': 'application/json'}},
    body: JSON.stringify({{slug, token, initData}})
  }});
  if (!response.ok) throw new Error(await response.text());
  const data = await response.json();
  const iframe = document.createElement('iframe');
  iframe.setAttribute('sandbox', 'allow-scripts allow-forms allow-popups allow-modals');
  iframe.srcdoc = data.html;
  document.body.replaceChildren(iframe);
}}
main().catch(error => {{
  document.getElementById('status').textContent = 'Mini App validation failed: ' + error.message;
}});
</script>
</body>
</html>"#,
        slug_json = serde_json::to_string(slug).unwrap(),
        token_json = serde_json::to_string(token).unwrap()
    )
}

fn html_response(html: String) -> Response {
    let mut response = html.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    response
}

#[derive(Debug, Deserialize)]
struct ChatRequest {
    message: String,
    #[serde(default)]
    user_id: i64,
    #[serde(default)]
    chat_id: Option<i64>,
    #[serde(default)]
    metadata: serde_json::Map<String, Value>,
}

async fn chat(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(mut body): Json<ChatRequest>,
) -> Response {
    if let Some(response) = require_auth(&state, &headers) {
        return response;
    }
    if body.message.trim().is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "message is required");
    }

    let chat_id = body.chat_id.unwrap_or(body.user_id);
    let (sender, mut receiver) = mpsc::channel::<ApiEvent>(SESSION_QUEUE_DEPTH);
    let session_id = state.register_session(chat_id, sender).await;
    body.metadata
        .insert("_api_session_id".to_string(), json!(session_id.clone()));

    let callback = process_chat_callback(state.clone());
    state
        .conversations
        .add_message(
            chat_id,
            body.user_id,
            body.message,
            Some(body.metadata),
            Some(callback),
        )
        .await;

    let stream_state = state.clone();
    let stream_session_id = session_id.clone();
    let stream_chat_id = chat_id;
    let event_stream = stream! {
        let mut guard = ApiStreamGuard::new(stream_state, stream_chat_id, stream_session_id);
        while let Some(event) = receiver.recv().await {
            let done = event.event == "done";
            yield Ok::<Event, Infallible>(event.into_sse());
            if done {
                break;
            }
        }
        guard.finish().await;
    };

    Sse::new(event_stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn process_chat_callback(state: ApiState) -> ProcessCallback {
    Arc::new(move |context: ProcessContext| {
        let state = state.clone();
        Box::pin(async move {
            process_chat_context(state, context).await;
            Ok(())
        })
    })
}

async fn process_chat_context(state: ApiState, context: ProcessContext) {
    let session_id = context
        .metadata
        .get("_api_session_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if session_id.is_empty() {
        return;
    }

    let _ = state
        .send_to_session(&session_id, "typing_start", json!({}))
        .await;
    let _ = state
        .send_to_session(
            &session_id,
            "turn.start",
            json!({"chat_id": context.chat_id}),
        )
        .await;
    let observer: Option<SharedTurnObserver> = {
        let sessions = state.sessions.lock().await;
        sessions.by_id.get(&session_id).map(|session| {
            Arc::new(ApiTurnObserver::new(session.sender.clone())) as SharedTurnObserver
        })
    };
    let tool_runtime = ToolRuntime {
        client: state
            .client_tool_context(
                &session_id,
                context.chat_id,
                metadata_i64(&context.metadata, "message_id"),
            )
            .await,
        observer,
        ..ToolRuntime::default()
    };
    let response = state
        .agent
        .chat_once(TurnRequest::new(&context.message).with_runtime(tool_runtime))
        .await;

    match response {
        // Deliver the reply even when the user typed mid-turn (interrupt token
        // set): the work is done and already persisted to history — dropping it
        // here just desyncs the live view from reality (observed: a 7-minute
        // research turn whose answer only ever existed after a page reload).
        Ok(message) if !message.trim().is_empty() => {
            let _ = state
                .send_to_session(
                    &session_id,
                    "text",
                    json!({
                        "content": message,
                        "parse_mode": "Markdown",
                        "message_id": 0,
                    }),
                )
                .await;
        }
        Ok(_) => {}
        Err(error) if !context.interrupt.is_interrupted() => {
            let _ = state
                .send_to_session(
                    &session_id,
                    "text",
                    json!({
                        "content": format!("Error: {error}"),
                        "parse_mode": null,
                        "message_id": 0,
                    }),
                )
                .await;
        }
        Err(_) => {}
    }

    if let Some(tokens) = state.agent.last_prompt_tokens() {
        let _ = state
            .send_to_session(&session_id, "usage", json!({"prompt_tokens": tokens}))
            .await;
    }
    let _ = state
        .send_to_session(&session_id, "typing_stop", json!({}))
        .await;
    let _ = state.send_to_session(&session_id, "done", json!({})).await;
    state.unregister_session(&session_id).await;
}

#[derive(Debug, Deserialize)]
struct ChatIdRequest {
    #[serde(default)]
    chat_id: i64,
}

async fn cancel(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(body): Json<ChatIdRequest>,
) -> Response {
    if let Some(response) = require_auth(&state, &headers) {
        return response;
    }
    let cancelled = if body.chat_id == 0 {
        false
    } else {
        let conversation = state.conversations.cancel(body.chat_id).await;
        let session = state.close_chat_session(body.chat_id).await;
        conversation || session
    };
    Json(json!({"status": "cancelled", "cancelled": cancelled})).into_response()
}

#[derive(Debug, Deserialize)]
struct ConfigureRequest {
    #[serde(default)]
    user_id: i64,
    #[serde(default)]
    username: String,
    #[serde(default)]
    first_name: String,
}

async fn configure(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(body): Json<ConfigureRequest>,
) -> Response {
    if let Some(response) = require_auth(&state, &headers) {
        return response;
    }

    let mut human = format!("Name: {}\n", body.first_name.trim());
    if !body.username.trim().is_empty() {
        human.push_str(&format!("Telegram: @{}\n", body.username.trim()));
    }
    human.push_str(&format!("User ID: {}\n", body.user_id));

    match state
        .agent
        .memory()
        .blocks
        .update("human", Some(&human), None)
    {
        Ok(true) => Json(json!({"status": "configured"})).into_response(),
        Ok(false) => match state.agent.memory().blocks.create(
            "human",
            &human,
            "Information about the human user.",
            crate::memory::DEFAULT_BLOCK_LIMIT,
            false,
            false,
        ) {
            Ok(_) => Json(json!({"status": "configured"})).into_response(),
            Err(error) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
        },
        Err(error) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn model_get(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_auth(&state, &headers) {
        return response;
    }
    let config = match state.agent.router_config() {
        Ok(config) => config,
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    Json(json!({
        "model": config.model,
        "model_aux": config.aux_model,
        "provider": model_provider(&config.model, &state.settings.llm.llm_provider),
        "current_auth": "API",
        "available_providers": available_provider_ids(),
        "provider_info": available_providers(),
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct ModelUpdateRequest {
    model: Option<String>,
    model_aux: Option<String>,
}

async fn model_post(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(body): Json<ModelUpdateRequest>,
) -> Response {
    if let Some(response) = require_auth(&state, &headers) {
        return response;
    }
    // Normalize bare OpenRouter ids (`vendor/model` -> `openrouter/vendor/model`)
    // against the configured provider, matching the persisted `lethe model` path.
    let provider = state.settings.llm.llm_provider.trim();
    let model = body
        .model
        .as_deref()
        .map(|id| normalize_model_id(provider, id));
    let model_aux = body
        .model_aux
        .as_deref()
        .map(|id| normalize_model_id(provider, id));
    let changed = match state
        .agent
        .reconfigure_models(model.as_deref(), model_aux.as_deref())
    {
        Ok(changed) => changed,
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let config = match state.agent.router_config() {
        Ok(config) => config,
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    Json(json!({
        "status": "updated",
        "model": config.model,
        "model_aux": config.aux_model,
        "provider": model_provider(&config.model, &state.settings.llm.llm_provider),
        "changed": changed,
    }))
    .into_response()
}

async fn events(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_auth(&state, &headers) {
        return response;
    }
    let mut proactive_rx = state.proactive_tx.subscribe();
    let mut stream_rx = state.stream_tx.subscribe();
    // Conversation messages from other transports (e.g. Telegram) so an open web
    // client can append them to its transcript live.
    let mut conversation_rx = state.agent.subscribe_conversation_events();
    let event_stream = stream! {
        loop {
            tokio::select! {
                event = proactive_rx.recv() => match event {
                    Ok(event) => yield Ok::<Event, Infallible>(event.into_sse()),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                event = stream_rx.recv() => match event {
                    Ok(event) => yield Ok::<Event, Infallible>(event.into_sse()),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                event = conversation_rx.recv() => match event {
                    Ok(event) => yield Ok::<Event, Infallible>(ApiEvent::new(event.event, event.data).into_sse()),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    };
    Sse::new(event_stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn list_actors(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_auth(&state, &headers) {
        return response;
    }
    let Some(runtime) = state.agent.actor_registry() else {
        return Json(json!({"actors": []})).into_response();
    };
    match runtime.list_actors().await {
        Ok(actors) => Json(json!({"actors": actors})).into_response(),
        Err(error) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

#[derive(Debug, Deserialize)]
struct TodoListQuery {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    include_completed: bool,
    #[serde(default)]
    limit: Option<usize>,
}

async fn list_todos(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(query): Query<TodoListQuery>,
) -> Response {
    if let Some(response) = require_auth(&state, &headers) {
        return response;
    }
    let filter = TodoFilter {
        status: query.status.as_deref().and_then(TodoStatus::parse),
        priority: query.priority.as_deref().and_then(TodoPriority::parse),
        include_completed: query.include_completed,
        limit: query.limit.unwrap_or(50),
    };
    match state.agent.memory().todos.list(filter) {
        Ok(todos) => Json(json!({"todos": todos})).into_response(),
        Err(error) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

#[derive(Debug, Deserialize)]
struct HistoryQuery {
    #[serde(default)]
    limit: Option<usize>,
}

async fn session_history(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(query): Query<HistoryQuery>,
) -> Response {
    if let Some(response) = require_auth(&state, &headers) {
        return response;
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let messages = match state.agent.memory().messages.get_recent(limit) {
        Ok(messages) => messages,
        Err(error) => {
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string());
        }
    };
    let serialized = messages
        .into_iter()
        .map(serialize_message)
        .collect::<Vec<_>>();
    Json(json!({"messages": serialized})).into_response()
}

fn serialize_message(message: StoredMessage) -> Value {
    json!({
        "id": message.id,
        "role": message.role.as_str(),
        "content": message.content,
        "created_at": message.created_at,
        "metadata": message.metadata,
    })
}

#[derive(Debug, Deserialize)]
struct FileQuery {
    path: String,
}

async fn serve_file(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(query): Query<FileQuery>,
) -> Response {
    if let Some(response) = require_auth(&state, &headers) {
        return response;
    }
    let Some(path) = resolve_workspace_path(&state.settings.paths.workspace_dir, &query.path)
    else {
        return json_error(StatusCode::FORBIDDEN, "path outside workspace");
    };
    if !path.is_file() {
        return json_error(StatusCode::NOT_FOUND, &format!("not found: {}", query.path));
    }
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut response = bytes.into_response();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
            response
        }
        Err(error) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

fn require_auth(state: &ApiState, headers: &HeaderMap) -> Option<Response> {
    let expected = state.settings.api.token.trim();
    let presented = presented_api_token(headers);
    if expected.is_empty() {
        return Some(json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "server misconfigured",
        ));
    }
    if presented != expected {
        return Some(json_error(StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    None
}

fn presented_api_token(headers: &HeaderMap) -> String {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .trim();
    if bearer.to_ascii_lowercase().starts_with("bearer ") {
        return bearer[7..].trim().to_string();
    }
    headers
        .get("x-lethe-token")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": message}))).into_response()
}

fn metadata_i64(metadata: &serde_json::Map<String, Value>, key: &str) -> Option<i64> {
    metadata.get(key).and_then(Value::as_i64)
}

fn available_provider_ids() -> Vec<String> {
    available_providers()
        .into_iter()
        .map(|provider| provider.provider)
        .collect()
}

fn model_provider<'a>(model: &'a str, configured_provider: &'a str) -> &'a str {
    provider_for_model(model)
        .or_else(|| (!configured_provider.trim().is_empty()).then_some(configured_provider))
        .unwrap_or("")
}

fn resolve_workspace_path(workspace_root: &Path, raw_path: &str) -> Option<PathBuf> {
    if raw_path.trim().is_empty() {
        return None;
    }
    let root = workspace_root.canonicalize().ok()?;
    let requested = Path::new(raw_path);
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    let resolved = candidate.canonicalize().ok()?;
    resolved.starts_with(&root).then_some(resolved)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::body::to_bytes;
    use axum::http::HeaderValue;
    use tempfile::tempdir;
    use tokio::sync::Notify;
    use tokio::time::{sleep, timeout};

    use super::*;

    fn test_settings(root: &std::path::Path) -> Settings {
        let mut settings = crate::config::test_settings(root);
        settings.api.token = "secret".to_string();
        settings.llm.llm_model = "openai/gpt-5".to_string();
        settings.llm.llm_model_aux = "openai/gpt-5-mini".to_string();
        settings
    }

    #[test]
    fn presented_token_prefers_bearer_then_custom_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-lethe-token", HeaderValue::from_static("fallback"));
        assert_eq!(presented_api_token(&headers), "fallback");

        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret"),
        );
        assert_eq!(presented_api_token(&headers), "secret");
    }

    #[test]
    fn workspace_file_resolution_rejects_traversal() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("ok.txt"), "ok").unwrap();
        std::fs::write(tmp.path().join("outside.txt"), "no").unwrap();

        assert_eq!(
            resolve_workspace_path(&workspace, "ok.txt")
                .unwrap()
                .file_name()
                .unwrap(),
            "ok.txt"
        );
        assert!(resolve_workspace_path(&workspace, "../outside.txt").is_none());
    }

    #[tokio::test]
    async fn static_mini_app_serves_self_contained_html() {
        let tmp = tempdir().unwrap();
        let state = ApiState::from_settings(test_settings(tmp.path())).unwrap();
        let path = crate::mini_app::static_mini_app_index_path(&state.settings.paths.workspace_dir, "calculator");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "<!doctype html><html><body><h1>calc</h1></body></html>").unwrap();

        let response = static_mini_app(State(state), AxumPath("calculator".to_string())).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            response.headers().get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
            "nosniff"
        );
        assert_eq!(response.headers().get(header::CACHE_CONTROL).unwrap(), "no-store, max-age=0");
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(std::str::from_utf8(&body).unwrap().contains("<h1>calc</h1>"));
    }

    #[test]
    fn api_event_preserves_event_name_and_data() {
        let event = ApiEvent::new("text", json!({"content": "hello"}));
        assert_eq!(event.event, "text");
        assert_eq!(event.data["content"], "hello");
    }

    #[tokio::test]
    async fn client_tool_context_routes_transport_tools_to_session_events() {
        let tmp = tempdir().unwrap();
        let state = ApiState::from_settings(test_settings(tmp.path())).unwrap();
        let (sender, mut receiver) = mpsc::channel::<ApiEvent>(SESSION_QUEUE_DEPTH);
        let session_id = state.register_session(99, sender).await;
        let context = state
            .client_tool_context(&session_id, 99, Some(42))
            .await
            .unwrap();

        let message_payload = context.send_message(
            "progress",
            "html",
            Some(r#"{"inline_keyboard":[[{"text":"Start","callback_data":"start"}]]}"#),
        );
        let message: Value = serde_json::from_str(&message_payload).unwrap();
        assert_eq!(message["success"], true);
        let event = receiver.recv().await.unwrap();
        assert_eq!(event.event, "text");
        assert_eq!(event.data["content"], "progress");
        assert_eq!(event.data["parse_mode"], "HTML");
        assert_eq!(event.data["message_id"], 1);
        assert_eq!(
            event.data["reply_markup"]["inline_keyboard"][0][0]["text"],
            "Start"
        );

        let reaction_payload = context.react("✅", 0);
        let reaction: Value = serde_json::from_str(&reaction_payload).unwrap();
        assert_eq!(reaction["success"], true);
        let event = receiver.recv().await.unwrap();
        assert_eq!(event.event, "reaction");
        assert_eq!(event.data["emoji"], "✅");
        assert_eq!(event.data["message_id"], 42);
    }

    #[tokio::test]
    async fn api_stream_guard_cancels_current_session_on_drop() {
        let tmp = tempdir().unwrap();
        let state = ApiState::from_settings(test_settings(tmp.path())).unwrap();
        let (sender, _receiver) = mpsc::channel::<ApiEvent>(SESSION_QUEUE_DEPTH);
        let session_id = state.register_session(99, sender).await;
        let started = Arc::new(Notify::new());
        let callback: ProcessCallback = {
            let started = started.clone();
            Arc::new(move |_context: ProcessContext| {
                let started = started.clone();
                Box::pin(async move {
                    started.notify_waiters();
                    sleep(Duration::from_secs(60)).await;
                    Ok(())
                })
            })
        };

        state
            .conversations
            .add_message(99, 7, "hello", None, Some(callback))
            .await;
        timeout(Duration::from_secs(1), started.notified())
            .await
            .unwrap();
        assert!(state.conversations.is_processing(99).await);

        drop(ApiStreamGuard::new(state.clone(), 99, session_id.clone()));

        timeout(Duration::from_secs(1), async {
            loop {
                if !state.conversations.is_processing(99).await
                    && !state.session_matches_chat(99, &session_id).await
                {
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
}
