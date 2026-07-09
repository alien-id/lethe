//! Append-only insight/activity ledger with per-row seen-state.
//!
//! Background work in Lethe used to be fire-and-forget: insights were
//! detected, gated, delivered, and forgotten; completed subagents were
//! snapshotted by [`ActorStore`](super::ActorStore) but never re-surfaced.
//! This ledger gives that output a persistent, stateful life: every
//! noteworthy background result becomes a row that survives restarts and
//! carries a `seen_at` timestamp, NULL until the user actually views it.
//! The unseen-insight badge, the activity history view, and the
//! click-for-detail summary all read from here — never from the `actors`
//! table and never through the actor rehydration path.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{ActorError, ActorResult};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    /// A user-facing noteworthy finding from background reflection.
    Insight,
    /// A completed background task record.
    Activity,
}

impl ActivityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Insight => "insight",
            Self::Activity => "activity",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "insight" => Some(Self::Insight),
            "activity" => Some(Self::Activity),
            _ => None,
        }
    }
}

/// A ledger entry to append. `event_id` deduplicates retries: appends with
/// an already-recorded event id are silently ignored.
#[derive(Clone, Debug)]
pub struct NewActivity {
    pub kind: ActivityKind,
    pub actor_id: Option<String>,
    pub event_id: Option<String>,
    pub title: String,
    pub summary: String,
    pub detail: Option<String>,
    pub category: Option<String>,
    pub urgency: Option<String>,
    pub source_name: String,
    pub produced_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActivityRow {
    pub id: i64,
    pub kind: String,
    pub actor_id: Option<String>,
    pub title: String,
    pub summary: String,
    pub detail: Option<String>,
    pub category: Option<String>,
    pub urgency: Option<String>,
    pub source_name: String,
    pub produced_at: String,
    pub seen_at: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ActivityLog {
    db_path: PathBuf,
}

impl ActivityLog {
    pub fn open(db_path: impl Into<PathBuf>) -> ActorResult<Self> {
        let log = Self {
            db_path: db_path.into(),
        };
        log.ensure_schema()?;
        Ok(log)
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Append one entry. Returns the new row id, or `None` when the entry's
    /// `event_id` was already recorded (retry dedup). Like actor
    /// persistence, callers treat failures as best-effort: log, never block
    /// agent work.
    pub fn append(&self, entry: &NewActivity) -> ActorResult<Option<i64>> {
        let conn = self.conn()?;
        let inserted = conn
            .execute(
                "INSERT OR IGNORE INTO activity_log (
                    kind, actor_id, event_id, title, summary, detail,
                    category, urgency, source_name, produced_at, seen_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL)",
                params![
                    entry.kind.as_str(),
                    entry.actor_id,
                    entry.event_id,
                    entry.title,
                    entry.summary,
                    entry.detail,
                    entry.category,
                    entry.urgency,
                    entry.source_name,
                    entry.produced_at.to_rfc3339(),
                ],
            )
            .map_err(sql_error)?;
        if inserted == 0 {
            return Ok(None);
        }
        Ok(Some(conn.last_insert_rowid()))
    }

    /// Rows never opened by the user, restricted to `kind` when given.
    pub fn unseen_count(&self, kind: Option<ActivityKind>) -> ActorResult<i64> {
        let conn = self.conn()?;
        let count = match kind {
            Some(kind) => conn
                .query_row(
                    "SELECT COUNT(*) FROM activity_log WHERE kind = ?1 AND seen_at IS NULL",
                    params![kind.as_str()],
                    |row| row.get(0),
                )
                .map_err(sql_error)?,
            None => conn
                .query_row(
                    "SELECT COUNT(*) FROM activity_log WHERE seen_at IS NULL",
                    [],
                    |row| row.get(0),
                )
                .map_err(sql_error)?,
        };
        Ok(count)
    }

    /// Newest-first page of history. `before_id` pages past the previous
    /// page's last row id (rows are append-only, so id order is stable).
    pub fn list(&self, limit: usize, before_id: Option<i64>) -> ActorResult<Vec<ActivityRow>> {
        let conn = self.conn()?;
        let limit = limit.clamp(1, 500) as i64;
        let before = before_id.unwrap_or(i64::MAX);
        let mut statement = conn
            .prepare(
                "SELECT id, kind, actor_id, title, summary, detail, category,
                        urgency, source_name, produced_at, seen_at
                 FROM activity_log
                 WHERE id < ?1
                 ORDER BY id DESC
                 LIMIT ?2",
            )
            .map_err(sql_error)?;
        let rows = statement
            .query_map(params![before, limit], row_from_sql)
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        Ok(rows)
    }

    pub fn get(&self, id: i64) -> ActorResult<Option<ActivityRow>> {
        let conn = self.conn()?;
        conn.query_row(
            "SELECT id, kind, actor_id, title, summary, detail, category,
                    urgency, source_name, produced_at, seen_at
             FROM activity_log WHERE id = ?1",
            params![id],
            row_from_sql,
        )
        .optional()
        .map_err(sql_error)
    }

    /// Mark specific rows as seen now. Already-seen rows keep their original
    /// `seen_at` ("seen" means the first time the user viewed it). Returns
    /// how many rows transitioned from unseen to seen.
    pub fn mark_seen(&self, ids: &[i64]) -> ActorResult<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn()?;
        let now = Utc::now().to_rfc3339();
        let mut changed = 0;
        for id in ids {
            changed += conn
                .execute(
                    "UPDATE activity_log SET seen_at = ?1 WHERE id = ?2 AND seen_at IS NULL",
                    params![now, id],
                )
                .map_err(sql_error)?;
        }
        Ok(changed)
    }

    /// Mark every unseen row (of `kind`, when given) as seen now. Returns
    /// how many rows transitioned.
    pub fn mark_all_seen(&self, kind: Option<ActivityKind>) -> ActorResult<usize> {
        let conn = self.conn()?;
        let now = Utc::now().to_rfc3339();
        let changed = match kind {
            Some(kind) => conn
                .execute(
                    "UPDATE activity_log SET seen_at = ?1 WHERE kind = ?2 AND seen_at IS NULL",
                    params![now, kind.as_str()],
                )
                .map_err(sql_error)?,
            None => conn
                .execute(
                    "UPDATE activity_log SET seen_at = ?1 WHERE seen_at IS NULL",
                    params![now],
                )
                .map_err(sql_error)?,
        };
        Ok(changed)
    }

    fn ensure_schema(&self) -> ActorResult<()> {
        if let Some(parent) = self.db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| ActorError::Runtime(format!("activity log dir: {error}")))?;
        }
        let conn = self.conn()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS activity_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                kind TEXT NOT NULL,
                actor_id TEXT,
                event_id TEXT UNIQUE,
                title TEXT NOT NULL,
                summary TEXT NOT NULL,
                detail TEXT,
                category TEXT,
                urgency TEXT,
                source_name TEXT NOT NULL DEFAULT '',
                produced_at TEXT NOT NULL,
                seen_at TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_activity_log_kind_seen
                ON activity_log(kind, seen_at);
            CREATE INDEX IF NOT EXISTS idx_activity_log_produced
                ON activity_log(produced_at);",
        )
        .map_err(sql_error)?;
        Ok(())
    }

    fn conn(&self) -> ActorResult<Connection> {
        Connection::open(&self.db_path).map_err(sql_error)
    }
}

