//! Alien agent-id tools: cryptographic identity, the encrypted credential vault,
//! and the vault-sealed browser (headless in a container/server, headed with a
//! display).
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
    ToolCategory, ToolDef, ToolExecutor, p_bool, p_enum, p_form_plan_req, p_obj, p_str,
    p_str_array, p_str_req,
};

type BoxFuture<'a> = Pin<Box<dyn Future<Output = String> + Send + 'a>>;

fn err(message: impl Into<String>) -> String {
    json!({ "error": message.into() }).to_string()
}

fn hub_of<'a>(registry: &'a ToolRegistry<'a>) -> Option<&'a SecurePromptHub> {
    registry.runtime.secure_prompt.as_ref()
}

fn state_dir_of(registry: &ToolRegistry<'_>) -> PathBuf {
    registry
        .runtime
        .agent_id_state_dir
        .clone()
        .unwrap_or_else(crate::agent_id::cached_state_dir)
}

fn hosted_safe(registry: &ToolRegistry<'_>) -> bool {
    registry.runtime.policy == crate::tools::registry::ToolPolicy::HostedSafe
}

fn interactive_hub<'a>(
    registry: &'a ToolRegistry<'a>,
) -> Result<Option<&'a SecurePromptHub>, String> {
    let hub = hub_of(registry);
    if hosted_safe(registry) && hub.is_none() {
        return Err(err("Secure credential input is temporarily unavailable."));
    }
    Ok(hub)
}

fn is_http_url(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    value.starts_with("https://") || value.starts_with("http://")
}

/// A secure-form credential write BLOCKS until the owner types and submits the
/// values, so an `ok` result means the fill already happened. Fold that fact into
/// the result as a `note`: without it the model reads `{ok:true}` as "I made an
/// empty slot" and — pulled by the strong "Alien" → Alien-app prior — tells the
/// owner to go fill it in an app and report back, when the card was right here in
/// the chat (hosted) or a browser form on their machine (local) and is already
/// done. `hosted` picks the surface named in the note.
fn note_secret_collected(result: String, hosted: bool) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(&result) else {
        return result;
    };
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
        return result; // an error result — nothing was collected
    }
    let surface = if hosted {
        "the secure card shown in THIS chat"
    } else {
        "the secure browser form on their machine"
    };
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "note".to_string(),
            json!(format!(
                "The owner has already entered and submitted the secret value(s) via {surface}; \
                 this credential is now fully stored, secrets included. Do NOT tell them to open \
                 an app or fill anything in elsewhere, and do NOT wait for them to confirm — the \
                 submission already happened. Continue with the task."
            )),
        );
    }
    value.to_string()
}

/// Run a fast subcommand and return its JSON as the tool string.
async fn fast(bin: Bin, sd: &Path, argv: &[String]) -> String {
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    cli::run_json(bin, sd, &refs).await.to_string()
}

/// Run a subcommand that can block on a human (secure form / headed window).
async fn interactive(
    bin: Bin,
    sd: &Path,
    argv: &[String],
    hub: Option<&SecurePromptHub>,
) -> String {
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    match cli::run_interactive(bin, sd, &refs, hub).await {
        Ok(result) => result.json.to_string(),
        Err(message) => err(message),
    }
}

// ── Identity ───────────────────────────────────────────────────────────────

fn exec_agent_id_status<'a>(r: &'a ToolRegistry<'a>, _args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = state_dir_of(r);
        fast(Bin::Core, &sd, &["status".to_string()]).await
    })
}

fn exec_agent_id_sign<'a>(r: &'a ToolRegistry<'a>, args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = state_dir_of(r);
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
        let sd = state_dir_of(r);
        let hub = hub_of(r).cloned();

        // Resume a live pending binding rather than voiding the user's in-flight
        // approval by re-running `auth`.
        if let Some(pending) = read_live_pending_auth(&sd) {
            spawn_bind_poll(sd.clone(), hub);
            return json!({
                "ok": true,
                "resumed": true,
                "deep_link": pending.get("deepLink").cloned().unwrap_or(Value::Null),
                "message": "Resuming a pending owner-binding — approve it in your Alien app. I'll confirm when it completes (or call agent_id_status). In a markdown chat, show deep_link inside a ```qr fenced code block (the UI renders it as a scannable QR) plus as a plain link.",
            })
            .to_string();
        }

        let provider = nonempty_string(args, "provider_address").or_else(|| {
            std::env::var("ALIEN_PROVIDER_ADDRESS")
                .ok()
                .filter(|v| !v.trim().is_empty())
        });
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
            &[
                "auth".to_string(),
                "--provider-address".to_string(),
                provider,
            ],
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
            "message": "Ask the owner to approve in their Alien app; I'll confirm here when it completes (or call agent_id_status). Presentation: in a markdown chat, show deep_link inside a ```qr fenced code block (the UI renders it as a scannable QR) plus as a plain link — qr_code is box-drawing art that is ONLY legible in a terminal, never paste it into markdown.",
        })
        .to_string()
    })
}

