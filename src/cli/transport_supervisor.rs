//! Runtime transport supervisor.
//!
//! Lets a control plane connect/disconnect a Telegram bot **without restarting
//! the process**. It polls a desired-config file (`config/transports.json`,
//! written by the control plane) and (re)starts or stops the Telegram poll loop
//! to match, writing runtime state (the locked-in owner) back to
//! `config/transports-state.json`.
//!
//! Both files live in the config dir (next to `.env`), never the workspace, so
//! the bot token and owner binding stay out of `lethe backup` archives and out
//! of the agent's view. When no desired-config file is present, the supervisor
//! falls back to the static `TELEGRAM_*` env/.env settings (desktop installs),
//! so this is a strict superset of the previous startup behaviour.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use lethe::agent::{Agent, AgentOptions};
use lethe::config::Settings;
use lethe::interfaces::telegram::FirstUserLockCallback;
use lethe::scheduler::brainstem::BrainstemHandle;

const DESIRED_FILE: &str = "transports.json";
const STATE_FILE: &str = "transports-state.json";
const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// Desired transport config, owned by the control plane.
#[derive(Debug, Default, Serialize, Deserialize)]
struct DesiredTransports {
    #[serde(default)]
    telegram: Option<DesiredTelegram>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DesiredTelegram {
    #[serde(default)]
    bot_token: String,
    #[serde(default)]
    enabled: bool,
    /// When true and no user is locked yet, bind to the first user who messages.
    #[serde(default)]
    lock_to_first_user: bool,
}

/// Runtime state, owned by lethe (this process). Read by the control plane.
#[derive(Debug, Default, Serialize, Deserialize)]
struct RuntimeState {
    #[serde(default)]
    telegram: Option<TelegramRuntime>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TelegramRuntime {
    #[serde(default)]
    locked_user_id: Option<i64>,
}

struct Desired {
    token: String,
    lock_to_first_user: bool,
}

fn config_dir(settings: &Settings) -> PathBuf {
    settings
        .paths
        .config_file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("config"))
}

fn load_desired(dir: &Path) -> DesiredTransports {
    std::fs::read_to_string(dir.join(DESIRED_FILE))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn load_state(dir: &Path) -> RuntimeState {
    std::fs::read_to_string(dir.join(STATE_FILE))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_state(dir: &Path, state: &RuntimeState) {
    let _ = std::fs::create_dir_all(dir);
    if let Ok(raw) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(dir.join(STATE_FILE), raw);
    }
}

/// Resolve the Telegram config to apply: the control-plane file is authoritative
/// when present; otherwise fall back to static env/.env settings.
fn resolve_desired(dir: &Path, settings: &Settings) -> Option<Desired> {
    if dir.join(DESIRED_FILE).exists() {
        return match load_desired(dir).telegram {
            Some(tg) if tg.enabled && !tg.bot_token.trim().is_empty() => Some(Desired {
                token: tg.bot_token.trim().to_string(),
                lock_to_first_user: tg.lock_to_first_user,
            }),
            _ => None,
        };
    }
    if settings.telegram.enabled && !settings.telegram.bot_token.trim().is_empty() {
        Some(Desired {
            token: settings.telegram.bot_token.trim().to_string(),
            lock_to_first_user: false,
        })
    } else {
        None
    }
}

fn spawn_telegram(
    agent: Arc<Agent>,
    mut settings: Settings,
    brainstem: BrainstemHandle,
    dir: PathBuf,
    token: String,
    lock_to_first_user: bool,
) -> JoinHandle<()> {
    let locked = load_state(&dir).telegram.and_then(|t| t.locked_user_id);

    settings.telegram.bot_token = token;
    settings.telegram.enabled = true;
    settings.telegram.allowed_user_ids = locked.map(|id| vec![id]).unwrap_or_default();

    // Lock to the first user only when asked and not already bound. The callback
    // persists the binding so a later restart reuses the same owner.
    let lock_on_first: Option<FirstUserLockCallback> = if lock_to_first_user && locked.is_none() {
        let dir = dir.clone();
        Some(Arc::new(move |uid: i64| {
            let mut state = load_state(&dir);
            state
                .telegram
                .get_or_insert_with(Default::default)
                .locked_user_id = Some(uid);
            save_state(&dir, &state);
            tracing::info!(user_id = uid, "telegram transport locked to first user");
        }))
    } else {
        None
    };

    tokio::spawn(async move {
        let options = AgentOptions::default();
        if let Err(error) = crate::cli::telegram_loop::run_telegram_with_agent(
            agent,
            settings,
            options,
            30,
            &brainstem,
            lock_on_first,
        )
        .await
        {
            tracing::warn!(error = %error, "telegram transport loop exited");
        }
    })
}

/// Long-running supervisor: reconciles the running Telegram transport to the
/// desired config, polling for changes. Spawned once by `api_command`.
pub async fn run(agent: Arc<Agent>, settings: Settings, brainstem: BrainstemHandle) {
    let dir = config_dir(&settings);
    // (token currently running, its task handle)
    let mut running: Option<(String, JoinHandle<()>)> = None;

    loop {
        // If the transport task died on its own (e.g. a fatal poll error), drop
        // it so the next tick restarts it.
        if running.as_ref().is_some_and(|(_, task)| task.is_finished()) {
            running = None;
        }

        match resolve_desired(&dir, &settings) {
            Some(desired) => {
                let same = running
                    .as_ref()
                    .is_some_and(|(token, _)| *token == desired.token);
                if !same {
                    if let Some((_, task)) = running.take() {
                        task.abort();
                    }
                    let task = spawn_telegram(
                        agent.clone(),
                        settings.clone(),
                        brainstem.clone(),
                        dir.clone(),
                        desired.token.clone(),
                        desired.lock_to_first_user,
                    );
                    running = Some((desired.token, task));
                }
            }
            None => {
                if let Some((_, task)) = running.take() {
                    task.abort();
                }
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
