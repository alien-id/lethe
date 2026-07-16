use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use glob::Pattern;
use regex::Regex;
use walkdir::WalkDir;

use crate::llm::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, GREP_MAX_LINE_LENGTH, format_size,
    format_truncation_notice, truncate_head, truncate_line,
};

const MAX_GLOB_RESULTS: usize = 100;
const MAX_GREP_MATCHES: usize = 200;

#[derive(Clone, Debug)]
pub struct FileTools {
    workspace_dir: PathBuf,
    /// Hosted jail: when set, every path must resolve (symlinks included)
    /// inside `workspace_dir`. Standalone keeps full-machine access.
    sandboxed: bool,
}

impl FileTools {
    pub fn new(workspace_dir: impl Into<PathBuf>) -> Self {
        Self {
            workspace_dir: workspace_dir.into(),
            sandboxed: false,
        }
    }

    /// Workspace-jailed file tools for hosted multiplexers: absolute paths are
    /// allowed only inside the workspace, `..`/tilde/symlink escapes are
    /// rejected after resolution against the real filesystem.
    pub fn sandboxed(workspace_dir: impl Into<PathBuf>) -> Self {
        Self {
            workspace_dir: workspace_dir.into(),
            sandboxed: true,
        }
    }

    /// Resolve an existing regular file using this instance's access policy.
    /// Browser uploads use this seam so hosted form filling can attach a file
    /// from the tenant workspace without reopening arbitrary host filesystem
    /// access through a generic browser `--files` flag.
    pub(crate) fn resolve_existing_file(&self, raw_path: &str) -> Result<PathBuf, String> {
        let path = self.resolve_path(raw_path)?;
        if !path.exists() {
            return Err(format!("File not found: {raw_path}"));
        }
        if !path.is_file() {
            return Err(format!("Not a file: {raw_path}"));
        }
        path.canonicalize()
            .map_err(|error| format!("could not resolve {raw_path}: {error}"))
    }

    pub fn read_file(&self, file_path: &str, offset: usize, limit: usize) -> String {
        match self.try_read_file(file_path, offset, limit) {
            Ok(output) => output,
            Err(error) => error,
        }
    }

    pub fn write_file(&self, file_path: &str, content: &str) -> String {
        match self.resolve_path(file_path) {
            Ok(path) => {
                if let Some(parent) = path.parent()
                    && let Err(error) = fs::create_dir_all(parent)
                {
                    return format!("Error writing file: {error}");
                }
                match fs::write(&path, content) {
                    Ok(()) => format!(
                        "Successfully wrote {} characters to {}",
                        content.len(),
                        path.display()
                    ),
                    Err(error) => format!("Error writing file: {error}"),
                }
            }
            Err(error) => format!("Error writing file: {error}"),
        }
    }

    pub fn edit_file(
        &self,
        file_path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> String {
        let path = match self.resolve_path(file_path) {
            Ok(path) => path,
            Err(error) => return format!("Error editing file: {error}"),
        };
        if !path.exists() {
            return format!("Error: File not found: {file_path}");
        }

        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) => return format!("Error editing file: {error}"),
        };

        if !content.contains(old_string) {
            return format!(
                "Error: String not found in file: {:?}",
                preview(old_string, 100)
            );
        }

        let count = content.matches(old_string).count();
        if !replace_all && count > 1 {
            return format!(
                "Error: String appears multiple times ({count}). Use replace_all=True or provide more context."
            );
        }

