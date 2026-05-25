//! Write rows into the SQLite-vec destination. All inserts run inside
//! a single transaction (per spec §4 / §5); a `Destination::commit()`
//! either lands every row or rolls the transaction back on drop.

use std::ffi::c_char;
use std::path::Path;
use std::sync::Once;

use anyhow::{Context, Result, bail};
use rusqlite::ffi::{sqlite3, sqlite3_api_routines, sqlite3_auto_extension};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sqlite_vec::sqlite3_vec_init;

use crate::source::{ArchivalRow, MessageRow, NoteRow};

// `c_char` is `i8` on x86_64-linux and `u8` on aarch64-linux. Hard-coding
// either side breaks the other arch; `c_char` resolves to whatever the
// platform's sqlite3 headers expose.
type ExtInit = unsafe extern "C" fn(
    *mut sqlite3,
    *mut *mut c_char,
    *const sqlite3_api_routines,
) -> i32;

static REGISTER: Once = Once::new();

/// Register sqlite-vec as a SQLite auto-extension. Per the sqlite-vec
/// docs this must run **before** any `Connection::open` — every later
/// connection automatically gets `vec0`/`vec_*` available.
pub fn register_sqlite_vec() {
    REGISTER.call_once(|| unsafe {
        sqlite3_auto_extension(Some(std::mem::transmute::<*const (), ExtInit>(
            sqlite3_vec_init as *const (),
        )));
    });
}

/// One open connection with an in-progress transaction. Built by
/// `Destination::create`; consumed by `commit` (success) or dropped
/// (rollback).
pub struct Destination {
    conn: Option<Connection>,
}

