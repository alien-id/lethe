//! Async bridge to the agent-id CLIs (`agent-id-core`, `agent-id-vault`,
//! `agent-id-browser`).
//!
//! Every call is `tokio::process` with `kill_on_drop(true)` so a cancelled turn
//! (the tool future is dropped at an `.await`) actually kills the child — the
//! secure-prompt-bearing subcommands can block for many minutes waiting on the
//! human, and must die when the user hits Stop. Secrets are never passed on the
//! argv; the only secret channel is the unix socket (see `secure_prompt`).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use super::secure_prompt::SecurePromptHub;

/// Budget for fast, non-interactive calls (status, list, init, sign).
pub const FAST_TIMEOUT: Duration = Duration::from_secs(60);
/// Budget for subcommands that can block on a human typing a secret. Must exceed
/// the secure-prompt server's 15-minute deadline plus slack so Lethe never kills
/// the child mid-wait (which would drop the secret while the card still shows).
pub const HUMAN_TIMEOUT: Duration = Duration::from_secs(16 * 60);

/// Which agent-id CLI to invoke.
#[derive(Clone, Copy)]
pub enum Bin {
    Core,
    Vault,
    Browser,
}

impl Bin {
    fn resolve(self) -> Option<PathBuf> {
        let (env_key, name) = match self {
            Bin::Core => ("AGENT_ID_CORE_BIN", "agent-id-core"),
            Bin::Vault => ("AGENT_ID_VAULT_BIN", "agent-id-vault"),
            Bin::Browser => ("AGENT_ID_BROWSER_BIN", "agent-id-browser"),
        };
        super::find_bin(env_key, name)
    }
}

/// A parsed CLI result. Non-zero exit or unparseable stdout is surfaced as an
/// error the tool layer turns into a JSON `{error}` for the model.
pub struct CliResult {
    pub json: Value,
    pub code: i32,
}

fn base_command(bin: Bin, state_dir: &Path) -> Result<Command, String> {
    let program = bin
        .resolve()
        .ok_or_else(|| "agent-id CLI not found on PATH".to_string())?;
    let mut cmd = Command::new(program);
    cmd.env("AGENT_ID_STATE_DIR", state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok(cmd)
}

/// Run a fast, non-interactive subcommand and parse its JSON stdout.
pub async fn run(bin: Bin, state_dir: &Path, args: &[&str]) -> Result<CliResult, String> {
    let mut cmd = base_command(bin, state_dir)?;
    cmd.args(args);
    run_command(cmd, FAST_TIMEOUT).await
}

/// Run a secure-prompt-bearing subcommand. When a hub is present (hosted mode),
/// wire the child to the socket and authorize its PID so its `collectSecret`
/// resolver can raise a frontend card; without a hub (local mode), the child's
/// resolver falls back to the loopback browser form. Always uses the long human
/// budget.
pub async fn run_interactive(
    bin: Bin,
    state_dir: &Path,
    args: &[&str],
    hub: Option<&SecurePromptHub>,
) -> Result<CliResult, String> {
    let mut cmd = base_command(bin, state_dir)?;
    cmd.args(args);
    if let Some(hub) = hub {
        cmd.env("AGENT_ID_SECURE_PROMPT_SOCK", hub.socket_path())
            .env("AGENT_ID_SECURE_PROMPT", "hosted");
    }

    let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
    // Authorize the child's PID before it can connect (the server also waits
    // briefly on unknown PIDs to absorb the race).
    let pid = child.id();
    if let (Some(hub), Some(pid)) = (hub, pid) {
        hub.authorize(pid);
    }

    let result = wait_parsed(&mut child, HUMAN_TIMEOUT).await;

    if let (Some(hub), Some(pid)) = (hub, pid) {
        hub.deauthorize(pid);
    }
    result
}

async fn run_command(cmd: Command, timeout: Duration) -> Result<CliResult, String> {
    let mut cmd = cmd;
    let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
    wait_parsed(&mut child, timeout).await
}

async fn wait_parsed(
    child: &mut tokio::process::Child,
    timeout: Duration,
) -> Result<CliResult, String> {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let collect = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_end(&mut out).await;
        }
        if let Some(mut s) = stderr {
            let _ = s.read_to_end(&mut err).await;
        }
        let status = child.wait().await;
        (out, err, status)
    };

    let (out, err, status) = match tokio::time::timeout(timeout, collect).await {
        Ok(triple) => triple,
        Err(_) => {
            let _ = child.start_kill();
            return Err("agent-id CLI timed out".to_string());
        }
    };

    let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
    let stdout_str = String::from_utf8_lossy(&out);
    let trimmed = stdout_str.trim();

    // The CLIs print one JSON object on stdout. If parsing fails, surface stderr
    // (trimmed) so the model sees something actionable.
    match serde_json::from_str::<Value>(trimmed) {
        Ok(json) => Ok(CliResult { json, code }),
        Err(_) => {
            let stderr_str = String::from_utf8_lossy(&err);
            let detail = stderr_str.trim();
            let detail = if detail.is_empty() { trimmed } else { detail };
            let detail: String = detail.chars().take(400).collect();
            Err(format!(
                "agent-id CLI produced no JSON (exit {code}): {detail}"
            ))
        }
    }
}

/// Convenience: run a fast subcommand and return the JSON, mapping errors to a
/// `{error}` object the tool layer can hand straight to the model.
pub async fn run_json(bin: Bin, state_dir: &Path, args: &[&str]) -> Value {
    match run(bin, state_dir, args).await {
        Ok(result) => result.json,
        Err(err) => json!({ "error": err }),
    }
}

