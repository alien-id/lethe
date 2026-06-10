use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use super::db::{MemoryDb, MemoryKind, MemoryRow, NewMemoryRow};
use super::search::{clean_tags, indent_block, query_terms, search_result_text, tags_match_any};

const SEARCH_RESULT_MAX_LINES: usize = 50;

#[derive(Debug, Error)]
pub enum ArchivalError {
    #[error("archival memory text is required")]
    EmptyText,
    #[error("archival metadata must be a JSON object")]
    InvalidMetadata,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Embedding(#[from] anyhow::Error),
}

pub type ArchivalResult<T> = Result<T, ArchivalError>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArchivalEntry {
    pub id: String,
    pub text: String,
    pub metadata: Value,
    pub tags: Vec<String>,
    pub created_at: String,
    pub completed_at: Option<String>,
    pub completion_summary: Option<String>,
    pub score: f64,
}

impl ArchivalEntry {
    fn from_row(row: MemoryRow) -> Self {
        Self {
            id: row.id,
            text: row.text,
            metadata: row.metadata,
            tags: row.tags,
            created_at: row.created_at,
            completed_at: row.completed_at,
            completion_summary: row.completion_summary,
            score: 0.0,
        }
    }

    pub fn is_completed(&self) -> bool {
        self.completed_at.is_some()
    }
}

#[derive(Clone, Debug)]
pub struct ArchivalMemory {
    db: MemoryDb,
}

impl ArchivalMemory {
    pub fn open(data_path: impl Into<PathBuf>) -> ArchivalResult<Self> {
        Ok(Self {
            db: MemoryDb::open(data_path)?,
        })
    }

    pub fn from_db(db: MemoryDb) -> Self {
        Self { db }
    }

    pub fn embedder(&self) -> &super::semantic::EmbeddingEngine {
        self.db.embedder()
    }

    #[cfg(test)]
    fn open_with_hash_embedder(
        data_path: impl Into<PathBuf>,
        dimensions: usize,
    ) -> ArchivalResult<Self> {
        Ok(Self {
            db: MemoryDb::open_with_hash_embedder(data_path, dimensions)?,
        })
    }

    pub fn add(
        &self,
        text: &str,
        metadata: Option<Value>,
        tags: &[String],
    ) -> ArchivalResult<String> {
        let text = text.trim();
        if text.is_empty() {
            return Err(ArchivalError::EmptyText);
        }
        let metadata = metadata.unwrap_or_else(|| json!({}));
        if !metadata.is_object() {
            return Err(ArchivalError::InvalidMetadata);
        }

        let id = format!("mem-{}", Uuid::new_v4());
        let now = Utc::now().to_rfc3339();
        let embedding = self.db.embedder().embed_document(text)?;

        self.db.insert(NewMemoryRow {
            id: id.clone(),
            kind: MemoryKind::Archival,
            title: None,
            text: text.to_string(),
            metadata,
            tags: tags.to_vec(),
            file_path: None,
            created_at: now,
            updated_at: None,
            embedding,
        })?;
        Ok(id)
    }

    pub fn search(
        &self,
        query: &str,
        limit: usize,
        tags: Option<&[String]>,
    ) -> ArchivalResult<Vec<ArchivalEntry>> {
        let query = query.trim();
        let limit = if limit == 0 { 10 } else { limit };
        let terms = query_terms(query);
        let tag_filter = clean_tags(tags.unwrap_or_default());
        let mut merged: HashMap<String, ArchivalEntry> = HashMap::new();

        for row in self.db.list_kind(MemoryKind::Archival)? {
            let mut entry = ArchivalEntry::from_row(row);
            if !tags_match_any(&entry.tags, &tag_filter) {
                continue;
            }
            entry.score = score_entry(query, &terms, &entry);
            if terms.is_empty() || entry.score > 0.0 {
                merged.insert(entry.id.clone(), entry);
            }
        }

        if !query.is_empty() {
            match self
                .db
                .vector_search(MemoryKind::Archival, query, limit * 3)
            {
                Ok(scored_rows) => {
                    for scored in scored_rows {
                        let mut entry = ArchivalEntry::from_row(scored.row);
                        entry.score = scored.score;
                        if !tags_match_any(&entry.tags, &tag_filter) {
                            continue;
                        }
                        merged
                            .entry(entry.id.clone())
                            .and_modify(|existing| existing.score += entry.score)
                            .or_insert(entry);
                    }
                }
                Err(error) => {
                    tracing::warn!("archival vector search failed; using lexical results: {error}");
                }
            }
        }

        let mut entries = merged.into_values().collect::<Vec<_>>();
        entries.sort_by(compare_entries);
        entries.truncate(limit);
        Ok(entries)
    }

