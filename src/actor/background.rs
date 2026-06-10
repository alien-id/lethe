use std::collections::{HashMap, HashSet};

use kameo::message::{Context, Message};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::actor::notification::{
    GateAction, GateDecision, NotificationAssessment, NotificationGate, NotificationScoring,
    UserNotificationSignal,
};
use crate::actor::{
    ActorConfig, ActorError, ActorNamedEvent, ActorRegistry, ActorRuntime, ActorSupervisor,
    MessageIntent, ModelTier,
};
use crate::scheduler::curator::CuratorRunStats;

pub const DMN_ACTOR_NAME: &str = "dmn";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BackgroundNotification {
    pub signal: UserNotificationSignal,
    pub assessment: NotificationAssessment,
    pub decision: GateDecision,
}

impl BackgroundNotification {
    pub fn user_message(&self) -> String {
        self.signal.content.trim().to_string()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BackgroundResult {
    pub dmn_actor_id: Option<String>,
    pub curator: Option<CuratorRunStats>,
    pub notifications: Vec<BackgroundNotification>,
}

impl BackgroundResult {
    pub fn user_messages(&self) -> Vec<String> {
        self.notifications
            .iter()
            .map(BackgroundNotification::user_message)
            .filter(|message| !message.trim().is_empty())
            .collect()
    }
}

fn ensure_dmn_actor(
    registry: &mut ActorRegistry,
    principal_id: &str,
) -> Result<String, ActorError> {
    let principal = registry
        .get(principal_id)
        .ok_or_else(|| ActorError::NotFound(principal_id.to_string()))?
        .clone();
    if let Some(existing) = registry.find_by_name(DMN_ACTOR_NAME, Some(&principal.config.group)) {
        return Ok(existing.id.clone());
    }

    let mut config = ActorConfig::new(
        DMN_ACTOR_NAME,
        "Run quiet background reflection. Review memory, active tasks, and reminders. Notify the cortex only for user-relevant reminders, warnings, or concise insights.",
    )
    .in_group(principal.config.group);
    config.model = Some(ModelTier::Aux);
    config.max_turns = 10_000;
    config.persistent = true;
    config.tools = vec![
        "memory_read".to_string(),
        "memory_update".to_string(),
        "archival_search".to_string(),
        "conversation_search".to_string(),
        "note_search".to_string(),
        "todo_list".to_string(),
        "send_message".to_string(),
        "terminate".to_string(),
    ];
    Ok(registry.spawn(config, Some(principal_id), false))
}

fn queue_dmn_heartbeat_message(
    registry: &mut ActorRegistry,
    principal_id: &str,
    dmn_actor_id: &str,
    heartbeat_message: &str,
    reminders: &str,
) -> Result<String, ActorError> {
    let mut metadata = serde_json::Map::new();
    metadata.insert("source".to_string(), json!("background_heartbeat"));
    metadata.insert("kind".to_string(), json!("heartbeat"));
    let content = format_dmn_heartbeat_message(heartbeat_message, reminders);
    let message = registry.send_to(
        principal_id,
        dmn_actor_id,
        content,
        None,
        metadata,
        Some(MessageIntent::Message),
    )?;
    Ok(message.id)
}

pub async fn queue_dmn_heartbeat(
    runtime: &ActorRuntime,
    principal_id: &str,
    heartbeat_message: &str,
    reminders: &str,
) -> Result<String, ActorError> {
    runtime
        .supervisor
        .ask(QueueDmnHeartbeat {
            principal_id: principal_id.to_string(),
            heartbeat_message: heartbeat_message.to_string(),
            reminders: reminders.to_string(),
        })
        .await
        .map_err(|error| ActorError::Runtime(format!("{error:?}")))
}

#[derive(Debug)]
struct QueueDmnHeartbeat {
    principal_id: String,
    heartbeat_message: String,
    reminders: String,
}

impl Message<QueueDmnHeartbeat> for ActorSupervisor {
    type Reply = Result<String, ActorError>;

    async fn handle(
        &mut self,
        message: QueueDmnHeartbeat,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let dmn_actor_id = ensure_dmn_actor(&mut self.registry, &message.principal_id)?;
        queue_dmn_heartbeat_message(
            &mut self.registry,
            &message.principal_id,
            &dmn_actor_id,
            &message.heartbeat_message,
            &message.reminders,
        )?;
        self.sync_resident_actors(ctx.actor_ref().clone());
        self.wake_actor(&dmn_actor_id, "dmn_heartbeat");
        Ok(dmn_actor_id)
    }
}

fn format_dmn_heartbeat_message(heartbeat_message: &str, reminders: &str) -> String {
    let mut parts = vec![
        "[Background heartbeat]".to_string(),
        "Reflect quietly. Use user_notify only for items that deserve user attention.".to_string(),
    ];
    if !heartbeat_message.trim().is_empty() {
        parts.push(format!(
            "Heartbeat context:\n{}",
            truncate(heartbeat_message.trim(), 1200)
        ));
    }
    if !reminders.trim().is_empty() {
        parts.push(format!(
            "Active reminders:\n{}",
            truncate(reminders.trim(), 800)
        ));
    }
    parts.join("\n\n")
}

/// Aux-LLM content gate that catches the class of leaks the heuristic gate
/// can't: internal reflection, meta commentary about DMN/cortex, redundant
/// pings the model already made. Renders `notification_review.md`, parses
/// the `{send, text}` decision, and either drops or rewrites the candidate.
/// Failures are treated as "drop" so a broken LLM call never leaks raw
/// reflection content to the user.
pub async fn review_notifications_with_llm(
    candidates: Vec<BackgroundNotification>,
    recent_context: &str,
    prompts: &crate::llm::prompts::PromptStore,
    router: &crate::llm::client::LlmRouter,
) -> Vec<BackgroundNotification> {
    let mut kept = Vec::new();
    for mut candidate in candidates {
        match review_one(&candidate, recent_context, prompts, router).await {
            ReviewOutcome::Send(text) => {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    candidate.signal.content = trimmed.to_string();
                }
                kept.push(candidate);
            }
            ReviewOutcome::Drop(reason) => {
                tracing::info!(
                    actor = %candidate.signal.source_name,
                    event = %candidate.signal.event_id,
                    "notification dropped by review gate: {reason}"
                );
            }
        }
    }
    kept
}

enum ReviewOutcome {
    Send(String),
    Drop(String),
}

async fn review_one(
    candidate: &BackgroundNotification,
    recent_context: &str,
    prompts: &crate::llm::prompts::PromptStore,
    router: &crate::llm::client::LlmRouter,
) -> ReviewOutcome {
    let mut variables = HashMap::new();
    variables.insert("signal".to_string(), candidate.signal.content.clone());
    variables.insert(
        "context".to_string(),
        if recent_context.trim().is_empty() {
            "(no recent context)".to_string()
        } else {
            recent_context.to_string()
        },
    );
    let prompt = prompts
        .render(
            "notification_review",
            &variables,
            "Review the SIGNAL. Reply JSON only: {\"send\":bool,\"text\":string}.\n\nSIGNAL:\n{signal}\n\nRECENT CONTEXT:\n{context}",
        )
        .text;
    let messages = vec![crate::llm::client::LlmMessage::user(prompt)];
    let raw = match router.complete(messages, true).await {
        Ok(text) => text,
        Err(error) => {
            return ReviewOutcome::Drop(format!("llm review failed: {error}"));
        }
    };
    parse_review(&raw)
}

fn parse_review(raw: &str) -> ReviewOutcome {
    let trimmed = raw.trim();
    let json_str = match (trimmed.find('{'), trimmed.rfind('}')) {
        (Some(start), Some(end)) if end >= start => &trimmed[start..=end],
        _ => return ReviewOutcome::Drop(format!("non-JSON review response: {trimmed}")),
    };
    let value: Value = match serde_json::from_str(json_str) {
        Ok(value) => value,
        Err(error) => return ReviewOutcome::Drop(format!("review JSON parse error: {error}")),
    };
    let send = value.get("send").and_then(Value::as_bool).unwrap_or(false);
    if !send {
        return ReviewOutcome::Drop("review gate said send=false".to_string());
    }
    let text = value
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return ReviewOutcome::Drop("review gate returned empty text".to_string());
    }
    ReviewOutcome::Send(text)
}

