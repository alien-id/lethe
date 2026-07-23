//! Tenant-scoped PostgreSQL implementation of Lethe's canonical memory model.
//!
//! This module contains storage only. Ranking, embeddings, recall assembly,
//! prompts, tools, compaction, and curation remain the same code used by the
//! local SQLite/file backend.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, Utc};
use postgres::{Row, Transaction};
use postgres_native_tls::MakeTlsConnector;
use r2d2::Pool;
use r2d2_postgres::PostgresConnectionManager;
use serde_json::{Value, json};
use uuid::Uuid;

use super::archival::{ArchivalEntry, ArchivalError, ArchivalResult, compare_entries, score_entry};
use super::backend::{ArchivalStorage, BlockStorage, MessageStorage, NoteStorage, TodoStorage};
use super::blocks::{
    MemoryBlock, MemoryError, MemoryResult, embedded_defaults, enforce_limit, validate_label,
};
use super::codec::semantic_score;
use super::db::{MemoryKind, MemoryRow};
use super::messages::{
    MessageHistoryError, MessageHistoryResult, MessageRole, StoredMessage, compare_messages,
    score_message,
};
use super::notes::{
    NoteError, NoteResult, NoteSearchResult, NoteSummary, compare_search_results, preview,
    score_note, slugify,
};
use super::search::{clean_tags, query_terms, tags_match_any};
use super::semantic::EmbeddingEngine;
use super::store::MemoryStore;
use crate::todos::{
    NewTodo, Todo, TodoError, TodoFilter, TodoPriority, TodoResult, TodoStatus, TodoUpdate,
};

type PgPool = Pool<PostgresConnectionManager<MakeTlsConnector>>;

struct PgPoolGuard {
    pool: Option<PgPool>,
}

impl std::fmt::Debug for PgPoolGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgPoolGuard").finish_non_exhaustive()
    }
}

impl Deref for PgPoolGuard {
    type Target = PgPool;

    fn deref(&self) -> &Self::Target {
        self.pool.as_ref().expect("PostgreSQL pool already dropped")
    }
}

impl Drop for PgPoolGuard {
    fn drop(&mut self) {
        let Some(pool) = self.pool.take() else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            // `postgres` owns an internal Tokio runtime and must also close its
            // idle clients outside a running async runtime.
            std::thread::scope(|scope| {
                scope.spawn(move || drop(pool));
            });
        } else {
            drop(pool);
        }
    }
}

/// Shared connection and embedding resources for a multiplexed process. Agent
/// instances are tenant-specific, but they must not each create their own
/// PostgreSQL pool or load a separate embedding model.
#[derive(Clone, Debug)]
pub struct PostgresMemoryFactory {
    pool: Arc<PgPoolGuard>,
    embedder: EmbeddingEngine,
}

impl PostgresMemoryFactory {
    pub fn new(
        database_url: &str,
        cache_root: impl AsRef<Path>,
        max_pool_size: u32,
    ) -> anyhow::Result<Self> {
        let config = database_url.parse()?;
        // libpq `sslmode=require` semantics: encrypt without CA verification —
        // managed Postgres (RDS et al.) presents a CA outside the web-PKI roots.
        // Whether TLS is used at all still follows the URL's sslmode.
        let tls = MakeTlsConnector::new(
            native_tls::TlsConnector::builder()
                .danger_accept_invalid_certs(true)
                .build()?,
        );
        let manager = PostgresConnectionManager::new(config, tls);
        Ok(Self {
            pool: Arc::new(PgPoolGuard {
                pool: Some(
                    Pool::builder()
                        .max_size(max_pool_size.max(1))
                        .build(manager)?,
                ),
            }),
            embedder: EmbeddingEngine::from_env(cache_root.as_ref()),
        })
    }

    pub fn memory_store(
        &self,
        tenant_id: Uuid,
        workspace_dir: impl Into<PathBuf>,
    ) -> anyhow::Result<MemoryStore> {
        let workspace_dir = workspace_dir.into();
        let backend = Arc::new(PostgresMemory {
            pool: self.pool.clone(),
            tenant_id,
            embedder: self.embedder.clone(),
            notes_root: PathBuf::from("notes"),
        });
        backend
            .init_embedded_defaults()
            .map_err(|error| anyhow::anyhow!(error))?;
        Ok(MemoryStore::from_backends(
            workspace_dir,
            PathBuf::from(format!("postgres://tenant/{tenant_id}")),
            backend.clone(),
            backend.clone(),
            backend.clone(),
            backend.clone(),
            backend,
        ))
    }
}

#[derive(Debug)]
pub struct PostgresMemory {
    pool: Arc<PgPoolGuard>,
    tenant_id: Uuid,
    embedder: EmbeddingEngine,
    notes_root: PathBuf,
}

impl PostgresMemory {
    pub fn connect(
        database_url: &str,
        tenant_id: Uuid,
        workspace_dir: impl Into<PathBuf>,
        cache_root: impl AsRef<Path>,
        max_pool_size: u32,
    ) -> anyhow::Result<MemoryStore> {
        PostgresMemoryFactory::new(database_url, cache_root, max_pool_size)?
            .memory_store(tenant_id, workspace_dir)
    }

    pub fn tenant_id(&self) -> Uuid {
        self.tenant_id
    }

