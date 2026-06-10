//! Shared `.env` rewriter for `lethe login <provider>` commands.
//!
//! Every login flow (subscription OAuth or API-key paste) ultimately
//! wants the same .env state: point `LLM_PROVIDER` at the new provider,
//! align `LLM_MODEL` / `LLM_MODEL_AUX` to the wizard catalog defaults,
//! and either (a) comment out an unnecessary API key under OAuth or
//! (b) write the freshly-pasted API key. Old values stay in the file as
//! commented lines so a user can revert by un-commenting.

use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

/// Convenience: rewrite the default `~/.lethe/config/.env` (subject to
/// `LETHE_ENV_FILE`, `LETHE_HOME`, and `HOME` overrides).
pub fn update_env_after_oauth_login(
    provider: &str,
    main_model: Option<&str>,
    aux_model: Option<&str>,
    key_env_to_comment: Option<&str>,
) -> Result<()> {
    update_env_file(
        &env_file_path(),
        provider,
        main_model,
        aux_model,
        key_env_to_comment,
        None,
    )
}

/// Convenience: rewrite the default `~/.lethe/config/.env` for an
/// API-key login (no OAuth tokens involved). Sets `set_key.0=set_key.1`
/// in addition to the provider + model alignment.
pub fn update_env_after_api_key_login(
    provider: &str,
    main_model: Option<&str>,
    aux_model: Option<&str>,
    set_key: (&str, &str),
) -> Result<()> {
    update_env_file(
        &env_file_path(),
        provider,
        main_model,
        aux_model,
        None,
        Some(set_key),
    )
}