    pub fn get(&self, memory_id: &str) -> ArchivalResult<Option<ArchivalEntry>> {
        Ok(self
            .db
            .get(memory_id)?
            .filter(|row| row.kind == MemoryKind::Archival)
            .map(ArchivalEntry::from_row))
    }

    pub fn delete(&self, memory_id: &str) -> ArchivalResult<bool> {
        Ok(self.db.delete(memory_id)?)
    }

    pub fn update_tags(&self, memory_id: &str, tags: &[String]) -> ArchivalResult<bool> {
        Ok(self.db.update_tags(memory_id, tags)?)
    }

    /// Mark a memory row (archival or note — id is unique across kinds) as
    /// completed. Pass `None` to reopen. Returns true when a row was updated.
    pub fn set_completed_at(&self, memory_id: &str, value: Option<&str>) -> ArchivalResult<bool> {
        Ok(self.db.set_completed_at(memory_id, value)?)
    }

    /// Write or clear the curator-generated one-line summary for a completed
    /// memory. Recall renders the summary instead of the full text when set.
    pub fn set_completion_summary(
        &self,
        memory_id: &str,
        value: Option<&str>,
    ) -> ArchivalResult<bool> {
        Ok(self.db.set_completion_summary(memory_id, value)?)
    }

    /// Archival entries marked done but not yet summarised, oldest first.
    pub fn list_completed_without_summary(
        &self,
        limit: usize,
    ) -> ArchivalResult<Vec<ArchivalEntry>> {
        Ok(self
            .db
            .list_completed_without_summary(MemoryKind::Archival, limit)?
            .into_iter()
            .map(ArchivalEntry::from_row)
            .collect())
    }

    pub fn count(&self) -> ArchivalResult<usize> {
        Ok(self.db.count(MemoryKind::Archival)?)
    }

    pub fn list_recent(&self, limit: usize) -> ArchivalResult<Vec<ArchivalEntry>> {
        let mut entries = self
            .db
            .list_kind(MemoryKind::Archival)?
            .into_iter()
            .map(ArchivalEntry::from_row)
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            parse_time(&right.created_at)
                .cmp(&parse_time(&left.created_at))
                .then_with(|| left.id.cmp(&right.id))
        });
        entries.truncate(if limit == 0 { 50 } else { limit });
        Ok(entries)
    }

    pub fn all_entries(&self) -> ArchivalResult<Vec<ArchivalEntry>> {
        Ok(self
            .db
            .list_kind(MemoryKind::Archival)?
            .into_iter()
            .map(ArchivalEntry::from_row)
            .collect())
    }

    /// Render a single archival entry in full, including the entire text
    /// without the line-cap that search results apply. Used by `archival_get`.
    pub fn format_detail(entry: &ArchivalEntry) -> String {
        let mut lines = vec![format!("id: {}", entry.id)];
        lines.push(format!("created_at: {}", entry.created_at));
        if !entry.tags.is_empty() {
            lines.push(format!("tags: {}", entry.tags.join(", ")));
        }
        if entry.metadata.is_object()
            && entry
                .metadata
                .as_object()
                .is_some_and(|map| !map.is_empty())
        {
            lines.push(format!(
                "metadata: {}",
                serde_json::to_string(&entry.metadata).unwrap_or_default()
            ));
        }
        lines.push(String::new());
        lines.push(entry.text.clone());
        lines.join("\n")
    }

    pub fn format_entries(entries: &[ArchivalEntry]) -> String {
        if entries.is_empty() {
            return "No archival memories found.".to_string();
        }
        let mut lines = vec![format!("Found {} archival memory(s):", entries.len())];
        for entry in entries {
            let tags = if entry.tags.is_empty() {
                "none".to_string()
            } else {
                entry.tags.join(", ")
            };
            let score = if entry.score > 0.0 {
                format!(" score={:.2}", entry.score)
            } else {
                String::new()
            };
            lines.push(format!(
                "\n- [{}] {} ({tags}{score})\n{}",
                entry.created_at,
                entry.id,
                indent_block(
                    &search_result_text(&entry.text, SEARCH_RESULT_MAX_LINES),
                    "  "
                )
            ));
        }
        lines.join("\n")
    }
}