    fn transaction<T>(
        &self,
        operation: impl FnOnce(&mut Transaction<'_>) -> Result<T, postgres::Error>,
    ) -> Result<T, String> {
        let run = || self.run_transaction(operation);
        match tokio::runtime::Handle::try_current().map(|handle| handle.runtime_flavor()) {
            Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(run),
            Ok(_) => Err(
                "the PostgreSQL memory backend requires a multi-thread Tokio runtime".to_string(),
            ),
            Err(_) => run(),
        }
    }

    fn run_transaction<T>(
        &self,
        operation: impl FnOnce(&mut Transaction<'_>) -> Result<T, postgres::Error>,
    ) -> Result<T, String> {
        let mut client = self.pool.get().map_err(|error| error.to_string())?;
        let mut tx = client.transaction().map_err(|error| error.to_string())?;
        tx.execute(
            "SELECT set_config('lethe.tenant_id', $1, true)",
            &[&self.tenant_id.to_string()],
        )
        .map_err(|error| error.to_string())?;
        let output = operation(&mut tx).map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(output)
    }

    fn memory_rows(&self, kind: MemoryKind) -> Result<Vec<(MemoryRow, Vec<f32>)>, String> {
        self.transaction(|tx| {
            tx.query(
                "SELECT id, kind, title, text, metadata, tags, file_path, created_at, updated_at, completed_at, completion_summary, embedding \
                 FROM lethe_memories WHERE tenant_id = $1 AND kind = $2 ORDER BY id",
                &[&self.tenant_id, &kind.as_str()],
            )
            .map(|rows| rows.iter().map(row_to_memory).collect())
        })
    }

    fn message_rows(&self) -> Result<Vec<(StoredMessage, Vec<f32>)>, String> {
        self.transaction(|tx| {
            tx.query(
                "SELECT id, role, content, metadata, created_at, embedding \
                 FROM lethe_messages WHERE tenant_id = $1 ORDER BY created_at, id",
                &[&self.tenant_id],
            )
            .map(|rows| rows.iter().map(row_to_message).collect())
        })
    }
}

impl BlockStorage for PostgresMemory {
    fn init_embedded_defaults(&self) -> MemoryResult<()> {
        let defaults = embedded_defaults()?;
        self.transaction(|tx| {
            for block in &defaults {
                tx.execute(
                    "INSERT INTO lethe_memory_blocks \
                     (tenant_id, label, value, description, char_limit, read_only, hidden, stable, created_at, updated_at) \
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,COALESCE($9,now()),COALESCE($10,now())) \
                     ON CONFLICT (tenant_id, label) DO NOTHING",
                    &[
                        &self.tenant_id,
                        &block.label,
                        &block.value,
                        &block.description,
                        &(block.limit as i64),
                        &block.read_only,
                        &block.hidden,
                        &block.stable,
                        &block.created_at,
                        &block.updated_at,
                    ],
                )?;
            }
            tx.execute(
                "UPDATE lethe_memory_blocks SET stable = true \
                 WHERE tenant_id = $1 AND label = 'human' AND stable = false",
                &[&self.tenant_id],
            )?;
            Ok(())
        })
        .map_err(MemoryError::Backend)
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
        validate_label(label)?;
        enforce_limit(value, limit)?;
        let inserted = self
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO lethe_memory_blocks \
                     (tenant_id,label,value,description,char_limit,read_only,hidden,stable) \
                     VALUES ($1,$2,$3,$4,$5,$6,$7,false) ON CONFLICT DO NOTHING",
                    &[
                        &self.tenant_id,
                        &label,
                        &value,
                        &description,
                        &(limit as i64),
                        &read_only,
                        &hidden,
                    ],
                )
            })
            .map_err(MemoryError::Backend)?;
        if inserted == 0 {
            return Err(MemoryError::AlreadyExists(label.to_string()));
        }
        Ok(label.to_string())
    }

    fn get(&self, label: &str) -> MemoryResult<Option<MemoryBlock>> {
        validate_label(label)?;
        self.transaction(|tx| {
            tx.query_opt(
                "SELECT label,value,description,char_limit,read_only,hidden,stable,created_at,updated_at \
                 FROM lethe_memory_blocks WHERE tenant_id=$1 AND label=$2",
                &[&self.tenant_id, &label],
            )
            .map(|row| row.as_ref().map(row_to_block))
        })
        .map_err(MemoryError::Backend)
    }

    fn update(
        &self,
        label: &str,
        value: Option<&str>,
        description: Option<&str>,
    ) -> MemoryResult<bool> {
        self.update_block(label, value, description, false)
    }

    fn system_update(&self, label: &str, value: &str) -> MemoryResult<bool> {
        self.update_block(label, Some(value), None, true)
    }

    fn append(&self, label: &str, text: &str) -> MemoryResult<bool> {
        let Some(block) = BlockStorage::get(self, label)? else {
            return Ok(false);
        };
        BlockStorage::update(self, label, Some(&format!("{}{}", block.value, text)), None)
    }

    fn str_replace(&self, label: &str, old: &str, new: &str) -> MemoryResult<bool> {
        let Some(block) = BlockStorage::get(self, label)? else {
            return Err(MemoryError::NotFound(label.to_string()));
        };
        if !block.value.contains(old) {
            return Ok(false);
        }
        BlockStorage::update(self, label, Some(&block.value.replacen(old, new, 1)), None)
    }

    fn delete(&self, label: &str) -> MemoryResult<bool> {
        validate_label(label)?;
        self.transaction(|tx| {
            tx.execute(
                "DELETE FROM lethe_memory_blocks WHERE tenant_id=$1 AND label=$2",
                &[&self.tenant_id, &label],
            )
            .map(|count| count > 0)
        })
        .map_err(MemoryError::Backend)
    }

    fn list_blocks(&self, include_hidden: bool) -> MemoryResult<Vec<MemoryBlock>> {
        self.transaction(|tx| {
            tx.query(
                "SELECT label,value,description,char_limit,read_only,hidden,stable,created_at,updated_at \
                 FROM lethe_memory_blocks WHERE tenant_id=$1 AND ($2 OR NOT hidden) ORDER BY label",
                &[&self.tenant_id, &include_hidden],
            )
            .map(|rows| rows.iter().map(row_to_block).collect())
        })
        .map_err(MemoryError::Backend)
    }
}

impl PostgresMemory {
    fn update_block(
        &self,
        label: &str,
        value: Option<&str>,
        description: Option<&str>,
        bypass_read_only: bool,
    ) -> MemoryResult<bool> {
        validate_label(label)?;
        let Some(block) = BlockStorage::get(self, label)? else {
            return Ok(false);
        };
        if value.is_some() && block.read_only && !bypass_read_only {
            return Err(MemoryError::ReadOnly(label.to_string()));
        }
        if let Some(value) = value {
            enforce_limit(value, block.limit)?;
        }
        self.transaction(|tx| {
            tx.execute(
                "UPDATE lethe_memory_blocks SET value=COALESCE($3,value), \
                 description=COALESCE($4,description), updated_at=now() \
                 WHERE tenant_id=$1 AND label=$2",
                &[&self.tenant_id, &label, &value, &description],
            )
            .map(|count| count > 0)
        })
        .map_err(MemoryError::Backend)
    }
}

