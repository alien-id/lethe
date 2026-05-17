# Changelog

All notable changes to this project are documented in this file.

## v0.16.1 - 2026-05-17

### Fixed
- **Telegram emoji-only replies double-fired** (#20): when the assistant queued a `telegram_react` and its final reply was emoji-only, both a reaction and a text reply were emitted, producing duplicate visible output. A per-turn `ContextVar` guard now buffers `telegram_react` calls during the active turn and, at finalization, picks one channel (reaction or text) when both would otherwise carry the same emoji-only signal. Mixed turns still flush queued reactions then send the text reply.

### Added
- **User reactions are now ingested as conversation events** (#20): the bot subscribes to `message_reaction` updates and injects synthetic `[Telegram reaction added/removed/updated: …]` messages into the conversation manager (auth-gated; self-reactions filtered via bot identity resolved at startup).
- **`telegram_react` promoted to core tool** (#20): no longer requires `request_tool()`. Accepts an optional `message_id` argument to react to a specific message instead of the last tracked inbound one.

### Internal
- Extracted shared `send_message_reaction` transport into `src/lethe/reaction_transport.py`; `TelegramBot.react_to_message` and the agent-facing `telegram_react` tool both route through it.
- Consolidated Telegram inbound-message metadata construction into `_build_message_metadata` / `_remember_last_message` helpers on `TelegramBot`.

## v0.16.0 - 2026-05-11

### Added
- **Telegram voice/audio transcription** (#17): voice notes and audio files are now downloaded, transcribed, and fed into the conversation pipeline as text. Providers are auto-selected (OpenRouter → OpenAI → local Whisper CLI) based on available API keys; explicit selection via `TRANSCRIPTION_PROVIDER`. New settings: `TELEGRAM_TRANSCRIPTION_ENABLED`, `TRANSCRIPTION_PROVIDER`, `TRANSCRIPTION_MODEL`, `TRANSCRIPTION_LANGUAGE`, `TRANSCRIPTION_LOCAL_COMMAND`. Transcription failures surface a clear error to the user instead of dropping the message silently.
- **Telegram sticker ingestion** (#18): stickers (static `.webp` and video `.webm`) are downloaded, cached by `file_unique_id`, normalized, and (for vision-capable models) described via a short LLM vision call. Falls back to metadata-only description (emoji + set name) when the model isn't vision-capable or rendering fails. `.tgs` (Lottie) is recognized but not rendered. Cache lives under `<cache_dir>/telegram/stickers/`.
- **`ffmpeg-free` in the Fedora container image**: needed for `.webm` sticker preview rendering. Without it the sticker handler degrades gracefully to metadata-only.

## v0.15.6 - 2026-05-11

### Internal
- Remove orphaned helpers left over from the v0.15.5 cortex routing change: `_get_recent_user_signals` in `actor/integration.py` and the now-unused `json` imports in `main.py` and `actor/integration.py`. No behavior change.

## v0.15.5 - 2026-05-10

### Fixed
- **Background notifications bypassed cortex**: DMN/brainstem `user_notify` messages were relayed straight to Telegram via a separate `_decide_user_notify` LLM call, which lacked full conversation/time context — producing artifacts like "Good morning" sent at midnight. The auto-relay pipeline (`_decide_user_notify` callback, `parse_notify_decision` helper) is removed; `user_notify` from brainstem/dmn now triggers `_run_cortex_turn` with a synthetic system prompt, so cortex wakes, reads its inbox, and decides whether/when/how to relay the thought in its own voice. Same pattern already used for subagent task updates. Event renamed `background_notify_relayed_to_user`/`_dropped_by_cortex` → `background_notify_deferred_to_cortex`.

## v0.15.4 - 2026-05-07

### Fixed
- **`container-setup.sh --rebuild` failed on Apple Silicon Macs without Rosetta**: apple/container's buildkit VM enables `build.rosetta=true` by default and aborts bootstrap on hosts where Rosetta isn't installed (`VZErrorDomain Code=2 "Rosetta is not installed"`). The setup script now probes Rosetta on arm64 hosts via `arch -arch x86_64 /usr/bin/true` and, if absent, sets `build.rosetta=false` and stops any running builder so the property takes effect. Native arm64 builds (Lethe's default) are unaffected; cross-arch amd64 builds fall back to QEMU. (#19)

## v0.15.3 - 2026-05-07

### Fixed
- **ARM Macs got an x86_64 container image**: `container build` (apple/container) and `podman build` were called without an architecture flag, so on Apple Silicon the resulting `lethe:latest` rootfs was `linux/amd64` running under emulation. `scripts/container-setup.sh` now detects host arch via `uname -m` and passes `--arch $ARCH` to `container build`/`run` and `--platform linux/$ARCH` to `podman build` on both Linux and macOS. The generated `run-container.sh` and printed Shell hint also carry the flag so copy-pasted commands stay consistent. **Existing users on ARM Macs:** rerun `./scripts/container-setup.sh --rebuild` to discard the cached x86 image; macOS-podman users with an amd64-initialized VM should also `podman machine rm && podman machine init --cpus 2 --memory 4096` first. Verify with `container run --arch arm64 lethe:latest uname -m` (should print `aarch64`).

## v0.15.2 - 2026-05-05

### Fixed
- **Anthropic OAuth burst throttling**: Claude Max/Pro returns 403 `permission_error` (not 429) when several actors hit the messages endpoint concurrently. A global `asyncio.Semaphore` (default 1, configurable via `LETHE_OAUTH_MAX_CONCURRENCY`) now serializes requests, and 403 burst-throttle responses are promoted to `RateLimitError` so `_call_with_retry_oauth` backs off and the shared cooldown reaches every queued caller. Resolves curator cascading-failure bursts. (#16)
- **OpenAI Codex OAuth streamed response parsing**: SSE streams that delivered text via `response.output_text.delta` events with no full output items returned an empty payload, causing Lethe to send the generic `"Done."` Telegram fallback. The parser now assembles output items from `item` / `output_item` events, reconstructs assistant text from delta events, accepts both `output_text` and `text` content block types, and falls back to top-level `output_text`. (#15)
- **`lethe.__version__` was stuck at `0.11.1`**: the constant in `src/lethe/__init__.py` had drifted from `pyproject.toml`. It now reads from package metadata via `importlib.metadata`, so it always matches the installed version (and the brainstem version-detection fallback stays accurate).

### Changed
- **Startup curator runs in the background**: `Agent.initialize()` no longer blocks on the harvest+curate pass; `main.run()` schedules `agent.run_startup_curator()` as a fire-and-forget task so the Telegram bot becomes responsive immediately on cold start.

## v0.15.1 - 2026-04-30

### Added
- **macOS podman fallback**: Intel Macs and pre-Sequoia systems automatically use podman when apple/container is not available. Installer tries `brew install container` first, falls back to `brew install podman`.

## v0.15.0 - 2026-04-30

### Changed
- **Linux container: nspawn → podman**: replaced systemd-nspawn (required root) with rootless podman. No sudo needed anywhere — builds, runs, and manages as the current user via systemd user service.
- **Automatic nspawn→podman migration**: `update.sh` detects old systemd-nspawn installs, removes them, and sets up podman automatically.
- **Container image**: added `sudo`, `which`, `file`, `tar`, `gzip`, `unzip`, `diff`, `procps-ng` to the Containerfile. Lethe user has passwordless sudo inside the container.
- **SELinux compatibility**: uses `--security-opt label=disable` instead of `:z` volume relabeling (which failed on non-relabelable files).
- **Uninstaller**: handles both old nspawn and current podman container cleanup.

## v0.14.0 - 2026-04-29

### Added
- **Container-first install**: default deployment now runs inside a rootless podman container (Linux) or apple/container (macOS). Native install still available via `--yolo` flag.
- **Containerfile and container-setup.sh** for reproducible, isolated builds.
- **Workspace reuse on reinstall**: installer detects existing `~/.lethe` workspace (config, memory, notes) and offers to reuse it instead of starting fresh.
- **Onboarding message**: first-start greeting when the message history is empty.
- **Notes subdirectory support**: `NoteStore.create()` accepts a `subdir` parameter; listing and reindexing use recursive glob.

### Changed
- **Embedding engine rewrite**: replaced sentence-transformers + PyTorch (~2 GB) with ONNX Runtime + Snowflake arctic-embed-m-v2.0 — multilingual (76 languages), 768-dim vectors, 297 MB total. Shared `embeddings.py` module used by all memory subsystems.
- **Automatic vector migration**: on startup, if the embedding model has changed, all archival/message tables are re-embedded and notes are reindexed. No manual intervention or data loss.
- **LanceDB API compatibility**: `list_tables()` result handling adapts to both old (list) and new (object) return types.
- **Dependency footprint reduced**: removed `sentence-transformers`, `torch`, and the PyTorch CPU index. Added `onnxruntime`, `tokenizers`, `huggingface-hub`.

### Fixed
- **macOS container installer**: XPC startup, PATH resolution, OAuth token capture, and onboarding flow for apple/container.

## v0.13.1 - 2026-04-23

### Fixed
- **Proactive messaging restored**: `decide_user_notify` was recording a send to the rate-limiter *before* the actor's `send_to_user` callback ran, so the subsequent `heartbeat_send` blocked itself on its own pre-record. All proactive paths (hourly heartbeat, DMN notifications) were suppressed for 60 minutes after any decision — in practice, zero messages ever delivered.
- **Duplicate subagent terminal notifications**: when a subagent emitted a terminal `task_update` via `send_message` before calling `terminate`, the registry also produced a synthetic termination message, so parents saw two "done/failed" events per outcome. The synthetic message is skipped when one already exists.
- **Redundant `_notify_parent` calls on actor error / max_turns**: `actor.terminate()` already notifies the parent; the extra emits before `terminate()` are gone.

### Changed
- **systemd units source `$CONFIG_DIR/.env`**: generated `lethe.service` (user and system variants) now includes `EnvironmentFile=-$CONFIG_DIR/.env`, so toggles like `LETHE_NO_SANDBOX=1` in the config file take effect at service start without editing the unit.

## v0.13.0 - 2026-04-22

### Added
- **OS-level write sandbox**: Landlock (Linux) and Seatbelt (macOS) restrict all writes to `~/.lethe` and `/tmp`. The sandbox is enforced at process start and cannot be escaped.
- **Centralized path management**: new `src/lethe/paths.py` module derives all runtime paths from `LETHE_HOME` (`~/.lethe`), eliminating scattered path logic.
- **OAuth subscription billing fix**: Anthropic subscription (Max/Pro) requests now embed the `x-anthropic-billing-header` attribution in the system prompt, and tool names use Claude Code's `mcp__<server>__<Tool>` naming convention. Both are required for Anthropic's server to classify requests as first-party and bill against the subscription plan instead of extra usage credits.

### Changed
- **Docker/container infrastructure removed**: Dockerfile, docker-compose files, entrypoint.sh, the entire `gateway/` directory, and `tests/test_api_gateway.py` have been deleted. Native + sandbox is now the only deployment mode.
- **API server binds to localhost**: the API server now defaults to `127.0.0.1` instead of `0.0.0.0`. Override with `LETHE_API_HOST` for reverse-proxy setups.
- **Install/update/uninstall scripts rewritten**: switched from zsh to bash for universal compatibility. `install.sh` detects existing dev checkouts (skips cloning), creates the `~/.lethe` directory tree, and writes systemd/launchd services with `LETHE_HOME`.
- **Model catalog updated to April 2026 SOTA**: added Claude Opus 4.7, Kimi K2.6, GPT-5.4 Pro/Codex, GLM 5.1; removed stale entries; simplified Haiku ID to `claude-haiku-4-5`.
- **OAuth tool naming convention**: tools sent to Anthropic's API are now named `mcp__lethe__PascalCase` (double-underscore MCP format) instead of `mcp_snake_case`, which Anthropic's server rejects as third-party.

### Fixed
- **Trailing assistant messages stripped from OAuth calls**: the API rejects prefill-style trailing assistant messages; these are now removed before sending.
- **Note extraction quality and dedup**: extraction prompt now enforces a higher quality bar and receives existing note titles to prevent duplicates.
- **Subagent termination notifications**: completion/failure results are now reliably delivered to parent actors.

## v0.12.3 - 2026-04-20

### Changed
- **API-mode conversations now use the shared conversation pipeline**: worker `/chat` requests run through `ConversationManager` instead of bypassing it, so API mode now inherits the same interrupt/cancel semantics as direct Telegram mode.
- **Hot model switching now rebuilds runtime state**: switching `model`, `model_aux`, provider, or auth mode refreshes the assembler, system prompt, auth client, embedded tool reference, and related context instead of mutating config in place.
- **Proactive routing follows the active chat**: direct Telegram mode and gateway mode now target the latest active chat for heartbeat and cortex follow-ups instead of pinning those messages to the first/earliest chat seen.
- **README updated for current deployment behavior**: docs now describe gateway/API mode, authenticated worker endpoints, current heartbeat defaults, and the current actor/memory layout.

### Fixed
- **API cancel was ineffective**: `/cancel` now cancels the actual in-flight worker conversation instead of a dead code path.
- **Worker API exposure tightened**: `/chat`, `/cancel`, `/model`, `/events`, `/configure`, and `/file` now require `LETHE_API_TOKEN`.
- **Arbitrary file reads through worker `/file` removed**: worker file serving is now restricted to the workspace mount (`/workspace`).
- **Gateway auth propagation**: the gateway now forwards worker auth headers for chat, cancel, model, configure, event streaming, and file fetches.
- **LanceDB deprecation cleanup**: startup checks now use `list_tables()` instead of deprecated `table_names()`.

### Removed
- **Amygdala actor retired fully**: emotional salience handling is now documented and surfaced as part of hippocampus, and the dead Amygdala runtime/UI code has been removed.
- **Dead `TaskQueue` implementation removed**: the unused queue layer is no longer shipped alongside the active conversation/SSE paths.

## v0.12.2 - 2026-04-20

### Added
- **Persistent notes system**: skills, conventions, and durable knowledge are stored as searchable notes under `~/lethe/notes/`.
- **Automatic note extraction**: Lethe now extracts notes from successful tool sequences and archival memory so useful procedures survive across sessions.

### Changed
- **Prompt architecture split cleanly**: workspace persona/identity was separated from repo-managed system instructions and prompt files.
- **Cortex tool budgeting tightened**: the active cortex tool set was reduced and reorganized around `request_tool()` for better Gemma 4 reliability.
- **Hippocampus streamlined**: recall logic was tightened, full note content can be surfaced when relevant, and actor context is skipped when there is no inbox/subagent activity.

### Fixed
- **Gemma 4 tool calling reliability**: Lethe now recovers text-embedded tool calls, strips stray native tool-call fragments, preserves tool/result pairing across sessions, and reduces cross-model prompt contamination.
- **Duplicate/unsafe tool surface cleanup**: dead tool registrations were removed, `telegram_send_message` was moved out of the compact cortex set to stop send loops, and `add_tool()` now keys registrations consistently.
- **Subagent model selection correctness**: actor model-tier selection was corrected.

## v0.11.2 - 2026-04-14

### Fixed
- **Context overflow recovery**: API calls that exceed the context window now auto-compact and retry (up to 3 attempts) instead of crashing. Second retry also truncates oversized tool results with error-aware head+tail preservation.
- **Tool outcomes lost across sessions**: Tool results were silently dropped on history reload — now extracted as brief outcome annotations and injected into adjacent assistant messages so the model remembers what tools accomplished.
- **Hippocampus couldn't recall tool outcomes**: Conversation search filtered out all tool messages. Now allows non-search tool results (capped at 2K chars) so hippocampus can surface past tool achievements.
- **Compaction loses active work context**: Summarization prompt now explicitly preserves active tasks, latest user request, commitments, and partial progress. Recent kept turns are passed to the summarizer to avoid redundancy.
- **Stale timestamps after compaction**: Summary block now includes a `[Compacted at ...]` temporal anchor that refreshes on each compaction.

### Changed
- **Proportional message capping**: Message truncation limit is now 30% of context window (floored 2K, capped 400K) instead of fixed 50KB. Truncation is error-aware — allocates more to the tail when it contains error/traceback patterns.
- **Actual token tracking**: Compaction decisions now use real `prompt_tokens` from API responses when available instead of the `len/4 * 1.3` heuristic.
- **Auto-archive tool achievements**: After turns with successful state-changing tools (writes, logins, API calls), a brief digest is automatically stored in archival memory for hippocampus discoverability.

## v0.11.1 - 2026-04-02

### Fixed
- **Anthropic `tool_use` 400 errors**: orphaned tool-use state is now cleaned up correctly so Anthropic requests no longer fail on malformed tool-call history.

### Changed
- Release badge switched to the dynamic GitHub release badge.

## v0.11.0 - 2026-03-30

### Added
- **`/model` and `/aux` switching**: Telegram and gateway users can hot-swap models without restarting Lethe.
- **Model picker/catalog unification**: provider/model selection now uses a single model catalog with auth-aware UI sections.

### Changed
- **Amygdala merged into hippocampus**: salience tagging moved into the per-message hippocampus path, and emotional state is injected through transient context instead of a separate background actor.
- **DMN cadence and role updated**: DMN moved to an hourly cadence, uses the main model, and treats memory compaction as a primary duty.
- **Provider/auth switching hardened**: picker UI separates subscription vs API-key routes and avoids invalid OAuth routing when crossing providers.

## v0.10.21 - 2026-03-30

### Fixed
- **ARM64 container browser support**: Dockerfile now falls back to system Chromium on ARM64.
- **Console consolidation view**: missing consolidation-context wiring was restored.

## v0.10.20 - 2026-03-30

### Added
- **Multi-tenant gateway architecture**: Telegram gateway can now route users to isolated per-user Lethe worker containers.
- **Memory consolidation module**: added background memory consolidation support.

### Fixed
- **Gateway file delivery**: files created inside worker containers are now resolved back to the host correctly.
- **Subagent completion/progress flow**: progress timers and completion relays now notify cortex reliably without the old polling loop.

## v0.10.19 - 2026-03-25

### Changed
- Communication and memory-management tools no longer consume the tool-iteration budget.

## v0.10.18 - 2026-03-25

### Changed
- `LLM_MODEL` is now env-driven instead of relying on hardcoded model defaults.
- Message timestamps now use local timezone formatting, and Telegram delivery uses more human-like pacing.

## v0.10.17 - 2026-03-24

### Fixed
- Subagent completion now notifies the user through cortex reliably.

## v0.10.16 - 2026-03-24

### Fixed
- Excluded compromised `litellm` versions `1.82.7` and `1.82.8` from the dependency range.

## v0.10.15 - 2026-03-23

### Fixed
- **Tool-message orphaning**: tool/result pairing checks now normalize IDs before validation.
- **Anthropic image handling**: `image_url` payloads are converted into the correct Anthropic image format.

### Changed
- Increased continuation depth to allow longer multi-tool runs before giving up.

## v0.10.14 - 2026-03-22

### Fixed
- **Anthropic OAuth 400 errors**: request shaping was hardened so the Claude Code prefix is emitted as a standalone system block, orphaned tool results are cleaned, and the required beta/header behavior is preserved.
- **File-based Anthropic OAuth tokens**: token-file installs can now bypass the API-key presence check correctly.

## v0.10.13 - 2026-02-25

### Fixed
- Forced CPU-only torch resolution to stabilize dependency locking and installs.

## v0.10.12 - 2026-02-24

### Added
- **OpenAI OAuth support** for ChatGPT/Codex-style authentication.
- **Subscription quota context** injected into transient runtime context for supervision and decision-making.

### Changed
- OpenAI OAuth login/install flow and token env naming were hardened and simplified.
- Default OpenAI auxiliary model was aligned with `gpt-5.2`.

### Fixed
- Multimodal image payloads are preserved and normalized correctly for OpenAI OAuth responses.

## v0.10.11 - 2026-02-23

### Added
- Hard, prompt-independent rate limiting for proactive user messages.

## v0.10.10 - 2026-02-22

### Changed
- Subconscious/background notifications are now presented as Lethe’s own thoughts, with hardcoded personal names removed from that path.

## v0.10.9 - 2026-02-22

### Changed
- **Cortex-gated notifications**: background notifications are rewritten in cortex’s own voice before reaching the user.
- **Console host binding**: added `LETHE_CONSOLE_HOST`, defaulting to `127.0.0.1`.
- **Brainstem restart awareness**: startup/restart signals are escalated more clearly.

### Fixed
- Stale idle-time markers and heartbeat accumulation cleanup.
- Context-assembly and truncation cleanup around proactive notifications.

## v0.10.8 - 2026-02-21

### Fixed
- **Context leakage under recall pressure**: transient recall no longer evicts recent short-term conversation state when over budget.

### Changed
- User/assistant messages now use a simple plain timestamp prefix, while XML markup is kept for tool messages only.

## v0.10.7 - 2026-02-20

### Changed
- Context assembly was refactored around explicit timeline/XML blocks.
- Prompt caching was enabled across providers.
- Heartbeats were extended to support proactive communication.

## v0.10.6 - 2026-02-16

### Fixed
- Container startup now uses `uv run --no-sync lethe` to avoid runtime writes to `/app/.venv` that can fail under macOS Podman/Docker UID mapping (`Permission denied` on `.venv/bin/lethe`).

## v0.10.5 - 2026-02-16

### Changed
- Installer shell compatibility improved for macOS defaults: removed Bash 4 requirement and replaced associative-array usage with Bash 3.x compatible provider mapping helpers.
- Container-mode installer now skips local Node/agent-browser/uv/Python setup and focuses on host prerequisites + container runtime.
- macOS container runtime selection now prefers Docker when both Docker and Podman are available; Podman auto-install remains supported.

### Fixed
- Docker image dependency resolution on macOS/container installs now uses runtime-only sync (`uv sync --frozen --no-dev`) and no longer forces a broad extra index, avoiding false unsatisfiable `pillow`/`lethe[dev]` resolution failures.
- Installer provider detection no longer triggers `DETECTED_PROVIDERS[*] unbound variable` under Bash `set -u`.
- Container runtime env now sets cache paths under `/workspace/.cache` to prevent uv cache permission errors at `/app/.cache/uv`.

## v0.10.4 - 2026-02-16

### Changed
- System actor `user_notify` routing is now strictly cortex-mediated: `brainstem`, `dmn`, and `amygdala` notifications are deferred to cortex instead of being auto-forwarded to the user.

### Fixed
- DMN urgent notifications no longer bypass cortex; cortex remains the only conversational agent deciding if/how to relay.

## v0.10.3 - 2026-02-16

### Changed
- Native updater now handles dirty repositories safely by creating a git-stash backup (including untracked files) before update, with automatic restore on failure and explicit recovery instructions.
- Brainstem auto-update no longer hard-skips dirty repos; it proceeds through the updater backup path and reports that behavior to cortex.
- Console context tabs updated: `LLM` renamed to `Cortex`, and a new `Stem` tab added for Brainstem context monitoring.

### Fixed
- Cache hit percentage in web console is now bounded and computed from total input (cached + uncached), preventing impossible values above 100%.
- Cache read/write totals are no longer double-counted when both unified and provider-native usage fields are present.
- Runtime artifact hygiene improved via `.gitignore` updates to reduce accidental install-repo dirtiness.

## v0.10.2 - 2026-02-16

### Added
- Explicit DMN model override config via `LLM_MODEL_DMN` (fallback remains automatic).
- Brainstem Anthropic unified ratelimit awareness with configurable warning thresholds.
- Brainstem successful self-update now emits a user-facing restart availability notice via cortex.

### Changed
- DMN now uses aux model by default unless explicit DMN model is configured.
- Brainstem supervision moved to main heartbeat cadence (default 15 minutes) for regular low-cost checks.
- Heartbeat/README/docs updated to reflect shared cadence for DMN, Amygdala, and Brainstem.
- Hippocampus recall payloads now apply hard caps and conversation-entry filtering to reduce noisy/oversized recall context.

### Fixed
- Anthropic OAuth response headers are now captured and exposed for runtime supervision.
- Brainstem now escalates near-limit Anthropic utilization and non-allowed unified status to cortex/user notify path.
- Intermediate assistant progress updates are now emitted only after successful tool execution, reducing progress spam.

## v0.10.1 - 2026-02-15

### Changed
- Inter-actor signaling migrated from in-band text tags to structured metadata channels (`channel` / `kind`).
- Actor tool `send_message(...)` extended with explicit signaling fields for channel-based routing.
- Background insight delivery flow tightened to `DMN/Amygdala -> Cortex -> User`, with policy enforcement in cortex.

### Fixed
- Subagent completion and failure results no longer bypass cortex and go directly to user output.
- Background user notifications now use throttled, de-duplicated forwarding to reduce notification spam.
- DMN direct-to-user callback path removed; background actors now escalate through cortex only.

## v0.10.0 - 2026-02-15

### Added
- Amygdala background actor on aux model with config toggle (enabled by default).
- Actor lifecycle visibility in console (`spawn`/`terminate` names in event stream).
- Prompt template externalization and workspace prompt seeding for runtime-editable behavior.
- Telegram reaction tool wiring in the base toolset and cortex toolset.

### Changed
- DMN behavior tuned for deeper background exploration, pacing, and telemetry.
- Console improved for monitoring: actor events, context panels, and safer payload rendering.
- Context truncation switched away from character caps toward line-aware handling.
- Search behavior constrained to reduce broad, noisy filesystem scans.
- Hippocampus recall filtering moved to LLM relevance policy guidance instead of regex stripping.

### Fixed
- Inbox loss and actor orchestration reliability issues.
- Missing parent notifications on actor completion/error paths.
- `telegram_react` availability regressions.
- Console leakage of image base64 payloads in self-sent image events.
- Install/update script flow for prompt/template deployment and runtime prompt discovery.

## v0.9.0 - 2026-02-14

### Changed
- DMN depth cadence, context anchoring, and telemetry were improved.

### Removed
- Committed backup template files were removed from the repo.

## v0.8.0 - 2026-02-14

### Changed
- Actor notification handling was hardened.
- Local image viewing support was enabled.
- Actor orchestration, loop safety, and context budgeting were improved.

## v0.7.1 - 2026-02-14

### Fixed
- **Compaction death spiral**: compaction now preserves tool-call/tool-result boundaries and uses safer cutoff validation.
- **Subagent model 404s**: provider prefixes are stripped correctly before OAuth calls.
- **`telegram_send_message` misuse**: tool guidance was rewritten so it is used for progress updates rather than duplicating final replies.

### Changed
- Raised the default context limit to `100000`.
- DMN gained QUICK/DEEP modes and a more proactive background-thinking prompt.

## v0.7.0 - 2026-02-14

### Added
- **Anthropic OAuth support** with direct Anthropic API calls for subscription auth.

### Changed
- OAuth now takes priority over API keys when both are available.
- Recall relevance filtering and entry trimming were tightened before memory injection.
- Recall is injected as assistant-side context instead of being concatenated onto the user message.

### Fixed
- Context wipeout on restart caused by malformed/stripped tool history.
- Search-result persistence bloat from recursive archival/conversation search outputs.
- Multiple token-efficiency issues in DMN/tool loops, including duplicate runs and wasted post-terminate API calls.

## v0.6.1 - 2026-02-10

### Changed
- Cortex now keeps CLI/file tools for direct work and only spawns subagents for longer or more complex tasks.

### Fixed
- Duplicate actor spawning was blocked with additional safeguards.
- Install/update/uninstall scripts were synced with the then-current deployment flow.

## v0.6.0 - 2026-02-10

### Added
- **Actor model architecture** with cortex, DMN, and subagents.
- **Prompt caching** with provider-aware cache behavior and console visualization.
- **Migration tooling** for the transition to the actor architecture.

### Changed
- Workspace paths are injected into agents/subagents to reduce path guessing.
- Naming migrated from `butler` to `cortex`, and `spawn_subagent` to `spawn_actor`.

## v0.5.0 - 2026-02-09

### Fixed
- Prompt caching now uses `cache_control` only for Anthropic models; non-Anthropic providers use plain system prompts without Anthropic-specific cache metadata.

## v0.4.0 - 2026-02-08

### Fixed
- Kimi tool calling now preserves the provider-required tool-call ID format for non-Anthropic models instead of sanitizing those IDs as if they were Anthropic requests.
