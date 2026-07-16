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

/// Providers that need explicit cache breakpoints but do not honour an
/// extended TTL — Google Gemini and Alibaba Qwen, reached via OpenRouter.
/// Both halves get the default 5-minute marker; only Anthropic accepts `1h`.
pub struct ExplicitCacheDialect;

impl PromptDialect for ExplicitCacheDialect {
    fn cache_marker_for_stable(&self) -> Option<CacheHint> {
        Some(CacheHint::Ephemeral)
    }
    fn cache_marker_for_volatile(&self) -> Option<CacheHint> {
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

/// Dialect for a model served through OpenRouter, keyed off the vendor segment
/// of `openrouter/<vendor>/<model>`.
///
/// OpenRouter relays `cache_control` to the upstream provider rather than
/// stripping it, and converts between the Anthropic and OpenAI marker formats.
/// So the dialect follows whoever actually serves the model:
///
/// - `anthropic` — explicit breakpoints, and the only vendor accepting the
///   extended `ttl: "1h"` marker.
/// - `google` (Gemini) and `qwen` (Alibaba) — explicit breakpoints, 5m only.
/// - Everyone else (`openai`, `x-ai`, `moonshotai`, `z-ai`, `deepseek`, …) —
///   automatic prefix caching, so a marker buys nothing.
///
/// Returns `None` for vendors that cache automatically.
///
/// See <https://openrouter.ai/docs/features/prompt-caching>.
fn openrouter_dialect(model_id: &str) -> Option<Box<dyn PromptDialect>> {
    let lower = model_id.trim().to_ascii_lowercase();
    let vendor = lower.strip_prefix("openrouter/")?.split('/').next()?;
    match vendor {
        "anthropic" => Some(Box::new(ClaudeDialect)),
        "google" | "qwen" => Some(Box::new(ExplicitCacheDialect)),
        _ => None,
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
        // OpenRouter is a relay, and it forwards cache_control to the upstream
        // provider rather than dropping it — so the dialect is decided by the
        // vendor actually serving the model, not by OpenRouter itself.
        Some("openrouter") => openrouter_dialect(model_id).unwrap_or_else(|| Box::new(DefaultDialect)),
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
    fn openrouter_claude_gets_the_extended_ttl_marker() {
        // OpenRouter forwards cache_control upstream, and Anthropic is the only
        // vendor accepting the 1h TTL. Every model id here is one we ship in
        // config/model_context_limits.json.
        for model in [
            "openrouter/anthropic/claude-opus-4.7",
            "openrouter/anthropic/claude-opus-4.6",
            "openrouter/anthropic/claude-sonnet-4.6",
            "openrouter/anthropic/claude-haiku-4.5",
        ] {
            let dialect = dialect_for_model(model);
            assert_eq!(
                dialect.cache_marker_for_stable(),
                Some(CacheHint::Persistent),
                "{model} must cache its stable prefix for 1h"
            );
            assert_eq!(
                dialect.cache_marker_for_volatile(),
                Some(CacheHint::Ephemeral),
                "{model} must cache its volatile tail for 5m"
            );
        }
    }

    #[test]
    fn openrouter_gemini_and_qwen_get_explicit_breakpoints_without_extended_ttl() {
        // Both need explicit markers, but 1h TTL is Anthropic-only.
        for model in [
            "openrouter/google/gemini-3.1-pro-preview",
            "openrouter/google/gemini-3-flash-preview",
            "openrouter/qwen/qwen3.5-plus-02-15",
            "openrouter/qwen/qwen3.5-flash-02-23",
        ] {
            let dialect = dialect_for_model(model);
            assert_eq!(
                dialect.cache_marker_for_stable(),
                Some(CacheHint::Ephemeral),
                "{model} needs an explicit breakpoint"
            );
            assert_eq!(dialect.cache_marker_for_volatile(), Some(CacheHint::Ephemeral));
        }
    }

    #[test]
    fn openrouter_auto_caching_vendors_get_no_markers() {
        // These cache automatically; a breakpoint buys nothing and costs
        // bookkeeping. Z.AI (GLM), Moonshot/Kimi, Grok and OpenAI are all
        // documented as automatic.
        for model in [
            "openrouter/moonshotai/kimi-k2.6",
            "openrouter/z-ai/glm-5.1",
            "openrouter/x-ai/grok-4.20-beta",
            "openrouter/openai/gpt-5.4",
            "openrouter/xiaomi/mimo-v2-flash",
        ] {
            let dialect = dialect_for_model(model);
            assert!(
                dialect.cache_marker_for_stable().is_none(),
                "{model} caches automatically and must not get a marker"
            );
        }
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