pub fn collect_user_notifications_from_events(
    events: Vec<ActorNamedEvent>,
    gate: &mut NotificationGate,
    processed_event_ids: &mut HashSet<String>,
) -> Vec<BackgroundNotification> {
    let scoring = NotificationScoring;
    let mut notifications = Vec::new();

    for named in events {
        let event = named.event;
        if !processed_event_ids.insert(event.id.clone()) {
            continue;
        }
        let Some(content) = event
            .payload
            .get("message")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|content| !content.is_empty())
        else {
            continue;
        };
        let kind = event
            .payload
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("info");
        let metadata = event
            .payload
            .get("metadata")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let signal = UserNotificationSignal::new(
            event.id,
            named.actor_name,
            event.actor_id,
            kind,
            content,
            metadata,
        );
        let assessment = scoring.assess(&signal);
        let decision = gate.decide(&signal, &assessment);
        if decision.action == GateAction::Review {
            notifications.push(BackgroundNotification {
                signal,
                assessment,
                decision,
            });
        }
    }
    notifications
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut out = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        out.push_str("\n...[truncated]");
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::json;

    use super::*;
    use crate::actor::ActorConfig;
    use crate::actor::notification::{NotificationCategory, NotificationSource};

    fn registry() -> (ActorRegistry, String) {
        let mut registry = ActorRegistry::new();
        let principal = registry.spawn(
            ActorConfig::new("cortex", "Serve user").in_group("main"),
            None,
            true,
        );
        (registry, principal)
    }

    #[test]
    fn dmn_actor_setup_is_idempotent_and_configured() {
        let (mut registry, principal) = registry();
        let first = ensure_dmn_actor(&mut registry, &principal).unwrap();
        let second = ensure_dmn_actor(&mut registry, &principal).unwrap();

        assert_eq!(first, second);
        let actor = registry.get(&first).unwrap();
        assert_eq!(actor.config.name, DMN_ACTOR_NAME);
        assert_eq!(actor.spawned_by, principal);
        assert_eq!(actor.config.model, Some(ModelTier::Aux));
        assert!(actor.config.persistent);
        assert!(actor.config.tools.contains(&"todo_list".to_string()));
    }

    #[test]
    fn dmn_heartbeat_is_queued_to_inbox() {
        let (mut registry, principal) = registry();
        let dmn = ensure_dmn_actor(&mut registry, &principal).unwrap();
        let message_id = queue_dmn_heartbeat_message(
            &mut registry,
            &principal,
            &dmn,
            "heartbeat",
            "- [high] File permit",
        )
        .unwrap();

        let message = registry.pop_inbox(&dmn).unwrap();
        assert_eq!(message.id, message_id);
        assert_eq!(message.sender, principal);
        assert!(message.content.contains("Background heartbeat"));
        assert!(message.content.contains("File permit"));
    }

    #[test]
    fn notification_collection_gates_and_marks_processed_events() {
        let (mut registry, principal) = registry();
        let dmn = ensure_dmn_actor(&mut registry, &principal).unwrap();
        let mut metadata = serde_json::Map::new();
        metadata.insert("signal_category".to_string(), json!("reminder"));
        metadata.insert("signal_urgency".to_string(), json!("high"));
        registry
            .send_to(
                &dmn,
                &principal,
                "Permit letter deadline needs attention.",
                None,
                metadata,
                Some(MessageIntent::Reminder),
            )
            .unwrap();

        registry
            .send_to(
                &dmn,
                &principal,
                "Routine reflection complete.",
                None,
                serde_json::Map::new(),
                Some(MessageIntent::Info),
            )
            .unwrap();

        let mut gate = NotificationGate::new(900);
        let mut processed = HashSet::new();
        let notifications = collect_user_notifications_from_events(
            named_user_notify_events(&registry, 10),
            &mut gate,
            &mut processed,
        );

        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].signal.source, NotificationSource::Dmn);
        assert_eq!(
            notifications[0].signal.category,
            NotificationCategory::Reminder
        );
        assert!(notifications[0].user_message().contains("Permit letter"));
        assert!(
            collect_user_notifications_from_events(
                named_user_notify_events(&registry, 10),
                &mut gate,
                &mut processed,
            )
            .is_empty()
        );
    }

    fn named_user_notify_events(registry: &ActorRegistry, limit: usize) -> Vec<ActorNamedEvent> {
        let mut events = registry
            .events
            .query(Some("user_notify"), None, None, limit.max(1));
        events.reverse();
        events
            .into_iter()
            .map(|event| {
                let actor_name = registry
                    .get(&event.actor_id)
                    .map(|actor| actor.config.name.clone())
                    .unwrap_or_else(|| event.actor_id.clone());
                ActorNamedEvent { event, actor_name }
            })
            .collect()
    }
}
