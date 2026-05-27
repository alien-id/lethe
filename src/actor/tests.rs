use chrono::{Duration as ChronoDuration, Utc};
use serde_json::json;

use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::time::{Duration as TokioDuration, sleep};

#[test]
fn intent_routing_matches_core_semantics() {
    assert!(MessageIntent::Done.wakes_cortex());
    assert!(MessageIntent::Alert.wakes_cortex());
    assert!(!MessageIntent::Info.wakes_cortex());
    assert!(!MessageIntent::Message.wakes_cortex());
    assert_eq!(MessageIntent::Reminder.channel(), "user_notify");
    assert_eq!(MessageIntent::Progress.channel(), "task_update");
    assert_eq!(MessageIntent::Message.channel(), "");
}

#[test]
fn unknown_user_notify_kind_defaults_to_info() {
    assert_eq!(
        MessageIntent::from_strings("user_notify", "routine"),
        MessageIntent::Info
    );
    // Strict parse: exact aliases match, compound/unknown strings default to Info.
    assert_eq!(MessageIntent::from_strings("", "warning"), MessageIntent::Alert);
    assert_eq!(
        MessageIntent::from_strings("", "deadline"),
        MessageIntent::Reminder
    );
    assert_eq!(
        MessageIntent::from_strings("", "deadline_warning"),
        MessageIntent::Info
    );
    assert_eq!(MessageIntent::from_strings("", "done"), MessageIntent::Done);
    assert_eq!(
        MessageIntent::from_strings("task_update", ""),
        MessageIntent::Progress
    );
}

#[test]
fn event_bus_keeps_only_recent_events() {
    let mut bus = ActorEventBus::new(2);
    bus.emit(ActorEvent::new("first", "a"));
    bus.emit(ActorEvent::new("second", "a"));
    bus.emit(ActorEvent::new("third", "b"));

    assert_eq!(bus.len(), 2);
    let all = bus.query(None, None, None, 10);
    assert_eq!(all[0].event_type, "third");
    assert_eq!(all[1].event_type, "second");
    assert_eq!(bus.query(None, Some("a"), None, 10).len(), 1);
}

fn registry_with_principal_and_worker() -> (ActorRegistry, String, String) {
    let mut registry = ActorRegistry::new();
    let principal = registry.spawn(
        ActorConfig::new("cortex", "Serve the user and delegate subtasks").in_group("main"),
        None,
        true,
    );
    let mut worker_config =
        ActorConfig::new("researcher", "Research the topic and report findings").in_group("main");
    worker_config.tools = vec!["web_search".to_string(), "read_file".to_string()];
    worker_config.max_turns = 5;
    let worker = registry.spawn(worker_config, Some(&principal), false);
    (registry, principal, worker)
}

#[test]
fn registry_spawns_discovers_and_cleans_terminated_actors() {
    let (mut registry, principal, worker) = registry_with_principal_and_worker();

    assert_eq!(registry.get_principal().unwrap().id, principal);
    assert_eq!(registry.active_count(), 2);
    assert_eq!(registry.discover("main").len(), 2);
    assert_eq!(registry.discover_active("main").len(), 2);
    assert_eq!(
        registry
            .find_by_name("researcher", Some("main"))
            .unwrap()
            .id,
        worker
    );
    assert!(
        !registry
            .events
            .query(Some("actor_spawned"), None, None, 10)
            .is_empty()
    );

    assert!(registry.terminate(&worker, Outcome::Success, "done").unwrap());
    assert_eq!(registry.active_count(), 1);
    assert_eq!(registry.discover("main").len(), 2);
    assert_eq!(registry.discover_active("main").len(), 1);
    assert_eq!(
        registry.discover_recently_finished("main", 1)[0]
            .result()
            .unwrap(),
        "done"
    );

    registry.get_mut(&worker).unwrap().terminated_at =
        Some(Utc::now() - ChronoDuration::seconds(ActorRegistry::STALE_SECONDS + 1));
    assert_eq!(registry.cleanup_terminated(false), 1);
    assert!(registry.get(&worker).is_none());
}

