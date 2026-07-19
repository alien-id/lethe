use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use portable_pty::{ChildKiller, CommandBuilder, PtySize, native_pty_system};

use crate::llm::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, format_truncation_notice, truncate_tail,
};

pub const DEFAULT_TIMEOUT_SECONDS: u64 = 120;
pub const MAX_TIMEOUT_SECONDS: u64 = 600;
const TERMINAL_BUFFER_LIMIT: usize = 50_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessStatus {
    Running,
    Completed,
    Failed,
}

impl ProcessStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

pub struct BackgroundProcess {
    pub command: String,
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
    pub status: ProcessStatus,
    pub exit_code: Option<i32>,
    pub start_time: SystemTime,
    pub pid: u32,
    pub is_pty: bool,
    pub terminal_buffer: String,
    pty_writer: Option<Box<dyn Write + Send>>,
    pty_killer: Option<Box<dyn ChildKiller + Send + Sync>>,
}

type SharedProcess = Arc<Mutex<BackgroundProcess>>;

#[derive(Clone)]
pub struct ShellTools {
    cwd: PathBuf,
    processes: Arc<Mutex<HashMap<String, SharedProcess>>>,
    next_id: Arc<AtomicUsize>,
}

impl ShellTools {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            processes: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn from_env() -> Self {
        let cwd = env::var_os("USER_CWD")
            .map(PathBuf::from)
            .or_else(|| env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        Self::new(cwd)
    }

    pub fn bash(
        &self,
        command: &str,
        timeout_seconds: u64,
        run_in_background: bool,
        use_pty: bool,
    ) -> String {
        if command.trim() == "/bg" {
            return self.list_background();
        }

        let timeout_seconds = timeout_seconds.clamp(1, MAX_TIMEOUT_SECONDS);
        if run_in_background {
            if use_pty {
                self.run_background_pty(command, timeout_seconds)
            } else {
                self.run_background(command, timeout_seconds)
            }
        } else {
            self.run_foreground(command, timeout_seconds)
        }
    }

    pub fn bash_output(&self, shell_id: &str, filter_pattern: &str, last_lines: usize) -> String {
        let Some(process) = self.get_process(shell_id) else {
            return format!("No background process found with ID: {shell_id}");
        };
        let process = process.lock().expect("background process lock");
        if process.is_pty {
            return format!(
                "Process {shell_id} is running in PTY mode.\nUse get_terminal_screen('{shell_id}') to view the terminal screen.\nStatus: {}",
                process.status.as_str()
            );
        }

        let mut lines = Vec::new();
        lines.extend(process.stdout.iter().cloned());
        lines.extend(process.stderr.iter().cloned());

        if !filter_pattern.is_empty() {
            lines.retain(|line| line.contains(filter_pattern));
        }

        let omitted = if last_lines > 0 && lines.len() > last_lines {
            let omitted = lines.len() - last_lines;
            lines = lines[omitted..].to_vec();
            omitted
        } else {
            0
        };

        let mut output = lines.join("\n");
        if omitted > 0 {
            output = format!("... [{omitted} earlier lines]\n{output}");
        }
        output = truncate_output(&output);

        if output.is_empty() {
            let mut status = format!(" (status: {})", process.status.as_str());
            if let Some(exit_code) = process.exit_code {
                status.push_str(&format!(", exit code: {exit_code}"));
            }
            format!("(no output yet){status}")
        } else {
            output
        }
    }

    pub fn kill_bash(&self, shell_id: &str) -> String {
        let Some(process) = self.get_process(shell_id) else {
            return format!("No background process found with ID: {shell_id}");
        };
        if let Ok(mut process) = process.lock() {
            if process.is_pty {
                if let Some(killer) = process.pty_killer.as_mut() {
                    let _ = killer.kill();
                }
            } else {
                let _ = Command::new("kill")
                    .arg("-TERM")
                    .arg(process.pid.to_string())
                    .status();
            }
            process.status = ProcessStatus::Failed;
        }
        self.processes
            .lock()
            .expect("process registry lock")
            .remove(shell_id);
        format!("Killed background process: {shell_id}")
    }

