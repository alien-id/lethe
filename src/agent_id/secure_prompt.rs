//! Hosted secure-input channel.
//!
//! Implements the agent-id-core `HostedHarnessProvider` protocol: a unix-domain
//! socket that speaks minimal HTTP/1.1. When a Lethe-spawned CLI child (vault
//! `add --form`, `set-totp`, an access-widen confirmation, …) needs a human to
//! type a secret out of band, its `collectSecret` resolver POSTs the field spec
//! to this socket and blocks on the response. We surface the request to the web
//! frontend as a `secure_input.request` SSE event carrying a per-request
//! ephemeral P-256 public key; the browser seals the typed values to that key
//! (see `crypto.rs`) and POSTs the ciphertext to `/secure-input`; we unseal and
//! hand `{ values }` back on the still-open socket connection.
//!
//! Trust boundary (documented in the design and the repo threat model): the
//! socket is same-uid with the agent's shell, so we bind each connection to the
//! set of child PIDs Lethe itself launched (SO_PEERCRED / LOCAL_PEERPID). A
//! prompt-injected agent that `curl`s the socket with a forged spec is rejected
//! because its PID was never authorized — it cannot fabricate a credential card
//! to harvest a freshly typed secret. This does NOT defend against an agent
//! reading the vault it already holds the key to; that boundary is the uid.

use std::collections::{HashMap, HashSet};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;
use zeroize::Zeroize;

use super::crypto::{ALG, SealedInput, ServerEphemeral, UnsealError, unseal};

/// Hard ceiling on how long a pending request lives, regardless of what the CLI
/// requested. Must exceed the CLI-bridge subprocess budget's human-wait so the
/// frontend card and the child agree on when the request is dead.
const MAX_DEADLINE: Duration = Duration::from_secs(15 * 60);
/// Window to absorb the spawn→authorize race: a freshly spawned CLI child may
/// connect before the bridge has registered its PID.
const AUTHORIZE_WAIT: Duration = Duration::from_secs(1);

pub type Emit = Arc<dyn Fn(&str, Value) + Send + Sync>;

struct Pending {
    server: ServerEphemeral,
    /// Public snapshot for `GET /secure-input/pending` (a reconnecting tab
    /// re-hydrates cards from this — the SSE event has no replay).
    public: Value,
    resolve: Option<oneshot::Sender<Vec<u8>>>,
}

struct HubInner {
    socket_path: PathBuf,
    pending: Mutex<HashMap<String, Pending>>,
    authorized_pids: Mutex<HashSet<u32>>,
    emit: Emit,
}

/// Shared, cloneable handle. Lives in `ApiState` (routes) and is copied into
/// `ToolRuntime` (the CLI bridge authorizes child PIDs through it).
#[derive(Clone)]
pub struct SecurePromptHub {
    inner: Arc<HubInner>,
}

pub enum SubmitOutcome {
    /// Unsealed and delivered to the waiting socket connection.
    Accepted,
    /// No such live request (unknown id, already resolved, expired, cancelled).
    NotFound,
    /// The id exists but the ciphertext failed authenticated decryption. The
    /// pending entry is left intact so a token holder cannot cancel a genuine
    /// request by submitting garbage; the frontend may retry.
    BadCiphertext(UnsealError),
}

