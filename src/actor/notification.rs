use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::actor::MessageIntent;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationSource {
    Brainstem,
    Dmn,
    Subagent,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationOrigin {
    Startup,
    Heartbeat,
    Background,
    Reflection,
    Task,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationCategory {
    Status,
    Warning,
    Reminder,
    Update,
    Insight,
    Error,
}

impl NotificationCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Warning => "warning",
            Self::Reminder => "reminder",
            Self::Update => "update",
            Self::Insight => "insight",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationUrgency {
    Low,
    Normal,
    High,
    Critical,
}

impl NotificationUrgency {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserNotificationSignal {
    pub event_id: String,
    pub source: NotificationSource,
    pub source_name: String,
    pub source_actor_id: String,
    pub origin: NotificationOrigin,
    pub category: NotificationCategory,
    pub urgency: NotificationUrgency,
    pub content: String,
    pub kind: String,
    pub metadata: serde_json::Map<String, Value>,
    pub observed_at: DateTime<Utc>,
}

impl UserNotificationSignal {
    pub fn new(
        event_id: impl Into<String>,
        source_name: impl Into<String>,
        source_actor_id: impl Into<String>,
        kind: impl Into<String>,
        content: impl Into<String>,
        metadata: serde_json::Map<String, Value>,
    ) -> Self {
        let source_name = source_name.into();
        let kind = kind.into();
        let source = notification_source_from_name(&source_name);
        let origin = notification_origin_from_metadata(&metadata, source, &kind);
        let category = notification_category_from_metadata(&metadata, &kind);
        let urgency = notification_urgency_from_metadata(&metadata, category, &kind);

        Self {
            event_id: event_id.into(),
            source,
            source_name,
            source_actor_id: source_actor_id.into(),
            origin,
            category,
            urgency,
            content: content.into(),
            kind,
            metadata,
            observed_at: Utc::now(),
        }
    }
}

pub fn notification_source_from_name(name: &str) -> NotificationSource {
    match name.trim().to_ascii_lowercase().as_str() {
        "brainstem" => NotificationSource::Brainstem,
        "dmn" => NotificationSource::Dmn,
        "" => NotificationSource::Unknown,
        _ => NotificationSource::Subagent,
    }
}

pub fn notification_origin_from_metadata(
    metadata: &serde_json::Map<String, Value>,
    source: NotificationSource,
    kind: &str,
) -> NotificationOrigin {
    if let Some(origin) =
        metadata_string(metadata, "signal_origin").and_then(|value| parse_origin(&value))
    {
        return origin;
    }

    let kind_lower = kind.to_ascii_lowercase();
    if kind_lower.contains("restart") {
        NotificationOrigin::Startup
    } else if source == NotificationSource::Dmn {
        NotificationOrigin::Reflection
    } else if kind_lower.contains("heartbeat") {
        NotificationOrigin::Heartbeat
    } else {
        NotificationOrigin::Background
    }
}

/// Map a [`MessageIntent`] to the notification category it implies. Returns
/// `None` for variants whose meaning is not a notification (Progress, Done,
/// Info, Message, MaxTurns) so the caller can fall back to extra heuristics.
fn category_from_intent(intent: MessageIntent) -> Option<NotificationCategory> {
    match intent {
        MessageIntent::Alert => Some(NotificationCategory::Warning),
        MessageIntent::Reminder => Some(NotificationCategory::Reminder),
        MessageIntent::Error | MessageIntent::Failed => Some(NotificationCategory::Error),
        MessageIntent::Done | MessageIntent::Progress => Some(NotificationCategory::Update),
        MessageIntent::MaxTurns => Some(NotificationCategory::Warning),
        MessageIntent::Info | MessageIntent::Message => None,
    }
}

pub fn notification_category_from_metadata(
    metadata: &serde_json::Map<String, Value>,
    kind: &str,
) -> NotificationCategory {
    if let Some(category) =
        metadata_string(metadata, "signal_category").and_then(|value| parse_category(&value))
    {
        return category;
    }

    // Primary: ask MessageIntent. Both layers now share the same vocabulary.
    if let Some(category) = category_from_intent(MessageIntent::from_strings("", kind)) {
        return category;
    }

    // Notification-only kinds. These substring checks are intentional here
    // because the notification layer accepts free-form kind strings (e.g.
    // "deadline_reminder", "weekly_update") that don't map to a strict
    // MessageIntent variant.
    let kind_lower = kind.to_ascii_lowercase();
    if kind_lower.contains("reminder") || kind_lower.contains("deadline") {
        NotificationCategory::Reminder
    } else if kind_lower.contains("alert") || kind_lower.contains("warning") {
        NotificationCategory::Warning
    } else if kind_lower.contains("update") {
        NotificationCategory::Update
    } else if kind_lower.contains("insight") || kind_lower.contains("idea") {
        NotificationCategory::Insight
    } else {
        NotificationCategory::Status
    }
}

pub fn notification_urgency_from_metadata(
    metadata: &serde_json::Map<String, Value>,
    category: NotificationCategory,
    kind: &str,
) -> NotificationUrgency {
    if let Some(urgency) =
        metadata_string(metadata, "signal_urgency").and_then(|value| parse_urgency(&value))
    {
        return urgency;
    }

    let kind_lower = kind.to_ascii_lowercase();
    if category == NotificationCategory::Error {
        NotificationUrgency::Critical
    } else if matches!(
        category,
        NotificationCategory::Warning | NotificationCategory::Reminder
    ) {
        NotificationUrgency::High
    } else if category == NotificationCategory::Update {
        NotificationUrgency::Normal
    } else if kind_lower.contains("restart") || category == NotificationCategory::Insight {
        NotificationUrgency::Low
    } else {
        NotificationUrgency::Normal
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NotificationAssessment {
    pub novelty: f32,
    pub urgency: f32,
    pub user_relevance: f32,
    pub interruptibility: f32,
    pub confidence: f32,
}

#[derive(Clone, Debug, Default)]
pub struct NotificationScoring;

impl NotificationScoring {
    pub fn assess(&self, signal: &UserNotificationSignal) -> NotificationAssessment {
        let urgency = match signal.urgency {
            NotificationUrgency::Low => 0.25,
            NotificationUrgency::Normal => 0.55,
            NotificationUrgency::High => 0.82,
            NotificationUrgency::Critical => 0.98,
        };
        let user_relevance = match signal.category {
            NotificationCategory::Status => 0.35,
            NotificationCategory::Insight => 0.30,
            NotificationCategory::Update => 0.62,
            NotificationCategory::Warning => 0.86,
            NotificationCategory::Reminder => 0.88,
            NotificationCategory::Error => 0.95,
        };
        let mut novelty = 0.50_f32;
        if signal.source == NotificationSource::Dmn {
            novelty += 0.05;
        }
        if matches!(
            signal.category,
            NotificationCategory::Warning
                | NotificationCategory::Reminder
                | NotificationCategory::Error
        ) {
            novelty += 0.15;
        }

        let confidence = if signal.metadata.contains_key("signal_category") {
            0.90
        } else {
            0.72
        };
        let interruptibility = f32::min(1.0, (urgency * 0.6) + (user_relevance * 0.4));

        NotificationAssessment {
            novelty: novelty.min(1.0),
            urgency,
            user_relevance,
            interruptibility,
            confidence,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateAction {
    Drop,
    Review,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GateDecision {
    pub action: GateAction,
    pub reason: String,
}

#[derive(Debug)]
pub struct NotificationGate {
    dedupe_window_seconds: i64,
    recent_signatures: HashMap<String, DateTime<Utc>>,
}

impl NotificationGate {
    pub fn new(dedupe_window_seconds: i64) -> Self {
        Self {
            dedupe_window_seconds: dedupe_window_seconds.max(1),
            recent_signatures: HashMap::new(),
        }
    }

    pub fn decide(
        &mut self,
        signal: &UserNotificationSignal,
        assessment: &NotificationAssessment,
    ) -> GateDecision {
        let now = Utc::now();
        self.prune(now);
        let signature = self.signature(signal);
        if self.recent_signatures.contains_key(&signature) {
            return decision(GateAction::Drop, "duplicate_signal");
        }

        if signal.origin == NotificationOrigin::Startup
            && signal.category == NotificationCategory::Status
        {
            self.remember(signature, now);
            return decision(GateAction::Drop, "startup_status_hushed");
        }

        if matches!(
            signal.category,
            NotificationCategory::Status | NotificationCategory::Insight
        ) && signal.urgency == NotificationUrgency::Low
        {
            self.remember(signature, now);
            return decision(GateAction::Drop, "low_priority_status");
        }

        if assessment.interruptibility < 0.58 && assessment.user_relevance < 0.60 {
            self.remember(signature, now);
            return decision(GateAction::Drop, "insufficient_interruptibility");
        }

        self.remember(signature, now);
        decision(GateAction::Review, "needs_review")
    }

    fn signature(&self, signal: &UserNotificationSignal) -> String {
        let mut preview = signal
            .content
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        preview.truncate(160);
        format!(
            "{:?}|{:?}|{:?}|{:?}|{}|{}",
            signal.source,
            signal.origin,
            signal.category,
            signal.urgency,
            signal.kind.to_ascii_lowercase(),
            preview
        )
    }

    fn remember(&mut self, signature: String, now: DateTime<Utc>) {
        self.recent_signatures.insert(signature, now);
    }

    fn prune(&mut self, now: DateTime<Utc>) {
        self.recent_signatures
            .retain(|_, seen_at| (now - *seen_at).num_seconds() <= self.dedupe_window_seconds);
    }
}

fn decision(action: GateAction, reason: &str) -> GateDecision {
    GateDecision {
        action,
        reason: reason.to_string(),
    }
}

fn metadata_string(metadata: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    metadata
        .get(key)?
        .as_str()
        .map(|value| value.trim().to_ascii_lowercase())
}

fn parse_origin(value: &str) -> Option<NotificationOrigin> {
    match value {
        "startup" => Some(NotificationOrigin::Startup),
        "heartbeat" => Some(NotificationOrigin::Heartbeat),
        "background" => Some(NotificationOrigin::Background),
        "reflection" => Some(NotificationOrigin::Reflection),
        "task" => Some(NotificationOrigin::Task),
        _ => None,
    }
}

fn parse_category(value: &str) -> Option<NotificationCategory> {
    match value {
        "status" => Some(NotificationCategory::Status),
        "warning" => Some(NotificationCategory::Warning),
        "reminder" => Some(NotificationCategory::Reminder),
        "update" => Some(NotificationCategory::Update),
        "insight" => Some(NotificationCategory::Insight),
        "error" => Some(NotificationCategory::Error),
        _ => None,
    }
}

fn parse_urgency(value: &str) -> Option<NotificationUrgency> {
    match value {
        "low" => Some(NotificationUrgency::Low),
        "normal" => Some(NotificationUrgency::Normal),
        "high" => Some(NotificationUrgency::High),
        "critical" => Some(NotificationUrgency::Critical),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn signal_metadata_drives_origin_category_and_urgency() {
        let metadata = serde_json::Map::from_iter([
            ("signal_origin".to_string(), json!("task")),
            ("signal_category".to_string(), json!("reminder")),
            ("signal_urgency".to_string(), json!("high")),
        ]);
        let signal = UserNotificationSignal::new(
            "event",
            "worker",
            "actor-1",
            "anything",
            "deadline soon",
            metadata,
        );

        assert_eq!(signal.source, NotificationSource::Subagent);
        assert_eq!(signal.origin, NotificationOrigin::Task);
        assert_eq!(signal.category, NotificationCategory::Reminder);
        assert_eq!(signal.urgency, NotificationUrgency::High);
    }

    #[test]
    fn startup_status_is_dropped_before_review() {
        let signal = UserNotificationSignal::new(
            "event",
            "brainstem",
            "brainstem",
            "restart",
            "Restarted successfully",
            serde_json::Map::new(),
        );
        let scoring = NotificationScoring;
        let assessment = scoring.assess(&signal);
        let mut gate = NotificationGate::new(900);
        let decision = gate.decide(&signal, &assessment);

        assert_eq!(decision.action, GateAction::Drop);
        assert_eq!(decision.reason, "startup_status_hushed");
    }

    #[test]
    fn urgent_reminder_reaches_review_and_duplicates_drop() {
        let signal = UserNotificationSignal::new(
            "event",
            "dmn",
            "dmn",
            "deadline_reminder",
            "A real deadline is approaching.",
            serde_json::Map::new(),
        );
        let scoring = NotificationScoring;
        let assessment = scoring.assess(&signal);
        let mut gate = NotificationGate::new(900);

        assert_eq!(gate.decide(&signal, &assessment).action, GateAction::Review);
        assert_eq!(gate.decide(&signal, &assessment).reason, "duplicate_signal");
    }
}
