//! End-to-end round trip: seed a synthetic legacy LanceDB store with
//! a few rows in each of the three tables, invoke the `lethe-migrate`
//! binary, then read the resulting `lethe-memory.db` and assert that
//! every row landed with its embedding intact.

use std::ffi::c_char;
use std::process::Command;
use std::sync::{Arc, Once};

use arrow_array::{
    Array, FixedSizeListArray, RecordBatch, StringArray, types::Float32Type,
};
use arrow_schema::{DataType, Field, Schema};
use rusqlite::Connection;
use rusqlite::ffi::{sqlite3, sqlite3_api_routines, sqlite3_auto_extension};
use sqlite_vec::sqlite3_vec_init;

const DIM: usize = 768;
const INIT_ID: &str = "_init_";

// See `migrator::dest::ExtInit` — same arch-dependent c_char wrinkle.
type ExtInit = unsafe extern "C" fn(
    *mut sqlite3,
    *mut *mut c_char,
    *const sqlite3_api_routines,
) -> i32;

static REGISTER: Once = Once::new();

/// Register sqlite-vec for any connection this test process opens, so
/// the `vec0` virtual tables produced by the migrator are readable.
fn register_sqlite_vec() {
    REGISTER.call_once(|| unsafe {
        sqlite3_auto_extension(Some(std::mem::transmute::<*const (), ExtInit>(
            sqlite3_vec_init as *const (),
        )));
    });
}

#[test]
fn migrates_archival_messages_and_notes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let lancedb_dir = tmp.path().join("lancedb");
    let sqlite_path = tmp.path().join("lethe-memory.db");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio rt");

    rt.block_on(seed_lancedb(&lancedb_dir));

    let bin = env!("CARGO_BIN_EXE_lethe-migrate");
    let output = Command::new(bin)
        .arg("--lancedb-dir")
        .arg(&lancedb_dir)
        .arg("--sqlite-path")
        .arg(&sqlite_path)
        .output()
        .expect("running lethe-migrate");
    assert!(
        output.status.success(),
        "migrator failed: status={:?}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(sqlite_path.exists(), "destination not produced");

    register_sqlite_vec();
    let conn = Connection::open(&sqlite_path).expect("open sqlite");

    let archival_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory WHERE kind = 'archival'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(archival_count, 2);

    let note_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory WHERE kind = 'note'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(note_count, 2);

    let msg_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM message_history", [], |row| row.get(0))
        .unwrap();
    assert_eq!(msg_count, 3);

    // Init rows MUST NOT have leaked through.
    let init_leaks: i64 = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM memory WHERE id = '_init_') \
             + (SELECT COUNT(*) FROM message_history WHERE id = '_init_')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(init_leaks, 0, "_init_ rows leaked into destination");

    // Vec table counts mirror their data tables.
    let memory_vec_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_vec", [], |row| row.get(0))
        .unwrap();
    assert_eq!(memory_vec_count, archival_count + note_count);

    let msg_vec_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM message_history_vec", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(msg_vec_count, msg_count);

    // Notes: comma-separated tag string should now be a JSON array.
    let note_tags: String = conn
        .query_row(
            "SELECT tags FROM memory WHERE id = 'note-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(note_tags, r#"["alpha","beta"]"#);

    // File-path uniqueness preserved.
    let note_path: String = conn
        .query_row(
            "SELECT file_path FROM memory WHERE id = 'note-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(note_path, "/tmp/notes/a.md");
}

async fn seed_lancedb(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    let uri = dir.display().to_string();
    let db = lancedb::connect(&uri).execute().await.unwrap();

    // ---- archival_memory ----
    let archival = archival_batch(&[
        ArchivalSeed {
            id: INIT_ID,
            text: "",
            metadata: "{}",
            tags: "[]",
            created_at: "1970-01-01T00:00:00Z",
            vector: vec![0.0; DIM],
        },
        ArchivalSeed {
            id: "mem-aaa",
            text: "first archived memory",
            metadata: r#"{"source":"test"}"#,
            tags: r#"["one","two"]"#,
            created_at: "2026-01-02T03:04:05Z",
            vector: ramp(DIM, 0.1),
        },
        ArchivalSeed {
            id: "mem-bbb",
            text: "second archived memory",
            metadata: "not-json",
            tags: "also-not-json",
            created_at: "2026-01-02T03:05:00Z",
            vector: ramp(DIM, 0.2),
        },
    ]);
    db.create_table("archival_memory", archival)
        .execute()
        .await
        .unwrap();

    // ---- message_history ----
    let messages = message_batch(&[
        MessageSeed {
            id: INIT_ID,
            role: "system",
            content: "",
            metadata: "{}",
            created_at: "1970-01-01T00:00:00Z",
            vector: vec![0.0; DIM],
        },
        MessageSeed {
            id: "msg-1",
            role: "user",
            content: "hello",
            metadata: r#"{"chat":"x"}"#,
            created_at: "2026-01-03T00:00:00Z",
            vector: ramp(DIM, 0.3),
        },
        MessageSeed {
            id: "msg-2",
            role: "assistant",
            content: "hi there",
            metadata: "{}",
            created_at: "2026-01-03T00:00:01Z",
            vector: ramp(DIM, 0.4),
        },
        MessageSeed {
            id: "msg-3",
            role: "user",
            content: "third",
            metadata: r#"{"k":1}"#,
            created_at: "2026-01-03T00:00:02Z",
            vector: ramp(DIM, 0.5),
        },
    ]);
    db.create_table("message_history", messages)
        .execute()
        .await
        .unwrap();

    // ---- notes ----
    let notes = notes_batch(&[
        NoteSeed {
            id: INIT_ID,
            title: "",
            text: "",
            tags: "",
            file_path: "",
            created_at: "1970-01-01",
            updated_at: "1970-01-01",
            vector: vec![0.0; DIM],
        },
        NoteSeed {
            id: "note-a",
            title: "Note A",
            text: "body of note a",
            tags: "alpha, beta",
            file_path: "/tmp/notes/a.md",
            created_at: "2026-01-04",
            updated_at: "2026-01-04",
            vector: ramp(DIM, 0.6),
        },
        NoteSeed {
            id: "note-b",
            title: "Note B",
            text: "body of note b",
            tags: "",
            file_path: "/tmp/notes/b.md",
            created_at: "2026-01-04",
            updated_at: "2026-01-05",
            vector: ramp(DIM, 0.7),
        },
    ]);
    db.create_table("notes", notes).execute().await.unwrap();
}

fn ramp(dim: usize, start: f32) -> Vec<f32> {
    (0..dim).map(|i| start + (i as f32) * 0.001).collect()
}

struct ArchivalSeed<'a> {
    id: &'a str,
    text: &'a str,
    metadata: &'a str,
    tags: &'a str,
    created_at: &'a str,
    vector: Vec<f32>,
}

