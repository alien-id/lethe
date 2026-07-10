//! Interactive `lethe init` wizard:
//!
//! 1. Asks who the assistant should be (custom identity, or keep the default
//!    Lethe persona).
//! 2. Detects any LLM keys already in the environment / config `.env`.
//! 3. Walks the user through provider / model / key / API-token choices.
//!    Provider + models can be supplied via flags to skip prompts.
//! 4. Merges the result into the config `.env` (preserving every other key
//!    and comment), seeds the workspace + default memory blocks, applies a
//!    custom identity/human intro.
//! 5. Runs a smoke test, offers to install a background service, and prints
//!    what to run next.
//!
//! When stdin is not a terminal (Docker / CI) it runs non-interactively:
//! provider from `--provider`/`LLM_PROVIDER`, key from the provider's env
//! var, models from flags or the catalog defaults.

use std::io::{self, IsTerminal};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use lethe::config::Settings;
use lethe::llm::models::{ModelEntry, model_catalog};
use lethe::llm::{LlmMessage, LlmRouter, LlmRouterConfig};
use lethe::memory::MemoryStore;

use crate::cli::identity;
use crate::cli::util::{
    confirm, mask_secret, prompt_line, prompt_secret, read_multiline, upsert_env,
};

/// Flags accepted by `lethe init`. All optional — the interactive flow
/// prompts for anything not supplied; the non-interactive flow requires
/// at least a resolvable provider + key.
#[derive(Debug, Default)]
pub struct InitArgs {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub aux_model: Option<String>,
    pub yes: bool,
    /// Native (uncontained) install. Default is an isolated container.
    pub yolo: bool,
}

/// Top-level entry point. Dispatches on whether we have a real terminal.
pub async fn run(args: InitArgs) -> Result<()> {
    if io::stdin().is_terminal() {
        run_interactive(args).await
    } else {
        run_noninteractive(args).await
    }
}

// =============================================================================
// Interactive flow
// =============================================================================

async fn run_interactive(args: InitArgs) -> Result<()> {
    let settings = Settings::from_env();
    let env_path = settings.paths.config_file.clone();
    let existing_env = read_existing_env(&env_path);

    // Animate the avatar above the setup header (visible while it plays).
    let header = vec![
        "Lethe — guided setup".to_string(),
        "────────────────────".to_string(),
        String::new(),
        "Stores config + state under:".to_string(),
        format!("  {}", settings.paths.lethe_home.display()),
        format!("Config file: {}", env_path.display()),
    ];
    crate::cli::avatar::play_above(&header);
    println!();

    // If a usable config already exists, offer a quick exit instead of
    // walking the whole flow every time.
    if env_path.exists() && !settings.llm.llm_model.trim().is_empty() {
        let provider = if settings.llm.llm_provider.trim().is_empty() {
            "?".to_string()
        } else {
            settings.llm.llm_provider.clone()
        };
        println!(
            "You already have a config (provider {provider}, model {}).",
            settings.llm.llm_model
        );
        let go = prompt_line("Reconfigure it? [y/N]: ")?;
        if !go.trim().to_ascii_lowercase().starts_with('y') {
            println!("Nothing changed. Run `lethe check` to verify, or `lethe chat -m \"hi\"`.");
            return Ok(());
        }
        println!();
    }

    // -- Identity (who the assistant is) — asked first ----------------------
    let identity = identity::prompt_identity(&settings.agent_name)?;

    let detected = detect_keys(&existing_env);
    if !detected.is_empty() {
        println!("\nFound existing API keys for: {}", detected.join(", "));
    }

    // -- Provider -----------------------------------------------------------
    let provider = match args.provider.as_deref() {
        Some(p) => Provider::from_id(p).ok_or_else(|| {
            anyhow!("unknown --provider `{p}` (use openrouter|anthropic|openai|opencode-go)")
        })?,
        None => prompt_provider(&detected)?,
    };
    info(&format!("Using {}", provider.label()));

    // -- Models -------------------------------------------------------------
    let (main_model, aux_model) = resolve_models(provider, &args)?;
    info(&format!("Main: {main_model}"));
    info(&format!("Aux:  {aux_model}"));

    // -- API key (or subscription OAuth) ------------------------------------
    let api_key = prompt_api_key(provider, &existing_env).await?;

    // -- Optional HTTP API / TUI token --------------------------------------
    let api_token = prompt_api_token(&existing_env)?;

    // -- Optional Telegram (always skippable) -------------------------------
    let telegram = prompt_telegram(&existing_env)?;

    // -- Optional human-block intro -----------------------------------------
    let human_intro = prompt_human_intro()?;

    // -- Persist (merge — never clobbers other keys) ------------------------
    write_config(
        &env_path,
        provider,
        &main_model,
        &aux_model,
        api_key.as_deref(),
        api_token.as_deref(),
        identity.as_ref().map(|i| i.name.as_str()),
        telegram.as_ref(),
    )?;
    info(&format!("Updated {}", env_path.display()));

    seed_workspace(&settings)?;
    if let Some(setup) = &identity {
        identity::apply_identity(&settings, setup)?;
        info(&format!("Identity set to {}", setup.name));
    }
    if let Some(text) = human_intro {
        seed_human_block(&settings, &text)?;
    }
    info("Seeded workspace + memory blocks.");
    provision_agent_id(&settings).await;

    // -- Smoke test ---------------------------------------------------------
    println!("\nRunning smoke test...");
    smoke_test(provider, &main_model, &aux_model, api_key.as_deref()).await?;

    // -- Deploy: isolated container (default) or native (--yolo) ------------
    deploy(&settings, args.yolo)?;

    print_next_steps(api_token.is_some(), args.yolo, telegram.is_some());
    Ok(())
}