        let new_content = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };

        match fs::write(&path, new_content) {
            Ok(()) => format!(
                "Successfully replaced {count} occurrence(s) in {}",
                path.display()
            ),
            Err(error) => format!("Error editing file: {error}"),
        }
    }

    pub fn list_directory(&self, path: &str, show_hidden: bool) -> String {
        let dir_path = match self.resolve_path(path) {
            Ok(path) => path,
            Err(error) => return format!("Error listing directory: {error}"),
        };
        if dir_path == Path::new("/") {
            return "Error: Refusing to list filesystem root. Use a narrower path (prefer WORKSPACE_DIR).".to_string();
        }
        if !dir_path.exists() {
            return format!("Error: Directory not found: {}", dir_path.display());
        }
        if !dir_path.is_dir() {
            return format!("Error: Not a directory: {}", dir_path.display());
        }

        let mut entries = Vec::new();
        let read_dir = match fs::read_dir(&dir_path) {
            Ok(read_dir) => read_dir,
            Err(error) => return format!("Error listing directory: {error}"),
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if !show_hidden && name.starts_with('.') {
                continue;
            }

            if path.is_dir() {
                entries.push(format!("[DIR]  {name}/"));
            } else if path.is_symlink() {
                let target = fs::read_link(&path)
                    .map(|target| target.display().to_string())
                    .unwrap_or_else(|_| "<unreadable>".to_string());
                entries.push(format!("[LINK] {name} -> {target}"));
            } else {
                let size = path.metadata().map(|meta| meta.len()).unwrap_or(0);
                entries.push(format!("[FILE] {name} ({})", compact_size(size)));
            }
        }

        entries.sort();
        if entries.is_empty() {
            format!("{} is empty", dir_path.display())
        } else {
            format!(
                "Contents of {}:\n{}",
                dir_path.display(),
                entries.join("\n")
            )
        }
    }

    pub fn glob_search(&self, pattern: &str, path: &str) -> String {
        let base_path = match self.resolve_path(path) {
            Ok(path) => path,
            Err(error) => return format!("Error in glob search: {error}"),
        };
        if is_broad_recursive_target(&base_path) {
            return "Error: Refusing broad recursive search in root/home. Set a narrower path (prefer WORKSPACE_DIR).".to_string();
        }
        if !base_path.exists() {
            return format!(
                "Error in glob search: base path not found: {}",
                base_path.display()
            );
        }

        let Ok(glob_pattern) = Pattern::new(pattern) else {
            return format!("Error in glob search: invalid glob pattern: {pattern}");
        };
        let recursive = pattern.contains("**");
        let mut matches = Vec::new();
        for entry in WalkDir::new(&base_path)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            let entry_path = entry.path();
            if entry_path == base_path {
                continue;
            }
            let Ok(relative) = entry_path.strip_prefix(&base_path) else {
                continue;
            };
            if !recursive && relative.components().count() > 1 {
                continue;
            }
            if glob_pattern.matches_path(relative) {
                matches.push(entry_path.to_path_buf());
            }
        }

        matches.sort_by_key(|path| std::cmp::Reverse(modified(path)));
        let truncated = matches.len() > MAX_GLOB_RESULTS;
        matches.truncate(MAX_GLOB_RESULTS);

        let result = matches
            .iter()
            .map(|path| relative_or_absolute(path, &base_path))
            .collect::<Vec<_>>();
        let mut header = format!(
            "Found {} files matching '{pattern}' in {}",
            result.len(),
            base_path.display()
        );
        if truncated {
            header.push_str(" (showing first 100)");
        }
        if result.is_empty() {
            format!("No files matching '{pattern}' in {}", base_path.display())
        } else {
            format!("{header}\n{}", result.join("\n"))
        }
    }

    pub fn grep_search(&self, pattern: &str, path: &str, file_pattern: &str) -> String {
        let base_path = match self.resolve_path(path) {
            Ok(path) => path,
            Err(error) => return format!("Error in grep search: {error}"),
        };
        if is_broad_recursive_target(&base_path) {
            return "Error: Refusing broad recursive search in root/home. Set a narrower path (prefer WORKSPACE_DIR).".to_string();
        }
        let regex = match Regex::new(pattern) {
            Ok(regex) => regex,
            Err(error) => return format!("Error in grep search: {error}"),
        };
        let glob_pattern = match Pattern::new(file_pattern) {
            Ok(pattern) => pattern,
            Err(error) => return format!("Error in grep search: {error}"),
        };

        let mut results = Vec::new();
        let mut files_searched = 0;
        let mut matches_found = 0;
        let mut lines_truncated = 0;

        for entry in WalkDir::new(&base_path)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            let file_path = entry.path();
            if !file_path.is_file() {
                continue;
            }
            if file_path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.'))
            {
                continue;
            }
            let Ok(relative) = file_path.strip_prefix(&base_path) else {
                continue;
            };
            if !glob_pattern.matches_path(relative)
                && !file_path
                    .file_name()
                    .is_some_and(|name| glob_pattern.matches(&name.to_string_lossy()))
            {
                continue;
            }

            files_searched += 1;
            let Ok(content) = fs::read_to_string(file_path) else {
                continue;
            };
            for (line_index, line) in content.lines().enumerate() {
                if regex.is_match(line) {
                    let (line, was_truncated) = truncate_line(line, GREP_MAX_LINE_LENGTH);
                    if was_truncated {
                        lines_truncated += 1;
                    }
                    results.push(format!(
                        "{}:{}: {}",
                        relative_or_absolute(file_path, &base_path),
                        line_index + 1,
                        line
                    ));
                    matches_found += 1;
                    if matches_found >= MAX_GREP_MATCHES {
                        break;
                    }
                }
            }
            if matches_found >= MAX_GREP_MATCHES {
                break;
            }
        }

        let mut header = format!("Found {matches_found} matches in {files_searched} files");
        if matches_found >= MAX_GREP_MATCHES {
            header.push_str(" (limit reached)");
        }
        if lines_truncated > 0 {
            header.push_str(&format!(
                " ({lines_truncated} lines truncated to 500 chars)"
            ));
        }

        if results.is_empty() {
            format!("No matches for '{pattern}' in {}", base_path.display())
        } else {
            format!("{header}\n{}", results.join("\n"))
        }
    }

    fn try_read_file(
        &self,
        file_path: &str,
        offset: usize,
        limit: usize,
    ) -> Result<String, String> {
        let path = self.resolve_path(file_path)?;
        if !path.exists() {
            return Err(format!("Error: File not found: {file_path}"));
        }
        if !path.is_file() {
            return Err(format!("Error: Not a file: {file_path}"));
        }

        let mut file =
            fs::File::open(&path).map_err(|error| format!("Error reading file: {error}"))?;
        let mut content = String::new();
        file.read_to_string(&mut content)
            .map_err(|error| format!("Error reading file: {error}"))?;
        let lines = split_preserving_logical_lines(&content);
        let total_lines = lines.len();
        let start_index = if offset > 0 { offset - 1 } else { 0 };
        if start_index >= total_lines {
            return Err(format!(
                "Error: Offset {offset} is beyond end of file ({total_lines} lines total)"
            ));
        }

        let selected_end = if limit > 0 {
            total_lines.min(start_index + limit)
        } else {
            total_lines
        };
        let selected = lines[start_index..selected_end].join("\n");
        let result = truncate_head(&selected, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);

        let mut output = result.content.clone();
        if result.truncated {
            let notice = format_truncation_notice(&result, start_index + 1, None);
            output.push_str("\n\n");
            output.push_str(&notice);
        } else if limit > 0 && selected_end < total_lines {
            let remaining = total_lines - selected_end;
            let next_offset = selected_end + 1;
            output.push_str(&format!(
                "\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]"
            ));
        }

        Ok(output)
    }

    fn resolve_path(&self, raw_path: &str) -> Result<PathBuf, String> {
        if self.sandboxed {
            return resolve_jailed_path(raw_path, &self.workspace_dir);
        }
        resolve_workspace_path(raw_path, &self.workspace_dir)
    }
}

