use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::codec::{ensure_parent, f32_slice_as_bytes, open_conn, parent_dir, semantic_score};
use super::search::clean_tags;
use super::semantic::{EmbeddingEngine, LEGACY_EMBEDDING_DIMENSIONS};

pub const TABLE_NAME: &str = "memory";
pub const VEC_TABLE_NAME: &str = "memory_vec";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Archival,
    Note,
}

impl MemoryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Archival => "archival",
            Self::Note => "note",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryRow {
    pub id: String,
    pub kind: MemoryKind,
    pub title: Option<String>,
    pub text: String,
    pub metadata: Value,
    pub tags: Vec<String>,
    pub file_path: Option<String>,
    pub created_at: String,
    pub updated_at: Option<String>,
    pub completed_at: Option<String>,
    pub completion_summary: Option<String>,
}

#[derive(Clone, Debug)]
pub struct NewMemoryRow {
    pub id: String,
    pub kind: MemoryKind,
    pub title: Option<String>,
    pub text: String,
    pub metadata: Value,
    pub tags: Vec<String>,
    pub file_path: Option<String>,
    pub created_at: String,
    pub updated_at: Option<String>,
    pub embedding: Vec<f32>,
}

#[derive(Clone, Debug)]
pub struct ScoredRow {
    pub row: MemoryRow,
    pub score: f64,
}

#[derive(Clone, Debug)]
pub struct MemoryDb {
    data_path: PathBuf,
    embedder: EmbeddingEngine,
}

impl MemoryDb {
    pub fn open(data_path: impl Into<PathBuf>) -> rusqlite::Result<Self> {
        let data_path = data_path.into();
        let db = Self {
            embedder: EmbeddingEngine::from_env(parent_dir(&data_path)),
            data_path,
        };
        db.ensure_schema()?;
        Ok(db)
    }

    #[cfg(test)]
    pub(crate) fn open_with_hash_embedder(
        data_path: impl Into<PathBuf>,
        dimensions: usize,
    ) -> rusqlite::Result<Self> {
        let data_path = data_path.into();
        let db = Self {
            embedder: EmbeddingEngine::with_hash_dimensions(dimensions),
            data_path,
        };
        db.ensure_schema()?;
        Ok(db)
    }

    pub fn data_path(&self) -> &Path {
        &self.data_path
    }

    pub fn embedder(&self) -> &EmbeddingEngine {
        &self.embedder
    }

    pub fn open_conn(&self) -> rusqlite::Result<Connection> {
        open_conn(&self.data_path)
    }