impl ArchivalStorage for PostgresMemory {
    fn embedder(&self) -> &EmbeddingEngine {
        &self.embedder
    }

    fn add(&self, text: &str, metadata: Option<Value>, tags: &[String]) -> ArchivalResult<String> {
        let text = text.trim();
        if text.is_empty() {
            return Err(ArchivalError::EmptyText);
        }
        let metadata = metadata.unwrap_or_else(|| json!({}));
        if !metadata.is_object() {
            return Err(ArchivalError::InvalidMetadata);
        }
        let id = format!("mem-{}", Uuid::new_v4());
        let tags = clean_tags(tags);
        let embedding = self.embedder.embed_document(text)?;
        self.transaction(|tx| {
            tx.execute(
                "INSERT INTO lethe_memories \
                 (tenant_id,id,kind,text,metadata,tags,embedding) \
                 VALUES ($1,$2,'archival',$3,$4,$5,$6)",
                &[&self.tenant_id, &id, &text, &metadata, &tags, &embedding],
            )?;
            Ok(())
        })
        .map_err(ArchivalError::Backend)?;
        Ok(id)
    }

    fn search(
        &self,
        query: &str,
        limit: usize,
        tags: Option<&[String]>,
    ) -> ArchivalResult<Vec<ArchivalEntry>> {
        let query = query.trim();
        let limit = if limit == 0 { 10 } else { limit };
        let terms = query_terms(query);
        let tag_filter = clean_tags(tags.unwrap_or_default());
        let rows = self
            .memory_rows(MemoryKind::Archival)
            .map_err(ArchivalError::Backend)?;
        let mut merged = HashMap::<String, ArchivalEntry>::new();
        for (row, _) in &rows {
            let mut entry = archival_entry(row);
            if !tags_match_any(&entry.tags, &tag_filter) {
                continue;
            }
            entry.score = score_entry(query, &terms, &entry);
            if terms.is_empty() || entry.score > 0.0 {
                merged.insert(entry.id.clone(), entry);
            }
        }
        if !query.is_empty() {
            let query_embedding = self.embedder.embed_query(query)?;
            let mut semantic = rows
                .iter()
                .map(|(row, embedding)| {
                    let mut entry = archival_entry(row);
                    entry.score = semantic_score(euclidean_distance(&query_embedding, embedding));
                    entry
                })
                .collect::<Vec<_>>();
            semantic.sort_by(compare_entries);
            semantic.truncate(limit * 3);
            for entry in semantic {
                if !tags_match_any(&entry.tags, &tag_filter) {
                    continue;
                }
                merged
                    .entry(entry.id.clone())
                    .and_modify(|current| current.score += entry.score)
                    .or_insert(entry);
            }
        }
        let mut entries = merged.into_values().collect::<Vec<_>>();
        entries.sort_by(compare_entries);
        entries.truncate(limit);
        Ok(entries)
    }

    fn get(&self, memory_id: &str) -> ArchivalResult<Option<ArchivalEntry>> {
        self.transaction(|tx| {
            tx.query_opt(
                "SELECT id,kind,title,text,metadata,tags,file_path,created_at,updated_at,completed_at,completion_summary,embedding \
                 FROM lethe_memories WHERE tenant_id=$1 AND id=$2 AND kind='archival'",
                &[&self.tenant_id, &memory_id],
            )
            .map(|row| row.as_ref().map(|row| archival_entry(&row_to_memory(row).0)))
        })
        .map_err(ArchivalError::Backend)
    }

    fn delete(&self, memory_id: &str) -> ArchivalResult<bool> {
        self.transaction(|tx| {
            tx.execute(
                "DELETE FROM lethe_memories WHERE tenant_id=$1 AND id=$2",
                &[&self.tenant_id, &memory_id],
            )
            .map(|count| count > 0)
        })
        .map_err(ArchivalError::Backend)
    }

    fn update_tags(&self, memory_id: &str, tags: &[String]) -> ArchivalResult<bool> {
        let tags = clean_tags(tags);
        self.transaction(|tx| {
            tx.execute(
                "UPDATE lethe_memories SET tags=$3,updated_at=now() WHERE tenant_id=$1 AND id=$2",
                &[&self.tenant_id, &memory_id, &tags],
            )
            .map(|count| count > 0)
        })
        .map_err(ArchivalError::Backend)
    }

    fn set_completed_at(&self, memory_id: &str, value: Option<&str>) -> ArchivalResult<bool> {
        let parsed = value
            .map(DateTime::parse_from_rfc3339)
            .transpose()
            .map_err(|error| ArchivalError::Backend(error.to_string()))?
            .map(|value| value.with_timezone(&Utc));
        self.transaction(|tx| {
            tx.execute(
                "UPDATE lethe_memories SET completed_at=$3,updated_at=now() WHERE tenant_id=$1 AND id=$2",
                &[&self.tenant_id, &memory_id, &parsed],
            )
            .map(|count| count > 0)
        })
        .map_err(ArchivalError::Backend)
    }

    fn set_completion_summary(&self, memory_id: &str, value: Option<&str>) -> ArchivalResult<bool> {
        self.transaction(|tx| {
            tx.execute(
                "UPDATE lethe_memories SET completion_summary=$3,updated_at=now() WHERE tenant_id=$1 AND id=$2",
                &[&self.tenant_id, &memory_id, &value],
            )
            .map(|count| count > 0)
        })
        .map_err(ArchivalError::Backend)
    }

