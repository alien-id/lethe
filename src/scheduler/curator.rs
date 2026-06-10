use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::llm::client::{LlmMessage, LlmRouter};
use crate::memory::archival::ArchivalError;
use crate::memory::message_metadata::MessageMetadata;
use crate::memory::messages::{MessageHistoryError, StoredMessage};
use crate::memory::{MemoryStore, MemoryStoreError};

const COMPLETION_SUMMARY_PROMPT: &str =
    include_str!("../../config/prompts/curator_summarize_completed.md");
const COMPLETION_SUMMARY_BATCH: usize = 20;
const COMPLETION_SUMMARY_MAX_CHARS: usize = 200;
/// Cadence for the per-completed-entry summariser. Callers (every
/// chat_once, every heartbeat) would otherwise fire up to
/// COMPLETION_SUMMARY_BATCH aux-LLM calls per invocation; gating here
/// caps the cost without coupling to the 6h harvest cadence.
pub const COMPLETION_SUMMARY_CADENCE_SECONDS: i64 = 60 * 60;

pub const CURATOR_CADENCE_SECONDS: i64 = 6 * 60 * 60;
const HARVEST_RECENT_LIMIT: usize = 200;
const HARVEST_CHUNK_MESSAGES: usize = 12;
const MIN_SUBSTANTIVE_CHARS: usize = 20;

