//! Implementations for the bulk of the `lethe` CLI subcommands: `check`,
//! `fs`, `sh`, `web`, `transcribe`, `todo`, `memory`, `archive`, `note`,
//! `messages`, `agent`, `heartbeat`, `chat`, plus the small helpers they share.
//!
//! Kept separate from `main.rs` (which owns Clap parsing) so each handler can
//! evolve without ballooning the binary's root.

use anyhow::{Result, anyhow};
use serde_json::json;

use lethe::agent::{Agent, AgentOptions, TurnRequest};
use lethe::config::Settings;
use lethe::conversation::transcription::{
    choose_transcription_provider, infer_audio_format, transcribe_audio,
};
use lethe::llm::prompts::{PromptSource, PromptStore};
use lethe::llm::{LlmMessage, LlmRouter, LlmRouterConfig, llm_auth_mode_for_settings};
use lethe::memory::BlockManager;
use lethe::memory::MemoryStore;
use lethe::memory::archival::ArchivalMemory;
use lethe::memory::message_metadata::{
    MessageKind, MessageVisibility, metadata_value as message_metadata_value,
};
use lethe::memory::messages::{MessageHistory, MessageRole};
use lethe::memory::notes::NoteStore;
use lethe::memory::recall::{Hippocampus, HippocampusConfig};
use lethe::scheduler::curator::MemoryCurator;
use lethe::scheduler::heartbeat::{Heartbeat, HeartbeatConfig, render_summary_prompt};
use lethe::scheduler::proactive::{ActiveReminder, format_active_reminders};
use lethe::todos::{NewTodo, TodoFilter, TodoManager, TodoPriority, TodoStatus, TodoUpdate};
use lethe::tools::filesystem::FileTools;
use lethe::tools::shell::ShellTools;
use lethe::tools::web::WebTools;

use crate::{
    AgentCommand, ArchiveCommand, FsCommand, HeartbeatCommand, MemoryCommand, MessageCommand,
    NoteCommand, ShCommand, TodoCommand, WebCommand,
};

pub(crate) async fn api_command(port: Option<u16>) -> Result<()> {
    let settings = Settings::from_env();
    if let Err(message) = settings.llm.ensure_ready() {
        anyhow::bail!(message);
    }
    let port = port.unwrap_or(settings.api.port);

    // One Agent shared by the HTTP API and (optionally) the Telegram
    // poller. Sharing keeps memory + actor registry singular so both
    // transports operate on the same conversation state.
    let agent = std::sync::Arc::new(Agent::from_settings(settings.clone())?);

    // Brainstem is the single source of periodic beats and proactive
    // emissions. Both transports subscribe via its handle — no
    // double-firing, no divergent rate-limiter state, no transport-level
    // brain logic. See scheduler/brainstem.rs.
    let brainstem = lethe::scheduler::brainstem::BrainstemHandle::new();
    let brainstem_task = tokio::spawn(lethe::scheduler::brainstem::run(
        agent.clone(),
        settings.clone(),
        AgentOptions::default(),
        brainstem.clone(),
    ));

    // Transport supervisor: starts/stops chat transports (currently Telegram) to
    // match config — including config written at runtime by a control plane (see
    // transport_supervisor) — so a bot can be connected without restarting. Falls
    // back to the static `TELEGRAM_*` settings when no runtime config is present.
    let transport_task = tokio::spawn(crate::cli::transport_supervisor::run(
        agent.clone(),
        settings.clone(),
        brainstem.clone(),
    ));

    let api_result = if settings.api.enabled {
        lethe::interfaces::api::serve_with_agent(settings, port, Some(agent), brainstem).await
    } else {
        // API transport disabled — no HTTP/SSE (and no TUI). Keep the
        // background brainstem and any chat transports running until Ctrl-C.
        tracing::info!("http api disabled (API_ENABLED=false); running background + chat only");
        let _ = tokio::signal::ctrl_c().await;
        Ok(())
    };

    transport_task.abort();
    let _ = transport_task.await;
    brainstem_task.abort();
    let _ = brainstem_task.await;
    api_result
}

