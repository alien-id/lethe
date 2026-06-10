use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::llm::truncate::truncate_with_ellipsis;
use crate::memory::archival::{ArchivalEntry, ArchivalError};
use crate::memory::message_metadata::MessageMetadata;
use crate::memory::messages::{MessageHistoryError, StoredMessage};
use crate::memory::notes::{NoteError, NoteSearchResult, parse_frontmatter};
use crate::memory::{MemoryStore, MemoryStoreError};

const MAX_RECALL_LINES: usize = 500;
const MAX_RECALL_CHARS: usize = 2_500 * 4;
const MAX_CONVERSATION_ENTRY_CHARS: usize = 12_000;
const MIN_SCORE_THRESHOLD: f64 = 0.3;
const SEARCH_RESULT_SKIP_TOOLS: &[&str] = &["conversation_search", "archival_search"];

const ACAUSAL_WARNING: &str = include_str!("../../config/prompts/hippocampus_acausal_warning.md");
const NOTES_HEADER: &str = include_str!("../../config/prompts/hippocampus_notes_header.md");
const ARCHIVAL_HEADER: &str = include_str!("../../config/prompts/hippocampus_archival_header.md");
const CONVERSATION_HEADER: &str =
    include_str!("../../config/prompts/hippocampus_conversation_header.md");

