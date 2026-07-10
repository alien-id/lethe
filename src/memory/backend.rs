//! Storage contracts for Lethe's memory domains.
//!
//! The local implementation remains file/SQLite backed. Hosted runtimes can
//! supply tenant-scoped implementations without replacing the agent, recall,
//! prompt, tool, or curator code that consumes these contracts.

use std::fmt::Debug;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::archival::{ArchivalEntry, ArchivalMemory, ArchivalResult};
use super::blocks::{BlockManager, MemoryBlock, MemoryResult};
use super::db::MemoryRow;
use super::messages::{MessageHistory, MessageHistoryResult, MessageRole, StoredMessage};
use super::notes::{NoteResult, NoteSearchResult, NoteStore, NoteSummary};
use super::semantic::EmbeddingEngine;
use crate::todos::{NewTodo, Todo, TodoFilter, TodoManager, TodoResult, TodoUpdate};

pub trait BlockStorage: Debug + Send + Sync {
    fn init_embedded_defaults(&self) -> MemoryResult<()>;
    fn create(
        &self,
        label: &str,
        value: &str,
        description: &str,
        limit: usize,
        read_only: bool,
        hidden: bool,
    ) -> MemoryResult<String>;
    fn get(&self, label: &str) -> MemoryResult<Option<MemoryBlock>>;
    fn update(
        &self,
        label: &str,
        value: Option<&str>,
        description: Option<&str>,
    ) -> MemoryResult<bool>;
    fn system_update(&self, label: &str, value: &str) -> MemoryResult<bool>;
    fn append(&self, label: &str, text: &str) -> MemoryResult<bool>;
    fn str_replace(&self, label: &str, old: &str, new: &str) -> MemoryResult<bool>;
    fn delete(&self, label: &str) -> MemoryResult<bool>;
    fn list_blocks(&self, include_hidden: bool) -> MemoryResult<Vec<MemoryBlock>>;
}

pub trait ArchivalStorage: Debug + Send + Sync {
    fn embedder(&self) -> &EmbeddingEngine;
    fn add(&self, text: &str, metadata: Option<Value>, tags: &[String]) -> ArchivalResult<String>;
    fn search(
        &self,
        query: &str,
        limit: usize,
        tags: Option<&[String]>,
    ) -> ArchivalResult<Vec<ArchivalEntry>>;
    fn get(&self, memory_id: &str) -> ArchivalResult<Option<ArchivalEntry>>;
    fn delete(&self, memory_id: &str) -> ArchivalResult<bool>;
    fn update_tags(&self, memory_id: &str, tags: &[String]) -> ArchivalResult<bool>;
    fn set_completed_at(&self, memory_id: &str, value: Option<&str>) -> ArchivalResult<bool>;
    fn set_completion_summary(&self, memory_id: &str, value: Option<&str>) -> ArchivalResult<bool>;
    fn list_completed_without_summary(&self, limit: usize) -> ArchivalResult<Vec<ArchivalEntry>>;
    fn count(&self) -> ArchivalResult<usize>;
    fn list_recent(&self, limit: usize) -> ArchivalResult<Vec<ArchivalEntry>>;
    fn all_entries(&self) -> ArchivalResult<Vec<ArchivalEntry>>;
}