pub(crate) async fn tui_command(url: Option<String>, token: Option<String>) -> Result<()> {
    let settings = Settings::from_env();
    let base_url =
        url.unwrap_or_else(|| format!("http://{}:{}", settings.api.host, settings.api.port));
    // Order: explicit --token > non-empty LETHE_API_TOKEN env > value
    // from ~/.lethe/config/.env. Treat an empty-string env var as
    // unset so a stale `export LETHE_API_TOKEN=` in the user's shell
    // doesn't shadow the value the service actually started with.
    let token = token
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            let configured = settings.api.token.trim().to_string();
            (!configured.is_empty()).then_some(configured)
        })
        .ok_or_else(|| {
            anyhow!("LETHE_API_TOKEN is required. Set it in ~/.lethe/config/.env or pass --token.")
        })?;
    lethe::tui::run(lethe::tui::app::TuiOptions {
        base_url,
        token,
        workspace: settings.paths.workspace_dir.clone(),
    })
    .await
}

pub(crate) fn prompt_store(settings: &Settings) -> PromptStore {
    PromptStore::new(&settings.paths.workspace_dir, &settings.paths.config_dir)
}

pub(crate) async fn check() -> Result<()> {
    let settings = Settings::from_env();
    let store = prompt_store(&settings);
    let prompt = store.load("agent_instructions", "");

    println!("Lethe Rust runtime {}", env!("CARGO_PKG_VERSION"));
    println!("  mode: {:?}", settings.mode);
    println!("  home: {}", settings.paths.lethe_home.display());
    println!("  workspace: {}", settings.paths.workspace_dir.display());
    println!("  config_file: {}", settings.paths.config_file.display());
    println!("  config_dir: {}", settings.paths.config_dir.display());
    println!("  llm_model: {}", empty_marker(&settings.llm.llm_model));
    println!(
        "  llm_model_aux: {}",
        empty_marker(settings.effective_aux_model())
    );
    println!("  llm_auth: {}", llm_auth_mode_for_settings(&settings));
    println!(
        "  agent_instructions_source: {}",
        source_label(&prompt.source)
    );
    println!(
        "  single_binary_prompt_fallback: {}",
        matches!(prompt.source, PromptSource::Embedded)
    );
    println!();
    println!("Live subsystem checks:");

    // -- Config gate --------------------------------------------------------
    match settings.llm.ensure_ready() {
        Ok(()) => println!("  [OK]   llm config — model + auth key present"),
        Err(message) => {
            println!("  [FAIL] llm config:");
            for line in message.lines() {
                println!("         {line}");
            }
            println!();
            println!("Run `lethe init` for a guided setup.");
            return Ok(());
        }
    }

    // -- Memory store -------------------------------------------------------
    let memory = match lethe::memory::MemoryStore::from_settings(&settings) {
        Ok(memory) => {
            println!("  [OK]   memory store — workspace + sqlite open");
            memory
        }
        Err(error) => {
            println!("  [FAIL] memory store: {error}");
            return Ok(());
        }
    };

    // -- Embedding pipeline -------------------------------------------------
    // First call triggers the ONNX runtime + model download on a fresh box.
    // Honour the dotenv-controlled disable knob (some deployments skip this).
    if std::env::var("LETHE_SEMANTIC_SEARCH_ENABLED")
        .map(|value| value.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
    {
        println!("  [SKIP] embeddings — disabled via LETHE_SEMANTIC_SEARCH_ENABLED=false");
    } else {
        let embedder = memory.archival.embedder().clone();
        match embedder.embed_query("lethe check probe") {
            Ok(vector) if !vector.is_empty() => {
                println!("  [OK]   embeddings — produced {}-dim vector", vector.len())
            }
            Ok(_) => println!("  [FAIL] embeddings — returned empty vector"),
            Err(error) => {
                println!("  [FAIL] embeddings: {error}");
                println!("         (first run downloads ~150MB; check network access)");
            }
        }
    }

    // -- LLM ping (main model round-trip) -----------------------------------
    let router = LlmRouter::new(LlmRouterConfig::from_settings(&settings));
    let probe = vec![
        LlmMessage::system("Reply with the single word: ok"),
        LlmMessage::user("ready?"),
    ];
    match router.complete(probe.clone(), false).await {
        Ok(reply) => {
            let preview = reply.trim().lines().next().unwrap_or("").to_string();
            println!(
                "  [OK]   llm (main) — `{}` ({})",
                preview, settings.llm.llm_model
            );
        }
        Err(error) => {
            println!("  [FAIL] llm (main): {error}");
            println!(
                "         (check that the key is valid and the model id `{}` exists)",
                settings.llm.llm_model
            );
        }
    }

    // -- LLM ping (aux model round-trip, only if distinct from main) ----------
    if settings.effective_aux_model() != settings.llm.llm_model {
        match router.complete(probe, true).await {
            Ok(reply) => {
                let preview = reply.trim().lines().next().unwrap_or("").to_string();
                println!(
                    "  [OK]   llm (aux)  — `{}` ({})",
                    preview,
                    settings.effective_aux_model()
                );
            }
            Err(error) => {
                println!("  [FAIL] llm (aux): {error}");
                println!(
                    "         (check that the key is valid and the model id `{}` exists)",
                    settings.effective_aux_model()
                );
            }
        }
    }

    Ok(())
}

pub(crate) fn print_prompt(name: &str) -> Result<()> {
    let settings = Settings::from_env();
    let prompt = prompt_store(&settings).load(name, "");
    println!("{}", prompt.text);
    Ok(())
}

pub(crate) fn init_memory() -> Result<()> {
    let settings = Settings::from_env();
    let blocks_dir = settings.paths.workspace_dir.join("memory");
    let manager = BlockManager::new(&blocks_dir)?;
    manager.init_embedded_defaults()?;
    let block_count = manager.list_blocks(true)?.len();
    println!("seeded_core_memory_blocks: {block_count}");
    println!("blocks_dir: {}", blocks_dir.display());
    Ok(())
}

pub(crate) fn fs_command(command: FsCommand) -> Result<()> {
    let settings = Settings::from_env();
    let tools = FileTools::new(settings.paths.workspace_dir);
    let output = match command {
        FsCommand::Read {
            file_path,
            offset,
            limit,
        } => tools.read_file(&file_path, offset, limit),
        FsCommand::Write { file_path, content } => tools.write_file(&file_path, &content),
        FsCommand::Edit {
            file_path,
            old_string,
            new_string,
            replace_all,
        } => tools.edit_file(&file_path, &old_string, &new_string, replace_all),
        FsCommand::List { path, show_hidden } => tools.list_directory(&path, show_hidden),
        FsCommand::Glob { pattern, path } => tools.glob_search(&pattern, &path),
        FsCommand::Grep {
            pattern,
            path,
            file_pattern,
        } => tools.grep_search(&pattern, &path, &file_pattern),
    };
    println!("{output}");
    Ok(())
}

pub(crate) fn sh_command(command: ShCommand) -> Result<()> {
    let shell = ShellTools::from_env();
    let output = match command {
        ShCommand::Run {
            command,
            timeout,
            background,
            pty,
        } => shell.bash(&command, timeout, background, pty),
        ShCommand::Env => shell.get_environment_info(),
        ShCommand::Which { command_name } => shell.check_command_exists(&command_name),
    };
    println!("{output}");
    Ok(())
}

pub(crate) async fn web_command(command: WebCommand) -> Result<()> {
    let settings = Settings::from_env();
    let tools = WebTools::new(settings.paths.cache_dir);
    let output = match command {
        WebCommand::Available => WebTools::is_available().to_string(),
        WebCommand::Search {
            query,
            num_results,
            include_text,
            category,
        } => {
            tools
                .web_search(&query, num_results, include_text, &category)
                .await
        }
        WebCommand::Fetch { url, max_chars } => tools.fetch_webpage(&url, max_chars).await,
    };
    println!("{output}");
    Ok(())
}

pub(crate) fn transcribe_command(file_path: &str, mime_type: Option<&str>) -> Result<()> {
    let settings = Settings::from_env();
    let audio = std::fs::read(file_path)?;
    let audio_format = infer_audio_format(file_path, mime_type);
    let provider = choose_transcription_provider(&settings)?;
    let text = transcribe_audio(&audio, file_path, mime_type, &settings)?;
    println!("provider: {}", provider.as_str());
    println!("format: {audio_format}");
    println!();
    println!("{text}");
    Ok(())
}

pub(crate) fn todo_command(command: TodoCommand) -> Result<()> {
    let settings = Settings::from_env();
    let memory = MemoryStore::from_settings(&settings)?;
    let manager = &memory.todos;
    let output = match command {
        TodoCommand::Create {
            title,
            description,
            priority,
            due_date,
            tags,
            source,
        } => {
            let todo = NewTodo {
                title: title.clone(),
                description,
                priority: parse_priority(&priority)?,
                due_date,
                tags,
                source,
                parent_id: None,
            };
            let todo_id = manager.create(todo)?;
            format!("Created todo #{todo_id}: {title}")
        }
        TodoCommand::List {
            status,
            priority,
            include_completed,
            limit,
        } => {
            let todos = manager.list(TodoFilter {
                status: parse_optional_status(status.as_deref())?,
                priority: parse_optional_priority(priority.as_deref())?,
                include_completed,
                limit,
            })?;
            TodoManager::format_list(&todos)
        }
        TodoCommand::Update {
            todo_id,
            title,
            description,
            status,
            priority,
            due_date,
        } => {
            let updated = manager.update(
                todo_id,
                TodoUpdate {
                    title,
                    description,
                    status: parse_optional_status(status.as_deref())?,
                    priority: parse_optional_priority(priority.as_deref())?,
                    due_date,
                    parent_id: None,
                },
            )?;
            if updated {
                format!("Updated todo #{todo_id}")
            } else {
                format!("Todo #{todo_id} not found")
            }
        }
        TodoCommand::Complete { todo_id } => {
            if manager.complete(todo_id)? {
                format!("Completed todo #{todo_id}")
            } else {
                format!("Todo #{todo_id} not found")
            }
        }
        TodoCommand::Search { query, limit } => {
            let todos = manager.search(&query, limit)?;
            TodoManager::format_search(&query, &todos)
        }
        TodoCommand::RemindCheck => {
            let todos = manager.due_reminders()?;
            TodoManager::format_due_reminders(&todos)
        }
        TodoCommand::Reminded { todo_id } => {
            if manager.mark_reminded(todo_id)? {
                format!("Marked todo #{todo_id} as reminded")
            } else {
                format!("Todo #{todo_id} not found")
            }
        }
        TodoCommand::Delete { todo_id } => {
            if manager.delete(todo_id)? {
                format!("Deleted todo #{todo_id}")
            } else {
                format!("Todo #{todo_id} not found")
            }
        }
    };
    println!("{output}");
    Ok(())
}

pub(crate) async fn agent_command(command: AgentCommand) -> Result<()> {
    let settings = Settings::from_env();
    if let Err(message) = settings.llm.ensure_ready() {
        anyhow::bail!(message);
    }
    let agent = Agent::from_settings(settings)?;
    let options = AgentOptions {
        use_hippocampus: !matches!(
            &command,
            AgentCommand::Chat {
                no_recall: true,
                ..
            } | AgentCommand::Prepare {
                no_recall: true,
                ..
            }
        ),
        ..Default::default()
    };
    match command {
        AgentCommand::Chat { message, .. } => {
            let response = agent
                .chat_once(TurnRequest::new(message).with_options(options))
                .await?;
            println!("{response}");
        }
        AgentCommand::Prepare { message, .. } => {
            let turn = agent
                .prepare_turn(&TurnRequest::new(message).with_options(options))
                .await?;
            println!("{}", serde_json::to_string_pretty(&turn)?);
        }
    }
    Ok(())
}

pub(crate) async fn heartbeat_command(command: HeartbeatCommand) -> Result<()> {
    let settings = Settings::from_env();
    let prompts = prompt_store(&settings);
    let reminders = active_reminders_text(&settings)?;
    match command {
        HeartbeatCommand::Prompt { minimal } => {
            let mut heartbeat = Heartbeat::new(heartbeat_config(&settings, minimal));
            // CLI inspection runs without an actor runtime — open work here
            // is the todo side only.
            let open_work = open_work_text(&settings)?;
            let prompt = heartbeat.trigger(&prompts, &reminders, &open_work);
            println!("{}", prompt.message);
        }
        HeartbeatCommand::Trigger {
            minimal,
            summarize,
            no_recall,
        } => {
            let mut heartbeat = Heartbeat::new(heartbeat_config(&settings, minimal));
            let agent = Agent::from_settings(settings.clone())?;
            let open_work = agent.open_work_digest().await;
            let prompt = heartbeat.trigger(&prompts, &reminders, &open_work);
            let req = TurnRequest::new(&prompt.message)
                .with_metadata(message_metadata_value(
                    MessageVisibility::Internal,
                    MessageKind::Heartbeat,
                    "heartbeat",
                ))
                .with_options(AgentOptions {
                    use_hippocampus: !no_recall,
                    ..Default::default()
                });
            let response = agent.chat_once(req).await?;
            let evaluated = if summarize && !response.trim().is_empty() {
                let router = LlmRouter::new(LlmRouterConfig::from_settings(&settings));
                Some(
                    router
                        .complete(
                            vec![LlmMessage::user(render_summary_prompt(&prompts, &response))],
                            true,
                        )
                        .await?,
                )
            } else {
                None
            };
            let outcome = heartbeat.finish_response(&response, evaluated.as_deref());
            let background = agent
                .process_background_heartbeat(&prompt.message, &reminders)
                .await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "prompt": {
                        "use_full_context": prompt.use_full_context,
                        "first_tick": prompt.first_tick,
                    },
                    "raw_response": response,
                    "evaluated": evaluated,
                    "outcome": outcome,
                    "background": background,
                }))?
            );
        }
    }
    Ok(())
}

