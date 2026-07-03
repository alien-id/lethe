//! Alien agent-id tools: cryptographic identity, the encrypted credential vault,
//! and (locally) the vault-sealed browser.
//!
//! These shell out to the agent-id CLIs via `crate::agent_id::cli`. Secrets never
//! transit the model: the vault tools take and return metadata only (there is no
//! `vault_show` and no generic `vault_exec` here), secret *values* are typed by
//! the human over the secure-input side channel (hosted) or the loopback browser
//! form (local), and the browser injects credentials inside its own session
//! process. See the module docs in `crate::agent_id` for the threat model.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use serde_json::{Value, json};

use crate::agent_id::cli::{self, Bin};
use crate::agent_id::secure_prompt::SecurePromptHub;
use crate::tools::registry::ToolRegistry;
use crate::tools::registry::args::{bool_arg, nonempty_string, string_arg, string_vec_arg};
use crate::tools::spec::{
    ToolCategory, ToolDef, ToolExecutor, p_bool, p_enum, p_str, p_str_array, p_str_req,
};

type BoxFuture<'a> = Pin<Box<dyn Future<Output = String> + Send + 'a>>;

fn err(message: impl Into<String>) -> String {
    json!({ "error": message.into() }).to_string()
}

fn hub_of<'a>(registry: &'a ToolRegistry<'a>) -> Option<&'a SecurePromptHub> {
    registry.runtime.secure_prompt.as_ref()
}

/// Run a fast subcommand and return its JSON as the tool string.
async fn fast(bin: Bin, sd: &Path, argv: &[String]) -> String {
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    cli::run_json(bin, sd, &refs).await.to_string()
}

/// Run a subcommand that can block on a human (secure form / headed window).
async fn interactive(bin: Bin, sd: &Path, argv: &[String], hub: Option<&SecurePromptHub>) -> String {
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    match cli::run_interactive(bin, sd, &refs, hub).await {
        Ok(result) => result.json.to_string(),
        Err(message) => err(message),
    }
}

// ── Identity ───────────────────────────────────────────────────────────────

fn exec_agent_id_status<'a>(_r: &'a ToolRegistry<'a>, _args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = crate::agent_id::cached_state_dir();
        fast(Bin::Core, &sd, &["status".to_string()]).await
    })
}

fn exec_agent_id_sign<'a>(_r: &'a ToolRegistry<'a>, args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = crate::agent_id::cached_state_dir();
        let (Some(op_type), Some(action), Some(payload)) = (
            nonempty_string(args, "type"),
            nonempty_string(args, "action"),
            nonempty_string(args, "payload"),
        ) else {
            return err("`type`, `action` and `payload` are required.");
        };
        let mut argv = vec![
            "sign".to_string(),
            "--type".to_string(),
            op_type,
            "--action".to_string(),
            action,
            "--payload".to_string(),
            payload,
        ];
        if let Some(meta) = nonempty_string(args, "meta") {
            argv.push("--meta".to_string());
            argv.push(meta);
        }
        fast(Bin::Core, &sd, &argv).await
    })
}

fn exec_agent_id_bind<'a>(r: &'a ToolRegistry<'a>, args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = crate::agent_id::cached_state_dir();
        let hub = hub_of(r).cloned();

        // Resume a live pending binding rather than voiding the user's in-flight
        // approval by re-running `auth`.
        if let Some(pending) = read_live_pending_auth(&sd) {
            spawn_bind_poll(sd.clone(), hub);
            return json!({
                "ok": true,
                "resumed": true,
                "deep_link": pending.get("deepLink").cloned().unwrap_or(Value::Null),
                "message": "Resuming a pending owner-binding — approve it in your Alien app. I'll confirm when it completes (or call agent_id_status).",
            })
            .to_string();
        }

        let provider = nonempty_string(args, "provider_address")
            .or_else(|| std::env::var("ALIEN_PROVIDER_ADDRESS").ok().filter(|v| !v.trim().is_empty()));
        let Some(provider) = provider else {
            return err(
                "No provider address. Pass `provider_address` or set ALIEN_PROVIDER_ADDRESS.",
            );
        };

        // `auth` returns the deep link + QR immediately (it does NOT read the env
        // var, so pass it explicitly).
        let auth = fast_json(
            Bin::Core,
            &sd,
            &["auth".to_string(), "--provider-address".to_string(), provider],
        )
        .await;
        if auth.get("ok").and_then(Value::as_bool) != Some(true) {
            return auth.to_string();
        }
        spawn_bind_poll(sd.clone(), hub);
        json!({
            "ok": true,
            "deep_link": auth.get("deepLink").cloned().unwrap_or(Value::Null),
            "qr_code": auth.get("qrCode").cloned().unwrap_or(Value::Null),
            "expires_at": auth.get("expiredAt").cloned().unwrap_or(Value::Null),
            "message": "Scan the QR (or open the deep link) in your Alien app to bind this agent to you. I'll confirm here when it completes, or call agent_id_status.",
        })
        .to_string()
    })
}

