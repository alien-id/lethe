use serde_json::Value;

use crate::tools::registry::ToolRegistry;
use crate::tools::registry::args::{
    bool_arg, i64_arg, nonempty_string, optional_tags, string_arg, string_arg_default,
    string_vec_arg, usize_arg,
};
use crate::tools::spec::{
    ToolCategory, ToolDef, ToolExecutor, p_bool, p_enum, p_int, p_int_req, p_str, p_str_array,
    p_str_req,
};

const TODO_STATUS_VALUES: &[&str] = &[
    "pending",
    "in_progress",
    "completed",
    "deferred",
    "cancelled",
];
const TODO_PRIORITY_VALUES: &[&str] = &["low", "normal", "high", "urgent"];

fn exec_request_tool(_registry: &ToolRegistry<'_>, _args: &Value) -> String {
    // Handled inline by the agent tool loop before dispatch is reached.
    "Error: request_tool must be intercepted by the tool loop.".to_string()
}

fn exec_memory_read(registry: &ToolRegistry<'_>, args: &Value) -> String {
    match registry.memory.blocks.get(&string_arg(args, "label")) {
        Ok(Some(block)) => {
            serde_json::to_string_pretty(&block).unwrap_or_else(|error| format!("Error: {error}"))
        }
        Ok(None) => format!("Block '{}' not found", string_arg(args, "label")),
        Err(error) => format!("Error: {error}"),
    }
}

fn exec_memory_list(registry: &ToolRegistry<'_>, args: &Value) -> String {
    match registry
        .memory
        .blocks
        .list_blocks(bool_arg(args, "include_hidden", false))
    {
        Ok(blocks) => {
            serde_json::to_string_pretty(&blocks).unwrap_or_else(|error| format!("Error: {error}"))
        }
        Err(error) => format!("Error: {error}"),
    }
}

fn exec_memory_update(registry: &ToolRegistry<'_>, args: &Value) -> String {
    match registry.memory.blocks.update(
        &string_arg(args, "label"),
        Some(&string_arg(args, "value")),
        None,
    ) {
        Ok(true) => format!("Updated block '{}'", string_arg(args, "label")),
        Ok(false) => format!("Block '{}' not found", string_arg(args, "label")),
        Err(error) => format!("Error: {error}"),
    }
}

fn exec_memory_append(registry: &ToolRegistry<'_>, args: &Value) -> String {
    match registry
        .memory
        .blocks
        .append(&string_arg(args, "label"), &string_arg(args, "text"))
    {
        Ok(true) => format!("Appended to block '{}'", string_arg(args, "label")),
        Ok(false) => format!("Block '{}' not found", string_arg(args, "label")),
        Err(error) => format!("Error: {error}"),
    }
}

fn exec_archival_search(registry: &ToolRegistry<'_>, args: &Value) -> String {
    match registry.memory.search_archival(
        &string_arg(args, "query"),
        usize_arg(args, "limit", 10),
        None,
    ) {
        Ok(entries) => crate::memory::archival::ArchivalMemory::format_entries(&entries),
        Err(error) => format!("Error: {error}"),
    }
}

fn exec_archival_get(registry: &ToolRegistry<'_>, args: &Value) -> String {
    let id = string_arg(args, "memory_id");
    if id.trim().is_empty() {
        return "Error: memory_id is required.".to_string();
    }
    match registry.memory.archival.get(&id) {
        Ok(Some(entry)) => crate::memory::archival::ArchivalMemory::format_detail(&entry),
        Ok(None) => format!("Archival entry {id} not found."),
        Err(error) => format!("Error: {error}"),
    }
}

fn exec_archival_insert(registry: &ToolRegistry<'_>, args: &Value) -> String {
    match registry
        .memory
        .archival
        .add(&string_arg(args, "text"), None, &[])
    {
        Ok(id) => format!("Stored in archival memory (id: {id})"),
        Err(error) => format!("Error: {error}"),
    }
}

fn exec_memory_complete(registry: &ToolRegistry<'_>, args: &Value) -> String {
    let target = string_arg(args, "target");
    if target.trim().is_empty() {
        return "Error: target is required (memory id or note file path).".to_string();
    }
    match registry.memory.complete_memory(&target) {
        Ok(Some(id)) => {
            format!("Marked {id} as done. It stays searchable but appears compressed in recall.")
        }
        Ok(None) => format!("No memory found for target: {target}"),
        Err(error) => format!("Error: {error}"),
    }
}