#[derive(Debug, Error)]
pub enum HippocampusError {
    #[error(transparent)]
    Store(#[from] MemoryStoreError),
    #[error(transparent)]
    Archival(#[from] ArchivalError),
    #[error(transparent)]
    Messages(#[from] MessageHistoryError),
    #[error(transparent)]
    Notes(#[from] NoteError),
}

pub type HippocampusResult<T> = Result<T, HippocampusError>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HippocampusConfig {
    pub enabled: bool,
    pub max_recall_lines: usize,
    pub max_recall_chars: usize,
    pub max_conversation_entry_chars: usize,
    pub exclude_recent_conversations: usize,
}

impl Default for HippocampusConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_recall_lines: MAX_RECALL_LINES,
            max_recall_chars: MAX_RECALL_CHARS,
            max_conversation_entry_chars: MAX_CONVERSATION_ENTRY_CHARS,
            exclude_recent_conversations: 5,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Hippocampus {
    config: HippocampusConfig,
}

impl Hippocampus {
    pub fn new(config: HippocampusConfig) -> Self {
        Self { config }
    }

    pub fn recall(
        &self,
        store: &MemoryStore,
        message: &str,
        recent_messages: &[StoredMessage],
    ) -> HippocampusResult<Option<String>> {
        if !self.config.enabled {
            return Ok(None);
        }

        let query = build_query(message, recent_messages);
        if query.trim().is_empty() {
            return Ok(None);
        }

        let notes = store.search_notes(&query, None, 3)?;
        let archival = store
            .search_archival(&query, 5, None)?
            .into_iter()
            .filter(|entry| entry.score >= MIN_SCORE_THRESHOLD)
            .collect::<Vec<_>>();
        let conversations = self.search_conversations(store, &query)?;

        let Some(memories) =
            self.format_memories(archival, conversations, notes, self.config.max_recall_lines)
        else {
            return Ok(None);
        };

        let recall = format!(
            "<associative_memory_recall reviewed=\"false\">\n{}\n\n{}\n</associative_memory_recall>",
            ACAUSAL_WARNING.trim(),
            memories
        );
        Ok(Some(cap_recall_payload(
            &recall,
            self.config.max_recall_chars,
        )))
    }

    fn search_conversations(
        &self,
        store: &MemoryStore,
        query: &str,
    ) -> HippocampusResult<Vec<StoredMessage>> {
        let limit = 5;
        let fetch_limit = (limit + self.config.exclude_recent_conversations) * 2;
        let results = store.search_messages(query, fetch_limit, None)?;
        let candidates = if results.len() > self.config.exclude_recent_conversations {
            results
                .into_iter()
                .skip(self.config.exclude_recent_conversations)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        let now = Utc::now();
        let mut filtered = candidates
            .into_iter()
            .filter_map(|message| self.conversation_entry_allowed(message))
            .collect::<Vec<_>>();

        for message in &mut filtered {
            let boost = parse_created_at(&message.created_at)
                .map(|created| {
                    let age_hours =
                        now.signed_duration_since(created).num_seconds().max(0) as f64 / 3600.0;
                    0.15 * (1.0 - age_hours / 168.0).max(0.0)
                })
                .unwrap_or(0.0);
            message.score += boost;
        }
        filtered.sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        filtered.truncate(limit);
        Ok(filtered)
    }

    fn conversation_entry_allowed(&self, mut message: StoredMessage) -> Option<StoredMessage> {
        let metadata = MessageMetadata::from_value(Some(&message.metadata));
        if metadata.is_internal() {
            return None;
        }

        if message.role.is_tool() {
            let tool_name = message
                .metadata
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("");
            if SEARCH_RESULT_SKIP_TOOLS.contains(&tool_name) {
                return None;
            }
            if message.content.chars().count() <= 2_000 {
                return Some(message);
            }
            return None;
        }

        if message.metadata.get("tool_call_id").is_some() {
            return None;
        }

        if message.role.is_assistant() && metadata.has_tool_calls() {
            message.content = condense_tool_calls(&message.content, &message.metadata);
            message.content = truncate_with_ellipsis(&message.content, 1_500);
            return Some(message);
        }

        if message.content.chars().count() > self.config.max_conversation_entry_chars {
            return None;
        }
        Some(message)
    }

    fn format_memories(
        &self,
        mut archival: Vec<ArchivalEntry>,
        mut conversations: Vec<StoredMessage>,
        notes: Vec<NoteSearchResult>,
        max_lines: usize,
    ) -> Option<String> {
        if archival.is_empty() && conversations.is_empty() && notes.is_empty() {
            return None;
        }

        let mut sections = Vec::new();
        let mut total_lines = 0;

        if !notes.is_empty() {
            let mut note_lines = Vec::new();
            for note in notes {
                if total_lines >= max_lines {
                    break;
                }
                let created = if note.created.is_empty() {
                    String::from("unknown-time")
                } else {
                    format_created_at(&note.created)
                };

                let entry = if let Some(completed_at) = note.completed_at.as_deref() {
                    if let Some(summary) = note.completion_summary.as_deref() {
                        format!(
                            "- **{}** [DONE {}]: {}  File: {}",
                            note.title,
                            format_created_at(completed_at),
                            summary.trim(),
                            note.file_path.display(),
                        )
                    } else {
                        let mut full_content = std::fs::read_to_string(&note.file_path)
                            .ok()
                            .map(|raw| parse_frontmatter(&raw).1)
                            .filter(|body| !body.trim().is_empty())
                            .unwrap_or_else(|| note.preview.clone());
                        if full_content.chars().count() > 2_000 {
                            full_content = format!(
                                "{}\n[...truncated, see full note]",
                                truncate_with_ellipsis(&full_content, 2_000)
                            );
                        }
                        format!(
                            "- **{}** [DONE {}] [{}] (awaiting curator summary):\n{}\n  File: {}",
                            note.title,
                            format_created_at(completed_at),
                            note.tags.join(", "),
                            full_content,
                            note.file_path.display(),
                        )
                    }
                } else {
                    let mut full_content = std::fs::read_to_string(&note.file_path)
                        .ok()
                        .map(|raw| parse_frontmatter(&raw).1)
                        .filter(|body| !body.trim().is_empty())
                        .unwrap_or_else(|| note.preview.clone());
                    if full_content.chars().count() > 2_000 {
                        full_content = format!(
                            "{}\n[...truncated, see full note]",
                            truncate_with_ellipsis(&full_content, 2_000)
                        );
                    }
                    format!(
                        "- **{}** [{}] (created {}):\n{}\n  File: {}",
                        note.title,
                        note.tags.join(", "),
                        created,
                        full_content,
                        note.file_path.display()
                    )
                };
                total_lines += entry.lines().count();
                note_lines.push(entry);
            }
            if !note_lines.is_empty() {
                sections.push(format!(
                    "{}\n{}",
                    NOTES_HEADER.trim(),
                    note_lines.join("\n")
                ));
            }
        }

        archival.sort_by(|left, right| {
            parse_created_at(&left.created_at).cmp(&parse_created_at(&right.created_at))
        });
        conversations.sort_by(|left, right| {
            parse_created_at(&left.created_at).cmp(&parse_created_at(&right.created_at))
        });

        if !archival.is_empty() && total_lines < max_lines {
            let mut archival_lines = Vec::new();
            for entry in archival {
                if total_lines >= max_lines {
                    break;
                }
                let line = if let Some(completed_at) = entry.completed_at.as_deref() {
                    if let Some(summary) = entry.completion_summary.as_deref() {
                        format!(
                            "- [{}] id={} [DONE {}]: {}",
                            format_created_at(&entry.created_at),
                            entry.id,
                            format_created_at(completed_at),
                            summary.trim(),
                        )
                    } else {
                        // Awaiting curator summary — show the full text (no
                        // mid-sentence truncation) plus the DONE marker so the
                        // model knows the thread is resolved.
                        let text = trim_entry(&entry.text, 50);
                        format!(
                            "- [{}] id={} [DONE {}] (awaiting curator summary):\n{}",
                            format_created_at(&entry.created_at),
                            entry.id,
                            format_created_at(completed_at),
                            text,
                        )
                    }
                } else {
                    let text = trim_entry(&entry.text, 50);
                    format!(
                        "- [{}] id={} {}",
                        format_created_at(&entry.created_at),
                        entry.id,
                        text
                    )
                };
                total_lines += line.lines().count();
                archival_lines.push(line);
            }
            if !archival_lines.is_empty() {
                sections.push(format!(
                    "{}\n{}\n(Use archival_get(memory_id) for the full text.)",
                    ARCHIVAL_HEADER.trim(),
                    archival_lines.join("\n")
                ));
            }
        }

        if !conversations.is_empty() && total_lines < max_lines {
            let mut conversation_lines = Vec::new();
            for message in conversations {
                if total_lines >= max_lines {
                    break;
                }
                let content = trim_entry(&message.content, 50);
                let line = format!(
                    "- [{}] id={} {}: {}",
                    format_created_at(&message.created_at),
                    message.id,
                    message.role,
                    content
                );
                total_lines += line.lines().count();
                conversation_lines.push(line);
            }
            if !conversation_lines.is_empty() {
                sections.push(format!(
                    "{}\n{}\n(Use conversation_get(message_id) for the full text.)",
                    CONVERSATION_HEADER.trim(),
                    conversation_lines.join("\n")
                ));
            }
        }

        if sections.is_empty() {
            None
        } else {
            Some(sections.join("\n\n"))
        }
    }
}

/// Char budget reserved for prior user-message context after the new message.
/// The new message itself is never truncated — losing intent would defeat the
/// purpose of building a search query from it. The prior-context budget is
/// generous enough for a few short turns but small enough to stay inside the
/// embedding model's input limit.
const PRIOR_CONTEXT_BUDGET_CHARS: usize = 800;

pub fn build_query(message: &str, recent_messages: &[StoredMessage]) -> String {
    let primary = message.trim().to_string();
    let mut prior = Vec::new();
    let mut prior_chars = 0;
    for recent in recent_messages
        .iter()
        .rev()
        .filter(|message| message.role.is_user())
        .take(5)
    {
        let content = recent.content.trim();
        if content.is_empty() {
            continue;
        }
        let candidate = if prior_chars + content.chars().count() > PRIOR_CONTEXT_BUDGET_CHARS {
            truncate_with_ellipsis(
                content,
                PRIOR_CONTEXT_BUDGET_CHARS.saturating_sub(prior_chars),
            )
        } else {
            content.to_string()
        };
        if candidate.is_empty() {
            break;
        }
        prior_chars += candidate.chars().count();
        prior.push(candidate);
        if prior_chars >= PRIOR_CONTEXT_BUDGET_CHARS {
            break;
        }
    }
    prior.reverse();
    if prior.is_empty() {
        primary
    } else if primary.is_empty() {
        prior.join(" ")
    } else {
        format!("{} {}", prior.join(" "), primary)
    }
}

fn condense_tool_calls(content: &str, metadata: &Value) -> String {
    let calls = metadata
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .take(5)
                .map(|call| {
                    let function = call.get("function").unwrap_or(&Value::Null);
                    let name = function.get("name").and_then(Value::as_str).unwrap_or("?");
                    let mut args = function
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if args.chars().count() > 300 {
                        args = truncate_with_ellipsis(&args, 300);
                    }
                    format!("{name}({args})")
                })
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_default();
    if content.trim().is_empty() {
        format!("[Called: {calls}]")
    } else {
        format!("{}\n[Called: {calls}]", content.trim())
    }
}

fn trim_entry(text: &str, max_lines: usize) -> String {
    const MAX_ENTRY_CHARS: usize = 10_000;
    let lines = text.lines().collect::<Vec<_>>();
    let mut trimmed = if lines.len() > max_lines {
        lines[..max_lines].join("\n")
    } else {
        text.to_string()
    };
    if trimmed.len() > MAX_ENTRY_CHARS {
        let first_line = lines.first().copied().unwrap_or("unknown content");
        trimmed = format!(
            "[large entry: {} lines, {} chars - {}]",
            lines.len(),
            text.len(),
            truncate_with_ellipsis(first_line, 200)
        );
    }
    trimmed
}

fn parse_created_at(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

fn format_created_at(value: &str) -> String {
    parse_created_at(value)
        .map(|time| {
            time.with_timezone(&Local)
                .format("%a %Y-%m-%d %H:%M:%S %Z")
                .to_string()
        })
        .unwrap_or_else(|| "unknown-time".to_string())
}

fn cap_recall_payload(value: &str, max_chars: usize) -> String {
    const CLOSING_TAG: &str = "</associative_memory_recall>";
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    // Reserve room for the truncation marker + closing tag so the final string
    // stays roughly within `max_chars`.
    let suffix_chars = CLOSING_TAG.chars().count() + 48;
    let body_budget = max_chars.saturating_sub(suffix_chars).max(1);
    format!(
        "{}\n[...recall truncated to {} chars]\n{CLOSING_TAG}",
        truncate_with_ellipsis(value, body_budget),
        max_chars
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;
    use crate::memory::MemoryStore;
    use crate::memory::messages::MessageRole;

    fn store() -> (tempfile::TempDir, MemoryStore) {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let db_path = tmp.path().join("data").join("lethe.db");
        let notes_dir = workspace.join("notes");
        let store = MemoryStore::open(workspace, db_path, notes_dir).unwrap();
        (tmp, store)
    }

    #[test]
    fn disabled_or_empty_recall_returns_none() {
        let (_tmp, store) = store();
        let hippo = Hippocampus::new(HippocampusConfig {
            enabled: false,
            ..Default::default()
        });
        assert!(hippo.recall(&store, "graph", &[]).unwrap().is_none());

        let hippo = Hippocampus::new(HippocampusConfig::default());
        assert!(hippo.recall(&store, "   ", &[]).unwrap().is_none());
    }

    #[test]
    fn recall_formats_notes_archival_and_conversation_sections() {
        let (_tmp, store) = store();
        store
            .notes
            .create(
                "Graph API Email",
                "Use MSAL to refresh graph tokens.",
                &["skill".to_string(), "email".to_string()],
                None,
            )
            .unwrap();
        store
            .archival
            .add("Graph token file is graph_tokens.json.", None, &[])
            .unwrap();
        for index in 0..6 {
            store
                .messages
                .add(
                    MessageRole::User,
                    &format!("graph email old message {index}"),
                    None,
                )
                .unwrap();
        }

        let hippo = Hippocampus::new(HippocampusConfig {
            exclude_recent_conversations: 1,
            ..Default::default()
        });
        let recall = hippo
            .recall(&store, "How do I read email with graph api?", &[])
            .unwrap()
            .unwrap();

        assert!(recall.contains("<associative_memory_recall reviewed=\"false\">"));
        assert!(recall.contains("**From notes"));
        assert!(recall.contains("Graph API Email"));
        assert!(recall.contains("**From long-term memory:**"));
        assert!(recall.contains("graph_tokens.json"));
        assert!(recall.contains("**From past conversations:**"));
        assert!(recall.contains("graph email old message"));
    }

    #[test]
    fn build_query_uses_recent_user_messages() {
        let messages = vec![
            StoredMessage {
                id: "1".to_string(),
                role: MessageRole::User,
                content: "previous question".to_string(),
                metadata: json!({}),
                created_at: String::new(),
                score: 0.0,
            },
            StoredMessage {
                id: "2".to_string(),
                role: MessageRole::Assistant,
                content: "answer".to_string(),
                metadata: json!({}),
                created_at: String::new(),
                score: 0.0,
            },
            StoredMessage {
                id: "3".to_string(),
                role: MessageRole::User,
                content: "follow up".to_string(),
                metadata: json!({}),
                created_at: String::new(),
                score: 0.0,
            },
        ];
        let query = build_query("new question", &messages);
        assert!(query.contains("new question"));
        assert!(query.contains("previous question"));
        assert!(query.contains("follow up"));
        assert!(!query.contains("answer"));
    }

    #[test]
    fn completed_archival_without_summary_shows_full_text_with_done_marker() {
        let (_tmp, store) = store();
        let body = "Long resolved-and-shipped narrative about the thing.";
        let id = store.archival.add(body, None, &[]).unwrap();
        let resolved = store.complete_memory(&id).unwrap();
        assert_eq!(resolved.as_deref(), Some(id.as_str()));

        let hippo = Hippocampus::new(HippocampusConfig::default());
        let recall = hippo
            .recall(&store, "resolved shipped narrative", &[])
            .unwrap()
            .unwrap();

        // Done marker present, full body still visible (curator hasn't run).
        assert!(recall.contains("[DONE"));
        assert!(recall.contains("awaiting curator summary"));
        assert!(recall.contains(body));
    }

    #[test]
    fn completed_archival_with_summary_renders_one_liner() {
        let (_tmp, store) = store();
        let body = "Long resolved-and-shipped narrative about the thing.".repeat(5);
        let id = store.archival.add(&body, None, &[]).unwrap();
        store.complete_memory(&id).unwrap();
        store
            .set_completion_summary(&id, "Shipped the thing; closed.")
            .unwrap();

        let hippo = Hippocampus::new(HippocampusConfig::default());
        let recall = hippo
            .recall(&store, "shipped narrative", &[])
            .unwrap()
            .unwrap();

        assert!(recall.contains("[DONE"));
        assert!(recall.contains("Shipped the thing; closed."));
        // The summary replaces the body in compressed form.
        assert!(!recall.contains(&body));
    }

    #[test]
    fn conversation_filter_skips_search_tools_and_condenses_assistant_calls() {
        let (_tmp, store) = store();
        store
            .messages
            .add(
                MessageRole::Assistant,
                "",
                Some(json!({
                    "tool_calls": [
                        {"id": "call-1", "function": {"name": "bash", "arguments": "{\"cmd\":\"echo graph\"}"}}
                    ]
                })),
            )
            .unwrap();
        store
            .messages
            .add(
                MessageRole::Tool,
                "recursive graph results",
                Some(json!({"name": "conversation_search"})),
            )
            .unwrap();
        store
            .messages
            .add(
                MessageRole::Tool,
                "bash graph output",
                Some(json!({"name": "bash"})),
            )
            .unwrap();

        let hippo = Hippocampus::new(HippocampusConfig {
            exclude_recent_conversations: 0,
            ..Default::default()
        });
        let recall = hippo.recall(&store, "graph", &[]).unwrap().unwrap();
        assert!(recall.contains("[Called: bash("));
        assert!(recall.contains("bash graph output"));
        assert!(!recall.contains("recursive graph results"));
    }
}