/// Resolve a path and require it to land inside `workspace_dir` after every
/// indirection is unwound: `..` and symlinks via canonicalization of the
/// longest existing prefix, tilde expansion via the normal resolver. Used by
/// the hosted-jailed tool constructors; error strings are model-facing.
pub(crate) fn resolve_jailed_path(raw_path: &str, workspace_dir: &Path) -> Result<PathBuf, String> {
    let root = workspace_dir
        .canonicalize()
        .map_err(|error| format!("workspace unavailable: {error}"))?;
    let resolved = resolve_workspace_path(raw_path, workspace_dir)?;
    // `resolve_workspace_path` canonicalizes existing paths, but a
    // not-yet-existing target (write_file into a new subtree) keeps its
    // non-existent suffix verbatim. Canonicalize the longest existing
    // ancestor so a symlinked or `..`-laden prefix cannot smuggle the
    // final path outside the jail, then re-attach the pending suffix.
    let checked = canonicalize_existing_prefix(&resolved)?;
    if checked.starts_with(&root) {
        Ok(resolved)
    } else {
        Err(format!(
            "Access denied: {raw_path} resolves outside the workspace"
        ))
    }
}

/// Canonicalize the longest existing prefix of `path` and append the
/// remaining (not yet existing) components verbatim. Rejects `..` inside a
/// non-existent suffix — there is nothing on disk to resolve it against.
fn canonicalize_existing_prefix(path: &Path) -> Result<PathBuf, String> {
    let mut existing = path.to_path_buf();
    let mut pending = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(name) => pending.push(name.to_os_string()),
            None => {
                return Err(format!("Access denied: cannot resolve {}", path.display()));
            }
        }
        let Some(parent) = existing.parent() else {
            return Err(format!("Access denied: cannot resolve {}", path.display()));
        };
        existing = parent.to_path_buf();
    }
    let mut base = existing.canonicalize().map_err(|error| error.to_string())?;
    for name in pending.iter().rev() {
        base.push(name);
    }
    Ok(base)
}

