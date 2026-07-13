# Lethe fork of genai 0.5.3

This is a vendored fork of [genai](https://github.com/jeremychone/rust-genai)
v0.5.3, applied via `[patch.crates-io]` in the workspace `Cargo.toml`.

## Why fork

Upstream `genai::chat::CacheControl::Ephemeral` translates to Anthropic's
`{"type": "ephemeral"}` with the default 5-minute TTL. For an always-on
assistant where user turns can be 5–60 minutes apart, this means the stable
prefix of the system prompt misses cache on most follow-up turns, which is
operationally catastrophic on input-token cost.

This fork adds `CacheControl::Persistent` which translates to
`{"type": "ephemeral", "ttl": "1h"}` (Anthropic's extended cache TTL).
Identity, persona, and instruction blocks — which change rarely — mark
themselves Persistent and survive the gap between user replies.

## Patch surface (vs upstream 0.5.3)

Four files are modified:

- `src/chat/chat_message.rs`: add `Persistent` variant to `CacheControl`.
- `src/adapter/adapters/anthropic/adapter_impl.rs`: stamp `ttl: "1h"` on
  the emitted `cache_control` object when the marker is `Persistent`.
  Four emission sites updated; `Ephemeral` behaviour is unchanged.
- `src/adapter/adapters/openai/adapter_impl.rs`: two patches.
  1. Forward `cache_control` on system messages as a content-parts array,
     mirroring OpenRouter's documented extension. Direct OpenAI silently
     drops unknown fields, so this is safe on both paths and unlocks
     caching for OpenRouter → Anthropic / Moonshot / Gemini routes that
     would otherwise re-bill the full prompt every turn.
  2. Reasoning-era Chat Completions params for direct OpenAI
     (`AdapterKind::OpenAI` only): o-series and gpt-5-family models reject
     `max_tokens` (400 — they require `max_completion_tokens`) and reject
     any non-default `temperature`/`top_p`. For those models the adapter
     now emits `max_completion_tokens` and drops `temperature`/`top_p`;
     `gpt-5-chat*` variants keep sampling params but still get
     `max_completion_tokens`. Upstream 0.5.3 sent `max_tokens` +
     `temperature` unconditionally, which made every gpt-5.x / o-series
     turn fail with a 400. See `openai_reasoning_era_rules()` and the
     `reasoning_era_param_tests` module.
- `Cargo.toml`: the published crate's `[[example]]`/`[[test]]` target
  declarations are removed — the `examples/` and `tests/` directories were
  not vendored, and the phantom targets broke `cargo test` inside the fork.

All other code is byte-identical to upstream 0.5.3.

## Tracking upstream

If/when upstream adds TTL support, drop this fork and depend on the
released crate. See https://github.com/jeremychone/rust-genai for issues.
