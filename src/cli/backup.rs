//! `lethe backup` and `lethe restore`: pack the workspace, agent state
//! (context + history), and the `.env` file into a single `.tar.gz`
//! archive, and unpack one back into place.
//!
//! Shells out to `tar` and `cp -R` rather than pulling in archive crates
//! — both are POSIX-standard on the unix targets lethe runs on.

use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::Local;
use serde_json::json;
use uuid::Uuid;

use lethe::config::Settings;

/// `lethe backup`. Returns Ok after writing `output` (or a timestamped
/// default in the current directory) as a 0600 tar.gz containing
/// workspace, data (memory + history), and `.env`.
pub fn backup(output: Option<String>) -> Result<()> {
    let settings = Settings::from_env();
    let output_path = resolve_output_path(output);

    let staging = scratch_dir("lethe-backup-");
    fs::create_dir_all(&staging)
        .with_context(|| format!("creating staging dir {}", staging.display()))?;

    let result = run_backup(&settings, &staging, &output_path);
    let _ = fs::remove_dir_all(&staging);
    result?;

    if let Err(error) = fs::set_permissions(&output_path, fs::Permissions::from_mode(0o600)) {
        eprintln!(
            "warning: could not chmod 0600 {}: {error}",
            output_path.display()
        );
    }

    println!("Wrote backup to {}", output_path.display());
    println!("Note: archive may contain secrets from .env — keep it private.");
    Ok(())
}

/// `lethe restore <archive> [--yes]`. Asks before overwriting an
/// existing workspace and before overwriting an existing `.env`;
/// memory + history are restored unconditionally (that's the point).
pub fn restore(archive: String, yes: bool) -> Result<()> {
    let settings = Settings::from_env();
    let archive_path = PathBuf::from(&archive);
    if !archive_path.exists() {
        bail!("archive not found: {}", archive_path.display());
    }

    let staging = scratch_dir("lethe-restore-");
    fs::create_dir_all(&staging)
        .with_context(|| format!("creating staging dir {}", staging.display()))?;

    let result = run_restore(&settings, &archive_path, &staging, yes);
    let _ = fs::remove_dir_all(&staging);
    result
}

