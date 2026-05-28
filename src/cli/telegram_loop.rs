//! Telegram service loop: long-polling driver, conversation manager wiring,
//! per-update dispatch, actor-update fan-in, and heartbeat tick handling.
//!
//! Owns the `lethe telegram` subcommand surface area. Helpers that are also
//! useful elsewhere (`empty_marker`, `prompt_store`, `active_reminders_text`)
//! live in `main.rs` and are reached via `crate::`.
use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use clap::Subcommand;
use rand::Rng as _;
use serde_json::json;

use lethe::actor::ActorNamedEvent;
use lethe::agent::{Agent, AgentOptions, TurnRequest};
use lethe::config::Settings;
use lethe::conversation::transcription::{
    choose_transcription_provider, default_model_for_provider, transcribe_audio,
};
use lethe::conversation::{ConversationManager, ProcessCallback, ProcessContext};
use lethe::interfaces::telegram::{
    IncomingTelegramText, SharedTelegramTurnGuard, TelegramClient, TelegramToolContext,
    TelegramTurnGuard, TelegramTypingObserver, VisibleTelegramChannel, image_mime_type_from_path,
    is_emoji_only_reply, split_telegram_messages,
};
use lethe::memory::MessageRole;
use lethe::memory::message_metadata::{
    MessageKind, MessageVisibility, annotate_map, metadata_value as message_metadata_value,
};
use lethe::scheduler::heartbeat::{Heartbeat, HeartbeatAction, HeartbeatConfig};
use lethe::scheduler::proactive::ProactiveRateLimiter;
use lethe::tools::registry::ToolRuntime;

use super::handlers::{active_reminders_text, empty_marker, prompt_store};

const TELEGRAM_TYPING_REFRESH_SECONDS: u64 = 3;
const TELEGRAM_ACTOR_UPDATE_POLL_SECONDS: u64 = 1;
const TELEGRAM_ACTOR_UPDATE_QUERY_LIMIT: usize = 50;

#[derive(Debug, Subcommand)]
pub enum TelegramCommand {
    /// Split a response into Telegram message chunks without sending it.
    Split { text: String },
    /// Poll Telegram once and process any received text messages.
    PollOnce {
        #[arg(long, default_value_t = 30)]
        timeout: u64,
        #[arg(long)]
        no_recall: bool,
    },
    /// Run Telegram long polling until interrupted.
    Run {
        #[arg(long, default_value_t = 30)]
        timeout: u64,
        #[arg(long)]
        no_recall: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TelegramRuntimeCommand {
    Start,
    Help,
    Status,
    Stop,
    Heartbeat,
    Model(Option<String>),
    Aux(Option<String>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelegramModelSlot {
    Main,
    Aux,
}

#[derive(Debug)]
pub struct TelegramTurnInput {
    pub chat_id: i64,
    pub user_id: i64,
    pub message_id: i64,
    pub content: String,
    pub metadata: Option<serde_json::Value>,
    pub attachments: Vec<lethe::llm::LlmAttachment>,
}

pub struct TelegramPollContext<'a> {
    pub client: &'a TelegramClient,
    pub agent: &'a Agent,
    pub settings: &'a Settings,
    pub options: &'a AgentOptions,
    pub conversation_manager: Option<&'a ConversationManager>,
    pub process_callback: Option<ProcessCallback>,
}

pub async fn telegram_command(command: TelegramCommand) -> Result<()> {
    match command {
        TelegramCommand::Split { text } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&split_telegram_messages(&text))?
            );
            Ok(())
        }
        TelegramCommand::PollOnce { timeout, no_recall } => {
            let settings = Settings::from_env();
            let client = TelegramClient::new(
                settings.telegram.bot_token.clone(),
                settings.telegram.allowed_user_ids.clone(),
            )?;
            let agent = Agent::from_settings(settings.clone())?;
            let options = AgentOptions {
                use_hippocampus: !no_recall,
                ..Default::default()
            };
            let processed = process_telegram_once(
                TelegramPollContext {
                    client: &client,
                    agent: &agent,
                    settings: &settings,
                    options: &options,
                    conversation_manager: None,
                    process_callback: None,
                },
                None,
                timeout,
            )
            .await?;
            println!("processed_updates: {}", processed.1);
            Ok(())
        }
        TelegramCommand::Run { timeout, no_recall } => {
            let settings = Settings::from_env();
            if let Err(message) = settings.llm.ensure_ready() {
                anyhow::bail!(message);
            }
            if settings.telegram.bot_token.trim().is_empty() {
                anyhow::bail!(
                    "TELEGRAM_BOT_TOKEN is not set. Get one from @BotFather and \n\
                     re-run, or run `lethe init` for guided Telegram setup."
                );
            }
            let client = TelegramClient::new(
                settings.telegram.bot_token.clone(),
                settings.telegram.allowed_user_ids.clone(),
            )?;
            let agent = Agent::from_settings(settings.clone())?;
            let options = AgentOptions {
                use_hippocampus: !no_recall,
                ..Default::default()
            };
            let agent = Arc::new(agent);
            let conversation_manager = ConversationManager::new(
                std::time::Duration::from_secs_f64(settings.background.debounce_seconds.max(0.0)),
            );
            let process_callback = telegram_process_callback(
                client.clone(),
                agent.clone(),
                settings.clone(),
                options.clone(),
            );
            let mut offset = None;
            let mut target_chat_id = settings.telegram.allowed_user_ids.first().copied();
            let target_chat_id_state = Arc::new(AtomicI64::new(target_chat_id.unwrap_or(0)));
            let actor_update_monitor = spawn_telegram_actor_update_monitor(
                client.clone(),
                agent.clone(),
                settings.clone(),
                options.clone(),
                target_chat_id_state.clone(),
            );
            let mut heartbeat = Heartbeat::new(HeartbeatConfig::from_settings(&settings));
            let mut proactive_limiter = ProactiveRateLimiter::from_settings(&settings);
            let mut heartbeat_interval = tokio::time::interval(std::time::Duration::from_secs(
                heartbeat.config().interval_seconds.max(1),
            ));
            heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        println!("telegram_runner_stopped: interrupt");
                        break;
                    }
                    _ = heartbeat_interval.tick(), if heartbeat.config().enabled => {
                        if let Some(chat_id) = target_chat_id
                            && let Err(error) = process_heartbeat_once(
                                &client,
                                &agent,
                                &settings,
                                &mut heartbeat,
                                &mut proactive_limiter,
                                chat_id,
                                &options,
                            ).await {
                                tracing::warn!(error = %error, "heartbeat failed");
                                eprintln!("heartbeat_error: {error}");
                            }
                    }
                    result = process_telegram_once(
                        TelegramPollContext {
                            client: &client,
                            agent: &agent,
                            settings: &settings,
                            options: &options,
                            conversation_manager: Some(&conversation_manager),
                            process_callback: Some(process_callback.clone()),
                        },
                        offset,
                        timeout,
                    ) => {
                        let (next_offset, processed, last_chat_id) = match result {
                            Ok(value) => value,
                            Err(error) => {
                                tracing::error!(error = %error, "telegram polling failed");
                                return Err(error);
                            }
                        };
                        offset = next_offset;
                        if let Some(chat_id) = last_chat_id {
                            target_chat_id = Some(chat_id);
                            target_chat_id_state.store(chat_id, Ordering::SeqCst);
                            heartbeat.reset_idle_timer();
                        }
                        if processed > 0 {
                            println!("processed_updates: {processed}");
                        }
                    }
                }
            }
            if let Some(task) = actor_update_monitor {
                task.abort();
                let _ = task.await;
            }
            Ok(())
        }
    }
}