async fn fast_json(bin: Bin, sd: &Path, argv: &[String]) -> Value {
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    cli::run_json(bin, sd, &refs).await
}

/// Tracks which state dirs already have a live `bind` poller, so repeated
/// `agent_id_bind` calls (a model retrying across turns) don't stack N
/// concurrent 14-minute `agent-id-core bind` child processes against one
/// pending-auth file — which, unbounded and detached, can exhaust a shared
/// multi-tenant container's `--pids-limit`.
static ACTIVE_BIND_POLLS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<PathBuf>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// Poll `bind` in the background so the turn never blocks the ~14-minute human
/// ceremony. Emits `agent_id.bound` on success when a hub is present. At most
/// one poll runs per state dir; a call while one is already live is a no-op
/// (the existing poll will observe the same pending binding).
fn spawn_bind_poll(sd: PathBuf, hub: Option<SecurePromptHub>) {
    {
        let mut active = ACTIVE_BIND_POLLS.lock().expect("bind-poll set poisoned");
        if !active.insert(sd.clone()) {
            tracing::debug!("agent-id: bind poll already active for this identity; not stacking");
            return;
        }
    }
    tokio::spawn(async move {
        let result =
            cli::run_interactive(Bin::Core, &sd, &["bind", "--timeout-sec", "840"], None).await;
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
        ACTIVE_BIND_POLLS
            .lock()
            .expect("bind-poll set poisoned")
            .remove(&sd);
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

fn exec_vault_list<'a>(r: &'a ToolRegistry<'a>, _args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = state_dir_of(r);
        fast(Bin::Vault, &sd, &["list".to_string()]).await
    })
}

fn exec_vault_add<'a>(r: &'a ToolRegistry<'a>, args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = state_dir_of(r);
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
        // Non-secret: the sign-in page a `login` credential drives. Required for
        // alien_browser_auto_login to work — without it there is nowhere to start.
        if let Some(login_url) = nonempty_string(args, "login_url") {
            if hosted_safe(r) && !is_http_url(&login_url) {
                return err("Hosted login URLs must use http:// or https://.");
            }
            argv.push("--login-url".to_string());
            argv.push(login_url);
        }
        let hub = match interactive_hub(r) {
            Ok(hub) => hub,
            Err(error) => return error,
        };
        let hosted = hub.is_some();
        note_secret_collected(interactive(Bin::Vault, &sd, &argv, hub).await, hosted)
    })
}

fn exec_vault_remove<'a>(r: &'a ToolRegistry<'a>, args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = state_dir_of(r);
        let Some(name) = nonempty_string(args, "name") else {
            return err("`name` is required.");
        };
        fast(
            Bin::Vault,
            &sd,
            &["remove".to_string(), "--name".to_string(), name],
        )
        .await
    })
}

fn exec_vault_set_totp<'a>(r: &'a ToolRegistry<'a>, args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = state_dir_of(r);
        let Some(name) = nonempty_string(args, "name") else {
            return err("`name` is required.");
        };
        let argv = vec![
            "set-totp".to_string(),
            "--name".to_string(),
            name,
            "--form".to_string(),
        ];
        let hub = match interactive_hub(r) {
            Ok(hub) => hub,
            Err(error) => return error,
        };
        let hosted = hub.is_some();
        note_secret_collected(interactive(Bin::Vault, &sd, &argv, hub).await, hosted)
    })
}

// ── Browser (local) ──────────────────────────────────────────────────────────

