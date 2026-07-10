use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use sha2::{Digest, Sha256};

const AGENT_BROWSER: &str = "agent-browser";
const DEFAULT_TIMEOUT_SECONDS: u64 = 60;

#[derive(Clone, Debug)]
pub struct BrowserTools {
    profile_dir: PathBuf,
    session_name: String,
    close_on_drop: bool,
    used: Arc<AtomicBool>,
}

#[derive(Debug)]
struct BrowserCommandOutput {
    stdout: String,
    stderr: String,
    code: i32,
}

impl BrowserTools {
    pub fn new(cache_dir: impl Into<PathBuf>) -> Self {
        Self::with_lifecycle(cache_dir, false)
    }

    pub fn hosted(cache_dir: impl Into<PathBuf>) -> Self {
        Self::with_lifecycle(cache_dir, true)
    }

    fn with_lifecycle(cache_dir: impl Into<PathBuf>, close_on_drop: bool) -> Self {
        let profile_dir = cache_dir.into().join("browser");
        let digest = Sha256::digest(profile_dir.to_string_lossy().as_bytes());
        let session_suffix = digest[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Self {
            profile_dir,
            session_name: format!("lethe-{session_suffix}"),
            close_on_drop,
            used: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn open(&self, url: &str) -> String {
        let result = self.run_agent_browser(&["open".to_string(), url.to_string()]);
        match result {
            Ok(output) if output.code == 0 => json!({
                "status": "OK",
                "url": url,
                "message": if output.stdout.trim().is_empty() {
                    format!("Navigated to {url}")
                } else {
                    output.stdout.trim().to_string()
                },
            }),
            Ok(output) => error_json(output.stderr, output.stdout, "Failed to open URL"),
            Err(error) => json!({"status": "error", "message": error}),
        }
        .to_string()
    }

    pub fn snapshot(&self, interactive_only: bool, compact: bool) -> String {
        let mut args = vec!["snapshot".to_string()];
        if interactive_only {
            args.push("-i".to_string());
        }
        if compact {
            args.push("-c".to_string());
        }
        let result = self.run_agent_browser(&args);
        match result {
            Ok(output) if output.code == 0 => json!({
                "status": "OK",
                "snapshot": output.stdout.trim(),
            }),
            Ok(output) => error_json(output.stderr, output.stdout, "Failed to get snapshot"),
            Err(error) => json!({"status": "error", "message": error}),
        }
        .to_string()
    }

    pub fn click(&self, ref_or_selector: &str) -> String {
        let result = self.run_agent_browser(&["click".to_string(), ref_or_selector.to_string()]);
        match result {
            Ok(output) if output.code == 0 => json!({
                "status": "OK",
                "message": format!("Clicked {ref_or_selector}"),
            }),
            Ok(output) => error_json(
                output.stderr,
                output.stdout,
                &format!("Failed to click {ref_or_selector}"),
            ),
            Err(error) => json!({"status": "error", "message": error}),
        }
        .to_string()
    }

    pub fn fill(&self, ref_or_selector: &str, text: &str) -> String {
        let result = self.run_agent_browser(&[
            "fill".to_string(),
            ref_or_selector.to_string(),
            text.to_string(),
        ]);
        match result {
            Ok(output) if output.code == 0 => json!({
                "status": "OK",
                "message": format!("Filled {ref_or_selector} with text"),
            }),
            Ok(output) => error_json(
                output.stderr,
                output.stdout,
                &format!("Failed to fill {ref_or_selector}"),
            ),
            Err(error) => json!({"status": "error", "message": error}),
        }
        .to_string()
    }

    fn run_agent_browser(&self, args: &[String]) -> Result<BrowserCommandOutput, String> {
        fs::create_dir_all(&self.profile_dir)
            .map_err(|error| format!("Failed to create browser profile directory: {error}"))?;
        let program = find_executable(AGENT_BROWSER).ok_or_else(|| {
            "agent-browser not found. Install with: npm install -g agent-browser".to_string()
        })?;
        if args.first().is_none_or(|arg| arg != "close") {
            self.used.store(true, Ordering::Relaxed);
        }
        run_command(
            &program,
            &self.agent_browser_args(args),
            Duration::from_secs(DEFAULT_TIMEOUT_SECONDS),
        )
    }

    fn agent_browser_args(&self, args: &[String]) -> Vec<String> {
        let mut command_args = Vec::new();
        let installing = args.iter().any(|arg| arg == "install");
        if !installing && !args.iter().any(|arg| arg == "--session") {
            command_args.push("--session".to_string());
            command_args.push(self.session_name.clone());
        }
        if !installing && !args.iter().any(|arg| arg == "--profile") {
            command_args.push("--profile".to_string());
            command_args.push(self.profile_dir.display().to_string());
        }
        command_args.extend(args.iter().cloned());
        command_args
    }
}

impl Drop for BrowserTools {
    fn drop(&mut self) {
        if !self.close_on_drop
            || Arc::strong_count(&self.used) != 1
            || !self.used.swap(false, Ordering::Relaxed)
        {
            return;
        }
        let Some(program) = find_executable(AGENT_BROWSER) else {
            return;
        };
        let _ = run_command(
            &program,
            &self.agent_browser_args(&["close".to_string()]),
            Duration::from_secs(5),
        );
    }
}

fn error_json(stderr: String, stdout: String, fallback: &str) -> serde_json::Value {
    let message = if !stderr.trim().is_empty() {
        stderr.trim()
    } else if !stdout.trim().is_empty() {
        stdout.trim()
    } else {
        fallback
    };
    json!({"status": "error", "message": message})
}

fn run_command(
    program: &Path,
    args: &[String],
    timeout: Duration,
) -> Result<BrowserCommandOutput, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Failed to start agent-browser: {error}"))?;

    let stdout = child.stdout.take().map(read_pipe);
    let stderr = child.stderr.take().map(read_pipe);
    let deadline = Instant::now() + timeout;

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(BrowserCommandOutput {
                        stdout: join_output(stdout),
                        stderr: format!("Command timed out after {}s", timeout.as_secs()),
                        code: -1,
                    });
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(format!("Failed to wait for agent-browser: {error}")),
        }
    };