#[test]
fn registry_enforces_relationships_and_routes_messages() {
    let (mut registry, principal, worker) = registry_with_principal_and_worker();

    let sent = registry
        .send_to(
            &principal,
            &worker,
            "Hello worker",
            None,
            serde_json::Map::new(),
            None,
        )
        .unwrap();
    assert_eq!(sent.intent, MessageIntent::Info);
    let received = registry.pop_inbox(&worker).unwrap();
    assert_eq!(received.content, "Hello worker");
    assert_eq!(registry.get(&worker).unwrap().messages().len(), 1);

    let mut metadata = serde_json::Map::new();
    metadata.insert("channel".to_string(), json!("user_notify"));
    metadata.insert("kind".to_string(), json!("deadline"));
    registry
        .send_to(&worker, &principal, "Deadline soon", None, metadata, None)
        .unwrap();
    let notice = registry.pop_inbox(&principal).unwrap();
    assert_eq!(notice.intent, MessageIntent::Reminder);
    assert_eq!(notice.metadata.get("channel").unwrap(), "user_notify");
    assert_eq!(
        registry
            .events
            .query(Some("user_notify"), Some(&worker), None, 10)
            .len(),
        1
    );

    let stranger = registry.spawn(
        ActorConfig::new("stranger", "Other").in_group("other"),
        None,
        false,
    );
    let err = registry
        .send_to(
            &stranger,
            &worker,
            "not allowed",
            None,
            serde_json::Map::new(),
            None,
        )
        .unwrap_err();
    assert!(matches!(err, ActorError::PermissionDenied { .. }));
}

#[test]
fn termination_and_kill_notify_parent_once() {
    let (mut registry, principal, worker) = registry_with_principal_and_worker();

    assert!(
        registry
            .terminate(&worker, Outcome::Success, "Found 5 results")
            .unwrap()
    );
    let done = registry.pop_inbox(&principal).unwrap();
    assert_eq!(done.intent, MessageIntent::Done);
    assert_eq!(done.metadata.get("kind").unwrap(), "done");
    assert!(done.content.contains("Found 5 results"));
    let done_events = registry
        .events
        .query(Some("actor_message"), Some(&worker), Some("main"), 10);
    assert!(done_events.iter().any(|event| {
        event.payload.get("message_id") == Some(&json!(done.id.clone()))
            && event.payload.get("recipient") == Some(&json!(principal.clone()))
            && event.payload.get("channel") == Some(&json!("task_update"))
            && event.payload.get("intent") == Some(&json!("done"))
    }));
    assert!(
        !registry
            .terminate(&worker, Outcome::Success, "second")
            .unwrap()
    );
    assert_eq!(
        registry.get(&worker).unwrap().result(),
        Some("Found 5 results")
    );

    let child = registry.spawn(
        ActorConfig::new("coder", "Write code").in_group("main"),
        Some(&principal),
        false,
    );
    assert!(registry.kill_child(&principal, &child).unwrap());
    let failed = registry.pop_inbox(&principal).unwrap();
    assert_eq!(failed.intent, MessageIntent::Failed);
    assert!(failed.content.contains("Killed by parent"));
}

#[test]
fn task_state_transitions_are_validated() {
    let (mut registry, _principal, worker) = registry_with_principal_and_worker();

    assert!(
        registry
            .set_task_state(&worker, "blocked", "waiting on input")
            .unwrap()
            .contains("running -> blocked")
    );
    assert_eq!(
        registry.get(&worker).unwrap().task_state,
        TaskState::Blocked
    );
    assert_eq!(
        registry.get(&worker).unwrap().task_state_note(),
        "waiting on input"
    );

    registry
        .set_task_state(&worker, "done", "finished")
        .unwrap();
    let err = registry
        .set_task_state(&worker, "running", "restart")
        .unwrap_err();
    assert!(matches!(err, ActorError::InvalidTaskTransition { .. }));
}

