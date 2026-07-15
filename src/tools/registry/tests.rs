use std::collections::HashSet;

use serde_json::json;
use tempfile::tempdir;

use super::*;
use crate::memory::MemoryStore;
use crate::tools::hosted_plugins::{HostedPluginClient, RemoteToolDef, RemoteToolExposure};
use crate::tools::shell::ShellTools;
use crate::tools::web::WebTools;

fn registry() -> (tempfile::TempDir, MemoryStore, ShellTools) {
    let tmp = tempdir().unwrap();
    assert!(tmp.path().starts_with(std::env::temp_dir()));
    let workspace = tmp.path().join("workspace");
    let db = tmp.path().join("data/lethe.db");
    let notes = workspace.join("notes");
    let memory = MemoryStore::open(&workspace, db, notes).unwrap();
    let shell = ShellTools::new(&workspace);
    (tmp, memory, shell)
}

#[test]
fn exposes_core_tool_specs() {
    let (_tmp, memory, shell) = registry();
    let registry = ToolRegistry::new(&memory, memory.workspace_dir(), "/tmp/lethe-cache", &shell);
    let names = registry
        .tools()
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    assert!(names.contains(&"read_file".to_string()));
    assert!(names.contains(&"bash".to_string()));
    assert!(names.contains(&"get_terminal_screen".to_string()));
    assert!(names.contains(&"send_terminal_input".to_string()));
    assert!(names.contains(&"get_environment_info".to_string()));
    assert!(names.contains(&"check_command_exists".to_string()));
    assert!(names.contains(&"memory_read".to_string()));
    assert!(names.contains(&"note_search".to_string()));
    assert!(names.contains(&"web_search".to_string()));
    assert!(names.contains(&"fetch_webpage".to_string()));
    assert!(names.contains(&"browser_open".to_string()));
    assert!(names.contains(&"browser_snapshot".to_string()));
    assert!(names.contains(&"browser_click".to_string()));
    assert!(names.contains(&"browser_fill".to_string()));
    assert!(names.contains(&"view_image".to_string()));
    assert!(names.contains(&"todo_update".to_string()));
    assert!(names.contains(&"todo_remind_check".to_string()));
    assert!(names.contains(&"todo_reminded".to_string()));
}

#[test]
fn active_tool_specs_start_small_and_expand_on_request() {
    let (_tmp, memory, shell) = registry();
    let registry = ToolRegistry::new(&memory, memory.workspace_dir(), "/tmp/lethe-cache", &shell);
    let active = HashSet::new();
    let initial = registry
        .tools_for_active(&active)
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    assert!(initial.contains(&"request_tool".to_string()));
    assert!(initial.contains(&"bash".to_string()));
    assert!(initial.contains(&"web_search".to_string()));
    assert!(!initial.contains(&"browser_open".to_string()));
    assert!(!initial.contains(&"fetch_webpage".to_string()));
    // Kept in the initial cortex set (the core memory read/update pair).
    assert!(initial.contains(&"memory_read".to_string()));
    assert!(initial.contains(&"memory_update".to_string()));
    // Recategorized to Requestable — discoverable via request_tool but not
    // loaded up front, to keep the initial tool set small (better prefill for
    // smaller models like Gemma). Leaves room for the 3 transport tools when a
    // Telegram bot is connected and still stay within a 15-tool initial budget.
    for moved in [
        "todo_list",
        "todo_create",
        "conversation_search",
        "note_search",
        "note_create",
        "memory_complete",
    ] {
        assert!(
            !initial.contains(&moved.to_string()),
            "{moved} should be requestable"
        );
    }
    // Top-level (non-subagent) turn keeps a lean initial set; with the 3
    // Transport tools added when Telegram is connected this stays <= 15.
    assert!(
        initial.len() <= 12,
        "initial tool set too large: {}",
        initial.len()
    );

    let active = ["browser_open".to_string(), "fetch_webpage".to_string()]
        .into_iter()
        .collect::<HashSet<_>>();
    let expanded = registry
        .tools_for_active(&active)
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    assert!(expanded.contains(&"browser_open".to_string()));
    assert!(expanded.contains(&"fetch_webpage".to_string()));
}