pub fn parse_telegram_runtime_command(text: &str) -> Option<TelegramRuntimeCommand> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let token = trimmed.split_whitespace().next()?;
    let command = token
        .trim_start_matches('/')
        .split('@')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let args = trimmed.get(token.len()..).unwrap_or_default().trim();
    let args = (!args.is_empty()).then(|| args.to_string());

    match command.as_str() {
        "start" => Some(TelegramRuntimeCommand::Start),
        "help" => Some(TelegramRuntimeCommand::Help),
        "status" => Some(TelegramRuntimeCommand::Status),
        "stop" => Some(TelegramRuntimeCommand::Stop),
        "heartbeat" => Some(TelegramRuntimeCommand::Heartbeat),
        "model" => Some(TelegramRuntimeCommand::Model(args)),
        "aux" => Some(TelegramRuntimeCommand::Aux(args)),
        _ => None,
    }
}

pub fn telegram_help_text(settings: &Settings) -> String {
    format!(
        "Hello. I'm {}.\n\nSend me any message and I'll help.\n\nCommands:\n/status - Check runtime status\n/stop - Cancel current processing when supported\n/heartbeat - Force a check-in\n/model [model-id] - Show or change the main model\n/aux [model-id] - Show or change the auxiliary model",
        settings.agent_name
    )
}

pub async fn telegram_status_text(agent: &Agent, settings: &Settings) -> Result<String> {
    let stats = agent.memory().stats()?;
    let router = agent.router_config()?;
    let actor_summary = if let Some(registry) = agent.actor_registry() {
        format!("enabled, active={}", registry.active_count().await)
    } else {
        "disabled".to_string()
    };

    Ok(format!(
        "Status: ready\nMemory: {} blocks, {} archival, {} messages, {} notes\nModel: {}\nAux model: {}\nHeartbeat: {}, interval={}s\nActors: {}",
        stats.memory_blocks,
        stats.archival_memories,
        stats.message_history,
        stats.notes,
        empty_marker(&router.model),
        empty_marker(&router.aux_model),
        if settings.background.heartbeat_enabled {
            "enabled"
        } else {
            "disabled"
        },
        settings.background.heartbeat_interval_seconds,
        actor_summary
    ))
}

pub async fn telegram_conversation_status_text(
    manager: Option<&ConversationManager>,
    chat_id: i64,
) -> String {
    let Some(manager) = manager else {
        return "Conversation: direct polling".to_string();
    };

    let pending = manager.pending_count(chat_id).await;
    let state = if manager.is_processing(chat_id).await {
        "processing"
    } else if manager.is_debouncing(chat_id).await {
        "debouncing"
    } else {
        "idle"
    };
    format!("Conversation: {state}, pending={pending}")
}

