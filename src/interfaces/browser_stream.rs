//! Live browser viewport relay.
//!
//! Bridges the authenticated `/browser/stream` WebSocket (served by the api
//! module) to whichever in-container browser stream server is live. Two
//! sources exist:
//!
//!  - `alien` — the agent-id-browser session daemon (vault-sealed patchright
//!    browser). Discovery: `<agent-id state dir>/browser-sessions/*.json`
//!    entries carrying `streamPort` + `streamToken` (written by
//!    agent-id-browser >= 7.3, alongside the daemon's own `port`/`token`).
//!    Dialed as `ws://127.0.0.1:{streamPort}/?token={streamToken}`.
//!  - `plain` — the public agent-browser daemon's built-in stream, pinned to
//!    `AGENT_BROWSER_STREAM_PORT` (default 9223) by the hosted supervisor.
//!
//! `alien` wins when both are live (it is the primary hosted stack); the
//! client can force one with `?source=alien|plain`. The relay itself is
//! transparent: viewport frames flow upstream→client and input events
//! client→upstream untouched, so the browser-side protocol is exactly what
//! the stream servers speak. When no source is dialable the client gets one
//! `{"type":"status","state":"no_browser"}` text message and a clean close —
//! "Lethe isn't browsing right now", not an error.

use std::path::PathBuf;

use axum::extract::ws::{Message as AxMessage, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::tungstenite::Message as TgMessage;

const DEFAULT_PLAIN_STREAM_PORT: u16 = 9223;

/// A dialable stream source, in preference order.
struct Source {
    name: &'static str,
    url: String,
}

fn agent_id_sessions_dir() -> PathBuf {
    crate::agent_id::cached_state_dir().join("browser-sessions")
}

/// Session-file fields we care about (agent-id-browser >= 7.3 adds the
/// stream pair; older daemons simply lack it and are skipped).
#[derive(serde::Deserialize)]
struct SessionFile {
    #[serde(rename = "streamPort")]
    stream_port: Option<u16>,
    #[serde(rename = "streamToken")]
    stream_token: Option<String>,
    #[serde(rename = "startedAt")]
    started_at: Option<u64>,
}

/// Newest alien session advertising a stream endpoint, if any.
fn alien_source() -> Option<Source> {
    let mut best: Option<(u64, Source)> = None;
    for entry in std::fs::read_dir(agent_id_sessions_dir()).ok()?.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(session) = serde_json::from_str::<SessionFile>(&raw) else {
            continue;
        };
        let (Some(port), Some(token)) = (session.stream_port, session.stream_token) else {
            continue;
        };
        let started = session.started_at.unwrap_or(0);
        let source = Source {
            name: "alien",
            url: format!("ws://127.0.0.1:{port}/?token={token}"),
        };
        if best.as_ref().is_none_or(|(t, _)| started >= *t) {
            best = Some((started, source));
        }
    }
    best.map(|(_, source)| source)
}

fn plain_source() -> Source {
    let port = std::env::var("AGENT_BROWSER_STREAM_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PLAIN_STREAM_PORT);
    Source {
        name: "plain",
        url: format!("ws://127.0.0.1:{port}"),
    }
}

fn candidates(requested: Option<&str>) -> Vec<Source> {
    match requested {
        Some("alien") => alien_source().into_iter().collect(),
        Some("plain") => vec![plain_source()],
        _ => {
            let mut list: Vec<Source> = alien_source().into_iter().collect();
            list.push(plain_source());
            list
        }
    }
}

async fn send_status(client: &mut WebSocket, state: &str, source: Option<&str>) {
    let mut body = json!({ "type": "status", "state": state });
    if let Some(source) = source {
        body["source"] = json!(source);
    }
    let _ = client.send(AxMessage::Text(body.to_string().into())).await;
}

/// Runs for the lifetime of one viewer connection. Dial failures across all
/// candidates are reported as `no_browser` (the daemons only listen while a
/// session is open, so a refused connection IS the "not browsing" signal).
pub async fn relay(mut client: WebSocket, requested_source: Option<String>) {
    let mut upstream = None;
    for source in candidates(requested_source.as_deref()) {
        match tokio_tungstenite::connect_async(&source.url).await {
            Ok((stream, _)) => {
                upstream = Some((source.name, stream));
                break;
            }
            Err(error) => {
                tracing::debug!(source = source.name, %error, "browser stream dial failed");
            }
        }
    }
    let Some((source_name, upstream)) = upstream else {
        send_status(&mut client, "no_browser", None).await;
        let _ = client.send(AxMessage::Close(None)).await;
        return;
    };

    send_status(&mut client, "relaying", Some(source_name)).await;
    tracing::info!(source = source_name, "browser stream relay opened");

    let (mut up_tx, mut up_rx) = upstream.split();
    let (mut client_tx, mut client_rx) = client.split();

    let to_upstream = async {
        while let Some(Ok(msg)) = client_rx.next().await {
            let forward = match msg {
                AxMessage::Text(text) => TgMessage::text(text.as_str()),
                AxMessage::Binary(bytes) => TgMessage::binary(bytes),
                AxMessage::Close(_) => break,
                // axum answers pings itself; nothing to forward.
                AxMessage::Ping(_) | AxMessage::Pong(_) => continue,
            };
            if up_tx.send(forward).await.is_err() {
                break;
            }
        }
    };

    let to_client = async {
        while let Some(Ok(msg)) = up_rx.next().await {
            let forward = match msg {
                TgMessage::Text(text) => AxMessage::Text(text.as_str().into()),
                TgMessage::Binary(bytes) => AxMessage::Binary(bytes),
                TgMessage::Close(_) => break,
                TgMessage::Ping(_) | TgMessage::Pong(_) | TgMessage::Frame(_) => continue,
            };
            if client_tx.send(forward).await.is_err() {
                break;
            }
        }
    };

    // Either side ending tears the whole relay down; dropping the halves
    // closes both sockets.
    tokio::select! {
        _ = to_upstream => {},
        _ = to_client => {},
    }
    tracing::info!(source = source_name, "browser stream relay closed");
}