fn exec_memory_reopen(registry: &ToolRegistry<'_>, args: &Value) -> String {
    let target = string_arg(args, "target");
    if target.trim().is_empty() {
        return "Error: target is required (memory id or note file path).".to_string();
    }
    match registry.memory.reopen_memory(&target) {
        Ok(Some(id)) => format!("Reopened {id}. It will surface in recall again."),
        Ok(None) => format!("No memory found for target: {target}"),
        Err(error) => format!("Error: {error}"),
    }
}

fn exec_conversation_search(registry: &ToolRegistry<'_>, args: &Value) -> String {
    use crate::memory::messages::MessageRole;
    let role_filter = nonempty_string(args, "role").map(|value| MessageRole::parse(&value));
    match registry.memory.search_messages(
        &string_arg(args, "query"),
        usize_arg(args, "limit", 10),
        role_filter.as_ref(),
    ) {
        Ok(messages) => crate::memory::messages::MessageHistory::format_messages(&messages),
        Err(error) => format!("Error: {error}"),
    }
}

fn exec_conversation_recent(registry: &ToolRegistry<'_>, args: &Value) -> String {
    use crate::memory::messages::MessageRole;
    let role_filter = nonempty_string(args, "role").map(|value| MessageRole::parse(&value));
    match registry
        .memory
        .recent_messages(usize_arg(args, "limit", 15), role_filter.as_ref())
    {
        Ok(messages) => crate::memory::messages::MessageHistory::format_messages(&messages),
        Err(error) => format!("Error: {error}"),
    }
}

fn exec_conversation_get(registry: &ToolRegistry<'_>, args: &Value) -> String {
    let id = string_arg(args, "message_id");
    if id.trim().is_empty() {
        return "Error: message_id is required.".to_string();
    }
    match registry.memory.messages.get(&id) {
        Ok(Some(message)) => crate::memory::messages::MessageHistory::format_detail(&message),
        Ok(None) => format!("Message {id} not found."),
        Err(error) => format!("Error: {error}"),
    }
}

fn exec_note_search(registry: &ToolRegistry<'_>, args: &Value) -> String {
    let tags = string_vec_arg(args, "tags");
    match registry.memory.search_notes(
        &string_arg(args, "query"),
        optional_tags(&tags),
        usize_arg(args, "limit", 5),
    ) {
        Ok(results) => crate::memory::notes::NoteStore::format_search(
            &string_arg(args, "query"),
            &tags,
            &results,
        ),
        Err(error) => format!("Error: {error}"),
    }
}

fn exec_note_create(registry: &ToolRegistry<'_>, args: &Value) -> String {
    let tags = string_vec_arg(args, "tags");
    match registry.memory.notes.create(
        &string_arg(args, "title"),
        &string_arg(args, "content"),
        &tags,
        None,
    ) {
        Ok(path) => format!("Note saved: {} (tags: {})", path.display(), tags.join(", ")),
        Err(error) => format!("Error: {error}"),
    }
}

fn exec_todo_create(registry: &ToolRegistry<'_>, args: &Value) -> String {
    use crate::todos::{NewTodo, TodoPriority};
    let priority =
        TodoPriority::parse(&string_arg_default(args, "priority", "normal")).unwrap_or_default();
    let parent_id = match i64_arg(args, "parent_id", 0) {
        0 => None,
        id => Some(id),
    };
    match registry.memory.todos.create(NewTodo {
        title: string_arg(args, "title"),
        description: nonempty_string(args, "description"),
        priority,
        due_date: nonempty_string(args, "due_date"),
        tags: vec![],
        source: Some("agent_tool".to_string()),
        parent_id,
    }) {
        Ok(id) => format!("Created todo #{id}"),
        Err(error) => format!("Error: {error}"),
    }
}

fn exec_todo_list(registry: &ToolRegistry<'_>, args: &Value) -> String {
    use crate::todos::{TodoFilter, TodoManager, TodoPriority, TodoStatus};
    let status = nonempty_string(args, "status").and_then(|value| TodoStatus::parse(&value));
    let priority = nonempty_string(args, "priority").and_then(|value| TodoPriority::parse(&value));
    match registry.memory.todos.list(TodoFilter {
        status,
        priority,
        include_completed: bool_arg(args, "include_completed", false),
        limit: usize_arg(args, "limit", 50),
    }) {
        Ok(todos) => TodoManager::format_list(&todos),
        Err(error) => format!("Error: {error}"),
    }
}

fn exec_todo_complete(registry: &ToolRegistry<'_>, args: &Value) -> String {
    let id = i64_arg(args, "todo_id", 0);
    match registry.memory.todos.complete(id) {
        Ok(true) => format!("Completed todo #{id}"),
        Ok(false) => format!("Todo #{id} not found"),
        Err(error) => format!("Error: {error}"),
    }
}