pub fn telegram_model_text(
    agent: &Agent,
    slot: TelegramModelSlot,
    requested_model: Option<&str>,
) -> Result<String> {
    let before = agent.router_config()?;
    let (label, current) = match slot {
        TelegramModelSlot::Main => ("Main model", before.model.as_str()),
        TelegramModelSlot::Aux => ("Aux model", before.aux_model.as_str()),
    };

    let Some(model) = requested_model
        .map(str::trim)
        .filter(|model| !model.is_empty())
    else {
        let command = match slot {
            TelegramModelSlot::Main => "/model",
            TelegramModelSlot::Aux => "/aux",
        };
        return Ok(format!(
            "{label}: {}\nUse `{command} <model-id>` to change it for this process.",
            empty_marker(current)
        ));
    };

    let changed = match slot {
        TelegramModelSlot::Main => agent.reconfigure_models(Some(model), None)?,
        TelegramModelSlot::Aux => agent.reconfigure_models(None, Some(model))?,
    };
    if changed
        .as_object()
        .is_some_and(|changes| changes.is_empty())
    {
        return Ok(format!("{label} unchanged: {}", empty_marker(model)));
    }

    let after = agent.router_config()?;
    let updated = match slot {
        TelegramModelSlot::Main => after.model.as_str(),
        TelegramModelSlot::Aux => after.aux_model.as_str(),
    };
    Ok(format!(
        "{label} updated: {} -> {}",
        empty_marker(current),
        empty_marker(updated)
    ))
}

fn telegram_text_metadata(
    incoming: &IncomingTelegramText,
) -> serde_json::Map<String, serde_json::Value> {
    let mut metadata = serde_json::Map::from_iter([
        ("source".to_string(), json!("telegram_text")),
        ("chat_id".to_string(), json!(incoming.chat_id)),
        ("user_id".to_string(), json!(incoming.user_id)),
        ("message_id".to_string(), json!(incoming.message_id)),
        ("update_id".to_string(), json!(incoming.update_id)),
    ]);
    annotate_map(
        &mut metadata,
        MessageVisibility::UserVisible,
        MessageKind::Chat,
        "telegram",
    );
    metadata
}