/// Set up how Lethe runs: an isolated rootless container by default (so the
/// agent can install software without touching the host), or directly on the
/// host with `--yolo`.
fn deploy(settings: &Settings, yolo: bool) -> Result<()> {
    if yolo {
        crate::cli::service::offer_install(settings)?;
        return Ok(());
    }
    println!("\nDeployment: isolated container (default; rootless, root-inside).");
    println!("  The agent can install software without touching your host — only the");
    println!("  directories you share are visible. `--yolo` would install natively instead.");
    if !confirm("  Set up the container now? [Y/n]: ", true)? {
        println!("  Skipped. Run `lethe container up` later, or re-run with --yolo for native.");
        return Ok(());
    }
    crate::cli::container::prompt_and_save_mounts(settings)?;
    crate::cli::container::up(
        settings,
        crate::cli::container::UpArgs {
            rebuild: false,
            force: false,
            extra_mounts: vec![],
            now: false,
            dry_run: false,
            from_source: false,
            with_tools: false,
        },
    )
}

// =============================================================================
// Non-interactive flow (Docker / CI)
// =============================================================================

async fn run_noninteractive(args: InitArgs) -> Result<()> {
    let settings = Settings::from_env();
    let env_path = settings.paths.config_file.clone();

    // Provider: --provider flag, else LLM_PROVIDER from the environment.
    let provider_id = args
        .provider
        .clone()
        .or_else(|| non_empty(&settings.llm.llm_provider));
    let Some(provider_id) = provider_id else {
        bail!(
            "`lethe init` has no terminal here (non-interactive). \
             Pass --provider <openrouter|anthropic|openai|opencode-go> or set LLM_PROVIDER."
        );
    };
    let provider = Provider::from_id(&provider_id).ok_or_else(|| {
        anyhow!("unknown provider `{provider_id}` (use openrouter|anthropic|openai|opencode-go)")
    })?;

    // Key: from the provider's env var, or (subscription) existing OAuth tokens.
    let key_from_env = std::env::var(provider.key_env())
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let api_key = match (provider, key_from_env) {
        (_, Some(key)) => Some(key),
        (Provider::Anthropic, None) if lethe::llm::anthropic_oauth_available() => None,
        (Provider::OpenAI, None) if lethe::llm::openai_oauth::openai_oauth_available() => None,
        _ => bail!(
            "No credentials for {}. Set {} in the environment before running \
             `lethe init` non-interactively.",
            provider.label(),
            provider.key_env()
        ),
    };

    // Models: flag → existing env → catalog default.
    let main_model = args
        .model
        .clone()
        .or_else(|| non_empty(&settings.llm.llm_model))
        .or_else(|| default_model_for(provider, "main"))
        .ok_or_else(|| anyhow!("no main model resolved; pass --model"))?;
    let aux_model = args
        .aux_model
        .clone()
        .or_else(|| non_empty(&settings.llm.llm_model_aux))
        .or_else(|| default_model_for(provider, "aux"))
        .unwrap_or_else(|| main_model.clone());

    write_config(
        &env_path,
        provider,
        &main_model,
        &aux_model,
        api_key.as_deref(),
        None,
        None,
        None,
    )?;
    println!("Wrote {}", env_path.display());
    seed_workspace(&settings)?;
    provision_agent_id(&settings).await;
    println!(
        "Provider: {}  Main: {main_model}  Aux: {aux_model}",
        provider.label()
    );
    println!("Running smoke test...");
    smoke_test(provider, &main_model, &aux_model, api_key.as_deref()).await?;
    println!("Done. Run `lethe check` to verify.");
    Ok(())
}