/// Serializes every `agent-id-browser open` (agent tool calls AND the
/// viewer-triggered launch in `interfaces::browser_stream`). The CLI's `open`
/// is NOT idempotent — it wipes the session work dir and unseals fresh, so a
/// concurrent second open destroys a live session's profile dir.
static OPEN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The live half of `browser-sessions/<name>.json`: a session whose daemon
/// pid is still running. Linux-only pid probe (/proc) — on other platforms we
/// report None and `open` behaves as before (hosted runtimes are linux).
pub fn live_session(state_dir: &Path, name: &str) -> Option<Value> {
    let raw =
        std::fs::read_to_string(state_dir.join("browser-sessions").join(format!("{name}.json")))
            .ok()?;
    let info = serde_json::from_str::<Value>(&raw).ok()?;
    let pid = info.get("pid").and_then(Value::as_u64)?;
    if !cfg!(target_os = "linux") || !Path::new(&format!("/proc/{pid}")).exists() {
        return None;
    }
    Some(info)
}

/// Start a long-lived browser session daemon (`agent-id-browser open`), read its
/// `{ ready: true, ... }` line, then leave it running and drain the rest of its
/// stdout to a log so it never blocks on a full pipe. Returns the ready line.
///
/// Idempotent per session: if `session_name` already has a live daemon, the
/// existing session is returned (`reused: true`) instead of re-opening over it.
pub async fn spawn_daemon_ready(
    state_dir: &Path,
    session_name: &str,
    args: &[&str],
    log_path: PathBuf,
) -> Result<Value, String> {
    let _open_serialized = OPEN_LOCK.lock().await;
    if let Some(info) = live_session(state_dir, session_name) {
        return Ok(json!({
            "ok": true,
            "ready": true,
            "reused": true,
            "session": session_name,
            "headless": info.get("headless").cloned().unwrap_or(Value::Bool(true)),
        }));
    }
    let mut cmd = base_command(Bin::Browser, state_dir)?;
    // The daemon must OUTLIVE this call: do not kill it when the child handle
    // drops.
    cmd.kill_on_drop(false);
    cmd.args(args);
    let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "no stdout from browser daemon".to_string())?;
    // The daemon prints its own diagnostics on stderr (the readiness/JSON line is
    // on stdout). Take stderr so we can fold it into the error when the daemon
    // exits before ready — otherwise a clean failure (NO_PROFILE, a bad loginUrl,
    // a launch crash) surfaces only as an opaque timeout and reads as a "crash".
    let mut stderr = child.stderr.take();

    let mut reader = BufReader::new(stdout).lines();
    // Terminal outcome from the daemon's stdout: Ok(ready value), or Err(the
    // daemon's own {ok:false,...} message). None = stdout closed with no
    // verdict (the daemon died); we then read stderr for why. The daemon
    // PRETTY-PRINTS structured errors (multi-line JSON), so a per-line parse
    // alone would swallow them — accumulate lines from each `{` and retry the
    // buffer, otherwise "no browser-profile named 'x'" degrades to an opaque
    // "closed before ready".
    let mut stdout_tail = String::new();
    let outcome = tokio::time::timeout(Duration::from_secs(90), async {
        let mut json_buf = String::new();
        while let Ok(Some(line)) = reader.next_line().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            stdout_tail.push_str(trimmed);
            stdout_tail.push('\n');
            if trimmed.starts_with('{') {
                json_buf.clear();
            }
            json_buf.push_str(trimmed);
            json_buf.push('\n');
            let Ok(value) = serde_json::from_str::<Value>(json_buf.trim()) else {
                continue;
            };
            if value.get("ready").and_then(Value::as_bool) == Some(true) {
                return Some(Ok(value));
            }
            // A structured failure the daemon reports before ready — surface
            // its own message (e.g. "no browser-profile named 'x' — run
            // auto-login first") instead of a generic timeout.
            if value.get("ok").and_then(Value::as_bool) == Some(false) {
                let msg = value
                    .get("message")
                    .or_else(|| value.get("error"))
                    .and_then(Value::as_str)
                    .unwrap_or("browser daemon reported an error")
                    .to_string();
                return Some(Err(msg));
            }
        }
        None
    })
    .await
    .map_err(|_| "browser daemon did not report ready in time".to_string())?;

    match outcome {
        Some(Ok(ready)) => {
            // Ready: keep draining stdout to a log so the daemon doesn't SIGPIPE
            // later, and let the process run detached from this call.
            tokio::spawn(async move {
                let mut sink = tokio::fs::File::create(&log_path).await.ok();
                let mut lines = reader;
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Some(f) = sink.as_mut() {
                        use tokio::io::AsyncWriteExt;
                        let _ = f.write_all(line.as_bytes()).await;
                        let _ = f.write_all(b"\n").await;
                    }
                }
                let _ = child.wait().await;
            });
            Ok(ready)
        }
        // The daemon reported a structured failure, or closed with none. Either
        // way it isn't going to serve — reap it and return the real reason.
        Some(Err(msg)) => {
            let _ = child.kill().await;
            Err(msg)
        }
        None => {
            let mut buf = String::new();
            if let Some(err) = stderr.as_mut() {
                let _ = err.read_to_string(&mut buf).await;
            }
            let _ = child.kill().await;
            // Prefer stderr; fall back to whatever the daemon printed on
            // stdout (non-JSON text) so the model never sees a bare "closed
            // before ready" when there was a reason available.
            let tail = if buf.trim().is_empty() {
                stdout_tail.trim().chars().take(400).collect::<String>()
            } else {
                buf.trim().chars().take(400).collect::<String>()
            };
            if tail.is_empty() {
                Err("browser daemon closed before ready".to_string())
            } else {
                Err(format!("browser daemon closed before ready: {tail}"))
            }
        }
    }
}
