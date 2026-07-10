use std::collections::hash_map::DefaultHasher;
#[cfg(feature = "fastembed")]
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::Path;
#[cfg(feature = "fastembed")]
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(feature = "fastembed")]
use std::sync::Mutex;

#[cfg(feature = "fastembed")]
use anyhow::{Context, bail};
use anyhow::{Result, anyhow};
#[cfg(feature = "fastembed")]
use fastembed::{
    EmbeddingModel, InitOptions, InitOptionsUserDefined, OutputKey, QuantizationMode,
    TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};
#[cfg(feature = "fastembed")]
use hf_hub::api::sync::ApiBuilder;
use serde::{Deserialize, Serialize};

pub const LEGACY_EMBEDDING_MODEL: &str = "Snowflake/snowflake-arctic-embed-m-v2.0";
pub const LEGACY_EMBEDDING_DIMENSIONS: usize = 768;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SemanticDocument {
    pub id: String,
    pub kind: String,
    pub text: String,
    pub source: String,
    pub tags: Vec<String>,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SemanticHit {
    pub id: String,
    pub kind: String,
    pub text: String,
    pub source: String,
    pub tags: Vec<String>,
    pub created_at: String,
    pub distance: f64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticIndexConfig {
    pub enabled: bool,
    pub provider: String,
    pub model: String,
}

impl SemanticIndexConfig {
    pub fn from_env() -> Self {
        Self {
            enabled: env_bool("LETHE_SEMANTIC_SEARCH_ENABLED", true),
            provider: env_string(
                "LETHE_EMBEDDING_PROVIDER",
                if cfg!(test) || !cfg!(feature = "fastembed") {
                    "hash"
                } else {
                    "fastembed"
                },
            ),
            model: env_string("LETHE_EMBEDDING_MODEL", LEGACY_EMBEDDING_MODEL),
        }
    }
}

#[derive(Clone)]
pub struct EmbeddingEngine {
    embedder: Arc<dyn TextEmbedder>,
}

impl std::fmt::Debug for EmbeddingEngine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EmbeddingEngine")
            .finish_non_exhaustive()
    }
}

impl EmbeddingEngine {
    pub fn from_env(cache_root: &Path) -> Self {
        let config = SemanticIndexConfig::from_env();
        Self::from_config(&config, cache_root)
    }

    pub fn from_config(config: &SemanticIndexConfig, cache_root: &Path) -> Self {
        Self {
            embedder: Arc::from(embedder_for_config(config, cache_root)),
        }
    }

    #[cfg(test)]
    pub fn with_hash_dimensions(dimensions: usize) -> Self {
        Self {
            embedder: Arc::new(HashTextEmbedder::new(dimensions)),
        }
    }

    pub fn embed_documents(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.embedder.embed_documents(texts)
    }

    pub fn embed_document(&self, text: &str) -> Result<Vec<f32>> {
        if text.trim().is_empty() {
            return Ok(vec![0.0; LEGACY_EMBEDDING_DIMENSIONS]);
        }
        self.embed_documents(&[text])?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("embedding provider returned no document vector"))
    }

    pub fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        if text.trim().is_empty() {
            return Ok(vec![0.0; LEGACY_EMBEDDING_DIMENSIONS]);
        }
        self.embedder.embed_query(text)
    }
}

pub trait TextEmbedder: Send + Sync {
    fn embed_documents(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    fn embed_query(&self, text: &str) -> Result<Vec<f32>>;
}

#[cfg(feature = "fastembed")]
struct FastEmbedTextEmbedder {
    model_name: String,
    cache_dir: PathBuf,
    model: Mutex<Option<TextEmbedding>>,
}

#[cfg(feature = "fastembed")]
impl FastEmbedTextEmbedder {
    fn new(model_name: impl Into<String>, cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            model_name: model_name.into(),
            cache_dir: cache_dir.into(),
            model: Mutex::new(None),
        }
    }

    fn model(&self) -> Result<std::sync::MutexGuard<'_, Option<TextEmbedding>>> {
        let mut guard = self
            .model
            .lock()
            .map_err(|error| anyhow!("embedding model lock poisoned: {error}"))?;
        if guard.is_none() {
            *guard = Some(if is_legacy_snowflake_model(&self.model_name) {
                legacy_snowflake_embedder(&self.cache_dir)?
            } else {
                let model = parse_fastembed_model(&self.model_name)?;
                // First-run download is ~150MB (ONNX runtime + model). Show
                // progress so users on a fresh machine see what's happening
                // instead of an apparent hang. Quiet on subsequent runs since
                // there's nothing to download.
                let options = InitOptions::new(model)
                    .with_cache_dir(self.cache_dir.clone())
                    .with_show_download_progress(true);
                TextEmbedding::try_new(options)?
            });
        }
        Ok(guard)
    }
}

