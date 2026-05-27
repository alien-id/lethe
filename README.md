# Lethe

[![Release](https://img.shields.io/github/v/release/atemerev/lethe?style=flat-square&color=blue)](https://github.com/atemerev/lethe/releases/latest)
[![License](https://img.shields.io/badge/license-MIT-green?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.88+-orange?style=flat-square&logo=rust)](https://www.rust-lang.org/)
![Swiss Made Software](https://img.shields.io/badge/swiss%20made-software-red?style=flat-square&labelColor=FF0000&logoColor=white)

Lethe is a long-running personal AI assistant with a brain-inspired cognitive architecture: cortex, hippocampus, brainstem, and a default-mode network running as cooperating actors. She has continuous memory across sessions, notices things on her own, delegates to focused subagents, and can read her own source — propose changes to it, restart herself with new logic. Lives on your machine as a systemd service. Persists across reboots, models, hardware upgrades.

**Written in Rust** as a single static binary. No Python runtime, no virtualenv pinball, no dependency drift between hosts. Fast cold-start, predictable memory, no GC pauses during a tool loop. The runtime ships as one ~50 MB executable plus a small migrator for legacy data. Uses `genai` as the universal LLM router and intentionally has no web console.

## Quickstart

```bash
# 1. Build (or download a binary from Releases)
cargo build --release
install -m 755 target/release/lethe ~/.local/bin/lethe

# 2. Set up — interactive prompts for provider, model, API key, workspace
lethe init

# 3. Chat
lethe chat -m "hello"
```

`lethe init` writes `~/.lethe/config/.env`, seeds the workspace and core memory blocks, and runs a smoke test against the LLM and embedding pipeline before declaring success. If you'd rather configure by hand, copy `.env.example` and edit. The first turn that uses recall/notes triggers a one-time ~150MB download of the embedding runtime and model (progress is shown).

To sign in (or re-auth) a single provider without re-running the full wizard, use `lethe login`:

```bash
lethe login openai       # asks: ChatGPT Plus/Pro subscription (default) or API key
lethe login anthropic    # asks: Claude Pro/Max subscription (default) or API key
lethe login openrouter   # API key only
```

Each command writes credentials to `~/.lethe/credentials/` (subscription) or sets the API key in `~/.lethe/config/.env`, flips `LLM_PROVIDER`, and prompts for `LLM_MODEL` / `LLM_MODEL_AUX` (defaults from the curated catalog — accept with Enter, or type any other model id).

Sanity-check an existing setup any time with `lethe check` — it pings the model and exercises the embedding pipeline rather than just printing config.

## Architecture

```
                 Telegram / HTTP API
                        |
                        v
              Cortex: user-facing agent
        memory, tools, delegation, final replies
                        |
       +----------------+----------------+
       |                |                |
       v                v                v
 Hippocampus       Actor System     Notification Pipeline
 recall over       subagents,       scoring, gating,
 notes/archive/    registry,        and proactive
 conversations     event bus        transport output
       |                |
       v                +----------------+
 Memory Stack                            |
 markdown blocks,                       v
 notes, LanceDB archive,       DMN + heartbeat
 message history              background thought
                        |
                        v
                    Tool Registry
       files, shell/PTY, browser, web, Telegram/API transport
```

Core runtime pieces:

| Area | Rust modules | Responsibility |
|------|--------------|----------------|
| Agent/cortex | `src/agent.rs` | Prompt assembly, LLM calls, tool loop, and actor turn execution. |
| LLM routing | `src/llm/` | `genai` client, OAuth (ChatGPT Plus/Pro, Claude Pro/Max) and API-key auth, OpenRouter prompt-cache forwarding via vendored genai patch, model metadata. |
| Memory | `src/memory/` | Markdown memory blocks, SQLite-vec recall tables (`memory`, `message_history`, plus their `*_vec` virtual siblings), SQLite todos. |
| Recall | `src/hippocampus.rs` | Hybrid lexical/vector recall over notes, archival memories, and conversation history. |
| Actors | `src/actor.rs`, `src/background.rs` | Resident Kameo actors, supervisor-owned state, mailbox/event routing, autonomous subagent wakeups, persistent DMN. |
| Notifications | `src/notification.rs`, `src/heartbeat.rs`, `src/runtime.rs` | Background candidate gating and proactive output limits. |
| Transports | `src/telegram.rs`, `src/api.rs`, `src/conversation.rs` | Telegram polling, HTTP/SSE API, debounce/cancel handling. |
| Tools | `src/tools/` | Filesystem, shell, PTY terminal, browser, image, web, memory, notes, todos, actors, transport tools. |

## Build

```bash
git clone https://github.com/atemerev/lethe.git
cd lethe
cp .env.example .env
cargo build --release
target/release/lethe check
```

The LanceDB dependency builds protobuf bindings, so the host needs `protoc` and protobuf headers available.

Native installer:

```bash
curl -fsSL https://lethe.gg/install | bash
~/.lethe/bin/lethe check
```

The installer downloads the latest GitHub binary release for the current platform when available. If no release asset matches, it falls back to a local Cargo build. Force source builds with `LETHE_INSTALL_FROM_SOURCE=1`.

Run tests:

```bash
cargo test
cargo build --release
```

Browser automation uses the external `agent-browser` CLI when browser tools are called.

## Running

CLI check is the default when `LETHE_MODE` is unset:

```bash
target/release/lethe check
```

Telegram long polling:

```bash
scripts/lethe-telegram-foreground
# or, without loading the foreground environment wrapper:
LETHE_MODE=telegram target/release/lethe
# or
target/release/lethe telegram run
```

HTTP API mode:

```bash
LETHE_MODE=api LETHE_API_TOKEN=change-me target/release/lethe
# or
target/release/lethe api --port 8080
```

API mode binds to `LETHE_API_HOST` (`127.0.0.1` by default). Use a reverse proxy for remote access.

## LLM Providers

Lethe routes chat through `genai`. The runtime supports both API-key and subscription-OAuth auth, plus OpenAI-compatible local servers:

| Provider | Auth | Example `LLM_MODEL` |
|----------|------|---------------------|
| Anthropic (API key) | `ANTHROPIC_API_KEY` | `claude-opus-4-7` |
| Anthropic (Claude Pro/Max) | `lethe login anthropic` → token file | `claude-opus-4-7` |
| OpenAI (API key) | `OPENAI_API_KEY` | `gpt-5.5` |
| OpenAI (ChatGPT Plus/Pro) | `lethe login openai` → token file | `gpt-5.5` |
| OpenRouter | `OPENROUTER_API_KEY` | `openrouter/moonshotai/kimi-k2.6` |
| Local OpenAI-compatible | `LLM_API_BASE` + `OPENAI_API_KEY=local` | `openai/gemma-4-31B-it-Q8_0.gguf` |

`LLM_PROVIDER` is optional but useful when a model id does not carry a provider prefix — for example `LLM_PROVIDER=openrouter` with `LLM_MODEL=moonshotai/kimi-k2.6`. Subscription auth also requires `LLM_PROVIDER=openai` or `LLM_PROVIDER=anthropic` so the router picks the OAuth path instead of looking for an API key (the `lethe login` commands set this for you).

`LLM_MODEL_AUX` defaults to the main model and is used for lightweight/background calls.

### Subscription OAuth

`lethe login openai` runs a device-code flow against `auth.openai.com`; tokens land in `~/.lethe/credentials/openai_oauth_tokens.json`. Calls then go to the Codex Responses API at `chatgpt.com/backend-api/codex/responses` using your ChatGPT Plus/Pro session — no `OPENAI_API_KEY` needed. Override the token file with `LETHE_OPENAI_OAUTH_TOKENS` or supply a raw token via `OPENAI_AUTH_TOKEN`.

`lethe login anthropic` runs a PKCE browser flow against `claude.ai/oauth/authorize`; tokens land in `~/.lethe/credentials/anthropic_oauth_tokens.json`. Override with `LETHE_ANTHROPIC_OAUTH_TOKENS` or `ANTHROPIC_AUTH_TOKEN`.

### Prompt caching

Lethe stamps cache breakpoints on the system prompt (1h-TTL persistent prefix + 5min-TTL ephemeral tail) and forwards them through to:

- **Anthropic direct** and **Anthropic OAuth** — cache_control is emitted on system blocks.
- **OpenRouter** — cache_control is emitted on system content parts, which OpenRouter forwards to upstream providers that support explicit caching (Anthropic, Qwen, Gemini explicit). Providers with automatic prefix caching (OpenAI, Grok, Moonshot/Kimi, Groq, DeepSeek, Gemini implicit) ignore the field but benefit from the stable structured shape.

Both `genai`'s native OpenAI adapter and our vendored fork now carry the patch — see [`vendor/genai/LETHE_FORK.md`](vendor/genai/LETHE_FORK.md) for the patch surface.

## Configuration

Configuration is read from process environment, a local `.env`, and `$LETHE_HOME/config/.env`.

| Variable | Description | Default |
|----------|-------------|---------|
| `LETHE_MODE` | `cli`, `telegram`, or `api` | `cli` |
| `LETHE_HOME` | Runtime root | `~/.lethe` |
| `WORKSPACE_DIR` | Workspace directory | `$LETHE_HOME/workspace` |
| `MEMORY_DIR` | Memory data directory | `$LETHE_HOME/data/memory` |
| `DB_PATH` | SQLite todo database path | `$LETHE_HOME/data/lethe.db` |
| `LOGS_DIR` | Runtime log directory | `$LETHE_HOME/logs` |
| `TELEGRAM_BOT_TOKEN` | Bot token from BotFather | required for Telegram |
| `TELEGRAM_ALLOWED_USER_IDS` | Comma-separated allowlist | all users |
| `TELEGRAM_TRANSCRIPTION_ENABLED` | Transcribe Telegram audio/voice | `true` |
| `LETHE_API_TOKEN` | Bearer or `x-lethe-token` auth for API mode | required for API |
| `LETHE_API_HOST` | API bind address | `127.0.0.1` |
| `LETHE_API_PORT` | API port | `8080` |
| `LLM_PROVIDER` | Optional provider hint | auto |
| `LLM_MODEL` | Main model | required for chat |
| `LLM_MODEL_AUX` | Auxiliary model | main model |
| `LLM_API_BASE` | Custom OpenAI-compatible base URL | unset |
| `LLM_CONTEXT_LIMIT` | Context size hint | `100000` |
| `OPENROUTER_API_KEY` | OpenRouter key | unset |
| `ANTHROPIC_API_KEY` | Anthropic key | unset |
| `ANTHROPIC_AUTH_TOKEN` | Optional Anthropic OAuth access token (raw) | unset |
| `LETHE_ANTHROPIC_OAUTH_TOKENS` | Optional Anthropic OAuth token file | `$CREDENTIALS_DIR/anthropic_oauth_tokens.json` |
| `OPENAI_API_KEY` | OpenAI/local-compatible key | unset |
| `OPENAI_AUTH_TOKEN` | Optional OpenAI OAuth access token (raw) | unset |
| `LETHE_OPENAI_OAUTH_TOKENS` | Optional OpenAI OAuth token file | `$CREDENTIALS_DIR/openai_oauth_tokens.json` |
| `EXA_API_KEY` | Exa search/fetch tools | unset |
| `LETHE_SEMANTIC_SEARCH_ENABLED` | Enable LanceDB vector search | `true` |
| `LETHE_EMBEDDING_PROVIDER` | `fastembed` or `hash` | `fastembed` |
| `LETHE_EMBEDDING_MODEL` | FastEmbed model id | `Snowflake/snowflake-arctic-embed-m-v2.0` |
| `ACTORS_ENABLED` | Enable actor/subagent system | `true` |
| `HIPPOCAMPUS_ENABLED` | Enable associative recall | `true` |
| `CURATOR_ENABLED` | Enable memory curator | `true` |
| `HEARTBEAT_ENABLED` | Enable proactive heartbeat loop | `true` |
| `HEARTBEAT_INTERVAL` | Heartbeat interval seconds | `3600` |
| `PROACTIVE_MAX_PER_DAY` | Proactive message daily limit | `4` |
| `PROACTIVE_COOLDOWN_MINUTES` | Minimum spacing for proactive messages | `60` |
| `TRANSCRIPTION_PROVIDER` | `auto`, `openrouter`, `openai`, or `local` | `auto` |
| `TRANSCRIPTION_MODEL` | STT model override | provider default |
| `TRANSCRIPTION_LANGUAGE` | Optional language hint | auto |
| `TRANSCRIPTION_LOCAL_COMMAND` | Local Whisper command | `whisper` |

## Memory

Lethe stores runtime state under the workspace and data directories:

- `workspace/memory/identity.md` -- persona and identity, user-editable.
- `workspace/memory/human.md` -- facts about the user.
- `workspace/memory/project.md` -- current project/context.
- `workspace/notes/` -- tagged markdown notes.
- `$MEMORY_DIR/lethe-memory.db` -- SQLite-vec database with `memory` (archival + notes, with `note-<uuid>` and `mem-<uuid>` ids), `message_history`, and their `*_vec` virtual siblings for embedding search.
- SQLite database at `$DB_PATH` -- todos.

The legacy LanceDB layout at `$MEMORY_DIR/lancedb/` is what `lethe-migrate` reads from when upgrading a pre-0.19 install — see the *Migrating from v0.18* section below.

Core memory block defaults and prompt templates are embedded into the binary, so `lethe check` and first startup work without copying prompt files into the workspace.

## Backup & Restore

Pack the workspace, agent state (memory + history), and `.env` into a single tar.gz archive:

```bash
lethe backup                              # ./lethe-backup-YYYYMMDD-HHMMSS.tar.gz
lethe backup --output ~/backups/lethe.tgz
```

The archive is written with `0600` permissions because it contains the `.env` secrets — keep it private.

Restore an archive into the current `$LETHE_HOME`:

```bash
lethe restore lethe-backup-20260525-160522.tar.gz
lethe restore archive.tgz --yes          # skip prompts (for scripts / non-TTY)
```

Restore prompts before overwriting an existing **workspace** and again before overwriting an existing **`.env`** — declining either keeps the local copy intact. Memory and history are restored unconditionally (that is the point of restoring).

## Migrating from v0.18 (LanceDB → SQLite-vec)

v0.19 moved memory storage from LanceDB to SQLite-vec. If you ran a pre-0.19 Lethe, use the one-shot `lethe-migrate` tool to copy your `archival_memory`, `message_history`, and `notes` into the new layout.

`install.sh` and binary release tarballs ship `lethe-migrate` alongside `lethe`, so if you installed Lethe through the installer you already have it at `~/.lethe/bin/lethe-migrate`. Source builders can build it explicitly with `cargo build --release --manifest-path migrator/Cargo.toml` — it's a standalone Cargo project so the Arrow/LanceDB stack stays out of the main `lethe` build.

**Recommended workflow** (no destructive step until you've verified):

```bash
# 1. Dry-run: writes to lethe-memory.db.dryrun, runs full verification.
lethe-migrate \
  --lancedb-dir  ~/.lethe/data/memory/lancedb \
  --sqlite-path  ~/.lethe/data/memory/lethe-memory.db \
  --dry-run

# 2. Inspect the dry-run file if you want, then run for real.
lethe-migrate \
  --lancedb-dir  ~/.lethe/data/memory/lancedb \
  --sqlite-path  ~/.lethe/data/memory/lethe-memory.db

# 3. Smoke-test the new storage.
lethe check
lethe memory recall -m "<something you remember>"

# 4. The old LanceDB directory is never touched. After step 3 looks good,
#    back it up, move it, or delete it — the migrator prints the full
#    path on success.
```

Flags: `--dry-run`, `--force` (overwrite an existing destination), `--embedding-dim N` (override the 768-dim guard if you used a non-default embedding model). Exit codes and the full data contract are in [`migrator/README.md`](migrator/) and [`MIGRATION-SPEC.md`](MIGRATION-SPEC.md).

## Logging

Lethe writes structured runtime logs to `$LOGS_DIR/lethe.log` and mirrors them to stderr. The default level is `info`; override it with `RUST_LOG`, for example:

```bash
RUST_LOG=debug scripts/lethe-telegram-foreground
tail -f ~/.lethe/logs/lethe.log
```

Telegram turns, LLM responses, tool calls, tool results, heartbeat failures, and background actor update relay failures are logged for post-mortem debugging.

Full LLM request/response dumps are opt-in because they contain prompts, memory, tool schemas, tool results, and attachments:

```bash
LLM_DEBUG=true scripts/lethe-telegram-foreground
ls ~/.lethe/logs/llm/
```

Override the dump directory with `LLM_DEBUG_DIR`.

## API

All API routes require `Authorization: Bearer <LETHE_API_TOKEN>` or `x-lethe-token`.

| Route | Method | Purpose |
|-------|--------|---------|
| `/health` | `GET` | Readiness check. |
| `/chat` | `POST` | Send a user message and receive SSE response events. |
| `/events` | `GET` | Subscribe to proactive SSE events. |
| `/cancel` | `POST` | Cancel active work for a chat. |
| `/configure` | `POST` | Store user metadata in memory. |
| `/model` | `GET`/`POST` | Inspect or update main/aux model ids. |
| `/file?path=...` | `GET` | Serve a workspace file. |

## Local llama.cpp Example

Start an OpenAI-compatible server:

```bash
./build/bin/llama-server \
  --model /path/to/gemma-4-31B-it-Q8_0.gguf \
  --host 0.0.0.0 --port 8090 \
  --ctx-size 98304 \
  --jinja
```

Configure Lethe:

```bash
LLM_PROVIDER=openai
LLM_MODEL=openai/gemma-4-31B-it-Q8_0.gguf
LLM_API_BASE=http://localhost:8090/v1
OPENAI_API_KEY=local
LLM_CONTEXT_LIMIT=96000
```

## Development

```bash
cargo fmt --check
cargo test
cargo build --release
```

Build a local release archive:

```bash
cargo build --release
scripts/package-release
ls dist/
```

Tagged pushes (`v*`) build GitHub release assets on a four-runner matrix — `linux-x86_64`, `linux-aarch64`, `macos-x86_64`, `macos-aarch64` — each producing one `lethe-<target>.tar.gz` plus a sibling `lethe-migrate-<target>.tar.gz` (`install.sh` and `update.sh` consume the `lethe-*` assets from the latest release). Linux gnu binaries are built on `ubuntu-22.04(-arm)` for a glibc 2.35 floor; macOS binaries link only against system frameworks.

Useful smoke checks:

```bash
target/release/lethe check
target/release/lethe telegram split "hello from lethe"
```

## License

MIT