// =============================================================================
// Provider selection
// =============================================================================

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Provider {
    OpenRouter,
    Anthropic,
    OpenAI,
    OpenCodeGo,
}

impl Provider {
    fn label(self) -> &'static str {
        match self {
            Provider::OpenRouter => "OpenRouter",
            Provider::Anthropic => "Anthropic",
            Provider::OpenAI => "OpenAI",
            Provider::OpenCodeGo => "OpenCode Go",
        }
    }
    fn id(self) -> &'static str {
        match self {
            Provider::OpenRouter => "openrouter",
            Provider::Anthropic => "anthropic",
            Provider::OpenAI => "openai",
            Provider::OpenCodeGo => "opencode-go",
        }
    }
    fn from_id(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "openrouter" => Some(Provider::OpenRouter),
            "anthropic" => Some(Provider::Anthropic),
            "openai" => Some(Provider::OpenAI),
            "opencode-go" | "opencodego" | "opencode_go" => Some(Provider::OpenCodeGo),
            _ => None,
        }
    }
    fn key_env(self) -> &'static str {
        match self {
            Provider::OpenRouter => "OPENROUTER_API_KEY",
            Provider::Anthropic => "ANTHROPIC_API_KEY",
            Provider::OpenAI => "OPENAI_API_KEY",
            Provider::OpenCodeGo => "OPENCODE_GO_API_KEY",
        }
    }
    fn key_url(self) -> &'static str {
        match self {
            Provider::OpenRouter => "https://openrouter.ai/keys",
            Provider::Anthropic => "https://console.anthropic.com/settings/keys",
            Provider::OpenAI => "https://platform.openai.com/api-keys",
            Provider::OpenCodeGo => "https://opencode.ai/zen/go",
        }
    }
    /// Subscription label for the OAuth path, if the provider has one.
    fn subscription_label(self) -> Option<&'static str> {
        match self {
            Provider::Anthropic => Some("Claude Pro/Max"),
            Provider::OpenAI => Some("ChatGPT Plus/Pro"),
            Provider::OpenRouter => None,
            Provider::OpenCodeGo => None,
        }
    }
}

fn detect_keys(existing_env: &EnvMap) -> Vec<&'static str> {
    let mut out = Vec::new();
    for provider in [
        Provider::OpenRouter,
        Provider::Anthropic,
        Provider::OpenAI,
        Provider::OpenCodeGo,
    ] {
        let key = provider.key_env();
        let present = std::env::var(key)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .is_some()
            || existing_env
                .get(key)
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false);
        if present {
            out.push(key);
        }
    }
    out
}

