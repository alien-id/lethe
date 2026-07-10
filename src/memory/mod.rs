mod codec;
mod db;
mod search;

pub mod archival;
pub mod backend;
pub mod blocks;
pub mod message_metadata;
pub mod messages;
pub mod notes;
#[cfg(feature = "postgres-memory")]
pub mod postgres;
pub mod recall;
pub mod semantic;
pub mod store;

pub use db::{MemoryDb, MemoryKind, MemoryRow, NewMemoryRow, ScoredRow};

pub use archival::{ArchivalEntry, ArchivalError, ArchivalMemory, ArchivalResult};
pub use backend::{ArchivalStorage, BlockStorage, MessageStorage, NoteStorage, TodoStorage};
pub use blocks::{
    BlockManager, BlockMetadata, DEFAULT_BLOCK_LIMIT, MemoryBlock, MemoryError, MemoryResult,
};
pub use message_metadata::{
    MESSAGE_KIND_KEY, MessageKind, MessageMetadata, MessageVisibility, SOURCE_KEY, VISIBILITY_KEY,
    annotate_map, annotate_value, metadata_value,
};
pub use messages::{
    MessageHistory, MessageHistoryError, MessageHistoryResult, MessageRole, StoredMessage,
};
pub use notes::{
    NoteError, NoteMetadata, NoteResult, NoteSearchResult, NoteStore, NoteSummary, normalize_tags,
    parse_frontmatter, render_frontmatter, slugify,
};
pub use recall::{
    Hippocampus, HippocampusConfig, HippocampusError, HippocampusResult, build_query,
};
pub use semantic::{
    EmbeddingEngine, LEGACY_EMBEDDING_DIMENSIONS, LEGACY_EMBEDDING_MODEL, SemanticDocument,
    SemanticHit, SemanticIndexConfig, TextEmbedder,
};
pub use store::{MemoryStats, MemoryStore, MemoryStoreError, MemoryStoreResult};