    fn list_completed_without_summary(&self, limit: usize) -> ArchivalResult<Vec<ArchivalEntry>> {
        self.transaction(|tx| {
            tx.query(
                "SELECT id,kind,title,text,metadata,tags,file_path,created_at,updated_at,completed_at,completion_summary,embedding \
                 FROM lethe_memories WHERE tenant_id=$1 AND kind='archival' AND completed_at IS NOT NULL \
                 AND completion_summary IS NULL ORDER BY completed_at,id LIMIT $2",
                &[&self.tenant_id, &(limit.max(1) as i64)],
            )
            .map(|rows| rows.iter().map(|row| archival_entry(&row_to_memory(row).0)).collect())
        })
        .map_err(ArchivalError::Backend)
    }

    fn count(&self) -> ArchivalResult<usize> {
        self.transaction(|tx| {
            tx.query_one(
                "SELECT count(*) FROM lethe_memories WHERE tenant_id=$1 AND kind='archival'",
                &[&self.tenant_id],
            )
            .map(|row| row.get::<_, i64>(0) as usize)
        })
        .map_err(ArchivalError::Backend)
    }

    fn list_recent(&self, limit: usize) -> ArchivalResult<Vec<ArchivalEntry>> {
        self.transaction(|tx| {
            tx.query(
                "SELECT id,kind,title,text,metadata,tags,file_path,created_at,updated_at,completed_at,completion_summary,embedding \
                 FROM lethe_memories WHERE tenant_id=$1 AND kind='archival' ORDER BY created_at DESC,id LIMIT $2",
                &[&self.tenant_id, &((if limit == 0 { 50 } else { limit }) as i64)],
            )
            .map(|rows| rows.iter().map(|row| archival_entry(&row_to_memory(row).0)).collect())
        })
        .map_err(ArchivalError::Backend)
    }

    fn all_entries(&self) -> ArchivalResult<Vec<ArchivalEntry>> {
        self.memory_rows(MemoryKind::Archival)
            .map(|rows| rows.iter().map(|(row, _)| archival_entry(row)).collect())
            .map_err(ArchivalError::Backend)
    }
}

impl MessageStorage for PostgresMemory {
    fn add(
        &self,
        role: MessageRole,
        content: &str,
        metadata: Option<Value>,
    ) -> MessageHistoryResult<String> {
        if role.as_str().is_empty() {
            return Err(MessageHistoryError::EmptyRole);
        }
        let metadata = metadata.unwrap_or_else(|| json!({}));
        if !metadata.is_object() {
            return Err(MessageHistoryError::InvalidMetadata);
        }
        let id = format!("msg-{}", Uuid::new_v4());
        let embedding = self.embedder.embed_document(content)?;
        self.transaction(|tx| {
            tx.execute(
                "INSERT INTO lethe_messages (tenant_id,id,role,content,metadata,embedding) \
                 VALUES ($1,$2,$3,$4,$5,$6)",
                &[
                    &self.tenant_id,
                    &id,
                    &role.as_str(),
                    &content,
                    &metadata,
                    &embedding,
                ],
            )?;
            Ok(())
        })
        .map_err(MessageHistoryError::Backend)?;
        Ok(id)
    }

    fn get(&self, message_id: &str) -> MessageHistoryResult<Option<StoredMessage>> {
        self.transaction(|tx| {
            tx.query_opt(
                "SELECT id,role,content,metadata,created_at,embedding FROM lethe_messages \
                 WHERE tenant_id=$1 AND id=$2",
                &[&self.tenant_id, &message_id],
            )
            .map(|row| row.as_ref().map(|row| row_to_message(row).0))
        })
        .map_err(MessageHistoryError::Backend)
    }

    fn get_recent(&self, limit: usize) -> MessageHistoryResult<Vec<StoredMessage>> {
        self.transaction(|tx| {
            tx.query(
                "SELECT id,role,content,metadata,created_at,embedding FROM ( \
                   SELECT id,role,content,metadata,created_at,embedding FROM lethe_messages \
                   WHERE tenant_id=$1 ORDER BY created_at DESC,id DESC LIMIT $2 \
                 ) recent ORDER BY created_at,id",
                &[
                    &self.tenant_id,
                    &((if limit == 0 { 20 } else { limit }) as i64),
                ],
            )
            .map(|rows| rows.iter().map(|row| row_to_message(row).0).collect())
        })
        .map_err(MessageHistoryError::Backend)
    }

    fn search(
        &self,
        query: &str,
        limit: usize,
        role: Option<&MessageRole>,
    ) -> MessageHistoryResult<Vec<StoredMessage>> {
        let query = query.trim();
        let limit = if limit == 0 { 20 } else { limit };
        let terms = query_terms(query);
        let rows = self.message_rows().map_err(MessageHistoryError::Backend)?;
        let mut merged = HashMap::<String, StoredMessage>::new();
        for (message, _) in &rows {
            if role.is_some_and(|role| role != &message.role) {
                continue;
            }
            let mut message = message.clone();
            message.score = score_message(query, &terms, &message);
            if terms.is_empty() || message.score > 0.0 {
                merged.insert(message.id.clone(), message);
            }
        }
        if !query.is_empty() {
            let query_embedding = self.embedder.embed_query(query)?;
            let mut semantic = rows
                .iter()
                .filter(|(message, _)| role.is_none_or(|role| role == &message.role))
                .map(|(message, embedding)| {
                    let mut message = message.clone();
                    message.score = semantic_score(euclidean_distance(&query_embedding, embedding));
                    message
                })
                .collect::<Vec<_>>();
            semantic.sort_by(compare_messages);
            semantic.truncate(limit * 4);
            for message in semantic {
                merged
                    .entry(message.id.clone())
                    .and_modify(|current| current.score += message.score)
                    .or_insert(message);
            }
        }
        let mut messages = merged.into_values().collect::<Vec<_>>();
        messages.sort_by(compare_messages);
        messages.truncate(limit);
        Ok(messages)
    }

    fn search_by_role(
        &self,
        query: &str,
        role: &MessageRole,
        limit: usize,
    ) -> MessageHistoryResult<Vec<StoredMessage>> {
        MessageStorage::search(self, query, limit, Some(role))
    }