#[derive(Debug, Error)]
pub enum CuratorError {
    #[error(transparent)]
    Archival(#[from] ArchivalError),
    #[error(transparent)]
    Messages(#[from] MessageHistoryError),
    #[error(transparent)]
    Store(#[from] MemoryStoreError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type CuratorResult<T> = Result<T, CuratorError>;

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CuratorState {
    pub last_run_at: Option<String>,
    pub last_harvest_at: Option<String>,
    #[serde(default)]
    pub last_summary_at: Option<String>,
    pub total_runs: usize,
    pub total_harvested: usize,
    pub total_deleted: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CuratorRunStats {
    pub skipped: bool,
    pub harvested: usize,
    pub deleted: usize,
    pub summarized: usize,
    pub total_runs: usize,
}

#[derive(Clone, Debug)]
pub struct MemoryCurator {
    state_path: PathBuf,
}

impl MemoryCurator {
    pub fn new(state_path: impl Into<PathBuf>) -> Self {
        Self {
            state_path: state_path.into(),
        }
    }

    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    pub fn load_state(&self) -> CuratorResult<CuratorState> {
        if !self.state_path.exists() {
            return Ok(CuratorState::default());
        }
        let raw = fs::read_to_string(&self.state_path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub fn should_run(&self) -> CuratorResult<bool> {
        let state = self.load_state()?;
        Ok(should_run_state(&state, Utc::now()))
    }

    pub fn run(&self, store: &MemoryStore, force: bool) -> CuratorResult<CuratorRunStats> {
        let mut state = self.load_state()?;
        if !force && !should_run_state(&state, Utc::now()) {
            return Ok(CuratorRunStats {
                skipped: true,
                total_runs: state.total_runs,
                ..Default::default()
            });
        }

        let harvested = self.harvest_recent_messages(store, &mut state)?;
        let deleted = self.dedupe_harvested_entries(store)?;
        state.last_run_at = Some(Utc::now().to_rfc3339());
        state.total_runs += 1;
        state.total_harvested += harvested;
        state.total_deleted += deleted;
        self.save_state(&state)?;

        Ok(CuratorRunStats {
            skipped: false,
            harvested,
            deleted,
            summarized: 0,
            total_runs: state.total_runs,
        })
    }

    /// Full pass: sync harvest + dedupe (cadence-gated), then summarise any
    /// completed archival entries that don't yet have a one-line summary.
    /// Summarisation runs even when the main pass is skipped — it's cheap
    /// when nothing's pending and decouples from the 6h harvest cadence.
    pub async fn run_pass(
        &self,
        store: &MemoryStore,
        router: &LlmRouter,
        force: bool,
    ) -> CuratorResult<CuratorRunStats> {
        let mut stats = self.run(store, force)?;
        stats.summarized = self.summarize_completed_entries(store, router).await?;
        Ok(stats)
    }

    /// Summarise archival entries that have been marked done but don't yet
    /// have a one-line summary. Uses the aux LLM and writes results back.
    /// Returns the number of entries successfully summarised. Safe to call
    /// alongside `run`; it has no shared state with harvest/dedupe.
    /// Cadence-gated to `COMPLETION_SUMMARY_CADENCE_SECONDS` so callers
    /// invoked on every turn/heartbeat don't spend up to BATCH aux-LLM
    /// calls per invocation.
    pub async fn summarize_completed_entries(
        &self,
        store: &MemoryStore,
        router: &LlmRouter,
    ) -> CuratorResult<usize> {
        let mut state = self.load_state()?;
        if !should_summarize_state(&state, Utc::now()) {
            return Ok(0);
        }
        let pending = store
            .archival
            .list_completed_without_summary(COMPLETION_SUMMARY_BATCH)?;
        if pending.is_empty() {
            // Touch the timestamp anyway so the cheap "list pending"
            // query is bounded to the cadence, not run on every tick.
            state.last_summary_at = Some(Utc::now().to_rfc3339());
            self.save_state(&state)?;
            return Ok(0);
        }
        let mut done = 0;
        for entry in pending {
            let prompt = COMPLETION_SUMMARY_PROMPT.replace("{text}", entry.text.trim());
            let messages = vec![LlmMessage::user(prompt)];
            match router.complete(messages, true).await {
                Ok(raw) => {
                    let summary = sanitize_summary(&raw);
                    if summary.is_empty() {
                        tracing::warn!(
                            id = %entry.id,
                            "curator summariser returned empty text; leaving entry for next pass"
                        );
                        continue;
                    }
                    store
                        .archival
                        .set_completion_summary(&entry.id, Some(&summary))?;
                    done += 1;
                }
                Err(error) => {
                    tracing::warn!(
                        id = %entry.id,
                        "curator summariser failed: {error}"
                    );
                }
            }
        }
        state.last_summary_at = Some(Utc::now().to_rfc3339());
        self.save_state(&state)?;
        Ok(done)
    }

    fn harvest_recent_messages(
        &self,
        store: &MemoryStore,
        state: &mut CuratorState,
    ) -> CuratorResult<usize> {
        let mut messages = store.messages.get_recent(HARVEST_RECENT_LIMIT)?;
        messages
            .retain(|message| is_harvestable_message(message, state.last_harvest_at.as_deref()));
        if messages.is_empty() {
            return Ok(0);
        }

        let mut harvested = 0;
        for chunk in messages.chunks(HARVEST_CHUNK_MESSAGES) {
            let episode = format_episode(chunk);
            if episode.trim().is_empty() {
                continue;
            }
            let message_ids = chunk
                .iter()
                .map(|message| message.id.clone())
                .collect::<Vec<_>>();
            store.archival.add(
                &episode,
                Some(json!({
                    "source": "rust_curator_harvest",
                    "message_ids": message_ids,
                })),
                &["conversation".to_string(), "curator".to_string()],
            )?;
            harvested += 1;
        }

        if let Some(last) = messages.last() {
            state.last_harvest_at = Some(last.created_at.clone());
        }
        Ok(harvested)
    }

    fn dedupe_harvested_entries(&self, store: &MemoryStore) -> CuratorResult<usize> {
        let entries = store.archival.list_recent(2000)?;
        let mut first_by_fingerprint: HashMap<String, String> = HashMap::new();
        let mut delete_ids = Vec::new();
        for entry in entries {
            if entry.metadata.get("source") != Some(&Value::String("rust_curator_harvest".into())) {
                continue;
            }
            let fingerprint = episode_fingerprint(&entry.text);
            if fingerprint.is_empty() {
                continue;
            }
            if first_by_fingerprint
                .insert(fingerprint, entry.id.clone())
                .is_some()
            {
                delete_ids.push(entry.id);
            }
        }

        let mut deleted = 0;
        for id in delete_ids {
            if store.archival.delete(&id)? {
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    fn save_state(&self, state: &CuratorState) -> CuratorResult<()> {
        if let Some(parent) = self.state_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.state_path, serde_json::to_string_pretty(state)?)?;
        Ok(())
    }
}

fn sanitize_summary(raw: &str) -> String {
    let mut text = raw.trim();
    // Strip a single leading "Summary:" / "summary:" if the model ignored the
    // prompt's "no preamble" rule.
    for prefix in ["Summary:", "summary:", "SUMMARY:"] {
        if let Some(rest) = text.strip_prefix(prefix) {
            text = rest.trim_start();
            break;
        }
    }
    let collapsed = text.lines().next().unwrap_or("").trim();
    if collapsed.chars().count() > COMPLETION_SUMMARY_MAX_CHARS {
        collapsed
            .chars()
            .take(COMPLETION_SUMMARY_MAX_CHARS)
            .collect::<String>()
    } else {
        collapsed.to_string()
    }
}

pub fn should_run_state(state: &CuratorState, now: DateTime<Utc>) -> bool {
    let Some(last_run_at) = &state.last_run_at else {
        return true;
    };
    let Ok(last) = DateTime::parse_from_rfc3339(last_run_at) else {
        return true;
    };
    now.signed_duration_since(last.with_timezone(&Utc))
        .num_seconds()
        >= CURATOR_CADENCE_SECONDS
}

pub fn should_summarize_state(state: &CuratorState, now: DateTime<Utc>) -> bool {
    let Some(last) = &state.last_summary_at else {
        return true;
    };
    let Ok(parsed) = DateTime::parse_from_rfc3339(last) else {
        return true;
    };
    now.signed_duration_since(parsed.with_timezone(&Utc))
        .num_seconds()
        >= COMPLETION_SUMMARY_CADENCE_SECONDS
}

fn is_harvestable_message(message: &StoredMessage, last_harvest_at: Option<&str>) -> bool {
    if !(message.role.is_user() || message.role.is_assistant()) {
        return false;
    }
    let metadata = MessageMetadata::from_value(Some(&message.metadata));
    if metadata.is_internal() {
        return false;
    }
    if message.content.trim().chars().count() < MIN_SUBSTANTIVE_CHARS {
        return false;
    }
    if metadata.has_tool_calls() {
        return false;
    }
    last_harvest_at.is_none_or(|last| message.created_at.as_str() > last)
}

fn format_episode(messages: &[StoredMessage]) -> String {
    let mut parts = vec![format!(
        "Conversation episode harvested by Rust curator from {} message(s).",
        messages.len()
    )];
    for message in messages {
        parts.push(format!(
            "[{}] {}: {}",
            message.created_at,
            message.role,
            truncate_for_episode(&message.content, 700)
        ));
    }
    parts.join("\n\n")
}

fn truncate_for_episode(content: &str, max_chars: usize) -> String {
    crate::llm::truncate::truncate_with_ellipsis(content.trim(), max_chars)
}

fn episode_fingerprint(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use tempfile::tempdir;

    use super::*;
    use crate::memory::MemoryStore;
    use crate::memory::messages::MessageRole;

    fn store() -> (tempfile::TempDir, MemoryStore) {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let db = tmp.path().join("data/lethe.db");
        let notes = workspace.join("notes");
        let store = MemoryStore::open(&workspace, db, notes).unwrap();
        (tmp, store)
    }

    #[test]
    fn cadence_uses_last_run_timestamp() {
        let now = Utc.with_ymd_and_hms(2026, 5, 22, 12, 0, 0).unwrap();
        assert!(should_run_state(&CuratorState::default(), now));
        assert!(!should_run_state(
            &CuratorState {
                last_run_at: Some((now - chrono::Duration::hours(1)).to_rfc3339()),
                ..Default::default()
            },
            now
        ));
        assert!(should_run_state(
            &CuratorState {
                last_run_at: Some((now - chrono::Duration::hours(7)).to_rfc3339()),
                ..Default::default()
            },
            now
        ));
        assert!(should_run_state(
            &CuratorState {
                last_run_at: Some("not a timestamp".to_string()),
                ..Default::default()
            },
            now
        ));
    }

    #[test]
    fn curator_harvests_substantive_messages_once() {
        let (tmp, store) = store();
        store
            .messages
            .add(
                MessageRole::User,
                "Please remember that the permit support letter is important.",
                None,
            )
            .unwrap();
        store
            .messages
            .add(MessageRole::Assistant, "ok", None)
            .unwrap();
        store
            .messages
            .add(
                MessageRole::Assistant,
                "The support letter should mention the research center and lab context.",
                None,
            )
            .unwrap();

        let curator = MemoryCurator::new(tmp.path().join("data/curator_state.json"));
        let first = curator.run(&store, true).unwrap();
        assert_eq!(first.harvested, 1);
        assert_eq!(store.archival.count().unwrap(), 1);
        let entry = store.archival.list_recent(1).unwrap().pop().unwrap();
        assert!(entry.text.contains("permit support letter"));
        assert_eq!(entry.metadata["source"], "rust_curator_harvest");

        let second = curator.run(&store, true).unwrap();
        assert_eq!(second.harvested, 0);
        assert_eq!(store.archival.count().unwrap(), 1);
        let state = curator.load_state().unwrap();
        assert_eq!(state.total_runs, 2);
        assert_eq!(state.total_harvested, 1);
    }

    #[test]
    fn curator_can_skip_when_recently_run() {
        let (tmp, store) = store();
        store
            .messages
            .add(
                MessageRole::User,
                "This message is substantive enough for a curator run.",
                None,
            )
            .unwrap();
        let curator = MemoryCurator::new(tmp.path().join("data/curator_state.json"));

        assert!(!curator.run(&store, true).unwrap().skipped);
        assert!(curator.run(&store, false).unwrap().skipped);
    }

    #[test]
    fn dedupe_removes_duplicate_harvested_entries_only() {
        let (tmp, store) = store();
        let curator = MemoryCurator::new(tmp.path().join("data/curator_state.json"));
        let text = "Conversation episode harvested by Rust curator.\n\nUSER: duplicate";
        store
            .archival
            .add(
                text,
                Some(json!({"source": "rust_curator_harvest"})),
                &["curator".to_string()],
            )
            .unwrap();
        store
            .archival
            .add(
                text,
                Some(json!({"source": "rust_curator_harvest"})),
                &["curator".to_string()],
            )
            .unwrap();
        store
            .archival
            .add(text, Some(json!({"source": "manual"})), &[])
            .unwrap();

        let stats = curator.run(&store, true).unwrap();
        assert_eq!(stats.deleted, 1);
        assert_eq!(store.archival.count().unwrap(), 2);
    }
}