fn exec_browser_login<'a>(r: &'a ToolRegistry<'a>, args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        if !crate::agent_id::browser_headed_available() {
            return err(
                "A headed browser login needs a GUI session (none detected). Use auto_login with stored credentials instead.",
            );
        }
        let sd = state_dir_of(r);
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
        let sd = state_dir_of(r);
        let Some(cred) = nonempty_string(args, "cred") else {
            return err("`cred` (a `login` credential name) is required.");
        };
        let mut argv = vec!["auto-login".to_string(), "--cred".to_string(), cred];
        push_opt(&mut argv, "--name", nonempty_string(args, "name"));
        // May prompt for an interactive OTP when there's no stored TOTP seed.
        let hub = match interactive_hub(r) {
            Ok(hub) => hub,
            Err(error) => return error,
        };
        interactive(Bin::Browser, &sd, &argv, hub).await
    })
}

fn exec_browser_open<'a>(r: &'a ToolRegistry<'a>, args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = state_dir_of(r);
        let name = nonempty_string(args, "name").unwrap_or_else(|| "main".to_string());
        let url = nonempty_string(args, "url");
        if hosted_safe(r)
            && let Some(url) = url.as_deref()
            && !is_http_url(url)
        {
            return err("Hosted browser navigation must use http:// or https://.");
        }
        let mut argv = vec!["open".to_string(), "--name".to_string(), name.clone()];
        if bool_arg(args, "headed", false) {
            if !crate::agent_id::browser_headed_available() {
                return err("`headed` requested but no GUI session is available.");
            }
            argv.push("--headed".to_string());
        }
        // Keep the daemon log inside this tenant's state dir, not a shared
        // /tmp path: many tenants run in one process, so a global
        // `browser-{name}.log` lets one tenant's daemon truncate and
        // interleave another's diagnostics — and `name` is model-controlled,
        // so a global path also invites traversal. `sanitize_session_name`
        // keeps the filename to a safe slug.
        let log_dir = sd.join("browser-logs");
        if let Err(error) = std::fs::create_dir_all(&log_dir) {
            return err(format!("could not create browser log dir: {error}"));
        }
        let log = log_dir.join(format!("open-{}.log", sanitize_session_name(&name)));
        match cli::spawn_daemon_ready(
            &sd,
            &name,
            &argv.iter().map(String::as_str).collect::<Vec<_>>(),
            log,
        )
        .await
        {
            Ok(ready) => {
                let Some(url) = url else {
                    return ready.to_string();
                };
                let navigation = fast_json(
                    Bin::Browser,
                    &sd,
                    &[
                        "navigate".to_string(),
                        "--url".to_string(),
                        url,
                        "--name".to_string(),
                        name,
                    ],
                )
                .await;
                json!({
                    "ok": navigation.get("ok").and_then(Value::as_bool) == Some(true),
                    "session": ready,
                    "navigation": navigation,
                })
                .to_string()
            }
            Err(message) => err(message),
        }
    })
}

fn exec_browser_close<'a>(r: &'a ToolRegistry<'a>, args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = state_dir_of(r);
        let name = nonempty_string(args, "name").unwrap_or_else(|| "main".to_string());
        fast(
            Bin::Browser,
            &sd,
            &["close".to_string(), "--name".to_string(), name],
        )
        .await
    })
}

fn exec_browser_act<'a>(r: &'a ToolRegistry<'a>, args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = state_dir_of(r);
        let Some(action) = nonempty_string(args, "action") else {
            return err(
                "`action` is required (e.g. snapshot, click, type, navigate, page-text, wait, tabs, screenshot).",
            );
        };
        // Guard the secret-injection verbs to their dedicated tools so the
        // airgap contract is explicit and not reachable via a generic passthrough.
        if action == "fill-secret" || action == "fill-otp" {
            return err(
                "Use alien_browser_fill_secret / alien_browser_fill_otp for credential injection.",
            );
        }
        if matches!(
            action.as_str(),
            "fill" | "upload" | "form-inspect" | "form-fill"
        ) {
            return err(
                "Use alien_browser_inspect_form and alien_browser_fill_form for ordinary form fields and workspace file uploads.",
            );
        }
        let mut params = args
            .get("params")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if hosted_safe(r)
            && let Err(message) = enforce_hosted_browser_params(&action, &mut params)
        {
            return err(&message);
        }
        let mut argv = vec![action];
        append_flags(&mut argv, &params);
        push_opt(&mut argv, "--name", nonempty_string(args, "name"));
        fast(Bin::Browser, &sd, &argv).await
    })
}