    fn get_by_role(
        &self,
        role: &MessageRole,
        limit: usize,
    ) -> MessageHistoryResult<Vec<StoredMessage>> {
        self.transaction(|tx| {
            tx.query(
                "SELECT id,role,content,metadata,created_at,embedding FROM lethe_messages \
                 WHERE tenant_id=$1 AND role=$2 ORDER BY created_at,id LIMIT $3",
                &[
                    &self.tenant_id,
                    &role.as_str(),
                    &((if limit == 0 { 50 } else { limit }) as i64),
                ],
            )
            .map(|rows| rows.iter().map(|row| row_to_message(row).0).collect())
        })
        .map_err(MessageHistoryError::Backend)
    }

    fn all_messages(&self) -> MessageHistoryResult<Vec<StoredMessage>> {
        self.message_rows()
            .map(|rows| rows.into_iter().map(|(message, _)| message).collect())
            .map_err(MessageHistoryError::Backend)
    }

    fn delete(&self, message_id: &str) -> MessageHistoryResult<bool> {
        self.transaction(|tx| {
            tx.execute(
                "DELETE FROM lethe_messages WHERE tenant_id=$1 AND id=$2",
                &[&self.tenant_id, &message_id],
            )
            .map(|count| count > 0)
        })
        .map_err(MessageHistoryError::Backend)
    }