async fn fast_json(bin: Bin, sd: &Path, argv: &[String]) -> Value {
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    cli::run_json(bin, sd, &refs).await
}

/// Poll `bind` in the background so the turn never blocks the ~14-minute human
/// ceremony. Emits `agent_id.bound` on success when a hub is present.
fn spawn_bind_poll(sd: PathBuf, hub: Option<SecurePromptHub>) {
    tokio::spawn(async move {
        let result = cli::run_interactive(
            Bin::Core,
            &sd,
            &["bind", "--timeout-sec", "840"],
            None,
        )
        .await;
        match result {
            Ok(r) if r.json.get("ok").and_then(Value::as_bool) == Some(true) => {
                tracing::info!("agent-id: owner binding completed");
                if let Some(hub) = hub {
                    hub.emit_event(
                        "agent_id.bound",
                        json!({
                            "owner_sub": r.json.get("ownerSub").cloned().unwrap_or(Value::Null),
                            "jkt": r.json.get("jkt").cloned().unwrap_or(Value::Null),
                        }),
                    );
                }
            }
            Ok(r) => tracing::info!(result = %r.json, "agent-id: owner binding did not complete"),
            Err(e) => tracing::info!(error = %e, "agent-id: owner binding poll ended"),
        }
    });
}

/// Read `pending-auth.json` and return it only if still within its window.
fn read_live_pending_auth(sd: &Path) -> Option<Value> {
    let raw = std::fs::read_to_string(sd.join("pending-auth.json")).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    let expires = value.get("expiredAt").and_then(Value::as_i64)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    (expires > now).then_some(value)
}

// ── Vault ───────────────────────────────────────────────────────────────────

fn exec_vault_list<'a>(_r: &'a ToolRegistry<'a>, _args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = crate::agent_id::cached_state_dir();
        fast(Bin::Vault, &sd, &["list".to_string()]).await
    })
}

fn exec_vault_add<'a>(r: &'a ToolRegistry<'a>, args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = crate::agent_id::cached_state_dir();
        let Some(name) = nonempty_string(args, "name") else {
            return err("`name` is required.");
        };
        let Some(cred_type) = nonempty_string(args, "type") else {
            return err("`type` is required (e.g. bearer, basic, header, oauth2, login, totp).");
        };
        let domains = string_vec_arg(args, "domains");
        let mut argv = vec![
            "add".to_string(),
            "--name".to_string(),
            name,
            "--type".to_string(),
            cred_type,
            // Secret values are collected over the secure channel, never argv.
            "--form".to_string(),
        ];
        if !domains.is_empty() {
            argv.push("--domains".to_string());
            argv.push(domains.join(","));
        }
        let access = string_arg(args, "access");
        if access == "ro" || access == "rw" {
            argv.push("--access".to_string());
            argv.push(access);
        }
        if let Some(desc) = nonempty_string(args, "description") {
            argv.push("--description".to_string());
            argv.push(desc);
        }
        interactive(Bin::Vault, &sd, &argv, hub_of(r)).await
    })
}

fn exec_vault_remove<'a>(_r: &'a ToolRegistry<'a>, args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = crate::agent_id::cached_state_dir();
        let Some(name) = nonempty_string(args, "name") else {
            return err("`name` is required.");
        };
        fast(Bin::Vault, &sd, &["remove".to_string(), "--name".to_string(), name]).await
    })
}

fn exec_vault_set_totp<'a>(r: &'a ToolRegistry<'a>, args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = crate::agent_id::cached_state_dir();
        let Some(name) = nonempty_string(args, "name") else {
            return err("`name` is required.");
        };
        let argv = vec![
            "set-totp".to_string(),
            "--name".to_string(),
            name,
            "--form".to_string(),
        ];
        interactive(Bin::Vault, &sd, &argv, hub_of(r)).await
    })
}

// ── Browser (local) ──────────────────────────────────────────────────────────