fn run_backup(settings: &Settings, staging: &Path, output: &Path) -> Result<()> {
    let mut components: Vec<&str> = Vec::new();

    if dir_exists(&settings.paths.workspace_dir) {
        copy_dir(&settings.paths.workspace_dir, &staging.join("workspace"))?;
        components.push("workspace");
    }

    let data_dst = staging.join("data");
    let mut wrote_data = false;
    if dir_exists(&settings.paths.memory_dir) {
        fs::create_dir_all(&data_dst)?;
        copy_dir(&settings.paths.memory_dir, &data_dst.join("memory"))?;
        wrote_data = true;
    }
    if settings.paths.db_path.exists() {
        fs::create_dir_all(&data_dst)?;
        fs::copy(&settings.paths.db_path, data_dst.join("lethe.db"))?;
        wrote_data = true;
    }
    if wrote_data {
        components.push("data");
    }

    let env_src = settings.paths.lethe_home.join("config").join(".env");
    if env_src.exists() {
        let dst_dir = staging.join("config");
        fs::create_dir_all(&dst_dir)?;
        fs::copy(&env_src, dst_dir.join(".env"))
            .with_context(|| format!("copying {} into staging", env_src.display()))?;
        components.push("env");
    }

    if components.is_empty() {
        bail!(
            "nothing to back up: workspace, data, and .env are all empty/missing under {}",
            settings.paths.lethe_home.display()
        );
    }

    let manifest = json!({
        "version": 1,
        "lethe_version": env!("CARGO_PKG_VERSION"),
        "created_at": Local::now().to_rfc3339(),
        "components": components,
    });
    fs::write(
        staging.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;

    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    tar_create(output, staging)
}

fn run_restore(settings: &Settings, archive: &Path, staging: &Path, yes: bool) -> Result<()> {
    tar_extract(archive, staging)?;

    if let Ok(text) = fs::read_to_string(staging.join("manifest.json")) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(created) = value.get("created_at").and_then(|v| v.as_str()) {
                println!("Archive created at {created}");
            }
        }
    }

    let src_ws = staging.join("workspace");
    if src_ws.exists() {
        let dst = settings.paths.workspace_dir.clone();
        let proceed = if dir_has_content(&dst) && !yes {
            confirm(&format!(
                "Workspace {} already exists. Overwrite? [y/N]: ",
                dst.display()
            ))?
        } else {
            true
        };
        if proceed {
            replace_dir(&src_ws, &dst)?;
            println!("Restored workspace → {}", dst.display());
        } else {
            println!("Skipped workspace.");
        }
    }

    let src_data = staging.join("data");
    if src_data.exists() {
        let src_mem = src_data.join("memory");
        if src_mem.exists() {
            replace_dir(&src_mem, &settings.paths.memory_dir)?;
            println!("Restored memory → {}", settings.paths.memory_dir.display());
        }
        let src_db = src_data.join("lethe.db");
        if src_db.exists() {
            if let Some(parent) = settings.paths.db_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&src_db, &settings.paths.db_path)
                .with_context(|| format!("restoring db to {}", settings.paths.db_path.display()))?;
            println!("Restored db → {}", settings.paths.db_path.display());
        }
    }

    let src_env = staging.join("config").join(".env");
    if src_env.exists() {
        let dst = settings.paths.lethe_home.join("config").join(".env");
        let proceed = if dst.exists() && !yes {
            confirm(&format!(
                ".env already exists at {}. Overwrite? [y/N]: ",
                dst.display()
            ))?
        } else {
            true
        };
        if proceed {
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&src_env, &dst)
                .with_context(|| format!("restoring .env to {}", dst.display()))?;
            // .env carries secrets; mirror the conventional 0600 perms.
            if let Err(error) = fs::set_permissions(&dst, fs::Permissions::from_mode(0o600)) {
                eprintln!("warning: could not chmod 0600 {}: {error}", dst.display());
            }
            println!("Restored .env → {}", dst.display());
        } else {
            println!("Skipped .env.");
        }
    }

    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let status = Command::new("cp")
        .arg("-R")
        .arg(src)
        .arg(dst)
        .status()
        .with_context(|| format!("running cp -R {} {}", src.display(), dst.display()))?;
    if !status.success() {
        bail!(
            "cp -R {} {} failed ({status})",
            src.display(),
            dst.display()
        );
    }
    Ok(())
}

fn replace_dir(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        fs::remove_dir_all(dst).with_context(|| format!("removing existing {}", dst.display()))?;
    }
    copy_dir(src, dst)
}

fn tar_create(output: &Path, staging: &Path) -> Result<()> {
    let status = Command::new("tar")
        .arg("-czf")
        .arg(output)
        .arg("-C")
        .arg(staging)
        .arg(".")
        .status()
        .with_context(|| "running tar — is it installed?")?;
    if !status.success() {
        bail!("tar create failed ({status})");
    }
    Ok(())
}

fn tar_extract(archive: &Path, dst: &Path) -> Result<()> {
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(dst)
        .status()
        .with_context(|| "running tar")?;
    if !status.success() {
        bail!("tar extract failed ({status})");
    }
    Ok(())
}

fn scratch_dir(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{prefix}{}", Uuid::new_v4()))
}

fn dir_exists(path: &Path) -> bool {
    path.is_dir()
}

fn dir_has_content(path: &Path) -> bool {
    fs::read_dir(path)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

fn confirm(prompt: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!(
            "cannot prompt for confirmation: stdin is not a TTY. \
             Pass --yes to overwrite without prompting."
        );
    }
    print!("{prompt}");
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .with_context(|| "reading stdin")?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

fn resolve_output_path(output: Option<String>) -> PathBuf {
    output.map(PathBuf::from).unwrap_or_else(|| {
        let ts = Local::now().format("%Y%m%d-%H%M%S");
        PathBuf::from(format!("lethe-backup-{ts}.tar.gz"))
    })
}