fn prompt_provider(detected: &[&'static str]) -> Result<Provider> {
    println!("\nSelect your LLM provider:\n");
    let entries = [
        (
            Provider::OpenRouter,
            "OpenRouter (recommended — single key, every major model)",
        ),
        (
            Provider::Anthropic,
            "Anthropic (API key or Claude Pro/Max subscription)",
        ),
        (
            Provider::OpenAI,
            "OpenAI (API key or ChatGPT Plus/Pro subscription)",
        ),
        (
            Provider::OpenCodeGo,
            "OpenCode Go (budget-friendly open models, $5–$10/month)",
        ),
    ];
    for (idx, (provider, desc)) in entries.iter().enumerate() {
        let badge = if detected.contains(&provider.key_env()) {
            " [key found]"
        } else {
            ""
        };
        println!("  {}) {desc}{badge}", idx + 1);
    }

    // Default to the first detected provider, else OpenRouter.
    let default = entries
        .iter()
        .position(|(p, _)| detected.contains(&p.key_env()))
        .map(|i| i + 1)
        .unwrap_or(1);

    let choice = prompt_line(&format!("\nChoose [1-4, default={default}]: "))?;
    let choice = choice.trim();
    let n = if choice.is_empty() {
        default
    } else {
        choice
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=entries.len()).contains(n))
            .unwrap_or(default)
    };
    Ok(entries[n - 1].0)
}

// =============================================================================
// Model selection
// =============================================================================

fn resolve_models(provider: Provider, args: &InitArgs) -> Result<(String, String)> {
    let catalog = model_catalog();
    let provider_entry = catalog.get(provider.id());
    let main_entries = provider_entry
        .and_then(|p| p.get("main"))
        .cloned()
        .unwrap_or_default();
    let aux_entries = provider_entry
        .and_then(|p| p.get("aux"))
        .cloned()
        .unwrap_or_default();

    let main = match &args.model {
        Some(m) => m.clone(),
        None if args.yes => default_model(&main_entries).ok_or_else(|| {
            anyhow!(
                "no catalog default for {} main model; pass --model",
                provider.id()
            )
        })?,
        None => {
            println!("\nMain model (handles user-facing turns):");
            pick_model("main", &main_entries)?
        }
    };
    let aux = match &args.aux_model {
        Some(m) => m.clone(),
        None if args.yes => default_model(&aux_entries).unwrap_or_else(|| main.clone()),
        None => {
            println!("\nAuxiliary model (cheap calls — summarization, heartbeat, background):");
            pick_model("aux", &aux_entries)?
        }
    };
    Ok((main, aux))
}

fn default_model(entries: &[ModelEntry]) -> Option<String> {
    entries.first().map(|e| e.model_id().to_string())
}

fn default_model_for(provider: Provider, kind: &str) -> Option<String> {
    model_catalog()
        .get(provider.id())
        .and_then(|p| p.get(kind))
        .and_then(|v| v.first())
        .map(|e| e.model_id().to_string())
}

fn pick_model(label: &str, entries: &[ModelEntry]) -> Result<String> {
    if entries.is_empty() {
        let raw = prompt_line(&format!("  {label} model id: "))?;
        let raw = raw.trim();
        if raw.is_empty() {
            bail!("a model id is required");
        }
        return Ok(raw.to_string());
    }
    for (idx, entry) in entries.iter().enumerate() {
        println!(
            "  {}) {} — {} ({})",
            idx + 1,
            entry.name(),
            entry.model_id(),
            entry.price()
        );
    }
    let prompt = format!(
        "  Choose [1-{}, default=1, or type a custom id]: ",
        entries.len()
    );
    let answer = prompt_line(&prompt)?;
    let answer = answer.trim();
    if answer.is_empty() {
        return Ok(entries[0].model_id().to_string());
    }
    if let Ok(n) = answer.parse::<usize>()
        && (1..=entries.len()).contains(&n)
    {
        return Ok(entries[n - 1].model_id().to_string());
    }
    Ok(answer.to_string())
}