fn heartbeat_config(settings: &Settings, minimal: bool) -> HeartbeatConfig {
    let mut config = HeartbeatConfig::from_settings(settings);
    if minimal {
        config.full_context_interval_seconds = 0;
    }
    config
}

pub(crate) fn open_work_text(settings: &Settings) -> Result<String> {
    if settings.hosted_plugins.replace_local_todos {
        return Ok(String::new());
    }
    let memory = MemoryStore::from_settings(settings)?;
    Ok(memory.todos.open_work_digest(20)?)
}

pub(crate) fn active_reminders_text(settings: &Settings) -> Result<String> {
    if settings.hosted_plugins.replace_local_todos {
        return Ok(String::new());
    }
    let memory = MemoryStore::from_settings(settings)?;
    let reminders = memory
        .todos
        .due_reminders()?
        .into_iter()
        .map(|todo| ActiveReminder {
            title: todo.title,
            priority: todo.priority.as_str().to_string(),
            due: todo.due_date,
        })
        .collect::<Vec<_>>();
    Ok(format_active_reminders(&reminders, 10))
}

pub(crate) fn messages_command(command: MessageCommand) -> Result<()> {
    let settings = Settings::from_env();
    let store = MemoryStore::from_settings(&settings)?;
    let history = &store.messages;
    let output = match command {
        MessageCommand::Add {
            role,
            content,
            metadata,
        } => {
            let metadata = metadata.as_deref().map(serde_json::from_str).transpose()?;
            let message_id = history.add(MessageRole::parse(&role), &content, metadata)?;
            format!("Stored message {message_id}")
        }
        MessageCommand::Recent { limit } => {
            let messages = history.get_recent(limit)?;
            MessageHistory::format_messages(&messages)
        }
        MessageCommand::Search { query, role, limit } => {
            let role_filter = role.as_deref().map(MessageRole::parse);
            let messages = store.search_messages(&query, limit, role_filter.as_ref())?;
            MessageHistory::format_messages(&messages)
        }
        MessageCommand::Role { role, limit } => {
            let messages = history.get_by_role(&MessageRole::parse(&role), limit)?;
            MessageHistory::format_messages(&messages)
        }
        MessageCommand::Get { message_id } => match history.get(&message_id)? {
            Some(message) => MessageHistory::format_messages(&[message]),
            None => format!("Message {message_id} not found"),
        },
        MessageCommand::Delete { message_id } => {
            if history.delete(&message_id)? {
                format!("Deleted message {message_id}")
            } else {
                format!("Message {message_id} not found")
            }
        }
        MessageCommand::CleanupSearchResults { tools } => {
            let deleted = history.cleanup_search_results(optional_slice(&tools))?;
            format!("Cleaned up {deleted} search result message(s)")
        }
        MessageCommand::Count => format!("message_count: {}", history.count()?),
        MessageCommand::Clear => {
            let cleared = history.clear()?;
            format!("Cleared {cleared} message(s)")
        }
        MessageCommand::Context {
            max_messages,
            max_chars,
        } => {
            let messages = history.get_context_window(max_messages, max_chars)?;
            MessageHistory::format_messages(&messages)
        }
    };
    println!("{output}");
    Ok(())
}