fn exec_browser_login<'a>(_r: &'a ToolRegistry<'a>, args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        if !crate::agent_id::browser_headed_available() {
            return err("A headed browser login needs a GUI session (none detected). Use auto_login with stored credentials instead.");
        }
        let sd = crate::agent_id::cached_state_dir();
        let mut argv = vec!["login".to_string()];
        push_opt(&mut argv, "--name", nonempty_string(args, "name"));
        push_opt(&mut argv, "--url", nonempty_string(args, "url"));
        push_opt(&mut argv, "--account", nonempty_string(args, "account"));
        let access = string_arg(args, "access");
        if access == "ro" || access == "rw" {
            argv.push("--access".to_string());
            argv.push(access);
        }
        if bool_arg(args, "fresh", false) {
            argv.push("--fresh".to_string());
        }
        // Headed owner sign-in — long, but no secure-prompt socket needed.
        interactive(Bin::Browser, &sd, &argv, None).await
    })
}

fn exec_browser_auto_login<'a>(r: &'a ToolRegistry<'a>, args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = crate::agent_id::cached_state_dir();
        let Some(cred) = nonempty_string(args, "cred") else {
            return err("`cred` (a `login` credential name) is required.");
        };
        let mut argv = vec!["auto-login".to_string(), "--cred".to_string(), cred];
        push_opt(&mut argv, "--name", nonempty_string(args, "name"));
        // May prompt for an interactive OTP when there's no stored TOTP seed.
        interactive(Bin::Browser, &sd, &argv, hub_of(r)).await
    })
}

fn exec_browser_open<'a>(_r: &'a ToolRegistry<'a>, args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = crate::agent_id::cached_state_dir();
        let name = nonempty_string(args, "name").unwrap_or_else(|| "main".to_string());
        let mut argv = vec!["open".to_string(), "--name".to_string(), name.clone()];
        if bool_arg(args, "headed", false) {
            if !crate::agent_id::browser_headed_available() {
                return err("`headed` requested but no GUI session is available.");
            }
            argv.push("--headed".to_string());
        }
        let log = std::env::temp_dir().join(format!("agent-browser-{name}.log"));
        match cli::spawn_daemon_ready(&sd, &argv.iter().map(String::as_str).collect::<Vec<_>>(), log).await {
            Ok(ready) => ready.to_string(),
            Err(message) => err(message),
        }
    })
}

fn exec_browser_close<'a>(_r: &'a ToolRegistry<'a>, args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = crate::agent_id::cached_state_dir();
        let name = nonempty_string(args, "name").unwrap_or_else(|| "main".to_string());
        fast(Bin::Browser, &sd, &["close".to_string(), "--name".to_string(), name]).await
    })
}

fn exec_browser_act<'a>(_r: &'a ToolRegistry<'a>, args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = crate::agent_id::cached_state_dir();
        let Some(action) = nonempty_string(args, "action") else {
            return err("`action` is required (e.g. snapshot, click, type, navigate, page-text, wait, tabs, screenshot).");
        };
        // Guard the secret-injection verbs to their dedicated tools so the
        // airgap contract is explicit and not reachable via a generic passthrough.
        if action == "fill-secret" || action == "fill-otp" {
            return err("Use alien_browser_fill_secret / alien_browser_fill_otp for credential injection.");
        }
        let mut argv = vec![action];
        if let Some(Value::Object(params)) = args.get("params") {
            append_flags(&mut argv, params);
        }
        push_opt(&mut argv, "--name", nonempty_string(args, "name"));
        fast(Bin::Browser, &sd, &argv).await
    })
}

fn exec_browser_fill_secret<'a>(_r: &'a ToolRegistry<'a>, args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = crate::agent_id::cached_state_dir();
        let (Some(reference), Some(cred)) =
            (nonempty_string(args, "ref"), nonempty_string(args, "cred"))
        else {
            return err("`ref` and `cred` (name.field) are required.");
        };
        let mut argv = vec![
            "fill-secret".to_string(),
            "--ref".to_string(),
            reference,
            "--cred".to_string(),
            cred,
        ];
        if bool_arg(args, "submit", false) {
            argv.push("--submit".to_string());
        }
        push_opt(&mut argv, "--name", nonempty_string(args, "name"));
        fast(Bin::Browser, &sd, &argv).await
    })
}

fn exec_browser_fill_otp<'a>(r: &'a ToolRegistry<'a>, args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = crate::agent_id::cached_state_dir();
        let (Some(reference), Some(cred)) =
            (nonempty_string(args, "ref"), nonempty_string(args, "cred"))
        else {
            return err("`ref` and `cred` are required.");
        };
        let argv = vec![
            "fill-otp".to_string(),
            "--ref".to_string(),
            reference,
            "--cred".to_string(),
            cred,
        ];
        // May prompt for an interactive code if the cred has no TOTP seed.
        interactive(Bin::Browser, &sd, &argv, hub_of(r)).await
    })
}

