# Lethe fork of genai 0.6.5

A vendored fork of [genai](https://github.com/jeremychone/rust-genai) v0.6.5,
applied via `[patch.crates-io]` in the workspace `Cargo.toml`.

## Why fork

Exactly one reason: **per-message `cache_control` on the OpenAI adapter.**

Upstream reads `cache_control` at the *request* level and maps it to OpenAI's
native `prompt_cache_retention` field. It never emits a per-message marker on
the OpenAI path. That does not cover the route Lethe needs:

OpenRouter speaks the OpenAI protocol, but it *relays* per-message
`cache_control` on to whichever vendor actually serves the model (converting
between the Anthropic and OpenAI marker formats). It is how prompt caching
reaches Anthropic, Gemini and Qwen through OpenRouter — and Anthropic is the
only vendor accepting the extended `ttl: "1h"`. Without the marker, an
always-on assistant re-bills its entire system prompt on every turn, which is
operationally catastrophic on input-token cost.

See <https://openrouter.ai/docs/features/prompt-caching>.

## Patch surface (vs upstream 0.6.5)

Four files, one behaviour change:

- `src/adapter/adapters/openai/adapter_shared.rs`: in
  `into_openai_request_parts()`, capture `cache_control` off each message and,
  for system messages carrying one, emit the content as a parts array with a
  `cache_control` field instead of a plain string. Messages without a marker
  are byte-identical to upstream. Direct OpenAI silently drops unknown fields,
  so this is safe on both paths. Covered by the `lethe_fork_*` tests in that
  file's `tests` module.
- `src/adapter/adapters/anthropic/adapter_shared.rs`: `cache_control_to_json()`
  visibility widened from private to `pub(in crate::adapter::adapters)` so the
  OpenAI path can reuse it. No behaviour change. The wire format we emit *is*
  Anthropic's, so both paths must agree on this mapping by construction rather
  than through a copy that can drift.
- `src/adapter/adapters/openai_resp/adapter_impl.rs`: one added test,
  `lethe_fork_agent_turn_on_a_gpt5_reasoning_model`. No production code touched.
  It pins the request shape a Lethe agent turn produces for gpt-5 — the URL,
  `max_output_tokens`, tools surviving, and no `temperature` — since that route
  is the whole reason those models are sent to the Responses API.
- `Cargo.toml`: the published crate's `[[example]]`/`[[test]]` target
  declarations are removed — the `examples/` and `tests/` directories are not
  vendored, and the phantom targets break `cargo test` inside the fork.

Everything else is byte-identical to upstream 0.6.5. To audit the divergence:

```sh
cargo package --list  # or diff src/ against a pristine 0.6.5 extract
```

## What upstream absorbed (do not re-add)

The 0.5.3 fork carried four more patches. All are now unnecessary — this is
recorded so nobody re-applies them:

- **1h cache TTL.** `CacheControl::Ephemeral1h` is upstream and emits
  `{"type": "ephemeral", "ttl": "1h"}` on the Anthropic path, identical to the
  old `CacheControl::Persistent`. Lethe maps `CacheHint::Persistent` to it in
  `cache_hint_to_genai()`.
- **`max_completion_tokens`.** Upstream picks the right key for gpt-5 / o-series
  on Chat Completions.
- **`temperature`/`top_p` dropped for reasoning models.** Moved *out* of the
  adapter and into Lethe's `LlmRouterConfig::chat_options()`. Upstream still
  emits `temperature` unconditionally, so the rule is still needed — it just
  belongs to the caller. An adapter that silently swaps a caller's `0.7` for the
  default is lying about what it sent; only the caller knows the request is
  headed for a model that cannot honour it.
- **`reasoning_effort: "none"` when tools are present.** This was an interim
  unblock for gpt-5.x on Chat Completions, which rejects function tools combined
  with any non-`"none"` reasoning effort. It bought tools at the cost of
  reasoning. It is obsolete: upstream 0.6.5 ships a real Responses streamer
  (`openai_resp/streamer.rs`), and Lethe now routes direct-OpenAI gpt-5
  reasoning models to `AdapterKind::OpenAIResp` (see `adapter_for()` in
  `src/llm/client.rs`). `/v1/responses` supports tools *and* reasoning together.

## Tracking upstream

The remaining patch is a genuine upstream gap, not a workaround — worth filing
as a feature request (per-message `cache_control` passthrough on the OpenAI
adapter, for OpenRouter-style relays). If upstream takes it, drop this fork and
depend on the released crate.

Note that upstream's `AdapterKind::from_model` already routes `gpt-5*` to
`OpenAIResp` on its own; Lethe's `adapter_for()` only matters because Lethe
pins an explicit adapter in its `ServiceTargetResolver`, which bypasses that
inference.

See <https://github.com/jeremychone/rust-genai> for issues.