pub trait MessageStorage: Debug + Send + Sync {
    fn add(
        &self,
        role: MessageRole,
        content: &str,
        metadata: Option<Value>,
    ) -> MessageHistoryResult<String>;
    fn get(&self, message_id: &str) -> MessageHistoryResult<Option<StoredMessage>>;
    fn get_recent(&self, limit: usize) -> MessageHistoryResult<Vec<StoredMessage>>;
    fn search(
        &self,
        query: &str,
        limit: usize,
        role: Option<&MessageRole>,
    ) -> MessageHistoryResult<Vec<StoredMessage>>;
    fn search_by_role(
        &self,
        query: &str,
        role: &MessageRole,
        limit: usize,
    ) -> MessageHistoryResult<Vec<StoredMessage>>;
    fn get_by_role(
        &self,
        role: &MessageRole,
        limit: usize,
    ) -> MessageHistoryResult<Vec<StoredMessage>>;
    fn all_messages(&self) -> MessageHistoryResult<Vec<StoredMessage>>;
    fn delete(&self, message_id: &str) -> MessageHistoryResult<bool>;
    fn cleanup_search_results(&self, tool_names: Option<&[String]>) -> MessageHistoryResult<usize>;
    fn count(&self) -> MessageHistoryResult<usize>;
    fn clear(&self) -> MessageHistoryResult<usize>;
    fn get_context_window(
        &self,
        max_messages: usize,
        max_chars: usize,
    ) -> MessageHistoryResult<Vec<StoredMessage>>;
}

pub trait NoteStorage: Debug + Send + Sync {
    fn create(
        &self,
        title: &str,
        content: &str,
        tags: &[String],
        subdir: Option<&str>,
    ) -> NoteResult<PathBuf>;
    fn list_notes(&self, tags: Option<&[String]>) -> NoteResult<Vec<NoteSummary>>;
    fn search(
        &self,
        query: &str,
        tags: Option<&[String]>,
        limit: usize,
    ) -> NoteResult<Vec<NoteSearchResult>>;
    fn find_row_by_path(&self, path: &Path) -> NoteResult<Option<MemoryRow>>;
    fn all_tags(&self) -> NoteResult<Vec<String>>;
    fn reindex(&self) -> NoteResult<usize>;
}

pub trait TodoStorage: Debug + Send + Sync {
    fn create(&self, todo: NewTodo) -> TodoResult<i64>;
    fn list(&self, filter: TodoFilter) -> TodoResult<Vec<Todo>>;
    fn get(&self, todo_id: i64) -> TodoResult<Option<Todo>>;
    fn update(&self, todo_id: i64, update: TodoUpdate) -> TodoResult<bool>;
    fn subtasks(&self, parent_id: i64) -> TodoResult<Vec<Todo>>;
    fn complete(&self, todo_id: i64) -> TodoResult<bool>;
    fn mark_reminded(&self, todo_id: i64) -> TodoResult<bool>;
    fn mark_reminded_at(&self, todo_id: i64, now: DateTime<Utc>) -> TodoResult<bool>;
    fn due_reminders(&self) -> TodoResult<Vec<Todo>>;
    fn due_reminders_at(&self, now: DateTime<Utc>) -> TodoResult<Vec<Todo>>;
    fn open_work_digest(&self, limit: usize) -> TodoResult<String>;
    fn open_work_digest_at(&self, now: DateTime<Utc>, limit: usize) -> TodoResult<String>;
    fn search(&self, query: &str, limit: usize) -> TodoResult<Vec<Todo>>;
    fn delete(&self, todo_id: i64) -> TodoResult<bool>;
}

impl BlockStorage for BlockManager {
    fn init_embedded_defaults(&self) -> MemoryResult<()> {
        BlockManager::init_embedded_defaults(self)
    }
    fn create(
        &self,
        label: &str,
        value: &str,
        description: &str,
        limit: usize,
        read_only: bool,
        hidden: bool,
    ) -> MemoryResult<String> {
        BlockManager::create(self, label, value, description, limit, read_only, hidden)
    }
    fn get(&self, label: &str) -> MemoryResult<Option<MemoryBlock>> {
        BlockManager::get(self, label)
    }
    fn update(
        &self,
        label: &str,
        value: Option<&str>,
        description: Option<&str>,
    ) -> MemoryResult<bool> {
        BlockManager::update(self, label, value, description)
    }
    fn system_update(&self, label: &str, value: &str) -> MemoryResult<bool> {
        BlockManager::system_update(self, label, value)
    }
    fn append(&self, label: &str, text: &str) -> MemoryResult<bool> {
        BlockManager::append(self, label, text)
    }
    fn str_replace(&self, label: &str, old: &str, new: &str) -> MemoryResult<bool> {
        BlockManager::str_replace(self, label, old, new)
    }
    fn delete(&self, label: &str) -> MemoryResult<bool> {
        BlockManager::delete(self, label)
    }
    fn list_blocks(&self, include_hidden: bool) -> MemoryResult<Vec<MemoryBlock>> {
        BlockManager::list_blocks(self, include_hidden)
    }
}

