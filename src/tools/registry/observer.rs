use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub type BoxToolFuture<'a> = Pin<Box<dyn Future<Output = String> + Send + 'a>>;

/// Transport-side hook called around each tool execution. Lets a transport
/// (e.g. Telegram) keep its "typing" indicator alive for the duration of a
/// long tool call without the agent loop having to know anything about it.
///
/// `on_tool_start` / `on_tool_end` are no-ops by default; the TUI/HTTP API
/// implements them to surface tool execution to clients as SSE events.
pub trait TurnObserver: Send + Sync {
    fn wrap_tool_call<'a>(&'a self, name: &'a str, inner: BoxToolFuture<'a>) -> BoxToolFuture<'a>;

    fn on_tool_start(&self, _name: &str, _call_id: &str, _args_preview: &str) {}

    fn on_tool_end(
        &self,
        _name: &str,
        _call_id: &str,
        _success: bool,
        _output_preview: &str,
        _duration_ms: u128,
    ) {
    }

    /// Streamed assistant token chunk. The default impl does nothing so
    /// non-streaming transports (Telegram) don't pay any cost.
    fn on_assistant_delta(&self, _content: &str) {}

    /// Streamed reasoning/thinking token chunk, separate from the visible
    /// answer. Lets clients render a live "thinking…" indicator. Default
    /// impl does nothing.
    fn on_reasoning_delta(&self, _reasoning: &str) {}
}

/// Convenience alias used inside `ToolRuntime`.
pub type SharedTurnObserver = Arc<dyn TurnObserver>;

/// Factory producing a per-turn observer for a subagent turn. A host installs
/// one via [`crate::agent::Agent::install_subagent_observer`]; the actor turn
/// executor calls it with the acting actor's id at the start of every
/// subagent turn, so the host can attribute the resulting tool/reasoning
/// events to that actor. Without an installed factory subagent turns run
/// unobserved, exactly as before.
pub type SubagentObserverFactory = Arc<dyn Fn(&str) -> SharedTurnObserver + Send + Sync>;

/// Shared slot holding the optional [`SubagentObserverFactory`]. Created at
/// agent build time and captured by the actor turn executor, so a factory
/// installed after startup applies to every subsequent subagent turn.
pub type SubagentObserverSlot = Arc<std::sync::RwLock<Option<SubagentObserverFactory>>>;