pub(crate) async fn memory_command(command: MemoryCommand) -> Result<()> {
    let settings = Settings::from_env();
    let store = MemoryStore::from_settings(&settings)?;
    match command {
        MemoryCommand::Init => {
            let stats = store.stats()?;
            println!("memory_initialized: true");
            println!("workspace: {}", store.workspace_dir().display());
            println!("memory_blocks: {}", stats.memory_blocks);
        }
        MemoryCommand::Stats => {
            let stats = store.stats()?;
            println!("memory_blocks: {}", stats.memory_blocks);
            println!("archival_memories: {}", stats.archival_memories);
            println!("message_history: {}", stats.message_history);
            println!("notes: {}", stats.notes);
        }
        MemoryCommand::Context => {
            println!("{}", store.get_context_for_prompt()?);
        }
        MemoryCommand::ContextSplit => {
            let (stable, volatile) = store.get_context_split()?;
            println!("<stable_context>\n{stable}\n</stable_context>");
            println!("\n<volatile_context>\n{volatile}\n</volatile_context>");
        }
        MemoryCommand::Recall { message } => {
            let recent = store.messages.get_recent(10)?;
            let recall = Hippocampus::new(HippocampusConfig {
                enabled: settings.background.hippocampus_enabled,
                ..Default::default()
            })
            .recall(&store, &message, &recent)?;
            match recall {
                Some(recall) => println!("{recall}"),
                None => println!("No associative memory recall."),
            }
        }
        MemoryCommand::Curate { force } => {
            let curator = MemoryCurator::new(settings.paths.memory_dir.join("curator_state.json"));
            let router = lethe::llm::client::LlmRouter::new(
                lethe::llm::client::LlmRouterConfig::from_settings(&settings),
            );
            let stats = curator.run_pass(&store, &router, force).await?;
            println!("{}", serde_json::to_string_pretty(&stats)?);
        }
        MemoryCommand::BlockList { include_hidden } => {
            let blocks = store.blocks.list_blocks(include_hidden)?;
            println!("{}", serde_json::to_string_pretty(&blocks)?);
        }
        MemoryCommand::BlockRead { label } => match store.blocks.get(&label)? {
            Some(block) => println!("{}", serde_json::to_string_pretty(&block)?),
            None => println!("Block '{label}' not found"),
        },
        MemoryCommand::BlockCreate {
            label,
            value,
            description,
            limit,
            read_only,
            hidden,
        } => {
            store
                .blocks
                .create(&label, &value, &description, limit, read_only, hidden)?;
            println!("Created block '{label}'");
        }
        MemoryCommand::BlockUpdate {
            label,
            value,
            description,
        } => {
            if store
                .blocks
                .update(&label, value.as_deref(), description.as_deref())?
            {
                println!("Updated block '{label}'");
            } else {
                println!("Block '{label}' not found");
            }
        }
        MemoryCommand::BlockAppend { label, text } => {
            if store.blocks.append(&label, &text)? {
                println!("Appended to block '{label}'");
            } else {
                println!("Block '{label}' not found");
            }
        }
        MemoryCommand::BlockReplace {
            label,
            old_string,
            new_string,
        } => {
            if store.blocks.str_replace(&label, &old_string, &new_string)? {
                println!("Replaced text in block '{label}'");
            } else {
                println!("Text not found in block '{label}'");
            }
        }
        MemoryCommand::BlockDelete { label } => {
            if store.blocks.delete(&label)? {
                println!("Deleted block '{label}'");
            } else {
                println!("Block '{label}' not found");
            }
        }
    }
    Ok(())
}

