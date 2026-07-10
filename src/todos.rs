use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Deferred,
    Cancelled,
}

impl TodoStatus {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            "deferred" => Some(Self::Deferred),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Deferred => "deferred",
            Self::Cancelled => "cancelled",
        }
    }

    fn active(self) -> bool {
        !matches!(self, Self::Completed | Self::Cancelled)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoPriority {
    Low,
    #[default]
    Normal,
    High,
    Urgent,
}

impl TodoPriority {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "normal" | "" => Some(Self::Normal),
            "high" => Some(Self::High),
            "urgent" => Some(Self::Urgent),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
            Self::Urgent => "urgent",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Urgent => 1,
            Self::High => 2,
            Self::Normal => 3,
            Self::Low => 4,
        }
    }

    fn remind_interval(self) -> Duration {
        match self {
            Self::Urgent => Duration::hours(1),
            Self::High => Duration::hours(4),
            Self::Normal => Duration::days(1),
            Self::Low => Duration::days(7),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Todo {
    pub id: i64,
    pub title: String,
    pub description: Option<String>,
    pub status: TodoStatus,
    pub priority: TodoPriority,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
    pub due_date: Option<String>,
    pub last_reminded_at: Option<String>,
    pub remind_count: i64,
    pub tags: Vec<String>,
    pub source: Option<String>,
    /// Parent todo for subtasks. Lets a multi-step plan live as one tree
    /// instead of disconnected flat rows.
    pub parent_id: Option<i64>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NewTodo {
    pub title: String,
    pub description: Option<String>,
    pub priority: TodoPriority,
    pub due_date: Option<String>,
    pub tags: Vec<String>,
    pub source: Option<String>,
    pub parent_id: Option<i64>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TodoFilter {
    pub status: Option<TodoStatus>,
    pub priority: Option<TodoPriority>,
    pub include_completed: bool,
    pub limit: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TodoUpdate {
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<TodoStatus>,
    pub priority: Option<TodoPriority>,
    pub due_date: Option<String>,
    pub parent_id: Option<i64>,
}

#[derive(Debug, Error)]
pub enum TodoError {
    #[error("todo title is required")]
    EmptyTitle,
    #[error("invalid todo status: {0}")]
    InvalidStatus(String),
    #[error("invalid todo priority: {0}")]
    InvalidPriority(String),
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("storage backend error: {0}")]
    Backend(String),
}

pub type TodoResult<T> = Result<T, TodoError>;

#[derive(Clone, Debug)]
pub struct TodoManager {
    db_path: PathBuf,
}

impl TodoManager {
    pub fn open(db_path: impl Into<PathBuf>) -> TodoResult<Self> {
        let manager = Self {
            db_path: db_path.into(),
        };
        manager.ensure_schema()?;
        Ok(manager)
    }

    pub fn create(&self, todo: NewTodo) -> TodoResult<i64> {
        if todo.title.trim().is_empty() {
            return Err(TodoError::EmptyTitle);
        }
        let now = now_iso();
        let tags_json = if todo.tags.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&todo.tags)?)
        };
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO todos (title, description, priority, due_date, tags, source, created_at, updated_at, parent_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                todo.title.trim(),
                empty_to_none(todo.description.as_deref()),
                todo.priority.as_str(),
                empty_to_none(todo.due_date.as_deref()),
                tags_json,
                empty_to_none(todo.source.as_deref()),
                now,
                now,
                todo.parent_id,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn list(&self, filter: TodoFilter) -> TodoResult<Vec<Todo>> {
        let mut todos = self.all()?;
        if let Some(status) = filter.status {
            todos.retain(|todo| todo.status == status);
        } else if !filter.include_completed {
            todos.retain(|todo| todo.status.active());
        }
        if let Some(priority) = filter.priority {
            todos.retain(|todo| todo.priority == priority);
        }

        todos.sort_by(compare_todos);
        let limit = if filter.limit == 0 { 50 } else { filter.limit };
        todos.truncate(limit);
        Ok(todos)
    }

    pub fn get(&self, todo_id: i64) -> TodoResult<Option<Todo>> {
        let conn = self.conn()?;
        conn.query_row(
            "SELECT * FROM todos WHERE id = ?1",
            params![todo_id],
            row_to_todo,
        )
        .optional()
        .map_err(TodoError::from)
    }

    pub fn update(&self, todo_id: i64, update: TodoUpdate) -> TodoResult<bool> {
        let Some(mut todo) = self.get(todo_id)? else {
            return Ok(false);
        };
        let mut changed = false;
        if let Some(title) = update.title {
            if title.trim().is_empty() {
                return Err(TodoError::EmptyTitle);
            }
            todo.title = title;
            changed = true;
        }
        if let Some(description) = update.description {
            todo.description = empty_to_none(Some(&description)).map(str::to_string);
            changed = true;
        }
        if let Some(status) = update.status {
            todo.status = status;
            if status == TodoStatus::Completed {
                todo.completed_at = Some(now_iso());
            }
            changed = true;
        }
        if let Some(priority) = update.priority {
            todo.priority = priority;
            changed = true;
        }
        if let Some(due_date) = update.due_date {
            todo.due_date = empty_to_none(Some(&due_date)).map(str::to_string);
            changed = true;
        }
        if let Some(parent_id) = update.parent_id {
            todo.parent_id = (parent_id > 0).then_some(parent_id);
            changed = true;
        }
        if !changed {
            return Ok(false);
        }
        todo.updated_at = now_iso();

        let conn = self.conn()?;
        let updated = conn.execute(
            "UPDATE todos
             SET title = ?1, description = ?2, status = ?3, priority = ?4, updated_at = ?5,
                 completed_at = ?6, due_date = ?7, parent_id = ?8
             WHERE id = ?9",
            params![
                todo.title,
                todo.description,
                todo.status.as_str(),
                todo.priority.as_str(),
                todo.updated_at,
                todo.completed_at,
                todo.due_date,
                todo.parent_id,
                todo_id,
            ],
        )?;
        Ok(updated > 0)
    }

    /// Active (non-completed, non-cancelled) children of a todo.
    pub fn subtasks(&self, parent_id: i64) -> TodoResult<Vec<Todo>> {
        let mut todos = self.all()?;
        todos.retain(|todo| todo.parent_id == Some(parent_id) && todo.status.active());
        todos.sort_by(compare_todos);
        Ok(todos)
    }

    pub fn complete(&self, todo_id: i64) -> TodoResult<bool> {
        self.update(
            todo_id,
            TodoUpdate {
                status: Some(TodoStatus::Completed),
                ..Default::default()
            },
        )
    }

    pub fn mark_reminded(&self, todo_id: i64) -> TodoResult<bool> {
        self.mark_reminded_at(todo_id, Utc::now())
    }

    pub fn mark_reminded_at(&self, todo_id: i64, now: DateTime<Utc>) -> TodoResult<bool> {
        let now = now.to_rfc3339();
        let conn = self.conn()?;
        let updated = conn.execute(
            "UPDATE todos
             SET last_reminded_at = ?1, remind_count = remind_count + 1, updated_at = ?2
             WHERE id = ?3",
            params![now, now, todo_id],
        )?;
        Ok(updated > 0)
    }

    pub fn due_reminders(&self) -> TodoResult<Vec<Todo>> {
        self.due_reminders_at(Utc::now())
    }

    pub fn due_reminders_at(&self, now: DateTime<Utc>) -> TodoResult<Vec<Todo>> {
        let mut due = Vec::new();
        for todo in self.list(TodoFilter {
            include_completed: false,
            limit: usize::MAX,
            ..Default::default()
        })? {
            let Some(last_reminded) = todo.last_reminded_at.as_deref() else {
                due.push(todo);
                continue;
            };
            let Ok(last) = DateTime::parse_from_rfc3339(last_reminded) else {
                due.push(todo);
                continue;
            };
            if now.signed_duration_since(last.with_timezone(&Utc))
                >= todo.priority.remind_interval()
            {
                due.push(todo);
            }
        }
        Ok(due)
    }

    /// Compact digest of work the agent is on the hook for: every
    /// `in_progress` todo plus anything past its due date. Empty string when
    /// nothing qualifies. Injected into the system prompt and the heartbeat
    /// so unfinished work stays visible without a tool call.
    pub fn open_work_digest(&self, limit: usize) -> TodoResult<String> {
        self.open_work_digest_at(Utc::now(), limit)
    }

    pub fn open_work_digest_at(&self, now: DateTime<Utc>, limit: usize) -> TodoResult<String> {
        let todos = self.list(TodoFilter {
            include_completed: false,
            limit: usize::MAX,
            ..Default::default()
        })?;
        let mut lines = Vec::new();
        for todo in &todos {
            let overdue = todo
                .due_date
                .as_deref()
                .and_then(parse_due_date)
                .is_some_and(|due| due < now);
            if todo.status != TodoStatus::InProgress && !overdue {
                continue;
            }
            let due = todo
                .due_date
                .as_deref()
                .map(|due| format!(" (due: {due})"))
                .unwrap_or_default();
            let overdue_marker = if overdue { " — OVERDUE" } else { "" };
            let subtask = todo
                .parent_id
                .map(|parent| format!(" [subtask of #{parent}]"))
                .unwrap_or_default();
            lines.push(format!(
                "- todo #{} [{}] ({}) {}{due}{overdue_marker}{subtask}",
                todo.id,
                todo.status.as_str(),
                todo.priority.as_str(),
                todo.title,
            ));
            if lines.len() >= limit.max(1) {
                break;
            }
        }
        Ok(lines.join("\n"))
    }

    pub fn search(&self, query: &str, limit: usize) -> TodoResult<Vec<Todo>> {
        let needle = query.to_ascii_lowercase();
        let mut todos = self.list(TodoFilter {
            include_completed: false,
            limit: usize::MAX,
            ..Default::default()
        })?;
        todos.retain(|todo| {
            todo.title.to_ascii_lowercase().contains(&needle)
                || todo
                    .description
                    .as_deref()
                    .unwrap_or("")
                    .to_ascii_lowercase()
                    .contains(&needle)
        });
        todos.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        todos.truncate(if limit == 0 { 20 } else { limit });
        Ok(todos)
    }

    pub fn delete(&self, todo_id: i64) -> TodoResult<bool> {
        let conn = self.conn()?;
        Ok(conn.execute("DELETE FROM todos WHERE id = ?1", params![todo_id])? > 0)
    }

    /// Render a single todo in full, including the entire description and all
    /// metadata. Used by `todo_get` so the agent can drill into a row whose
    /// description was truncated in the list view.
    pub fn format_detail(todo: &Todo) -> String {
        let mut lines = vec![format!(
            "#{} [{}] ({}) {}",
            todo.id,
            todo.status.as_str(),
            todo.priority.as_str(),
            todo.title
        )];
        if let Some(parent) = todo.parent_id {
            lines.push(format!("subtask of: #{parent}"));
        }
        if let Some(due) = todo.due_date.as_deref() {
            lines.push(format!("due: {due}"));
        }
        lines.push(format!("created_at: {}", todo.created_at));
        if !todo.updated_at.is_empty() && todo.updated_at != todo.created_at {
            lines.push(format!("updated_at: {}", todo.updated_at));
        }
        if let Some(completed) = todo.completed_at.as_deref() {
            lines.push(format!("completed_at: {completed}"));
        }
        if todo.remind_count > 0 {
            let last = todo.last_reminded_at.as_deref().unwrap_or("unknown");
            lines.push(format!(
                "reminded: {} time(s), last at {last}",
                todo.remind_count
            ));
        }
        if !todo.tags.is_empty() {
            lines.push(format!("tags: {}", todo.tags.join(", ")));
        }
        if let Some(source) = todo.source.as_deref() {
            lines.push(format!("source: {source}"));
        }
        if let Some(description) = todo.description.as_deref() {
            lines.push(String::new());
            lines.push(description.to_string());
        }
        lines.join("\n")
    }

    pub fn format_list(todos: &[Todo]) -> String {
        if todos.is_empty() {
            return "No todos found matching criteria.".to_string();
        }
        let mut lines = vec![format!("Found {} todo(s):\n", todos.len())];
        for todo in todos {
            let status_icon = match todo.status {
                TodoStatus::Pending => "[]",
                TodoStatus::InProgress => "[~]",
                TodoStatus::Completed => "[x]",
                TodoStatus::Deferred => "[pause]",
                TodoStatus::Cancelled => "[cancelled]",
            };
            let priority_icon = match todo.priority {
                TodoPriority::Urgent => "(!) ",
                TodoPriority::High => "(high) ",
                TodoPriority::Normal => "",
                TodoPriority::Low => "(low) ",
            };
            let due = todo
                .due_date
                .as_deref()
                .map(|due| format!(" (due: {due})"))
                .unwrap_or_default();
            let reminded = if todo.remind_count > 0 {
                format!(" [reminded {}x]", todo.remind_count)
            } else {
                String::new()
            };
            let subtask = todo
                .parent_id
                .map(|parent| format!(" [subtask of #{parent}]"))
                .unwrap_or_default();
            lines.push(format!(
                "{status_icon} #{} {priority_icon}{}{}{}{}",
                todo.id, todo.title, due, reminded, subtask
            ));
            if let Some(description) = todo.description.as_deref() {
                lines.push(format!(
                    "   {}",
                    crate::llm::truncate::truncate_with_ellipsis(description, 400)
                ));
            }
        }
        lines.join("\n")
    }

    pub fn format_search(query: &str, todos: &[Todo]) -> String {
        if todos.is_empty() {
            return format!("No todos matching '{query}'");
        }
        let mut lines = vec![format!("Found {} matching todo(s):", todos.len())];
        for todo in todos {
            lines.push(format!(
                "#{} [{}] {}",
                todo.id,
                todo.status.as_str(),
                todo.title
            ));
        }
        lines.join("\n")
    }

    pub fn format_due_reminders(todos: &[Todo]) -> String {
        if todos.is_empty() {
            return "No todos due for reminder.".to_string();
        }
        let mut lines = vec![format!("{} todo(s) due for reminder:", todos.len())];
        for todo in todos {
            let priority = match todo.priority {
                TodoPriority::Urgent => "(!) ",
                TodoPriority::High => "(high) ",
                TodoPriority::Normal => "",
                TodoPriority::Low => "(low) ",
            };
            lines.push(format!("#{} {priority}{}", todo.id, todo.title));
            if let Some(description) = todo.description.as_deref() {
                lines.push(format!(
                    "   {}",
                    crate::llm::truncate::truncate_with_ellipsis(description, 400)
                ));
            }
        }
        lines.join("\n")
    }

    fn ensure_schema(&self) -> TodoResult<()> {
        if let Some(parent) = self.db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = self.conn()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS todos (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                title TEXT NOT NULL,
                description TEXT,
                status TEXT DEFAULT 'pending',
                priority TEXT DEFAULT 'normal',
                created_at TEXT DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT DEFAULT CURRENT_TIMESTAMP,
                completed_at TEXT,
                due_date TEXT,
                last_reminded_at TEXT,
                remind_count INTEGER DEFAULT 0,
                tags TEXT,
                source TEXT,
                parent_id INTEGER
            );",
        )?;
        // Migration for databases created before subtasks existed.
        let has_parent_id = conn
            .prepare("SELECT 1 FROM pragma_table_info('todos') WHERE name = 'parent_id'")?
            .exists([])?;
        if !has_parent_id {
            conn.execute("ALTER TABLE todos ADD COLUMN parent_id INTEGER", [])?;
        }
        Ok(())
    }

    fn all(&self) -> TodoResult<Vec<Todo>> {
        let conn = self.conn()?;
        let mut statement = conn.prepare("SELECT * FROM todos")?;
        let rows = statement.query_map([], row_to_todo)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(TodoError::from)
    }

    fn conn(&self) -> TodoResult<Connection> {
        Ok(Connection::open(&self.db_path)?)
    }
}

fn row_to_todo(row: &rusqlite::Row<'_>) -> rusqlite::Result<Todo> {
    let status_raw: String = row.get("status")?;
    let priority_raw: String = row.get("priority")?;
    let tags_raw: Option<String> = row.get("tags")?;
    Ok(Todo {
        id: row.get("id")?,
        title: row.get("title")?,
        description: row.get("description")?,
        status: TodoStatus::parse(&status_raw).unwrap_or(TodoStatus::Pending),
        priority: TodoPriority::parse(&priority_raw).unwrap_or(TodoPriority::Normal),
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        completed_at: row.get("completed_at")?,
        due_date: row.get("due_date")?,
        last_reminded_at: row.get("last_reminded_at")?,
        remind_count: row.get("remind_count")?,
        tags: tags_raw
            .as_deref()
            .and_then(|value| serde_json::from_str(value).ok())
            .unwrap_or_default(),
        source: row.get("source")?,
        parent_id: row.get("parent_id")?,
    })
}

/// Parse a due date stored as free text. Accepts RFC3339 or a plain
/// `YYYY-MM-DD` prefix (treated as end-of-day UTC). Anything else is not
/// considered for overdue detection.
fn parse_due_date(due: &str) -> Option<DateTime<Utc>> {
    let trimmed = due.trim();
    if let Ok(parsed) = DateTime::parse_from_rfc3339(trimmed) {
        return Some(parsed.with_timezone(&Utc));
    }
    let date = chrono::NaiveDate::parse_from_str(trimmed.get(..10)?, "%Y-%m-%d").ok()?;
    Some(DateTime::from_naive_utc_and_offset(
        date.and_hms_opt(23, 59, 59)?,
        Utc,
    ))
}

fn compare_todos(left: &Todo, right: &Todo) -> Ordering {
    left.priority
        .rank()
        .cmp(&right.priority.rank())
        .then_with(|| compare_due(left.due_date.as_deref(), right.due_date.as_deref()))
        .then_with(|| right.created_at.cmp(&left.created_at))
}

fn compare_due(left: Option<&str>, right: Option<&str>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn empty_to_none(value: Option<&str>) -> Option<&str> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

fn now_iso() -> String {
    Utc::now().to_rfc3339()
}

#[allow(dead_code)]
fn _path_exists(path: &Path) -> bool {
    path.exists()
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};
    use tempfile::tempdir;

    use super::*;

    fn manager() -> TodoManager {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("todos.sqlite");
        let manager = TodoManager::open(&path).unwrap();
        std::mem::forget(tmp);
        manager
    }

    #[test]
    fn create_get_list_and_format_todos() {
        let manager = manager();
        let low = manager
            .create(NewTodo {
                title: "low task".to_string(),
                priority: TodoPriority::Low,
                ..Default::default()
            })
            .unwrap();
        let urgent = manager
            .create(NewTodo {
                title: "urgent task".to_string(),
                description: Some("details".to_string()),
                priority: TodoPriority::Urgent,
                due_date: Some("2026-05-23".to_string()),
                tags: vec!["work".to_string()],
                source: Some("test".to_string()),
                parent_id: None,
            })
            .unwrap();

        assert_eq!(manager.get(low).unwrap().unwrap().title, "low task");
        let urgent_todo = manager.get(urgent).unwrap().unwrap();
        assert_eq!(urgent_todo.tags, vec!["work"]);

        let todos = manager
            .list(TodoFilter {
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(todos[0].title, "urgent task");
        assert_eq!(todos[1].title, "low task");
        let formatted = TodoManager::format_list(&todos);
        assert!(formatted.contains("Found 2 todo"));
        assert!(formatted.contains("#"));
        assert!(formatted.contains("urgent task"));
        assert!(formatted.contains("details"));
    }

    #[test]
    fn update_complete_search_and_delete() {
        let manager = manager();
        let id = manager
            .create(NewTodo {
                title: "deploy production".to_string(),
                ..Default::default()
            })
            .unwrap();

        assert!(
            manager
                .update(
                    id,
                    TodoUpdate {
                        status: Some(TodoStatus::InProgress),
                        priority: Some(TodoPriority::High),
                        description: Some("ship it".to_string()),
                        ..Default::default()
                    },
                )
                .unwrap()
        );
        let todo = manager.get(id).unwrap().unwrap();
        assert_eq!(todo.status, TodoStatus::InProgress);
        assert_eq!(todo.priority, TodoPriority::High);

        let search = manager.search("deploy", 20).unwrap();
        assert_eq!(search.len(), 1);
        assert!(TodoManager::format_search("deploy", &search).contains("[in_progress]"));

        assert!(manager.complete(id).unwrap());
        let completed = manager.get(id).unwrap().unwrap();
        assert_eq!(completed.status, TodoStatus::Completed);
        assert!(completed.completed_at.is_some());
        assert!(
            manager
                .list(TodoFilter {
                    limit: 10,
                    ..Default::default()
                })
                .unwrap()
                .is_empty()
        );

        assert!(manager.delete(id).unwrap());
        assert!(manager.get(id).unwrap().is_none());
    }

    #[test]
    fn reminder_intervals_match_priority_policy() {
        let manager = manager();
        let urgent = manager
            .create(NewTodo {
                title: "urgent".to_string(),
                priority: TodoPriority::Urgent,
                ..Default::default()
            })
            .unwrap();
        let low = manager
            .create(NewTodo {
                title: "low".to_string(),
                priority: TodoPriority::Low,
                ..Default::default()
            })
            .unwrap();
        let now = Utc::now();
        assert_eq!(manager.due_reminders_at(now).unwrap().len(), 2);

        assert!(manager.mark_reminded_at(urgent, now).unwrap());
        assert!(manager.mark_reminded_at(low, now).unwrap());
        assert!(
            manager
                .due_reminders_at(now + Duration::minutes(30))
                .unwrap()
                .is_empty()
        );

        let due_after_two_hours = manager
            .due_reminders_at(now + Duration::hours(2))
            .unwrap()
            .into_iter()
            .map(|todo| todo.id)
            .collect::<Vec<_>>();
        assert_eq!(due_after_two_hours, vec![urgent]);

        let due_after_eight_days = manager.due_reminders_at(now + Duration::days(8)).unwrap();
        assert_eq!(due_after_eight_days.len(), 2);
        assert!(TodoManager::format_due_reminders(&due_after_eight_days).contains("urgent"));
    }

    #[test]
    fn subtasks_link_to_parent_and_round_trip() {
        let manager = manager();
        let parent = manager
            .create(NewTodo {
                title: "migrate the database".to_string(),
                priority: TodoPriority::High,
                ..Default::default()
            })
            .unwrap();
        let child = manager
            .create(NewTodo {
                title: "write schema migration".to_string(),
                parent_id: Some(parent),
                ..Default::default()
            })
            .unwrap();
        let orphan = manager
            .create(NewTodo {
                title: "unrelated".to_string(),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(manager.get(child).unwrap().unwrap().parent_id, Some(parent));
        let children = manager.subtasks(parent).unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id, child);

        // Re-parenting via update, and detail/list formatting carry the link.
        assert!(
            manager
                .update(
                    orphan,
                    TodoUpdate {
                        parent_id: Some(parent),
                        ..Default::default()
                    },
                )
                .unwrap()
        );
        assert_eq!(manager.subtasks(parent).unwrap().len(), 2);
        let detail = TodoManager::format_detail(&manager.get(child).unwrap().unwrap());
        assert!(detail.contains(&format!("subtask of: #{parent}")));
        // Completed subtasks drop out of the active subtask view.
        manager.complete(child).unwrap();
        assert_eq!(manager.subtasks(parent).unwrap().len(), 1);
    }

    #[test]
    fn pre_subtask_databases_are_migrated_in_place() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("todos.sqlite");
        // Simulate a database created before the parent_id column existed.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE todos (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                title TEXT NOT NULL,
                description TEXT,
                status TEXT DEFAULT 'pending',
                priority TEXT DEFAULT 'normal',
                created_at TEXT DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT DEFAULT CURRENT_TIMESTAMP,
                completed_at TEXT,
                due_date TEXT,
                last_reminded_at TEXT,
                remind_count INTEGER DEFAULT 0,
                tags TEXT,
                source TEXT
            );
            INSERT INTO todos (title) VALUES ('legacy row');",
        )
        .unwrap();
        drop(conn);

        let manager = TodoManager::open(&path).unwrap();
        let todos = manager.list(TodoFilter::default()).unwrap();
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].parent_id, None);
        // And the migrated schema accepts subtasks.
        let child = manager
            .create(NewTodo {
                title: "new subtask".to_string(),
                parent_id: Some(todos[0].id),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            manager.get(child).unwrap().unwrap().parent_id,
            Some(todos[0].id)
        );
    }

    #[test]
    fn open_work_digest_surfaces_in_progress_and_overdue() {
        let manager = manager();
        let in_progress = manager
            .create(NewTodo {
                title: "port the parser".to_string(),
                priority: TodoPriority::High,
                ..Default::default()
            })
            .unwrap();
        manager
            .update(
                in_progress,
                TodoUpdate {
                    status: Some(TodoStatus::InProgress),
                    ..Default::default()
                },
            )
            .unwrap();
        manager
            .create(NewTodo {
                title: "renew permit".to_string(),
                due_date: Some("2026-01-15".to_string()),
                ..Default::default()
            })
            .unwrap();
        manager
            .create(NewTodo {
                title: "future thing".to_string(),
                due_date: Some("2099-01-01".to_string()),
                ..Default::default()
            })
            .unwrap();
        manager
            .create(NewTodo {
                title: "plain pending".to_string(),
                ..Default::default()
            })
            .unwrap();

        let now = Utc.with_ymd_and_hms(2026, 6, 10, 12, 0, 0).unwrap();
        let digest = manager.open_work_digest_at(now, 10).unwrap();
        assert!(digest.contains("port the parser"));
        assert!(digest.contains("[in_progress]"));
        assert!(digest.contains("renew permit"));
        assert!(digest.contains("OVERDUE"));
        // Not yet due and not started — stays out of the digest.
        assert!(!digest.contains("future thing"));
        assert!(!digest.contains("plain pending"));

        // Empty digest when nothing is open.
        let tmp = tempdir().unwrap();
        let empty = TodoManager::open(tmp.path().join("e.sqlite")).unwrap();
        assert_eq!(empty.open_work_digest_at(now, 10).unwrap(), "");
    }

    #[test]
    fn invalid_inputs_are_rejected() {
        assert_eq!(TodoStatus::parse("bad"), None);
        assert_eq!(TodoPriority::parse("bad"), None);
        let manager = manager();
        let error = manager
            .create(NewTodo {
                title: " ".to_string(),
                ..Default::default()
            })
            .unwrap_err();
        assert!(matches!(error, TodoError::EmptyTitle));
    }
}
