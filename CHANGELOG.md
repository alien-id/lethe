# Changelog

## 0.22.1 - Bug fixes

- **TUI: no more duplicated replies**: a streamed assistant message was
  rendered twice — once as the streamed `---`-split bubbles, then again
  in full when the turn-final `text` echo arrived. The echo-suppression
  matched the streamed tail against the re-split segments by string, which
  missed whenever a trailing `---` divider sealed the stream or the
  provider normalized the final body, re-rendering the whole reply. The
  TUI now drops the final echo outright whenever the turn streamed; only
  non-streaming turns push the text.
- **Container builds from source actually work** (#22, thanks @voldmar):
  copy the vendored `genai` crate and add `libssl-dev` so `cargo build`
  resolves; pin `rust:1.96-slim` for reproducible images and drop a dead
  `.cargo` COPY. Adds a `--force` flag to `container up`/`rebuild` to
  replace an already-installed service unit.

## 0.22.0 - Container-first CLI

- **Isolated container by default**: `lethe init` now deploys Lethe
  into a rootless container (Podman on Linux, Apple Container on macOS,
  auto-installed if missing) and registers it as a background service.
  Pass `--yolo` for the old native, uncontained install. New
  `lethe container` subcommands — `up` (build image, create container,
  install + start the service), `down`, `status`, `logs [-f]`, `shell`,
  `rebuild`, `build` — plus repeatable, persisted `--mount host[:container]`
  shares.
- **Service management**: `lethe service install [--now] [--force]`,
  `status`, and `uninstall` write/inspect/remove the systemd user unit
  (Linux) or launchd agent (macOS).
- **New top-level commands**: `lethe install` (alias for `init`),
  `lethe uninstall [--purge]` (teardown; `--purge` also deletes `~/.lethe`,
  always confirmed), `lethe run [--yolo]` (foreground), `lethe status`
  (version + censored config — now what bare `lethe` prints in CLI mode),
  `lethe identity {show,set,reset,edit}` (name + persona),
  `lethe transport {list,api,telegram}` (configure how you reach her),
  `lethe model [<id>] [--aux <id>] [--pick]`, `lethe prompts {export,list}`,
  and `lethe completions <shell>`.
- **Non-interactive `init`**: when stdin isn't a terminal (Docker/CI),
  `init` reads `--provider`/`--model`/`--aux-model` and the key from the
  provider's env var, with no prompts.
- **Global `--config <PATH>`** flag on every command to point at a
  different `.env` (also honored via `LETHE_CONFIG_FILE`).
- **TUI `/model`**: `/model <id>` now switches the running agent's model
  live via `POST /model` (with feedback in the transcript); bare `/model`
  shows the current model. Bare OpenRouter ids are normalized
  (`vendor/model` → `openrouter/vendor/model`) server-side, matching the
  persisted `lethe model` path.
- **Release workflow**: the one-shot `lethe-migrate` build moved to its
  own `migrator-v*`-tagged workflow so the main release no longer pulls
  LanceDB/Arrow into its build; main release builds now use the `mold`
  linker.

## 0.21.2 - Release packaging fix

- **Fix `scripts/package-migrator`**: referenced `MIGRATION-SPEC.md`,
  which was renamed to `MIGRATION.md` in commit `a4b3817`. The
  release workflow's "Package lethe-migrate" step has been failing
  on every platform since 0.20.6, producing no published binaries
  for 0.21.0 / 0.21.1. Switched the copy to `MIGRATION.md`.

## 0.21.1 - TUI polish: scroll, history seed, preflight

- **Transcript scrolling**: switched scroll math to ratatui's wrapped
  line count (`Paragraph::line_count`, gated behind
  `unstable-rendered-line-info`). The previous calc counted raw
  `lines.len()`, so wrapped paragraphs lied about overflow and the
  transcript appeared frozen at the bottom. Mouse wheel,
  `PgUp/PgDn`, `Ctrl-Up/Down`, `Ctrl-Home/End`, and (with the
  transcript pane focused) bare `Up/Down/Home/End` all scroll now.
- **History seed on startup**: TUI pulls `/session/history?limit=50`,
  filters internal-visibility rows (heartbeats, DMN reflections,
  actor updates) and tool/system entries, then seeds the transcript
  with the last 5 user↔assistant exchanges.
- **Preflight + clean error**: `client.preflight()` hits an
  auth-required endpoint before `enter_terminal()`, so a 401 / bad
  URL prints a single-line error to stderr and exits without
  taking over the screen.
- **`LETHE_API_TOKEN=` empty in shell**: treated as unset so a stale
  shell export doesn't shadow the value in `~/.lethe/config/.env`.
- **Brighter palette over SSH**: replaced every `Color::DarkGray`
  (terminal color 8, often invisible on remote sessions) with
  `Color::Gray` and dropped `Modifier::DIM` from tool args, due
  dates, sidebar IDs, footer hints, and the thinking label.
- **Scroll keys visible**: footer now shows
  `PgUp/PgDn scroll · Ctrl-Home/End jump · Tab pane · Ctrl-B sidebar · Ctrl-C quit · /help`,
  and `/help` lists the full key + scroll vocabulary.

## 0.21.0 - TUI client, streaming, Brainstem

- **Terminal UI** (`lethe tui`). New ratatui-based client that talks to a local `lethe api` over HTTP+SSE: transcript pane with inline tool cards, right sidebar with the actors tree and todos, streaming assistant text with a visible thinking spinner, `@`-prefix workspace path autocomplete, and slash commands (`/help`, `/clear`, `/cancel`, `/todos`, `/actors`, `/model`, `/quit`). See `src/tui/`.
- **Real LLM streaming on subscription OAuth**. Anthropic OAuth (`call_messages_stream`) parses Messages SSE incrementally (`content_block_delta`/`text_delta` for text, `input_json_delta` for tool args). OpenAI OAuth (`call_messages_stream`) consumes the Codex Responses SSE stream incrementally via a new `OpenAiStreamState`. Both surface chunks via a new `TurnObserver::on_assistant_delta` hook that the API translates to `assistant.delta` SSE events. The genai-native path falls back to non-streaming with a single replay delta.
- **Brainstem** (`scheduler::brainstem`). Heartbeats, proactive emissions, and any future internally-triggered urges live in a single Brainstem task. Transports (Telegram, HTTP/SSE) subscribe to its `BrainstemHandle` broadcast and forward emissions to their own clients. Removed the duplicate heartbeat loops from `cli/telegram_loop.rs` and `interfaces/api.rs`.
- **Combined api+telegram in one process**. `lethe api` now spawns the Telegram poller in-process when `TELEGRAM_BOT_TOKEN` is set, sharing one Agent, one ActorRegistry, and one Brainstem. The standalone `lethe telegram run` and `lethe api` subcommands still work for single-transport deployments.
- **New SSE events**: `tool.start`, `tool.end`, `actor.spawned`, `actor.state`, `actor.task`, `actor.message`, `assistant.delta`, `usage`, `turn.start`. Backward-compatible — `text`/`typing_start`/`typing_stop`/`reaction`/`done` unchanged.
- **New readback endpoints**: `GET /actors` (live tree), `GET /todos` (filterable), `GET /session/history?limit=N`. The TUI uses these for initial paint and on event-driven refresh.
- **Default API port is `1373`** (was `8080`). Override with `LETHE_API_PORT`.
- **TUI submessage handling matches Telegram's**. Both clients split assistant output on pure `---`/`-----` lines outside fenced code blocks (`interfaces/telegram/formatting.rs::telegram_message_segments`), rendering each segment as its own bubble with latency jitter preserved. No more visible horizontal dividers in the transcript.

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
