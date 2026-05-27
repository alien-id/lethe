# Changelog

## 0.20.6 - Subscription OAuth + OpenRouter prompt-cache fix

- **OpenAI ChatGPT Plus/Pro OAuth** (`lethe login openai`). Device-code flow against `auth.openai.com`; tokens persist to `~/.lethe/credentials/openai_oauth_tokens.json` with auto-refresh ≥60s before expiry. Calls route to the Codex Responses API at `chatgpt.com/backend-api/codex/responses` with full tool-call parity (function_call / function_call_output items) and an SSE response translator. Override the token file with `LETHE_OPENAI_OAUTH_TOKENS`; supply a raw token with `OPENAI_AUTH_TOKEN`.
- **Anthropic Pro/Max OAuth login** (`lethe login anthropic`). PKCE browser flow at `claude.ai/oauth/authorize`; tokens persist to `~/.lethe/credentials/anthropic_oauth_tokens.json` and feed the existing OAuth client.
- **OpenRouter API-key login** (`lethe login openrouter`). Prompts for `OPENROUTER_API_KEY`, sets it in `.env`. Model prompts strip the `openrouter/` prefix from displayed defaults and re-prefix the user's input automatically.
- **Subscription-vs-API choice** on `lethe login openai` / `lethe login anthropic`. Each opens with a `[1] subscription (default) [2] API key` prompt and dispatches accordingly. After auth, the user is prompted for `LLM_MODEL` and `LLM_MODEL_AUX` with the catalog's first entry as the default.
- **OpenRouter prompt caching now works**. Vendored genai's OpenAI adapter forwards `cache_control` markers as content-parts arrays — OpenRouter routes them to upstream providers that support explicit caching (Anthropic, Qwen, Gemini explicit). Before this fix, every OpenRouter call re-billed the full prompt.
- **Anthropic OAuth path now honors cache_control** (`src/llm/client.rs::anthropic_request_body`). The OAuth client was rebuilding the JSON body manually and silently dropping the `Persistent` / `Ephemeral` markers `apply_cache_markers` sets upstream. Heartbeat token use dropped substantially after this.
- **Heartbeat idle gate** (`src/cli/telegram_loop.rs`): skip both cortex `chat_once` and DMN queue when no reminders are due, it isn't the first tick, and it isn't a periodic full-context tick. First-tick, full-context, and reminder-bearing ticks always proceed.
- **Curator summarization cadence gate** (`src/scheduler/curator.rs`): `summarize_completed_entries` was firing up to `COMPLETION_SUMMARY_BATCH` aux-LLM calls per heartbeat / per chat turn. Now gated to once per hour via a new `last_summary_at` field on `CuratorState`.
- **DMN reflection leak fix** (`src/actor/runtime.rs::PrincipalTaskUpdateEvents`). DMN's `task_update` channel messages were waking cortex via the actor-update monitor, which then parroted the verbose reflection back to Telegram. The supervisor now filters `actor_message` events whose sender is the DMN actor; user-facing signals still flow through `user_notify`.
- **Migrator correctness** (`migrator/`):
  - Backfill `note-<uuid>` prefix on legacy note ids so the live writer's id-format invariant holds.
  - Normalize note tags through trim + lowercase + dedupe to match the live `clean_tags` contract — without this, migrated mixed-case or duplicate tags silently failed to match the live tag filter.
  - Treat empty `updated_at` as `NULL` instead of `""` (column is nullable; live reader expects `Option<String>`).
  - Surface init-count predicate errors instead of swallowing them with `unwrap_or(0)`, which would inflate the expected user-row target and produce a misleading verification failure.
  - Extend verification's vector check to the full embedding length (was first 4 dims).
- **Model catalog refresh** (`config/model_catalog.json`). OpenAI `main` defaults to `gpt-5.5`, aux to `gpt-5.4-mini`; OpenRouter gains `openrouter/openai/gpt-5.5`. `_updated` bumped to 2026-05-27.

## 0.20.0 - Rust v1 release

- First Rust release on `main`. Merges the entire v1 branch (single-binary runtime, SQLite-vec memory, lethe-migrate, multi-target release pipeline).
- Aligned agent loop with the Python `main` reference implementation:
  - Dropped the duplicated `<recent_tool_context>` system-prompt block; tool calls live only in the conversation stream.
  - User messages are always timestamped (current + historical).
  - Removed the hard 20-message history cap; token-budget compaction is the only trimmer. DB read raised to 500 rows per turn.
- Tool-loop hardening:
  - `MAX_TOOL_ITERATIONS` 8 → 50; on cap, push a wrap-up nudge and run a no-tools final call.
  - Empty-response nudge: retry once before forcing wrap-up.
  - `FREE_TOOL_NAMES` (memory, telegram, actor lifecycle) excluded from the billable counter.
  - Per-turn tool log (ready for future auto-archival).
  - Circuit breakers: `MAX_TOOL_ERRORS=8`, `MAX_REPEATED_TOOL_CALLS=4`, `MAX_NO_PROGRESS_TURNS=4`.
  - Recover Gemma/llama-style `<tool_call:name{args}>` text embeddings when the native tool_calls field is empty.
- Telegram transport:
  - Send with `parse_mode=Markdown`; fall back to plain text on parse-entity errors.
  - Restored `---` bubble splitter (Python convention): pure-dash divider lines split, fenced code and markdown table separators preserved.
  - Actor-update flow uses an `ok` sentinel contract — prompt asks for `ok` when nothing to surface; code checks exact match and skips Telegram.

## 0.18.0 - Rust v1

- Rewrote Lethe as a Rust single-binary runtime.
- Added Telegram polling and authenticated HTTP/SSE API modes.
- Added local markdown memory, old-schema LanceDB notes/archival/message recall, SQLite todos, hippocampus recall, curator, heartbeat, notification gating, and resident Kameo actor/subagent runtime.
- Added LanceDB-backed semantic search for notes, archival memory, and message history using the legacy Snowflake Arctic embedding model id.
- Added `genai` LLM routing with OpenRouter model-id normalization and `LLM_API_BASE` support for OpenAI-compatible local servers.
- Added filesystem, shell, PTY terminal, browser, image, web, memory, notes, todos, actor, and transport tools.
- Added binary release packaging and binary-first install/update scripts with source-build fallback.
- Added `lethe backup` / `lethe restore` to pack and unpack the workspace, agent state (memory + history), and `.env` as a single tar.gz, prompting before overwriting an existing workspace or `.env`.
- Added `migrator/` subproject (`lethe-migrate` binary) that moves legacy LanceDB data (`archival_memory`, `message_history`, `notes`) into the new SQLite-vec storage. Standalone Cargo project — keeps the Arrow/LanceDB stack out of the main `lethe` build.
- Release workflow now builds `lethe` and `lethe-migrate` for four targets (linux x86_64/aarch64, macOS x86_64/aarch64) on native GitHub Actions runners.
- `install.sh` now fetches both `lethe` and `lethe-migrate` from the release assets and hands off to `lethe init` for the provider/model/key wizard (no more duplicated bash prompts). `uninstall.sh` explicitly removes both binaries and tidies an emptied `$LETHE_HOME/bin/`.
- Removed the former package/test stack and the web console while keeping Anthropic subscription/OAuth support in the Rust runtime.
