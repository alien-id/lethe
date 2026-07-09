//! Unified event vocabulary the TUI loop consumes. The SSE client adapts
//! `event/data` SSE frames into these; future in-process subscribers will
//! produce the same enum so the renderer stays transport-agnostic.

use serde_json::Value;

#[derive(Clone, Debug)]
pub enum UiEvent {
    /// Connection state. Render shows a footer indicator while disconnected.
    Connected,
    Disconnected(String),
    /// Final assistant message text (markdown).
    AssistantText(String),
    /// Streaming delta; appends to the in-progress assistant message.
    AssistantDelta(String),
    TypingStart,
    TypingStop,
    TurnStart,
    TurnDone,
    ToolStart {
        call_id: String,
        name: String,
        args_preview: String,
    },
    ToolEnd {
        call_id: String,
        name: String,
        success: bool,
        output_preview: String,
        duration_ms: u64,
    },
    ActorEvent {
        kind: String,
        actor_id: String,
        payload: Value,
    },
    /// A new insight/activity ledger row was recorded; the sidebar refreshes
    /// its activity list and unseen badge.
    ActivityLogged,
    /// `Usage { prompt_tokens }` powers the footer's context indicator.
    Usage {
        prompt_tokens: u64,
    },
    Reaction {
        emoji: String,
    },
    /// Catch-all so unknown SSE event names are still visible in logs without
    /// crashing the parser.
    Unknown {
        event: String,
        data: Value,
    },
}

/// User -> background driver commands.
#[derive(Clone, Debug)]
pub enum AppCommand {
    SendMessage(String),
    Cancel,
    RefreshActors,
    RefreshTodos,
    RefreshActivity,
    /// Persist that the user viewed these ledger rows (detail opened).
    MarkActivitySeen(Vec<i64>),
    SwitchModel(String),
    Quit,
}
