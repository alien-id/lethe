# Changelog

## 0.22.22 - Knowledge-graph agent tools

- **The agent can query and curate the user's knowledge graph.** New `kg_search`,
  `kg_get`, `kg_add`, `kg_delete`, `kg_merge`, and `kg_set_notes` tools call the
  hosted `/kg` API over the entities (people, places, companies) extracted from
  conversations. They are backed by `KG_API_BASE` + `KG_API_TOKEN` injected by
  the hosted supervisor; a new `ToolCategory::KnowledgeGraph` hides them entirely
  when unconfigured, so self-hosted installs without a graph backend are
  unaffected.

## 0.22.21 - Multi-byte streaming fix

- **Streamed replies in non-Latin scripts no longer abort mid-turn.** The
  vendored genai `WebStream` decoded each network chunk with `String::from_utf8`
  in isolation, so a multi-byte character (Cyrillic, accented Latin, emoji, …)
  split across two chunk boundaries failed to decode and aborted the whole
  stream with `LLM streaming chat request failed`. It now buffers an incomplete
  trailing UTF-8 sequence and prepends it to the next chunk, decoding only the
  validated prefix; genuinely malformed bytes still error as before.
  Heavy-Cyrillic conversations — which split a character on most turns — were
  effectively unusable.

## 0.22.20 - Streaming truncation fix

- **Streamed replies no longer lose their final tokens.** vLLM-based providers
  batch the last content delta into the same SSE frame as `finish_reason`; the
  vendored genai OpenAI streamer discarded that frame's content, cutting
  replies mid-sentence — the trailing words and punctuation were missing from
  both the live stream and persisted history (~1 in 3 turns on Lightning).
  The finish_reason branch now captures and emits batched content and
  reasoning content, mirroring its existing tool_calls rescue.

## 0.22.19 - Overridable autonomy prompts

- **All model-facing strings from 0.22.18 moved into the prompt template
  system.** The wrap-up checkpoint nudge, active-tasks preamble, heartbeat
  open-work wrapper, subagent previous-turn header, restart notice, and
  max-turns handoff were hardcoded in Rust; they now live in
  `config/prompts/*.md` — embedded as defaults, listed by `lethe prompts
  export`, and overridable per-install from `workspace/prompts/`.
- **Sharper action-discipline examples.** Positive examples no longer show a
  `[tool_call: ...]` pseudo-code notation that models mimicked as text instead
  of making real tool calls.
- **CI: release workflows on Node 24 runtimes** (checkout v6,
  upload-artifact v7, download-artifact v8, action-gh-release v3) ahead of
  GitHub's 2026-06-16 forced migration.

## 0.22.18 - Long-horizon autonomy

- **Subagent state survives restarts.** Every actor mutation is snapshotted to an
  `actors` table in the unified memory DB; on startup, unfinished subagents are
  rehydrated with their goals, task state, turn budget, and last checkpoint,
  re-parented to the live principal, and woken to continue. A deploy, crash, or
  self-restart now interrupts work instead of erasing it.
- **The heartbeat sees unfinished work.** The idle gate only skips a tick when
  there are no due reminders *and* no open work; heartbeat prompts carry an
  open-work digest — unfinished subagents (including blocked ones, which never
  autocontinue on their own), in-progress and overdue todos — with instructions
  to act on each item. Previously a blocked subagent could sit invisible for days.
- **Turn caps produce checkpoints, not truncated answers.** Hitting the tool
  budget forces a structured GOAL/DONE/REMAINING/NEXT checkpoint; subagents see
  their own previous turn each iteration; a subagent that runs out of turns hands
  its checkpoint to its parent for a successor. The compaction-summary race is
  closed: the next turn waits (bounded) for the previous turn's summary update.
- **Todos are the work queue.** In-progress and overdue todos are injected into
  every system prompt as `<active_tasks>`, and todos support `parent_id` subtasks
  (in-place schema migration, agent tools, CLI).
- **Smarter circuit breakers, no dropped signals.** Transient tool errors
  (timeout/429/5xx/network) weigh half a permanent error; the repeated-call
  breaker requires identical call *and* result, so polling that observes progress
  survives; rate-limited proactive messages defer to an outbox and re-deliver on
  a later tick (6h TTL) instead of being silently discarded.

## 0.22.17 - Non-blocking post-turn memory maintenance

- **The reply no longer waits on post-turn memory work.** The conversation-summary
  update (after a compaction) and the cadence-gated curator pass make aux-model LLM
  calls; they ran synchronously before `done`, so a client sat on a typing indicator
  after the answer was already complete. They're now spawned detached — `done` fires
  right after the reply (measured gap ~0.1s) and memory consolidation runs in the
  background. Errors are logged, never propagated.

## 0.22.16 - Stream reasoning (live "thinking") on the API

