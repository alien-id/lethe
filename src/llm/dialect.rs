//! Per-model prompt dialects. Mirrors the Python `ContextAssembler` plugin
//! system that was on `main`: each model family controls cache markers and
//! whether the auto-generated tool directory should be embedded in the prompt
//! text. Routing happens via [`provider_for_model`].
//!
//! Add a new dialect by implementing [`PromptDialect`] and matching it inside
//! [`dialect_for_model`].

use crate::llm::CacheHint;
use crate::llm::models::{protocol_for_model, provider_for_model};

/// Behaviour knobs the system-prompt assembler queries per turn. Currently
/// the only differentiator we need across families is whether to attach
/// cache markers to the stable/volatile system parts; more knobs can be
/// added here (e.g. preferred output format, markdown vs XML structuring,
/// extended-thinking opt-out) when concrete models demand them.
pub trait PromptDialect: Send + Sync {
    /// Cache marker for the long-stable head of the system prompt (identity,
    /// persona, instructions, stable memory blocks). Anthropic respects this
    /// to land an ephemeral cache breakpoint; other providers ignore it.
    fn cache_marker_for_stable(&self) -> Option<CacheHint>;

    /// Cache marker for the per-turn-volatile tail. Useful when rapid
    /// follow-up turns leave the volatile content unchanged within the
    /// cache's TTL.
    fn cache_marker_for_volatile(&self) -> Option<CacheHint>;
}

/// Anthropic Claude family. Supports prompt caching, benefits from XML-shaped
/// prompts.
pub struct ClaudeDialect;

impl PromptDialect for ClaudeDialect {
    fn cache_marker_for_stable(&self) -> Option<CacheHint> {
        // 1-hour TTL: identity, persona, instructions change rarely. The
        // stable prefix survives between user replies on an always-on
        // assistant, which is the whole point of prompt caching for us.
        Some(CacheHint::Persistent)
    }
    fn cache_marker_for_volatile(&self) -> Option<CacheHint> {
        // 5-minute TTL: memory state, clock, recall change often. Still
        // worth caching for rapid back-and-forth within a conversation.
        Some(CacheHint::Ephemeral)
    }
}

/// Baseline for every other provider — no cache markers (providers either
/// ignore them or use a separate caching API).
pub struct DefaultDialect;

impl PromptDialect for DefaultDialect {
    fn cache_marker_for_stable(&self) -> Option<CacheHint> {
        None
    }
    fn cache_marker_for_volatile(&self) -> Option<CacheHint> {
        None
    }
}

/// Pick the dialect for a given model id. Routing is by provider (anthropic →
/// Claude, anything else → Default) — explicit model_id matching for finer
/// distinctions can be layered on later.
pub fn dialect_for_model(model_id: &str) -> Box<dyn PromptDialect> {
    match provider_for_model(model_id) {
        Some("anthropic") => Box::new(ClaudeDialect),
        // OpenCode Go models using the Anthropic wire protocol get Claude
        // cache markers; OpenAI-protocol models use the default dialect.
        Some("opencode-go") => match protocol_for_model(model_id) {
            "anthropic" => Box::new(ClaudeDialect),
            _ => Box::new(DefaultDialect),
        },
        // OpenRouter is a relay — the underlying model could be Claude, but
        // OpenRouter strips the cache_control hint anyway. Treat as default.
        _ => Box::new(DefaultDialect),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_family_gets_cache_markers() {
        let dialect = dialect_for_model("claude-opus-4-7");
        assert!(dialect.cache_marker_for_stable().is_some());
        assert!(dialect.cache_marker_for_volatile().is_some());
    }

    #[test]
    fn gpt_family_uses_default_dialect() {
        let dialect = dialect_for_model("gpt-5");
        assert!(dialect.cache_marker_for_stable().is_none());
    }

    #[test]
    fn openrouter_falls_back_to_default() {
        let dialect = dialect_for_model("openrouter/anthropic/claude-opus-4");
        assert!(dialect.cache_marker_for_stable().is_none());
    }

    #[test]
    fn opencode_go_anthropic_protocol_gets_claude_dialect() {
        let dialect = dialect_for_model("opencode-go/qwen3.7-max");
        assert!(
            dialect.cache_marker_for_stable().is_some(),
            "opencode-go Anthropic-protocol model should get ClaudeDialect with cache markers"
        );
    }

    #[test]
    fn opencode_go_openai_protocol_uses_default_dialect() {
        let dialect = dialect_for_model("opencode-go/kimi-k2.6");
        assert!(
            dialect.cache_marker_for_stable().is_none(),
            "opencode-go OpenAI-protocol model should use DefaultDialect (no cache markers)"
        );
    }
}