// =============================================================================
// API key
// =============================================================================

/// Returns `None` when the user picked a no-API-key auth path (subscription
/// OAuth for OpenAI or Anthropic). Caller skips writing an API key then —
/// the OAuth tokens live in `~/.lethe/credentials/`.
async fn prompt_api_key(provider: Provider, existing_env: &EnvMap) -> Result<Option<String>> {
    let env_name = provider.key_env();

    // Subscription path, where the provider supports one.
    if let Some(subscription) = provider.subscription_label() {
        println!("\n{} sign-in options:", provider.label());
        println!("  1) API key ({})", provider.key_url());
        println!("  2) {subscription} subscription (browser sign-in)");
        let choice = prompt_line("Choose [1-2, default=1]: ")?;
        if choice.trim() == "2" {
            match provider {
                Provider::OpenAI => lethe::llm::openai_oauth::run_device_login().await?,
                Provider::Anthropic => lethe::llm::anthropic_oauth::run_device_login().await?,
                Provider::OpenCodeGo | Provider::OpenRouter => unreachable!("no subscription path"),
            }
            return Ok(None);
        }
    }

    let existing = std::env::var(env_name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            existing_env
                .get(env_name)
                .filter(|v| !v.trim().is_empty())
                .cloned()
        });
    if let Some(key) = existing {
        println!("\nFound existing {env_name}: {}", mask_secret(&key));
        let answer = prompt_line("Use it? [Y/n]: ")?;
        if !answer.trim().to_ascii_lowercase().starts_with('n') {
            return Ok(Some(key));
        }
    }
    println!("\n{env_name} required.");
    println!("  Get one at: {}", provider.key_url());
    let key = prompt_secret(&format!("  Paste {env_name} (input hidden): "))?;
    let key = key.trim().to_string();
    if key.is_empty() {
        bail!("{env_name} is required to continue");
    }
    Ok(Some(key))
}

// =============================================================================
// Optional HTTP API / TUI token
// =============================================================================

fn prompt_api_token(existing_env: &EnvMap) -> Result<Option<String>> {
    println!("\nOptional: HTTP API + terminal UI (`lethe tui`, `lethe api`)");
    println!("  These need a bearer token (LETHE_API_TOKEN). Skip for CLI-only use.");
    let yes_no = prompt_line("  Enable API/TUI now? [y/N]: ")?;
    if !yes_no.trim().to_ascii_lowercase().starts_with('y') {
        return Ok(None);
    }
    if let Some(existing) = existing_env
        .get("LETHE_API_TOKEN")
        .filter(|v| !v.trim().is_empty())
    {
        println!(
            "  Found existing LETHE_API_TOKEN: {}",
            mask_secret(existing)
        );
        let keep = prompt_line("  Keep it? [Y/n]: ")?;
        if !keep.trim().to_ascii_lowercase().starts_with('n') {
            return Ok(Some(existing.clone()));
        }
    }
    let token = generate_token();
    println!("  Generated a new LETHE_API_TOKEN (saved to your config).");
    Ok(Some(token))
}

fn generate_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

// =============================================================================
// Optional Telegram
// =============================================================================

struct TelegramSetup {
    bot_token: String,
    allowed_user_ids: String,
}

