//! Anthropic OAuth login flow (Claude Pro/Max subscription).
//!
//! The runtime client (token refresh, request shaping) lives in
//! `client.rs` alongside the rest of the LLM router — this module just
//! holds the one-shot login command. PKCE + browser sign-in at
//! claude.ai → paste authorization code → exchange at
//! `console.anthropic.com/v1/oauth/token` → write tokens to
//! `~/.lethe/credentials/anthropic_oauth_tokens.json`.
//!
//! Mirrors the v0.17.1 Python `tools/oauth_login_anthropic.py`. We use
//! the manual paste-code path (not a localhost listener) because the
//! redirect URI is on console.anthropic.com — same model as the
//! Python predecessor and the upstream Claude Code CLI.

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use rand::Rng;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::llm::client::{
    ANTHROPIC_OAUTH_CLIENT_ID, ANTHROPIC_OAUTH_TOKEN_URL, anthropic_oauth_token_file,
};

const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code";

pub async fn run_device_login() -> Result<()> {
    println!();
    println!("Anthropic OAuth login (Claude Pro/Max)");
    println!("──────────────────────────────────────────");
    println!("Browser sign-in with PKCE. Paste the code shown after approval.");
    println!();

    let (verifier, challenge) = generate_pkce();
    let url = build_authorize_url(&verifier, &challenge);

    println!("Open this URL in a browser:");
    println!("    {url}");
    println!();
    best_effort_open(&url);
    let code = prompt_line("Paste the authorization code (or `code#state`): ")?;
    let code = code.trim();
    if code.is_empty() {
        bail!("no authorization code provided");
    }

    println!("Exchanging code for tokens...");
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .context("building HTTP client for Anthropic OAuth")?;
    let token_data = exchange_authorization_code(&http, code, &verifier)
        .await
        .context("token exchange")?;

    let access_token = token_data
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("token response missing access_token"))?
        .to_string();
    let refresh_token = token_data
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_string);
    let expires_in = token_data
        .get("expires_in")
        .and_then(Value::as_f64)
        .unwrap_or(3600.0);

    let token_file = anthropic_oauth_token_file();
    let payload = json!({
        "access_token": access_token,
        "refresh_token": refresh_token,
        "expires_at": unix_now_seconds() + expires_in,
    });
    write_token_file(&token_file, &payload)
        .with_context(|| format!("writing tokens to {}", token_file.display()))?;

    println!();
    println!("OAuth tokens saved to {}", token_file.display());
    println!(
        "Refresh token: {}",
        if refresh_token.is_some() { "yes" } else { "no" }
    );
    println!("Expires in: {expires_in:.0}s");
    Ok(())
}

fn generate_pkce() -> (String, String) {
    // RFC 7636: code_verifier is 43-128 chars of unreserved URL-safe
    // characters. We use 32 random bytes → base64url → ~43 chars.
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize());
    (verifier, challenge)
}

fn build_authorize_url(verifier: &str, challenge: &str) -> String {
    // Matches the Python predecessor's params (oauth_login_anthropic.py).
    // `state` is the verifier itself so the user can paste either `code`
    // or `code#state` and we can recover it.
    let params = [
        ("code", "true"),
        ("client_id", ANTHROPIC_OAUTH_CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", REDIRECT_URI),
        ("scope", SCOPES),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", verifier),
    ];
    let encoded = params
        .iter()
        .map(|(k, v)| format!("{}={}", k, url_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{AUTHORIZE_URL}?{encoded}")
}

async fn exchange_authorization_code(
    http: &reqwest::Client,
    code: &str,
    verifier: &str,
) -> Result<Value> {
    // The Anthropic console redirect appends `#<state>` to the code on
    // some browsers; the Python predecessor split on `#` and forwarded
    // both halves. Mirror that.
    let (auth_code, state) = match code.split_once('#') {
        Some((c, s)) => (c.trim(), Some(s.trim())),
        None => (code, None),
    };

    let mut body = serde_json::Map::new();
    body.insert("code".into(), Value::String(auth_code.to_string()));
    body.insert(
        "grant_type".into(),
        Value::String("authorization_code".into()),
    );
    body.insert(
        "client_id".into(),
        Value::String(ANTHROPIC_OAUTH_CLIENT_ID.to_string()),
    );
    body.insert("redirect_uri".into(), Value::String(REDIRECT_URI.into()));
    body.insert("code_verifier".into(), Value::String(verifier.to_string()));
    if let Some(state) = state
        && !state.is_empty()
    {
        body.insert("state".into(), Value::String(state.to_string()));
    }

    let response = http
        .post(ANTHROPIC_OAUTH_TOKEN_URL)
        .json(&Value::Object(body))
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
        .with_context(|| format!("invalid token response JSON: {}", truncate_err(&text)))
}

fn write_token_file(path: &Path, payload: &Value) -> Result<()> {
    let Some(parent) = path.parent() else {
        bail!("token path has no parent: {}", path.display());
    };
    fs::create_dir_all(parent)?;
    let text = serde_json::to_string_pretty(payload)?;
    fs::write(path, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn url_encode(value: &str) -> String {
    // Lightweight percent-encode for the OAuth URL — encode anything
    // that isn't unreserved (RFC 3986). No external dep.
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        let unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~');
        if unreserved {
            out.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

fn best_effort_open(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn();
}

fn prompt_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush().ok();
    let stdin = io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    Ok(line
        .trim_end_matches('\n')
        .trim_end_matches('\r')
        .to_string())
}

fn unix_now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
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

// The .env-rewriting bookend lives in `crate::llm::oauth_env` so the
// `lethe login anthropic` dispatch can prompt for models between the
// device-login and the .env write. Callers go through
// `oauth_env::prompt_provider_models` + `update_env_after_oauth_login`.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_pair_round_trips_through_sha256() {
        let (verifier, challenge) = generate_pkce();
        assert!(verifier.len() >= 43);
        assert!(challenge.len() >= 43);
        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize());
        assert_eq!(challenge, expected);
    }

    #[test]
    fn authorize_url_contains_required_params() {
        let url = build_authorize_url("verifier-xyz", "challenge-abc");
        assert!(url.starts_with(AUTHORIZE_URL));
        assert!(url.contains("client_id=9d1c250a"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge=challenge-abc"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=verifier-xyz"));
        // Scopes contain a colon — must be percent-encoded.
        assert!(url.contains("scope=org%3Acreate_api_key"));
    }

    #[test]
    fn url_encode_handles_reserved_and_unreserved_chars() {
        assert_eq!(url_encode("abc-XYZ_0.9~"), "abc-XYZ_0.9~");
        assert_eq!(url_encode("a b/c?d"), "a%20b%2Fc%3Fd");
        assert_eq!(url_encode("org:create_api_key"), "org%3Acreate_api_key");
    }

    #[test]
    fn token_file_round_trips_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("anthropic_oauth_tokens.json");
        let payload = json!({
            "access_token": "sk-ant-...",
            "refresh_token": "refresh",
            "expires_at": 1_700_000_000.0,
        });
        write_token_file(&path, &payload).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["access_token"], json!("sk-ant-..."));
        assert_eq!(parsed["refresh_token"], json!("refresh"));
        assert_eq!(parsed["expires_at"], json!(1_700_000_000.0));
    }
}