impl ArchivalStorage for ArchivalMemory {
    fn embedder(&self) -> &EmbeddingEngine {
        ArchivalMemory::embedder(self)
    }
    fn add(&self, text: &str, metadata: Option<Value>, tags: &[String]) -> ArchivalResult<String> {
        ArchivalMemory::add(self, text, metadata, tags)
    }
    fn search(
        &self,
        query: &str,
        limit: usize,
        tags: Option<&[String]>,
    ) -> ArchivalResult<Vec<ArchivalEntry>> {
        ArchivalMemory::search(self, query, limit, tags)
    }
    fn get(&self, memory_id: &str) -> ArchivalResult<Option<ArchivalEntry>> {
        ArchivalMemory::get(self, memory_id)
    }
    fn delete(&self, memory_id: &str) -> ArchivalResult<bool> {
        ArchivalMemory::delete(self, memory_id)
    }
    fn update_tags(&self, memory_id: &str, tags: &[String]) -> ArchivalResult<bool> {
        ArchivalMemory::update_tags(self, memory_id, tags)
    }
    fn set_completed_at(&self, memory_id: &str, value: Option<&str>) -> ArchivalResult<bool> {
        ArchivalMemory::set_completed_at(self, memory_id, value)
    }
    fn set_completion_summary(&self, memory_id: &str, value: Option<&str>) -> ArchivalResult<bool> {
        ArchivalMemory::set_completion_summary(self, memory_id, value)
    }
    fn list_completed_without_summary(&self, limit: usize) -> ArchivalResult<Vec<ArchivalEntry>> {
        ArchivalMemory::list_completed_without_summary(self, limit)
    }
    fn count(&self) -> ArchivalResult<usize> {
        ArchivalMemory::count(self)
    }
    fn list_recent(&self, limit: usize) -> ArchivalResult<Vec<ArchivalEntry>> {
        ArchivalMemory::list_recent(self, limit)
    }
    fn all_entries(&self) -> ArchivalResult<Vec<ArchivalEntry>> {
        ArchivalMemory::all_entries(self)
    }
}

impl MessageStorage for MessageHistory {
    fn add(
        &self,
        role: MessageRole,
        content: &str,
        metadata: Option<Value>,
    ) -> MessageHistoryResult<String> {
        MessageHistory::add(self, role, content, metadata)
    }
    fn get(&self, message_id: &str) -> MessageHistoryResult<Option<StoredMessage>> {
        MessageHistory::get(self, message_id)
    }
    fn get_recent(&self, limit: usize) -> MessageHistoryResult<Vec<StoredMessage>> {
        MessageHistory::get_recent(self, limit)
    }
    fn search(
        &self,
        query: &str,
        limit: usize,
        role: Option<&MessageRole>,
    ) -> MessageHistoryResult<Vec<StoredMessage>> {
        MessageHistory::search(self, query, limit, role)
    }
    fn search_by_role(
        &self,
        query: &str,
        role: &MessageRole,
        limit: usize,
    ) -> MessageHistoryResult<Vec<StoredMessage>> {
        MessageHistory::search_by_role(self, query, role, limit)
    }
    fn get_by_role(
        &self,
        role: &MessageRole,
        limit: usize,
    ) -> MessageHistoryResult<Vec<StoredMessage>> {
        MessageHistory::get_by_role(self, role, limit)
    }
    fn all_messages(&self) -> MessageHistoryResult<Vec<StoredMessage>> {
        MessageHistory::all_messages(self)
    }
    fn delete(&self, message_id: &str) -> MessageHistoryResult<bool> {
        MessageHistory::delete(self, message_id)
    }
    fn cleanup_search_results(&self, tool_names: Option<&[String]>) -> MessageHistoryResult<usize> {
        MessageHistory::cleanup_search_results(self, tool_names)
    }
    fn count(&self) -> MessageHistoryResult<usize> {
        MessageHistory::count(self)
    }
    fn clear(&self) -> MessageHistoryResult<usize> {
        MessageHistory::clear(self)
    }
    fn get_context_window(
        &self,
        max_messages: usize,
        max_chars: usize,
    ) -> MessageHistoryResult<Vec<StoredMessage>> {
        MessageHistory::get_context_window(self, max_messages, max_chars)
    }
}