fn exec_todo_get(registry: &ToolRegistry<'_>, args: &Value) -> String {
    let id = i64_arg(args, "todo_id", 0);
    match registry.memory.todos.get(id) {
        Ok(Some(todo)) => crate::todos::TodoManager::format_detail(&todo),
        Ok(None) => format!("Todo #{id} not found"),
        Err(error) => format!("Error: {error}"),
    }
}

fn exec_todo_update(registry: &ToolRegistry<'_>, args: &Value) -> String {
    use crate::todos::{TodoPriority, TodoStatus, TodoUpdate};
    let status = match nonempty_string(args, "status") {
        None => None,
        Some(value) => match TodoStatus::parse(&value) {
            Some(parsed) => Some(parsed),
            None => return format!("Error: invalid todo status '{value}'"),
        },
    };
    let priority = match nonempty_string(args, "priority") {
        None => None,
        Some(value) => match TodoPriority::parse(&value) {
            Some(parsed) => Some(parsed),
            None => return format!("Error: invalid todo priority '{value}'"),
        },
    };
    let todo_id = i64_arg(args, "todo_id", 0);
    match registry.memory.todos.update(
        todo_id,
        TodoUpdate {
            title: nonempty_string(args, "title"),
            description: nonempty_string(args, "description"),
            status,
            priority,
            due_date: nonempty_string(args, "due_date"),
            parent_id: match i64_arg(args, "parent_id", 0) {
                0 => None,
                id => Some(id),
            },
        },
    ) {
        Ok(true) => format!("Updated todo #{todo_id}"),
        Ok(false) => format!("Todo #{todo_id} not found or no changes supplied"),
        Err(error) => format!("Error: {error}"),
    }
}

fn exec_todo_search(registry: &ToolRegistry<'_>, args: &Value) -> String {
    use crate::todos::TodoManager;
    match registry
        .memory
        .todos
        .search(&string_arg(args, "query"), usize_arg(args, "limit", 20))
    {
        Ok(todos) => TodoManager::format_search(&string_arg(args, "query"), &todos),
        Err(error) => format!("Error: {error}"),
    }
}

fn exec_todo_remind_check(registry: &ToolRegistry<'_>, _args: &Value) -> String {
    use crate::todos::TodoManager;
    match registry.memory.todos.due_reminders() {
        Ok(todos) => TodoManager::format_due_reminders(&todos),
        Err(error) => format!("Error: {error}"),
    }
}

fn exec_todo_reminded(registry: &ToolRegistry<'_>, args: &Value) -> String {
    let id = i64_arg(args, "todo_id", 0);
    match registry.memory.todos.mark_reminded(id) {
        Ok(true) => format!("Marked todo #{id} as reminded"),
        Ok(false) => format!("Todo #{id} not found"),
        Err(error) => format!("Error: {error}"),
    }
}