/// Rewrite the given `.env` to align it with a new auth method. No-op
/// when the file doesn't exist (init writes it from scratch a few
/// steps later).
///
/// All `Option` parameters mean "leave whatever is in the .env alone
/// for this key" when `None`. `key_env_to_comment` and `set_key` are
/// mutually informative — typically a subscription login passes
/// `key_env_to_comment=Some(...)` and `set_key=None`, while an
/// API-key login does the opposite.
///
/// Takes the path as a parameter rather than reading a process-global
/// env var so concurrent tests can each write to their own tempdir
/// without racing on a shared `LETHE_ENV_FILE`.
pub fn update_env_file(
    env_path: &Path,
    provider: &str,
    main_model: Option<&str>,
    aux_model: Option<&str>,
    key_env_to_comment: Option<&str>,
    set_key: Option<(&str, &str)>,
) -> Result<()> {
    if !env_path.exists() {
        println!();
        println!(
            "No .env at {} — run `lethe init` to set models and other settings,",
            env_path.display()
        );
        println!("or write `LLM_PROVIDER={provider}` and `LLM_MODEL=<id>` there yourself.");
        return Ok(());
    }

    let existing =
        fs::read_to_string(env_path).with_context(|| format!("reading {}", env_path.display()))?;
    let stamp = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S %Z");
    let mut lines: Vec<String> = Vec::new();
    let mut provider_set = false;
    let mut main_set = false;
    let mut aux_set = false;
    let mut api_key_commented = false;
    let mut api_key_set = false;

    for raw in existing.lines() {
        let trimmed = raw.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            lines.push(raw.to_string());
            continue;
        }
        let Some((key, _value)) = trimmed.split_once('=') else {
            lines.push(raw.to_string());
            continue;
        };
        let key = key.trim();
        if key.eq_ignore_ascii_case("LLM_PROVIDER") {
            if !provider_set {
                lines.push(format!(
                    "# {raw}   # was: pre-login provider, updated {stamp}"
                ));
                lines.push(format!("LLM_PROVIDER={provider}"));
                provider_set = true;
            } else {
                lines.push(format!(
                    "# {raw}   # duplicate LLM_PROVIDER, commented {stamp}"
                ));
            }
        } else if key.eq_ignore_ascii_case("LLM_MODEL") && main_model.is_some() {
            if !main_set {
                lines.push(format!("# {raw}   # was: previous main, updated {stamp}"));
                lines.push(format!("LLM_MODEL={}", main_model.unwrap()));
                main_set = true;
            } else {
                lines.push(format!(
                    "# {raw}   # duplicate LLM_MODEL, commented {stamp}"
                ));
            }
        } else if key.eq_ignore_ascii_case("LLM_MODEL_AUX") && aux_model.is_some() {
            if !aux_set {
                lines.push(format!("# {raw}   # was: previous aux, updated {stamp}"));
                lines.push(format!("LLM_MODEL_AUX={}", aux_model.unwrap()));
                aux_set = true;
            } else {
                lines.push(format!(
                    "# {raw}   # duplicate LLM_MODEL_AUX, commented {stamp}"
                ));
            }
        } else if let Some((set_env, set_value)) = set_key
            && key.eq_ignore_ascii_case(set_env)
        {
            if !api_key_set {
                lines.push(format!(
                    "# {raw}   # was: previous {set_env}, updated {stamp}"
                ));
                lines.push(format!("{set_env}={set_value}"));
                api_key_set = true;
            } else {
                lines.push(format!(
                    "# {raw}   # duplicate {set_env}, commented {stamp}"
                ));
            }
        } else if key_env_to_comment.is_some_and(|target| key.eq_ignore_ascii_case(target)) {
            lines.push(format!("# {raw}   # not used by OAuth, commented {stamp}"));
            api_key_commented = true;
        } else {
            lines.push(raw.to_string());
        }
    }

    // Append missing keys (when the original .env didn't have them at all).
    let mut appended_header = false;
    let ensure_header = |lines: &mut Vec<String>, appended_header: &mut bool| {
        if !*appended_header {
            lines.push(String::new());
            lines.push(format!("# Added by `lethe login {provider}` on {stamp}"));
            *appended_header = true;
        }
    };
    if !provider_set {
        ensure_header(&mut lines, &mut appended_header);
        lines.push(format!("LLM_PROVIDER={provider}"));
    }
    if let Some(main) = main_model
        && !main_set
    {
        ensure_header(&mut lines, &mut appended_header);
        lines.push(format!("LLM_MODEL={main}"));
    }
    if let Some(aux) = aux_model
        && !aux_set
    {
        ensure_header(&mut lines, &mut appended_header);
        lines.push(format!("LLM_MODEL_AUX={aux}"));
    }
    if let Some((set_env, set_value)) = set_key
        && !api_key_set
    {
        ensure_header(&mut lines, &mut appended_header);
        lines.push(format!("{set_env}={set_value}"));
    }

    let mut body = lines.join("\n");
    if !body.ends_with('\n') {
        body.push('\n');
    }
    write_env_atomically(env_path, &body)?;

    println!();
    println!("Updated {}:", env_path.display());
    println!("  LLM_PROVIDER={provider}");
    if let Some(main) = main_model {
        println!("  LLM_MODEL={main}");
    }
    if let Some(aux) = aux_model {
        println!("  LLM_MODEL_AUX={aux}");
    }
    if api_key_commented && let Some(key_env) = key_env_to_comment {
        println!("  {key_env} commented out (not used by OAuth; left for rollback)");
    }
    if let Some((set_env, _)) = set_key {
        println!("  {set_env}=*** (saved)");
    }
    Ok(())
}

/// Stdin prompt for an API key. Echoes the prompt to stdout, reads a
/// line from stdin, trims whitespace, errors if empty. Used by all
/// `lethe login <provider>` API-key paths.
pub fn prompt_api_key_line(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush().ok();
    let stdin = io::stdin();
    let mut line = String::new();
    stdin
        .lock()
        .read_line(&mut line)
        .with_context(|| "reading API key from stdin")?;
    let trimmed = line
        .trim_end_matches('\n')
        .trim_end_matches('\r')
        .trim()
        .to_string();
    if trimmed.is_empty() {
        bail!("no API key provided");
    }
    Ok(trimmed)
}

/// First (default) main + aux model id for the given provider in the
/// curated catalog. `None` when the catalog has no entries for that
/// provider — callers should fall back to leaving the models alone.
pub fn catalog_defaults(provider: &str) -> Option<(String, String)> {
    let catalog = crate::llm::models::model_catalog();
    let provider_entry = catalog.get(provider)?;
    let main = provider_entry.get("main")?.first()?.model_id().to_string();
    let aux = provider_entry.get("aux")?.first()?.model_id().to_string();
    Some((main, aux))
}