pub(crate) fn archive_command(command: ArchiveCommand) -> Result<()> {
    let settings = Settings::from_env();
    let store = MemoryStore::from_settings(&settings)?;
    let memory = &store.archival;
    let output = match command {
        ArchiveCommand::Add {
            text,
            tags,
            metadata,
        } => {
            let metadata = metadata.as_deref().map(serde_json::from_str).transpose()?;
            let memory_id = memory.add(&text, metadata, &tags)?;
            format!("Stored in archival memory (id: {memory_id})")
        }
        ArchiveCommand::Search { query, tags, limit } => {
            let results = store.search_archival(&query, limit, optional_slice(&tags))?;
            ArchivalMemory::format_entries(&results)
        }
        ArchiveCommand::Recent { limit } => {
            let entries = memory.list_recent(limit)?;
            ArchivalMemory::format_entries(&entries)
        }
        ArchiveCommand::Get { memory_id } => match memory.get(&memory_id)? {
            Some(entry) => ArchivalMemory::format_entries(&[entry]),
            None => format!("Archival memory {memory_id} not found"),
        },
        ArchiveCommand::Tag { memory_id, tags } => {
            if memory.update_tags(&memory_id, &tags)? {
                format!("Updated archival memory {memory_id} tags")
            } else {
                format!("Archival memory {memory_id} not found")
            }
        }
        ArchiveCommand::Delete { memory_id } => {
            if memory.delete(&memory_id)? {
                format!("Deleted archival memory {memory_id}")
            } else {
                format!("Archival memory {memory_id} not found")
            }
        }
    };
    println!("{output}");
    Ok(())
}