    Ok(BrowserCommandOutput {
        stdout: join_output(stdout),
        stderr: join_output(stderr),
        code: status.code().unwrap_or(-1),
    })
}

fn read_pipe<T: Read + Send + 'static>(mut pipe: T) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).to_string()
    })
}

fn join_output(handle: Option<thread::JoinHandle<String>>) -> String {
    handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default()
}

fn find_executable(command: &str) -> Option<PathBuf> {
    if command.contains('/') {
        let path = PathBuf::from(command);
        return is_executable_file(&path).then_some(path);
    }

    let path_env = env::var_os("PATH")?;
    for dir in env::split_paths(&path_env) {
        let candidate = dir.join(command);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

use serde_json::Value;

use crate::tools::registry::ToolRegistry;
use crate::tools::registry::args::{bool_arg, string_arg};
use crate::tools::spec::{ToolCategory, ToolDef, ToolExecutor, p_bool, p_str_req};

fn exec_browser_open(registry: &ToolRegistry<'_>, args: &Value) -> String {
    registry.browser.open(&string_arg(args, "url"))
}

fn exec_browser_snapshot(registry: &ToolRegistry<'_>, args: &Value) -> String {
    registry.browser.snapshot(
        bool_arg(args, "interactive_only", true),
        bool_arg(args, "compact", true),
    )
}

fn exec_browser_click(registry: &ToolRegistry<'_>, args: &Value) -> String {
    registry.browser.click(&string_arg(args, "ref_or_selector"))
}

fn exec_browser_fill(registry: &ToolRegistry<'_>, args: &Value) -> String {
    registry.browser.fill(
        &string_arg(args, "ref_or_selector"),
        &string_arg(args, "text"),
    )
}

pub const TOOL_DEFS: &[ToolDef] = &[
    ToolDef {
        name: "browser_open",
        description: "Open a URL in the persistent browser.",
        params: &[p_str_req("url", "URL.")],
        category: ToolCategory::BrowserBuiltin,
        execute: ToolExecutor::Sync(exec_browser_open),
    },
    ToolDef {
        name: "browser_snapshot",
        description: "Accessibility snapshot of the page with element refs.",
        params: &[
            p_bool("interactive_only", "Only interactive elements."),
            p_bool("compact", "Omit empty structural elements."),
        ],
        category: ToolCategory::BrowserBuiltin,
        execute: ToolExecutor::Sync(exec_browser_snapshot),
    },
    ToolDef {
        name: "browser_click",
        description: "Click an element by snapshot ref or selector.",
        params: &[p_str_req(
            "ref_or_selector",
            "Element ref (@e1) or selector.",
        )],
        category: ToolCategory::BrowserBuiltin,
        execute: ToolExecutor::Sync(exec_browser_click),
    },
    ToolDef {
        name: "browser_fill",
        description: "Fill a text input by snapshot ref or selector.",
        params: &[
            p_str_req("ref_or_selector", "Element ref (@e1) or selector."),
            p_str_req("text", "Text."),
        ],
        category: ToolCategory::BrowserBuiltin,
        execute: ToolExecutor::Sync(exec_browser_fill),
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    // The built-in browser tools MUST be BrowserBuiltin (not Requestable) — that
    // category is what hides them when the agent-id vault-sealed browser is active,
    // so the agent is never offered two competing browsers. Guard against a silent
    // revert to Requestable.
    #[test]
    fn builtin_browser_tools_use_the_gated_category() {
        assert!(!TOOL_DEFS.is_empty());
        for def in TOOL_DEFS {
            assert_eq!(
                def.category,
                crate::tools::spec::ToolCategory::BrowserBuiltin,
                "{} must be BrowserBuiltin so it hides under the vault-sealed browser",
                def.name,
            );
        }
    }

    #[test]
    fn browser_args_add_persistent_profile() {
        let tools = BrowserTools::new("/tmp/lethe-cache");
        let args =
            tools.agent_browser_args(&["open".to_string(), "https://example.com".to_string()]);

        assert_eq!(args[0], "--session");
        assert!(args[1].starts_with("lethe-"));
        assert_eq!(
            &args[2..],
            [
                "--profile",
                "/tmp/lethe-cache/browser",
                "open",
                "https://example.com"
            ]
        );
    }

    #[test]
    fn browser_args_preserve_explicit_profile_and_install() {
        let tools = BrowserTools::new("/tmp/lethe-cache");
        let explicit = tools.agent_browser_args(&[
            "--profile".to_string(),
            "/tmp/custom".to_string(),
            "open".to_string(),
            "https://example.com".to_string(),
        ]);
        assert_eq!(
            &explicit[2..],
            ["--profile", "/tmp/custom", "open", "https://example.com"]
        );
        assert_eq!(explicit[0], "--session");

        let install = tools.agent_browser_args(&["install".to_string()]);
        assert_eq!(install, vec!["install"]);
    }

    #[test]
    fn cache_paths_get_distinct_browser_sessions() {
        let first = BrowserTools::hosted("/tmp/lethe/tenant-a/cache");
        let second = BrowserTools::hosted("/tmp/lethe/tenant-b/cache");

        assert_ne!(first.session_name, second.session_name);
        assert!(first.close_on_drop);
        assert!(second.close_on_drop);
    }

    #[test]
    fn error_json_prefers_stderr_then_stdout() {
        assert_eq!(
            error_json("bad".to_string(), "ok".to_string(), "fallback")["message"],
            "bad"
        );
        assert_eq!(
            error_json("".to_string(), "out".to_string(), "fallback")["message"],
            "out"
        );
        assert_eq!(
            error_json("".to_string(), "".to_string(), "fallback")["message"],
            "fallback"
        );
    }
}
