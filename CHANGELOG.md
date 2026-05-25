# Changelog

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