pub(crate) fn note_command(command: NoteCommand) -> Result<()> {
    let settings = Settings::from_env();
    let memory = MemoryStore::from_settings(&settings)?;
    let store = &memory.notes;
    let output = match command {
        NoteCommand::Create {
            title,
            content,
            tags,
            subdir,
        } => {
            let path = store.create(&title, &content, &tags, subdir.as_deref())?;
            format!("Note saved: {} (tags: {})", path.display(), tags.join(", "))
        }
        NoteCommand::List { tags } => {
            let notes = store.list_notes(optional_slice(&tags))?;
            let mut output = NoteStore::format_list(&notes);
            if notes.is_empty() && !tags.is_empty() {
                output.push_str(&format!(" (tags: {})", tags.join(",")));
            }
            output
        }
        NoteCommand::Search { query, tags, limit } => {
            let results = memory.search_notes(&query, optional_slice(&tags), limit)?;
            NoteStore::format_search(&query, &tags, &results)
        }
        NoteCommand::Tags => {
            let tags = store.all_tags()?;
            if tags.is_empty() {
                "No note tags found.".to_string()
            } else {
                tags.join("\n")
            }
        }
        NoteCommand::Reindex => {
            let count = store.reindex()?;
            format!("Reindexed {count} note(s)")
        }
    };
    println!("{output}");
    Ok(())
}