    fn ensure_schema(&self) -> rusqlite::Result<()> {
        ensure_parent(&self.data_path)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        let conn = self.open_conn()?;
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS {table} (
                id                 TEXT PRIMARY KEY,
                kind               TEXT NOT NULL,
                title              TEXT,
                text               TEXT NOT NULL,
                metadata           TEXT NOT NULL DEFAULT '{{}}',
                tags               TEXT NOT NULL DEFAULT '[]',
                file_path          TEXT UNIQUE,
                created_at         TEXT NOT NULL,
                updated_at         TEXT,
                completed_at       TEXT,
                completion_summary TEXT
            );
            CREATE INDEX IF NOT EXISTS {table}_kind_idx          ON {table} (kind);
            CREATE INDEX IF NOT EXISTS {table}_created_at_idx    ON {table} (created_at);
            CREATE VIRTUAL TABLE IF NOT EXISTS {vec_table} USING vec0(
                id TEXT PRIMARY KEY,
                embedding float[{dim}]
            );",
            table = TABLE_NAME,
            vec_table = VEC_TABLE_NAME,
            dim = LEGACY_EMBEDDING_DIMENSIONS,
        ))?;
        ensure_optional_columns(&conn)?;
        Ok(())
    }

    pub fn insert(&self, row: NewMemoryRow) -> rusqlite::Result<()> {
        let metadata_str = serde_json::to_string(&row.metadata)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        let tags_str = serde_json::to_string(&clean_tags(&row.tags))
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        let mut conn = self.open_conn()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO memory (id, kind, title, text, metadata, tags, file_path, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                row.id,
                row.kind.as_str(),
                row.title,
                row.text,
                metadata_str,
                tags_str,
                row.file_path,
                row.created_at,
                row.updated_at,
            ],
        )?;
        tx.execute(
            "INSERT INTO memory_vec (id, embedding) VALUES (?, ?)",
            params![row.id, f32_slice_as_bytes(&row.embedding)],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete(&self, id: &str) -> rusqlite::Result<bool> {
        let mut conn = self.open_conn()?;
        let tx = conn.transaction()?;
        let removed = tx.execute("DELETE FROM memory WHERE id = ?", params![id])?;
        tx.execute("DELETE FROM memory_vec WHERE id = ?", params![id])?;
        tx.commit()?;
        Ok(removed > 0)
    }

    pub fn delete_by_file_path(&self, file_path: &str) -> rusqlite::Result<usize> {
        let mut conn = self.open_conn()?;
        let tx = conn.transaction()?;
        let ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM memory WHERE file_path = ?")?;
            stmt.query_map(params![file_path], |row| row.get::<_, String>(0))?
                .collect::<Result<_, _>>()?
        };
        for id in &ids {
            tx.execute("DELETE FROM memory WHERE id = ?", params![id])?;
            tx.execute("DELETE FROM memory_vec WHERE id = ?", params![id])?;
        }
        tx.commit()?;
        Ok(ids.len())
    }

    pub fn delete_kind(&self, kind: MemoryKind) -> rusqlite::Result<usize> {
        let mut conn = self.open_conn()?;
        let tx = conn.transaction()?;
        let ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM memory WHERE kind = ?")?;
            stmt.query_map(params![kind.as_str()], |row| row.get::<_, String>(0))?
                .collect::<Result<_, _>>()?
        };
        for id in &ids {
            tx.execute("DELETE FROM memory_vec WHERE id = ?", params![id])?;
        }
        tx.execute("DELETE FROM memory WHERE kind = ?", params![kind.as_str()])?;
        tx.commit()?;
        Ok(ids.len())
    }

    pub fn update_tags(&self, id: &str, tags: &[String]) -> rusqlite::Result<bool> {
        let tags_str = serde_json::to_string(&clean_tags(tags))
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        let conn = self.open_conn()?;
        let updated = conn.execute(
            "UPDATE memory SET tags = ? WHERE id = ?",
            params![tags_str, id],
        )?;
        Ok(updated > 0)
    }

    pub fn get(&self, id: &str) -> rusqlite::Result<Option<MemoryRow>> {
        let conn = self.open_conn()?;
        conn.query_row(
            "SELECT id, kind, title, text, metadata, tags, file_path, created_at, updated_at, completed_at, completion_summary \
             FROM memory WHERE id = ?",
            params![id],
            row_to_memory,
        )
        .optional()
    }

    pub fn get_by_file_path(&self, file_path: &str) -> rusqlite::Result<Option<MemoryRow>> {
        let conn = self.open_conn()?;
        conn.query_row(
            "SELECT id, kind, title, text, metadata, tags, file_path, created_at, updated_at, completed_at, completion_summary \
             FROM memory WHERE file_path = ?",
            params![file_path],
            row_to_memory,
        )
        .optional()
    }

    /// Mark a memory row as completed (idempotent). Returns true if a row was
    /// updated. The recall pipeline keeps completed rows visible; when the
    /// `completion_summary` column is populated by a later curator pass, recall
    /// renders the one-line summary instead of the full text.
    pub fn set_completed_at(&self, id: &str, value: Option<&str>) -> rusqlite::Result<bool> {
        let conn = self.open_conn()?;
        let updated = conn.execute(
            "UPDATE memory SET completed_at = ? WHERE id = ?",
            params![value, id],
        )?;
        Ok(updated > 0)
    }

    /// Write the curator-generated one-line summary for a completed memory.
    /// Recall will prefer this over the full text once it's set.
    pub fn set_completion_summary(&self, id: &str, value: Option<&str>) -> rusqlite::Result<bool> {
        let conn = self.open_conn()?;
        let updated = conn.execute(
            "UPDATE memory SET completion_summary = ? WHERE id = ?",
            params![value, id],
        )?;
        Ok(updated > 0)
    }

    /// Rows that have been marked done but don't yet have a curator summary.
    /// Ordered by completion time so the curator can summarise the oldest
    /// pending entries first.
    pub fn list_completed_without_summary(
        &self,
        kind: MemoryKind,
        limit: usize,
    ) -> rusqlite::Result<Vec<MemoryRow>> {
        let conn = self.open_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, kind, title, text, metadata, tags, file_path, created_at, updated_at, completed_at, completion_summary \
             FROM memory \
             WHERE kind = ? AND completed_at IS NOT NULL AND completion_summary IS NULL \
             ORDER BY completed_at ASC \
             LIMIT ?",
        )?;
        let rows = stmt.query_map(params![kind.as_str(), limit as i64], row_to_memory)?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(row?);
        }
        Ok(entries)
    }

    pub fn count(&self, kind: MemoryKind) -> rusqlite::Result<usize> {
        let conn = self.open_conn()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM memory WHERE kind = ?",
            params![kind.as_str()],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    pub fn list_kind(&self, kind: MemoryKind) -> rusqlite::Result<Vec<MemoryRow>> {
        let conn = self.open_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, kind, title, text, metadata, tags, file_path, created_at, updated_at, completed_at, completion_summary \
             FROM memory WHERE kind = ? ORDER BY id",
        )?;
        let rows = stmt.query_map(params![kind.as_str()], row_to_memory)?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(row?);
        }
        Ok(entries)
    }

    pub fn vector_search(
        &self,
        kind: MemoryKind,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<ScoredRow>> {
        let query_vector = self.embedder.embed_query(query)?;
        let limit = limit.max(1);
        let conn = self.open_conn()?;
        let mut stmt = conn.prepare(
            "SELECT m.id, m.kind, m.title, m.text, m.metadata, m.tags, m.file_path, m.created_at, m.updated_at, m.completed_at, m.completion_summary, v.distance \
             FROM memory_vec v \
             JOIN memory m ON m.id = v.id \
             WHERE v.embedding MATCH ? AND k = ? AND m.kind = ? \
             ORDER BY v.distance",
        )?;
        let rows = stmt.query_map(
            params![
                f32_slice_as_bytes(&query_vector),
                limit as i64,
                kind.as_str()
            ],
            |row| {
                let memory = row_to_memory(row)?;
                let distance: f64 = row.get(11)?;
                Ok(ScoredRow {
                    row: memory,
                    score: semantic_score(distance),
                })
            },
        )?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }
}