/// Collapse whitespace and cap length for the ledger's `summary` field — a
/// clean one-glance line, never a raw multi-paragraph actor result.
pub fn compact_summary(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.chars().take(max_chars).collect();
    if collapsed.chars().count() > max_chars {
        out.push('…');
    }
    out
}

fn row_from_sql(row: &rusqlite::Row<'_>) -> rusqlite::Result<ActivityRow> {
    Ok(ActivityRow {
        id: row.get("id")?,
        kind: row.get("kind")?,
        actor_id: row.get("actor_id")?,
        title: row.get("title")?,
        summary: row.get("summary")?,
        detail: row.get("detail")?,
        category: row.get("category")?,
        urgency: row.get("urgency")?,
        source_name: row.get("source_name")?,
        produced_at: row.get("produced_at")?,
        seen_at: row.get("seen_at")?,
    })
}

fn sql_error(error: rusqlite::Error) -> ActorError {
    ActorError::Runtime(format!("activity log: {error}"))
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn entry(kind: ActivityKind, title: &str, event_id: Option<&str>) -> NewActivity {
        NewActivity {
            kind,
            actor_id: Some("actor-1".to_string()),
            event_id: event_id.map(str::to_string),
            title: title.to_string(),
            summary: format!("{title} summary"),
            detail: Some(format!("{title} detail")),
            category: Some("insight".to_string()),
            urgency: Some("low".to_string()),
            source_name: "dmn".to_string(),
            produced_at: Utc::now(),
        }
    }

    #[test]
    fn append_list_and_seen_state_round_trip() {
        let tmp = tempdir().unwrap();
        let log = ActivityLog::open(tmp.path().join("ledger.db")).unwrap();

        let first = log
            .append(&entry(ActivityKind::Insight, "First insight", Some("ev-1")))
            .unwrap()
            .expect("row inserted");
        let second = log
            .append(&entry(ActivityKind::Activity, "Task done", None))
            .unwrap()
            .expect("row inserted");

        assert_eq!(log.unseen_count(None).unwrap(), 2);
        assert_eq!(log.unseen_count(Some(ActivityKind::Insight)).unwrap(), 1);

        let rows = log.list(10, None).unwrap();
        assert_eq!(rows.len(), 2);
        // Newest first.
        assert_eq!(rows[0].id, second);
        assert_eq!(rows[1].id, first);
        assert_eq!(rows[1].title, "First insight");
        assert!(rows[1].seen_at.is_none());

        assert_eq!(log.mark_seen(&[first]).unwrap(), 1);
        assert_eq!(log.unseen_count(Some(ActivityKind::Insight)).unwrap(), 0);
        assert_eq!(log.unseen_count(None).unwrap(), 1);
        // Re-marking does not rewrite the original seen_at.
        assert_eq!(log.mark_seen(&[first]).unwrap(), 0);

        let detail = log.get(first).unwrap().expect("row exists");
        assert!(detail.seen_at.is_some());
        assert_eq!(detail.summary, "First insight summary");
    }

    #[test]
    fn event_id_deduplicates_retries() {
        let tmp = tempdir().unwrap();
        let log = ActivityLog::open(tmp.path().join("ledger.db")).unwrap();

        let inserted = log
            .append(&entry(ActivityKind::Insight, "Once", Some("ev-dup")))
            .unwrap();
        assert!(inserted.is_some());
        let replay = log
            .append(&entry(ActivityKind::Insight, "Once again", Some("ev-dup")))
            .unwrap();
        assert!(replay.is_none(), "same event id must not double-log");
        assert_eq!(log.list(10, None).unwrap().len(), 1);

        // NULL event ids never collide with each other.
        assert!(
            log.append(&entry(ActivityKind::Activity, "No id 1", None))
                .unwrap()
                .is_some()
        );
        assert!(
            log.append(&entry(ActivityKind::Activity, "No id 2", None))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn mark_all_seen_clears_by_kind_and_survives_reopen() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("ledger.db");
        {
            let log = ActivityLog::open(&path).unwrap();
            log.append(&entry(ActivityKind::Insight, "I1", Some("e1")))
                .unwrap();
            log.append(&entry(ActivityKind::Insight, "I2", Some("e2")))
                .unwrap();
            log.append(&entry(ActivityKind::Activity, "A1", None)).unwrap();
            assert_eq!(
                log.mark_all_seen(Some(ActivityKind::Insight)).unwrap(),
                2
            );
        }
        // Persistence across a process restart is the whole point.
        let reopened = ActivityLog::open(&path).unwrap();
        assert_eq!(reopened.unseen_count(None).unwrap(), 1);
        assert_eq!(
            reopened.unseen_count(Some(ActivityKind::Insight)).unwrap(),
            0
        );
        assert_eq!(reopened.list(10, None).unwrap().len(), 3);

        // Paging: before_id walks backwards.
        let page = reopened.list(1, None).unwrap();
        let next = reopened.list(10, Some(page[0].id)).unwrap();
        assert_eq!(next.len(), 2);
        assert!(next.iter().all(|row| row.id < page[0].id));
    }
}