    pub fn get_terminal_screen(&self, shell_id: &str) -> String {
        let Some(process) = self.get_process(shell_id) else {
            return format!("No background process found with ID: {shell_id}");
        };
        let process = process.lock().expect("background process lock");
        if !process.is_pty {
            return format!(
                "Process {shell_id} is not running in PTY mode.\nUse bash_output('{shell_id}') to view output.\nTo run in PTY mode, use: bash(command, run_in_background=True, use_pty=True)"
            );
        }

        let screen = if process.terminal_buffer.trim().is_empty() {
            "(no terminal output yet)".to_string()
        } else {
            truncate_output(&process.terminal_buffer)
        };
        let mut status = format!("\n--- Process: {}", process.status.as_str());
        if let Some(exit_code) = process.exit_code {
            status.push_str(&format!(", exit code: {exit_code}"));
        }
        status.push_str(" ---");
        format!("{screen}{status}")
    }

    pub fn send_terminal_input(&self, shell_id: &str, text: &str, send_enter: bool) -> String {
        let Some(process) = self.get_process(shell_id) else {
            return format!("No background process found with ID: {shell_id}");
        };
        let mut process = process.lock().expect("background process lock");
        if !process.is_pty {
            return format!("Process {shell_id} is not running in PTY mode. Cannot send input.");
        }
        if process.status != ProcessStatus::Running {
            return format!(
                "Process {shell_id} is not running (status: {})",
                process.status.as_str()
            );
        }
        let Some(writer) = process.pty_writer.as_mut() else {
            return format!("Process {shell_id} does not have a writable PTY.");
        };
        // Interactive TUI prompts (gh's survey selector, etc.) put the PTY in
        // raw mode and treat CARRIAGE RETURN as Enter — a bare LF does NOT
        // advance them. Send CR for "Enter", and translate any embedded newlines
        // too so a model that stuffs "\n" into `text` still works. Cooked-mode
        // readers (plain `read`) get CR→LF via the tty's ICRNL, so this stays
        // correct there as well.
        let mut input = text.replace('\n', "\r");
        if send_enter && !input.ends_with('\r') {
            input.push('\r');
        }
        match writer
            .write_all(input.as_bytes())
            .and_then(|_| writer.flush())
        {
            Ok(()) => format!("Sent input to {shell_id}: {text:?}"),
            Err(error) => format!("Error sending input: {error}"),
        }
    }

    pub fn get_environment_info(&self) -> String {
        let os = Command::new("uname")
            .arg("-a")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        [
            "Environment Information:".to_string(),
            format!(
                "user: {}",
                env::var("USER").unwrap_or_else(|_| "unknown".to_string())
            ),
            format!(
                "home: {}",
                env::var("HOME").unwrap_or_else(|_| "unknown".to_string())
            ),
            format!("pwd: {}", self.cwd.display()),
            format!(
                "shell: {}",
                env::var("SHELL").unwrap_or_else(|_| "unknown".to_string())
            ),
            format!("os: {os}"),
        ]
        .join("\n")
    }

    pub fn check_command_exists(&self, command_name: &str) -> String {
        if command_name.contains('/') {
            let path = Path::new(command_name);
            return if path.exists() {
                format!("'{command_name}' is available at: {}", path.display())
            } else {
                format!("'{command_name}' is not found in PATH")
            };
        }

        let Some(path_env) = env::var_os("PATH") else {
            return format!("'{command_name}' is not found in PATH");
        };
        for dir in env::split_paths(&path_env) {
            let candidate = dir.join(command_name);
            if is_executable_file(&candidate) {
                return format!("'{command_name}' is available at: {}", candidate.display());
            }
        }
        format!("'{command_name}' is not found in PATH")
    }

    pub fn list_background(&self) -> String {
        let processes = self.processes.lock().expect("process registry lock");
        if processes.is_empty() {
            return "(no background processes)".to_string();
        }

        let mut lines = Vec::new();
        for (shell_id, process) in processes.iter() {
            let process = process.lock().expect("background process lock");
            let runtime = process
                .start_time
                .elapsed()
                .map(|elapsed| format!(", runtime: {}s", elapsed.as_secs()))
                .unwrap_or_default();
            let mode = if process.is_pty { "PTY" } else { "subprocess" };
            lines.push(format!(
                "{shell_id}: {} ({}, {mode}{runtime})",
                process.command,
                process.status.as_str()
            ));
        }
        lines.sort();
        lines.join("\n")
    }