impl Destination {
    /// Create (or truncate) the destination DB file and apply the schema
    /// from spec §4 with the embedding dim baked into the `vec0` tables.
    pub fn create(path: &Path, embedding_dim: usize) -> Result<Self> {
        register_sqlite_vec();
        if path.exists() {
            std::fs::remove_file(path)
                .with_context(|| format!("removing stale {}", path.display()))?;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        apply_schema(&conn, embedding_dim)?;
        // Open the implicit "all of migration is one transaction" scope.
        conn.execute_batch("BEGIN")?;
        Ok(Self { conn: Some(conn) })
    }

    fn conn(&self) -> &Connection {
        self.conn.as_ref().expect("connection consumed")
    }

    pub fn insert_archival(&self, row: &ArchivalRow) -> Result<()> {
        let metadata = sanitize_metadata(&row.metadata, &row.id, "archival");
        let tags = sanitize_tags_json(&row.tags, &row.id, "archival");
        self.conn().execute(
            "INSERT INTO memory \
                 (id, kind, title, text, metadata, tags, file_path, created_at, updated_at) \
             VALUES (?, 'archival', NULL, ?, ?, ?, NULL, ?, NULL)",
            params![row.id, row.text, metadata, tags, row.created_at],
        )?;
        self.conn().execute(
            "INSERT INTO memory_vec (id, embedding) VALUES (?, ?)",
            params![row.id, f32_slice_bytes(&row.embedding)],
        )?;
        Ok(())
    }

    pub fn insert_message(&self, row: &MessageRow) -> Result<()> {
        let metadata = sanitize_metadata(&row.metadata, &row.id, "message");
        self.conn().execute(
            "INSERT INTO message_history (id, role, content, metadata, created_at) \
             VALUES (?, ?, ?, ?, ?)",
            params![row.id, row.role, row.content, metadata, row.created_at],
        )?;
        self.conn().execute(
            "INSERT INTO message_history_vec (id, embedding) VALUES (?, ?)",
            params![row.id, f32_slice_bytes(&row.embedding)],
        )?;
        Ok(())
    }

    pub fn insert_note(&self, row: &NoteRow) -> Result<()> {
        let tags = csv_to_json_array(&row.tags_csv);
        self.conn().execute(
            "INSERT INTO memory \
                 (id, kind, title, text, metadata, tags, file_path, created_at, updated_at) \
             VALUES (?, 'note', ?, ?, '{}', ?, ?, ?, ?)",
            params![
                row.id,
                row.title,
                row.text,
                tags,
                row.file_path,
                row.created_at,
                row.updated_at,
            ],
        )?;
        self.conn().execute(
            "INSERT INTO memory_vec (id, embedding) VALUES (?, ?)",
            params![row.id, f32_slice_bytes(&row.embedding)],
        )?;
        Ok(())
    }

    /// Commit the in-flight transaction and yield the open connection
    /// back to the caller for verification reads.
    pub fn commit(mut self) -> Result<Connection> {
        let conn = self.conn.take().expect("connection already taken");
        conn.execute_batch("COMMIT")?;
        Ok(conn)
    }
}

impl Drop for Destination {
    fn drop(&mut self) {
        // If commit() was not called, force a rollback so a partial
        // batch never lands. Errors here are best-effort.
        if let Some(conn) = self.conn.take() {
            let _ = conn.execute_batch("ROLLBACK");
        }
    }
}

fn apply_schema(conn: &Connection, dim: usize) -> Result<()> {
    if dim == 0 {
        bail!("embedding dim must be > 0");
    }
    conn.execute_batch(&format!(
        "
        CREATE TABLE IF NOT EXISTS memory (
            id          TEXT PRIMARY KEY,
            kind        TEXT NOT NULL,
            title       TEXT,
            text        TEXT NOT NULL,
            metadata    TEXT NOT NULL DEFAULT '{{}}',
            tags        TEXT NOT NULL DEFAULT '[]',
            file_path   TEXT UNIQUE,
            created_at  TEXT NOT NULL,
            updated_at  TEXT
        );
        CREATE INDEX IF NOT EXISTS memory_kind_idx       ON memory (kind);
        CREATE INDEX IF NOT EXISTS memory_created_at_idx ON memory (created_at);
        CREATE VIRTUAL TABLE IF NOT EXISTS memory_vec USING vec0(
            id        TEXT PRIMARY KEY,
            embedding float[{dim}]
        );

        CREATE TABLE IF NOT EXISTS message_history (
            id          TEXT PRIMARY KEY,
            role        TEXT NOT NULL,
            content     TEXT NOT NULL,
            metadata    TEXT NOT NULL DEFAULT '{{}}',
            created_at  TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS message_history_created_at_idx ON message_history (created_at);
        CREATE INDEX IF NOT EXISTS message_history_role_idx       ON message_history (role);
        CREATE VIRTUAL TABLE IF NOT EXISTS message_history_vec USING vec0(
            id        TEXT PRIMARY KEY,
            embedding float[{dim}]
        );
        "
    ))?;
    Ok(())
}

fn sanitize_metadata(raw: &str, id: &str, kind: &str) -> String {
    match serde_json::from_str::<Value>(raw) {
        Ok(v) if v.is_object() => raw.to_string(),
        _ => {
            tracing::warn!(
                "{kind} row {id}: metadata is not a JSON object — replacing with {{}}"
            );
            "{}".to_string()
        }
    }
}

fn sanitize_tags_json(raw: &str, id: &str, kind: &str) -> String {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Array(items)) if items.iter().all(|t| t.is_string()) => raw.to_string(),
        _ => {
            tracing::warn!(
                "{kind} row {id}: tags is not a JSON array of strings — replacing with []"
            );
            "[]".to_string()
        }
    }
}

fn csv_to_json_array(csv: &str) -> String {
    let tags: Vec<&str> = csv
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    json!(tags).to_string()
}

/// Encode an `f32` slice as raw little-endian bytes for the `vec0`
/// virtual table. SQLite-vec accepts this binary layout directly.
pub fn f32_slice_bytes(values: &[f32]) -> &[u8] {
    // SAFETY: f32 has the same alignment requirements satisfied by
    // `values.as_ptr()`, and we expose exactly `size_of::<f32>() * len`
    // bytes — no out-of-bounds read.
    unsafe {
        std::slice::from_raw_parts(values.as_ptr() as *const u8, std::mem::size_of_val(values))
    }
}