pub(crate) async fn chat(message: String, system: Option<String>, aux: bool) -> Result<()> {
    let settings = Settings::from_env();
    if let Err(message) = settings.llm.ensure_ready() {
        anyhow::bail!(message);
    }
    let config = LlmRouterConfig::from_settings(&settings);
    let router = LlmRouter::new(config);
    let system = system.unwrap_or_else(|| {
        prompt_store(&settings)
            .load("agent_instructions", "You are Lethe.")
            .text
    });

    let response = router
        .complete(
            vec![LlmMessage::system(system), LlmMessage::user(message)],
            aux,
        )
        .await?;
    println!("{response}");
    Ok(())
}

pub(crate) fn empty_marker(value: &str) -> &str {
    if value.trim().is_empty() {
        "<unset>"
    } else {
        value
    }
}

fn source_label(source: &PromptSource) -> String {
    match source {
        PromptSource::Workspace(path) => format!("workspace:{}", path.display()),
        PromptSource::Config(path) => format!("config:{}", path.display()),
        PromptSource::Embedded => "embedded".to_string(),
        PromptSource::Fallback => "fallback".to_string(),
    }
}

fn parse_optional_status(value: Option<&str>) -> Result<Option<TodoStatus>> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(parse_status)
        .transpose()
}

fn parse_status(value: &str) -> Result<TodoStatus> {
    TodoStatus::parse(value).ok_or_else(|| {
        anyhow!(
            "invalid todo status '{}'; expected pending, in_progress, completed, deferred, or cancelled",
            value
        )
    })
}

fn parse_optional_priority(value: Option<&str>) -> Result<Option<TodoPriority>> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(parse_priority)
        .transpose()
}

fn parse_priority(value: &str) -> Result<TodoPriority> {
    TodoPriority::parse(value).ok_or_else(|| {
        anyhow!(
            "invalid todo priority '{}'; expected low, normal, high, or urgent",
            value
        )
    })
}

fn optional_slice(values: &[String]) -> Option<&[String]> {
    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}
