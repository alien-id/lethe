# Lethe

[![Release](https://img.shields.io/github/v/release/atemerev/lethe?style=flat-square&color=blue)](https://github.com/atemerev/lethe/releases/latest)
[![License](https://img.shields.io/badge/license-MIT-green?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.88+-orange?style=flat-square&logo=rust)](https://www.rust-lang.org/)
![Swiss Made Software](https://img.shields.io/badge/swiss%20made-software-red?style=flat-square&labelColor=FF0000&logoColor=white)

Lethe is a long-running personal AI assistant with a brain-inspired cognitive architecture: cortex, hippocampus, brainstem, and a default-mode network running as cooperating actors. She has continuous memory across sessions, notices things on her own, delegates to focused subagents, and can read her own source — propose changes to it, restart herself with new logic. Lives on your machine as an isolated, rootless container (or, with `--yolo`, natively as a systemd/launchd service). Persists across reboots, models, hardware upgrades.

Written in Rust as a single ~50 MB static binary. Routes LLM traffic through `genai`. Intentionally has no web console.

## Quickstart

```bash
# Download the latest release and run the setup wizard
curl -fsSL https://lethe.gg/install | bash
```

The installer drops a prebuilt binary at `~/.lethe/bin/lethe` and hands off to `lethe init`, which walks you through provider, model, API key, and identity — then deploys Lethe as an **isolated, rootless container** (the default: Podman on Linux, Apple Container on macOS, installed for you if missing). Pass `--yolo` to skip the container and run natively on the host instead.

Prefer to build from source?

> Linux linker note: check `.cargo/config.toml` first. If it points at `mold`, install it before building (`sudo dnf install mold` or `sudo apt-get install mold`) or adjust the linker setting for your system.

```bash
git clone https://github.com/atemerev/lethe.git
cd lethe
cargo build --release
install -m 755 target/release/lethe ~/.local/bin/lethe
lethe init            # add --yolo for a native (uncontained) setup
```

`lethe init` writes `~/.lethe/config/.env`, seeds the workspace and core memory blocks, and runs a smoke test against the LLM and embedding pipeline before declaring success. It runs **non-interactively** when stdin isn't a terminal (Docker/CI): pass `--provider`/`--model`/`--aux-model` and supply the key via the provider's env var (e.g. `OPENROUTER_API_KEY`). If you'd rather configure by hand, copy `.env.example` and edit. The first turn that uses recall/notes triggers a one-time ~150MB download of the embedding runtime and model (progress is shown).

Then check on her:

```bash
lethe                 # status: version + current config (no live probes)
lethe check           # live health check (LLM + embeddings)
lethe chat -m "hello" # one-off message straight to the model
```

To sign in (or re-auth) a single provider without re-running the full wizard, use `lethe login`:

```bash
lethe login openai       # asks: ChatGPT Plus/Pro subscription (default) or API key
lethe login anthropic    # asks: Claude Pro/Max subscription (default) or API key
lethe login openrouter   # API key only
lethe login opencode-go  # API key only
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
 notes, SQLite-vec index,     DMN + heartbeat
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
| LLM routing | `src/llm/` | `genai` client, OAuth (ChatGPT Plus/Pro, Claude Pro/Max) and API-key auth, per-model prompt-cache dialects, OpenRouter prompt-cache forwarding via vendored genai patch, model metadata. |
| Memory | `src/memory/` | Markdown memory blocks, SQLite-vec recall tables (`memory`, `message_history`, plus their `*_vec` virtual siblings), SQLite todos. |
| Recall | `src/hippocampus.rs` | Hybrid lexical/vector recall over notes, archival memories, and conversation history. |
| Actors | `src/actor.rs`, `src/actor/` | Resident Kameo actors, supervisor-owned state, mailbox/event routing, autonomous subagent wakeups, persistent DMN, SQLite-backed actor snapshots that survive restarts. |
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

Browser automation uses the vault-sealed Alien Browser exclusively; the former
standalone `agent-browser`/`browser_*` integration has been removed. See
[Alien agent-id](#alien-agent-id).

## Running

Lethe's default home is an isolated, rootless container managed as a background service — `lethe init` sets that up for you. The CLI drives and inspects the whole deployment.

**Deploy & manage**

```bash
lethe run                      # run in the foreground here (Ctrl-C to stop); --yolo for native
lethe service install --now    # install + start the background service (systemd user unit / launchd agent)
lethe service status           # platform, unit path, live status
lethe container up             # build image (if needed), create the container, install + start the service
lethe container status         # engine, container state, shared mounts
lethe container logs -f        # follow the container logs
lethe container shell          # root shell inside the running container
lethe container up --rebuild   # rebuild the image and recreate the container
lethe container down           # stop the container
lethe uninstall                # remove the service/container (add --purge to also delete ~/.lethe)
```

Share extra host directories with the container via `lethe container up --mount host[:container]` (repeatable; persisted).

**Reach her**

```bash
lethe transport list                       # API + Telegram channels and their status
lethe transport api --port 1373 --token    # configure the local HTTP API (powers the TUI); --token mints a fresh one
lethe transport telegram --enable          # configure + enable the Telegram bot
```

Under the hood a single `lethe api` process hosts the HTTP/SSE transport **and** the Telegram poller (when `TELEGRAM_BOT_TOKEN` is set) in the same address space, sharing one Agent, one actor registry, and one Brainstem (the sole source of heartbeats / proactive emissions — transports just subscribe and forward). API mode binds to `LETHE_API_HOST` (`127.0.0.1` by default) on `LETHE_API_PORT` (`1373`); use a reverse proxy for remote access.

**Configure on the fly**

```bash
lethe status                   # version + current config, secrets censored (this is also the bare `lethe`)
lethe identity set --name "…"  # change who she is (name + persona); `lethe identity edit` opens $EDITOR
lethe model                    # show current model + catalog; `lethe model <id>` or `--pick` to change
lethe login anthropic          # (re-)auth a single provider
lethe completions fish         # print a shell completion script
```

Add `--config <PATH>` to any command to point at a different `.env`. Low-level/debug subcommands (`memory`, `fs`, `sh`, `todo`, `agent`, …) are hidden but still work — `lethe help <command>`.

### Terminal UI

```bash
lethe tui                                       # local API
lethe tui --url http://host:1373 --token $LETHE_API_TOKEN
```

Inline tool cards, an actors/todos sidebar, streaming assistant text
(Anthropic + OpenAI OAuth providers), `@`-prefix workspace path
autocomplete, and slash commands (`/help`, `/clear`, `/cancel`,
`/todos`, `/actors`, `/model`, `/quit`).

## LLM Providers

Lethe routes chat through `genai`. The runtime supports both API-key and subscription-OAuth auth, plus OpenAI-compatible local servers:

| Provider | Auth | Example `LLM_MODEL` |
|----------|------|---------------------|
| Anthropic (API key) | `ANTHROPIC_API_KEY` | `claude-opus-4-7` |
| Anthropic (Claude Pro/Max) | `lethe login anthropic` → token file | `claude-opus-4-7` |
| OpenAI (API key) | `OPENAI_API_KEY` | `gpt-5.5` |
| OpenAI (ChatGPT Plus/Pro) | `lethe login openai` → token file | `gpt-5.5` |
| OpenRouter | `OPENROUTER_API_KEY` | `openrouter/moonshotai/kimi-k2.6` |
| OpenCode Go | `OPENCODE_GO_API_KEY` | `opencode-go/kimi-k2.6` |
| Local OpenAI-compatible | `LLM_API_BASE` + `OPENAI_API_KEY=local` | `openai/gemma-4-31B-it-Q8_0.gguf` |

[OpenCode Go](https://opencode.ai/zen/go) is a budget-friendly gateway ($5–$10/month) to a curated set of open models. Unlike the other providers it speaks **different wire protocols per model** — some models expect the OpenAI API, others the Anthropic Messages API — so each catalog entry declares its protocol and the router selects the matching adapter automatically. No subscription/OAuth path; API key only.

`LLM_PROVIDER` is optional but useful when a model id does not carry a provider prefix — for example `LLM_PROVIDER=openrouter` with `LLM_MODEL=moonshotai/kimi-k2.6`. Subscription auth also requires `LLM_PROVIDER=openai` or `LLM_PROVIDER=anthropic` so the router picks the OAuth path instead of looking for an API key (the `lethe login` commands set this for you).

`LLM_MODEL_AUX` defaults to the main model and is used for lightweight/background calls.

### Subscription OAuth

`lethe login openai` runs a device-code flow against `auth.openai.com`; tokens land in `~/.lethe/credentials/openai_oauth_tokens.json`. Calls then go to the Codex Responses API at `chatgpt.com/backend-api/codex/responses` using your ChatGPT Plus/Pro session — no `OPENAI_API_KEY` needed. Override the token file with `LETHE_OPENAI_OAUTH_TOKENS` or supply a raw token via `OPENAI_AUTH_TOKEN`.

`lethe login anthropic` runs a PKCE browser flow against `claude.ai/oauth/authorize`; tokens land in `~/.lethe/credentials/anthropic_oauth_tokens.json`. Override with `LETHE_ANTHROPIC_OAUTH_TOKENS` or `ANTHROPIC_AUTH_TOKEN`.

### Prompt caching

Lethe stamps cache breakpoints on the system prompt — a 1h-TTL prefix (identity, persona, instructions) plus a 5min-TTL tail (clock, memory state, recall) — but only for models that need an explicit breakpoint. Which marker a model gets is decided by its dialect in [`src/llm/dialect.rs`](src/llm/dialect.rs):

- **Anthropic direct** and **Anthropic OAuth** — emitted on system blocks, 1h + 5min.
- **OpenRouter → Anthropic** — emitted on system content parts. OpenRouter relays `cache_control` to the upstream vendor (converting between the Anthropic and OpenAI marker formats), and Anthropic is the only vendor accepting the extended `ttl: "1h"`.
- **OpenRouter → Gemini / Qwen** — explicit breakpoints too, but 5min only: the 1h TTL is Anthropic-only.
- **Everything else** — no marker. OpenAI, Grok, Moonshot/Kimi, Groq, DeepSeek and Z.AI/GLM cache automatically, so a breakpoint buys nothing.

See [OpenRouter's prompt-caching docs](https://openrouter.ai/docs/features/prompt-caching) for the per-vendor rules. Upstream `genai` only supports request-level `cache_control` (OpenAI's native `prompt_cache_retention`), which does not cover the OpenRouter route — forwarding it per-message is the one patch our vendored fork carries. See [`vendor/genai/LETHE_FORK.md`](vendor/genai/LETHE_FORK.md).

## Configuration

Configuration is read from process environment, a local `.env`, and `$LETHE_HOME/config/.env`.

| Variable | Description | Default |
|----------|-------------|---------|
| `LETHE_MODE` | `cli`, `telegram`, or `api` | `cli` |
| `LETHE_HOME` | Runtime root | `~/.lethe` |
| `LETHE_AGENT_NAME` | Assistant name (see `lethe identity`) | `lethe` |
| `LETHE_CONFIG_FILE` | Config `.env` path (also `--config`) | `$LETHE_HOME/config/.env` |
| `WORKSPACE_DIR` | Workspace directory | `$LETHE_HOME/workspace` |
| `MEMORY_DIR` | Memory data directory | `$LETHE_HOME/data/memory` |
| `DB_PATH` | SQLite todo database path | `$LETHE_HOME/data/lethe.db` |
| `LOGS_DIR` | Runtime log directory | `$LETHE_HOME/logs` |
| `TELEGRAM_BOT_TOKEN` | Bot token from BotFather | required for Telegram |
| `TELEGRAM_ALLOWED_USER_IDS` | Comma-separated allowlist | all users |
| `TELEGRAM_TRANSCRIPTION_ENABLED` | Transcribe Telegram audio/voice | `true` |
| `LETHE_API_TOKEN` | Bearer or `x-lethe-token` auth for API mode | required for API |
| `LETHE_API_HOST` | API bind address | `127.0.0.1` |
| `LETHE_API_PORT` | API port | `1373` |
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
| `OPENCODE_GO_API_KEY` | OpenCode Go key | unset |
| `EXA_API_KEY` | Exa search/fetch tools | unset |
| `LETHE_SEMANTIC_SEARCH_ENABLED` | Enable vector recall (fallback is keyword search) | `true` |
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
- `$MEMORY_DIR/lethe-memory.db` -- SQLite-vec database with `memory` (archival + notes, with `note-<uuid>` and `mem-<uuid>` ids), `message_history`, their `*_vec` virtual siblings for embedding search, plus `todos` (with `parent_id` subtasks) and `actors` (snapshots of subagent state).
- SQLite database at `$DB_PATH` -- legacy todos location, migrated into `lethe-memory.db` on first run.

Unfinished work is first-class state, not conversation residue:

- In-progress and overdue todos are injected into every system prompt as `<active_tasks>` — the agent sees its own open work without having to remember to ask.
- The heartbeat receives an open-work digest (unfinished subagents — including blocked ones — and in-progress/overdue todos) and never skips a tick while that digest is non-empty.
- Subagent state is snapshotted to the `actors` table on every change. After a restart (deploy, crash, self-upgrade) unfinished subagents are restored with their goals, task state, turn budget, and last checkpoint, and resume automatically.
- When a turn hits its tool budget, the agent is forced to emit a resumable GOAL / DONE / REMAINING / NEXT checkpoint instead of a truncated answer; subagents see their own previous checkpoint each turn, and a subagent that runs out of turns hands its checkpoint to its parent for a successor.

Core memory block defaults and prompt templates are embedded into the binary, so `lethe check` and first startup work without copying prompt files into the workspace.

Upgrading from a pre-0.19 install? See [`MIGRATION.md`](MIGRATION.md) for the one-shot `lethe-migrate` workflow that moves legacy LanceDB data into the new layout.

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
| `/events` | `GET` | Subscribe to brainstem + actor SSE events. |
| `/cancel` | `POST` | Cancel active work for a chat. |
| `/configure` | `POST` | Store user metadata in memory. |
| `/model` | `GET`/`POST` | Inspect or update main/aux model ids. |
| `/file?path=...` | `GET` | Serve a workspace file. |
| `/actors` | `GET` | Snapshot of active and recently terminated actors. |
| `/todos` | `GET` | List todos (filters: `status`, `priority`, `include_completed`, `limit`). |
| `/session/history` | `GET` | Last N persisted messages (`limit`). |
| `/secure-input` | `POST` | Deliver a browser-sealed credential envelope to a pending agent-id prompt (hosted mode). |
| `/secure-input/cancel` | `POST` | Dismiss a pending secure-input request. |
| `/secure-input/pending` | `GET` | Live secure-input requests (with sealing envelope) for tab re-hydration. |

SSE event vocabulary on `/chat` and `/events`:

| Event | Payload | Meaning |
|-------|---------|---------|
| `turn.start` | `{chat_id}` | A new agent turn has begun. |
| `assistant.delta` | `{content}` | Streamed assistant token chunk (Anthropic + OpenAI OAuth). |
| `text` | `{content, parse_mode, message_id}` | Complete (sub-)message; submessage boundaries follow the `---` rule from `interfaces/telegram/formatting.rs`. |
| `tool.start` | `{call_id, name, args_preview}` | Tool execution started. |
| `tool.end` | `{call_id, name, success, output_preview, duration_ms}` | Tool execution finished. |
| `actor.spawned` / `actor.state` / `actor.task` / `actor.message` | `{actor_id, payload}` | Actor lifecycle events fanned out from `ActorEventBus`. |
| `usage` | `{prompt_tokens}` | Updated context window usage. |
| `typing_start` / `typing_stop` | `{}` | Compatibility hints for chat clients. |
| `secure_input.request` | `{request_id, title, description, fields, server_pub, alg, expires_at, …}` | The agent needs a human-typed secret; render a sealed credential card (hosted). |
| `secure_input.resolved` | `{request_id, outcome}` | A secure-input request was `submitted` / `expired` / `cancelled`. |
| `agent_id.bound` | `{owner_sub, jkt}` | Background owner-binding completed. |
| `done` | `{}` | Turn complete; safe to close the stream. |

## Alien agent-id

Each Lethe instance can carry its own **Alien agent identity** (Ed25519, L0
self-asserted out of the box; optionally bound to a human owner via the Alien app
for L1/L2 assurance) and an **encrypted credential vault**, and drive a
**vault-sealed browser** (headless on a server or in a container, headed when a
display is available). These are provided by the
[`agent-id`](https://github.com/alien-id/agent-id) CLIs (`agent-id-core`,
`agent-id-vault`, `agent-id-browser`); Lethe shells out to them.

Enable identity + vault by installing the two published CLIs so they're on `PATH`:

```bash
npm i -g @alien-id/agent-id-core @alien-id/agent-id-vault   # identity + vault
```

`lethe init` provisions an L0 identity + vault automatically when the CLIs are
present; the daemon re-provisions on start. State is isolated per instance under
`AGENT_ID_STATE_DIR` (default `<LETHE_HOME>/agent-id`).

### Browser tools (optional)

The vault-sealed browser adds the `alien_browser_*` tools (`_open` starts a
session, `_act` runs any page verb — snapshot/click/type/navigate/… — and
`_fill_secret` / `_fill_otp` inject vaulted credentials the model never sees).
Because it's a superset of the built-in browser, **it replaces it**: whenever the
vault-sealed browser is active the plain `browser_*` tools are hidden, so the
agent only ever sees one browser. `agent-id-browser` is **marketplace-only — not
on npm**; install it from the plugin tarball and point `AGENT_ID_BROWSER_BIN` at
it (or put it on `PATH`). It drives **real Google Chrome** via `channel:"chrome"`
(a stealth-tuned patchright launch — not bundled Chromium), so the host needs
Chrome installed:

```bash
# From a checkout of github.com/alien-id/agent-id:
( cd plugins/agent-id-browser && bun pm pack --destination /tmp )   # -> /tmp/alien-id-agent-id-browser-*.tgz
npm i -g /tmp/alien-id-agent-id-browser-*.tgz    # pulls core/vault + patchright
# …and install Google Chrome (google-chrome-stable) on the host.
```

Chrome refuses to run as **root** with its sandbox on. In a container that runs as
root (the container is the isolation boundary), set
`AGENT_ID_BROWSER_NO_SANDBOX=1`; leave it unset on a normal desktop so the sandbox
stays on. Headed login (`alien_browser_login`) needs a display and is therefore
unavailable on a headless server — there, use the headless flow below.

**Headless flow:** `alien_browser_open` automatically creates the shared `main`
profile as an anonymous L0 cookie jar, so public pages work immediately. For a
site that needs an account, add a `login` credential *with a `login_url`* (via
`vault_add`), then use `alien_browser_auto_login` to sign in and reseal that
profile. Inspect forms with `alien_browser_inspect_form` and fill ordinary fields,
selections, checks, and workspace file uploads in one verified
`alien_browser_fill_form` call. (A site with an aggressive anti-automation wall
may still need a one-time headed sign-in on a machine with a display.)

| env var | default | meaning |
|---|---|---|
| `AGENT_ID_ENABLED` | `true` | Master switch for the integration. |
| `AGENT_ID_STATE_DIR` | `<LETHE_HOME>/agent-id` | Per-instance identity + vault state. |
| `AGENT_ID_CORE_BIN` / `AGENT_ID_VAULT_BIN` / `AGENT_ID_BROWSER_BIN` | discovered on `PATH` | Override CLI locations. |
| `AGENT_ID_BROWSER_NO_SANDBOX` | unset | Set `1` to keep Chrome's `--no-sandbox` (required when running as root, e.g. in a container). |
| `ALIEN_PROVIDER_ADDRESS` | — | Alien SSO provider for `agent_id_bind`. |
| `LETHE_SECURE_PROMPT` | `off` | `hosted` runs the secure-input socket server (set by lethe-hosted). |

Tools (requested on demand): `agent_id_status`, `agent_id_bind`, `agent_id_sign`,
`vault_list`, `vault_add`, `vault_remove`, `vault_set_totp`, and the browser tools
`alien_browser_login` / `_auto_login` / `_open` / `_close` / `_act` /
`_fill_secret` / `_fill_otp`.

### Security model

Secrets are kept out of the model's context by construction of the tool surface —
there is **no `vault_show` and no generic `vault_exec`** exposed to the agent. The
vault tools return metadata only; secret *values* are typed by the human (over the
hosted secure-input channel or the local loopback browser form) and are used inside
the vault-sealed browser's own session process (`fill_secret`/`fill_otp`), never
handed back to the model.

The **hosted secure-input channel** lets a headless Lethe (which cannot open a
browser) collect a human secret: a credential-collecting CLI POSTs a field spec to
a unix socket Lethe owns; Lethe surfaces it as a `secure_input.request` event
carrying a per-request ephemeral P-256 public key; the browser end-to-end-seals the
typed values (ECDH-P256 → HKDF-SHA256 → AES-256-GCM, with the request id and server
key bound as AAD) and the control plane relays **ciphertext only** — it never sees
plaintext and persists nothing. Lethe binds each socket connection to the PID of a
CLI child it launched itself (`SO_PEERCRED`), so a prompt-injected agent cannot
forge a card via the socket to harvest a freshly typed secret.

**Trust boundary — same uid.** An agent with shell access at the same uid as
`AGENT_ID_STATE_DIR` holds the vault's agent-key and can read the vault directly;
the boundary these tools enforce is against *accidental* transcript/context and
control-plane exposure, not against an actively adversarial agent that acts to
obtain a secret. An actively malicious control plane (it ships the frontend JS and
proxies the SSE stream) is likewise out of scope.

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

Tagged pushes (`v*`) build GitHub release assets on a three-runner matrix —
`linux-x86_64`, `linux-aarch64`, and `macos-aarch64` — each producing one
`lethe-<target>.tar.gz` plus its `.sha256` checksum (`install.sh` and
`update.sh` consume these assets from the latest release). Linux GNU binaries
are built on Ubuntu 24.04 for a glibc 2.39 floor; macOS binaries link only
against system frameworks.

Useful smoke checks:

```bash
target/release/lethe check
target/release/lethe telegram split "hello from lethe"
```

## License

MIT