    fn run_foreground(&self, command: &str, timeout_seconds: u64) -> String {
        let mut child = match shell_command(command, &self.cwd).spawn() {
            Ok(child) => child,
            Err(error) => return format!("Error executing command: {error}"),
        };
        let pid = child.id();

        let stdout = child.stdout.take().map(read_pipe);
        let stderr = child.stderr.take().map(read_pipe);
        let deadline = Instant::now() + Duration::from_secs(timeout_seconds);

        let mut timed_out = false;
        let exit_status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        timed_out = true;
                        break None;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return format!("Error executing command: {error}"),
            }
        };

        // Whether the command exited or timed out, tear down the WHOLE process
        // group before reading the pipes. This closes the stdout/stderr write
        // ends that any backgrounded child (e.g. `gh auth login &`) is holding
        // open — otherwise join_reader() below would block forever waiting for an
        // EOF that never comes (the foreground-hang bug). It also enforces the
        // timeout on stragglers. To intentionally keep a process running past the
        // call, use run_in_background (or `setsid` to leave the group).
        kill_process_group(pid);
        let _ = child.wait();

        let stdout = join_reader(stdout);
        let stderr = join_reader(stderr);
        let mut parts = Vec::new();
        if !stdout.is_empty() {
            parts.push(stdout);
        }
        if !stderr.is_empty() {
            if !parts.is_empty() {
                parts.push("--- stderr ---".to_string());
            }
            parts.push(stderr);
        }
        let output = truncate_output(parts.join("\n").trim());

