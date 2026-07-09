//! App state mutated by `UiEvent`s and read by the renderer. Plain data —
//! no I/O, no rendering — so tests can drive it directly.

use std::collections::HashMap;
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::tui::events::UiEvent;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolStatus {
    Running,
    Done { success: bool, duration_ms: u64 },
}

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub args_preview: String,
    pub output_preview: String,
    pub status: ToolStatus,
}

#[derive(Clone, Debug)]
pub enum TranscriptItem {
    User { content: String, at: DateTime<Utc> },
    Assistant { content: String, at: DateTime<Utc> },
    Tool(ToolCall),
    Notice { content: String, at: DateTime<Utc> },
}

#[derive(Clone, Debug)]
pub struct ActorRow {
    pub id: String,
    pub name: String,
    pub state: String,
    pub task_state: String,
    pub spawned_by: String,
    pub outcome: Option<String>,
    pub goals: String,
}

#[derive(Clone, Debug)]
pub struct TodoRow {
    pub id: i64,
    pub title: String,
    pub status: String,
    pub priority: String,
    pub due_date: Option<String>,
}

/// One insight/activity ledger row as rendered in the sidebar (fetched from
/// `GET /activity`; newest first).
#[derive(Clone, Debug)]
pub struct ActivityUiRow {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub summary: String,
    pub detail: Option<String>,
    pub source_name: String,
    pub produced_at: String,
    pub seen: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Pane {
    Transcript,
    Actors,
    Activity,
    Todos,
    Editor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Idle,
    Thinking,
    Disconnected,
}

#[derive(Debug)]
pub struct AppState {
    pub transcript: Vec<TranscriptItem>,
    pub actors: Vec<ActorRow>,
    pub todos: Vec<TodoRow>,
    /// Persistent background-activity history, newest first.
    pub activity: Vec<ActivityUiRow>,
    /// Unseen-insight count behind the sidebar's green dot.
    pub unseen_insights: u64,
    /// Cursor into `activity` when the activity pane has focus.
    pub activity_selected: usize,
    /// Index into `activity` whose detail popup is open, if any.
    pub activity_detail: Option<usize>,
    pub focused_pane: Pane,
    pub status: Status,
    pub model: String,
    pub provider: String,
    pub prompt_tokens: Option<u64>,
    pub max_context: u64,
    pub status_message: Option<String>,
    pub sidebar_visible: bool,
    pub transcript_scroll: u16,
    /// Maps tool `call_id` to its position in the transcript so `tool.end`
    /// can update the right card in place.
    pub tool_index: HashMap<String, usize>,
    pub last_event_at: Option<Instant>,
    /// Animation counter (frames since process start). The driver bumps
    /// this on its idle tick; the renderer reads it for the thinking
    /// spinner.
    pub tick: u64,
    /// Index of the assistant message currently receiving streamed
    /// deltas. Cleared when a tool call or a turn-end seals the bubble.
    pub streaming_index: Option<usize>,
    /// Whether any assistant delta has streamed during the current turn.
    /// When true, the turn-final `text` event is a pure echo of the
    /// already-rendered bubbles and is dropped; only non-streaming turns
    /// (providers that send no deltas) push the text. Reset at each turn
    /// boundary.
    pub streamed_this_turn: bool,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            transcript: Vec::new(),
            actors: Vec::new(),
            todos: Vec::new(),
            activity: Vec::new(),
            unseen_insights: 0,
            activity_selected: 0,
            activity_detail: None,
            focused_pane: Pane::Editor,
            status: Status::Disconnected,
            model: String::from("(unknown)"),
            provider: String::new(),
            prompt_tokens: None,
            max_context: 128_000,
            status_message: None,
            sidebar_visible: true,
            transcript_scroll: 0,
            tool_index: HashMap::new(),
            last_event_at: None,
            tick: 0,
            streaming_index: None,
            streamed_this_turn: false,
        }
    }

    pub fn push_user(&mut self, content: String) {
        self.transcript.push(TranscriptItem::User {
            content,
            at: Utc::now(),
        });
    }

    pub fn push_assistant(&mut self, content: String) {
        if content.trim().is_empty() {
            return;
        }
        self.transcript.push(TranscriptItem::Assistant {
            content,
            at: Utc::now(),
        });
    }

    /// Closes the currently-streaming assistant bubble so the next
    /// `AssistantDelta` starts a new message. Called when a tool runs or
    /// when the turn ends.
    pub fn seal_streaming(&mut self) {
        self.streaming_index = None;
    }

    pub fn push_notice(&mut self, content: impl Into<String>) {
        self.transcript.push(TranscriptItem::Notice {
            content: content.into(),
            at: Utc::now(),
        });
    }

    pub fn apply_event(&mut self, event: UiEvent) {
        self.last_event_at = Some(Instant::now());
        match event {
            UiEvent::Connected => {
                self.status = Status::Idle;
                self.status_message = None;
            }
            UiEvent::Disconnected(message) => {
                self.status = Status::Disconnected;
                self.status_message = Some(message);
            }
            UiEvent::AssistantText(content) => {
                // If anything streamed this turn, the `---`-split bubbles
                // already render this message verbatim, so the turn-final
                // `text` event is a pure echo — seal and drop it. (Matching
                // the streamed tail against re-split segments was fragile:
                // a trailing `---` divider or provider-side normalization
                // made it miss and re-render the whole reply twice.) Only a
                // non-streaming turn — no deltas, e.g. a proactive message
                // pushed straight to `/events` — actually pushes the text.
                self.seal_streaming();
                if !self.streamed_this_turn {
                    for segment in split_into_segments(&content) {
                        self.push_assistant(segment);
                    }
                }
            }
            UiEvent::AssistantDelta(delta) => self.extend_streaming(delta),
            UiEvent::TypingStart | UiEvent::TurnStart => {
                self.status = Status::Thinking;
                self.streamed_this_turn = false;
            }
            UiEvent::TypingStop | UiEvent::TurnDone => {
                self.status = Status::Idle;
                self.seal_streaming();
                self.streamed_this_turn = false;
            }
            UiEvent::ToolStart {
                call_id,
                name,
                args_preview,
            } => {
                // A tool call always seals whatever assistant text was
                // being streamed; subsequent deltas start a new bubble.
                self.seal_streaming();
                let call = ToolCall {
                    call_id: call_id.clone(),
                    name,
                    args_preview,
                    output_preview: String::new(),
                    status: ToolStatus::Running,
                };
                self.tool_index.insert(call_id, self.transcript.len());
                self.transcript.push(TranscriptItem::Tool(call));
            }
            UiEvent::ToolEnd {
                call_id,
                success,
                output_preview,
                duration_ms,
                ..
            } => {
                if let Some(&index) = self.tool_index.get(&call_id) {
                    if let Some(TranscriptItem::Tool(call)) = self.transcript.get_mut(index) {
                        call.output_preview = output_preview;
                        call.status = ToolStatus::Done {
                            success,
                            duration_ms,
                        };
                    }
                }
            }
            UiEvent::ActorEvent { .. } => {
                // Sidebar refresh is triggered by the driver after each
                // actor event; the state itself is read via GET /actors.
            }
            UiEvent::ActivityLogged => {
                // Same pattern: the driver issues RefreshActivity; the
                // rows and unseen count arrive via GET /activity.
            }
            UiEvent::Usage { prompt_tokens } => self.prompt_tokens = Some(prompt_tokens),
            UiEvent::Reaction { .. } => {}
            UiEvent::Unknown { event, .. } => {
                tracing::debug!(event = %event, "unknown SSE event");
            }
        }
    }

    fn extend_streaming(&mut self, delta: String) {
        if delta.is_empty() {
            return;
        }
        self.streamed_this_turn = true;
        // Append the new chunk to the in-flight bubble (or start one),
        // then split off any sub-messages that are now complete (any
        // `---` divider line whose newline has already arrived).
        match self.streaming_index {
            Some(index) => {
                if let Some(TranscriptItem::Assistant { content, .. }) =
                    self.transcript.get_mut(index)
                {
                    content.push_str(&delta);
                } else {
                    self.transcript.push(TranscriptItem::Assistant {
                        content: delta,
                        at: Utc::now(),
                    });
                    self.streaming_index = Some(self.transcript.len() - 1);
                }
            }
            None => {
                self.transcript.push(TranscriptItem::Assistant {
                    content: delta,
                    at: Utc::now(),
                });
                self.streaming_index = Some(self.transcript.len() - 1);
            }
        }
        self.flush_streaming_segments();
    }

    /// If the streaming bubble's content now contains one or more
    /// complete divider lines, split off the sealed segments into their
    /// own assistant messages and leave the trailing partial in the
    /// streaming bubble.
    fn flush_streaming_segments(&mut self) {
        let Some(index) = self.streaming_index else {
            return;
        };
        let content = match self.transcript.get(index) {
            Some(TranscriptItem::Assistant { content, .. }) => content.clone(),
            _ => return,
        };
        let mut split = split_streamed_segments(&content);
        let tail = split.pop().unwrap_or_default();
        if split.is_empty() {
            return;
        }
        // First sealed segment replaces the current bubble; the rest
        // become new assistant entries; the tail becomes a fresh
        // streaming bubble for the next chunk.
        let mut iter = split.into_iter();
        let first = iter.next().unwrap_or_default();
        if let Some(TranscriptItem::Assistant {
            content: target, ..
        }) = self.transcript.get_mut(index)
        {
            *target = first;
        }
        for segment in iter {
            self.transcript.push(TranscriptItem::Assistant {
                content: segment,
                at: Utc::now(),
            });
        }
        if tail.trim().is_empty() {
            self.streaming_index = None;
        } else {
            self.transcript.push(TranscriptItem::Assistant {
                content: tail,
                at: Utc::now(),
            });
            self.streaming_index = Some(self.transcript.len() - 1);
        }
    }

    /// Take the JSON returned by `GET /session/history` (oldest →
    /// newest) and push up to `limit` user-visible chat messages onto
    /// the transcript. Skips internal heartbeat/background entries and
    /// tool messages so the seeded view shows just the conversation,
    /// not the agent's internal cogwheels.
    pub fn seed_history_from_json(&mut self, messages: Vec<Value>, limit: usize) {
        let mut chronological: Vec<TranscriptItem> = messages
            .into_iter()
            .filter_map(history_entry_to_item)
            .collect();
        if chronological.len() > limit {
            let drop = chronological.len() - limit;
            chronological.drain(..drop);
        }
        if chronological.is_empty() {
            return;
        }
        self.transcript.extend(chronological);
    }

    pub fn replace_actors(&mut self, actors: Vec<Value>) {
        self.actors = actors.into_iter().filter_map(actor_from_json).collect();
        self.actors.sort_by(|left, right| {
            let parent = left.spawned_by.is_empty().cmp(&right.spawned_by.is_empty());
            parent.reverse().then_with(|| left.name.cmp(&right.name))
        });
    }

    pub fn replace_todos(&mut self, todos: Vec<Value>) {
        self.todos = todos.into_iter().filter_map(todo_from_json).collect();
    }

    pub fn replace_activity(&mut self, activity: Vec<Value>) {
        self.activity = activity.into_iter().filter_map(activity_from_json).collect();
        if self.activity.is_empty() {
            self.activity_selected = 0;
            self.activity_detail = None;
        } else {
            self.activity_selected = self.activity_selected.min(self.activity.len() - 1);
            self.activity_detail = self
                .activity_detail
                .filter(|index| *index < self.activity.len());
        }
    }

    pub fn move_activity_selection(&mut self, down: bool) {
        if self.activity.is_empty() {
            return;
        }
        let last = self.activity.len() - 1;
        self.activity_selected = if down {
            (self.activity_selected + 1).min(last)
        } else {
            self.activity_selected.saturating_sub(1)
        };
    }

    /// Open the detail popup for the selected row. Marks it seen locally and
    /// returns its ledger id when this was its first viewing, so the caller
    /// can persist the seen-state; `None` when there is nothing to persist.
    pub fn open_activity_detail(&mut self) -> Option<i64> {
        if self.activity.is_empty() {
            return None;
        }
        let index = self.activity_selected.min(self.activity.len() - 1);
        self.activity_detail = Some(index);
        let row = &mut self.activity[index];
        if row.seen {
            return None;
        }
        row.seen = true;
        if row.kind == "insight" {
            self.unseen_insights = self.unseen_insights.saturating_sub(1);
        }
        Some(row.id)
    }

    pub fn close_activity_detail(&mut self) -> bool {
        self.activity_detail.take().is_some()
    }

    pub fn set_model(&mut self, model: impl Into<String>, provider: impl Into<String>) {
        self.model = model.into();
        self.provider = provider.into();
        self.max_context = guess_context_window(&self.model);
    }

    pub fn toggle_sidebar(&mut self) {
        self.sidebar_visible = !self.sidebar_visible;
    }

    pub fn cycle_focus(&mut self) {
        self.focused_pane = match self.focused_pane {
            Pane::Editor => Pane::Transcript,
            Pane::Transcript if self.sidebar_visible => Pane::Actors,
            Pane::Transcript => Pane::Editor,
            Pane::Actors => Pane::Activity,
            Pane::Activity => Pane::Todos,
            Pane::Todos => Pane::Editor,
        };
    }
}