- **Reasoning tokens now stream on a separate channel.** The genai streaming path
  forwards `ReasoningChunk` deltas to a new `TurnObserver::on_reasoning_delta`,
  and the HTTP API emits them as `assistant.reasoning` SSE events (distinct from
  `assistant.delta`). Clients can render a live "thinking…" indicator instead of
  sitting through dead-air while the model reasons before answering. No change to
  the agent loop or non-streaming paths.

## 0.22.15 - Leaner initial tool set (better prefill)

- **The top-level agent now loads ~12 tools up front instead of ~30.** Two
  changes: (1) actor-orchestration tools are loaded initially only for actual
  subagents — the top-level agent discovers them via `request_tool` (they were
  already visible, just not lazy); (2) lower-frequency cortex tools
  (`note_create`, `conversation_search`, `note_search`, `memory_complete`,
  `todo_create`, `todo_list`) moved from initial to requestable. The core file/
  shell/web/memory tools stay initial. With a Telegram bot connected the initial
  set stays ≤ 15. Smaller prompts = faster prefill/TTFT, especially for smaller
  models; everything remains reachable through tool discovery.

## 0.22.14 - Retry transient errors on the OpenRouter path

- **The genai / OpenAI-compatible path now retries transient failures.** HTTP 429
  (rate limits, incl. OpenRouter's shared-pool throttling), 5xx, and network blips
  are retried up to 3 attempts with capped exponential backoff (~1s, 2s); permanent
  errors (400/401/403/404) surface immediately. Previously only the Anthropic OAuth
  path retried, so a brief OpenRouter rate-limit spike became a hard "LLM streaming
  chat request failed" error. The streaming path's pre-stream fallback inherits this.

## 0.22.13 - Keep the fast tool handoff + web_search guard

- **Reverts 0.22.12's discard-on-handoff.** When `LLM_MODEL_TOOL` is set, the
  base model again executes its own first tool call and the tool model takes
  over from the next iteration (the 0.22.11 behavior) — fewer round-trips. The
  malformed/runaway tool-call batches that motivated 0.22.12 were traced to a
  single OpenRouter provider (Parasail) being incompatible with the `genai`
  client, not to the base model; that's handled by provider routing instead.
- Keeps the `web_search` empty-query guard from 0.22.12.

## 0.22.12 - Tool-model handoff hardening

- **The base model's tool call is now a routing signal, not an action**: when
  `LLM_MODEL_TOOL` is set, a weaker base model (e.g. Gemma) sometimes emits
  malformed or runaway tool-call batches (empty arguments, duplicate ids). The
  agent loop now uses the base model's tool-call emission only to detect that
  tools are needed — it discards those calls without executing them and lets the
  tool model issue the real, well-formed calls. Prevents floods of failed tool
  calls on handoff.
- **`web_search` rejects an empty query** up front with a clear error instead of
  forwarding it to Exa and surfacing a raw 400.

## 0.22.11 - Dynamic tool-model routing

- **A turn can now switch models mid-flight for tool chains**: set the new
  optional `LLM_MODEL_TOOL` and a turn starts on `LLM_MODEL`, then transparently
  switches to the tool model the moment the assistant calls a tool — staying on
  it for the rest of the chain and the post-chain reply — and the next turn
  starts on `LLM_MODEL` again. This lets a cheap model drive normal conversation
  while a stronger reasoner runs tool chains. Leave `LLM_MODEL_TOOL` empty (the
  default) for the previous single-model behavior. The switch also applies to
  background actors/DMN turns that call tools.

## 0.22.10 - Live web sync of Telegram turns

- **Telegram conversations now show up live in an open web client**: an
  Agent-level conversation-event broadcast carries each Telegram turn (the
  incoming user message and the assistant reply) onto the HTTP `/events` SSE
  stream, so a web tab can append them to its transcript without a reload. Web
  `/chat` turns stream over a private per-request channel (not `/events`), so
  there's no double-render.

## 0.22.9 - Telegram out-of-credits reply

- **Telegram no longer fails silently when out of credits**: a turn rejected for
  lack of credits (the hosted metering proxy returns HTTP 402) bubbled the error
  and sent nothing back, so the user saw silence. The Telegram path now detects
  that case and replies with a clear out-of-credits message.

## 0.22.8 - Runtime Telegram transport control

- **Connect/disconnect Telegram at runtime, no restart**: a new transport
  supervisor (in `api` mode) reconciles the running Telegram poller to a
  desired-config file (`config/transports.json`). A control plane can write that
  file to connect or disconnect a bot and the change is picked up live. When no
  file is present it falls back to the static `TELEGRAM_*` settings, so desktop
  behaviour is unchanged.
- **Lock to the first user who messages**: `TelegramClient` gains an opt-in mode
  where, with no allowlist configured, the first user to message the bot is
  bound in as the sole allowed user (persisted to `config/transports-state.json`)
  and everyone else is rejected — closing the "anyone who finds the bot can talk
  to it" hole for unattended/hosted bots. The token and binding live in the
  config dir, never the workspace, so they stay out of `lethe backup` archives.

## 0.22.7 - Streaming on OpenAI-compatible providers

- **Stream assistant text on the generic (genai) provider path**: streaming
  only worked on the Anthropic-OAuth and OpenAI-OAuth paths; every
  OpenAI-compatible provider routed through genai (OpenRouter, OpenCode Go, any
  custom `LLM_API_BASE`) fell back to a non-streaming call and replayed the
  whole reply as a single delta, so clients saw the entire message appear at
  once after a long pause. `exec_chat_request_stream` now streams via genai's
  `exec_chat_stream` with `StreamEnd` captures enabled, then rebuilds the same
  `ChatResponse` (text + tool calls + usage) the non-streaming path returned —
  the agent loop is unchanged. Pre-stream failures fall back to the
  non-streaming path; mid-stream failures surface the error without retrying
  (partial text may already be on screen). Requesting `stream:true` also lets a
  metering proxy stream its forwarded response.

## 0.22.6 - Tool-call history fix on OpenAI

- **Fix every tool-using turn failing on strict OpenAI**: replayed tool
  history serialized the tool result as a `user`-role message, so genai's
  OpenAI adapter never emitted the required `role:"tool"` message and the API
  rejected the turn (`an assistant message with 'tool_calls' must be followed
  by tool messages ... the following tool_call_ids did not have response
  messages`). Tool results are now mapped to `ChatRole::Tool`, which the OpenAI
  adapter renders correctly; the Anthropic path is unchanged (it renders both
  as the same `tool_result` block). Reproduced against real OpenAI; preserves
  structured tool-call/result history for every provider.

## 0.22.5 - OpenCode Go provider

- **OpenCode Go provider with dual-protocol routing** (#27, thanks @voldmar): a
  new budget-friendly provider gateway alongside OpenRouter/Anthropic/OpenAI.
  OpenCode Go speaks different wire protocols per model — some OpenAI-API, some
  Anthropic-Messages — so each catalog entry declares its protocol and the
  router selects the matching adapter (and cache dialect) automatically. Adds
  `OPENCODE_GO_API_KEY`, `lethe login opencode-go`, 14 catalog models, and the
  `opencode-go/` provider prefix. API key only — no subscription path.
- **History compaction now counts and archives inline images**: base64 image
  payloads in conversation history (e.g. Telegram photos) were invisible to the
  compaction budget, so history with images could grow past the context window
  and fail with `context_length_exceeded`. `message_chars()` now tallies image
  attachments, and old images (older than the last 2 user turns) are replaced
  with lightweight stubs before compaction.
- **`lethe check` pings both models**: the smoke test now exercises the main
  model and, if distinct, the aux model separately (previously only the aux).
- **Fixes**: preserve the full error cause chain in LLM failure logs
  (`{error:#}` / `?error`); repo-root detection (#26, thanks @voldmar) now
  resolves from the current directory before falling back to the binary path.

## 0.22.4 - Telegram interactive keyboards

- **Inline & reply keyboards** (#24, thanks @voldmar): the assistant can now
  attach a Telegram `reply_markup` to its messages — inline buttons for
  message-scoped actions (callback presses are parsed into assistant-visible
  context, answered, and the keyboard is removed after the press) and reply
  keyboards for quick short replies (removed once a matching button text
  arrives).
- **Hardening on merge**: route callback presses even when the original message
  is no longer accessible (>48h old), accept `reply_markup_json` as either a
  JSON string or a raw object, match callback data robustly, and drop two
  non-Bot-API button fields that could otherwise trigger send errors.

## 0.22.3 - Telegram reaction replies

- **Respond to reactions on her own messages**: when you react to a Telegram
  message Lethe sent, she now takes a turn and answers — but only when a reply
  is warranted, staying silent otherwise. She tracks the messages she sends (in
  a bounded per-process log, shared with the tool send path) so reactions can be
  attributed to her own messages; reactions on anything else are still just
  recorded to memory as before.

## 0.22.2 - OpenAI OAuth reliability + context cap

- **OpenAI OAuth stream reliability** (#22, thanks @voldmar): trim oversized
  request bodies (cap 500 KB, drop a leading role-less input item) so the
  Codex/Responses endpoint stops rejecting large turns, and surface real
  stream errors (`error` / `response.failed`) and truncated streams instead
  of collapsing them into a misleading empty-payload message.
- **Context windows capped at 128k**: auto-compaction manages history, so the
  per-model window is a deliberate working-set cap, not the model's maximum.
  Every entry in `model_context_limits.json` is now 128k (was up to 400k/1M),
  with an explicit `gpt-5.5` row; the TUI footer gauge tracks the same cap.

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