#[test]
fn hosted_tools_replace_local_defs_and_join_requestable_groups() {
    let (_tmp, memory, shell) = registry();
    let hosted = HostedPluginClient::with_catalog_for_test(
        vec![
            RemoteToolDef {
                plugin_id: "agenda".to_string(),
                name: "todo_list".to_string(),
                description: "List hosted todos".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"limit": {"type": "integer"}},
                    "additionalProperties": false,
                }),
                exposure: RemoteToolExposure::Requestable,
                group: Some("agenda.todos".to_string()),
                replaces: vec!["todo_remind_check".to_string()],
                mutating: false,
            },
            RemoteToolDef {
                plugin_id: "agenda".to_string(),
                name: "todo_reopen".to_string(),
                description: "Reopen a hosted todo".to_string(),
                input_schema: json!({"type": "object", "properties": {}}),
                exposure: RemoteToolExposure::Requestable,
                group: Some("agenda.todos".to_string()),
                replaces: Vec::new(),
                mutating: true,
            },
        ],
        false,
    );
    let registry = ToolRegistry::with_runtime(
        &memory,
        memory.workspace_dir(),
        "/tmp/lethe-cache",
        &shell,
        ToolRuntime {
            hosted_plugins: Some(hosted),
            ..ToolRuntime::default()
        },
    );

    let all = registry.tools();
    assert_eq!(
        all.iter().filter(|tool| tool.name == "todo_list").count(),
        1,
        "remote todo_list must replace, not duplicate, the local schema"
    );
    assert!(all.iter().any(|tool| tool.name == "todo_reopen"));
    assert!(!all.iter().any(|tool| tool.name == "todo_remind_check"));
    assert_eq!(
        registry.group_siblings("todo_list"),
        vec!["todo_reopen".to_string()]
    );
    assert!(
        registry
            .requestable_tools_directory()
            .contains("todo_reopen — Reopen a hosted todo")
    );
}

#[test]
fn executes_files_memory_notes_and_shell_tools() {
    let (_tmp, memory, shell) = registry();
    let registry = ToolRegistry::new(&memory, memory.workspace_dir(), "/tmp/lethe-cache", &shell);
    let file_path = "test-output/a.txt";
    let resolved_file = memory.workspace_dir().join(file_path);

    assert!(
        registry
            .execute(
                "write_file",
                &json!({"file_path": file_path, "content": "hello"})
            )
            .contains("Successfully wrote")
    );
    assert!(resolved_file.starts_with(std::env::temp_dir()));
    assert!(resolved_file.exists());
    assert!(
        registry
            .execute("read_file", &json!({"file_path": file_path}))
            .contains("hello")
    );
    std::fs::write(memory.workspace_dir().join("image.png"), b"not a real png").unwrap();
    let image_payload = registry.execute("view_image", &json!({"file_path": "image.png"}));
    let image: serde_json::Value = serde_json::from_str(&image_payload).unwrap();
    assert_eq!(image["status"], "ok");
    assert_eq!(image["_image_view"]["mime_type"], "image/png");
    assert!(
        registry
            .execute(
                "memory_append",
                &json!({"label": "project", "text": "\nRust port"})
            )
            .contains("Appended")
    );
    assert!(
        registry
            .execute("memory_read", &json!({"label": "project"}))
            .contains("Rust port")
    );
    assert!(
        registry
            .execute(
                "note_create",
                &json!({"title": "Test Note", "content": "graph api", "tags": ["skill"]})
            )
            .contains("Note saved")
    );
    assert!(
        registry
            .execute("note_search", &json!({"query": "graph"}))
            .contains("Test Note")
    );
    assert_eq!(
        registry.execute("bash", &json!({"command": "echo ok"})),
        "ok"
    );
    assert!(
        registry
            .execute("get_environment_info", &json!({}))
            .contains("Environment Information")
    );
    assert!(
        registry
            .execute("check_command_exists", &json!({"command_name": "ls"}))
            .contains("available")
    );
    if !WebTools::is_available() {
        assert!(
            registry
                .execute("web_search", &json!({"query": "rust"}))
                .contains("EXA_API_KEY")
        );
    }
}