/// Optional, always skippable. Returns `None` when the user declines (the
/// default) — CLI / TUI / HTTP-API users just press Enter past it.
fn prompt_telegram(existing_env: &EnvMap) -> Result<Option<TelegramSetup>> {
    println!("\nOptional: Telegram bot");
    println!("  Skip this if you only want CLI, TUI, or HTTP API access.");
    if !confirm("  Set up a Telegram bot now? [y/N]: ", false)? {
        return Ok(None);
    }
    let existing_token = existing_env.get("TELEGRAM_BOT_TOKEN").cloned();
    let token = match existing_token.as_deref().filter(|v| !v.trim().is_empty()) {
        Some(value) => {
            println!(
                "  Found existing TELEGRAM_BOT_TOKEN: {}",
                mask_secret(value)
            );
            if confirm("  Use it? [Y/n]: ", true)? {
                value.to_string()
            } else {
                prompt_secret("  Paste new bot token (input hidden): ")?
                    .trim()
                    .to_string()
            }
        }
        None => {
            println!("  Get a bot token from @BotFather (https://t.me/BotFather).");
            prompt_secret("  Paste TELEGRAM_BOT_TOKEN (input hidden): ")?
                .trim()
                .to_string()
        }
    };
    if token.is_empty() {
        println!("  No token entered — skipping Telegram.");
        return Ok(None);
    }
    let allowed = prompt_line("  Allowed Telegram user ids (comma-separated, blank for any): ")?
        .trim()
        .to_string();
    if allowed.is_empty() {
        warn("No allowed ids set — ANYONE who finds the bot can talk to your assistant.");
        warn("Lock it down later via TELEGRAM_ALLOWED_USER_IDS (your numeric Telegram id).");
    }
    Ok(Some(TelegramSetup {
        bot_token: token,
        allowed_user_ids: allowed,
    }))
}

// =============================================================================
// Optional human-block seed
// =============================================================================

fn prompt_human_intro() -> Result<Option<String>> {
    println!("\nOptional: tell Lethe about yourself");
    println!("  Seeds the `human` memory block — anything you want the assistant to");
    println!("  remember from turn one (name, preferences, role).");
    read_multiline(&["  Type one or more lines; finish with an empty line. Leave blank to skip."])
}

// =============================================================================
// Persistence
// =============================================================================

#[allow(clippy::too_many_arguments)]
fn write_config(
    path: &Path,
    provider: Provider,
    main_model: &str,
    aux_model: &str,
    api_key: Option<&str>,
    api_token: Option<&str>,
    agent_name: Option<&str>,
    telegram: Option<&TelegramSetup>,
) -> Result<()> {
    let mut updates: Vec<(String, String)> = vec![
        ("LLM_PROVIDER".into(), provider.id().into()),
        ("LLM_MODEL".into(), main_model.into()),
        ("LLM_MODEL_AUX".into(), aux_model.into()),
    ];
    if let Some(key) = api_key {
        updates.push((provider.key_env().into(), key.into()));
    }
    if let Some(token) = api_token {
        updates.push(("LETHE_API_TOKEN".into(), token.into()));
    }
    if let Some(name) = agent_name {
        updates.push(("LETHE_AGENT_NAME".into(), name.into()));
    }
    if let Some(tg) = telegram {
        updates.push(("TELEGRAM_BOT_TOKEN".into(), tg.bot_token.clone()));
        if !tg.allowed_user_ids.trim().is_empty() {
            updates.push((
                "TELEGRAM_ALLOWED_USER_IDS".into(),
                tg.allowed_user_ids.clone(),
            ));
        }
    }
    upsert_env(path, &updates)
}

fn seed_workspace(settings: &Settings) -> Result<()> {
    // MemoryStore::from_settings already creates directories + seeds blocks.
    // We re-instantiate here so init works even if no chat has run yet.
    let _ = MemoryStore::from_settings(settings)
        .with_context(|| "opening memory store under workspace")?;
    Ok(())
}

fn seed_human_block(settings: &Settings, text: &str) -> Result<()> {
    identity::write_block(settings, "human", text)
}

// =============================================================================
// Smoke test
// =============================================================================

