//! `@`-prefix file-path autocomplete for the editor. Triggered by the
//! latest `@` token on the current cursor line; expands relative to the
//! configured workspace dir. Matches are kept short (top 8 by fuzzy score)
//! so the popup stays scannable.

use std::path::{Path, PathBuf};

use walkdir::WalkDir;

#[derive(Clone, Debug)]
pub struct CompletionContext {
    /// Byte offset in the current line where the `@token` starts (the `@`
    /// character itself). Used by the caller to splice the replacement in.
    pub start_col: usize,
    pub query: String,
    pub matches: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Autocomplete {
    workspace: PathBuf,
    file_index: Vec<String>,
}

impl Autocomplete {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        let file_index = build_index(&workspace);
        Self {
            workspace,
            file_index,
        }
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn refresh(&mut self) {
        self.file_index = build_index(&self.workspace);
    }

    /// Find a completion context at the current cursor position. Returns
    /// `None` when there's no `@token` on the line up to the cursor.
    pub fn context_at(&self, line: &str, cursor_col: usize) -> Option<CompletionContext> {
        let prefix: String = line.chars().take(cursor_col).collect();
        let at = prefix.rfind('@')?;
        let after = &prefix[at + 1..];
        if after.contains(char::is_whitespace) {
            return None;
        }
        let query = after.to_string();
        let matches = self.search(&query, 8);
        Some(CompletionContext {
            start_col: at,
            query,
            matches,
        })
    }

    fn search(&self, query: &str, limit: usize) -> Vec<String> {
        let lower = query.to_ascii_lowercase();
        let mut scored: Vec<(i32, &String)> = self
            .file_index
            .iter()
            .filter_map(|path| {
                let path_lower = path.to_ascii_lowercase();
                if lower.is_empty() {
                    Some((-(path.len() as i32), path))
                } else if let Some(score) = fuzzy_score(&path_lower, &lower) {
                    Some((score, path))
                } else {
                    None
                }
            })
            .collect();
        scored.sort_by_key(|(score, _)| -*score);
        scored
            .into_iter()
            .take(limit)
            .map(|(_, path)| path.clone())
            .collect()
    }
}

fn build_index(workspace: &Path) -> Vec<String> {
    if !workspace.is_dir() {
        return Vec::new();
    }
    WalkDir::new(workspace)
        .max_depth(8)
        .into_iter()
        .filter_entry(|entry| {
            // Always descend the root; only filter children. The root's name
            // may itself start with `.` (mktemp dirs, hidden workspaces),
            // and skipping it would yield zero results.
            if entry.depth() == 0 {
                return true;
            }
            let name = entry.file_name().to_string_lossy();
            !(name.starts_with('.') && name.len() > 1) && name != "target" && name != "node_modules"
        })
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| {
            entry
                .path()
                .strip_prefix(workspace)
                .ok()
                .map(|path| path.to_string_lossy().into_owned())
        })
        .take(20_000)
        .collect()
}

/// Tiny subsequence fuzzy match: scores how cleanly `needle` appears in
/// `haystack`. Higher scores favour earlier, contiguous, and word-start
/// matches; `None` when `needle` does not appear at all.
fn fuzzy_score(haystack: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(-(haystack.len() as i32));
    }
    let mut score: i32 = 0;
    let mut last_match: Option<usize> = None;
    let mut needle_chars = needle.chars().peekable();
    for (index, ch) in haystack.chars().enumerate() {
        let Some(&target) = needle_chars.peek() else {
            break;
        };
        if ch == target {
            score += 10;
            if let Some(last) = last_match {
                if index == last + 1 {
                    score += 12;
                }
                score -= ((index - last - 1) as i32).min(8);
            } else {
                score -= (index as i32).min(20);
            }
            if index == 0 || matches!(haystack.chars().nth(index.saturating_sub(1)), Some('/')) {
                score += 6;
            }
            last_match = Some(index);
            needle_chars.next();
        }
    }
    if needle_chars.peek().is_some() {
        return None;
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn context_finds_at_token_before_cursor() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().to_path_buf();
        std::fs::write(workspace.join("notes.md"), "hi").unwrap();
        let autocomplete = Autocomplete::new(&workspace);
        let context = autocomplete.context_at("see @no", 7).unwrap();
        assert_eq!(context.query, "no");
        assert!(context.matches.iter().any(|path| path == "notes.md"));
    }

    #[test]
    fn no_context_without_at_token() {
        let tmp = tempdir().unwrap();
        let autocomplete = Autocomplete::new(tmp.path());
        assert!(autocomplete.context_at("hello world", 11).is_none());
    }
}