fn metadata_i64(metadata: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<i64> {
    metadata.get(key).and_then(serde_json::Value::as_i64)
}

fn metadata_value_from_map(
    metadata: &serde_json::Map<String, serde_json::Value>,
) -> Option<serde_json::Value> {
    (!metadata.is_empty()).then(|| serde_json::Value::Object(metadata.clone()))
}

fn metadata_map_from_value(
    metadata: Option<&serde_json::Value>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    match metadata {
        Some(serde_json::Value::Object(map)) => Some(map.clone()),
        Some(value) => Some(serde_json::Map::from_iter([(
            "metadata".to_string(),
            value.clone(),
        )])),
        None => None,
    }
}

fn telegram_process_callback(
    client: TelegramClient,
    agent: Arc<Agent>,
    settings: Settings,
    options: AgentOptions,
) -> ProcessCallback {
    Arc::new(move |context: ProcessContext| {
        let client = client.clone();
        let agent = agent.clone();
        let settings = settings.clone();
        let options = options.clone();
        Box::pin(async move {
            if context.interrupt.is_interrupted() {
                return Ok(());
            }
            tracing::info!(
                chat_id = context.chat_id,
                user_id = context.user_id,
                attachments = context.attachments.len(),
                message_chars = context.message.chars().count(),
                "telegram conversation turn started"
            );

            let guard = Arc::new(Mutex::new(TelegramTurnGuard::new()));
            let runtime = ToolRuntime {
                telegram: Some(TelegramToolContext {
                    token: settings.telegram.bot_token.clone(),
                    chat_id: context.chat_id,
                    last_message_id: metadata_i64(&context.metadata, "message_id"),
                    guard: Some(guard.clone()),
                    dry_run: false,
                }),
                observer: Some(Arc::new(TelegramTypingObserver::new(
                    settings.telegram.bot_token.clone(),
                    context.chat_id,
                ))),
                ..ToolRuntime::default()
            };
            let mut req = TurnRequest::new(&context.message)
                .with_attachments(context.attachments.clone())
                .with_runtime(runtime)
                .with_options(options.clone());
            if let Some(metadata) = metadata_value_from_map(&context.metadata) {
                req = req.with_metadata(metadata);
            }
            let response =
                with_telegram_typing(&client, context.chat_id, agent.chat_once(req)).await?;
            if !context.interrupt.is_interrupted() {
                tracing::info!(
                    chat_id = context.chat_id,
                    response_chars = response.chars().count(),
                    "telegram conversation turn completed"
                );
                send_guarded_telegram_final_response(&client, context.chat_id, &response, guard)
                    .await?;
            }
            Ok(())
        })
    })
}

async fn handle_telegram_runtime_command(
    client: &TelegramClient,
    agent: &Agent,
    settings: &Settings,
    incoming: &IncomingTelegramText,
    options: &AgentOptions,
    conversation_manager: Option<&ConversationManager>,
) -> Result<bool> {
    let Some(command) = parse_telegram_runtime_command(&incoming.text) else {
        return Ok(false);
    };

    match command {
        TelegramRuntimeCommand::Start | TelegramRuntimeCommand::Help => {
            client
                .send_message(incoming.chat_id, &telegram_help_text(settings))
                .await?;
        }
        TelegramRuntimeCommand::Status => {
            let mut status = telegram_status_text(agent, settings).await?;
            status.push('\n');
            status.push_str(
                &telegram_conversation_status_text(conversation_manager, incoming.chat_id).await,
            );
            client.send_message(incoming.chat_id, &status).await?;
        }
        TelegramRuntimeCommand::Stop => {
            let message = if let Some(manager) = conversation_manager {
                if manager.cancel(incoming.chat_id).await {
                    "Processing cancelled."
                } else {
                    "Nothing to cancel."
                }
            } else {
                "Nothing to cancel in this Rust polling mode."
            };
            client.send_message(incoming.chat_id, message).await?;
        }
        TelegramRuntimeCommand::Heartbeat => {
            client
                .send_message(incoming.chat_id, "Triggering heartbeat...")
                .await?;
            let mut heartbeat = Heartbeat::new(HeartbeatConfig::from_settings(settings));
            let mut proactive_limiter = ProactiveRateLimiter::from_settings(settings);
            if let Err(error) = process_heartbeat_once(
                client,
                agent,
                settings,
                &mut heartbeat,
                &mut proactive_limiter,
                incoming.chat_id,
                options,
            )
            .await
            {
                client
                    .send_message(incoming.chat_id, &format!("Heartbeat failed: {error}"))
                    .await?;
            }
        }
        TelegramRuntimeCommand::Model(model) => {
            let response = telegram_model_text(agent, TelegramModelSlot::Main, model.as_deref())?;
            client.send_message(incoming.chat_id, &response).await?;
        }
        TelegramRuntimeCommand::Aux(model) => {
            let response = telegram_model_text(agent, TelegramModelSlot::Aux, model.as_deref())?;
            client.send_message(incoming.chat_id, &response).await?;
        }
    }

    Ok(true)
}

async fn handle_telegram_turn(
    client: &TelegramClient,
    agent: &Agent,
    settings: &Settings,
    options: &AgentOptions,
    conversation_manager: Option<&ConversationManager>,
    process_callback: Option<ProcessCallback>,
    turn: TelegramTurnInput,
) -> Result<()> {
    tracing::info!(
        chat_id = turn.chat_id,
        user_id = turn.user_id,
        message_id = turn.message_id,
        attachments = turn.attachments.len(),
        message_chars = turn.content.chars().count(),
        "telegram turn started"
    );
    if let (Some(manager), Some(callback)) = (conversation_manager, process_callback) {
        manager
            .add_message_with_attachments(
                turn.chat_id,
                turn.user_id,
                turn.content,
                metadata_map_from_value(turn.metadata.as_ref()),
                turn.attachments,
                Some(callback),
            )
            .await;
        return Ok(());
    }

    let TelegramTurnInput {
        chat_id,
        user_id: _,
        message_id,
        content,
        metadata,
        attachments,
    } = turn;
    let guard = Arc::new(Mutex::new(TelegramTurnGuard::new()));
    let runtime = ToolRuntime {
        telegram: Some(TelegramToolContext {
            token: settings.telegram.bot_token.clone(),
            chat_id,
            last_message_id: Some(message_id),
            guard: Some(guard.clone()),
            dry_run: false,
        }),
        observer: Some(Arc::new(TelegramTypingObserver::new(
            settings.telegram.bot_token.clone(),
            chat_id,
        ))),
        ..ToolRuntime::default()
    };
    let mut req = TurnRequest::new(&content)
        .with_attachments(attachments)
        .with_runtime(runtime)
        .with_options(options.clone());
    if let Some(metadata) = metadata {
        req = req.with_metadata(metadata);
    }
    let response = with_telegram_typing(client, chat_id, agent.chat_once(req)).await?;
    tracing::info!(
        chat_id,
        response_chars = response.chars().count(),
        "telegram turn completed"
    );
    send_guarded_telegram_final_response(client, chat_id, &response, guard).await?;
    Ok(())
}

fn start_telegram_typing(client: TelegramClient, chat_id: i64) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match client.send_chat_action(chat_id, "typing").await {
                Ok(_) => {
                    tokio::time::sleep(std::time::Duration::from_secs(
                        TELEGRAM_TYPING_REFRESH_SECONDS,
                    ))
                    .await;
                }
                Err(error) => {
                    tracing::debug!(chat_id, error = %error, "telegram typing action failed");
                    break;
                }
            }
        }
    })
}

async fn with_telegram_typing<F, T>(client: &TelegramClient, chat_id: i64, future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let typing_task = start_telegram_typing(client.clone(), chat_id);
    let output = future.await;
    typing_task.abort();
    let _ = typing_task.await;
    output
}