        if timed_out {
            return if output.is_empty() {
                format!("Error: Command timed out after {timeout_seconds} seconds")
            } else {
                format!("Error: Command timed out after {timeout_seconds} seconds\n{output}")
            };
        }
        if let Some(status) = exit_status
            && !status.success()
        {
            return format!("Exit code: {}\n{output}", status.code().unwrap_or(-1));
        }
        if output.is_empty() {
            "(command completed with no output)".to_string()
        } else {
            output
        }
    }

    fn run_background(&self, command: &str, timeout_seconds: u64) -> String {
        let shell_id = self.next_shell_id();
        let mut child = match shell_command(command, &self.cwd).spawn() {
            Ok(child) => child,
            Err(error) => return format!("Error starting background command: {error}"),
        };
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let process = Arc::new(Mutex::new(BackgroundProcess {
            command: command.to_string(),
            stdout: Vec::new(),
            stderr: Vec::new(),
            status: ProcessStatus::Running,
            exit_code: None,
            start_time: SystemTime::now(),
            pid,
            is_pty: false,
            terminal_buffer: String::new(),
            pty_writer: None,
            pty_killer: None,
        }));

        self.processes
            .lock()
            .expect("process registry lock")
            .insert(shell_id.clone(), process.clone());

        if let Some(stdout) = stdout {
            spawn_pipe_collector(stdout, process.clone(), false);
        }
        if let Some(stderr) = stderr {
            spawn_pipe_collector(stderr, process.clone(), true);
        }

        let monitor_process = process.clone();
        let monitor_pid = pid;
        thread::spawn(move || {
            let started = Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let mut process = monitor_process.lock().expect("background process lock");
                        process.exit_code = status.code();
                        process.status = if status.success() {
                            ProcessStatus::Completed
                        } else {
                            ProcessStatus::Failed
                        };
                        return;
                    }
                    Ok(None) => {
                        if started.elapsed() > Duration::from_secs(timeout_seconds) {
                            // Kill the whole group, not just the shell, so any
                            // children it spawned are reaped too.
                            kill_process_group(monitor_pid);
                            let _ = child.kill();
                            let _ = child.wait();
                            let mut process =
                                monitor_process.lock().expect("background process lock");
                            process.status = ProcessStatus::Failed;
                            process
                                .stderr
                                .push(format!("Command timed out after {timeout_seconds}s"));
                            return;
                        }
                        thread::sleep(Duration::from_millis(25));
                    }
                    Err(error) => {
                        let mut process = monitor_process.lock().expect("background process lock");
                        process.status = ProcessStatus::Failed;
                        process
                            .stderr
                            .push(format!("Process monitor error: {error}"));
                        return;
                    }
                }
            }
        });

        format!(
            "Command running in background with shell_id=\"{shell_id}\". \
             Read its output with bash_output(shell_id=\"{shell_id}\")."
        )
    }

    fn run_background_pty(&self, command: &str, timeout_seconds: u64) -> String {
        let shell_id = self.next_shell_id();
        let pty_system = native_pty_system();
        let pair = match pty_system.openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            Ok(pair) => pair,
            Err(error) => return format!("Error starting PTY command: {error}"),
        };

        let mut builder = CommandBuilder::new("/bin/bash");
        builder.args(["-lc", command]);
        builder.cwd(self.cwd.as_os_str());
        builder.env("TERM", "xterm-256color");

        let mut child = match pair.slave.spawn_command(builder) {
            Ok(child) => child,
            Err(error) => return format!("Error starting PTY command: {error}"),
        };
        let reader = match pair.master.try_clone_reader() {
            Ok(reader) => reader,
            Err(error) => return format!("Error starting PTY command: {error}"),
        };
        let writer = match pair.master.take_writer() {
            Ok(writer) => writer,
            Err(error) => return format!("Error starting PTY command: {error}"),
        };
        let pid = child.process_id().unwrap_or(0);
        let killer = child.clone_killer();

        let process = Arc::new(Mutex::new(BackgroundProcess {
            command: command.to_string(),
            stdout: Vec::new(),
            stderr: Vec::new(),
            status: ProcessStatus::Running,
            exit_code: None,
            start_time: SystemTime::now(),
            pid,
            is_pty: true,
            terminal_buffer: String::new(),
            pty_writer: Some(writer),
            pty_killer: Some(killer),
        }));

        self.processes
            .lock()
            .expect("process registry lock")
            .insert(shell_id.clone(), process.clone());

        spawn_pty_collector(reader, process.clone());

        thread::spawn(move || {
            let started = Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let mut process = process.lock().expect("background process lock");
                        process.exit_code = Some(status.exit_code() as i32);
                        process.status = if status.success() {
                            ProcessStatus::Completed
                        } else {
                            ProcessStatus::Failed
                        };
                        process.pty_writer = None;
                        process.pty_killer = None;
                        return;
                    }
                    Ok(None) => {
                        if started.elapsed() > Duration::from_secs(timeout_seconds) {
                            let _ = child.kill();
                            let _ = child.wait();
                            let mut process = process.lock().expect("background process lock");
                            process.status = ProcessStatus::Failed;
                            process.exit_code = Some(1);
                            process.pty_writer = None;
                            process.pty_killer = None;
                            push_terminal_output(
                                &mut process,
                                &format!("\nCommand timed out after {timeout_seconds}s\n"),
                            );
                            return;
                        }
                        thread::sleep(Duration::from_millis(25));
                    }
                    Err(error) => {
                        let mut process = process.lock().expect("background process lock");
                        process.status = ProcessStatus::Failed;
                        process.exit_code = Some(1);
                        process.pty_writer = None;
                        process.pty_killer = None;
                        push_terminal_output(
                            &mut process,
                            &format!("\nProcess monitor error: {error}\n"),
                        );
                        return;
                    }
                }
            }
        });

        format!(
            "Command running in PTY with shell_id=\"{shell_id}\". \
             View the screen (e.g. a device-login URL + code) with \
             get_terminal_screen(shell_id=\"{shell_id}\"), and type into it with \
             send_terminal_input(shell_id=\"{shell_id}\", text=\"…\"). If a login \
             code appears, send it to the user."
        )
    }

    fn get_process(&self, shell_id: &str) -> Option<SharedProcess> {
        self.processes
            .lock()
            .expect("process registry lock")
            .get(shell_id)
            .cloned()
    }

    fn next_shell_id(&self) -> String {
        let next = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        format!("bash_{next}")
    }
}