#[test]
fn system_prompt_includes_relationships_and_inbox() {
    let (mut registry, principal, worker) = registry_with_principal_and_worker();
    registry
        .send_to(
            &principal,
            &worker,
            "Check the database",
            None,
            serde_json::Map::new(),
            None,
        )
        .unwrap();

    let principal_prompt = registry.build_system_prompt(&principal).unwrap();
    assert!(principal_prompt.contains("cortex"));
    assert!(
        principal_prompt
            .to_ascii_lowercase()
            .contains("quick tasks")
    );
    assert!(principal_prompt.contains("<available_on_request>"));
    assert!(principal_prompt.contains("[child]"));

    let worker_prompt = registry.build_system_prompt(&worker).unwrap();
    assert!(worker_prompt.contains("subagent"));
    assert!(worker_prompt.contains("researcher"));
    assert!(worker_prompt.contains("[parent]"));
    assert!(worker_prompt.contains("visible_actors"));
    assert!(worker_prompt.contains("inbox_block"));
    assert!(worker_prompt.contains("Check the database"));
}

#[test]
fn actor_tool_methods_spawn_discover_send_and_ping() {
    let (mut registry, principal, worker) = registry_with_principal_and_worker();

    let discovered = registry
        .discover_for_actor(&principal, None, false)
        .unwrap();
    assert!(discovered.contains("researcher"));
    assert!(discovered.contains("[child]"));
    assert!(discovered.contains("active only"));

    let result = registry
        .spawn_child_for_actor(
            &principal,
            ActorSpawnRequest {
                name: "Code Helper",
                goals: "Write the implementation",
                group: None,
                tools: "read_file,write_file",
                model: "main",
                max_turns: 10,
            },
        )
        .unwrap();
    assert!(matches!(result, SpawnReport::Spawned { .. }));
    assert!(result.message().contains("Spawned actor 'code-helper'"));
    assert!(result.message().contains("model=main"));
    assert!(registry.find_by_name("code-helper", Some("main")).is_some());

    let duplicate = registry
        .spawn_child_for_actor(
            &principal,
            ActorSpawnRequest {
                name: "researcher",
                goals: "same task",
                group: None,
                tools: "",
                model: "aux",
                max_turns: 20,
            },
        )
        .unwrap();
    assert!(matches!(duplicate, SpawnReport::Rejected { .. }));
    assert!(duplicate.message().contains("DUPLICATE BLOCKED"));
    assert!(duplicate.message().contains(&worker));

    let sent = registry.send_message_tool(&principal, &worker, "Hello", None, "", "");
    assert!(sent.contains("Message sent"));
    assert_eq!(registry.pop_inbox(&worker).unwrap().content, "Hello");

    let ping = registry.ping_actor(&worker);
    assert!(ping.contains("researcher"));
    assert!(ping.contains("running"));
}

#[test]
fn actor_tool_methods_kill_terminate_restart_and_finished_listing() {
    let (mut registry, principal, worker) = registry_with_principal_and_worker();

    let terminate =
        registry.terminate_tool(&worker, "All done", "partial", "src/lib.rs", "run tests");
    assert!(terminate.contains("Terminated"));
    let finished = registry
        .discover_for_actor(&principal, None, true)
        .unwrap();
    assert!(finished.contains("researcher"));
    assert!(finished.contains("[outcome: partial]"));
    assert!(finished.contains("[files: src/lib.rs]"));

    let child = registry.spawn(
        ActorConfig::new("worker-two", "Do another task").in_group("main"),
        Some(&principal),
        false,
    );
    let killed = registry.kill_actor_tool(&principal, &child);
    assert!(killed.contains("Killed"));
    assert_eq!(registry.get(&child).unwrap().state, ActorState::Terminated);

    let restart_child = registry.spawn(
        ActorConfig::new("restart-me", "Bad goal").in_group("main"),
        Some(&principal),
        false,
    );
    let restart = registry.restart_self(&restart_child, "Better goals");
    assert!(restart.contains("Restart requested"));
    assert_eq!(
        registry.get(&restart_child).unwrap().result(),
        Some("Restart requested with new goals:\nBetter goals")
    );
}

