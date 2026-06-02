use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use thiserror::Error;
use uuid::Uuid;

use crate::config::Settings;

#[derive(Debug, Error)]
pub enum MiniAppError {
    #[error("mini app storage error: {0}")]
    Io(#[from] std::io::Error),
    #[error("mini app metadata error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsafe Mini App HTML: {0}")]
    UnsafeHtml(String),
    #[error("mini app artifact not found: {0}")]
    NotFound(String),
    #[error("invalid Mini App generation JSON: {0}")]
    InvalidGeneration(String),
    #[error("mini app configuration error: {0}")]
    Config(String),
}

pub type MiniAppResult<T> = Result<T, MiniAppError>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MiniAppLlmArtifact {
    pub title: String,
    pub slug_hint: String,
    pub summary: String,
    pub html: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MiniAppArtifact {
    pub metadata: MiniAppMetadata,
    pub access_token: String,
    pub html: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StaticMiniAppArtifact {
    pub slug: String,
    pub path: PathBuf,
    pub url: String,
}

pub const STATIC_MINI_APPS_DIR: &str = "mini_apps";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MiniAppMetadata {
    pub title: String,
    pub summary: String,
    pub slug: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telegram_chat_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telegram_user_id: Option<i64>,
    pub token_hash: String,
    pub artifact_path: PathBuf,
    pub metadata_path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct MiniAppCreateRequest {
    pub title: String,
    pub slug_hint: String,
    pub summary: String,
    pub html: String,
    pub telegram_chat_id: Option<i64>,
    pub telegram_user_id: Option<i64>,
}

#[derive(Clone, Debug, Default)]
pub struct MiniAppGenerationRequest {
    pub user_request: String,
    pub telegram_chat_id: Option<i64>,
    pub telegram_user_id: Option<i64>,
    pub prior_artifact_context: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MiniAppGenerationResponse {
    pub title: String,
    pub summary: String,
    pub slug: String,
    pub access_token: String,
    pub url: String,
    pub metadata: MiniAppMetadata,
}

#[derive(Clone, Debug)]
pub struct MiniAppStore {
    root: PathBuf,
}

impl MiniAppStore {
    pub fn from_settings(settings: &Settings) -> Self {
        Self::new(settings.paths.lethe_home.join("data").join("mini-apps"))
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn create_artifact(&self, request: MiniAppCreateRequest) -> MiniAppResult<MiniAppArtifact> {
        validate_html_safety(&request.html)?;
        fs::create_dir_all(&self.root)?;

        let base_slug = slugify(if request.slug_hint.trim().is_empty() {
            &request.title
        } else {
            &request.slug_hint
        });
        let (slug, dir) = self.create_unique_artifact_dir(&base_slug)?;
        let artifact_path = dir.join("artifact.html");
        let metadata_path = dir.join("metadata.json");
        let access_token = generate_access_token();
        let now = Utc::now();
        let metadata = MiniAppMetadata {
            title: request.title.trim().to_string(),
            summary: request.summary.trim().to_string(),
            slug,
            created_at: now,
            updated_at: now,
            telegram_chat_id: request.telegram_chat_id,
            telegram_user_id: request.telegram_user_id,
            token_hash: token_hash(&access_token),
            artifact_path,
            metadata_path,
        };

        fs::write(&metadata.artifact_path, &request.html)?;
        write_metadata(&metadata)?;

        Ok(MiniAppArtifact {
            metadata,
            access_token,
            html: request.html,
        })
    }

    pub fn load_metadata(&self, slug: &str) -> MiniAppResult<MiniAppMetadata> {
        let path = self.metadata_path_for_slug(slug)?;
        let raw = fs::read_to_string(path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub fn verify_token(&self, slug: &str, token: &str) -> MiniAppResult<bool> {
        let metadata = self.load_metadata(slug)?;
        Ok(metadata.token_hash == token_hash(token))
    }

    pub fn read_artifact_html(&self, slug: &str) -> MiniAppResult<String> {
        let metadata = self.load_metadata(slug)?;
        Ok(fs::read_to_string(metadata.artifact_path)?)
    }

    pub fn overwrite_artifact_html(
        &self,
        slug: &str,
        html: &str,
    ) -> MiniAppResult<MiniAppMetadata> {
        validate_html_safety(html)?;
        let mut metadata = self.load_metadata(slug)?;
        fs::write(&metadata.artifact_path, html)?;
        metadata.updated_at = Utc::now();
        write_metadata(&metadata)?;
        Ok(metadata)
    }

    fn create_unique_artifact_dir(&self, base_slug: &str) -> MiniAppResult<(String, PathBuf)> {
        for _ in 0..32 {
            let suffix = Uuid::new_v4().simple().to_string()[..8].to_string();
            let slug = format!("{base_slug}-{suffix}");
            let dir = self.root.join(&slug);
            match fs::create_dir(&dir) {
                Ok(()) => return Ok((slug, dir)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(MiniAppError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate collision-free mini app slug",
        )))
    }

    fn metadata_path_for_slug(&self, slug: &str) -> MiniAppResult<PathBuf> {
        let slug = slug.trim();
        if slug.is_empty() || slug.contains('/') || slug.contains('\\') || slug.contains("..") {
            return Err(MiniAppError::NotFound(slug.to_string()));
        }
        let path = self.root.join(slug).join("metadata.json");
        if !path.is_file() {
            return Err(MiniAppError::NotFound(slug.to_string()));
        }
        Ok(path)
    }
}

pub fn static_mini_app_slug(app_name: &str) -> String {
    slugify(app_name)
}

pub fn static_mini_app_root(settings: &Settings) -> PathBuf {
    settings.paths.workspace_dir.join(STATIC_MINI_APPS_DIR)
}

pub fn static_mini_app_index_path(workspace_dir: &Path, app_name: &str) -> PathBuf {
    workspace_dir
        .join(STATIC_MINI_APPS_DIR)
        .join(static_mini_app_slug(app_name))
        .join("index.html")
}

pub fn static_mini_app_public_url(public_base_url: &str, app_name: &str) -> String {
    format!(
        "{}/mini/{}",
        public_base_url.trim().trim_end_matches('/'),
        static_mini_app_slug(app_name)
    )
}

pub fn publish_static_mini_app(
    workspace_dir: &Path,
    public_base_url: &str,
    app_name: &str,
    html: &str,
) -> MiniAppResult<StaticMiniAppArtifact> {
    validate_html_safety(html)?;
    let slug = static_mini_app_slug(app_name);
    let path = static_mini_app_index_path(workspace_dir, &slug);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, html)?;
    Ok(StaticMiniAppArtifact {
        slug: slug.clone(),
        path,
        url: static_mini_app_public_url(public_base_url, &slug),
    })
}

pub fn parse_llm_artifact_json(raw: &str) -> MiniAppResult<MiniAppLlmArtifact> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| MiniAppError::InvalidGeneration(error.to_string()))?;
    let object = value.as_object().ok_or_else(|| {
        MiniAppError::InvalidGeneration("response must be a JSON object".to_string())
    })?;
    let required = |key: &str| -> MiniAppResult<String> {
        object
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| MiniAppError::InvalidGeneration(format!("missing required key `{key}`")))
    };
    let artifact = MiniAppLlmArtifact {
        title: required("title")?,
        slug_hint: required("slug_hint")?,
        summary: required("summary")?,
        html: required("html")?,
    };
    validate_html_safety(&artifact.html)?;
    Ok(artifact)
}

pub fn artifact_url(public_base_url: &str, slug: &str, token: &str) -> String {
    format!(
        "{}/mini-apps/{}?token={}",
        public_base_url.trim().trim_end_matches('/'),
        slug,
        token
    )
}

type HmacSha256 = Hmac<Sha256>;

pub fn validate_telegram_init_data(
    init_data: &str,
    bot_token: &str,
    max_age_seconds: i64,
) -> MiniAppResult<()> {
    let fields = parse_init_data(init_data)?;
    let presented_hash = fields
        .get("hash")
        .ok_or_else(|| MiniAppError::InvalidGeneration("initData hash is missing".to_string()))?;
    let auth_date = fields
        .get("auth_date")
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or_else(|| {
            MiniAppError::InvalidGeneration("initData auth_date is missing".to_string())
        })?;
    let now = Utc::now().timestamp();
    if auth_date > now + 60 || now.saturating_sub(auth_date) > max_age_seconds {
        return Err(MiniAppError::InvalidGeneration(
            "initData auth_date is stale".to_string(),
        ));
    }
    let data_check_string = telegram_data_check_string(&fields);
    let expected_hash = telegram_init_data_hash(bot_token, &data_check_string)?;
    if !constant_time_eq_hex(&expected_hash, presented_hash) {
        return Err(MiniAppError::InvalidGeneration(
            "initData hash is invalid".to_string(),
        ));
    }
    Ok(())
}

pub fn telegram_init_data_user_id(init_data: &str) -> Option<i64> {
    let fields = parse_init_data(init_data).ok()?;
    let user = fields.get("user")?;
    serde_json::from_str::<Value>(user)
        .ok()?
        .get("id")?
        .as_i64()
}

pub fn telegram_data_check_string(fields: &BTreeMap<String, String>) -> String {
    fields
        .iter()
        .filter(|(key, _)| key.as_str() != "hash")
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn telegram_init_data_hash(bot_token: &str, data_check_string: &str) -> MiniAppResult<String> {
    let mut secret_mac = HmacSha256::new_from_slice(b"WebAppData")
        .map_err(|error| MiniAppError::InvalidGeneration(error.to_string()))?;
    secret_mac.update(bot_token.as_bytes());
    let secret = secret_mac.finalize().into_bytes();
    let mut mac = HmacSha256::new_from_slice(&secret)
        .map_err(|error| MiniAppError::InvalidGeneration(error.to_string()))?;
    mac.update(data_check_string.as_bytes());
    Ok(hex_string(&mac.finalize().into_bytes()))
}

fn parse_init_data(init_data: &str) -> MiniAppResult<BTreeMap<String, String>> {
    let mut fields = BTreeMap::new();
    for pair in init_data.split('&') {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| MiniAppError::InvalidGeneration("initData is malformed".to_string()))?;
        fields.insert(percent_decode(key)?, percent_decode(value)?);
    }
    Ok(fields)
}

fn percent_decode(value: &str) -> MiniAppResult<String> {
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                    .map_err(|error| MiniAppError::InvalidGeneration(error.to_string()))?;
                let byte = u8::from_str_radix(hex, 16)
                    .map_err(|error| MiniAppError::InvalidGeneration(error.to_string()))?;
                out.push(byte);
                i += 3;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|error| MiniAppError::InvalidGeneration(error.to_string()))
}

fn constant_time_eq_hex(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

fn hex_string(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn validate_html_safety(html: &str) -> MiniAppResult<()> {
    let lowered = html.to_ascii_lowercase();
    let forbidden = [
        ("http://", "external http URLs are not allowed"),
        ("https://", "external https URLs are not allowed"),
        ("fetch(", "fetch() is not allowed"),
        ("xmlhttprequest", "XMLHttpRequest is not allowed"),
        ("<script src=", "external script sources are not allowed"),
        ("import(", "dynamic import() is not allowed"),
        (
            "navigator.sendbeacon",
            "navigator.sendBeacon is not allowed",
        ),
    ];
    for (pattern, message) in forbidden {
        if lowered.contains(pattern) {
            return Err(MiniAppError::UnsafeHtml(message.to_string()));
        }
    }
    Ok(())
}

pub fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            previous_dash = false;
        } else if !previous_dash && !slug.is_empty() {
            slug.push('-');
            previous_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "mini-app".to_string()
    } else {
        slug.chars()
            .take(48)
            .collect::<String>()
            .trim_end_matches('-')
            .to_string()
    }
}

pub fn token_hash(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    hex_string(&digest)
}

fn generate_access_token() -> String {
    format!("{}.{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn write_metadata(metadata: &MiniAppMetadata) -> MiniAppResult<()> {
    let raw = serde_json::to_string_pretty(metadata)?;
    fs::write(&metadata.metadata_path, raw)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_request(title: &str, html: &str) -> MiniAppCreateRequest {
        MiniAppCreateRequest {
            title: title.to_string(),
            slug_hint: title.to_string(),
            summary: "A compact test app".to_string(),
            html: html.to_string(),
            telegram_chat_id: Some(123),
            telegram_user_id: Some(456),
        }
    }

    #[test]
    fn mini_app_slug_generation_is_readable() {
        assert_eq!(
            slugify("Real-time CSS Gradient Builder!"),
            "real-time-css-gradient-builder"
        );
        assert_eq!(slugify("---"), "mini-app");
    }

    #[test]
    fn mini_app_store_creates_collision_safe_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MiniAppStore::new(tmp.path().join("mini-apps"));
        let html = "<html><style>body{}</style><script>let x = 1;</script></html>";

        let first = store
            .create_artifact(create_request("Gradient Builder", html))
            .unwrap();
        let second = store
            .create_artifact(create_request("Gradient Builder", html))
            .unwrap();

        assert_ne!(first.metadata.slug, second.metadata.slug);
        assert!(first.metadata.slug.starts_with("gradient-builder-"));
        assert!(first.metadata.artifact_path.is_file());
        assert!(first.metadata.metadata_path.is_file());
    }

    #[test]
    fn mini_app_store_hashes_token_and_verifies_access() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MiniAppStore::new(tmp.path().join("mini-apps"));
        let artifact = store
            .create_artifact(create_request(
                "Calculator",
                "<html><script>let total = 0;</script></html>",
            ))
            .unwrap();

        let metadata_raw = fs::read_to_string(&artifact.metadata.metadata_path).unwrap();
        assert!(!metadata_raw.contains(&artifact.access_token));
        assert_eq!(
            artifact.metadata.token_hash,
            token_hash(&artifact.access_token)
        );
        assert!(
            store
                .verify_token(&artifact.metadata.slug, &artifact.access_token)
                .unwrap()
        );
        assert!(
            !store
                .verify_token(&artifact.metadata.slug, "bad-token")
                .unwrap()
        );
    }

    #[test]
    fn mini_app_safety_rejects_network_and_dynamic_patterns() {
        for html in [
            "<img src=\"https://cdn.example/app.png\">",
            "<script>fetch('/x')</script>",
            "<script>new XMLHttpRequest()</script>",
            "<script src=\"app.js\"></script>",
            "<script>import('x')</script>",
            "<script>navigator.sendBeacon('/x')</script>",
        ] {
            assert!(validate_html_safety(html).is_err(), "{html}");
        }
    }

    #[test]
    fn mini_app_safety_accepts_inline_html_css_js() {
        let html = r#"<!doctype html><html><head><style>body{font-family:sans-serif}</style></head><body><input id="x"><script>document.body.dataset.ready = "1";</script></body></html>"#;
        validate_html_safety(html).unwrap();
    }

    #[test]
    fn mini_app_store_overwrites_refinement_without_changing_token_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MiniAppStore::new(tmp.path().join("mini-apps"));
        let artifact = store
            .create_artifact(create_request(
                "Playground",
                "<html><script>let theme='light';</script></html>",
            ))
            .unwrap();
        let original_hash = artifact.metadata.token_hash.clone();

        let updated = store
            .overwrite_artifact_html(
                &artifact.metadata.slug,
                "<html><script>let theme='dark';</script></html>",
            )
            .unwrap();

        assert_eq!(updated.slug, artifact.metadata.slug);
        assert_eq!(updated.token_hash, original_hash);
        assert!(
            store
                .read_artifact_html(&updated.slug)
                .unwrap()
                .contains("dark")
        );
    }

    #[test]
    fn mini_app_generation_json_parser_accepts_valid_output() {
        let raw = r#"{"title":"Gradient Builder","slug_hint":"gradient builder","summary":"Build gradients.","html":"<html><script>let ok = true;</script></html>"}"#;
        let artifact = parse_llm_artifact_json(raw).unwrap();
        assert_eq!(artifact.title, "Gradient Builder");
        assert!(artifact.html.contains("ok"));
    }

    #[test]
    fn mini_app_generation_json_parser_rejects_malformed_and_missing_keys() {
        assert!(parse_llm_artifact_json("not json").is_err());
        let missing = r#"{"title":"Only title","slug_hint":"x","summary":"y"}"#;
        let error = parse_llm_artifact_json(missing).unwrap_err();
        assert!(matches!(error, MiniAppError::InvalidGeneration(_)));
    }

    #[test]
    fn mini_app_generation_json_parser_rejects_unsafe_html_before_storage() {
        let raw = r#"{"title":"Bad","slug_hint":"bad","summary":"Bad.","html":"<html><script>fetch('/x')</script></html>"}"#;
        let error = parse_llm_artifact_json(raw).unwrap_err();
        assert!(matches!(error, MiniAppError::UnsafeHtml(_)));
    }

    #[test]
    fn mini_app_artifact_url_uses_public_base_slug_and_token() {
        assert_eq!(
            artifact_url("https://mini.example.test/", "gradient-abc", "tok"),
            "https://mini.example.test/mini-apps/gradient-abc?token=tok"
        );
    }

    #[test]
    fn static_mini_app_helpers_use_workspace_index_html() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let path = static_mini_app_index_path(&workspace, "Calculator App");
        assert_eq!(
            path,
            workspace.join("mini_apps").join("calculator-app").join("index.html")
        );
        assert_eq!(
            static_mini_app_public_url("https://mini.example.test/", "Calculator App"),
            "https://mini.example.test/mini/calculator-app"
        );
    }

    #[test]
    fn publish_static_mini_app_writes_self_contained_html() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let published = publish_static_mini_app(
            &workspace,
            "https://mini.example.test",
            "Calculator",
            "<!doctype html><html><body><button>1</button></body></html>",
        )
        .unwrap();
        assert_eq!(published.slug, "calculator");
        assert_eq!(published.url, "https://mini.example.test/mini/calculator");
        assert_eq!(published.path, workspace.join("mini_apps/calculator/index.html"));
        assert!(published.path.is_file());
        assert!(fs::read_to_string(&published.path).unwrap().contains("<button>1</button>"));
    }