fn shell_command(command: &str, cwd: &Path) -> Command {
    let mut cmd = Command::new("/bin/bash");
    cmd.arg("-lc")
        .arg(command)
        .current_dir(cwd)
        .env("TERM", "dumb")
        // No inherited stdin: an interactive prompt (e.g. `gh auth login`) gets
        // EOF immediately instead of blocking the call forever waiting for input.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Put the shell in its OWN process group so we can later kill the whole group
    // (the shell plus anything it backgrounded) in one shot — see kill_process_group.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd
}

/// Compute the `kill` target that signals the whole process group led by `pid`.
/// Returns `None` for a degenerate pid (`0` or `1`): `format!("-{pid}")` would
/// expand to `-0` — our OWN process group — or `-1` — *every* process we are
/// permitted to signal. `kill(-1, SIGKILL)` would take down the agent process
/// itself (and, run natively rather than inside the sandboxed container, every
/// other process the user owns). A group created via `process_group(0)` always
/// has pgid == the child's pid, which is necessarily > 1, so `pid <= 1` means the
/// pid was never set / was lost and the group teardown must be skipped entirely.
fn kill_group_target(pid: u32) -> Option<String> {
    (pid > 1).then(|| format!("-{pid}"))
}

/// SIGKILL the entire process group led by `pid` (created via `process_group(0)`).
/// Best-effort. A negative target tells `kill` to signal the whole group, which
/// reaps any child the command backgrounded — both to honor the timeout and to
/// close stdout/stderr pipes a lingering child would otherwise hold open.
fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    {
        let Some(target) = kill_group_target(pid) else {
            return;
        };
        let _ = Command::new("kill")
            .arg("-KILL")
            .arg(target)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

fn truncate_output(output: &str) -> String {
    let result = truncate_tail(output, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    if !result.truncated {
        return result.content;
    }
    let start_line = result.total_lines.saturating_sub(result.output_lines) + 1;
    let notice = format_truncation_notice(&result, start_line, None);
    format!("{}\n\n{notice}", result.content)
}

fn read_pipe<R>(pipe: R) -> thread::JoinHandle<Vec<String>>
where
    R: std::io::Read + Send + 'static,
{
    thread::spawn(move || {
        BufReader::new(pipe)
            .lines()
            .map_while(Result::ok)
            .collect::<Vec<_>>()
    })
}

fn join_reader(handle: Option<thread::JoinHandle<Vec<String>>>) -> String {
    handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default()
        .join("\n")
}

fn spawn_pipe_collector<R>(pipe: R, process: SharedProcess, stderr: bool)
where
    R: std::io::Read + Send + 'static,
{
    thread::spawn(move || {
        for line in BufReader::new(pipe).lines().map_while(Result::ok) {
            let mut process = process.lock().expect("background process lock");
            if stderr {
                process.stderr.push(line);
            } else {
                process.stdout.push(line);
            }
        }
    });
}

fn spawn_pty_collector(mut reader: Box<dyn Read + Send>, process: SharedProcess) {
    thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buffer[..n]);
                    let mut process = process.lock().expect("background process lock");
                    push_terminal_output(&mut process, &chunk);
                }
                Err(_) => return,
            }
        }
    });
}

fn push_terminal_output(process: &mut BackgroundProcess, output: &str) {
    let cleaned = clean_terminal_output(output);
    if cleaned.is_empty() {
        return;
    }
    process.terminal_buffer.push_str(&cleaned);
    process.stdout.push(cleaned);
    trim_terminal_buffer(&mut process.terminal_buffer);
}

fn trim_terminal_buffer(buffer: &mut String) {
    if buffer.len() <= TERMINAL_BUFFER_LIMIT {
        return;
    }
    let keep_from = buffer.len().saturating_sub(TERMINAL_BUFFER_LIMIT);
    let keep_from = buffer
        .char_indices()
        .find_map(|(index, _)| (index >= keep_from).then_some(index))
        .unwrap_or(0);
    buffer.drain(..keep_from);
}

fn clean_terminal_output(output: &str) -> String {
    let mut cleaned = String::with_capacity(output.len());
    let mut chars = output.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() || matches!(next, '~' | '@') {
                    break;
                }
            }
            continue;
        }
        match ch {
            '\r' => {
                if chars.peek() != Some(&'\n') {
                    cleaned.push('\n');
                }
            }
            '\u{8}' => {
                cleaned.pop();
            }
            _ => cleaned.push(ch),
        }
    }
    cleaned
}

fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

use serde_json::Value;

use crate::tools::registry::ToolRegistry;
use crate::tools::registry::args::{bool_arg, string_arg, string_arg_default, u64_arg, usize_arg};
use crate::tools::spec::{ToolCategory, ToolDef, ToolExecutor, p_bool, p_int, p_str, p_str_req};

/// The background-shell tools key off the id returned by `bash`. Models
/// frequently pass it as `bash_id` or `id` rather than `shell_id`; accept all
/// three so a one-word naming slip can't strand an interactive flow — e.g. a
/// device-login PTY whose one-time code the model then can't read back.
fn shell_id_arg(args: &Value) -> String {
    for key in ["shell_id", "bash_id", "id"] {
        let value = string_arg(args, key);
        if !value.is_empty() {
            return value;
        }
    }
    String::new()
}

fn invokes_removed_agent_browser(command: &str) -> bool {
    command.to_ascii_lowercase().contains("agent-browser")
}

fn exec_bash(registry: &ToolRegistry<'_>, args: &Value) -> String {
    let command = string_arg(args, "command");
    if registry.runtime.secure_prompt.is_some() && invokes_removed_agent_browser(&command) {
        return "Error: `agent-browser` has been removed from hosted Lethe; do not install or invoke it. Call request_tool(name=\"alien_browser_open\") to load the Alien browser family, then use alien_browser_open / alien_browser_act. For forms, use alien_browser_inspect_form followed by one alien_browser_fill_form call."
            .to_string();
    }
    registry.shell.bash(
        &command,
        u64_arg(args, "timeout", DEFAULT_TIMEOUT_SECONDS),
        bool_arg(args, "run_in_background", false),
        bool_arg(args, "use_pty", false),
    )
}

fn exec_bash_output(registry: &ToolRegistry<'_>, args: &Value) -> String {
    registry.shell.bash_output(
        &shell_id_arg(args),
        &string_arg_default(args, "filter_pattern", ""),
        usize_arg(args, "last_lines", 0),
    )
}

fn exec_kill_bash(registry: &ToolRegistry<'_>, args: &Value) -> String {
    registry.shell.kill_bash(&shell_id_arg(args))
}

fn exec_get_terminal_screen(registry: &ToolRegistry<'_>, args: &Value) -> String {
    registry.shell.get_terminal_screen(&shell_id_arg(args))
}

fn exec_send_terminal_input(registry: &ToolRegistry<'_>, args: &Value) -> String {
    registry.shell.send_terminal_input(
        &shell_id_arg(args),
        &string_arg(args, "text"),
        bool_arg(args, "send_enter", true),
    )
}

fn exec_get_environment_info(registry: &ToolRegistry<'_>, _args: &Value) -> String {
    registry.shell.get_environment_info()
}

fn exec_check_command_exists(registry: &ToolRegistry<'_>, args: &Value) -> String {
    registry
        .shell
        .check_command_exists(&string_arg(args, "command_name"))
}