#[test]
fn hosted_safe_policy_exposes_memory_but_blocks_local_execution() {
    assert!(ToolPolicy::HostedSafe.allows_builtin("agent_id_status"));
    assert!(ToolPolicy::HostedSafe.allows_builtin("vault_list"));
    assert!(ToolPolicy::HostedSafe.allows_builtin("alien_browser_open"));
    assert!(!ToolPolicy::HostedSafe.allows_builtin("read_file"));
    assert!(!ToolPolicy::HostedSafe.allows_builtin("bash"));
    // The built-in browser is NOT a hosted capability — only the vault-sealed
    // `alien_browser_*` is. It must not resurface under the hosted policy.
    assert!(!ToolPolicy::HostedSafe.allows_builtin("browser_open"));
    assert!(!ToolPolicy::HostedSafe.allows_builtin("browser_snapshot"));

    let (_tmp, memory, shell) = registry();
    let registry = ToolRegistry::with_runtime(
        &memory,
        memory.workspace_dir(),
        "/tmp/lethe-cache",
        &shell,
        ToolRuntime {
            policy: ToolPolicy::HostedSafe,
            ..ToolRuntime::default()
        },
    );

    let names = registry
        .tools()
        .into_iter()
        .map(|tool| tool.name)
        .collect::<std::collections::HashSet<_>>();
    assert!(names.contains("memory_read"));
    assert!(names.contains("memory_update"));
    // The built-in browser is not a hosted capability — hidden and unavailable
    // under the hosted policy (hosted browsing is only the vault-sealed browser).
    assert!(!names.contains("browser_open"));
    assert!(!names.contains("bash"));
    assert!(!names.contains("read_file"));
    assert!(!registry.tool_is_available("bash"));
    assert!(!registry.tool_is_available("browser_open"));
    assert!(
        registry
            .execute("bash", &json!({"command": "echo forbidden"}))
            .contains("disabled by the active capability policy")
    );
    assert!(
        registry
            .execute("browser_open", &json!({"url": "https://example.com"}))
            .contains("disabled by the active capability policy")
    );
}