fn push_opt(argv: &mut Vec<String>, flag: &str, value: Option<String>) {
    if let Some(value) = value {
        argv.push(flag.to_string());
        argv.push(value);
    }
}

/// Turn a `params` object into `--key value` flags for the browser CLI. Bools
/// become bare flags when true; arrays are comma-joined.
fn append_flags(argv: &mut Vec<String>, params: &serde_json::Map<String, Value>) {
    for (key, value) in params {
        let flag = format!("--{key}");
        match value {
            Value::Bool(true) => argv.push(flag),
            Value::Bool(false) => {}
            Value::String(s) => {
                argv.push(flag);
                argv.push(s.clone());
            }
            Value::Number(n) => {
                argv.push(flag);
                argv.push(n.to_string());
            }
            Value::Array(items) => {
                let joined = items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string).or_else(|| {
                        if v.is_number() { Some(v.to_string()) } else { None }
                    }))
                    .collect::<Vec<_>>()
                    .join(",");
                argv.push(flag);
                argv.push(joined);
            }
            _ => {}
        }
    }
}

pub const TOOL_DEFS: &[ToolDef] = &[
    ToolDef {
        name: "agent_id_status",
        description: "Report this agent's Alien identity: assurance level (L0 self-asserted / L1 anonymous-human / L2 linked), key fingerprint, and owner-binding state.",
        params: &[],
        category: ToolCategory::AgentId,
        execute: ToolExecutor::Async(exec_agent_id_status),
    },
    ToolDef {
        name: "agent_id_bind",
        description: "Begin binding this agent to its human owner via the Alien app. Returns a deep link + QR for the owner to approve; binding completes in the background (identity keeps working as L0 until then). Safe to call again — it resumes a pending request rather than restarting it.",
        params: &[
            p_str("provider_address", "Alien SSO provider address. Defaults to ALIEN_PROVIDER_ADDRESS."),
        ],
        category: ToolCategory::AgentId,
        execute: ToolExecutor::Async(exec_agent_id_bind),
    },
    ToolDef {
        name: "agent_id_sign",
        description: "Append a signed, tamper-evident operation to this agent's audit trail (Ed25519 over a canonical envelope). Use to attest a meaningful action.",
        params: &[
            p_str_req("type", "Operation type (short label, e.g. 'commit', 'payment', 'email')."),
            p_str_req("action", "Action verb (e.g. 'create', 'send', 'approve')."),
            p_str_req("payload", "JSON string describing what was done."),
            p_str("meta", "Optional JSON string of extra metadata."),
        ],
        category: ToolCategory::AgentId,
        execute: ToolExecutor::Async(exec_agent_id_sign),
    },
    ToolDef {
        name: "vault_list",
        description: "List credentials in the Alien vault — names, types, domains, and access level only (never secret values). A credential listed here HAS its secret fields stored; null bookkeeping metadata (e.g. lastUsedAt) does NOT mean it is unfilled.",
        params: &[],
        category: ToolCategory::AgentId,
        execute: ToolExecutor::Async(exec_vault_list),
    },
    ToolDef {
        name: "vault_add",
        description: "Store a credential in the Alien vault. You supply only name/type/domains/access; the owner types the secret values into a secure form that appears AUTOMATICALLY — in the hosted chat it is a credential card shown right in this conversation's UI; locally it is a browser form on the owner's machine. No phone or external app is involved; never direct the owner elsewhere. The values never reach you or this conversation, and an ok result means the credential IS fully stored, secrets included. Types: bearer, basic, header, query, cookie, oauth2, login, totp.",
        params: &[
            p_str_req("name", "Credential name (letters, digits, dot/dash/underscore)."),
            p_enum("type", "Credential type.", &["bearer", "basic", "header", "query", "cookie", "oauth2", "login", "totp"]),
            p_str_array("domains", "Host allowlist this credential may be used on (e.g. api.github.com)."),
            p_enum("access", "Access level: 'ro' read-only or 'rw' unrestricted (default rw).", &["ro", "rw"]),
            p_str("description", "Optional human-readable description."),
        ],
        category: ToolCategory::AgentId,
        execute: ToolExecutor::Async(exec_vault_add),
    },
    ToolDef {
        name: "vault_remove",
        description: "Delete a credential from the Alien vault by name.",
        params: &[p_str_req("name", "Credential name to remove.")],
        category: ToolCategory::AgentId,
        execute: ToolExecutor::Async(exec_vault_remove),
    },
    ToolDef {
        name: "vault_set_totp",
        description: "Attach a 2FA/TOTP seed to a login or totp credential so logins can generate codes automatically. The owner types the seed into a secure form that appears automatically (hosted: a card in this chat; locally: a browser form) — no phone or external app is involved; it never reaches you. Only useful where a browser session can consume it.",
        params: &[p_str_req("name", "Credential name to attach the TOTP seed to.")],
        category: ToolCategory::AgentIdBrowser,
        execute: ToolExecutor::Async(exec_vault_set_totp),
    },
    ToolDef {
        name: "alien_browser_login",
        description: "Open a real (headed) browser window so the owner signs in once; the session is sealed into the vault for later headless reuse. Requires a local GUI session.",
        params: &[
            p_str("name", "Session name (default 'main')."),
            p_str("url", "URL to open for sign-in."),
            p_str("account", "Optional account label."),
            p_enum("access", "Seal the session read-only or read-write.", &["ro", "rw"]),
            p_bool("fresh", "Start from a fresh profile instead of resuming."),
        ],
        category: ToolCategory::AgentIdBrowser,
        execute: ToolExecutor::Async(exec_browser_login),
    },
    ToolDef {
        name: "alien_browser_auto_login",
        description: "Headlessly log in using a stored `login` credential (username + password + 2FA policy). 2FA is answered from a stored TOTP seed, or via a secure prompt to the owner.",
        params: &[
            p_str_req("cred", "Name of a `login` credential in the vault."),
            p_str("name", "Session name to seal into (default from the credential)."),
        ],
        category: ToolCategory::AgentIdBrowser,
        execute: ToolExecutor::Async(exec_browser_auto_login),
    },
    ToolDef {
        name: "alien_browser_open",
        description: "Start a persistent browser session daemon (unseals the sealed profile). Returns once ready; drive it with alien_browser_act and close it with alien_browser_close.",
        params: &[
            p_str("name", "Session name (default 'main')."),
            p_bool("headed", "Show the window (requires a GUI session)."),
        ],
        category: ToolCategory::AgentIdBrowser,
        execute: ToolExecutor::Async(exec_browser_open),
    },
    ToolDef {
        name: "alien_browser_close",
        description: "Close a running browser session; the profile is re-sealed into the vault and the working copy wiped.",
        params: &[p_str("name", "Session name (default 'main').")],
        category: ToolCategory::AgentIdBrowser,
        execute: ToolExecutor::Async(exec_browser_close),
    },
    ToolDef {
        name: "alien_browser_act",
        description: "Run a browser action in an open session. `action` is a verb (snapshot, click, type, navigate, page-text, wait, tabs, tab-new, screenshot, get, scroll, press, …) and `params` its flags (e.g. {\"ref\":\"e3\",\"text\":\"hi\"}). For credential injection use the dedicated fill tools.",
        params: &[
            p_str_req("action", "Browser verb to run."),
            p_str("name", "Session name (default 'main')."),
        ],
        category: ToolCategory::AgentIdBrowser,
        execute: ToolExecutor::Async(exec_browser_act),
    },
    ToolDef {
        name: "alien_browser_fill_secret",
        description: "Type a vault secret into a form field WITHOUT exposing it to you: the session process unlocks the vault, reads the value, and types it. You pass only the element ref and credential (name.field). Refused for sealed fields and off-allowlist hosts.",
        params: &[
            p_str_req("ref", "Element ref from a snapshot (e.g. e5)."),
            p_str_req("cred", "Credential reference as name.field (e.g. github.password)."),
            p_bool("submit", "Press Enter after filling."),
            p_str("name", "Session name (default 'main')."),
        ],
        category: ToolCategory::AgentIdBrowser,
        execute: ToolExecutor::Async(exec_browser_fill_secret),
    },
    ToolDef {
        name: "alien_browser_fill_otp",
        description: "Type the current 2FA code into a field WITHOUT exposing it to you: generated from the credential's stored TOTP seed, or prompted from the owner. Refused for off-allowlist hosts.",
        params: &[
            p_str_req("ref", "Element ref from a snapshot."),
            p_str_req("cred", "Credential name carrying the TOTP seed (login or totp)."),
            p_str("name", "Session name (default 'main')."),
        ],
        category: ToolCategory::AgentIdBrowser,
        execute: ToolExecutor::Async(exec_browser_fill_otp),
    },
];
