use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::Settings;
use crate::todos::{TodoError, TodoManager};

use super::archival::{ArchivalEntry, ArchivalError, ArchivalMemory};
use super::blocks::{BlockManager, MemoryBlock, MemoryError};
use super::db::MemoryDb;
use super::messages::{MessageHistory, MessageHistoryError, MessageRole, StoredMessage};
use super::notes::{NoteError, NoteSearchResult, NoteStore};

#[derive(Debug, Error)]
pub enum MemoryStoreError {
    #[error(transparent)]
    Blocks(#[from] MemoryError),
    #[error(transparent)]
    Archival(#[from] ArchivalError),
    #[error(transparent)]
    Messages(#[from] MessageHistoryError),
    #[error(transparent)]
    Notes(#[from] NoteError),
    #[error(transparent)]
    Todos(#[from] TodoError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

pub type MemoryStoreResult<T> = Result<T, MemoryStoreError>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MemoryStats {
    pub memory_blocks: usize,
    pub archival_memories: usize,
    pub message_history: usize,
    pub total_messages: usize,
    pub notes: usize,
}

#[derive(Debug)]
pub struct MemoryStore {
    pub blocks: BlockManager,
    pub archival: ArchivalMemory,
    pub messages: MessageHistory,
    pub notes: NoteStore,
    pub todos: TodoManager,
    memory_data_path: PathBuf,
    workspace_dir: PathBuf,
}

impl MemoryStore {
    pub fn from_settings(settings: &Settings) -> MemoryStoreResult<Self> {
        Self::open_with_data_path(
            &settings.paths.workspace_dir,
            Some(&settings.paths.db_path),
            &settings.paths.notes_dir,
            settings.paths.memory_dir.join("lethe-memory.db"),
        )
    }

    pub fn open(
        workspace_dir: impl Into<PathBuf>,
        db_path: impl Into<PathBuf>,
        notes_dir: impl Into<PathBuf>,
    ) -> MemoryStoreResult<Self> {
        let workspace_dir = workspace_dir.into();
        let db_path = db_path.into();
        let notes_dir = notes_dir.into();
        let memory_data_path = memory_data_path_for(&db_path);
        Self::open_with_data_path(workspace_dir, Some(db_path), notes_dir, memory_data_path)
    }

    pub fn open_with_data_path(
        workspace_dir: impl Into<PathBuf>,
        legacy_db_path: Option<impl Into<PathBuf>>,
        notes_dir: impl Into<PathBuf>,
        memory_data_path: impl Into<PathBuf>,
    ) -> MemoryStoreResult<Self> {
        let workspace_dir = workspace_dir.into();
        let legacy_db_path = legacy_db_path.map(Into::into);
        let notes_dir = notes_dir.into();
        let memory_data_path = memory_data_path.into();

        fs::create_dir_all(&workspace_dir)?;
        fs::create_dir_all(workspace_dir.join("projects"))?;
        // Verifier-pattern scaffold: prompts reference these directories for
        // Phase 0 (criteria), Phase 0.5 (plan), and Phase 2 (verification log).
        // Plans are required for multi-step state-touching work; they
        // recurse with bounded depth so each sub-step is itself well-defined.
        // Empty dirs are cheap; missing dirs break the workflow the prompts
        // describe.
        fs::create_dir_all(workspace_dir.join("acceptance_criteria"))?;
        fs::create_dir_all(workspace_dir.join("plans"))?;
        fs::create_dir_all(workspace_dir.join("verification_logs"))?;
        ensure_skills_bootstrap(&workspace_dir.join("skills"))?;

        let blocks = BlockManager::new(workspace_dir.join("memory"))?;
        blocks.init_embedded_defaults()?;
        let memory_db = MemoryDb::open(&memory_data_path)?;
        let archival = ArchivalMemory::from_db(memory_db.clone());
        let messages =
            MessageHistory::open_with_embedder(&memory_data_path, memory_db.embedder().clone())?;
        let notes = NoteStore::new_with_db(notes_dir, memory_db)?;
        let todos = TodoManager::open(&memory_data_path)?;
        if let Some(legacy) = legacy_db_path.as_deref() {
            migrate_legacy_todos(legacy, &memory_data_path)?;
        }

        Ok(Self {
            blocks,
            archival,
            messages,
            notes,
            todos,
            memory_data_path,
            workspace_dir,
        })
    }

    pub fn memory_data_path(&self) -> &Path {
        &self.memory_data_path
    }

    pub fn workspace_dir(&self) -> &Path {
        &self.workspace_dir
    }

    pub fn stats(&self) -> MemoryStoreResult<MemoryStats> {
        let memory_blocks = self.blocks.list_blocks(true)?.len();
        let archival_memories = self.archival.count()?;
        let message_history = self.messages.count()?;
        let notes = self.notes.list_notes(None)?.len();
        Ok(MemoryStats {
            memory_blocks,
            archival_memories,
            message_history,
            total_messages: message_history,
            notes,
        })
    }

    pub fn get_context_for_prompt(&self) -> MemoryStoreResult<String> {
        let (stable, volatile) = self.get_context_split()?;
        Ok([stable, volatile]
            .into_iter()
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"))
    }

    /// Current rolling summary of conversation turns that have aged out of the
    /// live history window. Maintained by the summarizer; the prompt assembler
    /// renders it as a dedicated `<conversation_summary>` block.
    pub fn conversation_summary(&self) -> MemoryStoreResult<String> {
        Ok(self
            .blocks
            .get(super::blocks::CONVERSATION_SUMMARY_LABEL)?
            .map(|block| block.value)
            .unwrap_or_default())
    }

    /// Replace the rolling conversation summary. Called by the summarizer
    /// after merging the dropped batch with the previous summary. Uses the
    /// system bypass so the block can stay `read_only` against the LLM.
    pub fn set_conversation_summary(&self, value: &str) -> MemoryStoreResult<()> {
        self.blocks
            .system_update(super::blocks::CONVERSATION_SUMMARY_LABEL, value)
            .map(|_| ())
            .map_err(Into::into)
    }

    pub fn get_context_split(&self) -> MemoryStoreResult<(String, String)> {
        let blocks = self.blocks.list_blocks(false)?;
        let mut stable_blocks = Vec::new();
        let mut volatile_blocks = Vec::new();

        for block in &blocks {
            if block.hidden || block.label == "identity" {
                continue;
            }
            let formatted = format_block(block);
            if block.stable {
                stable_blocks.push(formatted);
            } else {
                volatile_blocks.push(formatted);
            }
        }

        let mut stable_parts = Vec::new();
        let mut volatile_parts = Vec::new();
        if !stable_blocks.is_empty() {
            stable_parts.push(format!(
                "<memory_blocks_stable>\n{}\n</memory_blocks_stable>",
                stable_blocks.join("\n\n")
            ));
        }
        if !volatile_blocks.is_empty() {
            volatile_parts.push(format!(
                "<memory_blocks>\n{}\n</memory_blocks>",
                volatile_blocks.join("\n\n")
            ));
        }

        volatile_parts.push(self.format_memory_metadata(&blocks)?);

        Ok((stable_parts.join("\n\n"), volatile_parts.join("\n\n")))
    }

    pub fn search_archival(
        &self,
        query: &str,
        limit: usize,
        tags: Option<&[String]>,
    ) -> MemoryStoreResult<Vec<ArchivalEntry>> {
        self.archival
            .search(query, normalized_limit(limit, 10), tags)
            .map_err(Into::into)
    }

    pub fn search_messages(
        &self,
        query: &str,
        limit: usize,
        role: Option<&MessageRole>,
    ) -> MemoryStoreResult<Vec<StoredMessage>> {
        self.messages
            .search(query, normalized_limit(limit, 20), role)
            .map_err(Into::into)
    }

    pub fn search_notes(
        &self,
        query: &str,
        tags: Option<&[String]>,
        limit: usize,
    ) -> MemoryStoreResult<Vec<NoteSearchResult>> {
        self.notes
            .search(query, tags, normalized_limit(limit, 5))
            .map_err(Into::into)
    }

    /// Mark a memory row (archival or note) as completed. Accepts a row id or a
    /// note file path. Returns the resolved id on success, or None if nothing
    /// matched.
    pub fn complete_memory(&self, identifier: &str) -> MemoryStoreResult<Option<String>> {
        self.set_memory_completion(identifier, Some(Utc::now().to_rfc3339()))
    }

    /// Clear the completed_at flag on a memory row. Inverse of `complete_memory`.
    pub fn reopen_memory(&self, identifier: &str) -> MemoryStoreResult<Option<String>> {
        self.set_memory_completion(identifier, None)
    }

    /// Write or clear the curator-generated one-line summary for a completed
    /// memory. Returns the resolved id when a row matched.
    pub fn set_completion_summary(
        &self,
        identifier: &str,
        summary: &str,
    ) -> MemoryStoreResult<Option<String>> {
        let identifier = identifier.trim();
        if identifier.is_empty() {
            return Ok(None);
        }
        let value = if summary.trim().is_empty() {
            None
        } else {
            Some(summary.trim())
        };
        if self.archival.set_completion_summary(identifier, value)? {
            return Ok(Some(identifier.to_string()));
        }
        if let Some(row) = self.notes.find_row_by_path(Path::new(identifier))? {
            self.archival.set_completion_summary(&row.id, value)?;
            return Ok(Some(row.id));
        }
        Ok(None)
    }

    fn set_memory_completion(
        &self,
        identifier: &str,
        value: Option<String>,
    ) -> MemoryStoreResult<Option<String>> {
        let identifier = identifier.trim();
        if identifier.is_empty() {
            return Ok(None);
        }
        // Try as a row id first — archival uses `mem-...` ids, notes also have
        // ids in the DB.
        if self
            .archival
            .set_completed_at(identifier, value.as_deref())?
        {
            return Ok(Some(identifier.to_string()));
        }
        // Fall back to resolving as a note file path.
        if let Some(row) = self.notes.find_row_by_path(Path::new(identifier))? {
            self.archival.set_completed_at(&row.id, value.as_deref())?;
            return Ok(Some(row.id));
        }
        Ok(None)
    }

    fn format_memory_metadata(&self, blocks: &[MemoryBlock]) -> MemoryStoreResult<String> {
        let now = Utc::now();
        let last_modified = blocks
            .iter()
            .filter_map(|block| block.updated_at.or(block.created_at))
            .max()
            .unwrap_or(now);
        let message_count = self.messages.count()?;
        let archival_count = self.archival.count()?;

        let mut lines = vec![
            "<memory_metadata>".to_string(),
            format!(
                "- memory_blocks_last_modified={}",
                format_timestamp(last_modified)
            ),
        ];
        if message_count > 0 {
            lines.push(format!(
                "- {message_count} previous messages in recall memory (search via conversation_search)"
            ));
        }
        if archival_count > 0 {
            lines.push(format!(
                "- {archival_count} archival memories (search via archival_search)"
            ));
        }
        lines.push("</memory_metadata>".to_string());
        Ok(lines.join("\n"))
    }
}

fn ensure_skills_bootstrap(skills_dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(skills_dir)?;
    let readme = skills_dir.join("README.md");
    if readme.exists() {
        return Ok(());
    }
    fs::write(
        readme,
        "# Skills\n\n\
This directory stores skill files with extended workflows and references.\n\
This README is intentionally always present so skills are discoverable.\n\n\
Use core tools to work with skills:\n\
- list_directory(\"workspace/skills/\")\n\
- read_file(\"workspace/skills/README.md\")\n\
- read_file(\"workspace/skills/<name>.md\")\n\
- grep_search(\"keyword\", path=\"workspace/skills/\")\n",
    )
}

fn format_block(block: &MemoryBlock) -> String {
    let mut lines = vec![format!("<{}>", block.label)];
    if !block.description.trim().is_empty() {
        lines.push("<description>".to_string());
        lines.push(block.description.clone());
        lines.push("</description>".to_string());
    }
    lines.push("<metadata>".to_string());
    lines.push(format!("- chars={}/{}", block.value.len(), block.limit));
    if let Some(created_at) = block.created_at {
        lines.push(format!("- created_at={}", format_timestamp(created_at)));
    }
    if let Some(updated_at) = block.updated_at {
        lines.push(format!("- updated_at={}", format_timestamp(updated_at)));
    }
    lines.extend([
        "</metadata>".to_string(),
        "<value>".to_string(),
        block.value.clone(),
        "</value>".to_string(),
        format!("</{}>", block.label),
    ]);
    lines.join("\n")
}

fn format_timestamp(time: DateTime<Utc>) -> String {
    time.with_timezone(&Local)
        .format("%a %Y-%m-%d %H:%M:%S %Z")
        .to_string()
}

fn memory_data_path_for(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .map(|parent| parent.join("memory").join("lethe-memory.db"))
        .unwrap_or_else(|| PathBuf::from("memory").join("lethe-memory.db"))
}

/// Copy todos from the pre-consolidation `lethe.db` into the unified memory
/// DB if (1) the legacy DB exists, (2) it has a `todos` table, and (3) the
/// unified DB has no todos yet. Subsequent runs are no-ops.
fn migrate_legacy_todos(legacy_db_path: &Path, unified_db_path: &Path) -> MemoryStoreResult<()> {
    if !legacy_db_path.exists() || legacy_db_path == unified_db_path {
        return Ok(());
    }

    let unified = rusqlite::Connection::open(unified_db_path)?;
    let existing_count: i64 =
        unified.query_row("SELECT COUNT(*) FROM todos", [], |row| row.get(0))?;
    if existing_count > 0 {
        return Ok(());
    }

    let legacy = rusqlite::Connection::open(legacy_db_path)?;
    let has_table: bool = legacy
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type='table' AND name='todos')",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);
    if !has_table {
        return Ok(());
    }

    struct LegacyTodoRow {
        id: i64,
        title: String,
        description: Option<String>,
        status: String,
        priority: String,
        created_at: Option<String>,
        updated_at: Option<String>,
        completed_at: Option<String>,
        due_date: Option<String>,
        last_reminded_at: Option<String>,
        remind_count: i64,
        tags: Option<String>,
        source: Option<String>,
    }

    let mut stmt = legacy.prepare(
        "SELECT id, title, description, status, priority, created_at, updated_at, completed_at, \
         due_date, last_reminded_at, remind_count, tags, source FROM todos",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(LegacyTodoRow {
                id: row.get(0)?,
                title: row.get(1)?,
                description: row.get(2)?,
                status: row.get(3)?,
                priority: row.get(4)?,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
                completed_at: row.get(7)?,
                due_date: row.get(8)?,
                last_reminded_at: row.get(9)?,
                remind_count: row.get(10)?,
                tags: row.get(11)?,
                source: row.get(12)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if rows.is_empty() {
        return Ok(());
    }

    let inserted = rows.len();
    let mut writer = unified;
    let tx = writer.transaction()?;
    for row in rows {
        tx.execute(
            "INSERT INTO todos (id, title, description, status, priority, created_at, \
             updated_at, completed_at, due_date, last_reminded_at, remind_count, tags, source) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            rusqlite::params![
                row.id,
                row.title,
                row.description,
                row.status,
                row.priority,
                row.created_at,
                row.updated_at,
                row.completed_at,
                row.due_date,
                row.last_reminded_at,
                row.remind_count,
                row.tags,
                row.source,
            ],
        )?;
    }
    tx.commit()?;
    tracing::info!(
        legacy = %legacy_db_path.display(),
        unified = %unified_db_path.display(),
        rows = inserted,
        "migrated legacy todos into unified memory DB"
    );
    Ok(())
}

fn normalized_limit(limit: usize, default: usize) -> usize {
    if limit == 0 { default } else { limit }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    fn store() -> (tempfile::TempDir, MemoryStore) {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let db_path = tmp.path().join("data").join("lethe.db");
        let notes_dir = workspace.join("notes");
        let store = MemoryStore::open(workspace, db_path, notes_dir).unwrap();
        (tmp, store)
    }

    #[test]
    fn open_initializes_workspace_blocks_and_stats() {
        let (tmp, store) = store();

        assert!(store.workspace_dir().join("skills/README.md").exists());
        assert!(store.workspace_dir().join("projects").exists());
        assert!(store.workspace_dir().join("acceptance_criteria").exists());
        assert!(store.workspace_dir().join("plans").exists());
        assert!(store.workspace_dir().join("verification_logs").exists());
        let labels = store
            .blocks
            .list_blocks(true)
            .unwrap()
            .into_iter()
            .map(|block| block.label)
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            vec!["conversation_summary", "human", "identity", "project"]
        );

        store
            .notes
            .create("Test note", "body", &["skill".to_string()], None)
            .unwrap();
        let stats = store.stats().unwrap();
        assert_eq!(stats.memory_blocks, 4);
        assert_eq!(stats.notes, 1);
        assert_eq!(stats.total_messages, 0);
        drop(tmp);
    }

    #[test]
    fn context_split_formats_blocks_and_metadata_counts() {
        let (_tmp, store) = store();
        store
            .blocks
            .update("human", Some("User prefers concise answers."), None)
            .unwrap();
        store
            .blocks
            .update("project", Some("Port Lethe to Rust."), None)
            .unwrap();
        store
            .messages
            .add(MessageRole::User, "hello", None)
            .unwrap();
        store
            .archival
            .add(
                "Remember the Rust port context.",
                Some(json!({"source": "test"})),
                &[],
            )
            .unwrap();

        let (stable, volatile) = store.get_context_split().unwrap();
        assert!(stable.contains("<memory_blocks_stable>"));
        assert!(stable.contains("User prefers concise answers."));
        assert!(volatile.contains("<memory_blocks>"));
        assert!(volatile.contains("Port Lethe to Rust."));
        assert!(volatile.contains("- 1 previous messages"));
        assert!(volatile.contains("- 1 archival memories"));
        assert!(!stable.contains("<identity>"));

        let combined = store.get_context_for_prompt().unwrap();
        assert!(combined.contains("<memory_metadata>"));
    }
}