#[test]
fn hosted_safe_policy_allows_subagent_orchestration() {
    // Actor tools manage internal LLM workers only, and every subagent turn
    // re-enters the same policy gate — so hosted agents may orchestrate.
    for name in [
        "spawn_actor",
        "spawn_chain",
        "send_message",
        "wait_for_response",
        "discover_actors",
        "ping_actor",
        "kill_actor",
        "update_task_state",
        "get_task_state",
        "terminate",
        "restart_self",
    ] {
        assert!(
            ToolPolicy::HostedSafe.allows_builtin(name),
            "hosted policy must allow actor tool {name}"
        );
    }
    // The category lookup must not accidentally admit non-actor tools that
    // share no hosted prefix.
    assert!(!ToolPolicy::HostedSafe.allows_builtin("write_file"));
    assert!(!ToolPolicy::HostedSafe.allows_builtin("web_search"));

    // With an actor context attached under the hosted policy, orchestration is
    // available to the cortex on request (not pre-loaded), exactly as in the
    // full-policy standalone binary.
    let (_tmp, memory, shell) = registry();
    let actor_runtime = crate::actor::ActorRuntime::new(crate::actor::ActorRegistry::new());
    let registry = ToolRegistry::with_runtime(
        &memory,
        memory.workspace_dir(),
        "/tmp/lethe-cache",
        &shell,
        ToolRuntime {
            policy: ToolPolicy::HostedSafe,
            actor: Some(ActorToolContext {
                runtime: actor_runtime,
                actor_id: "cortex-test".to_string(),
                is_subagent: false,
            }),
            ..ToolRuntime::default()
        },
    );
    assert!(registry.tool_is_available("spawn_actor"));
    assert!(registry.tool_is_available("send_message"));
    assert!(!registry.is_initial_tool("spawn_actor"));
    assert!(!registry.tool_is_available("bash"));

    // The subagent-facing requestable directory honors the hosted policy: no
    // local execution families, no identity/vault families without a tenant
    // agent-id state dir. Actor tools are pre-loaded for subagents, so they
    // are deliberately absent from the on-request listing too.
    let directory = requestable_tools_directory_for_shape(ToolContextShape {
        has_actor: true,
        is_subagent: true,
        has_telegram: false,
        has_client: false,
        has_agent_id_state: false,
        policy: ToolPolicy::HostedSafe,
    });
    assert!(!directory.contains("spawn_actor"));
    assert!(!directory.contains("bash"));
    assert!(!directory.contains("read_file"));
    assert!(!directory.contains("web_search"));
    assert!(!directory.contains("alien_browser_open"));
    assert!(!directory.contains("agent_id_status"));
}

#[test]
fn executes_todo_update_and_reminder_tools() {
    let (_tmp, memory, shell) = registry();
    let registry = ToolRegistry::new(&memory, memory.workspace_dir(), "/tmp/lethe-cache", &shell);

    let created = registry.execute(
        "todo_create",
        &json!({"title": "Submit permit letter", "priority": "urgent"}),
    );
    assert!(created.contains("Created todo #1"));

    let updated = registry.execute(
        "todo_update",
        &json!({
            "todo_id": 1,
            "status": "in_progress",
            "description": "Need lab support context",
            "due_date": "2026-05-23"
        }),
    );
    assert!(updated.contains("Updated todo #1"));

    let listed = registry.execute("todo_list", &json!({"include_completed": true}));
    assert!(listed.contains("[~] #1"));
    assert!(listed.contains("Need lab support context"));

    let due = registry.execute("todo_remind_check", &json!({}));
    assert!(due.contains("Submit permit letter"));

    let reminded = registry.execute("todo_reminded", &json!({"todo_id": 1}));
    assert!(reminded.contains("Marked todo #1 as reminded"));

    let after = registry.execute("todo_remind_check", &json!({}));
    assert!(after.contains("No todos due for reminder"));

    let invalid = registry.execute("todo_update", &json!({"todo_id": 1, "status": "weird"}));
    assert!(invalid.contains("invalid todo status"));
}

