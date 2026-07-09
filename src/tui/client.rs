//! Thin HTTP+SSE client for the lethe API. Speaks the same event surface
//! defined in `interfaces/api.rs` and adapts it to the TUI's `UiEvent`
//! vocabulary. Holds no state beyond the bearer token and base URL.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::tui::events::UiEvent;

#[derive(Clone, Debug)]
pub struct LetheClient {
    base_url: String,
    token: String,
    http: Client,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ModelInfo {
    pub model: String,
    #[serde(default)]
    pub model_aux: String,
    #[serde(default)]
    pub provider: String,
}

/// Failure modes the TUI startup wants to discriminate so it can render
/// a clean, single-line message *before* taking over the terminal.
#[derive(Debug, thiserror::Error)]
pub enum PreflightError {
    #[error("API at {url} is unreachable: {source}")]
    Unreachable {
        url: String,
        #[source]
        source: anyhow::Error,
    },
    #[error("API rejected the bearer token (HTTP 401). Check LETHE_API_TOKEN.")]
    Unauthorized,
    #[error("API check failed: {0}")]
    Other(#[source] anyhow::Error),
}

impl LetheClient {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .context("build reqwest client")?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            http,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder.bearer_auth(&self.token)
    }

    pub async fn health(&self) -> Result<()> {
        let url = format!("{}/health", self.base_url);
        let response = self
            .http
            .get(url)
            .send()
            .await
            .context("GET /health")?
            .error_for_status()?;
        let _ = response.text().await?;
        Ok(())
    }

    /// Pre-flight check that the server is up *and* our token is
    /// accepted. Hits an auth-required endpoint so a wrong/missing
    /// token surfaces here, before we enter the alternate screen and
    /// hide the error behind a half-painted UI. Returns
    /// [`PreflightError`] with the specific failure mode so the caller
    /// can render a clean message and exit.
    pub async fn preflight(&self) -> Result<ModelInfo, PreflightError> {
        // Health first: a connection refused / DNS failure is far more
        // common than a token issue and produces a better error.
        if let Err(error) = self.health().await {
            return Err(PreflightError::Unreachable {
                url: self.base_url.clone(),
                source: anyhow::anyhow!(error.to_string()),
            });
        }
        let url = format!("{}/model", self.base_url);
        let response = match self.auth(self.http.get(url)).send().await {
            Ok(response) => response,
            Err(error) => {
                return Err(PreflightError::Unreachable {
                    url: self.base_url.clone(),
                    source: anyhow::anyhow!(error.to_string()),
                });
            }
        };
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(PreflightError::Unauthorized);
        }
        match response.error_for_status() {
            Ok(ok) => ok
                .json::<ModelInfo>()
                .await
                .map_err(|error| PreflightError::Other(anyhow::anyhow!(error.to_string()))),
            Err(error) => Err(PreflightError::Other(anyhow::anyhow!(error.to_string()))),
        }
    }

    pub async fn model(&self) -> Result<ModelInfo> {
        let url = format!("{}/model", self.base_url);
        let response = self.auth(self.http.get(url)).send().await?;
        let response = response.error_for_status()?;
        let info = response.json::<ModelInfo>().await?;
        Ok(info)
    }

    /// Switch the running agent's main model (POST /model). Reconfigures the
    /// live session; persistence to `.env` is a separate `lethe model`.
    pub async fn set_model(&self, model: &str) -> Result<ModelInfo> {
        let url = format!("{}/model", self.base_url);
        let response = self
            .auth(self.http.post(url).json(&json!({ "model": model })))
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            // Surface the server's `{"error": ...}` body — `error_for_status`
            // alone discards it, leaving a useless bare status code.
            let body = response.text().await.unwrap_or_default();
            let detail = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|value| {
                    value
                        .get("error")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .filter(|detail| !detail.is_empty())
                .unwrap_or_else(|| body.trim().to_string());
            return Err(anyhow!("server returned {status}: {detail}"));
        }
        let info = response.json::<ModelInfo>().await?;
        Ok(info)
    }