#[test]
fn actor_turn_specs_increment_and_enforce_max_turns() {
    let (mut registry, _principal, worker) = registry_with_principal_and_worker();
    registry.get_mut(&worker).unwrap().config.max_turns = 1;

    let spec = registry.prepare_actor_turn(&worker).unwrap().unwrap();
    assert_eq!(spec.actor_id, worker);
    assert_eq!(spec.name, "researcher");
    assert_eq!(spec.turn_number, 1);
    assert_eq!(spec.max_turns, 1);
    assert_eq!(registry.get(&worker).unwrap().turn_count(), 1);

    registry
        .record_actor_turn_response(&worker, "I need another pass")
        .unwrap();
    assert_eq!(registry.get(&worker).unwrap().state, ActorState::Waiting);
    assert!(
        registry
            .get(&worker)
            .unwrap()
            .messages()
            .iter()
            .any(|message| message.content == "I need another pass")
    );

    assert!(registry.prepare_actor_turn(&worker).unwrap().is_none());
    assert_eq!(registry.get(&worker).unwrap().state, ActorState::Terminated);
    assert!(
        registry
            .get(&worker)
            .unwrap()
            .result()
            .unwrap()
            .contains("Max turns reached")
    );
}

#[tokio::test]
async fn runtime_executor_wakes_resident_actor_without_external_round() {
    let (mut registry, _principal, worker) = registry_with_principal_and_worker();
    registry.get_mut(&worker).unwrap().config.max_turns = 1;
    let runtime = ActorRuntime::new(registry);
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_executor = calls.clone();
    runtime
        .install_turn_executor(Arc::new(move |_spec, _runtime| {
            let calls = calls_for_executor.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok("resident actor completed a spontaneous turn".to_string())
            })
        }))
        .unwrap();

    for _ in 0..20 {
        if calls.load(Ordering::SeqCst) > 0 {
            break;
        }
        sleep(TokioDuration::from_millis(25)).await;
    }

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let info = runtime
        .find_by_name("researcher", Some("main"))
        .await
        .unwrap();
    assert_eq!(info.state, ActorState::Waiting);
}

#[tokio::test]
async fn runtime_returns_principal_task_update_events() {
    let (mut registry, principal, worker) = registry_with_principal_and_worker();
    let mut metadata = serde_json::Map::new();
    metadata.insert("channel".to_string(), json!("task_update"));
    metadata.insert("kind".to_string(), json!("progress"));
    registry
        .send_to(
            &worker,
            &principal,
            "Halfway through the research",
            None,
            metadata,
            None,
        )
        .unwrap();
    registry
        .terminate(&worker, Outcome::Success, "Found the answer")
        .unwrap();

    let runtime = ActorRuntime::new(registry);
    let events = runtime
        .principal_task_update_events(&principal, 10)
        .await
        .unwrap();

    assert_eq!(events.len(), 2);
    assert_eq!(events[0].actor_name, "researcher");
    assert_eq!(
        events[0].event.payload.get("intent"),
        Some(&json!("progress"))
    );
    assert_eq!(events[1].event.payload.get("intent"), Some(&json!("done")));
}

#[tokio::test]
async fn runtime_excludes_dmn_task_updates_to_avoid_reflection_leak() {
    let (mut registry, principal, worker) = registry_with_principal_and_worker();
    let mut dmn_config = ActorConfig::new(
        crate::actor::background::DMN_ACTOR_NAME,
        "Background reflection",
    )
    .in_group("main");
    dmn_config.persistent = true;
    let dmn = registry.spawn(dmn_config, Some(&principal), false);

    let mut progress_meta = serde_json::Map::new();
    progress_meta.insert("channel".to_string(), json!("task_update"));
    progress_meta.insert("kind".to_string(), json!("progress"));
    registry
        .send_to(
            &dmn,
            &principal,
            "Background reflection summary that must not surface",
            None,
            progress_meta.clone(),
            None,
        )
        .unwrap();
    registry
        .send_to(
            &worker,
            &principal,
            "Worker progress that should reach cortex",
            None,
            progress_meta,
            None,
        )
        .unwrap();

    let runtime = ActorRuntime::new(registry);
    let events = runtime
        .principal_task_update_events(&principal, 10)
        .await
        .unwrap();

    assert_eq!(events.len(), 1, "only the non-DMN update should pass");
    assert_eq!(events[0].actor_name, "researcher");
}
