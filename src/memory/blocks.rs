use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_BLOCK_LIMIT: usize = 20_000;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("invalid block label: {0}")]
    InvalidLabel(String),
    #[error("block already exists: {0}")]
    AlreadyExists(String),
    #[error("block not found: {0}")]
    NotFound(String),
    #[error("block is read-only: {0}")]
    ReadOnly(String),
    #[error("value length {actual} exceeds limit {limit}")]
    LimitExceeded { actual: usize, limit: usize },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("storage backend error: {0}")]
    Backend(String),
}

pub type MemoryResult<T> = Result<T, MemoryError>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BlockMetadata {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub hidden: bool,
    /// Stable blocks (user profile etc.) render in their own section ahead of
    /// volatile blocks so they cache better in the system prompt.
    #[serde(default)]
    pub stable: bool,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

impl Default for BlockMetadata {
    fn default() -> Self {
        Self {
            label: String::new(),
            description: String::new(),
            limit: DEFAULT_BLOCK_LIMIT,
            read_only: false,
            hidden: false,
            stable: false,
            created_at: None,
            updated_at: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MemoryBlock {
    pub label: String,
    pub value: String,
    pub description: String,
    pub limit: usize,
    pub read_only: bool,
    pub hidden: bool,
    #[serde(default)]
    pub stable: bool,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug)]
pub struct BlockManager {
    blocks_dir: PathBuf,
}

impl BlockManager {
    pub fn new(blocks_dir: impl Into<PathBuf>) -> MemoryResult<Self> {
        let blocks_dir = blocks_dir.into();
        fs::create_dir_all(&blocks_dir)?;
        Ok(Self { blocks_dir })
    }

    pub fn init_embedded_defaults(&self) -> MemoryResult<()> {
        self.write_seed_if_missing(
            "identity",
            embedded_block("identity").unwrap(),
            embedded_meta("identity"),
        )?;
        self.write_seed_if_missing(
            "human",
            embedded_block("human").unwrap(),
            embedded_meta("human"),
        )?;
        self.write_seed_if_missing("project", embedded_block("project").unwrap(), None)?;
        // The conversation_summary block is maintained automatically by the
        // summarizer when history overflows the live window. Seeded empty
        // and hidden so it doesn't clutter memory_list.
        self.write_seed_if_missing(
            "conversation_summary",
            embedded_block("conversation_summary").unwrap_or(""),
            embedded_meta("conversation_summary"),
        )?;
        // Backfill: legacy installs may already have a `human` block without the
        // `stable: true` marker. Patch its metadata in place so the system prompt
        // continues to place it in the stable section.
        self.mark_block_stable_if_missing("human")?;
        Ok(())
    }

    fn mark_block_stable_if_missing(&self, label: &str) -> MemoryResult<()> {
        let path = self.meta_path(label);
        if !path.exists() {
            return Ok(());
        }
        let mut meta = self.load_meta(label)?;
        if meta.stable {
            return Ok(());
        }
        meta.stable = true;
        self.save_meta(label, &meta)?;
        Ok(())
    }

    pub fn create(
        &self,
        label: &str,
        value: &str,
        description: &str,
        limit: usize,
        read_only: bool,
        hidden: bool,
    ) -> MemoryResult<String> {
        validate_label(label)?;
        let block_path = self.block_path(label);
        if block_path.exists() {
            return Err(MemoryError::AlreadyExists(label.to_string()));
        }
        enforce_limit(value, limit)?;

        fs::write(block_path, value)?;
        let now = Utc::now();
        self.save_meta(
            label,
            &BlockMetadata {
                label: label.to_string(),
                description: description.to_string(),
                limit,
                read_only,
                hidden,
                stable: false,
                created_at: Some(now),
                updated_at: Some(now),
            },
        )?;
        Ok(label.to_string())
    }

    pub fn get(&self, label: &str) -> MemoryResult<Option<MemoryBlock>> {
        validate_label(label)?;
        let block_path = self.block_path(label);
        if !block_path.exists() {
            return Ok(None);
        }

        let value = fs::read_to_string(block_path)?;
        let meta = self.load_meta(label)?;
        Ok(Some(MemoryBlock {
            label: label.to_string(),
            value,
            description: meta.description,
            limit: meta.limit,
            read_only: meta.read_only,
            hidden: meta.hidden,
            stable: meta.stable,
            created_at: meta.created_at,
            updated_at: meta.updated_at,
        }))
    }

    pub fn update(
        &self,
        label: &str,
        value: Option<&str>,
        description: Option<&str>,
    ) -> MemoryResult<bool> {
        self.update_internal(label, value, description, false)
    }

    /// Like [`update`] but ignores the `read_only` flag. Used by the runtime
    /// to refresh blocks the model is not allowed to edit by hand
    /// (`conversation_summary`, future maintenance blocks).
    pub fn system_update(&self, label: &str, value: &str) -> MemoryResult<bool> {
        self.update_internal(label, Some(value), None, true)
    }

    fn update_internal(
        &self,
        label: &str,
        value: Option<&str>,
        description: Option<&str>,
        bypass_read_only: bool,
    ) -> MemoryResult<bool> {
        validate_label(label)?;
        let block_path = self.block_path(label);
        if !block_path.exists() {
            return Ok(false);
        }

        let mut meta = self.load_meta(label)?;
        if !bypass_read_only && meta.read_only && value.is_some() {
            return Err(MemoryError::ReadOnly(label.to_string()));
        }
        if let Some(value) = value {
            enforce_limit(value, meta.limit)?;
            fs::write(&block_path, value)?;
        }
        if let Some(description) = description {
            meta.description = description.to_string();
        }
        if meta.created_at.is_none() {
            meta.created_at = Some(Utc::now());
        }
        meta.updated_at = Some(Utc::now());
        self.save_meta(label, &meta)?;
        Ok(true)
    }

    pub fn append(&self, label: &str, text: &str) -> MemoryResult<bool> {
        let Some(block) = self.get(label)? else {
            return Ok(false);
        };
        let new_value = format!("{}{}", block.value, text);
        self.update(label, Some(&new_value), None)
    }

    pub fn str_replace(&self, label: &str, old: &str, new: &str) -> MemoryResult<bool> {
        let Some(block) = self.get(label)? else {
            return Err(MemoryError::NotFound(label.to_string()));
        };
        if !block.value.contains(old) {
            return Ok(false);
        }
        let new_value = block.value.replacen(old, new, 1);
        self.update(label, Some(&new_value), None)
    }

    pub fn delete(&self, label: &str) -> MemoryResult<bool> {
        validate_label(label)?;
        let block_path = self.block_path(label);
        if !block_path.exists() {
            return Ok(false);
        }
        fs::remove_file(block_path)?;
        let meta_path = self.meta_path(label);
        if meta_path.exists() {
            fs::remove_file(meta_path)?;
        }
        Ok(true)
    }

    pub fn list_blocks(&self, include_hidden: bool) -> MemoryResult<Vec<MemoryBlock>> {
        let mut blocks = Vec::new();
        for entry in fs::read_dir(&self.blocks_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            let Some(label) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if let Some(block) = self.get(label)?
                && (include_hidden || !block.hidden)
            {
                blocks.push(block);
            }
        }
        blocks.sort_by(|left, right| left.label.cmp(&right.label));
        Ok(blocks)
    }

    fn write_seed_if_missing(
        &self,
        label: &str,
        value: &str,
        meta_json: Option<&str>,
    ) -> MemoryResult<()> {
        validate_label(label)?;
        let block_path = self.block_path(label);
        if !block_path.exists() {
            fs::write(&block_path, value.trim())?;
        }

        let meta_path = self.meta_path(label);
        if !meta_path.exists() {
            let mut meta = meta_json
                .map(serde_json::from_str::<BlockMetadata>)
                .transpose()?
                .unwrap_or_default();
            if meta.label.is_empty() {
                meta.label = label.to_string();
            }
            if meta.limit == 0 {
                meta.limit = DEFAULT_BLOCK_LIMIT;
            }
            self.save_meta(label, &meta)?;
        }
        Ok(())
    }

    fn block_path(&self, label: &str) -> PathBuf {
        self.blocks_dir.join(format!("{label}.md"))
    }

    fn meta_path(&self, label: &str) -> PathBuf {
        self.blocks_dir.join(format!("{label}.meta.json"))
    }

    fn load_meta(&self, label: &str) -> MemoryResult<BlockMetadata> {
        let path = self.meta_path(label);
        if !path.exists() {
            return Ok(BlockMetadata {
                label: label.to_string(),
                ..Default::default()
            });
        }
        let mut meta: BlockMetadata = serde_json::from_str(&fs::read_to_string(path)?)?;
        if meta.label.is_empty() {
            meta.label = label.to_string();
        }
        if meta.limit == 0 {
            meta.limit = DEFAULT_BLOCK_LIMIT;
        }
        Ok(meta)
    }

    fn save_meta(&self, label: &str, meta: &BlockMetadata) -> MemoryResult<()> {
        fs::write(self.meta_path(label), serde_json::to_string_pretty(meta)?)?;
        Ok(())
    }
}

fn default_limit() -> usize {
    DEFAULT_BLOCK_LIMIT
}

pub(crate) fn validate_label(label: &str) -> MemoryResult<()> {
    let valid = !label.is_empty()
        && label
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'));
    if valid {
        Ok(())
    } else {
        Err(MemoryError::InvalidLabel(label.to_string()))
    }
}

pub(crate) fn enforce_limit(value: &str, limit: usize) -> MemoryResult<()> {
    if value.len() <= limit {
        Ok(())
    } else {
        Err(MemoryError::LimitExceeded {
            actual: value.len(),
            limit,
        })
    }
}

pub(crate) const CONVERSATION_SUMMARY_LABEL: &str = "conversation_summary";

fn embedded_block(name: &str) -> Option<&'static str> {
    match name {
        "identity" => Some(include_str!("../../config/blocks/identity.md")),
        "human" => Some(include_str!("../../config/blocks/human.md")),
        "project" => Some(include_str!("../../config/blocks/project.md")),
        "conversation_summary" => Some(include_str!("../../config/blocks/conversation_summary.md")),
        _ => None,
    }
}

fn embedded_meta(name: &str) -> Option<&'static str> {
    match name {
        "identity" => Some(include_str!("../../config/blocks/identity.meta.json")),
        "human" => Some(include_str!("../../config/blocks/human.meta.json")),
        "conversation_summary" => Some(include_str!(
            "../../config/blocks/conversation_summary.meta.json"
        )),
        _ => None,
    }
}

pub(crate) fn embedded_defaults() -> MemoryResult<Vec<MemoryBlock>> {
    ["identity", "human", "project", CONVERSATION_SUMMARY_LABEL]
        .into_iter()
        .map(|label| {
            let mut metadata = embedded_meta(label)
                .map(serde_json::from_str::<BlockMetadata>)
                .transpose()?
                .unwrap_or_default();
            if metadata.label.is_empty() {
                metadata.label = label.to_string();
            }
            if metadata.limit == 0 {
                metadata.limit = DEFAULT_BLOCK_LIMIT;
            }
            Ok(MemoryBlock {
                label: label.to_string(),
                value: embedded_block(label).unwrap_or("").trim().to_string(),
                description: metadata.description,
                limit: metadata.limit,
                read_only: metadata.read_only,
                hidden: metadata.hidden,
                stable: metadata.stable,
                created_at: metadata.created_at,
                updated_at: metadata.updated_at,
            })
        })
        .collect()
}

#[allow(dead_code)]
fn is_inside(parent: &Path, child: &Path) -> bool {
    child.starts_with(parent)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn creates_updates_and_lists_blocks() {
        let tmp = tempdir().unwrap();
        let manager = BlockManager::new(tmp.path()).unwrap();

        manager
            .create("human", "initial", "about the user", 20, false, false)
            .unwrap();
        assert_eq!(manager.get("human").unwrap().unwrap().value, "initial");

        assert!(manager.update("human", Some("updated"), None).unwrap());
        let block = manager.get("human").unwrap().unwrap();
        assert_eq!(block.value, "updated");
        assert_eq!(block.description, "about the user");
        assert_eq!(manager.list_blocks(false).unwrap().len(), 1);
    }

    #[test]
    fn appends_and_replaces_block_text() {
        let tmp = tempdir().unwrap();
        let manager = BlockManager::new(tmp.path()).unwrap();
        manager
            .create("project", "Hello", "project context", 20, false, false)
            .unwrap();

        assert!(manager.append("project", " world").unwrap());
        assert_eq!(
            manager.get("project").unwrap().unwrap().value,
            "Hello world"
        );
        assert!(manager.str_replace("project", "world", "Rust").unwrap());
        assert_eq!(manager.get("project").unwrap().unwrap().value, "Hello Rust");
        assert!(!manager.str_replace("project", "missing", "value").unwrap());
    }

    #[test]
    fn rejects_path_traversal_labels() {
        let tmp = tempdir().unwrap();
        let manager = BlockManager::new(tmp.path()).unwrap();
        let error = manager
            .create("../secret", "", "", 10, false, false)
            .unwrap_err();
        assert!(matches!(error, MemoryError::InvalidLabel(_)));
    }

    #[test]
    fn enforces_limits_and_read_only() {
        let tmp = tempdir().unwrap();
        let manager = BlockManager::new(tmp.path()).unwrap();
        manager
            .create("identity", "abc", "", 3, true, false)
            .unwrap();

        let limit_error = manager
            .create("project", "abcd", "", 3, false, false)
            .unwrap_err();
        assert!(matches!(limit_error, MemoryError::LimitExceeded { .. }));

        let read_only = manager.update("identity", Some("xyz"), None).unwrap_err();
        assert!(matches!(read_only, MemoryError::ReadOnly(_)));
    }

    #[test]
    fn embedded_defaults_seed_core_memory_for_single_binary() {
        let tmp = tempdir().unwrap();
        let manager = BlockManager::new(tmp.path()).unwrap();
        manager.init_embedded_defaults().unwrap();

        let labels: Vec<String> = manager
            .list_blocks(true)
            .unwrap()
            .into_iter()
            .map(|block| block.label)
            .collect();
        assert_eq!(
            labels,
            vec!["conversation_summary", "human", "identity", "project"]
        );
        let identity = manager.get("identity").unwrap().unwrap();
        assert_eq!(identity.limit, 20_000);
        assert!(identity.description.contains("System prompt"));
    }
}