/// API-key login: prompt for the key + main/aux models (with catalog
/// defaults), then write everything to `.env`. Shared between
/// openrouter and the API-key path of openai/anthropic.
pub fn run_api_key_login(provider: &str, key_env: &str, prompt_label: &str) -> Result<()> {
    let key = prompt_api_key_line(prompt_label)?;
    let (main_model, aux_model) = prompt_provider_models(provider)?;
    update_env_after_api_key_login(
        provider,
        main_model.as_deref(),
        aux_model.as_deref(),
        (key_env, &key),
    )
}

/// Prompt for main + aux model ids with the catalog's first entry as
/// the default. Returns `(Some(main), Some(aux))` when the user
/// accepted defaults or supplied custom values, `(None, None)` when
/// the catalog has no entries for that provider (in which case the
/// caller should leave the .env model lines untouched).
///
/// Empty input keeps the printed default; any other input replaces it.
///
/// For the `openrouter` provider the noisy `openrouter/` prefix is
/// stripped from the displayed defaults — the user types
/// `moonshotai/kimi-k2.6` and we re-prefix to the canonical
/// `openrouter/moonshotai/kimi-k2.6` before returning. Lets `lethe
/// chat -m openrouter/anthropic/claude-opus-4.7` style overrides still
/// work too (anything already containing `openrouter/` passes through
/// untouched).
pub fn prompt_provider_models(provider: &str) -> Result<(Option<String>, Option<String>)> {
    let Some((main_default, aux_default)) = catalog_defaults(provider) else {
        // No catalog entries → don't prompt; let the .env keep its
        // current model lines. Users with an exotic provider can edit
        // .env directly or re-run `lethe init`.
        println!(
            "(No catalog entries for `{provider}`; keeping the existing LLM_MODEL / LLM_MODEL_AUX from .env.)"
        );
        return Ok((None, None));
    };
    println!();
    let main = prompt_one_model(provider, "Main model", &main_default)?;
    let aux = prompt_one_model(provider, "Aux model", &aux_default)?;
    Ok((Some(main), Some(aux)))
}

const OPENROUTER_PREFIX: &str = "openrouter/";
const OPENCODE_GO_PREFIX: &str = "opencode-go/";

fn prompt_one_model(provider: &str, label: &str, default: &str) -> Result<String> {
    if provider == "openrouter" {
        let display_default = default.strip_prefix(OPENROUTER_PREFIX).unwrap_or(default);
        let raw = prompt_model_id(label, display_default)?;
        Ok(prefix_openrouter(&raw))
    } else if provider == "opencode-go" {
        let display_default = default.strip_prefix(OPENCODE_GO_PREFIX).unwrap_or(default);
        let raw = prompt_model_id(label, display_default)?;
        Ok(prefix_opencode_go(&raw))
    } else {
        prompt_model_id(label, default)
    }
}

fn prefix_openrouter(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.starts_with(OPENROUTER_PREFIX) {
        trimmed.to_string()
    } else {
        format!("{OPENROUTER_PREFIX}{trimmed}")
    }
}

fn prefix_opencode_go(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.starts_with(OPENCODE_GO_PREFIX) {
        trimmed.to_string()
    } else {
        format!("{OPENCODE_GO_PREFIX}{trimmed}")
    }
}

