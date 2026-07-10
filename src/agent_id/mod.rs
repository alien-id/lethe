//! Alien agent-id integration: a cryptographic identity, an encrypted credential
//! vault, and (locally) a vault-sealed browser for each Lethe instance.
//!
//! Lethe shells out to the `agent-id-core` / `agent-id-vault` / `agent-id-browser`
//! CLIs (installed on PATH, or pointed at by `AGENT_ID_*_BIN`). State is isolated
//! per instance under `AGENT_ID_STATE_DIR` (default `<LETHE_HOME>/agent-id`), so a
//! hosted per-user container and a local daemon each hold their own identity and
//! vault.
//!
//! Secrets never reach the model: vault tools return metadata only, and the
//! hosted secure-input channel (see `secure_prompt`) collects human-typed values
//! over an end-to-end-sealed side channel. See the repo README for the threat
//! model and its same-uid caveat.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::config::Settings;

pub mod cli;
pub mod crypto;
pub mod secure_prompt;

/// Whether the integration is enabled at all (`AGENT_ID_ENABLED`, default on).
pub fn is_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env_bool("AGENT_ID_ENABLED", true))
}

/// Hosted secure-prompt mode: run the unix-socket server so CLI children can
/// raise frontend credential cards. Set by the lethe-hosted supervisor
/// (`LETHE_SECURE_PROMPT=hosted`). Local API/TUI keep the loopback browser form.
pub fn secure_prompt_hosted() -> bool {
    std::env::var("LETHE_SECURE_PROMPT")
        .map(|v| v.trim().eq_ignore_ascii_case("hosted"))
        .unwrap_or(false)
}

/// The per-instance agent-id state directory.
pub fn state_dir(settings: &Settings) -> PathBuf {
    std::env::var_os("AGENT_ID_STATE_DIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| settings.paths.lethe_home.join("agent-id"))
}

/// The resolved state dir, cached at startup so tools (which don't carry
/// `Settings`) can reach it. Falls back to the env/`$HOME` default before
/// `set_state_dir` runs.
static STATE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Cache the resolved state dir. Called once at startup (provisioning / serve).
pub fn set_state_dir(settings: &Settings) {
    let _ = STATE_DIR.set(state_dir(settings));
}

/// The cached state dir, or the env/`$HOME/.agent-id` fallback.
pub fn cached_state_dir() -> PathBuf {
    if let Some(dir) = STATE_DIR.get() {
        return dir.clone();
    }
    std::env::var_os("AGENT_ID_STATE_DIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".agent-id")
        })
}

/// Directory for runtime sockets (`<LETHE_HOME>/run`).
fn run_dir(settings: &Settings) -> PathBuf {
    settings.paths.lethe_home.join("run")
}

/// Path of the secure-prompt unix socket. Falls back to a hashed `/tmp` path if
/// the natural path would exceed the platform `sun_path` limit.
pub fn secure_prompt_socket_path(settings: &Settings) -> PathBuf {
    let natural = run_dir(settings).join("secure-prompt.sock");
    // sun_path is 104 bytes on macOS, 108 on Linux; stay well under the min.
    if natural.as_os_str().len() < 100 {
        return natural;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&settings.paths.lethe_home, &mut hasher);
    let h = std::hash::Hasher::finish(&hasher);
    PathBuf::from(format!("/tmp/lethe-sp-{h:016x}.sock"))
}

/// Identity + vault tools are usable when enabled and both CLIs are present.
pub fn vault_tools_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        is_enabled()
            && find_bin("AGENT_ID_CORE_BIN", "agent-id-core").is_some()
            && find_bin("AGENT_ID_VAULT_BIN", "agent-id-vault").is_some()
    })
}

/// Browser tools additionally require the (marketplace-only) browser CLI.
pub fn browser_tools_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        vault_tools_available() && find_bin("AGENT_ID_BROWSER_BIN", "agent-id-browser").is_some()
    })
}