async fn smoke_test(
    provider: Provider,
    main_model: &str,
    aux_model: &str,
    api_key: Option<&str>,
) -> Result<()> {
    // Apply the new config in-process so the smoke test reflects what the
    // user will actually run with on the next invocation. When the user
    // picked an OAuth path, leave the api-key env var alone — the OAuth
    // client picks tokens up from the credentials file.
    unsafe {
        if let Some(key) = api_key {
            std::env::set_var(provider.key_env(), key);
        }
        std::env::set_var("LLM_PROVIDER", provider.id());
        std::env::set_var("LLM_MODEL", main_model);
        std::env::set_var("LLM_MODEL_AUX", aux_model);
    }
    let settings = Settings::from_env();
    let router = LlmRouter::new(LlmRouterConfig::from_settings(&settings));
    let probe = vec![
        LlmMessage::system("Reply with the single word: ok"),
        LlmMessage::user("ready?"),
    ];
    match router.complete(probe, true).await {
        Ok(reply) => {
            let preview = reply.trim().lines().next().unwrap_or("").to_string();
            info(&format!("LLM ping via aux model: `{preview}`"));
            Ok(())
        }
        Err(error) => {
            warn(&format!("LLM ping failed: {error}"));
            warn("Config was saved — fix the key/model and re-run `lethe check` to verify.");
            Ok(())
        }
    }
}

// =============================================================================
// I/O helpers
// =============================================================================

type EnvMap = std::collections::HashMap<String, String>;

fn read_existing_env(path: &Path) -> EnvMap {
    let Ok(content) = std::fs::read_to_string(path) else {
        return EnvMap::new();
    };
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (k, v) = line.split_once('=')?;
            Some((k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        })
        .collect()
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn print_next_steps(api: bool, yolo: bool, telegram: bool) {
    println!();
    success("Setup complete.");
    println!();
    println!("Next steps:");
    println!("  lethe chat -m \"hello\"     # one-off chat");
    println!("  lethe                       # show version + config");
    println!("  lethe identity              # view / change who your assistant is");
    if api {
        println!("  lethe tui                   # terminal UI (uses your API token)");
    }
    if telegram {
        println!("  lethe telegram run          # start the Telegram bot");
    }
    if yolo {
        println!("  lethe service install       # run Lethe in the background (native)");
    } else {
        println!("  lethe container shell       # root shell inside the container");
        println!("  lethe container status      # container + service state");
    }
    println!("  lethe check                 # live health check any time");
}

fn info(message: &str) {
    println!("  → {message}");
}

/// Provision the agent's Alien identity + vault at init time and report it, or
/// point the user at the CLIs to install when they're absent. Best-effort — a
/// failure here never blocks setup (the daemon re-provisions on start).
async fn provision_agent_id(settings: &Settings) {
    if !lethe::agent_id::is_enabled() {
        return;
    }
    if !lethe::agent_id::vault_tools_available() {
        info(
            "Alien agent-id: not set up (optional). Install with \
             `npm i -g @alien-id/agent-id-core @alien-id/agent-id-vault` to give this \
             agent a verifiable identity + credential vault.",
        );
        return;
    }
    lethe::agent_id::ensure_provisioned(settings).await;
    let sd = lethe::agent_id::state_dir(settings);
    let status =
        lethe::agent_id::cli::run_json(lethe::agent_id::cli::Bin::Core, &sd, &["status"]).await;
    let assurance = status
        .get("assurance")
        .and_then(|v| v.as_str())
        .unwrap_or("self-asserted");
    match status.get("jkt").and_then(|v| v.as_str()) {
        Some(jkt) => info(&format!(
            "Alien identity ready ({assurance}, key {}…). Bind it to you later with the agent_id_bind tool.",
            &jkt[..jkt.len().min(12)]
        )),
        None => info("Alien identity ready. Bind it to you later with the agent_id_bind tool."),
    }
}

fn success(message: &str) {
    println!("✓ {message}");
}

fn warn(message: &str) {
    println!("! {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_from_id_roundtrip() {
        assert_eq!(Provider::from_id("openrouter"), Some(Provider::OpenRouter));
        assert_eq!(Provider::from_id("ANTHROPIC"), Some(Provider::Anthropic));
        assert_eq!(Provider::from_id(" openai "), Some(Provider::OpenAI));
        assert_eq!(Provider::from_id("opencode-go"), Some(Provider::OpenCodeGo));
        assert_eq!(Provider::from_id("opencodego"), Some(Provider::OpenCodeGo));
        assert_eq!(Provider::from_id("nope"), None);
    }
}