/// Prompt for a single model id with a printed default. Empty input
/// returns the default.
pub fn prompt_model_id(label: &str, default: &str) -> Result<String> {
    print!("{label} [{default}]: ");
    io::stdout().flush().ok();
    let stdin = io::stdin();
    let mut line = String::new();
    stdin
        .lock()
        .read_line(&mut line)
        .with_context(|| "reading model id from stdin")?;
    let trimmed = line.trim_end_matches('\n').trim_end_matches('\r').trim();
    if trimmed.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

/// Ask whether to use subscription OAuth (default) or paste an API
/// key. Returns true for subscription, false for API key. Anything
/// other than a leading "a" (case-insensitive) keeps the default.
pub fn prompt_subscription_or_api(provider_label: &str, subscription_label: &str) -> Result<bool> {
    println!();
    println!("How do you want to sign in to {provider_label}?");
    println!("  1) {subscription_label} subscription (default)");
    println!("  2) API key");
    print!("Choose [1-2, default=1]: ");
    io::stdout().flush().ok();
    let stdin = io::stdin();
    let mut line = String::new();
    stdin
        .lock()
        .read_line(&mut line)
        .with_context(|| "reading auth choice from stdin")?;
    let trimmed = line.trim();
    // "2" or anything starting with 'a'/'A' (api/API) → api key path.
    let is_api_key = trimmed == "2" || trimmed.to_ascii_lowercase().starts_with('a');
    Ok(!is_api_key)
}

pub fn env_file_path() -> PathBuf {
    if let Some(path) = env::var_os("LETHE_ENV_FILE") {
        return PathBuf::from(path);
    }
    let home = env::var_os("LETHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".lethe")))
        .unwrap_or_else(|| PathBuf::from(".lethe"));
    home.join("config").join(".env")
}

fn write_env_atomically(path: &Path, body: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!(".env path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".env.lethe-login.{}.tmp", std::process::id()));
    fs::write(&temp, body).with_context(|| format!("writing {}", temp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&temp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&temp, path)
        .with_context(|| format!("promoting {} → {}", temp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tempfile(initial: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let env_path = tmp.path().join("config").join(".env");
        std::fs::create_dir_all(env_path.parent().unwrap()).unwrap();
        std::fs::write(&env_path, initial).unwrap();
        (tmp, env_path)
    }

    #[test]
    fn rewrites_provider_models_and_comments_api_key() {
        let (_tmp, path) = write_tempfile(
            "\
LLM_PROVIDER=anthropic
LLM_MODEL=claude-opus-4-6
LLM_MODEL_AUX=claude-haiku-4-5
OPENAI_API_KEY=sk-old
TELEGRAM_BOT_TOKEN=tg
",
        );
        update_env_file(
            &path,
            "openai",
            Some("gpt-5.5"),
            Some("gpt-5.4-mini"),
            Some("OPENAI_API_KEY"),
            None,
        )
        .unwrap();
        let rewritten = std::fs::read_to_string(&path).unwrap();
        assert!(rewritten.contains("# LLM_PROVIDER=anthropic"));
        assert!(rewritten.contains("\nLLM_PROVIDER=openai\n"));
        assert!(rewritten.contains("# LLM_MODEL=claude-opus-4-6"));
        assert!(rewritten.contains("\nLLM_MODEL=gpt-5.5\n"));
        assert!(rewritten.contains("# LLM_MODEL_AUX=claude-haiku-4-5"));
        assert!(rewritten.contains("\nLLM_MODEL_AUX=gpt-5.4-mini\n"));
        assert!(rewritten.contains("# OPENAI_API_KEY=sk-old"));
        assert!(!rewritten.contains("\nOPENAI_API_KEY=sk-old"));
        assert!(rewritten.contains("TELEGRAM_BOT_TOKEN=tg"));
    }

    #[test]
    fn appends_missing_keys() {
        let (_tmp, path) = write_tempfile("TELEGRAM_BOT_TOKEN=tg\n");
        update_env_file(
            &path,
            "anthropic",
            Some("claude-opus-4-7"),
            Some("claude-haiku-4-5"),
            Some("ANTHROPIC_API_KEY"),
            None,
        )
        .unwrap();
        let rewritten = std::fs::read_to_string(&path).unwrap();
        assert!(rewritten.contains("TELEGRAM_BOT_TOKEN=tg"));
        assert!(rewritten.contains("\nLLM_PROVIDER=anthropic\n"));
        assert!(rewritten.contains("\nLLM_MODEL=claude-opus-4-7\n"));
        assert!(rewritten.contains("\nLLM_MODEL_AUX=claude-haiku-4-5\n"));
    }

    #[test]
    fn leaves_models_alone_when_caller_passes_none() {
        let (_tmp, path) = write_tempfile(
            "\
LLM_PROVIDER=anthropic
LLM_MODEL=claude-opus-4-6
LLM_MODEL_AUX=claude-haiku-4-5
",
        );
        update_env_file(&path, "openai", None, None, None, None).unwrap();
        let rewritten = std::fs::read_to_string(&path).unwrap();
        // Provider swapped, but model lines are untouched.
        assert!(rewritten.contains("\nLLM_PROVIDER=openai\n"));
        assert!(rewritten.contains("LLM_MODEL=claude-opus-4-6\n"));
        assert!(rewritten.contains("LLM_MODEL_AUX=claude-haiku-4-5\n"));
        // No "was:" comment on the model lines because we didn't touch them.
        assert!(!rewritten.contains("was: previous main"));
    }

    #[test]
    fn api_key_login_sets_key_and_replaces_existing_value() {
        let (_tmp, path) = write_tempfile(
            "\
LLM_PROVIDER=anthropic
LLM_MODEL=claude-opus-4-6
LLM_MODEL_AUX=claude-haiku-4-5
OPENROUTER_API_KEY=sk-or-old
TELEGRAM_BOT_TOKEN=tg
",
        );
        update_env_file(
            &path,
            "openrouter",
            Some("openrouter/moonshotai/kimi-k2.6"),
            Some("openrouter/google/gemini-3-flash-preview"),
            None,
            Some(("OPENROUTER_API_KEY", "sk-or-new")),
        )
        .unwrap();
        let rewritten = std::fs::read_to_string(&path).unwrap();
        assert!(rewritten.contains("\nLLM_PROVIDER=openrouter\n"));
        assert!(rewritten.contains("\nLLM_MODEL=openrouter/moonshotai/kimi-k2.6\n"));
        // Old key kept as comment for rollback; new value set.
        assert!(rewritten.contains("# OPENROUTER_API_KEY=sk-or-old"));
        assert!(rewritten.contains("\nOPENROUTER_API_KEY=sk-or-new\n"));
        assert!(rewritten.contains("TELEGRAM_BOT_TOKEN=tg"));
    }

    #[test]
    fn prefix_openrouter_is_idempotent_and_required() {
        assert_eq!(
            prefix_openrouter("moonshotai/kimi-k2.6"),
            "openrouter/moonshotai/kimi-k2.6"
        );
        assert_eq!(
            prefix_openrouter("openrouter/moonshotai/kimi-k2.6"),
            "openrouter/moonshotai/kimi-k2.6",
            "already-prefixed values pass through untouched"
        );
        assert_eq!(
            prefix_openrouter("  qwen/qwen3.5-flash-02-23  "),
            "openrouter/qwen/qwen3.5-flash-02-23"
        );
    }

    #[test]
    fn catalog_defaults_returns_first_entry_per_provider() {
        // Sanity-check that the wizard catalog is shaped how the
        // login prompts expect (provider has main + aux entries).
        let (main, aux) = catalog_defaults("openai").expect("openai in catalog");
        assert!(!main.is_empty());
        assert!(!aux.is_empty());
        let (main_a, aux_a) = catalog_defaults("anthropic").expect("anthropic in catalog");
        assert!(main_a.starts_with("claude-"));
        assert!(aux_a.starts_with("claude-"));
        let (main_or, _aux_or) = catalog_defaults("openrouter").expect("openrouter in catalog");
        assert!(main_or.starts_with("openrouter/"));
        // Unknown provider — caller should fall back to "leave alone".
        assert!(catalog_defaults("nonsense-provider").is_none());
    }

    #[test]
    fn api_key_login_appends_key_when_missing() {
        let (_tmp, path) = write_tempfile("TELEGRAM_BOT_TOKEN=tg\n");
        update_env_file(
            &path,
            "openrouter",
            None,
            None,
            None,
            Some(("OPENROUTER_API_KEY", "sk-or-new")),
        )
        .unwrap();
        let rewritten = std::fs::read_to_string(&path).unwrap();
        assert!(rewritten.contains("\nLLM_PROVIDER=openrouter\n"));
        assert!(rewritten.contains("\nOPENROUTER_API_KEY=sk-or-new\n"));
    }
}
