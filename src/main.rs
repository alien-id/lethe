use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use lethe::config::{RuntimeMode, Settings};
use lethe::tools::shell::DEFAULT_TIMEOUT_SECONDS;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing_subscriber::EnvFilter;

mod cli;

#[derive(Debug, Parser)]
#[command(author, version, about, after_help = AFTER_HELP)]
struct Cli {
    /// Path to the config `.env` file. Overrides the default
    /// `~/.lethe/config/.env` (and the `LETHE_HOME`-derived location).
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

const AFTER_HELP: &str = "\
Getting started:
  lethe init       First-time setup: provider, model, API key.
  lethe            Show version + current config (no subcommand).
  lethe check      Live health check (LLM + embeddings).
  lethe chat -m \"hi\"   Send a one-off message.

Config lives in ~/.lethe/config/.env (override with --config or LETHE_HOME).
Low-level/debug subcommands (memory, fs, sh, todo, agent, …) are hidden but
still work — run `lethe help <command>` for any of them.";

#[derive(Debug, Subcommand)]
enum Command {
    /// First-time setup: provider, model, API key, workspace.
    ///
    /// Interactive by default (and sets up an isolated container unless
    /// --yolo). Runs non-interactively when stdin is not a terminal
    /// (Docker/CI): pass --provider/--model/--aux-model and supply the key via
    /// the provider's env var (e.g. OPENROUTER_API_KEY).
    Init {
        /// Provider id: openrouter | anthropic | openai | opencode-go. Skips the prompt.
        #[arg(long)]
        provider: Option<String>,
        /// Main model id. Skips the prompt (defaults to the catalog top pick).
        #[arg(long)]
        model: Option<String>,
        /// Auxiliary model id (cheap background calls). Defaults to the catalog pick.
        #[arg(long)]
        aux_model: Option<String>,
        /// Accept defaults without prompting where a value can be inferred.
        #[arg(long)]
        yes: bool,
        /// Native (uncontained) install — runs Lethe directly on the host with
        /// full access. Default is an isolated container.
        #[arg(long)]
        yolo: bool,
    },
    /// Alias for `init` — first-time setup. Defaults to a contained install;
    /// pass --yolo for a native one.
    Install {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        aux_model: Option<String>,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        yolo: bool,
    },
    /// Interactive teardown: remove the service (and container), optionally
    /// purge data with --purge.
    Uninstall {
        /// Don't prompt for the service/container removal steps.
        #[arg(long)]
        yes: bool,
        /// Also delete ~/.lethe (config, memory, workspace). Still confirmed.
        #[arg(long)]
        purge: bool,
    },
    /// Run Lethe in the foreground in this terminal (Ctrl-C to stop).
    ///
    /// Defaults to the isolated container (builds/creates it on first run);
    /// `--yolo` runs natively on the host instead. For a background service,
    /// use `lethe service install` / `lethe container up` instead.
    Run {
        /// Run natively on the host instead of in the container.
        #[arg(long)]
        yolo: bool,
    },
    /// Configure how you reach Lethe: the API (powers the TUI) and chat
    /// channels like Telegram. With no subcommand, lists them and their status.
    Transport {
        #[command(subcommand)]
        command: Option<TransportCommand>,
    },
    /// Run Lethe in an isolated, rootless container (the default deployment).
    Container {
        #[command(subcommand)]
        command: ContainerCommand,
    },
    /// Show version + current config (provider, models, which keys are set,
    /// with secrets censored). The default action when run with no subcommand.
    Status,
    /// View or change who the assistant is (name + persona). With no
    /// subcommand, prints the current identity.
    Identity {
        #[command(subcommand)]
        command: Option<IdentityCommand>,
    },
    /// Show or change the LLM model. With no argument, shows the current
    /// main/aux models and the catalog for your provider.
    Model {
        /// New main model id (catalog id or any custom id). Omit to show.
        model: Option<String>,
        /// Set the auxiliary model id (cheap background calls).
        #[arg(long)]
        aux: Option<String>,
        /// Interactively pick the main + aux models from the catalog.
        #[arg(long)]
        pick: bool,
    },
    /// Install / uninstall / inspect the background service
    /// (systemd user unit on Linux, launchd agent on macOS).
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Print a shell completion script (bash, zsh, fish, ...).
    Completions {
        /// Target shell.
        shell: clap_complete::Shell,
    },
    /// Sign in to an LLM provider (API key, or subscription OAuth).
    ///
    /// Subscription paths (ChatGPT Plus/Pro, Claude Pro/Max) store OAuth tokens
    /// under `~/.lethe/credentials/`; API-key paths write the key to
    /// `~/.lethe/config/.env`. Either way, `LLM_PROVIDER` and the model
    /// defaults are aligned to the chosen provider.
    Login {
        #[command(subcommand)]
        command: LoginCommand,
    },
    /// Live health check: confirm the model and embeddings actually respond.
    Check,
    /// Print a prompt template after workspace/config/embedded resolution.
    #[command(hide = true)]
    Prompt { name: String },
    /// Export the built-in prompt templates to ~/.lethe for editing, or list
    /// where each currently resolves from.
    Prompts {
        #[command(subcommand)]
        command: PromptsCommand,
    },
    /// Seed core memory block files from embedded defaults if they are missing.
    #[command(hide = true)]
    InitMemory,
    /// Inspect and initialize unified memory state.
    #[command(hide = true)]
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
    /// Run local filesystem tools.
    #[command(hide = true)]
    Fs {
        #[command(subcommand)]
        command: FsCommand,
    },
    /// Run local shell tools.
    #[command(hide = true)]
    Sh {
        #[command(subcommand)]
        command: ShCommand,
    },
    /// Run web search and fetch tools.
    #[command(hide = true)]
    Web {
        #[command(subcommand)]
        command: WebCommand,
    },
    /// Transcribe a local audio file through the configured STT provider.
    #[command(hide = true)]
    Transcribe {
        file_path: String,
        #[arg(long)]
        mime_type: Option<String>,
    },
    /// Manage persistent todos stored in the local SQLite database.
    #[command(hide = true)]
    Todo {
        #[command(subcommand)]
        command: TodoCommand,
    },
    /// Manage persistent markdown notes.
    #[command(hide = true)]
    Note {
        #[command(subcommand)]
        command: NoteCommand,
    },
    /// Manage long-term archival memory.
    #[command(hide = true)]
    Archive {
        #[command(subcommand)]
        command: ArchiveCommand,
    },
    /// Manage durable conversation message history.
    #[command(hide = true)]
    Messages {
        #[command(subcommand)]
        command: MessageCommand,
    },
    /// Run the persisted local agent loop.
    #[command(hide = true)]
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Run heartbeat prompt and proactive-check helpers.
    #[command(hide = true)]
    Heartbeat {
        #[command(subcommand)]
        command: HeartbeatCommand,
    },
    /// Run Telegram transport commands.
    #[command(hide = true)]
    Telegram {
        #[command(subcommand)]
        command: cli::telegram_loop::TelegramCommand,
    },
    /// Run the authenticated HTTP API server.
    #[command(hide = true)]
    Api {
        #[arg(long)]
        port: Option<u16>,
    },
    /// Launch the terminal UI. Defaults to the local `lethe api` instance
    /// (port from settings) using `LETHE_API_TOKEN` for auth.
    Tui {
        /// Base URL of the lethe API. Defaults to `http://{api.host}:{api.port}`.
        #[arg(long)]
        url: Option<String>,
        /// Bearer token. Falls back to settings (which loads `LETHE_API_TOKEN`).
        #[arg(long, env = "LETHE_API_TOKEN")]
        token: Option<String>,
    },
    /// Send one message straight to the model and print the reply (no memory or tools).
    Chat {
        #[arg(short, long)]
        message: String,
        #[arg(long)]
        system: Option<String>,
        #[arg(long)]
        aux: bool,
    },
    /// Pack workspace, agent state (memory + history), and the `.env`
    /// file into a single tar.gz archive.
    Backup {
        #[arg(short, long)]
        output: Option<String>,
    },
    /// Restore a `lethe backup` archive. Asks before overwriting an
    /// existing workspace and before overwriting an existing `.env`.
    Restore {
        archive: String,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum LoginCommand {
    /// Sign in to OpenAI. Prompts for subscription (ChatGPT Plus/Pro
    /// device-code OAuth, default) or API key.
    Openai,
    /// Sign in to Anthropic. Prompts for subscription (Claude Pro/Max
    /// browser-PKCE OAuth, default) or API key.
    Anthropic,
    /// Sign in to OpenRouter (API key only — no subscription path).
    Openrouter,
    /// Sign in to OpenCode Go (API key only — no subscription path).
    OpencodeGo,
}

#[derive(Debug, Subcommand)]
pub enum PromptsCommand {
    /// Write the built-in (overridable) prompts to <workspace>/prompts/.
    /// Existing files are kept unless --force is given.
    Export {
        /// Overwrite files that already exist.
        #[arg(long)]
        force: bool,
        /// Target directory (default: <workspace>/prompts).
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// List the overridable prompts and where each currently resolves from.
    List,
}

#[derive(Debug, Subcommand)]
pub enum IdentityCommand {
    /// Print the current name + identity (persona) block.
    Show,
    /// Set the assistant's name and (optionally) rewrite its persona.
    Set {
        /// Name to use (skips the name prompt). LETHE_AGENT_NAME.
        #[arg(long)]
        name: Option<String>,
    },
    /// Restore the embedded default Lethe identity.
    Reset {
        #[arg(long)]
        yes: bool,
    },
    /// Open the identity block in $EDITOR.
    Edit,
}

#[derive(Debug, Subcommand)]
pub enum TransportCommand {
    /// List transports and their status.
    List,
    /// Configure + enable the Telegram bot (token, allowed users).
    Telegram {
        #[arg(long)]
        enable: bool,
        #[arg(long)]
        disable: bool,
    },
    /// Configure the local HTTP API (powers the TUI). Enabled by default.
    Api {
        #[arg(long)]
        enable: bool,
        #[arg(long)]
        disable: bool,
        /// Set the API port.
        #[arg(long)]
        port: Option<u16>,
        /// Generate a fresh API token.
        #[arg(long)]
        token: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ContainerCommand {
    /// Build the image (if needed), create the persistent container, and
    /// install + start the service.
    Up {
        /// Rebuild the image and recreate the container (drops installed software).
        #[arg(long)]
        rebuild: bool,
        /// Replace an existing service unit if one is already installed.
        #[arg(long)]
        force: bool,
        /// Extra host dir to share: `host[:container]`. Repeatable; persisted.
        #[arg(long = "mount")]
        mount: Vec<String>,
        /// Enable + start without the confirmation prompt.
        #[arg(long)]
        now: bool,
        /// Print the engine commands without running them.
        #[arg(long)]
        dry_run: bool,
        /// Build the image from the repo Containerfile instead of a release binary.
        #[arg(long)]
        from_source: bool,
        /// Bake the heavy toolset (ffmpeg/python/build-essential/…) into the
        /// image. Default is a lean image; the agent installs the rest on demand.
        #[arg(long)]
        with_tools: bool,
    },
    /// Stop the container (via the service if installed).
    Down,
    /// Open a root shell inside the running container.
    Shell,
    /// Rebuild the image and recreate the container (resets installed software).
    Rebuild {
        #[arg(long)]
        dry_run: bool,
        /// Replace an existing service unit if one is already installed.
        #[arg(long)]
        force: bool,
        /// Bake the heavy toolset into the rebuilt image.
        #[arg(long)]
        with_tools: bool,
    },
    /// Show engine, container state, and shared mounts.
    Status,
    /// Show (or follow) the container logs.
    Logs {
        #[arg(short, long)]
        follow: bool,
    },
    /// Build the image only.
    Build {
        #[arg(long)]
        from_source: bool,
        #[arg(long)]
        dry_run: bool,
        /// Bake the heavy toolset into the image.
        #[arg(long)]
        with_tools: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// Write the service unit and (optionally) enable + start it.
    Install {
        /// Overwrite an existing unit. Does not stop the running service.
        #[arg(long)]
        force: bool,
        /// Enable + start immediately without the confirmation prompt.
        #[arg(long)]
        now: bool,
    },
    /// Stop, disable, and remove the service unit.
    Uninstall {
        #[arg(long)]
        yes: bool,
    },
    /// Show the detected platform, unit path, and live status.
    Status,
}

#[derive(Debug, Subcommand)]
pub enum FsCommand {
    /// Read a file with line numbers and truncation.
    Read {
        file_path: String,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// Write content to a file, creating parents as needed.
    Write { file_path: String, content: String },
    /// Replace text in a file.
    Edit {
        file_path: String,
        old_string: String,
        #[arg(default_value = "")]
        new_string: String,
        #[arg(long)]
        replace_all: bool,
    },
    /// List a directory.
    List {
        #[arg(default_value = ".")]
        path: String,
        #[arg(long)]
        show_hidden: bool,
    },
    /// Search for files by glob pattern.
    Glob {
        pattern: String,
        #[arg(default_value = ".")]
        path: String,
    },
    /// Search file contents with a regex.
    Grep {
        pattern: String,
        #[arg(default_value = ".")]
        path: String,
        #[arg(long, default_value = "*")]
        file_pattern: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum MemoryCommand {
    /// Initialize workspace memory directories and embedded block defaults.
    Init,
    /// Print memory store counts.
    Stats,
    /// Print combined prompt memory context.
    Context,
    /// Print stable and volatile prompt memory context separately.
    ContextSplit,
    /// Search recall memory and print an associative recall block.
    Recall {
        #[arg(short, long)]
        message: String,
    },
    /// Run deterministic memory curation and archival harvesting.
    Curate {
        #[arg(long)]
        force: bool,
    },
    /// List memory blocks.
    BlockList {
        #[arg(long)]
        include_hidden: bool,
    },
    /// Read one memory block.
    BlockRead { label: String },
    /// Create a memory block.
    BlockCreate {
        label: String,
        #[arg(default_value = "")]
        value: String,
        #[arg(long, default_value = "")]
        description: String,
        #[arg(long, default_value_t = lethe::memory::DEFAULT_BLOCK_LIMIT)]
        limit: usize,
        #[arg(long)]
        read_only: bool,
        #[arg(long)]
        hidden: bool,
    },
    /// Update a memory block value or description.
    BlockUpdate {
        label: String,
        #[arg(long)]
        value: Option<String>,
        #[arg(long)]
        description: Option<String>,
    },
    /// Append text to a memory block.
    BlockAppend { label: String, text: String },
    /// Replace the first matching string in a memory block.
    BlockReplace {
        label: String,
        old_string: String,
        new_string: String,
    },
    /// Delete a memory block.
    BlockDelete { label: String },
}

#[derive(Debug, Subcommand)]
pub enum ShCommand {
    /// Run a shell command with captured output or as a background process.
    Run {
        command: String,
        #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECONDS)]
        timeout: u64,
        #[arg(long)]
        background: bool,
        #[arg(long)]
        pty: bool,
    },
    /// Print environment information visible to shell tools.
    Env,
    /// Check whether a command exists in PATH.
    Which { command_name: String },
}

#[derive(Debug, Subcommand)]
pub enum WebCommand {
    /// Print whether EXA_API_KEY is configured.
    Available,
    /// Search the web through Exa.
    Search {
        query: String,
        #[arg(long, default_value_t = 10)]
        num_results: usize,
        #[arg(long)]
        include_text: bool,
        #[arg(long, default_value = "")]
        category: String,
    },
    /// Fetch full page text through Exa.
    Fetch {
        url: String,
        #[arg(long, default_value_t = 5000)]
        max_chars: usize,
    },
}

#[derive(Debug, Subcommand)]
pub enum TodoCommand {
    /// Create a new todo.
    Create {
        title: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long, default_value = "normal")]
        priority: String,
        #[arg(long)]
        due_date: Option<String>,
        #[arg(long = "tag")]
        tags: Vec<String>,
        #[arg(long)]
        source: Option<String>,
    },
    /// List todos with optional status and priority filters.
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        priority: Option<String>,
        #[arg(long)]
        include_completed: bool,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Update an existing todo.
    Update {
        todo_id: i64,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        priority: Option<String>,
        #[arg(long)]
        due_date: Option<String>,
    },
    /// Mark a todo as completed.
    Complete { todo_id: i64 },
    /// Search active todos by title or description.
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Check active todos due for a reminder.
    RemindCheck,
    /// Mark that a todo was just reminded.
    Reminded { todo_id: i64 },
    /// Delete a todo by ID.
    Delete { todo_id: i64 },
}

#[derive(Debug, Subcommand)]
pub enum NoteCommand {
    /// Create a persistent markdown note.
    Create {
        title: String,
        content: String,
        #[arg(long = "tag", value_delimiter = ',')]
        tags: Vec<String>,
        #[arg(long)]
        subdir: Option<String>,
    },
    /// List notes, optionally filtered by tags.
    List {
        #[arg(long = "tag", value_delimiter = ',')]
        tags: Vec<String>,
    },
    /// Search notes by title, tag, or body text.
    Search {
        query: String,
        #[arg(long = "tag", value_delimiter = ',')]
        tags: Vec<String>,
        #[arg(long, default_value_t = 5)]
        limit: usize,
    },
    /// Print all known note tags.
    Tags,
    /// Count markdown files in the note store.
    Reindex,
}

#[derive(Debug, Subcommand)]
pub enum ArchiveCommand {
    /// Add a long-term memory.
    Add {
        text: String,
        #[arg(long = "tag", value_delimiter = ',')]
        tags: Vec<String>,
        #[arg(long)]
        metadata: Option<String>,
    },
    /// Search long-term memories.
    Search {
        query: String,
        #[arg(long = "tag", value_delimiter = ',')]
        tags: Vec<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// List recent long-term memories.
    Recent {
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Get a long-term memory by ID.
    Get { memory_id: String },
    /// Replace the tags on a long-term memory.
    Tag {
        memory_id: String,
        #[arg(long = "tag", value_delimiter = ',')]
        tags: Vec<String>,
    },
    /// Delete a long-term memory by ID.
    Delete { memory_id: String },
}

#[derive(Debug, Subcommand)]
pub enum MessageCommand {
    /// Add a message to durable history.
    Add {
        role: String,
        content: String,
        #[arg(long)]
        metadata: Option<String>,
    },
    /// Get recent messages in chronological order.
    Recent {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Search messages by content and optional role.
    Search {
        query: String,
        #[arg(long)]
        role: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Get recent messages for a role.
    Role {
        role: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Get a message by ID.
    Get { message_id: String },
    /// Delete a message by ID.
    Delete { message_id: String },
    /// Remove stored tool outputs for recursive search tools.
    CleanupSearchResults {
        #[arg(long = "tool", value_delimiter = ',')]
        tools: Vec<String>,
    },
    /// Count stored messages.
    Count,
    /// Clear all stored messages.
    Clear,
    /// Build a recent context window under a character budget.
    Context {
        #[arg(long, default_value_t = 50)]
        max_messages: usize,
        #[arg(long, default_value_t = 50_000)]
        max_chars: usize,
    },
}

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Send one message through the memory-backed agent loop.
    Chat {
        #[arg(short, long)]
        message: String,
        #[arg(long)]
        no_recall: bool,
    },
    /// Build and print the LLM messages for one agent turn without calling the model.
    Prepare {
        #[arg(short, long)]
        message: String,
        #[arg(long)]
        no_recall: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum HeartbeatCommand {
    /// Render the next heartbeat prompt without calling the model.
    Prompt {
        #[arg(long)]
        minimal: bool,
    },
    /// Process one heartbeat through the agent.
    Trigger {
        #[arg(long)]
        minimal: bool,
        #[arg(long)]
        summarize: bool,
        #[arg(long)]
        no_recall: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse before loading settings so a global `--config` can redirect
    // where `Settings::from_env` reads the `.env` from.
    let cli = Cli::parse();
    if let Some(path) = &cli.config {
        // SAFETY: single-threaded startup, before any tasks are spawned.
        unsafe { std::env::set_var("LETHE_CONFIG_FILE", path) };
    }

    let settings = Settings::from_env();
    // Debug-level so one-shot CLI commands (status, completions, identity)
    // stay quiet on stderr; bump RUST_LOG=debug to see it.
    if let Some(log_path) = init_logging(&settings) {
        tracing::debug!(path = %log_path.display(), "logging initialized");
    } else {
        tracing::debug!("logging initialized without file output");
    }
    let command = match cli.command {
        Some(command) => command,
        // Bare `lethe` in CLI mode is a fast status view (no live probes);
        // api/telegram modes still launch their server.
        None => match settings.mode {
            RuntimeMode::Cli => return status(&settings),
            _ => default_command_for_mode(&settings.mode),
        },
    };
    use cli::handlers as h;
    match command {
        Command::Init {
            provider,
            model,
            aux_model,
            yes,
            yolo,
        }
        | Command::Install {
            provider,
            model,
            aux_model,
            yes,
            yolo,
        } => {
            cli::init::run(cli::init::InitArgs {
                provider,
                model,
                aux_model,
                yes,
                yolo,
            })
            .await
        }
        Command::Uninstall { yes, purge } => cli::uninstall::run(&settings, yes, purge),
        Command::Run { yolo } => {
            if yolo {
                h::api_command(None).await
            } else {
                cli::container::run_foreground(&settings)
            }
        }
        Command::Transport { command } => cli::transport::run(&settings, command),
        Command::Container { command } => cli::container::run(&settings, command),
        Command::Status => status(&settings),
        Command::Identity { command } => cli::identity::run(&settings, command),
        Command::Model { model, aux, pick } => cli::model::run(&settings, model, aux, pick),
        Command::Service { command } => cli::service::run(&settings, command),
        Command::Completions { shell } => {
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(shell, &mut cmd, name, &mut io::stdout());
            Ok(())
        }
        Command::Login { command } => match command {
            LoginCommand::Openai => {
                if lethe::llm::oauth_env::prompt_subscription_or_api("OpenAI", "ChatGPT Plus/Pro")?
                {
                    lethe::llm::openai_oauth::run_device_login().await?;
                    let (main, aux) = lethe::llm::oauth_env::prompt_provider_models("openai")?;
                    lethe::llm::oauth_env::update_env_after_oauth_login(
                        "openai",
                        main.as_deref(),
                        aux.as_deref(),
                        Some("OPENAI_API_KEY"),
                    )
                } else {
                    lethe::llm::oauth_env::run_api_key_login(
                        "openai",
                        "OPENAI_API_KEY",
                        "Paste OPENAI_API_KEY (https://platform.openai.com/api-keys): ",
                    )
                }
            }
            LoginCommand::Anthropic => {
                if lethe::llm::oauth_env::prompt_subscription_or_api("Anthropic", "Claude Pro/Max")?
                {
                    lethe::llm::anthropic_oauth::run_device_login().await?;
                    let (main, aux) = lethe::llm::oauth_env::prompt_provider_models("anthropic")?;
                    lethe::llm::oauth_env::update_env_after_oauth_login(
                        "anthropic",
                        main.as_deref(),
                        aux.as_deref(),
                        Some("ANTHROPIC_API_KEY"),
                    )
                } else {
                    lethe::llm::oauth_env::run_api_key_login(
                        "anthropic",
                        "ANTHROPIC_API_KEY",
                        "Paste ANTHROPIC_API_KEY (https://console.anthropic.com/settings/keys): ",
                    )
                }
            }
            LoginCommand::Openrouter => lethe::llm::oauth_env::run_api_key_login(
                "openrouter",
                "OPENROUTER_API_KEY",
                "Paste OPENROUTER_API_KEY (https://openrouter.ai/keys): ",
            ),
            LoginCommand::OpencodeGo => lethe::llm::oauth_env::run_api_key_login(
                "opencode-go",
                "OPENCODE_GO_API_KEY",
                "Paste OPENCODE_GO_API_KEY (https://opencode.ai/zen/go): ",
            ),
        },
        Command::Check => h::check().await,
        Command::Prompt { name } => h::print_prompt(&name),
        Command::Prompts { command } => cli::prompts::run(&settings, command),
        Command::InitMemory => h::init_memory(),
        Command::Memory { command } => h::memory_command(command).await,
        Command::Fs { command } => h::fs_command(command),
        Command::Sh { command } => h::sh_command(command),
        Command::Web { command } => h::web_command(command),
        Command::Transcribe {
            file_path,
            mime_type,
        } => h::transcribe_command(&file_path, mime_type.as_deref()),
        Command::Todo { command } => h::todo_command(command),
        Command::Note { command } => h::note_command(command),
        Command::Archive { command } => h::archive_command(command),
        Command::Messages { command } => h::messages_command(command),
        Command::Agent { command } => h::agent_command(command).await,
        Command::Heartbeat { command } => h::heartbeat_command(command).await,
        Command::Telegram { command } => cli::telegram_loop::telegram_command(command).await,
        Command::Api { port } => h::api_command(port).await,
        Command::Tui { url, token } => h::tui_command(url, token).await,
        Command::Chat {
            message,
            system,
            aux,
        } => h::chat(message, system, aux).await,
        Command::Backup { output } => cli::backup::backup(output),
        Command::Restore { archive, yes } => cli::backup::restore(archive, yes),
    }
}

#[derive(Clone)]
struct LogWriter {
    file: Arc<Mutex<File>>,
}

struct LogLineWriter {
    file: Arc<Mutex<File>>,
}

impl Write for LogLineWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(buf);
        let mut file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("log file lock poisoned"))?;
        file.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stderr().flush();
        let mut file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("log file lock poisoned"))?;
        file.flush()
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for LogWriter {
    type Writer = LogLineWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        LogLineWriter {
            file: self.file.clone(),
        }
    }
}

fn init_logging(settings: &Settings) -> Option<PathBuf> {
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if let Err(error) = std::fs::create_dir_all(&settings.paths.logs_dir) {
        eprintln!(
            "logging_file_unavailable: cannot create {}: {error}",
            settings.paths.logs_dir.display()
        );
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter())
            .with_target(true)
            .with_ansi(false)
            .try_init();
        return None;
    }
    let log_path = settings.paths.logs_dir.join("lethe.log");
    let file = match OpenOptions::new().create(true).append(true).open(&log_path) {
        Ok(file) => file,
        Err(error) => {
            eprintln!(
                "logging_file_unavailable: cannot open {}: {error}",
                log_path.display()
            );
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter())
                .with_target(true)
                .with_ansi(false)
                .try_init();
            return None;
        }
    };

    if let Err(error) = tracing_subscriber::fmt()
        .with_env_filter(filter())
        .with_target(true)
        .with_ansi(false)
        .with_writer(LogWriter {
            file: Arc::new(Mutex::new(file)),
        })
        .try_init()
    {
        eprintln!("logging_setup_failed: {error}");
        return None;
    }
    Some(log_path)
}

/// Fast, side-effect-free status view: version, the resolved config path,
/// and the current config with secrets censored. When no config file exists
/// yet, nudge the user to `lethe init` instead. This is what bare `lethe`
/// runs in CLI mode — no model download, no network, no SQLite open.
fn status(settings: &Settings) -> Result<()> {
    use cli::util::secret_status;

    let mut lines: Vec<String> = vec![format!("lethe {}", env!("CARGO_PKG_VERSION"))];
    let config_file = &settings.paths.config_file;
    if !config_file.exists() {
        lines.push(String::new());
        lines.push(format!("No config found at {}", config_file.display()));
        lines.push("Run `lethe init` to set up a provider, model and API key.".to_string());
        // Animate the avatar alongside the text, then leave it on screen.
        cli::avatar::play_above(&lines);
        return Ok(());
    }

    let llm = &settings.llm;
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
    let anthropic_token = std::env::var("ANTHROPIC_AUTH_TOKEN").unwrap_or_default();

    lines.push(format!("  config:    {}", config_file.display()));
    lines.push(format!(
        "  home:      {}",
        settings.paths.lethe_home.display()
    ));
    lines.push(format!(
        "  workspace: {}",
        settings.paths.workspace_dir.display()
    ));
    lines.push(format!("  mode:      {:?}", settings.mode));
    lines.push(format!("  identity:  {}", settings.agent_name));
    lines.push(format!("  provider:  {}", marker(&llm.llm_provider)));
    lines.push(format!("  model:     {}", marker(&llm.llm_model)));
    lines.push(format!("  aux model: {}", settings.effective_aux_model()));
    lines.push(format!(
        "  auth:      {}",
        lethe::llm::llm_auth_mode_for_settings(settings)
    ));
    lines.push("  keys:".to_string());
    for (label, value) in [
        ("OPENROUTER_API_KEY", llm.openrouter_api_key.as_str()),
        ("ANTHROPIC_API_KEY", anthropic_key.as_str()),
        ("ANTHROPIC_AUTH_TOKEN", anthropic_token.as_str()),
        ("OPENAI_API_KEY", llm.openai_api_key.as_str()),
        ("OPENCODE_GO_API_KEY", llm.opencode_go_api_key.as_str()),
    ] {
        lines.push(format!("    {label:<20} {}", secret_status(value)));
    }
    lines.push(format!(
        "  telegram:  {}",
        if settings.telegram.bot_token.trim().is_empty() {
            "not set".to_string()
        } else {
            format!(
                "configured ({} allowed user id(s))",
                settings.telegram.allowed_user_ids.len()
            )
        }
    ));
    lines.push(format!(
        "  api token: {}",
        secret_status(&settings.api.token)
    ));
    lines.push(String::new());
    lines.push("Run `lethe check` for a live health check (LLM + embeddings).".to_string());

    cli::avatar::play_above(&lines);
    Ok(())
}

/// Render a config string for the status view: `(not set)` when empty.
fn marker(value: &str) -> String {
    if value.trim().is_empty() {
        "(not set)".to_string()
    } else {
        value.to_string()
    }
}

fn default_command_for_mode(mode: &RuntimeMode) -> Command {
    match mode {
        RuntimeMode::Api => Command::Api { port: None },
        RuntimeMode::Telegram => Command::Telegram {
            command: cli::telegram_loop::TelegramCommand::Run {
                timeout: 30,
                no_recall: false,
            },
        },
        RuntimeMode::Cli => Command::Check,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_command_honors_runtime_mode() {
        assert!(matches!(
            default_command_for_mode(&RuntimeMode::Api),
            Command::Api { port: None }
        ));
        assert!(matches!(
            default_command_for_mode(&RuntimeMode::Telegram),
            Command::Telegram {
                command: cli::telegram_loop::TelegramCommand::Run {
                    timeout: 30,
                    no_recall: false
                }
            }
        ));
        assert!(matches!(
            default_command_for_mode(&RuntimeMode::Cli),
            Command::Check
        ));
    }
}