fn spawn_telegram_actor_update_monitor(
    client: TelegramClient,
    agent: Arc<Agent>,
    settings: Settings,
    options: AgentOptions,
    target_chat_id: Arc<AtomicI64>,
) -> Option<tokio::task::JoinHandle<()>> {
    let actor_runtime = agent.actor_registry()?;
    let principal_actor_id = agent.principal_actor_id()?.to_string();
    Some(tokio::spawn(async move {
        let mut processed_event_ids = HashSet::<String>::new();
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(
            TELEGRAM_ACTOR_UPDATE_POLL_SECONDS,
        ));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let chat_id = target_chat_id.load(Ordering::SeqCst);
            if chat_id == 0 {
                continue;
            }
            let events = match actor_runtime
                .principal_task_update_events(
                    &principal_actor_id,
                    TELEGRAM_ACTOR_UPDATE_QUERY_LIMIT,
                )
                .await
            {
                Ok(events) => events,
                Err(error) => {
                    tracing::warn!(error = %error, "actor update query failed");
                    continue;
                }
            };
            let fresh_events = events
                .into_iter()
                .filter(|event| processed_event_ids.insert(event.event.id.clone()))
                .collect::<Vec<_>>();
            if fresh_events.is_empty() {
                continue;
            }
            if let Err(error) = process_telegram_actor_updates(
                &client,
                &agent,
                &settings,
                &options,
                chat_id,
                &fresh_events,
            )
            .await
            {
                tracing::warn!(error = %error, "actor update cortex turn failed");
            }
        }
    }))
}

async fn process_telegram_actor_updates(
    client: &TelegramClient,
    agent: &Agent,
    settings: &Settings,
    options: &AgentOptions,
    chat_id: i64,
    updates: &[ActorNamedEvent],
) -> Result<()> {
    let guard = Arc::new(Mutex::new(TelegramTurnGuard::new()));
    let runtime = ToolRuntime {
        telegram: Some(TelegramToolContext {
            token: settings.telegram.bot_token.clone(),
            chat_id,
            last_message_id: None,
            guard: Some(guard.clone()),
            dry_run: false,
        }),
        observer: Some(Arc::new(TelegramTypingObserver::new(
            settings.telegram.bot_token.clone(),
            chat_id,
        ))),
        ..ToolRuntime::default()
    };
    let synthetic_message = actor_update_synthetic_message(updates);
    let req = TurnRequest::new(&synthetic_message)
        .with_runtime(runtime)
        .with_options(options.clone())
        .with_metadata(message_metadata_value(
            MessageVisibility::Internal,
            MessageKind::ActorUpdate,
            "actor_update",
        ));
    let response = with_telegram_typing(client, chat_id, agent.chat_once(req)).await?;
    // Match Python heartbeat: the synthetic prompt explicitly instructs the
    // model to reply with literal "ok" when there's nothing to surface.
    // Anything else is forwarded. This is a sentinel contract, not pattern
    // matching arbitrary acknowledgments.
    let is_idle_ack = response.trim().eq_ignore_ascii_case("ok");
    if !is_idle_ack {
        send_guarded_telegram_final_response(client, chat_id, &response, guard).await?;
    }
    if !response.trim().is_empty() {
        agent.memory().messages.add(
            MessageRole::Assistant,
            &response,
            Some(json!({
                "source": "actor_update",
                "actor_event_ids": updates
                    .iter()
                    .map(|update| update.event.id.clone())
                    .collect::<Vec<_>>(),
            })),
        )?;
    }
    Ok(())
}

fn actor_update_synthetic_message(updates: &[ActorNamedEvent]) -> String {
    let terminal = updates
        .iter()
        .any(|update| actor_update_is_terminal(actor_update_kind(update)));
    let mut lines = vec![
        "[System: actor update]".to_string(),
        "One or more subagents sent task updates to your cortex inbox.".to_string(),
        "The authoritative details are in your actor inbox; review them before responding."
            .to_string(),
        String::new(),
        "<updates>".to_string(),
    ];
    for update in updates {
        let kind = actor_update_kind(update);
        lines.push(format!("- {} ({kind})", update.actor_name));
    }
    lines.push("</updates>".to_string());
    lines.push(String::new());
    if terminal {
        lines.push(
            "At least one subagent finished or failed. Send the user a concise update with the result, blocker, or next action. Respond with `ok` only if there's truly nothing worth telling them."
                .to_string(),
        );
    } else {
        lines.push(
            "These are progress updates. Respond with `ok` unless something here is worth surfacing to the user — in which case send a brief status."
                .to_string(),
        );
    }
    lines.join("\n")
}