/// A headed browser login needs a real GUI session. Honors an explicit override
/// (`LETHE_BROWSER_FORCE_HEADED`); otherwise probes for a display.
pub fn browser_headed_available() -> bool {
    if !browser_tools_available() {
        return false;
    }
    if env_bool("LETHE_BROWSER_FORCE_HEADED", false) {
        return true;
    }
    #[cfg(target_os = "macos")]
    {
        // A GUI session sets these; an SSH-only shell on a headless Mac does not.
        std::env::var_os("XPC_FLAGS").is_some() && has_aqua_session()
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
    }
}

#[cfg(target_os = "macos")]
fn has_aqua_session() -> bool {
    // `launchctl managername` prints "Aqua" inside a GUI login session.
    std::process::Command::new("launchctl")
        .arg("managername")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "Aqua")
        .unwrap_or(false)
}

/// Resolve a CLI to an executable argv prefix: `[program]`, or `[node, path]`
/// when the target is a `.mjs`/`.js` script (the marketplace browser plugin).
pub(crate) fn find_bin(env_key: &str, name: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(env_key)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        && path.exists()
    {
        return Some(path);
    }
    which(name)
}

/// Minimal `which`: first executable named `name` on `PATH`.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Provision identity + vault at startup: create an L0 identity if none exists,
/// initialize the vault (agent-key slot) if none exists. Idempotent; degrades to
/// a warning if the CLIs are missing or fail, leaving Lethe otherwise usable.
pub async fn ensure_provisioned(settings: &Settings) {
    set_state_dir(settings);
    ensure_provisioned_at(&state_dir(settings)).await;
}

/// Provision one tenant's identity + vault without touching the process-global
/// standalone state cache. Multiplexed hosts call this with a tenant-private
/// directory before exposing the agent-id tools for that tenant.
pub async fn ensure_provisioned_at(sd: &Path) {
    if !is_enabled() {
        return;
    }
    if !vault_tools_available() {
        tracing::info!(
            "agent-id: core/vault CLIs not found on PATH; identity + vault tools disabled \
             (install with `npm i -g @alien-id/agent-id-core @alien-id/agent-id-vault`)"
        );
        return;
    }

    if let Err(err) = std::fs::create_dir_all(sd) {
        tracing::warn!(error = %err, dir = %sd.display(), "agent-id: could not create state dir");
        return;
    }
    set_private_dir(sd);

    // Identity (L0 is instant, no network).
    let status = cli::run_json(cli::Bin::Core, sd, &["status"]).await;
    let initialized = status
        .get("initialized")
        .and_then(serde_json::Value::as_bool);
    if initialized == Some(false) {
        let init = cli::run_json(cli::Bin::Core, sd, &["init"]).await;
        match init.get("fingerprint").and_then(serde_json::Value::as_str) {
            Some(fp) => tracing::info!(fingerprint = %fp, "agent-id: created L0 identity"),
            None => {
                tracing::warn!(result = %init, "agent-id: identity init returned no fingerprint")
            }
        }
    }

    // Vault: check the file directly (the CLI has no machine-readable error code
    // for "not found", so string-matching its message would be brittle).
    let vault_file = sd.join("vault.enc");
    if !vault_file.exists() {
        let init = cli::run_json(cli::Bin::Vault, sd, &["init"]).await;
        if init.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
            tracing::info!("agent-id: initialized credential vault (agent-key slot)");
        } else {
            tracing::warn!(result = %init, "agent-id: vault init did not report ok");
        }
    }
}

/// Close and reseal every live vault-browser session for one tenant. Hosted
/// multiplexers call this after each turn so Chromium and plaintext working
/// profiles never remain resident while the user is idle.
pub async fn close_browser_sessions(sd: &Path) {
    if !browser_tools_available() {
        return;
    }
    let sessions = cli::run_json(cli::Bin::Browser, sd, &["sessions"]).await;
    let names = sessions
        .get("sessions")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("name").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect::<Vec<_>>();
    for name in names {
        let result = cli::run_json(cli::Bin::Browser, sd, &["close", "--name", &name]).await;
        if result.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            tracing::warn!(session = %name, result = %result, "agent-id browser cleanup failed");
        }
    }
}

fn set_private_dir(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key)
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => default,
    }
}