    #[test]
    fn mini_app_telegram_data_check_string_sorts_keys_and_skips_hash() {
        let mut fields = BTreeMap::new();
        fields.insert("query_id".to_string(), "abc".to_string());
        fields.insert("hash".to_string(), "ignored".to_string());
        fields.insert("auth_date".to_string(), "1700000000".to_string());
        assert_eq!(
            telegram_data_check_string(&fields),
            "auth_date=1700000000\nquery_id=abc"
        );
    }

    #[test]
    fn mini_app_telegram_init_data_accepts_valid_hash_and_rejects_bad_or_stale() {
        let bot_token = "123456:ABC-DEF";
        let auth_date = Utc::now().timestamp();
        let user = "%7B%22id%22%3A456%7D";
        let data_check_string = format!("auth_date={auth_date}\nquery_id=q1\nuser={{\"id\":456}}");
        let hash = telegram_init_data_hash(bot_token, &data_check_string).unwrap();
        let init_data = format!("query_id=q1&user={user}&auth_date={auth_date}&hash={hash}");

        validate_telegram_init_data(&init_data, bot_token, 60).unwrap();
        assert_eq!(telegram_init_data_user_id(&init_data), Some(456));

        let bad = init_data.replace(&hash, "00");
        assert!(validate_telegram_init_data(&bad, bot_token, 60).is_err());

        let stale_auth_date = auth_date - 120;
        let stale_data_check_string =
            format!("auth_date={stale_auth_date}\nquery_id=q1\nuser={{\"id\":456}}");
        let stale_hash = telegram_init_data_hash(bot_token, &stale_data_check_string).unwrap();
        let stale =
            format!("query_id=q1&user={user}&auth_date={stale_auth_date}&hash={stale_hash}");
        assert!(validate_telegram_init_data(&stale, bot_token, 60).is_err());
    }

    #[test]
    fn mini_app_store_rejects_unsafe_refinement_before_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MiniAppStore::new(tmp.path().join("mini-apps"));
        let artifact = store
            .create_artifact(create_request(
                "Playground",
                "<html><script>let theme='light';</script></html>",
            ))
            .unwrap();

        let error = store
            .overwrite_artifact_html(
                &artifact.metadata.slug,
                "<html><script>fetch('/x')</script></html>",
            )
            .unwrap_err();
        assert!(matches!(error, MiniAppError::UnsafeHtml(_)));
        assert!(
            store
                .read_artifact_html(&artifact.metadata.slug)
                .unwrap()
                .contains("light")
        );
    }
}