#[tokio::test]
async fn exposes_and_executes_actor_tools_when_context_is_present() {
    use crate::actor::{ActorConfig, ActorRegistry, ActorRuntime, TaskState};

    let (_tmp, memory, shell) = registry();
    let mut actor_registry = ActorRegistry::new();
    let principal = actor_registry.spawn(
        ActorConfig::new("cortex", "Delegate focused work").in_group("main"),
        None,
        true,
    );
    let shared = ActorRuntime::new(actor_registry);
    let registry = ToolRegistry::with_runtime(
        &memory,
        memory.workspace_dir(),
        "/tmp/lethe-cache",
        &shell,
        ToolRuntime {
            actor: Some(ActorToolContext {
                runtime: shared.clone(),
                actor_id: principal.clone(),
                is_subagent: false,
            }),
            ..ToolRuntime::default()
        },
    );

    let names = registry
        .tools()
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    assert!(names.contains(&"spawn_actor".to_string()));
    assert!(names.contains(&"spawn_chain".to_string()));
    assert!(names.contains(&"send_message".to_string()));
    assert!(names.contains(&"discover_actors".to_string()));
    assert!(names.contains(&"update_task_state".to_string()));
    assert!(names.contains(&"terminate".to_string()));
    assert!(!names.contains(&"restart_self".to_string()));

    let spawned = registry
        .execute_async(
            "spawn_actor",
            &json!({
                "name": "Worker",
                "goals": "Read the report and summarize risks",
                "tools": "read_file",
                "model": "aux",
                "max_turns": 3
            }),
        )
        .await;
    assert!(spawned.contains("Spawned actor 'worker'"));
    let worker = shared
        .find_by_name("worker", Some("main"))
        .await
        .unwrap()
        .id
        .clone();

    let discovered = registry.execute_async("discover_actors", &json!({})).await;
    assert!(discovered.contains("worker"));
    assert!(discovered.contains("[child]"));

    let sent = registry
        .execute_async(
            "send_message",
            &json!({"actor_id": worker.clone(), "content": "Please start now"}),
        )
        .await;
    assert!(sent.contains("Message sent"));
    assert_eq!(
        shared.pop_inbox(&worker).await.unwrap().content,
        "Please start now"
    );

    let updated = registry
        .execute_async(
            "update_task_state",
            &json!({"state": "blocked", "note": "waiting on worker"}),
        )
        .await;
    assert!(updated.contains("running -> blocked"));
    assert_eq!(
        shared.task_state(&principal).await.unwrap(),
        TaskState::Blocked
    );
    assert_eq!(
        registry.execute_async("get_task_state", &json!({})).await,
        "Task state: blocked"
    );

    let killed = registry
        .execute_async("kill_actor", &json!({"actor_id": worker}))
        .await;
    assert!(killed.contains("Killed"));
}

#[test]
fn exposes_and_executes_telegram_tools_when_context_is_present() {
    use std::sync::{Arc, Mutex};

    use crate::interfaces::telegram::{TelegramToolContext, TelegramTurnGuard};

    let (_tmp, memory, shell) = registry();
    let guard = Arc::new(Mutex::new(TelegramTurnGuard::new()));
    let registry = ToolRegistry::with_runtime(
        &memory,
        memory.workspace_dir(),
        "/tmp/lethe-cache",
        &shell,
        ToolRuntime {
            telegram: Some(TelegramToolContext {
                token: "token".to_string(),
                chat_id: 99,
                user_id: Some(7),
                last_message_id: Some(42),
                guard: Some(guard.clone()),
                dry_run: true,
                sent_messages: None,
            }),
            ..ToolRuntime::default()
        },
    );

    let names = registry
        .tools()
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    assert!(names.contains(&"telegram_send_message".to_string()));
    assert!(names.contains(&"telegram_send_file".to_string()));
    assert!(names.contains(&"telegram_react".to_string()));
    // Telegram sessions keep the branded set; the neutral alias stays hidden.
    assert!(!names.contains(&"chat_send_message".to_string()));

    let message_payload = registry.execute(
        "telegram_send_message",
        &json!({
            "text": "hello",
            "parse_mode": "html",
            "reply_markup_json": r#"{"inline_keyboard":[[{"text":"Start","callback_data":"start"}]]}"#
        }),
    );
    let message: serde_json::Value = serde_json::from_str(&message_payload).unwrap();
    assert_eq!(message["success"], true);
    assert_eq!(message["parse_mode"], "HTML");
    assert_eq!(
        message["reply_markup"]["inline_keyboard"][0][0]["text"],
        "Start"
    );

    let invalid_markup = registry.execute(
        "telegram_send_message",
        &json!({"text": "hello", "reply_markup_json": r#"{"inline_keyboard":[[{"text":"Bad"}]]}"#}),
    );
    assert!(invalid_markup.contains("exactly one action"));

    let file_payload = registry.execute(
        "telegram_send_file",
        &json!({"file_path_or_url": "https://example.com/chart.png"}),
    );
    let file: serde_json::Value = serde_json::from_str(&file_payload).unwrap();
    assert_eq!(file["success"], true);
    assert_eq!(file["type"], "photo");
    assert_eq!(file["filename"], "chart.png");

    let payload = registry.execute("telegram_react", &json!({"emoji": "🔥"}));
    let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(value["success"], true);
    assert_eq!(value["queued"], true);
    assert_eq!(value["message_id"], 42);

    let pending = guard.lock().unwrap().drain_pending_reactions();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].chat_id, 99);
    assert_eq!(pending[0].message_id, 42);
    assert_eq!(pending[0].emoji, "🔥");
}