impl NoteStorage for NoteStore {
    fn create(
        &self,
        title: &str,
        content: &str,
        tags: &[String],
        subdir: Option<&str>,
    ) -> NoteResult<PathBuf> {
        NoteStore::create(self, title, content, tags, subdir)
    }
    fn list_notes(&self, tags: Option<&[String]>) -> NoteResult<Vec<NoteSummary>> {
        NoteStore::list_notes(self, tags)
    }
    fn search(
        &self,
        query: &str,
        tags: Option<&[String]>,
        limit: usize,
    ) -> NoteResult<Vec<NoteSearchResult>> {
        NoteStore::search(self, query, tags, limit)
    }
    fn find_row_by_path(&self, path: &Path) -> NoteResult<Option<MemoryRow>> {
        NoteStore::find_row_by_path(self, path)
    }
    fn all_tags(&self) -> NoteResult<Vec<String>> {
        NoteStore::all_tags(self)
    }
    fn reindex(&self) -> NoteResult<usize> {
        NoteStore::reindex(self)
    }
}

impl TodoStorage for TodoManager {
    fn create(&self, todo: NewTodo) -> TodoResult<i64> {
        TodoManager::create(self, todo)
    }
    fn list(&self, filter: TodoFilter) -> TodoResult<Vec<Todo>> {
        TodoManager::list(self, filter)
    }
    fn get(&self, todo_id: i64) -> TodoResult<Option<Todo>> {
        TodoManager::get(self, todo_id)
    }
    fn update(&self, todo_id: i64, update: TodoUpdate) -> TodoResult<bool> {
        TodoManager::update(self, todo_id, update)
    }
    fn subtasks(&self, parent_id: i64) -> TodoResult<Vec<Todo>> {
        TodoManager::subtasks(self, parent_id)
    }
    fn complete(&self, todo_id: i64) -> TodoResult<bool> {
        TodoManager::complete(self, todo_id)
    }
    fn mark_reminded(&self, todo_id: i64) -> TodoResult<bool> {
        TodoManager::mark_reminded(self, todo_id)
    }
    fn mark_reminded_at(&self, todo_id: i64, now: DateTime<Utc>) -> TodoResult<bool> {
        TodoManager::mark_reminded_at(self, todo_id, now)
    }
    fn due_reminders(&self) -> TodoResult<Vec<Todo>> {
        TodoManager::due_reminders(self)
    }
    fn due_reminders_at(&self, now: DateTime<Utc>) -> TodoResult<Vec<Todo>> {
        TodoManager::due_reminders_at(self, now)
    }
    fn open_work_digest(&self, limit: usize) -> TodoResult<String> {
        TodoManager::open_work_digest(self, limit)
    }
    fn open_work_digest_at(&self, now: DateTime<Utc>, limit: usize) -> TodoResult<String> {
        TodoManager::open_work_digest_at(self, now, limit)
    }
    fn search(&self, query: &str, limit: usize) -> TodoResult<Vec<Todo>> {
        TodoManager::search(self, query, limit)
    }
    fn delete(&self, todo_id: i64) -> TodoResult<bool> {
        TodoManager::delete(self, todo_id)
    }
}