fn activity_from_json(value: Value) -> Option<ActivityUiRow> {
    Some(ActivityUiRow {
        id: value.get("id")?.as_i64()?,
        kind: value
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("activity")
            .to_string(),
        title: value.get("title")?.as_str()?.to_string(),
        summary: value
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        detail: value
            .get("detail")
            .and_then(Value::as_str)
            .map(str::to_string),
        source_name: value
            .get("source_name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        produced_at: value
            .get("produced_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        seen: value
            .get("seen_at")
            .map(|seen| !seen.is_null())
            .unwrap_or(false),
    })
}

fn actor_from_json(value: Value) -> Option<ActorRow> {
    Some(ActorRow {
        id: value.get("id")?.as_str()?.to_string(),
        name: value.get("name")?.as_str()?.to_string(),
        state: value
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        task_state: value
            .get("task_state")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        spawned_by: value
            .get("spawned_by")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        outcome: value
            .get("outcome")
            .and_then(Value::as_str)
            .map(str::to_string),
        goals: value
            .get("goals")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// Convert one `/session/history` row to a user-visible transcript
/// item. Returns `None` for tool/system messages, for internal-only
/// metadata (heartbeats, DMN reflections, actor updates), and for
/// empty content.
fn history_entry_to_item(value: Value) -> Option<TranscriptItem> {
    let role = value.get("role").and_then(Value::as_str)?;
    let content = value
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if content.is_empty() {
        return None;
    }
    let metadata = value.get("metadata");
    let visibility = metadata
        .and_then(|m| m.get("lethe_visibility"))
        .and_then(Value::as_str)
        .unwrap_or("user_visible");
    if visibility == "internal" {
        return None;
    }
    let kind = metadata
        .and_then(|m| m.get("lethe_message_kind"))
        .and_then(Value::as_str)
        .unwrap_or("chat");
    if matches!(kind, "heartbeat" | "actor_update" | "background") {
        return None;
    }
    let at = value
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    match role {
        "user" => Some(TranscriptItem::User {
            content: content.to_string(),
            at,
        }),
        "assistant" => Some(TranscriptItem::Assistant {
            content: content.to_string(),
            at,
        }),
        _ => None,
    }
}

fn todo_from_json(value: Value) -> Option<TodoRow> {
    Some(TodoRow {
        id: value.get("id")?.as_i64()?,
        title: value.get("title")?.as_str()?.to_string(),
        status: value
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("planned")
            .to_string(),
        priority: value
            .get("priority")
            .and_then(Value::as_str)
            .unwrap_or("normal")
            .to_string(),
        due_date: value
            .get("due_date")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Mirrors the Telegram path's bubble splitter
/// (`telegram::formatting::telegram_message_segments`): pure `---` /
/// `-----` lines OUTSIDE fenced code blocks are submessage boundaries.
/// Each non-empty segment becomes its own transcript item — same way
/// Claude's chained sub-messages render as separate Telegram bubbles.
fn split_into_segments(text: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut in_code = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            current.push(line);
            continue;
        }
        if !in_code && is_divider_line(line) {
            push_segment(&mut segments, &mut current);
            continue;
        }
        current.push(line);
    }
    push_segment(&mut segments, &mut current);
    if segments.is_empty() && !text.trim().is_empty() {
        segments.push(text.trim().to_string());
    }
    segments
}

/// Streaming variant. Splits on complete divider lines; returns
/// `[sealed_0, ..., sealed_n, tail]` where `tail` is the in-flight bubble
/// content (possibly empty when the buffer ends on a divider).
fn split_streamed_segments(text: &str) -> Vec<String> {
    let mut segments: Vec<String> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut in_code = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            current.push(line);
            continue;
        }
        if !in_code && is_divider_line(line) {
            let joined = current.join("\n").trim().to_string();
            if !joined.is_empty() {
                segments.push(joined);
            }
            current.clear();
            continue;
        }
        current.push(line);
    }
    let tail = current.join("\n").trim_start().to_string();
    segments.push(tail);
    segments
}

fn is_divider_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.len() >= 3 && trimmed.chars().all(|c| c == '-')
}

fn push_segment(segments: &mut Vec<String>, current: &mut Vec<&str>) {
    let joined = current.join("\n").trim().to_string();
    if !joined.is_empty() {
        segments.push(joined);
    }
    current.clear();
}

/// Context window for the footer's `x/y` gauge. Capped at 128k by policy
/// (see `config/model_context_limits.json`): even when a model supports
/// more, auto-compaction keeps history below 128k, so the gauge tracks the
/// effective working window, not the model's theoretical maximum.
fn guess_context_window(_model: &str) -> u64 {
    128_000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant_texts(state: &AppState) -> Vec<String> {
        state
            .transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Assistant { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect()
    }

    fn run(events: Vec<UiEvent>) -> AppState {
        let mut state = AppState::new();
        for event in events {
            state.apply_event(event);
        }
        state
    }

    #[test]
    fn streamed_reply_with_divider_is_not_duplicated_by_final_echo() {
        // The `---`-split bubbles already rendered both parts; the
        // turn-final `text` echo must be dropped, not re-rendered.
        let body = "Part A\n\n---\n\nPart B";
        let state = run(vec![
            UiEvent::TurnStart,
            UiEvent::AssistantDelta(body.to_string()),
            UiEvent::AssistantText(body.to_string()),
            UiEvent::TypingStop,
        ]);
        assert_eq!(assistant_texts(&state), vec!["Part A", "Part B"]);
    }

    #[test]
    fn trailing_divider_does_not_re_render_whole_reply() {
        // A trailing `---` seals the stream (streaming_index -> None),
        // which used to defeat the tail match and re-push every segment,
        // showing the whole reply twice.
        let body = "Part A\n\n---\n\nPart B\n\n---";
        let state = run(vec![
            UiEvent::TurnStart,
            UiEvent::AssistantDelta(body.to_string()),
            UiEvent::AssistantText(body.to_string()),
            UiEvent::TypingStop,
        ]);
        assert_eq!(assistant_texts(&state), vec!["Part A", "Part B"]);
    }

    #[test]
    fn non_streaming_turn_pushes_the_text() {
        // No deltas (a provider that doesn't stream, or a proactive
        // message): the `text` event is the only source, so it renders.
        let state = run(vec![
            UiEvent::TurnStart,
            UiEvent::AssistantText("Hello there".to_string()),
            UiEvent::TypingStop,
        ]);
        assert_eq!(assistant_texts(&state), vec!["Hello there"]);
    }
}
