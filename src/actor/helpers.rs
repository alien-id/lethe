use uuid::Uuid;

use super::{Actor, ActorInfo, ActorState, MessageIntent, ModelTier, TaskState};

pub(super) fn short_id() -> String {
    Uuid::new_v4().to_string()[..8].to_string()
}

pub(super) fn parse_task_state(value: &str) -> Option<TaskState> {
    match value.trim().to_ascii_lowercase().as_str() {
        "planned" => Some(TaskState::Planned),
        "running" => Some(TaskState::Running),
        "blocked" => Some(TaskState::Blocked),
        "done" => Some(TaskState::Done),
        _ => None,
    }
}

pub(super) fn valid_task_transition(from: TaskState, to: TaskState) -> bool {
    matches!(
        (from, to),
        (TaskState::Planned, TaskState::Running)
            | (TaskState::Planned, TaskState::Blocked)
            | (TaskState::Planned, TaskState::Done)
            | (TaskState::Running, TaskState::Running)
            | (TaskState::Running, TaskState::Blocked)
            | (TaskState::Running, TaskState::Done)
            | (TaskState::Blocked, TaskState::Blocked)
            | (TaskState::Blocked, TaskState::Running)
            | (TaskState::Blocked, TaskState::Done)
            | (TaskState::Done, TaskState::Done)
    )
}

pub(super) fn state_name(state: TaskState) -> &'static str {
    match state {
        TaskState::Planned => "planned",
        TaskState::Running => "running",
        TaskState::Blocked => "blocked",
        TaskState::Done => "done",
    }
}

pub(super) fn actor_state_name(state: ActorState) -> &'static str {
    match state {
        ActorState::Initializing => "initializing",
        ActorState::Running => "running",
        ActorState::Waiting => "waiting",
        ActorState::Terminated => "terminated",
    }
}

pub(super) fn intent_name(intent: MessageIntent) -> &'static str {
    match intent {
        MessageIntent::Progress => "progress",
        MessageIntent::Done => "done",
        MessageIntent::Failed => "failed",
        MessageIntent::Error => "error",
        MessageIntent::MaxTurns => "max_turns",
        MessageIntent::Alert => "alert",
        MessageIntent::Reminder => "reminder",
        MessageIntent::Info => "info",
        MessageIntent::Message => "message",
    }
}

pub(super) fn intent_model_name(model: ModelTier) -> &'static str {
    match model {
        ModelTier::Main => "main",
        ModelTier::Aux => "aux",
    }
}

pub(super) fn parse_model_tier(value: &str) -> Option<ModelTier> {
    match value.trim().to_ascii_lowercase().as_str() {
        "main" => Some(ModelTier::Main),
        "aux" | "" => Some(ModelTier::Aux),
        _ => None,
    }
}

pub(super) fn split_tool_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|tool| !tool.is_empty())
        .map(str::to_string)
        .collect()
}

pub(super) fn normalize_actor_name(value: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in value.trim().to_ascii_lowercase().chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' {
            out.push(ch);
            last_dash = false;
        } else if (ch.is_ascii_whitespace() || ch == '_') && !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

pub(super) fn relationship_label(actor: &Actor, info: &ActorInfo) -> &'static str {
    if info.spawned_by == actor.id {
        " [child]"
    } else if info.id == actor.spawned_by {
        " [parent]"
    } else if !actor.spawned_by.is_empty() && info.spawned_by == actor.spawned_by {
        " [sibling]"
    } else {
        ""
    }
}

pub(super) fn format_active_children(children: &[Actor]) -> String {
    if children.is_empty() {
        return String::new();
    }
    let mut lines = vec![format!("\n\nActive children ({}):", children.len())];
    for child in children {
        lines.push(format!(
            "  - {} (id={}, state={}): {}",
            child.config.name,
            child.id,
            actor_state_name(child.state),
            truncate_chars(&child.config.goals, 300)
        ));
    }
    lines.join("\n")
}

pub(super) fn truncate_chars(value: &str, limit: usize) -> String {
    crate::llm::truncate::truncate_with_ellipsis(value, limit)
}

pub(super) fn format_age(age: chrono::Duration) -> String {
    let minutes = age.num_minutes().max(0);
    if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 60 * 24 {
        format!("{}h{}m", minutes / 60, minutes % 60)
    } else {
        format!("{}d{}h", minutes / (60 * 24), (minutes % (60 * 24)) / 60)
    }
}