#[cfg(feature = "fastembed")]
impl TextEmbedder for FastEmbedTextEmbedder {
    fn embed_documents(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let inputs = texts
            .iter()
            .map(|text| text.trim().to_string())
            .collect::<Vec<_>>();
        let mut guard = self.model()?;
        guard
            .as_mut()
            .ok_or_else(|| anyhow!("embedding model unavailable"))?
            .embed(inputs, None)
            .context("failed to embed documents")
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let mut guard = self.model()?;
        let mut embeddings = guard
            .as_mut()
            .ok_or_else(|| anyhow!("embedding model unavailable"))?
            .embed(vec![format!("query: {}", text.trim())], None)
            .context("failed to embed query")?;
        embeddings
            .pop()
            .ok_or_else(|| anyhow!("embedding provider returned no query vector"))
    }
}

#[derive(Debug)]
struct HashTextEmbedder {
    dimensions: usize,
}

impl HashTextEmbedder {
    fn new(dimensions: usize) -> Self {
        Self {
            dimensions: dimensions.max(8),
        }
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let mut vector = vec![0.0_f32; self.dimensions];
        for token in text
            .split(|ch: char| !ch.is_alphanumeric())
            .map(str::to_ascii_lowercase)
            .filter(|token| !token.is_empty())
        {
            let mut hasher = DefaultHasher::new();
            token.hash(&mut hasher);
            let hash = hasher.finish();
            let index = hash as usize % self.dimensions;
            let sign = if (hash >> 63) == 0 { 1.0 } else { -1.0 };
            vector[index] += sign;
        }
        normalize(&mut vector);
        vector
    }
}

impl TextEmbedder for HashTextEmbedder {
    fn embed_documents(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|text| self.embed(text)).collect())
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        Ok(self.embed(text))
    }
}

fn embedder_for_config(config: &SemanticIndexConfig, _root: &Path) -> Box<dyn TextEmbedder> {
    match config.provider.trim().to_ascii_lowercase().as_str() {
        "hash" => Box::new(HashTextEmbedder::new(LEGACY_EMBEDDING_DIMENSIONS)),
        #[cfg(feature = "fastembed")]
        _ => Box::new(FastEmbedTextEmbedder::new(
            config.model.clone(),
            _root.join("models"),
        )),
        #[cfg(not(feature = "fastembed"))]
        provider => {
            tracing::warn!(
                provider,
                "local FastEmbed support is not compiled in; using hash embeddings"
            );
            Box::new(HashTextEmbedder::new(LEGACY_EMBEDDING_DIMENSIONS))
        }
    }
}

#[cfg(feature = "fastembed")]
fn parse_fastembed_model(model: &str) -> Result<EmbeddingModel> {
    match model.trim().to_ascii_lowercase().as_str() {
        "" | "all-minilm-l6-v2" | "all_minilm_l6_v2" => Ok(EmbeddingModel::AllMiniLML6V2),
        "snowflake/snowflake-arctic-embed-m-v2.0"
        | "snowflake-arctic-embed-m-v2.0"
        | "snowflake/snowflake-arctic-embed-m"
        | "snowflake-arctic-embed-m" => Ok(EmbeddingModel::SnowflakeArcticEmbedMQ),
        "snowflake/snowflake-arctic-embed-m-fp32" | "snowflake-arctic-embed-m-fp32" => {
            Ok(EmbeddingModel::SnowflakeArcticEmbedM)
        }
        other => bail!("unsupported LETHE_EMBEDDING_MODEL for fastembed: {other}"),
    }
}

#[cfg(feature = "fastembed")]
fn is_legacy_snowflake_model(model: &str) -> bool {
    matches!(
        model.trim().to_ascii_lowercase().as_str(),
        "" | "snowflake/snowflake-arctic-embed-m-v2.0" | "snowflake-arctic-embed-m-v2.0"
    )
}

#[cfg(feature = "fastembed")]
fn legacy_snowflake_embedder(cache_dir: &Path) -> Result<TextEmbedding> {
    let repo = ApiBuilder::new()
        .with_cache_dir(cache_dir.to_path_buf())
        .build()?
        .model(LEGACY_EMBEDDING_MODEL.to_string());
    let tokenizer_files = TokenizerFiles {
        tokenizer_file: fs::read(repo.get("tokenizer.json")?)?,
        config_file: fs::read(repo.get("config.json")?)?,
        special_tokens_map_file: fs::read(repo.get("special_tokens_map.json")?)?,
        tokenizer_config_file: fs::read(repo.get("tokenizer_config.json")?)?,
    };
    let mut model = UserDefinedEmbeddingModel::new(
        fs::read(repo.get("onnx/model_int8.onnx")?)?,
        tokenizer_files,
    )
    .with_quantization(QuantizationMode::Dynamic);
    model.output_key = Some(OutputKey::ByName("sentence_embedding"));

    TextEmbedding::try_new_from_user_defined(
        model,
        InitOptionsUserDefined::new().with_max_length(512),
    )
}

fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in vector {
            *value /= norm;
        }
    }
}

fn env_string(name: &str, default: &str) -> String {
    std::env::var(name)
        .map(|value| value.trim().to_string())
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn env_bool(name: &str, default: bool) -> bool {
    std::env::var(name)
        .map(|value| match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => default,
        })
        .unwrap_or(default)
}