impl SecurePromptHub {
    pub fn new(socket_path: PathBuf, emit: Emit) -> Self {
        Self {
            inner: Arc::new(HubInner {
                socket_path,
                pending: Mutex::new(HashMap::new()),
                authorized_pids: Mutex::new(HashSet::new()),
                emit,
            }),
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.inner.socket_path
    }

    /// Authorize a child PID to POST specs. Called by the CLI bridge right after
    /// spawning a secure-prompt-bearing subcommand.
    pub fn authorize(&self, pid: u32) {
        self.inner.authorized_pids.lock().unwrap().insert(pid);
    }

    pub fn deauthorize(&self, pid: u32) {
        self.inner.authorized_pids.lock().unwrap().remove(&pid);
    }

    fn is_authorized(&self, pid: u32) -> bool {
        self.inner.authorized_pids.lock().unwrap().contains(&pid)
    }

    async fn wait_authorized(&self, pid: u32) -> bool {
        let deadline = tokio::time::Instant::now() + AUTHORIZE_WAIT;
        loop {
            if self.is_authorized(pid) {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Snapshot of live pending requests, for `GET /secure-input/pending`.
    pub fn list_pending(&self) -> Vec<Value> {
        self.inner
            .pending
            .lock()
            .unwrap()
            .values()
            .map(|p| p.public.clone())
            .collect()
    }

    /// Attempt to unseal a submitted envelope and deliver it to the waiting
    /// socket connection.
    pub fn submit(&self, request_id: &str, sealed: &SealedInput) -> SubmitOutcome {
        let mut guard = self.inner.pending.lock().unwrap();
        let Some(pending) = guard.get_mut(request_id) else {
            return SubmitOutcome::NotFound;
        };
        match unseal(&pending.server, sealed, request_id) {
            Ok(plaintext) => {
                // Remove so a second submit can't double-resolve; hand the
                // plaintext to the socket task, which writes it back to the CLI.
                let mut pending = guard.remove(request_id).unwrap();
                match pending.resolve.take() {
                    Some(tx) => match tx.send(plaintext) {
                        Ok(()) => SubmitOutcome::Accepted,
                        // Receiver gone (connection already closed) — nothing to
                        // deliver to; treat as no-op cancellation.
                        Err(mut leftover) => {
                            leftover.zeroize();
                            SubmitOutcome::NotFound
                        }
                    },
                    None => SubmitOutcome::NotFound,
                }
            }
            Err(err) => SubmitOutcome::BadCiphertext(err),
        }
    }

    /// User-initiated dismissal. Dropping the entry drops the oneshot sender,
    /// which the socket task observes and reports as cancelled.
    pub fn cancel(&self, request_id: &str) -> bool {
        self.inner
            .pending
            .lock()
            .unwrap()
            .remove(request_id)
            .is_some()
    }

    fn emit(&self, event: &str, data: Value) {
        (self.inner.emit)(event, data);
    }

    /// Emit an event on the frontend `/events` stream (e.g. `agent_id.bound`
    /// when a background owner-binding completes).
    pub fn emit_event(&self, event: &str, data: Value) {
        self.emit(event, data);
    }

    /// Bind the socket (unlink stale, create parent 0700, chmod 0600).
    pub fn bind(&self) -> std::io::Result<UnixListener> {
        let path = &self.inner.socket_path;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            set_mode(parent, 0o700);
        }
        // A leftover socket from a crashed run would make bind() fail with
        // EADDRINUSE; it is a pointer, not state, so unlinking is safe.
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        set_mode(path, 0o600);
        Ok(listener)
    }
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Accept loop. Spawn this once at server start when hosted secure-prompt mode
/// is active.
pub async fn serve(hub: SecurePromptHub, listener: UnixListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let hub = hub.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_conn(hub, stream).await {
                        tracing::debug!(error = %err, "secure-prompt connection ended");
                    }
                });
            }
            Err(err) => {
                tracing::warn!(error = %err, "secure-prompt accept failed");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

async fn handle_conn(hub: SecurePromptHub, stream: UnixStream) -> std::io::Result<()> {
    let pid = peer_pid(stream.as_raw_fd());

    let (mut read_half, mut write_half) = stream.into_split();

    // Peer-PID allowlist: only CLI children Lethe launched may raise a prompt.
    let authorized = match pid {
        Some(pid) => hub.wait_authorized(pid).await,
        None => {
            // No credential available on this platform to identify the peer.
            // Hosted runs on Linux where SO_PEERCRED works; refuse otherwise so
            // we never surface an unauthenticated prompt.
            tracing::warn!("secure-prompt: peer pid unavailable; refusing connection");
            false
        }
    };
    if !authorized {
        write_http(&mut write_half, 403, &json!({ "error": "forbidden" })).await?;
        return Ok(());
    }

    let (_method, _path, body) = match read_http_request(&mut read_half).await {
        Ok(parts) => parts,
        Err(err) => {
            write_http(&mut write_half, 400, &json!({ "error": err })).await?;
            return Ok(());
        }
    };

    let spec: Value = match serde_json::from_slice(&body) {
        Ok(spec) => spec,
        Err(_) => {
            write_http(
                &mut write_half,
                400,
                &json!({ "error": "invalid spec json" }),
            )
            .await?;
            return Ok(());
        }
    };

    let request_id = uuid::Uuid::new_v4().to_string();
    let server = ServerEphemeral::generate();
    let requested_ms = spec
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(MAX_DEADLINE)
        .min(MAX_DEADLINE);
    let expires_at = now_ms() + requested_ms.as_millis() as u64;

    let (event_data, pending_entry) = build_payloads(&request_id, &spec, &server, expires_at);

    let (tx, rx) = oneshot::channel::<Vec<u8>>();
    hub.inner.pending.lock().unwrap().insert(
        request_id.clone(),
        Pending {
            server,
            public: pending_entry,
            resolve: Some(tx),
        },
    );

    hub.emit("secure_input.request", event_data);

    // Race: submission vs. the deadline vs. the CLI closing the connection
    // (child killed by turn-abort / kill_on_drop, or its own timeout).
    let outcome = tokio::select! {
        delivered = rx => match delivered {
            Ok(mut plaintext) => {
                let result = match serde_json::from_slice::<Value>(&plaintext) {
                    Ok(values) => {
                        write_http(&mut write_half, 200, &json!({ "values": values })).await?;
                        "submitted"
                    }
                    Err(_) => {
                        write_http(&mut write_half, 500, &json!({ "error": "bad values" })).await?;
                        "error"
                    }
                };
                plaintext.zeroize();
                result
            }
            // Sender dropped without sending → cancelled via hub.cancel().
            Err(_) => {
                write_http(&mut write_half, 409, &json!({ "error": "cancelled" })).await?;
                "cancelled"
            }
        },
        _ = tokio::time::sleep(requested_ms) => {
            hub.inner.pending.lock().unwrap().remove(&request_id);
            write_http(&mut write_half, 504, &json!({ "error": "timed out" })).await?;
            "expired"
        },
        _ = wait_closed(&mut read_half) => {
            hub.inner.pending.lock().unwrap().remove(&request_id);
            // Connection gone; nothing to write.
            "cancelled"
        },
    };

    hub.emit(
        "secure_input.resolved",
        json!({ "request_id": request_id, "outcome": outcome }),
    );
    Ok(())
}

/// Build the two shapes the frontend consumes from one spec:
///   - the flat `secure_input.request` SSE event (fields at top level), and
///   - the nested `GET /secure-input/pending` entry (fields under `spec`, so a
///     reconnecting tab re-hydrates the card with the full sealing envelope).
///
/// The CLI's `security` note is HTML-stripped in both.
fn build_payloads(
    request_id: &str,
    spec: &Value,
    server: &ServerEphemeral,
    expires_at: u64,
) -> (Value, Value) {
    let str_field = |key: &str| {
        spec.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let title = str_field("title");
    let description = str_field("description");
    let label = str_field("label");
    let security_note = strip_html(&str_field("security"));
    let submit_label = str_field("submitLabel");
    let fields = spec
        .get("fields")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|f| {
                    json!({
                        "name": f.get("name").and_then(Value::as_str).unwrap_or(""),
                        "label": f.get("label").and_then(Value::as_str),
                        "secret": f.get("secret").and_then(Value::as_bool).unwrap_or(true),
                        "required": f.get("required").and_then(Value::as_bool).unwrap_or(true),
                        "multiline": f.get("multiline").and_then(Value::as_bool).unwrap_or(false),
                        "placeholder": f.get("placeholder").and_then(Value::as_str),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let server_pub = server.public_b64();

    let event = json!({
        "request_id": request_id,
        "title": title,
        "description": description,
        "label": label,
        "security_note": security_note,
        "submit_label": submit_label,
        "fields": fields,
        "server_pub": server_pub,
        "alg": ALG,
        "expires_at": expires_at,
    });
    let pending_entry = json!({
        "request_id": request_id,
        "spec": {
            "title": title,
            "description": description,
            "label": label,
            "security_note": security_note,
            "submit_label": submit_label,
            "fields": fields,
        },
        "server_pub": server_pub,
        "alg": ALG,
        "expires_at": expires_at,
    });
    (event, pending_entry)
}

/// Best-effort tag stripper — the CLI's `security` note embeds `<code>` markup
/// that React would escape; we render it as plain text.
fn strip_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.trim().to_string()
}

/// Resolve once the peer closes the connection (EOF). The provider keeps the
/// request open until we reply, so any read here blocks until close.
async fn wait_closed(read_half: &mut tokio::net::unix::OwnedReadHalf) {
    let mut buf = [0u8; 64];
    loop {
        match read_half.read(&mut buf).await {
            Ok(0) => return,
            Ok(_) => continue, // unexpected extra bytes; keep waiting for EOF
            Err(_) => return,
        }
    }
}

/// Read a single HTTP/1.1 request from the socket: `(method, path, body)`.
async fn read_http_request(
    read_half: &mut tokio::net::unix::OwnedReadHalf,
) -> Result<(String, String, Vec<u8>), String> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let header_end;
    loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
        if buf.len() > 64 * 1024 {
            return Err("header too large".into());
        }
        let n = read_half
            .read(&mut chunk)
            .await
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connection closed before request".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }

    let head = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }

    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = read_half
            .read(&mut chunk)
            .await
            .map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    Ok((method, path, body))
}

async fn write_http(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    status: u16,
    body: &Value,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        409 => "Conflict",
        500 => "Internal Server Error",
        504 => "Gateway Timeout",
        _ => "OK",
    };
    let body = serde_json::to_vec(body).unwrap_or_default();
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    write_half.write_all(header.as_bytes()).await?;
    write_half.write_all(&body).await?;
    write_half.flush().await?;
    Ok(())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// --- Peer credentials ------------------------------------------------------

#[cfg(target_os = "linux")]
fn peer_pid(fd: std::os::fd::RawFd) -> Option<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc == 0 && cred.pid > 0 {
        Some(cred.pid as u32)
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
fn peer_pid(fd: std::os::fd::RawFd) -> Option<u32> {
    // getsockopt(fd, SOL_LOCAL, LOCAL_PEERPID) — SOL_LOCAL is 0 on Darwin.
    const SOL_LOCAL: libc::c_int = 0;
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            SOL_LOCAL,
            libc::LOCAL_PEERPID,
            &mut pid as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc == 0 && pid > 0 {
        Some(pid as u32)
    } else {
        None
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn peer_pid(_fd: std::os::fd::RawFd) -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn strip_html_removes_tags() {
        assert_eq!(
            strip_html("Uses <code>AES-256-GCM</code> once."),
            "Uses AES-256-GCM once."
        );
        assert_eq!(strip_html("plain"), "plain");
    }

    #[test]
    fn find_subslice_locates_header_terminator() {
        assert_eq!(find_subslice(b"ab\r\n\r\ncd", b"\r\n\r\n"), Some(2));
        assert_eq!(find_subslice(b"abcd", b"\r\n\r\n"), None);
    }

    type Events = Arc<StdMutex<Vec<(String, Value)>>>;

    fn test_hub(socket: PathBuf) -> (SecurePromptHub, Events) {
        let events: Events = Arc::new(StdMutex::new(Vec::new()));
        let sink = events.clone();
        let emit: Emit = Arc::new(move |ev: &str, data| {
            sink.lock().unwrap().push((ev.to_string(), data));
        });
        (SecurePromptHub::new(socket, emit), events)
    }

    /// Raw HTTP/1.1 POST over the unix socket (stands in for the CLI's
    /// `HostedHarnessProvider`). Returns `(status, body)`.
    async fn cli_post(socket: &Path, body: &str) -> (u16, String) {
        let mut stream = UnixStream::connect(socket).await.unwrap();
        let req = format!(
            "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf).to_string();
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
        (status, body)
    }

    async fn wait_for_request(events: &Events) -> Value {
        for _ in 0..200 {
            if let Some((_, data)) = events
                .lock()
                .unwrap()
                .iter()
                .find(|(ev, _)| ev == "secure_input.request")
            {
                return data.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no secure_input.request emitted");
    }

    #[tokio::test]
    async fn end_to_end_delivers_sealed_values() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("sp.sock");
        let (hub, events) = test_hub(socket.clone());
        let listener = hub.bind().unwrap();
        tokio::spawn(serve(hub.clone(), listener));

        // The CLI child is us: authorize our own PID (the peer of an in-process
        // connection is this process).
        hub.authorize(std::process::id());

        // CLI side: POST a field spec and block on the sealed response.
        let cli_socket = socket.clone();
        let cli = tokio::spawn(async move {
            let spec = json!({
                "title": "Sign in to Example",
                "fields": [{ "name": "password", "secret": true, "required": true }],
                "security": "Uses <code>AES-256-GCM</code>.",
            });
            cli_post(&cli_socket, &spec.to_string()).await
        });

        // Frontend side: read the request event, seal a response, submit it.
        let event = wait_for_request(&events).await;
        let request_id = event["request_id"].as_str().unwrap();
        let server_pub = event["server_pub"].as_str().unwrap();
        // The pending list must carry the nested sealing envelope for reconnects.
        let pending = hub.list_pending();
        assert_eq!(pending.len(), 1);
        assert!(pending[0]["spec"]["title"].as_str() == Some("Sign in to Example"));
        assert_eq!(pending[0]["server_pub"].as_str(), Some(server_pub));
        // The security note reached the event HTML-stripped.
        assert_eq!(event["security_note"].as_str(), Some("Uses AES-256-GCM."));

        let values = br#"{"password":"hunter2"}"#;
        let sealed = crate::agent_id::crypto::seal_for_test(server_pub, request_id, values);
        assert!(matches!(
            hub.submit(request_id, &sealed),
            SubmitOutcome::Accepted
        ));

        let (status, body) = cli.await.unwrap();
        assert_eq!(status, 200);
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["values"]["password"].as_str(), Some("hunter2"));

        // The pending entry is consumed after delivery.
        assert!(hub.list_pending().is_empty());
    }

    #[tokio::test]
    async fn unauthorized_peer_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("sp.sock");
        let (hub, _events) = test_hub(socket.clone());
        let listener = hub.bind().unwrap();
        tokio::spawn(serve(hub.clone(), listener));

        // Do NOT authorize our PID: a forged prompt (agent's own `curl`) is
        // rejected before any card is surfaced.
        let (status, _body) = cli_post(&socket, "{\"title\":\"forged\"}").await;
        assert_eq!(status, 403);
        assert!(hub.list_pending().is_empty());
    }

    #[tokio::test]
    async fn bad_ciphertext_leaves_request_live() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("sp.sock");
        let (hub, events) = test_hub(socket.clone());
        let listener = hub.bind().unwrap();
        tokio::spawn(serve(hub.clone(), listener));
        hub.authorize(std::process::id());

        let cli_socket = socket.clone();
        let cli = tokio::spawn(async move {
            cli_post(
                &cli_socket,
                &json!({ "title": "x", "fields": [] }).to_string(),
            )
            .await
        });

        let event = wait_for_request(&events).await;
        let request_id = event["request_id"].as_str().unwrap().to_string();
        let server_pub = event["server_pub"].as_str().unwrap().to_string();

        // A garbage submission must not consume the pending request.
        let garbage = crate::agent_id::crypto::SealedInput {
            client_pub: "AAAA".into(),
            salt: "AAAA".into(),
            iv: "AAAA".into(),
            ciphertext: "AAAA".into(),
        };
        assert!(matches!(
            hub.submit(&request_id, &garbage),
            SubmitOutcome::BadCiphertext(_)
        ));
        assert_eq!(
            hub.list_pending().len(),
            1,
            "request must survive a bad submit"
        );

        // A genuine submission still succeeds afterwards.
        let sealed =
            crate::agent_id::crypto::seal_for_test(&server_pub, &request_id, br#"{"a":"b"}"#);
        assert!(matches!(
            hub.submit(&request_id, &sealed),
            SubmitOutcome::Accepted
        ));
        let (status, _body) = cli.await.unwrap();
        assert_eq!(status, 200);
    }
}