    pub async fn cancel(&self, chat_id: i64) -> Result<()> {
        let url = format!("{}/cancel", self.base_url);
        let _ = self
            .auth(self.http.post(url).json(&json!({"chat_id": chat_id})))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn list_actors(&self) -> Result<Vec<Value>> {
        let url = format!("{}/actors", self.base_url);
        let response = self.auth(self.http.get(url)).send().await?;
        let response = response.error_for_status()?;
        let payload: Value = response.json().await?;
        Ok(payload
            .get("actors")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    pub async fn list_todos(&self, include_completed: bool) -> Result<Vec<Value>> {
        let url = format!("{}/todos", self.base_url);
        let response = self
            .auth(self.http.get(url))
            .query(&[("include_completed", include_completed)])
            .send()
            .await?;
        let response = response.error_for_status()?;
        let payload: Value = response.json().await?;
        Ok(payload
            .get("todos")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    pub async fn list_activity(&self, limit: usize) -> Result<Vec<Value>> {
        let url = format!("{}/activity", self.base_url);
        let response = self
            .auth(self.http.get(url))
            .query(&[("limit", limit)])
            .send()
            .await?;
        let response = response.error_for_status()?;
        let payload: Value = response.json().await?;
        Ok(payload
            .get("activity")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// Unseen insights only — the number behind the sidebar's green dot.
    pub async fn unseen_insights(&self) -> Result<u64> {
        let url = format!("{}/activity/unseen-count", self.base_url);
        let response = self
            .auth(self.http.get(url))
            .query(&[("kind", "insight")])
            .send()
            .await?;
        let response = response.error_for_status()?;
        let payload: Value = response.json().await?;
        Ok(payload
            .get("unseen")
            .and_then(Value::as_u64)
            .unwrap_or_default())
    }

    pub async fn mark_activity_seen(&self, ids: &[i64]) -> Result<()> {
        let url = format!("{}/activity/seen", self.base_url);
        self.auth(self.http.post(url).json(&json!({ "ids": ids })))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn session_history(&self, limit: usize) -> Result<Vec<Value>> {
        let url = format!("{}/session/history", self.base_url);
        let response = self
            .auth(self.http.get(url))
            .query(&[("limit", limit)])
            .send()
            .await?;
        let response = response.error_for_status()?;
        let payload: Value = response.json().await?;
        Ok(payload
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// POST `/chat` and forward the SSE stream into `tx` as `UiEvent`s. The
    /// returned future ends when the server closes the stream (after `done`)
    /// or when the channel is dropped.
    pub async fn chat_stream(
        &self,
        chat_id: i64,
        user_id: i64,
        message: String,
        tx: mpsc::Sender<UiEvent>,
    ) -> Result<()> {
        let url = format!("{}/chat", self.base_url);
        let response = self
            .auth(self.http.post(url).json(&json!({
                "message": message,
                "user_id": user_id,
                "chat_id": chat_id,
            })))
            .send()
            .await?
            .error_for_status()?;

        let mut stream = response.bytes_stream().eventsource();
        while let Some(event) = stream.next().await {
            let event = match event {
                Ok(event) => event,
                Err(error) => {
                    let _ = tx
                        .send(UiEvent::Disconnected(format!("SSE error: {error}")))
                        .await;
                    return Err(anyhow!(error));
                }
            };
            let ui = parse_sse(&event.event, &event.data);
            let done = matches!(ui, UiEvent::TurnDone);
            if tx.send(ui).await.is_err() {
                break;
            }
            if done {
                break;
            }
        }
        Ok(())
    }

    /// Long-lived subscription to `/events`. Reconnects on transient failures.
    pub async fn events_stream(&self, tx: mpsc::Sender<UiEvent>) -> Result<()> {
        let url = format!("{}/events", self.base_url);
        loop {
            let response = match self.auth(self.http.get(&url)).send().await {
                Ok(response) => response,
                Err(error) => {
                    let _ = tx
                        .send(UiEvent::Disconnected(format!("events: {error}")))
                        .await;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };
            let response = match response.error_for_status() {
                Ok(response) => response,
                Err(error) => {
                    let _ = tx
                        .send(UiEvent::Disconnected(format!("events: {error}")))
                        .await;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };

            let _ = tx.send(UiEvent::Connected).await;
            let mut stream = response.bytes_stream().eventsource();
            while let Some(event) = stream.next().await {
                let Ok(event) = event else {
                    break;
                };
                let ui = parse_sse(&event.event, &event.data);
                if tx.send(ui).await.is_err() {
                    return Ok(());
                }
            }
            let _ = tx
                .send(UiEvent::Disconnected("events stream ended".into()))
                .await;
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

/// SSE event names match the strings emitted by `interfaces::api`. Anything
/// unknown surfaces as `UiEvent::Unknown` so we can debug new event types
/// without crashing.
fn parse_sse(event: &str, data: &str) -> UiEvent {
    let value: Value = serde_json::from_str(data).unwrap_or(Value::Null);
    match event {
        "text" => {
            let content = value
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            UiEvent::AssistantText(content)
        }
        "assistant.delta" => UiEvent::AssistantDelta(
            value
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        "typing_start" => UiEvent::TypingStart,
        "typing_stop" => UiEvent::TypingStop,
        "turn.start" => UiEvent::TurnStart,
        "done" => UiEvent::TurnDone,
        "reaction" => UiEvent::Reaction {
            emoji: value
                .get("emoji")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        "tool.start" => UiEvent::ToolStart {
            call_id: value
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            name: value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            args_preview: value
                .get("args_preview")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        "tool.end" => UiEvent::ToolEnd {
            call_id: value
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            name: value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            success: value
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            output_preview: value
                .get("output_preview")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            duration_ms: value
                .get("duration_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        },
        "activity.logged" => UiEvent::ActivityLogged,
        "actor.spawned" | "actor.state" | "actor.task" | "actor.message" => UiEvent::ActorEvent {
            kind: event.to_string(),
            actor_id: value
                .get("actor_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            payload: value.get("payload").cloned().unwrap_or(Value::Null),
        },
        "usage" => UiEvent::Usage {
            prompt_tokens: value
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        },
        other => UiEvent::Unknown {
            event: other.to_string(),
            data: value,
        },
    }
}