fn exec_browser_inspect_form<'a>(r: &'a ToolRegistry<'a>, args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = state_dir_of(r);
        let mut argv = vec!["form-inspect".to_string()];
        push_opt(&mut argv, "--name", nonempty_string(args, "name"));
        fast(Bin::Browser, &sd, &argv).await
    })
}

fn exec_browser_fill_form<'a>(r: &'a ToolRegistry<'a>, args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = state_dir_of(r);
        let Some(mut plan) = args.get("plan").and_then(Value::as_object).cloned() else {
            return err("`plan` is required.");
        };
        if let Err(message) = resolve_form_uploads(r, &mut plan) {
            return err(message);
        }
        let Ok(spec) = serde_json::to_string(&plan) else {
            return err("Could not encode the form plan.");
        };
        let mut argv = vec!["form-fill".to_string(), "--spec".to_string(), spec];
        push_opt(&mut argv, "--name", nonempty_string(args, "name"));
        fast(Bin::Browser, &sd, &argv).await
    })
}

/// Canonicalize only upload paths, through the same FileTools policy the rest
/// of the turn uses. Hosted registries are workspace-jailed; standalone keeps
/// its established full-machine behavior. Other form values are untouched.
fn resolve_form_uploads(
    registry: &ToolRegistry<'_>,
    plan: &mut serde_json::Map<String, Value>,
) -> Result<(), String> {
    let Some(uploads) = plan.get_mut("uploads") else {
        return Ok(());
    };
    let Some(uploads) = uploads.as_array_mut() else {
        return Err("`plan.uploads` must be an array.".to_string());
    };
    for upload in uploads {
        let Some(upload) = upload.as_object_mut() else {
            return Err("Each upload must be an object with `ref` and `files`.".to_string());
        };
        let Some(files) = upload.get_mut("files").and_then(Value::as_array_mut) else {
            return Err("Each upload needs a `files` array.".to_string());
        };
        for file in files {
            let Some(raw) = file.as_str() else {
                return Err("Upload file paths must be strings.".to_string());
            };
            let resolved = registry
                .files
                .resolve_existing_file(raw)
                .map_err(|message| format!("Upload denied: {message}"))?;
            *file = Value::String(resolved.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

fn exec_browser_fill_secret<'a>(r: &'a ToolRegistry<'a>, args: &'a Value) -> BoxFuture<'a> {
    Box::pin(async move {
        let sd = state_dir_of(r);
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
        let sd = state_dir_of(r);
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
        let hub = match interactive_hub(r) {
            Ok(hub) => hub,
            Err(error) => return error,
        };
        interactive(Bin::Browser, &sd, &argv, hub).await
    })
}

/// Reduce a model-supplied session name to a filesystem-safe slug for use in a
/// log filename: alphanumerics, dash, and underscore survive; everything else
/// (path separators, `..`, dots) becomes `_`. Bounded so a huge name can't
/// blow past filename limits. The value passed to the daemon as `--name` is
/// unchanged; this only guards the derived log path.
fn sanitize_session_name(name: &str) -> String {
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if slug.is_empty() {
        "session".to_string()
    } else {
        slug
    }
}

fn push_opt(argv: &mut Vec<String>, flag: &str, value: Option<String>) {
    if let Some(value) = value {
        argv.push(flag.to_string());
        argv.push(value);
    }
}

/// Verbs that hand the page arbitrary local files. Blocked wholesale under the
/// hosted policy — filesystem tools are disabled, so these would be the only
/// path back to the disk.
const HOSTED_BLOCKED_BROWSER_ACTIONS: &[&str] = &["upload"];

/// Param keys that name a caller-chosen output/input path. Stripped from EVERY
/// action under the hosted policy (not just the file-writing verbs known
/// today), so a plugin that later adds `pdf`/`har`/`record`/`trace`/`download`
/// with a `--path`/`--output` flag can't become an indirect file-write. When a
/// verb needs a file (screenshot/zoom), dropping the key falls back to the
/// plugin's default inside this tenant's state directory.
const HOSTED_BLOCKED_PATH_KEYS: &[&str] =
    &["path", "output", "out", "dest", "file", "files", "save"];

/// Param keys carrying a navigation target. Every one must be http(s) under the
/// hosted policy so no action can reach file://, chrome://, or a custom scheme.
const HOSTED_URL_KEYS: &[&str] = &["url", "start"];

/// Apply the hosted browser-action policy in place. Generalizes what used to be
/// a three-verb denylist: block file-handoff verbs, strip output-path flags
/// from all actions, and require http(s) on any navigation flag. A stricter
/// verb+flag allowlist owned by the plugin would be the deeper fix; this keeps
/// the gate from silently failing open as the plugin's verb set grows.
fn enforce_hosted_browser_params(
    action: &str,
    params: &mut serde_json::Map<String, Value>,
) -> Result<(), String> {
    if HOSTED_BLOCKED_BROWSER_ACTIONS.contains(&action) {
        return Err(format!(
            "The `{action}` browser action is disabled by the hosted capability policy."
        ));
    }
    for key in HOSTED_URL_KEYS {
        if let Some(url) = params.get(*key).and_then(Value::as_str)
            && !is_http_url(url)
        {
            return Err("Hosted browser navigation must use http:// or https://.".to_string());
        }
    }
    for key in HOSTED_BLOCKED_PATH_KEYS {
        params.remove(*key);
    }
    Ok(())
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
                    .filter_map(|v| {
                        v.as_str().map(str::to_string).or_else(|| {
                            if v.is_number() {
                                Some(v.to_string())
                            } else {
                                None
                            }
                        })
                    })
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
        params: &[p_str(
            "provider_address",
            "Alien SSO provider address. Defaults to ALIEN_PROVIDER_ADDRESS.",
        )],
        category: ToolCategory::AgentId,
        execute: ToolExecutor::Async(exec_agent_id_bind),
    },
    ToolDef {
        name: "agent_id_sign",
        description: "Append a signed, tamper-evident operation to this agent's audit trail (Ed25519 over a canonical envelope). Use to attest a meaningful action.",
        params: &[
            p_str_req(
                "type",
                "Operation type (short label, e.g. 'commit', 'payment', 'email').",
            ),
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
        description: "Store a credential in the Alien vault. You supply only name/type/domains/access; the owner types the secret values into a secure form that appears AUTOMATICALLY — in the hosted chat it is a credential card shown right in this conversation's UI; locally it is a browser form on the owner's machine. No phone or external app is involved; never direct the owner elsewhere. This call BLOCKS until the owner submits that form and returns only AFTER they have — so an ok result means the values are already entered and the credential is fully stored. Do not ask the owner to fill anything in or to report back; just continue. The values never reach you or this conversation. Types: bearer, basic, header, query, cookie, oauth2, login, totp.",
        params: &[
            p_str_req(
                "name",
                "Credential name (letters, digits, dot/dash/underscore).",
            ),
            p_enum(
                "type",
                "Credential type.",
                &[
                    "bearer", "basic", "header", "query", "cookie", "oauth2", "login", "totp",
                ],
            ),
            p_str_array(
                "domains",
                "Host allowlist this credential may be used on (e.g. api.github.com).",
            ),
            p_enum(
                "access",
                "Access level: 'ro' read-only or 'rw' unrestricted (default rw).",
                &["ro", "rw"],
            ),
            p_str("description", "Optional human-readable description."),
            p_str(
                "login_url",
                "For type=login only: the sign-in page URL (e.g. https://www.reddit.com/login). REQUIRED for alien_browser_auto_login to work — set it whenever you add a login credential you'll log in with.",
            ),
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
        params: &[p_str_req(
            "name",
            "Credential name to attach the TOTP seed to.",
        )],
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
            p_enum(
                "access",
                "Seal the session read-only or read-write.",
                &["ro", "rw"],
            ),
            p_bool("fresh", "Start from a fresh profile instead of resuming."),
        ],
        category: ToolCategory::AgentIdBrowser,
        execute: ToolExecutor::Async(exec_browser_login),
    },
    ToolDef {
        name: "alien_browser_auto_login",
        description: "Headlessly log in using a stored `login` credential (username + password + 2FA policy) and SEAL the resulting signed-in session into a browser-profile for reuse. Public browsing does not require this: alien_browser_open auto-creates the anonymous default profile. Use auto-login only when the task actually needs an account. Requires the login credential to have a login_url (set it on vault_add). 2FA is answered from a stored TOTP seed, or via a secure prompt to the owner.",
        params: &[
            p_str_req("cred", "Name of a `login` credential in the vault."),
            p_str(
                "name",
                "Session name to seal into (default from the credential).",
            ),
        ],
        category: ToolCategory::AgentIdBrowser,
        execute: ToolExecutor::Async(exec_browser_auto_login),
    },
    ToolDef {
        name: "alien_browser_open",
        description: "Start the persistent Alien browser and optionally navigate in the same call. The shared `main` profile is created automatically as an anonymous L0 profile on first use, so public pages need no login/setup. Returns once ready; use the typed form tools for forms and alien_browser_act for other actions.",
        params: &[
            p_str("name", "Session name (default 'main'). Only 'main' auto-creates; any other name must match a profile sealed by alien_browser_login/auto_login, else open fails with NO_PROFILE. For public browsing omit this."),
            p_str("url", "Optional http(s) URL to open immediately."),
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
        description: "Run a non-form browser action in an open Alien session: snapshot, click, navigate, page-text, wait, tabs, screenshot, get, scroll, press, etc. Flags belong in `params`; never paste CLI syntax into a ref/value. For forms use alien_browser_inspect_form then ONE alien_browser_fill_form call; for credentials use the dedicated secret/OTP tools.",
        params: &[
            p_str_req(
                "action",
                "Browser verb to run (bare verb only — flags go in `params`).",
            ),
            p_obj(
                "params",
                "Flags for the verb as key/value pairs, e.g. {\"url\": \"https://example.com\"} for navigate or {\"ref\": \"e3\", \"text\": \"hi\"} for type. `true` becomes a bare flag, arrays are comma-joined.",
            ),
            p_str("name", "Session name (default 'main')."),
        ],
        category: ToolCategory::AgentIdBrowser,
        execute: ToolExecutor::Async(exec_browser_act),
    },
    ToolDef {
        name: "alien_browser_inspect_form",
        description: "Inspect only the current page's form controls in a compact structured form: refs, associated labels, types, required/checked state, select options, and file accept rules. Text/password values are never returned. Call once, then pass the refs to alien_browser_fill_form.",
        params: &[p_str("name", "Session name (default 'main').")],
        category: ToolCategory::AgentIdBrowser,
        execute: ToolExecutor::Async(exec_browser_inspect_form),
    },
    ToolDef {
        name: "alien_browser_fill_form",
        description: "Fill and verify up to 50 ordinary controls in ONE fast atomic browser call. `plan` may contain fields [{ref,value}], checks [{ref,checked}], selects [{ref,values}], uploads [{ref,files}] and optional submit (a button ref). Each result is verified; failed controls and native validation errors are returned individually. Upload paths are confined to the user's workspace in hosted mode. Do not put passwords/OTP here — use the sealed credential tools.",
        params: &[
            p_form_plan_req(
                "plan",
                "Structured form plan, e.g. {\"fields\":[{\"ref\":\"e1\",\"value\":\"Ada\"}],\"checks\":[{\"ref\":\"e2\",\"checked\":true}],\"selects\":[{\"ref\":\"e3\",\"values\":[\"ms\"]}],\"uploads\":[{\"ref\":\"e4\",\"files\":[\"resume.pdf\"]}]}",
            ),
            p_str("name", "Session name (default 'main')."),
        ],
        category: ToolCategory::AgentIdBrowser,
        execute: ToolExecutor::Async(exec_browser_fill_form),
    },
    ToolDef {
        name: "alien_browser_fill_secret",
        description: "Type a vault secret into a form field WITHOUT exposing it to you: the session process unlocks the vault, reads the value, and types it. You pass only the element ref and credential (name.field). Refused for sealed fields and off-allowlist hosts.",
        params: &[
            p_str_req("ref", "Element ref from a snapshot (e.g. e5)."),
            p_str_req(
                "cred",
                "Credential reference as name.field (e.g. github.password).",
            ),
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
            p_str_req(
                "cred",
                "Credential name carrying the TOTP seed (login or totp).",
            ),
            p_str("name", "Session name (default 'main')."),
        ],
        category: ToolCategory::AgentIdBrowser,
        execute: ToolExecutor::Async(exec_browser_fill_otp),
    },
];

#[cfg(test)]
mod tests {
    use super::{
        enforce_hosted_browser_params, is_http_url, note_secret_collected, sanitize_session_name,
    };
    use serde_json::{Value, json};

    #[test]
    fn ok_result_gets_a_submission_note() {
        let out = note_secret_collected(r#"{"ok":true,"name":"LinkedIn"}"#.to_string(), true);
        let v: Value = serde_json::from_str(&out).unwrap();
        let note = v.get("note").and_then(Value::as_str).unwrap();
        assert!(
            note.contains("THIS chat"),
            "hosted note names the in-chat card"
        );
        assert!(
            note.contains("already"),
            "note states the fill already happened"
        );
        assert!(
            note.contains("Do NOT tell them to open"),
            "note forbids the go-to-an-app instruction",
        );
        // The original fields survive.
        assert_eq!(v.get("name").and_then(Value::as_str), Some("LinkedIn"));
    }

    #[test]
    fn local_mode_names_the_browser_form_not_a_chat_card() {
        let out = note_secret_collected(r#"{"ok":true}"#.to_string(), false);
        let v: Value = serde_json::from_str(&out).unwrap();
        let note = v.get("note").and_then(Value::as_str).unwrap();
        assert!(
            note.contains("browser form"),
            "local note names the browser form"
        );
        assert!(!note.contains("THIS chat"));
    }

    #[test]
    fn error_result_is_left_untouched() {
        let src = r#"{"ok":false,"error":"boom"}"#;
        assert_eq!(note_secret_collected(src.to_string(), true), src);
        // Non-JSON passes through verbatim too.
        assert_eq!(
            note_secret_collected("not json".to_string(), true),
            "not json"
        );
    }

    #[test]
    fn hosted_browser_urls_reject_local_file_schemes() {
        assert!(is_http_url("https://example.com"));
        assert!(is_http_url(" HTTP://example.com/path "));
        assert!(!is_http_url("file:///etc/passwd"));
        assert!(!is_http_url("data:text/plain,secret"));
        assert!(!is_http_url("javascript:alert(1)"));
    }

    fn map(value: Value) -> serde_json::Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn hosted_policy_blocks_file_handoff_verbs() {
        let mut params = map(json!({"ref": "@e1", "files": "/a.pdf"}));
        assert!(enforce_hosted_browser_params("upload", &mut params).is_err());
    }

    #[test]
    fn hosted_policy_strips_output_paths_from_any_action() {
        // Not just screenshot/zoom: a future file-writing verb with a path flag
        // must have it stripped rather than forwarded to the CLI.
        for action in ["screenshot", "zoom", "pdf", "har", "record", "trace"] {
            let mut params =
                map(json!({"path": "/etc/evil", "output": "/tmp/x", "region": "0,0,1,1"}));
            enforce_hosted_browser_params(action, &mut params).unwrap();
            assert!(!params.contains_key("path"), "{action} keeps path");
            assert!(!params.contains_key("output"), "{action} keeps output");
            // Non-path params survive.
            assert!(params.contains_key("region"), "{action} dropped region");
        }
    }

    #[test]
    fn hosted_policy_requires_http_on_every_navigation_flag() {
        for key in ["url", "start"] {
            let mut params = map(json!({ key: "file:///etc/passwd" }));
            assert!(
                enforce_hosted_browser_params("navigate", &mut params).is_err(),
                "{key} allowed a file scheme"
            );
            let mut ok = map(json!({ key: "https://example.com" }));
            assert!(enforce_hosted_browser_params("navigate", &mut ok).is_ok());
        }
    }

    #[test]
    fn hosted_policy_allows_ordinary_actions() {
        let mut params = map(json!({"ref": "@e2", "text": "hello"}));
        enforce_hosted_browser_params("type", &mut params).unwrap();
        assert_eq!(params.get("ref").and_then(Value::as_str), Some("@e2"));
        assert_eq!(params.get("text").and_then(Value::as_str), Some("hello"));
    }

    #[test]
    fn session_name_sanitizes_traversal_and_bounds_length() {
        assert_eq!(sanitize_session_name("main"), "main");
        assert_eq!(sanitize_session_name("work-1_A"), "work-1_A");
        assert_eq!(
            sanitize_session_name("../../etc/passwd"),
            "______etc_passwd"
        );
        assert_eq!(sanitize_session_name("a/b"), "a_b");
        assert_eq!(sanitize_session_name(""), "session");
        assert_eq!(sanitize_session_name(&"x".repeat(200)).len(), 64);
        // No path separators or parent refs survive.
        let slug = sanitize_session_name("..\\..\\win");
        assert!(!slug.contains('/') && !slug.contains('\\') && !slug.contains(".."));
    }
}