pub fn row_to_memory(row: &Row<'_>) -> rusqlite::Result<MemoryRow> {
    let id: String = row.get(0)?;
    let kind_raw: String = row.get(1)?;
    let title: Option<String> = row.get(2)?;
    let text: String = row.get(3)?;
    let metadata_raw: String = row.get(4)?;
    let tags_raw: String = row.get(5)?;
    let file_path: Option<String> = row.get(6)?;
    let created_at: String = row.get(7)?;
    let updated_at: Option<String> = row.get(8)?;
    let completed_at: Option<String> = row.get(9)?;
    let completion_summary: Option<String> = row.get(10)?;

    let kind = match kind_raw.as_str() {
        "archival" => MemoryKind::Archival,
        "note" => MemoryKind::Note,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown memory kind: {other}"),
                )),
            ));
        }
    };

    let metadata = serde_json::from_str(&metadata_raw).unwrap_or_else(|_| json!({}));
    let metadata = if metadata.is_object() {
        metadata
    } else {
        json!({})
    };
    let tags: Vec<String> = serde_json::from_str(&tags_raw).unwrap_or_default();

    Ok(MemoryRow {
        id,
        kind,
        title,
        text,
        metadata,
        tags,
        file_path,
        created_at,
        updated_at,
        completed_at,
        completion_summary,
    })
}

/// Idempotent additive migrations for nullable text columns on the memory
/// table. SQLite has no `ADD COLUMN IF NOT EXISTS`, so we probe table_info
/// first. Adding more columns later is just one more entry in the list.
fn ensure_optional_columns(conn: &Connection) -> rusqlite::Result<()> {
    let existing: Vec<String> = {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", TABLE_NAME))?;
        stmt.query_map([], |row| row.get::<_, String>(1))?
            .filter_map(Result::ok)
            .collect()
    };
    let extras: &[(&str, &str)] = &[("completed_at", "TEXT"), ("completion_summary", "TEXT")];
    for (name, ty) in extras {
        if !existing.iter().any(|column| column == name) {
            conn.execute(
                &format!("ALTER TABLE {} ADD COLUMN {name} {ty}", TABLE_NAME),
                [],
            )?;
        }
    }
    conn.execute(
        &format!(
            "CREATE INDEX IF NOT EXISTS {table}_completed_at_idx ON {table} (completed_at)",
            table = TABLE_NAME
        ),
        [],
    )?;
    Ok(())
}