pub const TOOL_DEFS: &[ToolDef] = &[
    ToolDef {
        name: "request_tool",
        description: "Enable an extended tool for the rest of this turn.",
        params: &[p_str_req("name", "Tool name.")],
        category: ToolCategory::Initial,
        execute: ToolExecutor::Sync(exec_request_tool),
    },
    ToolDef {
        name: "memory_read",
        description: "Read a core memory block.",
        params: &[p_str_req("label", "Block label.")],
        category: ToolCategory::CortexOnly,
        execute: ToolExecutor::Sync(exec_memory_read),
    },
    ToolDef {
        name: "memory_list",
        description: "List core memory blocks.",
        params: &[p_bool("include_hidden", "Include hidden blocks.")],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_memory_list),
    },
    ToolDef {
        name: "memory_update",
        description: "Replace a core memory block.",
        params: &[
            p_str_req("label", "Block label."),
            p_str_req("value", "New value."),
        ],
        category: ToolCategory::CortexOnly,
        execute: ToolExecutor::Sync(exec_memory_update),
    },
    ToolDef {
        name: "memory_append",
        description: "Append to a core memory block.",
        params: &[
            p_str_req("label", "Block label."),
            p_str_req("text", "Text to append."),
        ],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_memory_append),
    },
    ToolDef {
        name: "archival_search",
        description: "Search long-term archival memory.",
        params: &[
            p_str_req("query", "Search query."),
            p_int("limit", "Max results."),
        ],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_archival_search),
    },
    ToolDef {
        name: "archival_insert",
        description: "Store a new archival memory.",
        params: &[p_str_req("text", "Text to store.")],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_archival_insert),
    },
    ToolDef {
        name: "archival_get",
        description: "Fetch a full archival memory by id.",
        params: &[p_str_req("memory_id", "Memory id.")],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_archival_get),
    },
    ToolDef {
        name: "memory_complete",
        description: "Mark an archival entry or note as done. It stays searchable but is rendered as a one-line marker in recall (full text via archival_get / note_search). Use when a thread is resolved.",
        params: &[p_str_req(
            "target",
            "Memory id (mem-...) or note file path.",
        )],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_memory_complete),
    },
    ToolDef {
        name: "memory_reopen",
        description: "Clear the done flag on an archival entry or note (inverse of memory_complete).",
        params: &[p_str_req(
            "target",
            "Memory id (mem-...) or note file path.",
        )],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_memory_reopen),
    },
    ToolDef {
        name: "conversation_search",
        description: "Search durable conversation history.",
        params: &[
            p_str_req("query", "Search query."),
            p_int("limit", "Max results."),
            p_str("role", "Optional role filter."),
        ],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_conversation_search),
    },
    ToolDef {
        name: "conversation_recent",
        description: "Return the last N conversation messages strictly by recency (newest last), with NO similarity ranking. Use this to recover recent context that has rolled out of the working-memory window (e.g. 'what did he just ask'), instead of conversation_search, which can bury recent turns under older high-similarity matches.",
        params: &[
            p_int("limit", "How many recent messages (default 10)."),
            p_str("role", "Optional role filter (user/assistant)."),
        ],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_conversation_recent),
    },
    ToolDef {
        name: "conversation_recent",
        description: "Fetch the most recent conversation turns in order (newest last), \
by recency only — no similarity ranking. Use this to recover 'what did they just \
ask / what were the last few turns' when working memory has rolled past it. \
Prefer over conversation_search for recency; use conversation_search for topical recall.",
        params: &[
            p_int("limit", "How many recent messages (default 15)."),
            p_str("role", "Optional role filter (user/assistant)."),
        ],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_conversation_recent),
    },
    ToolDef {
        name: "conversation_get",
        description: "Fetch a full conversation message by id.",
        params: &[p_str_req("message_id", "Message id.")],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_conversation_get),
    },
    ToolDef {
        name: "note_search",
        description: "Search notes and skills (empty query lists by tag).",
        params: &[
            p_str("query", "Search query."),
            p_str_array("tags", "Required tags."),
            p_int("limit", "Max results."),
        ],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_note_search),
    },
    ToolDef {
        name: "note_create",
        description: "Create a persistent markdown note.",
        params: &[
            p_str_req("title", "Note title."),
            p_str_req("content", "Markdown body."),
            p_str_array("tags", "Tags."),
        ],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_note_create),
    },
    ToolDef {
        name: "todo_create",
        description: "Create a persistent todo. Use parent_id to add a subtask of a larger todo.",
        params: &[
            p_str_req("title", "Title."),
            p_str("description", "Description."),
            p_enum("priority", "Priority.", TODO_PRIORITY_VALUES),
            p_str("due_date", "Due date."),
            p_int("parent_id", "Parent todo id (makes this a subtask)."),
        ],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_todo_create),
    },
    ToolDef {
        name: "todo_list",
        description: "List persistent todos.",
        params: &[
            p_enum("status", "Status filter.", TODO_STATUS_VALUES),
            p_enum("priority", "Priority filter.", TODO_PRIORITY_VALUES),
            p_bool("include_completed", "Include completed/cancelled."),
            p_int("limit", "Max results."),
        ],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_todo_list),
    },
    ToolDef {
        name: "todo_complete",
        description: "Mark a todo completed.",
        params: &[p_int_req("todo_id", "Todo id.")],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_todo_complete),
    },
    ToolDef {
        name: "todo_get",
        description: "Fetch a full todo by id.",
        params: &[p_int_req("todo_id", "Todo id.")],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_todo_get),
    },
    ToolDef {
        name: "todo_update",
        description: "Update a persistent todo.",
        params: &[
            p_int_req("todo_id", "Todo id."),
            p_str("title", "New title."),
            p_str("description", "New description."),
            p_enum("status", "New status.", TODO_STATUS_VALUES),
            p_enum("priority", "New priority.", TODO_PRIORITY_VALUES),
            p_str("due_date", "New due date."),
            p_int("parent_id", "Parent todo id (makes this a subtask)."),
        ],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_todo_update),
    },
    ToolDef {
        name: "todo_search",
        description: "Search active todos.",
        params: &[
            p_str_req("query", "Search query."),
            p_int("limit", "Max results."),
        ],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_todo_search),
    },
    ToolDef {
        name: "todo_remind_check",
        description: "Check todos due for a reminder.",
        params: &[],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_todo_remind_check),
    },
    ToolDef {
        name: "todo_reminded",
        description: "Record that the user was reminded about a todo.",
        params: &[p_int_req("todo_id", "Todo id.")],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Sync(exec_todo_reminded),
    },
];