fn compare_entries(left: &ArchivalEntry, right: &ArchivalEntry) -> Ordering {
    right
        .score
        .partial_cmp(&left.score)
        .unwrap_or(Ordering::Equal)
        .then_with(|| parse_time(&right.created_at).cmp(&parse_time(&left.created_at)))
        .then_with(|| left.id.cmp(&right.id))
}

fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

fn score_entry(query: &str, terms: &[String], entry: &ArchivalEntry) -> f64 {
    if terms.is_empty() {
        return 1.0;
    }
    let query_lower = query.to_ascii_lowercase();
    let text_lower = entry.text.to_ascii_lowercase();
    let tags_lower = entry.tags.join(" ").to_ascii_lowercase();
    let metadata_lower = entry.metadata.to_string().to_ascii_lowercase();
    let mut score = 0.0;

    if !query_lower.is_empty() && text_lower.contains(&query_lower) {
        score += 5.0;
    }
    for term in terms {
        score += text_lower.matches(term).count() as f64;
        if tags_lower.split_whitespace().any(|tag| tag == term) {
            score += 3.0;
        } else if tags_lower.contains(term) {
            score += 1.5;
        }
        if metadata_lower.contains(term) {
            score += 1.0;
        }
    }
    score
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::memory::semantic::LEGACY_EMBEDDING_DIMENSIONS;

    fn memory() -> (tempfile::TempDir, ArchivalMemory) {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("memory.db");
        let memory =
            ArchivalMemory::open_with_hash_embedder(path, LEGACY_EMBEDDING_DIMENSIONS).unwrap();
        (tmp, memory)
    }

    #[test]
    fn add_get_count_and_delete_archival_memories() {
        let (_tmp, memory) = memory();
        let first = memory
            .add("hello there", Some(json!({"source": "test"})), &[])
            .unwrap();
        let second = memory
            .add("another entry", None, &["misc".to_string()])
            .unwrap();

        assert_eq!(memory.count().unwrap(), 2);
        let entry = memory.get(&first).unwrap().unwrap();
        assert_eq!(entry.text, "hello there");
        assert_eq!(entry.metadata["source"], "test");

        assert!(memory.delete(&second).unwrap());
        assert_eq!(memory.count().unwrap(), 1);
    }

    #[test]
    fn search_combines_lexical_and_vector() {
        let (_tmp, memory) = memory();
        memory
            .add(
                "graph api email outlook tokens",
                None,
                &["rust".to_string(), "email".to_string()],
            )
            .unwrap();
        memory
            .add(
                "cargo fmt and clippy checks for Rust",
                None,
                &["rust".to_string()],
            )
            .unwrap();

        let results = memory.search("graph api email", 10, None).unwrap();
        assert!(!results.is_empty());
        assert!(results[0].text.contains("graph api"));

        let filtered = memory
            .search("graph cargo", 10, Some(&["rust".to_string()]))
            .unwrap();
        assert!(
            filtered
                .iter()
                .all(|entry| entry.tags.contains(&"rust".to_string()))
        );
    }

    #[test]
    fn update_tags_replaces_tag_set() {
        let (_tmp, memory) = memory();
        let id = memory.add("hello", None, &["alpha".to_string()]).unwrap();
        assert!(
            memory
                .update_tags(&id, &["beta".to_string(), "gamma".to_string()])
                .unwrap()
        );
        let entry = memory.get(&id).unwrap().unwrap();
        assert_eq!(entry.tags, vec!["beta".to_string(), "gamma".to_string()]);
    }
}
