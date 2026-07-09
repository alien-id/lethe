use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::broadcast;

mod activity_log;
pub mod background;
mod helpers;
pub mod notification;
mod persistence;
mod registry;
mod runtime;

pub use activity_log::{ActivityKind, ActivityLog, ActivityRow, NewActivity, compact_summary};
pub use persistence::ActorStore;
pub use registry::ActorRegistry;
pub use runtime::{
    ActorNamedEvent, ActorRuntime, ActorSupervisor, ActorToolCommand, SpawnSubagent,
};

use helpers::short_id;

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    Main,
    Aux,
}

pub type ActorTurnFuture = Pin<Box<dyn Future<Output = ActorResult<String>> + Send + 'static>>;
pub type ActorTurnExecutor =
    Arc<dyn Fn(ActorRunSpec, ActorRuntime) -> ActorTurnFuture + Send + Sync + 'static>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorState {
    Initializing,
    Running,
    Waiting,
    Terminated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Planned,
    Running,
    Blocked,
    Done,
}

/// Why an actor stopped running. Set on every termination so callers (the
/// chain runner, the parent-notification path, the user-facing actor info)
/// never have to parse the result text to learn the verdict.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Success,
    Failure,
    Partial,
    Killed,
    MaxTurns,
    Restarted,
    SystemShutdown,
}