    fn cleanup_search_results(&self, tool_names: Option<&[String]>) -> MessageHistoryResult<usize> {
        let names = tool_names
            .map(|names| {
                names
                    .iter()
                    .map(|name| name.trim().to_string())
                    .filter(|name| !name.is_empty())
                    .collect::<HashSet<_>>()
            })
            .filter(|names| !names.is_empty())
            .unwrap_or_else(|| {
                ["conversation_search", "archival_search"]
                    .into_iter()
                    .map(str::to_string)
                    .collect()
            });
        let messages = self.all_messages()?;
        let mut calls = HashMap::new();
        for message in &messages {
            if !message.role.is_assistant() {
                continue;
            }
            for call in message
                .metadata
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let (Some(id), Some(name)) = (
                    call.get("id").and_then(Value::as_str),
                    call.get("function")
                        .and_then(|value| value.get("name"))
                        .and_then(Value::as_str),
                ) {
                    calls.insert(id.to_string(), name.to_string());
                }
            }
        }
        let mut deleted = 0;
        for message in messages {
            if !message.role.is_tool() {
                continue;
            }
            let Some(call_id) = message.metadata.get("tool_call_id").and_then(Value::as_str) else {
                continue;
            };
            if calls.get(call_id).is_some_and(|name| names.contains(name))
                && MessageStorage::delete(self, &message.id)?
            {
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    fn count(&self) -> MessageHistoryResult<usize> {
        self.transaction(|tx| {
            tx.query_one(
                "SELECT count(*) FROM lethe_messages WHERE tenant_id=$1",
                &[&self.tenant_id],
            )
            .map(|row| row.get::<_, i64>(0) as usize)
        })
        .map_err(MessageHistoryError::Backend)
    }

    fn clear(&self) -> MessageHistoryResult<usize> {
        self.transaction(|tx| {
            tx.execute(
                "DELETE FROM lethe_messages WHERE tenant_id=$1",
                &[&self.tenant_id],
            )
            .map(|count| count as usize)
        })
        .map_err(MessageHistoryError::Backend)
    }

    fn get_context_window(
        &self,
        max_messages: usize,
        max_chars: usize,
    ) -> MessageHistoryResult<Vec<StoredMessage>> {
        let messages = self.get_recent(max_messages)?;
        let mut total = 0;
        let mut result = Vec::new();
        for message in messages.into_iter().rev() {
            let size = message.content.chars().count();
            if total + size > max_chars {
                break;
            }
            total += size;
            result.insert(0, message);
        }
        Ok(result)
    }
}

impl NoteStorage for PostgresMemory {
    fn create(
        &self,
        title: &str,
        content: &str,
        tags: &[String],
        subdir: Option<&str>,
    ) -> NoteResult<PathBuf> {
        let title = title.trim();
        if title.is_empty() {
            return Err(NoteError::EmptyTitle);
        }
        let subdir = safe_subdir(subdir)?;
        let tags = clean_tags(tags);
        let base = slugify(title);
        let existing = self.list_notes(None)?;
        let mut counter = 1;
        let path = loop {
            let filename = if counter == 1 {
                format!("{base}.md")
            } else {
                format!("{base}_{counter}.md")
            };
            let candidate = self.notes_root.join(&subdir).join(filename);
            if !existing.iter().any(|note| note.file_path == candidate) {
                break candidate;
            }
            counter += 1;
        };
        let id = format!("note-{}", Uuid::new_v4());
        let indexed_text = format!("{}\n{}\n{}", title, tags.join(" "), content.trim());
        let embedding = self.embedder.embed_document(&indexed_text)?;
        let path_string = path.display().to_string();
        self.transaction(|tx| {
            tx.execute(
                "INSERT INTO lethe_memories \
                 (tenant_id,id,kind,title,text,metadata,tags,file_path,embedding) \
                 VALUES ($1,$2,'note',$3,$4,'{}'::jsonb,$5,$6,$7)",
                &[
                    &self.tenant_id,
                    &id,
                    &title,
                    &content.trim(),
                    &tags,
                    &path_string,
                    &embedding,
                ],
            )?;
            Ok(())
        })
        .map_err(NoteError::Backend)?;
        Ok(path)
    }

    fn list_notes(&self, tags: Option<&[String]>) -> NoteResult<Vec<NoteSummary>> {
        let filter = clean_tags(tags.unwrap_or_default());
        let mut notes = self
            .memory_rows(MemoryKind::Note)
            .map_err(NoteError::Backend)?
            .into_iter()
            .filter(|(row, _)| filter.iter().all(|tag| row.tags.contains(tag)))
            .map(|(row, _)| NoteSummary {
                title: row.title.unwrap_or_else(|| "untitled".to_string()),
                tags: row.tags,
                file_path: PathBuf::from(row.file_path.unwrap_or_default()),
                created: date_only(&row.created_at),
            })
            .collect::<Vec<_>>();
        notes.sort_by(|left, right| left.file_path.cmp(&right.file_path));
        Ok(notes)
    }

    fn search(
        &self,
        query: &str,
        tags: Option<&[String]>,
        limit: usize,
    ) -> NoteResult<Vec<NoteSearchResult>> {
        let query = query.trim();
        let limit = if limit == 0 { 5 } else { limit };
        let terms = query_terms(query);
        let filter = clean_tags(tags.unwrap_or_default());
        let rows = self
            .memory_rows(MemoryKind::Note)
            .map_err(NoteError::Backend)?;
        let mut merged = HashMap::<String, NoteSearchResult>::new();
        for (row, _) in &rows {
            if !filter.iter().all(|tag| row.tags.contains(tag)) {
                continue;
            }
            let title = row.title.clone().unwrap_or_else(|| "untitled".to_string());
            let score = score_note(query, &terms, &title, &row.tags, &row.text);
            if score <= 0.0 && !terms.is_empty() {
                continue;
            }
            merged.insert(row.id.clone(), note_result(row, score));
        }
        if !query.is_empty() {
            let query_embedding = self.embedder.embed_query(query)?;
            let mut semantic = rows
                .iter()
                .map(|(row, embedding)| {
                    (
                        row.id.clone(),
                        note_result(
                            row,
                            semantic_score(euclidean_distance(&query_embedding, embedding)),
                        ),
                    )
                })
                .collect::<Vec<_>>();
            semantic.sort_by(|left, right| compare_search_results(&left.1, &right.1));
            semantic.truncate(limit * 3);
            for (id, result) in semantic {
                if !filter.iter().all(|tag| result.tags.contains(tag)) {
                    continue;
                }
                merged
                    .entry(id)
                    .and_modify(|current| current.score += result.score)
                    .or_insert(result);
            }
        }
        let mut results = merged.into_values().collect::<Vec<_>>();
        results.sort_by(compare_search_results);
        results.truncate(limit);
        Ok(results)
    }

    fn find_row_by_path(&self, path: &Path) -> NoteResult<Option<MemoryRow>> {
        let path = path.display().to_string();
        self.transaction(|tx| {
            tx.query_opt(
                "SELECT id,kind,title,text,metadata,tags,file_path,created_at,updated_at,completed_at,completion_summary,embedding \
                 FROM lethe_memories WHERE tenant_id=$1 AND kind='note' AND file_path=$2",
                &[&self.tenant_id, &path],
            )
            .map(|row| row.as_ref().map(|row| row_to_memory(row).0))
        })
        .map_err(NoteError::Backend)
    }

    fn all_tags(&self) -> NoteResult<Vec<String>> {
        let mut tags = BTreeSet::new();
        for note in self.list_notes(None)? {
            tags.extend(note.tags);
        }
        Ok(tags.into_iter().collect())
    }

    fn reindex(&self) -> NoteResult<usize> {
        self.list_notes(None).map(|notes| notes.len())
    }
}

impl TodoStorage for PostgresMemory {
    fn create(&self, todo: NewTodo) -> TodoResult<i64> {
        if todo.title.trim().is_empty() {
            return Err(TodoError::EmptyTitle);
        }
        let title = todo.title.trim();
        let description = nonempty(todo.description.as_deref());
        let due_date = nonempty(todo.due_date.as_deref());
        let source = nonempty(todo.source.as_deref());
        self.transaction(|tx| {
            tx.query_one(
                "INSERT INTO lethe_todos \
                 (tenant_id,title,description,priority,due_date,tags,source,parent_id) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8) RETURNING id",
                &[
                    &self.tenant_id,
                    &title,
                    &description,
                    &todo.priority.as_str(),
                    &due_date,
                    &todo.tags,
                    &source,
                    &todo.parent_id,
                ],
            )
            .map(|row| row.get(0))
        })
        .map_err(TodoError::Backend)
    }

    fn list(&self, filter: TodoFilter) -> TodoResult<Vec<Todo>> {
        let mut todos = self.all_todos()?;
        if let Some(status) = filter.status {
            todos.retain(|todo| todo.status == status);
        } else if !filter.include_completed {
            todos.retain(|todo| todo_active(todo.status));
        }
        if let Some(priority) = filter.priority {
            todos.retain(|todo| todo.priority == priority);
        }
        todos.sort_by(compare_todos);
        todos.truncate(if filter.limit == 0 { 50 } else { filter.limit });
        Ok(todos)
    }

    fn get(&self, todo_id: i64) -> TodoResult<Option<Todo>> {
        self.transaction(|tx| {
            tx.query_opt(
                "SELECT id,title,description,status,priority,created_at,updated_at,completed_at,due_date,last_reminded_at,remind_count,tags,source,parent_id \
                 FROM lethe_todos WHERE tenant_id=$1 AND id=$2",
                &[&self.tenant_id, &todo_id],
            )
            .map(|row| row.as_ref().map(row_to_todo))
        })
        .map_err(TodoError::Backend)
    }

    fn update(&self, todo_id: i64, update: TodoUpdate) -> TodoResult<bool> {
        let Some(mut todo) = TodoStorage::get(self, todo_id)? else {
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
            todo.description = nonempty(Some(&description)).map(str::to_string);
            changed = true;
        }
        if let Some(status) = update.status {
            todo.status = status;
            if status == TodoStatus::Completed {
                todo.completed_at = Some(Utc::now().to_rfc3339());
            }
            changed = true;
        }
        if let Some(priority) = update.priority {
            todo.priority = priority;
            changed = true;
        }
        if let Some(due_date) = update.due_date {
            todo.due_date = nonempty(Some(&due_date)).map(str::to_string);
            changed = true;
        }
        if let Some(parent_id) = update.parent_id {
            todo.parent_id = (parent_id > 0).then_some(parent_id);
            changed = true;
        }
        if !changed {
            return Ok(false);
        }
        let completed_at = todo.completed_at.as_deref().and_then(parse_timestamp);
        self.transaction(|tx| {
            tx.execute(
                "UPDATE lethe_todos SET title=$3,description=$4,status=$5,priority=$6,updated_at=now(), \
                 completed_at=$7,due_date=$8,parent_id=$9 WHERE tenant_id=$1 AND id=$2",
                &[
                    &self.tenant_id,
                    &todo_id,
                    &todo.title,
                    &todo.description,
                    &todo.status.as_str(),
                    &todo.priority.as_str(),
                    &completed_at,
                    &todo.due_date,
                    &todo.parent_id,
                ],
            )
            .map(|count| count > 0)
        })
        .map_err(TodoError::Backend)
    }

    fn subtasks(&self, parent_id: i64) -> TodoResult<Vec<Todo>> {
        let mut todos = self.all_todos()?;
        todos.retain(|todo| todo.parent_id == Some(parent_id) && todo_active(todo.status));
        todos.sort_by(compare_todos);
        Ok(todos)
    }

    fn complete(&self, todo_id: i64) -> TodoResult<bool> {
        TodoStorage::update(
            self,
            todo_id,
            TodoUpdate {
                status: Some(TodoStatus::Completed),
                ..Default::default()
            },
        )
    }

    fn mark_reminded(&self, todo_id: i64) -> TodoResult<bool> {
        self.mark_reminded_at(todo_id, Utc::now())
    }

    fn mark_reminded_at(&self, todo_id: i64, now: DateTime<Utc>) -> TodoResult<bool> {
        self.transaction(|tx| {
            tx.execute(
                "UPDATE lethe_todos SET last_reminded_at=$3,remind_count=remind_count+1,updated_at=$3 \
                 WHERE tenant_id=$1 AND id=$2",
                &[&self.tenant_id, &todo_id, &now],
            )
            .map(|count| count > 0)
        })
        .map_err(TodoError::Backend)
    }

    fn due_reminders(&self) -> TodoResult<Vec<Todo>> {
        self.due_reminders_at(Utc::now())
    }

    fn due_reminders_at(&self, now: DateTime<Utc>) -> TodoResult<Vec<Todo>> {
        let mut due = Vec::new();
        for todo in self.list(TodoFilter {
            include_completed: false,
            limit: usize::MAX,
            ..Default::default()
        })? {
            let Some(last) = todo.last_reminded_at.as_deref().and_then(parse_timestamp) else {
                due.push(todo);
                continue;
            };
            if now.signed_duration_since(last) >= reminder_interval(todo.priority) {
                due.push(todo);
            }
        }
        Ok(due)
    }

    fn open_work_digest(&self, limit: usize) -> TodoResult<String> {
        self.open_work_digest_at(Utc::now(), limit)
    }

    fn open_work_digest_at(&self, now: DateTime<Utc>, limit: usize) -> TodoResult<String> {
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

    fn search(&self, query: &str, limit: usize) -> TodoResult<Vec<Todo>> {
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

    fn delete(&self, todo_id: i64) -> TodoResult<bool> {
        self.transaction(|tx| {
            tx.execute(
                "DELETE FROM lethe_todos WHERE tenant_id=$1 AND id=$2",
                &[&self.tenant_id, &todo_id],
            )
            .map(|count| count > 0)
        })
        .map_err(TodoError::Backend)
    }
}

impl PostgresMemory {
    fn all_todos(&self) -> TodoResult<Vec<Todo>> {
        self.transaction(|tx| {
            tx.query(
                "SELECT id,title,description,status,priority,created_at,updated_at,completed_at,due_date,last_reminded_at,remind_count,tags,source,parent_id \
                 FROM lethe_todos WHERE tenant_id=$1",
                &[&self.tenant_id],
            )
            .map(|rows| rows.iter().map(row_to_todo).collect())
        })
        .map_err(TodoError::Backend)
    }
}

fn row_to_block(row: &Row) -> MemoryBlock {
    MemoryBlock {
        label: row.get("label"),
        value: row.get("value"),
        description: row.get("description"),
        limit: row.get::<_, i64>("char_limit") as usize,
        read_only: row.get("read_only"),
        hidden: row.get("hidden"),
        stable: row.get("stable"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn row_to_memory(row: &Row) -> (MemoryRow, Vec<f32>) {
    let kind: String = row.get("kind");
    (
        MemoryRow {
            id: row.get("id"),
            kind: if kind == "note" {
                MemoryKind::Note
            } else {
                MemoryKind::Archival
            },
            title: row.get("title"),
            text: row.get("text"),
            metadata: row.get("metadata"),
            tags: row.get("tags"),
            file_path: row.get("file_path"),
            created_at: row.get::<_, DateTime<Utc>>("created_at").to_rfc3339(),
            updated_at: row
                .get::<_, Option<DateTime<Utc>>>("updated_at")
                .map(|value| value.to_rfc3339()),
            completed_at: row
                .get::<_, Option<DateTime<Utc>>>("completed_at")
                .map(|value| value.to_rfc3339()),
            completion_summary: row.get("completion_summary"),
        },
        row.get("embedding"),
    )
}

fn archival_entry(row: &MemoryRow) -> ArchivalEntry {
    ArchivalEntry {
        id: row.id.clone(),
        text: row.text.clone(),
        metadata: row.metadata.clone(),
        tags: row.tags.clone(),
        created_at: row.created_at.clone(),
        completed_at: row.completed_at.clone(),
        completion_summary: row.completion_summary.clone(),
        score: 0.0,
    }
}

fn row_to_message(row: &Row) -> (StoredMessage, Vec<f32>) {
    (
        StoredMessage {
            id: row.get("id"),
            role: MessageRole::parse(row.get::<_, String>("role").as_str()),
            content: row.get("content"),
            metadata: row.get("metadata"),
            created_at: row.get::<_, DateTime<Utc>>("created_at").to_rfc3339(),
            score: 0.0,
        },
        row.get("embedding"),
    )
}

fn note_result(row: &MemoryRow, score: f64) -> NoteSearchResult {
    NoteSearchResult {
        title: row.title.clone().unwrap_or_else(|| "untitled".to_string()),
        tags: row.tags.clone(),
        file_path: PathBuf::from(row.file_path.clone().unwrap_or_default()),
        preview: preview(&row.text),
        score,
        created: date_only(&row.created_at),
        completed_at: row.completed_at.clone(),
        completion_summary: row.completion_summary.clone(),
    }
}

fn row_to_todo(row: &Row) -> Todo {
    Todo {
        id: row.get("id"),
        title: row.get("title"),
        description: row.get("description"),
        status: TodoStatus::parse(row.get::<_, String>("status").as_str())
            .unwrap_or(TodoStatus::Pending),
        priority: TodoPriority::parse(row.get::<_, String>("priority").as_str())
            .unwrap_or(TodoPriority::Normal),
        created_at: row.get::<_, DateTime<Utc>>("created_at").to_rfc3339(),
        updated_at: row.get::<_, DateTime<Utc>>("updated_at").to_rfc3339(),
        completed_at: row
            .get::<_, Option<DateTime<Utc>>>("completed_at")
            .map(|value| value.to_rfc3339()),
        due_date: row.get("due_date"),
        last_reminded_at: row
            .get::<_, Option<DateTime<Utc>>>("last_reminded_at")
            .map(|value| value.to_rfc3339()),
        remind_count: row.get("remind_count"),
        tags: row.get("tags"),
        source: row.get("source"),
        parent_id: row.get("parent_id"),
    }
}

fn euclidean_distance(left: &[f32], right: &[f32]) -> f64 {
    left.iter()
        .zip(right)
        .map(|(left, right)| {
            let difference = f64::from(*left) - f64::from(*right);
            difference * difference
        })
        .sum::<f64>()
        .sqrt()
}

fn safe_subdir(value: Option<&str>) -> NoteResult<PathBuf> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(PathBuf::new());
    };
    let path = Path::new(value);
    if path.is_absolute()
        || !path.components().all(|component| {
            matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        return Err(NoteError::UnsafeSubdir(value.to_string()));
    }
    Ok(path.to_path_buf())
}

fn date_only(value: &str) -> String {
    value.get(..10).unwrap_or(value).to_string()
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn parse_due_date(value: &str) -> Option<DateTime<Utc>> {
    if let Some(value) = parse_timestamp(value.trim()) {
        return Some(value);
    }
    let date = NaiveDate::parse_from_str(value.trim().get(..10)?, "%Y-%m-%d").ok()?;
    Some(DateTime::from_naive_utc_and_offset(
        date.and_hms_opt(23, 59, 59)?,
        Utc,
    ))
}

fn reminder_interval(priority: TodoPriority) -> chrono::Duration {
    match priority {
        TodoPriority::Urgent => chrono::Duration::hours(1),
        TodoPriority::High => chrono::Duration::hours(4),
        TodoPriority::Normal => chrono::Duration::days(1),
        TodoPriority::Low => chrono::Duration::days(7),
    }
}

fn todo_active(status: TodoStatus) -> bool {
    !matches!(status, TodoStatus::Completed | TodoStatus::Cancelled)
}

fn compare_todos(left: &Todo, right: &Todo) -> std::cmp::Ordering {
    priority_rank(left.priority)
        .cmp(&priority_rank(right.priority))
        .then_with(
            || match (left.due_date.as_deref(), right.due_date.as_deref()) {
                (Some(left), Some(right)) => left.cmp(right),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            },
        )
        .then_with(|| right.created_at.cmp(&left.created_at))
}

fn priority_rank(priority: TodoPriority) -> u8 {
    match priority {
        TodoPriority::Urgent => 1,
        TodoPriority::High => 2,
        TodoPriority::Normal => 3,
        TodoPriority::Low => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::recall::{Hippocampus, HippocampusConfig};

    #[test]
    fn postgres_backend_preserves_recall_domains_and_tenant_isolation() {
        let Ok(database_url) = std::env::var("LETHE_TEST_POSTGRES_URL") else {
            return;
        };
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let mut admin = postgres::Client::connect(&database_url, postgres::NoTls).unwrap();
        for (id, name) in [(first, "pg-memory-a"), (second, "pg-memory-b")] {
            admin
                .execute(
                    "INSERT INTO tenants (id,name) VALUES ($1,$2)",
                    &[&id, &name],
                )
                .unwrap();
        }

        let first_store = PostgresMemory::connect(
            &database_url,
            first,
            "/tmp/lethe-postgres-test-a",
            "/tmp/lethe-postgres-cache",
            2,
        )
        .unwrap();
        let second_store = PostgresMemory::connect(
            &database_url,
            second,
            "/tmp/lethe-postgres-test-b",
            "/tmp/lethe-postgres-cache",
            2,
        )
        .unwrap();

        assert!(first_store.blocks.get("human").unwrap().is_some());
        first_store
            .blocks
            .update("human", Some("Prefers graph APIs"), None)
            .unwrap();
        first_store
            .archival
            .add(
                "Outlook mail uses Microsoft Graph API",
                None,
                &["email".into()],
            )
            .unwrap();
        first_store
            .notes
            .create(
                "Graph authentication",
                "Use delegated OAuth for email.",
                &["email".into()],
                None,
            )
            .unwrap();
        first_store
            .messages
            .add(
                MessageRole::User,
                "Previously discussed Graph email access",
                None,
            )
            .unwrap();

        let recall = Hippocampus::new(HippocampusConfig {
            exclude_recent_conversations: 0,
            ..Default::default()
        })
        .recall(&first_store, "How should I access Graph email?", &[])
        .unwrap()
        .unwrap();
        assert!(recall.contains("Graph authentication"));
        assert!(recall.contains("Microsoft Graph API"));
        assert!(recall.contains("Previously discussed Graph email access"));

        assert_eq!(second_store.archival.count().unwrap(), 0);
        assert_eq!(second_store.messages.count().unwrap(), 0);
        assert!(second_store.notes.list_notes(None).unwrap().is_empty());
        assert_ne!(
            first_store.blocks.get("human").unwrap().unwrap().value,
            second_store.blocks.get("human").unwrap().unwrap().value
        );

        admin
            .execute(
                "DELETE FROM tenants WHERE id = ANY($1)",
                &[&vec![first, second]],
            )
            .unwrap();
    }
}