fn resolve_workspace_path(raw_path: &str, workspace_dir: &Path) -> Result<PathBuf, String> {
    let expanded = expand_tilde(raw_path);
    let path = if expanded.trim().is_empty() || expanded.trim() == "." {
        workspace_dir.to_path_buf()
    } else {
        let candidate = PathBuf::from(expanded);
        if candidate.is_absolute() {
            candidate
        } else {
            workspace_dir.join(candidate)
        }
    };
    normalize_path(path)
}

fn normalize_path(path: PathBuf) -> Result<PathBuf, String> {
    if path.exists() {
        path.canonicalize().map_err(|error| error.to_string())
    } else {
        absolutize(&path).map_err(|error| error.to_string())
    }
}

fn absolutize(path: &Path) -> std::io::Result<PathBuf> {
    if path.exists() {
        path.canonicalize()
    } else if let Some(parent) = path.parent() {
        let parent = if parent.exists() {
            parent.canonicalize()?
        } else {
            parent.to_path_buf()
        };
        Ok(parent.join(path.file_name().unwrap_or_default()))
    } else {
        Ok(path.to_path_buf())
    }
}

fn expand_tilde(path: &str) -> String {
    if path == "~" {
        home_dir().display().to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home_dir().join(rest).display().to_string()
    } else {
        path.to_string()
    }
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn is_broad_recursive_target(path: &Path) -> bool {
    path == Path::new("/") || path == home_dir()
}

fn modified(path: &Path) -> SystemTime {
    path.metadata()
        .and_then(|metadata| metadata.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn relative_or_absolute(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

fn compact_size(size: u64) -> String {
    if size < 1024 {
        format!("{size}B")
    } else if size < 1024 * 1024 {
        format!("{}KB", size / 1024)
    } else {
        format!("{}MB", size / (1024 * 1024))
    }
}

fn preview(value: &str, max_chars: usize) -> String {
    crate::llm::truncate::truncate_with_ellipsis(value, max_chars)
}

fn split_preserving_logical_lines(content: &str) -> Vec<String> {
    if content.is_empty() {
        vec![String::new()]
    } else if let Some(stripped) = content.strip_suffix('\n') {
        stripped.split('\n').map(ToString::to_string).collect()
    } else {
        content.split('\n').map(ToString::to_string).collect()
    }
}

#[allow(dead_code)]
fn _format_size_for_tools(bytes_count: usize) -> String {
    format_size(bytes_count)
}

use serde_json::Value;

use crate::tools::registry::ToolRegistry;
use crate::tools::registry::args::{bool_arg, string_arg, string_arg_default, usize_arg};
use crate::tools::spec::{ToolCategory, ToolDef, ToolExecutor, p_bool, p_int, p_str, p_str_req};

fn exec_read_file(registry: &ToolRegistry<'_>, args: &Value) -> String {
    registry.files.read_file(
        &string_arg(args, "file_path"),
        usize_arg(args, "offset", 0),
        usize_arg(args, "limit", 0),
    )
}

fn exec_write_file(registry: &ToolRegistry<'_>, args: &Value) -> String {
    registry
        .files
        .write_file(&string_arg(args, "file_path"), &string_arg(args, "content"))
}

fn exec_edit_file(registry: &ToolRegistry<'_>, args: &Value) -> String {
    registry.files.edit_file(
        &string_arg(args, "file_path"),
        &string_arg(args, "old_string"),
        &string_arg(args, "new_string"),
        bool_arg(args, "replace_all", false),
    )
}

fn exec_list_directory(registry: &ToolRegistry<'_>, args: &Value) -> String {
    registry.files.list_directory(
        &string_arg_default(args, "path", "."),
        bool_arg(args, "show_hidden", false),
    )
}

fn exec_glob_search(registry: &ToolRegistry<'_>, args: &Value) -> String {
    registry.files.glob_search(
        &string_arg(args, "pattern"),
        &string_arg_default(args, "path", "."),
    )
}

fn exec_grep_search(registry: &ToolRegistry<'_>, args: &Value) -> String {
    registry.files.grep_search(
        &string_arg(args, "pattern"),
        &string_arg_default(args, "path", "."),
        &string_arg_default(args, "file_pattern", "*"),
    )
}

pub const TOOL_DEFS: &[ToolDef] = &[
    ToolDef {
        name: "read_file",
        description: "Read a file.",
        params: &[
            p_str_req("file_path", "Path."),
            p_int("offset", "Line offset (0 = start)."),
            p_int("limit", "Max lines (0 = default)."),
        ],
        category: ToolCategory::Initial,
        execute: ToolExecutor::Sync(exec_read_file),
    },
    ToolDef {
        name: "write_file",
        description: "Write a file (creates parent dirs).",
        params: &[
            p_str_req("file_path", "Path."),
            p_str_req("content", "File content."),
        ],
        category: ToolCategory::Initial,
        execute: ToolExecutor::Sync(exec_write_file),
    },
    ToolDef {
        name: "edit_file",
        description: "Replace exact text in a file.",
        params: &[
            p_str_req("file_path", "Path."),
            p_str_req("old_string", "Exact text to replace."),
            p_str_req("new_string", "Replacement."),
            p_bool("replace_all", "Replace all occurrences."),
        ],
        category: ToolCategory::Initial,
        execute: ToolExecutor::Sync(exec_edit_file),
    },
    ToolDef {
        name: "list_directory",
        description: "List a directory.",
        params: &[
            p_str("path", "Directory (default: workspace)."),
            p_bool("show_hidden", "Include hidden files."),
        ],
        category: ToolCategory::Initial,
        execute: ToolExecutor::Sync(exec_list_directory),
    },
    ToolDef {
        name: "glob_search",
        description: "Find files by glob.",
        params: &[
            p_str_req("pattern", "Glob pattern."),
            p_str("path", "Base path (default: workspace)."),
        ],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_glob_search),
    },
    ToolDef {
        name: "grep_search",
        description: "Regex search across file contents.",
        params: &[
            p_str_req("pattern", "Regex."),
            p_str("path", "Base path (default: workspace)."),
            p_str("file_pattern", "File glob (default: *)."),
        ],
        category: ToolCategory::Initial,
        execute: ToolExecutor::Sync(exec_grep_search),
    },
];

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn read_existing_file_returns_raw_lines() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("test.txt");
        fs::write(&path, "line 1\nline 2\nline 3\n").unwrap();
        let tools = FileTools::new(tmp.path());

        let result = tools.read_file(path.to_str().unwrap(), 0, 0);

        assert_eq!(result, "line 1\nline 2\nline 3");
    }

    #[test]
    fn read_offset_limit_and_continuation_notice() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("test.txt");
        let content = (0..10)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, content).unwrap();
        let tools = FileTools::new(tmp.path());

        let result = tools.read_file(path.to_str().unwrap(), 3, 3);

        assert!(result.contains("line 2"));
        assert!(result.contains("line 3"));
        assert!(result.contains("line 4"));
        assert!(!result.contains("line 0"));
        assert!(!result.contains("line 5"));
        assert!(result.contains("Use offset=6"));
    }

    #[test]
    fn write_file_creates_parents_and_edit_handles_occurrences() {
        let tmp = tempdir().unwrap();
        let tools = FileTools::new(tmp.path());
        let path = tmp.path().join("nested/test.txt");

        let result = tools.write_file(path.to_str().unwrap(), "foo bar foo");
        assert!(result.contains("Successfully"));
        assert!(path.exists());

        let result = tools.edit_file(path.to_str().unwrap(), "foo", "baz", false);
        assert!(result.contains("multiple times"));

        let result = tools.edit_file(path.to_str().unwrap(), "foo", "baz", true);
        assert!(result.contains("2 occurrence"));
        assert!(
            tools
                .read_file(path.to_str().unwrap(), 0, 0)
                .contains("baz bar baz")
        );
    }

    #[test]
    fn list_directory_respects_hidden_flag_and_root_guard() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("visible"), "").unwrap();
        fs::write(tmp.path().join(".hidden"), "").unwrap();
        fs::create_dir(tmp.path().join("subdir")).unwrap();
        let tools = FileTools::new(tmp.path());

        let result = tools.list_directory(tmp.path().to_str().unwrap(), false);
        assert!(result.contains("visible"));
        assert!(result.contains("[DIR]  subdir/"));
        assert!(!result.contains(".hidden"));

        let result = tools.list_directory(tmp.path().to_str().unwrap(), true);
        assert!(result.contains(".hidden"));

        assert!(tools.list_directory("/", false).contains("Refusing"));
    }

    #[test]
    fn glob_search_supports_plain_and_recursive_patterns() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("root.rs"), "").unwrap();
        fs::write(tmp.path().join("note.txt"), "").unwrap();
        fs::create_dir(tmp.path().join("subdir")).unwrap();
        fs::write(tmp.path().join("subdir/nested.rs"), "").unwrap();
        let tools = FileTools::new(tmp.path());

        let plain = tools.glob_search("*.rs", tmp.path().to_str().unwrap());
        assert!(plain.contains("Found 1 files"));
        assert!(plain.contains("root.rs"));
        assert!(!plain.contains("nested.rs"));

        let recursive = tools.glob_search("**/*.rs", tmp.path().to_str().unwrap());
        assert!(recursive.contains("root.rs"));
        assert!(recursive.contains("subdir/nested.rs"));
    }

    #[test]
    fn grep_search_supports_regex_and_file_pattern() {
        let tmp = tempdir().unwrap();
        fs::write(
            tmp.path().join("code.rs"),
            "hello world\nfoo bar\nhello again\n",
        )
        .unwrap();
        fs::write(tmp.path().join("text.txt"), "hello text\n").unwrap();
        let tools = FileTools::new(tmp.path());

        let result = tools.grep_search("hello", tmp.path().to_str().unwrap(), "*.rs");
        assert!(result.contains("Found 2 matches"));
        assert!(result.contains("code.rs:1: hello world"));
        assert!(result.contains("code.rs:3: hello again"));
        assert!(!result.contains("text.txt"));

        let regex = tools.grep_search(r"foo\s+bar", tmp.path().to_str().unwrap(), "*");
        assert!(regex.contains("foo bar"));
    }

    #[test]
    fn grep_truncates_long_match_lines() {
        let tmp = tempdir().unwrap();
        fs::write(
            tmp.path().join("test.txt"),
            format!("match {}\n", "x".repeat(700)),
        )
        .unwrap();
        let tools = FileTools::new(tmp.path());

        let result = tools.grep_search("match", tmp.path().to_str().unwrap(), "*");
        assert!(result.contains("lines truncated"));
        assert!(result.contains("[truncated]"));
    }
}