impl Outcome {
    pub fn parse_terminate_arg(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "success" | "" => Self::Success,
            "failure" | "failed" | "error" => Self::Failure,
            "partial" => Self::Partial,
            _ => Self::Success,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Partial => "partial",
            Self::Killed => "killed",
            Self::MaxTurns => "max_turns",
            Self::Restarted => "restarted",
            Self::SystemShutdown => "system_shutdown",
        }
    }

    pub fn is_failed(self) -> bool {
        matches!(
            self,
            Self::Failure | Self::Killed | Self::MaxTurns | Self::SystemShutdown
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageIntent {
    Progress,
    Done,
    Failed,
    Error,
    MaxTurns,
    Alert,
    Reminder,
    Info,
    Message,
}

impl MessageIntent {
    pub fn wakes_cortex(self) -> bool {
        !matches!(self, Self::Info | Self::Message)
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Done | Self::Failed | Self::Error | Self::MaxTurns
        )
    }

    pub fn channel(self) -> &'static str {
        if matches!(self, Self::Alert | Self::Reminder | Self::Info) {
            "user_notify"
        } else if self.is_terminal() || matches!(self, Self::Progress) {
            "task_update"
        } else {
            ""
        }
    }

    pub fn from_strings(channel: &str, kind: &str) -> Self {
        match kind.trim().to_ascii_lowercase().as_str() {
            "progress" => Self::Progress,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "error" => Self::Error,
            "max_turns" => Self::MaxTurns,
            "alert" | "warning" => Self::Alert,
            "reminder" | "deadline" | "update_ready" => Self::Reminder,
            "info" => Self::Info,
            "message" => Self::Message,
            "" if channel.trim() == "task_update" => Self::Progress,
            _ => Self::Info,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActorMessage {
    pub id: String,
    pub sender: String,
    pub recipient: String,
    pub content: String,
    pub reply_to: Option<String>,
    pub intent: MessageIntent,
    pub metadata: serde_json::Map<String, Value>,
    pub created_at: DateTime<Utc>,
}

impl ActorMessage {
    pub fn new(
        sender: impl Into<String>,
        recipient: impl Into<String>,
        content: impl Into<String>,
        intent: MessageIntent,
    ) -> Self {
        Self {
            id: short_id(),
            sender: sender.into(),
            recipient: recipient.into(),
            content: content.into(),
            reply_to: None,
            intent,
            metadata: serde_json::Map::new(),
            created_at: Utc::now(),
        }
    }

    pub fn format_for_context(&self) -> String {
        let reply = self
            .reply_to
            .as_deref()
            .map(|reply_to| format!(" (reply to {reply_to})"))
            .unwrap_or_default();
        format!(
            "[{}] {}{}: {}",
            self.created_at.format("%a %Y-%m-%d %H:%M:%S UTC"),
            self.sender,
            reply,
            self.content
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActorEvent {
    pub id: String,
    pub event_type: String,
    pub actor_id: String,
    pub group: String,
    pub payload: serde_json::Map<String, Value>,
    pub created_at: DateTime<Utc>,
}

impl ActorEvent {
    pub fn new(event_type: impl Into<String>, actor_id: impl Into<String>) -> Self {
        Self {
            id: short_id(),
            event_type: event_type.into(),
            actor_id: actor_id.into(),
            group: String::new(),
            payload: serde_json::Map::new(),
            created_at: Utc::now(),
        }
    }
}

#[derive(Debug)]
pub struct ActorEventBus {
    max_events: usize,
    events: VecDeque<ActorEvent>,
    /// Optional fan-out for live subscribers (e.g. the HTTP API streaming
    /// actor lifecycle to a TUI). Attached via `set_broadcaster` after the
    /// agent runtime is wired up. Bus stays usable when no broadcaster is
    /// installed, which is the case in CLI subcommands and tests.
    broadcaster: Option<broadcast::Sender<ActorEvent>>,
}

impl ActorEventBus {
    pub fn new(max_events: usize) -> Self {
        Self {
            max_events: max_events.max(1),
            events: VecDeque::new(),
            broadcaster: None,
        }
    }

    pub fn set_broadcaster(&mut self, sender: broadcast::Sender<ActorEvent>) {
        self.broadcaster = Some(sender);
    }

    pub fn emit(&mut self, event: ActorEvent) {
        if let Some(sender) = &self.broadcaster {
            let _ = sender.send(event.clone());
        }
        self.events.push_back(event);
        while self.events.len() > self.max_events {
            self.events.pop_front();
        }
    }

    pub fn query(
        &self,
        event_type: Option<&str>,
        actor_id: Option<&str>,
        group: Option<&str>,
        limit: usize,
    ) -> Vec<ActorEvent> {
        self.events
            .iter()
            .rev()
            .filter(|event| event_type.is_none_or(|value| event.event_type == value))
            .filter(|event| actor_id.is_none_or(|value| event.actor_id == value))
            .filter(|event| group.is_none_or(|value| event.group == value))
            .take(limit.max(1))
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActorConfig {
    pub name: String,
    pub group: String,
    pub goals: String,
    pub model: Option<ModelTier>,
    pub tools: Vec<String>,
    pub max_turns: usize,
    pub max_messages: usize,
    pub persistent: bool,
}

impl ActorConfig {
    pub fn new(name: impl Into<String>, goals: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            group: "default".to_string(),
            goals: goals.into(),
            model: None,
            tools: Vec::new(),
            max_turns: 20,
            max_messages: 50,
            persistent: false,
        }
    }

    pub fn in_group(mut self, group: impl Into<String>) -> Self {
        self.group = group.into();
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActorInfo {
    pub id: String,
    pub name: String,
    pub group: String,
    pub goals: String,
    pub state: ActorState,
    pub task_state: TaskState,
    pub spawned_by: String,
    pub result: Option<String>,
    pub outcome: Option<Outcome>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActorSpawnRequest<'a> {
    pub name: &'a str,
    pub goals: &'a str,
    pub group: Option<&'a str>,
    pub tools: &'a str,
    pub model: &'a str,
    pub max_turns: usize,
}

/// Typed result of a spawn attempt. Callers that need the new actor id read
/// `Spawned.actor_id` directly instead of grepping the formatted message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpawnReport {
    Spawned { actor_id: String, message: String },
    Rejected { message: String },
}

impl SpawnReport {
    pub fn message(&self) -> &str {
        match self {
            Self::Spawned { message, .. } | Self::Rejected { message } => message,
        }
    }

    pub fn actor_id(&self) -> Option<&str> {
        match self {
            Self::Spawned { actor_id, .. } => Some(actor_id.as_str()),
            Self::Rejected { .. } => None,
        }
    }
}

impl ActorInfo {
    pub fn format(&self) -> String {
        format!(
            "- {} (id={}, state={:?}, task={:?}): {}",
            self.name, self.id, self.state, self.task_state, self.goals
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActorRunSpec {
    pub actor_id: String,
    pub name: String,
    pub system_prompt: String,
    pub turn_number: usize,
    pub max_turns: usize,
    pub model: ModelTier,
    pub has_pending_messages: bool,
    pub requested_tools: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Actor {
    pub id: String,
    pub config: ActorConfig,
    pub spawned_by: String,
    pub is_principal: bool,
    pub state: ActorState,
    pub task_state: TaskState,
    pub created_at: DateTime<Utc>,
    pub terminated_at: Option<DateTime<Utc>>,
    pub(crate) outcome: Option<Outcome>,
    result: Option<String>,
    messages: Vec<ActorMessage>,
    inbox: VecDeque<ActorMessage>,
    turn_count: usize,
    task_state_note: String,
    task_state_updated_at: Option<DateTime<Utc>>,
}

impl Actor {
    fn new(config: ActorConfig, spawned_by: Option<&str>, is_principal: bool) -> Self {
        Self {
            id: short_id(),
            config,
            spawned_by: spawned_by.unwrap_or_default().to_string(),
            is_principal,
            state: ActorState::Initializing,
            task_state: TaskState::Planned,
            created_at: Utc::now(),
            terminated_at: None,
            outcome: None,
            result: None,
            messages: Vec::new(),
            inbox: VecDeque::new(),
            turn_count: 0,
            task_state_note: String::new(),
            task_state_updated_at: None,
        }
    }

    pub fn info(&self) -> ActorInfo {
        ActorInfo {
            id: self.id.clone(),
            name: self.config.name.clone(),
            group: self.config.group.clone(),
            goals: self.config.goals.clone(),
            state: self.state,
            task_state: self.task_state,
            spawned_by: self.spawned_by.clone(),
            result: self.result.clone(),
            outcome: self.outcome,
        }
    }

    pub fn result(&self) -> Option<&str> {
        self.result.as_deref()
    }

    pub fn messages(&self) -> &[ActorMessage] {
        &self.messages
    }

    pub fn turn_count(&self) -> usize {
        self.turn_count
    }

    pub fn task_state_note(&self) -> &str {
        &self.task_state_note
    }

    pub fn task_state_updated_at(&self) -> Option<DateTime<Utc>> {
        self.task_state_updated_at
    }

    pub fn has_pending_messages(&self) -> bool {
        !self.inbox.is_empty()
    }

    pub fn recent_messages(&self, limit: usize, include_self: bool) -> Vec<ActorMessage> {
        self.messages
            .iter()
            .filter(|message| include_self || message.sender != self.id)
            .rev()
            .take(limit)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
}

#[derive(Debug, Error)]
pub enum ActorError {
    #[error("actor {0} not found")]
    NotFound(String),
    #[error("actor {sender} cannot message actor {recipient}")]
    PermissionDenied { sender: String, recipient: String },
    #[error("invalid task state: {0}")]
    InvalidTaskState(String),
    #[error("invalid transition: {from:?} -> {to:?}")]
    InvalidTaskTransition { from: TaskState, to: TaskState },
    #[error("invalid actor config: {0}")]
    InvalidActorConfig(String),
    #[error("actor runtime error: {0}")]
    Runtime(String),
}

pub type ActorResult<T> = Result<T, ActorError>;