fn actor_update_kind(update: &ActorNamedEvent) -> &str {
    update
        .event
        .payload
        .get("intent")
        .or_else(|| update.event.payload.get("kind"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("progress")
}

fn actor_update_is_terminal(kind: &str) -> bool {
    matches!(kind, "done" | "failed" | "error" | "max_turns")
}

async fn process_telegram_once(
    context: TelegramPollContext<'_>,
    offset: Option<i64>,
    timeout: u64,
) -> Result<(Option<i64>, usize, Option<i64>)> {
    let client = context.client;
    let agent = context.agent;
    let settings = context.settings;
    let options = context.options;
    let conversation_manager = context.conversation_manager;
    let process_callback = context.process_callback;
    let updates = client.get_updates(offset, timeout).await?;
    let mut next_offset = offset;
    let mut processed = 0;
    let mut last_chat_id = None;
    for update in updates {
        next_offset = Some(update.update_id + 1);

        if let Some(incoming) = update.incoming_text() {
            if !client.user_allowed(incoming.user_id) {
                continue;
            }
            last_chat_id = Some(incoming.chat_id);
            if handle_telegram_runtime_command(
                client,
                agent,
                settings,
                &incoming,
                options,
                conversation_manager,
            )
            .await?
            {
                processed += 1;
                continue;
            }
            let metadata = telegram_text_metadata(&incoming);
            handle_telegram_turn(
                client,
                agent,
                settings,
                options,
                conversation_manager,
                process_callback.clone(),
                TelegramTurnInput {
                    chat_id: incoming.chat_id,
                    user_id: incoming.user_id,
                    message_id: incoming.message_id,
                    content: incoming.text,
                    metadata: Some(serde_json::Value::Object(metadata)),
                    attachments: Vec::new(),
                },
            )
            .await?;
            processed += 1;
            continue;
        }

        if let Some(incoming) = update.incoming_photo() {
            if !client.user_allowed(incoming.user_id) {
                continue;
            }
            last_chat_id = Some(incoming.chat_id);
            let photo_result = async {
                let file = client.get_file(&incoming.file_id).await?;
                let image_bytes = client.download_file(&file.file_path).await?;
                let content_type = image_mime_type_from_path(&file.file_path).to_string();
                let attachment = lethe::llm::LlmAttachment {
                    content_type: content_type.clone(),
                    base64_content: BASE64_STANDARD.encode(&image_bytes),
                    name: Some(incoming.attachment_name(&content_type)),
                };
                Ok::<(String, lethe::llm::LlmAttachment), anyhow::Error>((content_type, attachment))
            }
            .await;

            let (content_type, attachment) = match photo_result {
                Ok(value) => value,
                Err(error) => {
                    client
                        .send_message(
                            incoming.chat_id,
                            &format!("Failed to process photo: {error}"),
                        )
                        .await?;
                    processed += 1;
                    continue;
                }
            };

            let content = incoming.content_text();
            let metadata = incoming.metadata(&content_type);
            handle_telegram_turn(
                client,
                agent,
                settings,
                options,
                conversation_manager,
                process_callback.clone(),
                TelegramTurnInput {
                    chat_id: incoming.chat_id,
                    user_id: incoming.user_id,
                    message_id: incoming.message_id,
                    content,
                    metadata: Some(metadata),
                    attachments: vec![attachment],
                },
            )
            .await?;
            processed += 1;
            continue;
        }

        if let Some(incoming) = update.incoming_audio() {
            if !client.user_allowed(incoming.user_id) {
                continue;
            }
            last_chat_id = Some(incoming.chat_id);
            if !settings.telegram.transcription_enabled {
                client
                    .send_message(incoming.chat_id, "Voice transcription is disabled.")
                    .await?;
                processed += 1;
                continue;
            }

            let transcript_result = async {
                let provider = choose_transcription_provider(settings)?;
                let model = if settings.transcription.model.trim().is_empty() {
                    default_model_for_provider(provider).to_string()
                } else {
                    settings.transcription.model.trim().to_string()
                };
                let file = client.get_file(&incoming.file_id).await?;
                let audio_bytes = client.download_file(&file.file_path).await?;
                let transcript = transcribe_audio(
                    &audio_bytes,
                    &incoming.file_name,
                    incoming.mime_type.as_deref(),
                    settings,
                )?;
                Ok::<(String, String, String), anyhow::Error>((
                    transcript,
                    provider.as_str().to_string(),
                    model,
                ))
            }
            .await;

            let (transcript, provider, model) = match transcript_result {
                Ok(value) => value,
                Err(error) => {
                    client
                        .send_message(
                            incoming.chat_id,
                            &format!("Failed to transcribe audio: {error}"),
                        )
                        .await?;
                    processed += 1;
                    continue;
                }
            };

            let content = incoming.content_with_transcript(&transcript);
            let metadata = incoming.metadata(&provider, &model);
            handle_telegram_turn(
                client,
                agent,
                settings,
                options,
                conversation_manager,
                process_callback.clone(),
                TelegramTurnInput {
                    chat_id: incoming.chat_id,
                    user_id: incoming.user_id,
                    message_id: incoming.message_id,
                    content,
                    metadata: Some(metadata),
                    attachments: Vec::new(),
                },
            )
            .await?;
            processed += 1;
            continue;
        }

        if let Some(incoming) = update.incoming_document() {
            if !client.user_allowed(incoming.user_id) {
                continue;
            }
            last_chat_id = Some(incoming.chat_id);
            let document_result = async {
                let file = client.get_file(&incoming.file_id).await?;
                let downloads_dir = settings.paths.workspace_dir.join("Downloads");
                std::fs::create_dir_all(&downloads_dir)?;
                let file_path = downloads_dir.join(&incoming.file_name);
                let bytes = client.download_file(&file.file_path).await?;
                std::fs::write(&file_path, bytes)?;
                Ok::<std::path::PathBuf, anyhow::Error>(file_path)
            }
            .await;

            let file_path = match document_result {
                Ok(path) => path,
                Err(error) => {
                    client
                        .send_message(
                            incoming.chat_id,
                            &format!("Failed to download file: {error}"),
                        )
                        .await?;
                    processed += 1;
                    continue;
                }
            };

            let content = incoming.content_with_path(&file_path);
            let metadata = incoming.metadata(&file_path);
            handle_telegram_turn(
                client,
                agent,
                settings,
                options,
                conversation_manager,
                process_callback.clone(),
                TelegramTurnInput {
                    chat_id: incoming.chat_id,
                    user_id: incoming.user_id,
                    message_id: incoming.message_id,
                    content,
                    metadata: Some(metadata),
                    attachments: Vec::new(),
                },
            )
            .await?;
            processed += 1;
            continue;
        }

        if let Some(incoming) = update.incoming_sticker() {
            if !client.user_allowed(incoming.user_id) {
                continue;
            }
            last_chat_id = Some(incoming.chat_id);
            let content = incoming.content();
            let metadata = incoming.metadata();
            handle_telegram_turn(
                client,
                agent,
                settings,
                options,
                conversation_manager,
                process_callback.clone(),
                TelegramTurnInput {
                    chat_id: incoming.chat_id,
                    user_id: incoming.user_id,
                    message_id: incoming.message_id,
                    content,
                    metadata: Some(metadata),
                    attachments: Vec::new(),
                },
            )
            .await?;
            processed += 1;
            continue;
        }

        if let Some(reaction) = update.incoming_reaction() {
            if !client.user_allowed(reaction.user_id) {
                continue;
            }
            last_chat_id = Some(reaction.chat_id);
            agent
                .memory()
                .messages
                .add(MessageRole::User, &reaction.content(), Some(reaction.metadata()))?;
            processed += 1;
        }
    }
    Ok((next_offset, processed, last_chat_id))
}

async fn send_guarded_telegram_final_response(
    client: &TelegramClient,
    chat_id: i64,
    response: &str,
    guard: SharedTelegramTurnGuard,
) -> Result<()> {
    let (pending_reactions, channel) = {
        let mut guard = guard
            .lock()
            .map_err(|error| anyhow!("telegram turn guard poisoned: {error}"))?;
        (
            guard.drain_pending_reactions(),
            guard.choose_visible_channel(),
        )
    };

    if is_emoji_only_reply(response) && !pending_reactions.is_empty() {
        if channel == VisibleTelegramChannel::Reaction {
            let pending = &pending_reactions[0];
            if client
                .set_message_reaction(pending.chat_id, pending.message_id, &pending.emoji)
                .await
                .unwrap_or(false)
            {
                return Ok(());
            }
        }
        send_telegram_messages_with_delays(client, chat_id, split_telegram_messages(response))
            .await?;
        return Ok(());
    }

    for pending in pending_reactions {
        let _ = client
            .set_message_reaction(pending.chat_id, pending.message_id, &pending.emoji)
            .await
            .unwrap_or(false);
    }

    if !response.trim().is_empty() {
        send_telegram_messages_with_delays(client, chat_id, split_telegram_messages(response))
            .await?;
    }
    Ok(())
}

async fn send_telegram_messages_with_delays(
    client: &TelegramClient,
    chat_id: i64,
    chunks: Vec<String>,
) -> Result<()> {
    let total = chunks.len();
    for (index, chunk) in chunks.into_iter().enumerate() {
        client.send_message(chat_id, &chunk).await?;
        if index + 1 < total {
            let delay = telegram_inter_message_delay(&chunk);
            let _ = client.send_chat_action(chat_id, "typing").await;
            tokio::time::sleep(delay).await;
        }
    }
    Ok(())
}

fn telegram_inter_message_delay(chunk: &str) -> std::time::Duration {
    let chars = chunk.chars().count() as f64;
    let mut rng = rand::rng();
    let think = rng.random_range(0.35..=1.0);
    let typing = chars * 0.012;
    let jitter = rng.random_range(0.75..=1.15);
    let seconds = ((think + typing).min(4.0) * jitter).clamp(0.25, 4.6);
    std::time::Duration::from_secs_f64(seconds)
}

async fn process_heartbeat_once(
    client: &TelegramClient,
    agent: &Agent,
    settings: &Settings,
    heartbeat: &mut Heartbeat,
    proactive_limiter: &mut ProactiveRateLimiter,
    chat_id: i64,
    options: &AgentOptions,
) -> Result<()> {
    let prompts = prompt_store(settings);
    let reminders = active_reminders_text(settings)?;
    let prompt = heartbeat.trigger(&prompts, &reminders);
    // Idle gate: no due reminders, not the first tick, and not a periodic
    // deeper review — the model would respond "idle" anyway, so don't
    // spend two LLM turns (cortex + DMN) to learn that. First-tick,
    // full-context, and reminder-bearing ticks always proceed.
    if !prompt.first_tick && !prompt.use_full_context && reminders.trim().is_empty() {
        let outcome =
            heartbeat.finish_response(r#"{"action":"idle","message":""}"#, None);
        tracing::debug!(
            count = heartbeat.heartbeat_count(),
            idle_minutes = ?outcome.idle_minutes,
            "heartbeat skipped: nothing to do"
        );
        return Ok(());
    }
    let req = TurnRequest::new(&prompt.message)
        .with_metadata(message_metadata_value(
            MessageVisibility::Internal,
            MessageKind::Heartbeat,
            "heartbeat",
        ))
        .with_options(options.clone());
    let response = agent.chat_once(req).await?;
    let outcome = heartbeat.finish_response(&response, None);
    let _background = agent
        .process_background_heartbeat_quiet(&prompt.message, &reminders)
        .await?;
    let mut messages = Vec::new();
    if outcome.action == HeartbeatAction::Send {
        messages.push(outcome.message);
    }
    for message in messages {
        if !proactive_limiter.allowed() {
            break;
        }
        send_telegram_messages_with_delays(client, chat_id, split_telegram_messages(&message))
            .await?;
        proactive_limiter.record();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_settings(root: &std::path::Path) -> Settings {
        let mut settings = lethe::config::test_settings(root);
        settings.llm.llm_model = "openai/gpt-5".to_string();
        settings.llm.llm_model_aux = "openai/gpt-5-mini".to_string();
        settings
    }

    #[test]
    fn telegram_runtime_command_parser_accepts_known_commands() {
        assert_eq!(
            parse_telegram_runtime_command("/status@LetheBot"),
            Some(TelegramRuntimeCommand::Status)
        );
        assert_eq!(
            parse_telegram_runtime_command("/model openai/gpt-5.1"),
            Some(TelegramRuntimeCommand::Model(Some(
                "openai/gpt-5.1".to_string()
            )))
        );
        assert_eq!(
            parse_telegram_runtime_command("/aux   anthropic/claude-sonnet-4.5"),
            Some(TelegramRuntimeCommand::Aux(Some(
                "anthropic/claude-sonnet-4.5".to_string()
            )))
        );
        assert_eq!(parse_telegram_runtime_command("hello"), None);
        assert_eq!(parse_telegram_runtime_command("/unknown"), None);
    }

    #[test]
    fn telegram_model_text_shows_and_updates_process_models() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = test_settings(tmp.path());
        let agent = Agent::from_settings(settings).unwrap();

        let current = telegram_model_text(&agent, TelegramModelSlot::Main, None).unwrap();
        assert!(current.contains("openai/gpt-5"));

        let updated =
            telegram_model_text(&agent, TelegramModelSlot::Main, Some("openrouter/kimi-k2"))
                .unwrap();
        assert!(updated.contains("openai/gpt-5 -> openrouter/kimi-k2"));
        assert_eq!(agent.router_config().unwrap().model, "openrouter/kimi-k2");

        let updated_aux =
            telegram_model_text(&agent, TelegramModelSlot::Aux, Some("google/gemini-flash"))
                .unwrap();
        assert!(updated_aux.contains("openai/gpt-5-mini -> google/gemini-flash"));
        assert_eq!(
            agent.router_config().unwrap().aux_model,
            "google/gemini-flash"
        );
    }

    #[tokio::test]
    async fn telegram_status_text_includes_runtime_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = test_settings(tmp.path());
        let agent = Agent::from_settings(settings.clone()).unwrap();

        let status = telegram_status_text(&agent, &settings).await.unwrap();

        assert!(status.contains("Status: ready"));
        assert!(status.contains("Memory:"));
        assert!(status.contains("Model: openai/gpt-5"));
        assert!(status.contains("Heartbeat: enabled"));
        assert!(status.contains("Actors: enabled"));
    }

    #[test]
    fn telegram_text_metadata_preserves_transport_ids() {
        let incoming = IncomingTelegramText {
            update_id: 10,
            chat_id: 20,
            user_id: 30,
            message_id: 40,
            text: "hello".to_string(),
        };

        let metadata = telegram_text_metadata(&incoming);

        assert_eq!(metadata.get("source"), Some(&json!("telegram_text")));
        assert_eq!(metadata_i64(&metadata, "chat_id"), Some(20));
        assert_eq!(metadata_i64(&metadata, "user_id"), Some(30));
        assert_eq!(metadata_i64(&metadata, "message_id"), Some(40));
        assert_eq!(metadata_i64(&metadata, "update_id"), Some(10));
    }

    #[tokio::test]
    async fn telegram_conversation_status_reports_direct_and_managed_modes() {
        assert_eq!(
            telegram_conversation_status_text(None, 1).await,
            "Conversation: direct polling"
        );

        let manager = ConversationManager::new(std::time::Duration::from_millis(5));
        manager.add_message(1, 2, "queued", None, None).await;

        assert_eq!(
            telegram_conversation_status_text(Some(&manager), 1).await,
            "Conversation: idle, pending=1"
        );
    }
}