pub const TOOL_DEFS: &[ToolDef] = &[
    ToolDef {
        name: "bash",
        description: "Run a shell command (use run_in_background for long-running). Browser automation does NOT belong in shell: never install or invoke the removed `agent-browser`; request `alien_browser_open` and use the typed Alien browser tools instead. For INTERACTIVE commands that wait for input or print a login URL + one-time code (e.g. `gh auth login`, OAuth device flows), set run_in_background=true AND use_pty=true with a generous timeout, then read the code with get_terminal_screen and send it to the user — never run a login in the foreground (it can't be completed and is killed when the call returns).",
        params: &[
            p_str_req("command", "Shell command."),
            p_int("timeout", "Timeout (sec)."),
            p_bool("run_in_background", "Run in background."),
            p_bool("use_pty", "Background PTY mode."),
        ],
        category: ToolCategory::Initial,
        execute: ToolExecutor::Sync(exec_bash),
    },
    ToolDef {
        name: "bash_output",
        description: "Read background shell output.",
        params: &[
            p_str_req("shell_id", "Shell id."),
            p_str("filter_pattern", "Substring filter."),
            p_int("last_lines", "Last N lines (0 = all)."),
        ],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_bash_output),
    },
    ToolDef {
        name: "kill_bash",
        description: "Kill a background shell.",
        params: &[p_str_req("shell_id", "Shell id.")],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_kill_bash),
    },
    ToolDef {
        name: "get_terminal_screen",
        description: "Read a PTY background buffer.",
        params: &[p_str_req("shell_id", "PTY shell id.")],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_get_terminal_screen),
    },
    ToolDef {
        name: "send_terminal_input",
        description: "Send input to a PTY background shell.",
        params: &[
            p_str_req("shell_id", "PTY shell id."),
            p_str_req("text", "Text."),
            p_bool("send_enter", "Append Enter."),
        ],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_send_terminal_input),
    },
    ToolDef {
        name: "get_environment_info",
        description: "Show shell environment details.",
        params: &[],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_get_environment_info),
    },
    ToolDef {
        name: "check_command_exists",
        description: "Check if a command is on PATH.",
        params: &[p_str_req("command_name", "Command name.")],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_check_command_exists),
    },
];

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tempfile::tempdir;

    use super::*;

    /// Pull `bash_3` out of a `…shell_id="bash_3"…` start message.
    fn extract_shell_id(start: &str) -> &str {
        start
            .split("shell_id=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap_or_else(|| panic!("no shell_id in: {start:?}"))
    }

    #[test]
    fn kill_group_target_refuses_degenerate_pids() {
        // `-0` would signal our own process group; `-1` would signal every
        // process we may signal (kill the agent itself). Both must be skipped.
        assert_eq!(kill_group_target(0), None);
        assert_eq!(kill_group_target(1), None);
        // A real child pid (always > 1) targets that group and nothing else.
        assert_eq!(kill_group_target(2), Some("-2".to_string()));
        assert_eq!(kill_group_target(197918), Some("-197918".to_string()));
    }

    #[test]
    fn shell_id_arg_accepts_bash_id_and_id_aliases() {
        use serde_json::json;
        assert_eq!(shell_id_arg(&json!({"shell_id": "bash_1"})), "bash_1");
        assert_eq!(shell_id_arg(&json!({"bash_id": "bash_2"})), "bash_2");
        assert_eq!(shell_id_arg(&json!({"id": "bash_3"})), "bash_3");
        assert_eq!(
            shell_id_arg(&json!({"shell_id": "bash_4", "bash_id": "x"})),
            "bash_4"
        );
        assert_eq!(shell_id_arg(&json!({})), "");
    }

    #[test]
    fn removed_agent_browser_command_is_detected_without_blocking_alien_browser() {
        assert!(invokes_removed_agent_browser(
            "npm install -g agent-browser"
        ));
        assert!(invokes_removed_agent_browser(
            "AGENT-BROWSER open https://example.com"
        ));
        assert!(!invokes_removed_agent_browser(
            "agent-id-browser open --name main"
        ));
    }

    #[test]
    fn foreground_command_captures_stdout_stderr_and_exit_code() {
        let tmp = tempdir().unwrap();
        let shell = ShellTools::new(tmp.path());

        assert_eq!(shell.bash("echo hello", 5, false, false), "hello");
        assert!(
            shell
                .bash("echo error >&2", 5, false, false)
                .contains("error")
        );
        assert!(
            shell
                .bash("exit 7", 5, false, false)
                .contains("Exit code: 7")
        );
        assert_eq!(
            shell.bash("true", 5, false, false),
            "(command completed with no output)"
        );
    }

    #[test]
    fn foreground_command_times_out() {
        let tmp = tempdir().unwrap();
        let shell = ShellTools::new(tmp.path());

        let result = shell.bash("sleep 3", 1, false, false);
        assert!(result.to_ascii_lowercase().contains("timed out"));
    }

    #[test]
    fn foreground_does_not_hang_on_backgrounded_child() {
        let tmp = tempdir().unwrap();
        let shell = ShellTools::new(tmp.path());

        // `sleep 30 &` leaves a child holding the shell's stdout pipe open after
        // the foreground shell exits. Before the process-group fix this hung the
        // call forever (the `gh auth login &` bug); now killing the group closes
        // the pipe and the call returns promptly with the foreground output.
        let started = std::time::Instant::now();
        let result = shell.bash("echo hi; sleep 30 &", 10, false, false);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "command hung instead of returning promptly"
        );
        assert!(result.contains("hi"), "output was: {result:?}");
    }

    #[test]
    fn background_command_output_and_listing_work() {
        let tmp = tempdir().unwrap();
        let shell = ShellTools::new(tmp.path());

        let start = shell.bash("echo hello; sleep 0.1; echo world", 5, true, false);
        assert!(start.contains("background"));
        let shell_id = extract_shell_id(&start);

        let mut output = String::new();
        // 100 × 50ms = 5s cap. Generous for slow CI runners (notably
        // ubuntu-24.04-arm) where pipe flushes can lag well past 500ms.
        for _ in 0..100 {
            output = shell.bash_output(shell_id, "", 0);
            if output.contains("hello") && output.contains("world") {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(output.contains("hello"), "output was: {output:?}");
        assert!(output.contains("world"), "output was: {output:?}");

        let listing = shell.bash("/bg", 5, false, false);
        assert!(listing.contains(shell_id));
    }

    #[test]
    fn background_output_filters_and_limits_lines() {
        let tmp = tempdir().unwrap();
        let shell = ShellTools::new(tmp.path());
        let start = shell.bash("printf 'a\\nkeep 1\\nb\\nkeep 2\\n'", 5, true, false);
        let shell_id = extract_shell_id(&start);
        let mut filtered = String::new();
        for _ in 0..100 {
            filtered = shell.bash_output(shell_id, "keep", 1);
            if filtered.contains("keep 2") {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        assert!(filtered.contains("keep 2"), "filtered was: {filtered:?}");
        assert!(!filtered.contains("keep 1\n"));
    }

    #[test]
    fn kill_background_process() {
        let tmp = tempdir().unwrap();
        let shell = ShellTools::new(tmp.path());
        let start = shell.bash("sleep 20", 30, true, false);
        let shell_id = extract_shell_id(&start);

        let killed = shell.kill_bash(shell_id);
        assert!(killed.contains("Killed"));
        assert!(
            shell
                .bash_output(shell_id, "", 0)
                .contains("No background process")
        );
    }

    #[test]
    fn pty_process_accepts_input_and_reports_screen() {
        let tmp = tempdir().unwrap();
        let shell = ShellTools::new(tmp.path());
        let start = shell.bash("read line; echo got:$line", 5, true, true);
        assert!(start.contains("PTY"));
        let shell_id = extract_shell_id(&start);

        assert!(
            shell
                .bash_output(shell_id, "", 0)
                .contains("get_terminal_screen")
        );
        assert!(
            shell
                .send_terminal_input(shell_id, "hello", true)
                .contains("Sent input")
        );

        let mut screen = String::new();
        for _ in 0..40 {
            screen = shell.get_terminal_screen(shell_id);
            if screen.contains("got:hello") {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(screen.contains("got:hello"), "{screen}");
    }

    #[test]
    fn terminal_tools_reject_non_pty_processes() {
        let tmp = tempdir().unwrap();
        let shell = ShellTools::new(tmp.path());
        let start = shell.bash("sleep 1", 5, true, false);
        let shell_id = extract_shell_id(&start);

        assert!(
            shell
                .get_terminal_screen(shell_id)
                .contains("not running in PTY mode")
        );
        assert!(
            shell
                .send_terminal_input(shell_id, "hello", true)
                .contains("not running in PTY mode")
        );
        let _ = shell.kill_bash(shell_id);
    }

    #[test]
    fn environment_info_and_command_lookup() {
        let tmp = tempdir().unwrap();
        let shell = ShellTools::new(tmp.path());

        let info = shell.get_environment_info();
        assert!(info.to_ascii_lowercase().contains("user:"));
        assert!(info.to_ascii_lowercase().contains("pwd:"));
        assert!(info.to_ascii_lowercase().contains("home:"));

        assert!(shell.check_command_exists("ls").contains("available"));
        assert!(
            shell
                .check_command_exists("nonexistent_command_xyz")
                .contains("not found")
        );
    }
}