#[test]
fn client_tool_context_exposes_telegram_tools_as_events() {
    use std::sync::{Arc, Mutex};

    let (_tmp, memory, shell) = registry();
    let events = Arc::new(Mutex::new(Vec::<ClientToolEvent>::new()));
    let registry = ToolRegistry::with_runtime(
        &memory,
        memory.workspace_dir(),
        "/tmp/lethe-cache",
        &shell,
        ToolRuntime {
            client: Some(ClientToolContext::new(7, Some(55), {
                let events = events.clone();
                move |event| {
                    events.lock().unwrap().push(event);
                    true
                }
            })),
            ..ToolRuntime::default()
        },
    );

    let names = registry
        .tools()
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    // Client-only sessions get the transport-neutral chat egress, never the
    // Telegram-branded tool set (a desktop/web user has no Telegram attached).
    assert!(names.contains(&"chat_send_message".to_string()));
    assert!(!names.contains(&"telegram_send_message".to_string()));
    assert!(!names.contains(&"telegram_send_file".to_string()));
    assert!(!names.contains(&"telegram_react".to_string()));

    let message_payload = registry.execute(
        "chat_send_message",
        &json!({
            "text": "progress",
            "parse_mode": "markdown",
            "reply_markup_json": r#"{"keyboard":[["Yes","No"]],"resize_keyboard":true}"#
        }),
    );
    let message: serde_json::Value = serde_json::from_str(&message_payload).unwrap();
    assert_eq!(message["success"], true);
    assert_eq!(message["message_id"], 1);
    assert_eq!(message["chat_id"], 7);
    assert_eq!(message["reply_markup"]["keyboard"][0][0], "Yes");
    assert_eq!(message["reply_markup"]["one_time_keyboard"], true);

    let file_payload = registry.execute(
        "telegram_send_file",
        &json!({
            "file_path_or_url": "https://example.com/report.pdf",
            "caption": "report"
        }),
    );
    let file: serde_json::Value = serde_json::from_str(&file_payload).unwrap();
    assert_eq!(file["success"], true);
    assert_eq!(file["message_id"], 2);
    assert_eq!(file["type"], "document");

    let reaction_payload = registry.execute("telegram_react", &json!({"emoji": "✅"}));
    let reaction: serde_json::Value = serde_json::from_str(&reaction_payload).unwrap();
    assert_eq!(reaction["success"], true);
    assert_eq!(reaction["message_id"], 55);

    let events = events.lock().unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].event, "text");
    assert_eq!(events[0].data["content"], "progress");
    assert_eq!(events[0].data["parse_mode"], "MarkdownV2");
    assert_eq!(events[0].data["message_id"], 1);
    assert_eq!(events[0].data["reply_markup"]["keyboard"][0][1], "No");
    assert_eq!(events[0].data["reply_markup"]["one_time_keyboard"], true);
    assert_eq!(events[1].event, "file");
    assert_eq!(events[1].data["type"], "document");
    assert_eq!(events[1].data["path"], "https://example.com/report.pdf");
    assert_eq!(events[1].data["caption"], "report");
    assert_eq!(events[2].event, "reaction");
    assert_eq!(events[2].data["emoji"], "✅");
    assert_eq!(events[2].data["message_id"], 55);
}