fn archival_batch(rows: &[ArchivalSeed<'_>]) -> RecordBatch {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("text", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                DIM as i32,
            ),
            false,
        ),
        Field::new("metadata", DataType::Utf8, false),
        Field::new("tags", DataType::Utf8, false),
        Field::new("created_at", DataType::Utf8, false),
    ]));
    RecordBatch::try_new(
        schema,
        vec![
            string_arr(rows.iter().map(|r| r.id)),
            string_arr(rows.iter().map(|r| r.text)),
            vector_arr(rows.iter().map(|r| r.vector.clone())),
            string_arr(rows.iter().map(|r| r.metadata)),
            string_arr(rows.iter().map(|r| r.tags)),
            string_arr(rows.iter().map(|r| r.created_at)),
        ],
    )
    .unwrap()
}

struct MessageSeed<'a> {
    id: &'a str,
    role: &'a str,
    content: &'a str,
    metadata: &'a str,
    created_at: &'a str,
    vector: Vec<f32>,
}

fn message_batch(rows: &[MessageSeed<'_>]) -> RecordBatch {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("role", DataType::Utf8, false),
        Field::new("content", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                DIM as i32,
            ),
            false,
        ),
        Field::new("metadata", DataType::Utf8, false),
        Field::new("created_at", DataType::Utf8, false),
    ]));
    RecordBatch::try_new(
        schema,
        vec![
            string_arr(rows.iter().map(|r| r.id)),
            string_arr(rows.iter().map(|r| r.role)),
            string_arr(rows.iter().map(|r| r.content)),
            vector_arr(rows.iter().map(|r| r.vector.clone())),
            string_arr(rows.iter().map(|r| r.metadata)),
            string_arr(rows.iter().map(|r| r.created_at)),
        ],
    )
    .unwrap()
}

struct NoteSeed<'a> {
    id: &'a str,
    title: &'a str,
    text: &'a str,
    tags: &'a str,
    file_path: &'a str,
    created_at: &'a str,
    updated_at: &'a str,
    vector: Vec<f32>,
}

fn notes_batch(rows: &[NoteSeed<'_>]) -> RecordBatch {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("title", DataType::Utf8, false),
        Field::new("text", DataType::Utf8, false),
        Field::new("tags", DataType::Utf8, false),
        Field::new("file_path", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                DIM as i32,
            ),
            false,
        ),
        Field::new("created_at", DataType::Utf8, false),
        Field::new("updated_at", DataType::Utf8, false),
    ]));
    RecordBatch::try_new(
        schema,
        vec![
            string_arr(rows.iter().map(|r| r.id)),
            string_arr(rows.iter().map(|r| r.title)),
            string_arr(rows.iter().map(|r| r.text)),
            string_arr(rows.iter().map(|r| r.tags)),
            string_arr(rows.iter().map(|r| r.file_path)),
            vector_arr(rows.iter().map(|r| r.vector.clone())),
            string_arr(rows.iter().map(|r| r.created_at)),
            string_arr(rows.iter().map(|r| r.updated_at)),
        ],
    )
    .unwrap()
}

fn string_arr<'a, I: IntoIterator<Item = &'a str>>(values: I) -> Arc<dyn Array> {
    Arc::new(StringArray::from_iter_values(values))
}

fn vector_arr(vectors: impl IntoIterator<Item = Vec<f32>>) -> Arc<dyn Array> {
    let iter = vectors
        .into_iter()
        .map(|v| Some(v.into_iter().map(Some).collect::<Vec<_>>()));
    Arc::new(FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        iter, DIM as i32,
    ))
}

