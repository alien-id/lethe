//! SQLite write-through for actor state.
//!
//! The actor registry is in-memory; before this module existed, any process
//! restart (deploy, crash, self-restart — the latter being Lethe's own
//! upgrade mechanism) silently destroyed every running subagent and its
//! in-flight task. The store keeps a durable snapshot of each actor — config,
//! task state, turn count, and its last end-of-turn checkpoint — updated on
//! every registry mutation. On startup, unfinished subagents are rehydrated
//! into the new registry: same id, same goals, same task-state note, with a
//! synthetic inbox notice telling the actor it was interrupted. Combined with
//! the supervisor's wake-on-executor-install, restored actors resume work
//! automatically.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

use super::helpers::{intent_model_name, parse_model_tier, parse_task_state};
use super::{Actor, ActorConfig, ActorError, ActorResult, ActorState, TaskState};

#[derive(Clone, Debug)]
pub struct ActorStore {
    db_path: PathBuf,
}

/// An actor rehydrated from the store, plus the persisted end-of-turn
/// checkpoint (kept separate because message history itself is not
/// persisted; the registry re-injects it as a self-message on restore).
#[derive(Clone, Debug)]
pub struct RestoredActor {
    pub actor: Actor,
    pub last_response: Option<String>,
}

impl ActorStore {
    pub fn open(db_path: impl Into<PathBuf>) -> ActorResult<Self> {
        let store = Self {
            db_path: db_path.into(),
        };
        store.ensure_schema()?;
        Ok(store)
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Upsert one actor snapshot. Called after every registry mutation;
    /// failures must be handled (logged) by the caller — persistence is
    /// best-effort and never blocks agent work.
    pub fn persist(&self, actor: &Actor) -> ActorResult<()> {
        let conn = self.conn()?;
        let tools_json =
            serde_json::to_string(&actor.config.tools).unwrap_or_else(|_| "[]".to_string());
        conn.execute(
            "INSERT OR REPLACE INTO actors (
                id, name, group_name, goals, spawned_by, is_principal, state,
                task_state, task_state_note, turn_count, max_turns, max_messages,
                model, tools, persistent, background, outcome, result, last_response,
                created_at, terminated_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
            params![
                actor.id,
                actor.config.name,
                actor.config.group,
                actor.config.goals,
                actor.spawned_by,
                actor.is_principal as i64,
                actor_state_str(actor.state),
                task_state_str(actor.task_state),
                actor.task_state_note,
                actor.turn_count as i64,
                actor.config.max_turns as i64,
                actor.config.max_messages as i64,
                actor.config.model.map(intent_model_name),
                tools_json,
                actor.config.persistent as i64,
                actor.config.background as i64,
                actor.outcome.map(|outcome| outcome.as_str()),
                actor.result(),
                last_self_response_text(actor),
                actor.created_at.to_rfc3339(),
                actor.terminated_at.map(|at| at.to_rfc3339()),
                Utc::now().to_rfc3339(),
            ],
        )
        .map_err(sql_error)?;
        Ok(())
    }

    /// Load every non-terminated, non-principal actor as a rehydrated
    /// [`Actor`]: state forced to `Waiting` (whatever turn was in flight died
    /// with the old process), message history reduced to the persisted last
    /// checkpoint, empty inbox. The registry layers the restart notice and
    /// re-parenting on top.
    pub fn load_unfinished(&self) -> ActorResult<Vec<RestoredActor>> {
        let conn = self.conn()?;
        let mut statement = conn
            .prepare(
                "SELECT id, name, group_name, goals, spawned_by, task_state,
                        task_state_note, turn_count, max_turns, max_messages,
                        model, tools, persistent, background, last_response, created_at
                 FROM actors
                 WHERE state != 'terminated' AND is_principal = 0",
            )
            .map_err(sql_error)?;
        let rows = statement
            .query_map([], |row| {
                let model: Option<String> = row.get("model")?;
                let tools_json: String = row.get("tools")?;
                let created_at_raw: String = row.get("created_at")?;
                let mut config = ActorConfig::new(
                    row.get::<_, String>("name")?,
                    row.get::<_, String>("goals")?,
                )
                .in_group(row.get::<_, String>("group_name")?);
                config.model = model.as_deref().and_then(parse_model_tier);
                config.tools = serde_json::from_str(&tools_json).unwrap_or_default();
                config.max_turns = row.get::<_, i64>("max_turns")?.max(1) as usize;
                config.max_messages = row.get::<_, i64>("max_messages")?.max(1) as usize;
                config.persistent = row.get::<_, i64>("persistent")? != 0;
                config.background = row.get::<_, i64>("background")? != 0;
                let task_state_raw: String = row.get("task_state")?;
                let actor = Actor {
                    id: row.get("id")?,
                    config,
                    spawned_by: row.get("spawned_by")?,
                    is_principal: false,
                    state: ActorState::Waiting,
                    task_state: parse_task_state(&task_state_raw).unwrap_or(TaskState::Running),
                    created_at: parse_timestamp(&created_at_raw),
                    terminated_at: None,
                    outcome: None,
                    result: None,
                    messages: Vec::new(),
                    inbox: VecDeque::new(),
                    turn_count: row.get::<_, i64>("turn_count")?.max(0) as usize,
                    task_state_note: row.get("task_state_note")?,
                    task_state_updated_at: None,
                };
                Ok(RestoredActor {
                    actor,
                    last_response: row.get("last_response")?,
                })
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        Ok(rows)
    }

    fn ensure_schema(&self) -> ActorResult<()> {
        if let Some(parent) = self.db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| ActorError::Runtime(format!("actor store dir: {error}")))?;
        }
        let conn = self.conn()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS actors (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                group_name TEXT NOT NULL,
                goals TEXT NOT NULL,
                spawned_by TEXT NOT NULL DEFAULT '',
                is_principal INTEGER NOT NULL DEFAULT 0,
                state TEXT NOT NULL,
                task_state TEXT NOT NULL,
                task_state_note TEXT NOT NULL DEFAULT '',
                turn_count INTEGER NOT NULL DEFAULT 0,
                max_turns INTEGER NOT NULL DEFAULT 20,
                max_messages INTEGER NOT NULL DEFAULT 50,
                model TEXT,
                tools TEXT NOT NULL DEFAULT '[]',
                persistent INTEGER NOT NULL DEFAULT 0,
                background INTEGER NOT NULL DEFAULT 0,
                outcome TEXT,
                result TEXT,
                last_response TEXT,
                created_at TEXT NOT NULL,
                terminated_at TEXT,
                updated_at TEXT NOT NULL
            );",
        )
        .map_err(sql_error)?;
        // Migration for databases created before background actors existed.
        let has_background = conn
            .prepare("SELECT 1 FROM pragma_table_info('actors') WHERE name = 'background'")
            .map_err(sql_error)?
            .exists([])
            .map_err(sql_error)?;
        if !has_background {
            conn.execute(
                "ALTER TABLE actors ADD COLUMN background INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(sql_error)?;
        }
        Ok(())
    }

    fn conn(&self) -> ActorResult<Connection> {
        Connection::open(&self.db_path).map_err(sql_error)
    }
}

fn sql_error(error: rusqlite::Error) -> ActorError {
    ActorError::Runtime(format!("actor store: {error}"))
}

fn parse_timestamp(raw: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(raw)
        .map(|parsed| parsed.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn actor_state_str(state: ActorState) -> &'static str {
    match state {
        ActorState::Initializing => "initializing",
        ActorState::Running => "running",
        ActorState::Waiting => "waiting",
        ActorState::Terminated => "terminated",
    }
}

fn task_state_str(state: TaskState) -> &'static str {
    match state {
        TaskState::Planned => "planned",
        TaskState::Running => "running",
        TaskState::Blocked => "blocked",
        TaskState::Done => "done",
    }
}

/// The actor's last end-of-turn self-message — the checkpoint worth carrying
/// across a restart.
fn last_self_response_text(actor: &Actor) -> Option<String> {
    actor
        .messages
        .iter()
        .rev()
        .find(|message| message.sender == actor.id && message.recipient == actor.id)
        .map(|message| message.content.clone())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::super::ActorRegistry;
    use super::*;

    #[test]
    fn store_round_trips_unfinished_actors_and_skips_terminated() {
        let tmp = tempdir().unwrap();
        let store = ActorStore::open(tmp.path().join("actors.db")).unwrap();

        let mut registry = ActorRegistry::new();
        let principal = registry.spawn(
            ActorConfig::new("cortex", "Serve the user").in_group("main"),
            None,
            true,
        );
        let mut worker_config =
            ActorConfig::new("researcher", "Survey embedding crates").in_group("main");
        worker_config.max_turns = 7;
        worker_config.tools = vec!["web_search".to_string()];
        worker_config.background = true;
        let worker = registry.spawn(worker_config, Some(&principal), false);
        let finished = registry.spawn(
            ActorConfig::new("done-already", "Old job").in_group("main"),
            Some(&principal),
            false,
        );
        registry
            .terminate(&finished, super::super::Outcome::Success, "done")
            .unwrap();

        for actor_id in [&principal, &worker, &finished] {
            store.persist(registry.get(actor_id).unwrap()).unwrap();
        }

        let restored = store.load_unfinished().unwrap();
        // The principal and the terminated actor are not work to restore.
        assert_eq!(restored.len(), 1);
        let actor = &restored[0].actor;
        assert_eq!(actor.id, worker);
        assert_eq!(actor.config.name, "researcher");
        assert_eq!(actor.config.goals, "Survey embedding crates");
        assert_eq!(actor.config.max_turns, 7);
        assert_eq!(actor.config.tools, vec!["web_search".to_string()]);
        assert!(actor.config.background);
        assert_eq!(actor.state, ActorState::Waiting);

        // Upsert: persisting again with new state replaces the row.
        let mut registry2 = ActorRegistry::new();
        registry2.spawn(
            ActorConfig::new("cortex", "Serve the user").in_group("main"),
            None,
            true,
        );
        store
            .persist(registry.get(&worker).unwrap())
            .expect("re-persist is an upsert, not a duplicate insert");
    }

    #[test]
    fn opening_a_pre_background_database_adds_the_column() {
        let tmp = tempdir().unwrap();
        let db_path = tmp.path().join("actors.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE actors (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                group_name TEXT NOT NULL,
                goals TEXT NOT NULL,
                spawned_by TEXT NOT NULL DEFAULT '',
                is_principal INTEGER NOT NULL DEFAULT 0,
                state TEXT NOT NULL,
                task_state TEXT NOT NULL,
                task_state_note TEXT NOT NULL DEFAULT '',
                turn_count INTEGER NOT NULL DEFAULT 0,
                max_turns INTEGER NOT NULL DEFAULT 20,
                max_messages INTEGER NOT NULL DEFAULT 50,
                model TEXT,
                tools TEXT NOT NULL DEFAULT '[]',
                persistent INTEGER NOT NULL DEFAULT 0,
                outcome TEXT,
                result TEXT,
                last_response TEXT,
                created_at TEXT NOT NULL,
                terminated_at TEXT,
                updated_at TEXT NOT NULL
            );
            INSERT INTO actors (id, name, group_name, goals, state, task_state, created_at, updated_at)
            VALUES ('old-1', 'researcher', 'main', 'Old job', 'waiting', 'running',
                    '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');",
        )
        .unwrap();
        drop(conn);

        let store = ActorStore::open(&db_path).unwrap();
        let restored = store.load_unfinished().unwrap();
        assert_eq!(restored.len(), 1);
        assert!(!restored[0].actor.config.background);
    }
}
