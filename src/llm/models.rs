use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

const EMBEDDED_MODEL_CATALOG: &str = include_str!("../../config/model_catalog.json");
const EMBEDDED_CONTEXT_LIMITS: &str = include_str!("../../config/model_context_limits.json");

pub type ModelCatalog = BTreeMap<String, BTreeMap<String, Vec<ModelEntry>>>;

/// Catalog entry for a single model. Deserialized from JSON arrays in
/// `config/model_catalog.json`. Existing entries are 3-element arrays
/// `[name, model_id, price]` (protocol defaults to `""`). New entries
/// may include a 4th element `[name, model_id, price, protocol]` to
/// declare which wire protocol the model uses ("openai" or "anthropic").
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelEntry(pub String, pub String, pub String, pub String);

impl ModelEntry {
    pub fn name(&self) -> &str {
        &self.0
    }

    pub fn model_id(&self) -> &str {
        &self.1
    }

    pub fn price(&self) -> &str {
        &self.2
    }

    /// Wire protocol for this model: `"openai"`, `"anthropic"`, or `""`
    /// (use provider default). Only meaningful for multi-protocol providers
    /// like OpenCode Go; single-protocol providers always return `""`.
    pub fn protocol(&self) -> &str {
        &self.3
    }
}

impl<'de> Deserialize<'de> for ModelEntry {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let arr: Vec<String> = Vec::deserialize(deserializer)?;
        match arr.as_slice() {
            [name, model_id, price] => Ok(ModelEntry(
                name.clone(),
                model_id.clone(),
                price.clone(),
                String::new(),
            )),
            [name, model_id, price, protocol] => Ok(ModelEntry(
                name.clone(),
                model_id.clone(),
                price.clone(),
                protocol.clone(),
            )),
            _ => Err(serde::de::Error::custom(
                "expected 3 or 4 elements in ModelEntry array",
            )),
        }
    }
}

impl Serialize for ModelEntry {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.3.is_empty() {
            (&[&self.0, &self.1, &self.2]).serialize(serializer)
        } else {
            (&[&self.0, &self.1, &self.2, &self.3]).serialize(serializer)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub provider: String,
    pub label: String,
    pub auth: String,
}

static MODEL_CATALOG: OnceLock<ModelCatalog> = OnceLock::new();

pub fn model_catalog() -> &'static ModelCatalog {
    MODEL_CATALOG.get_or_init(load_embedded_catalog)
}

pub fn available_providers() -> Vec<ProviderInfo> {
    available_providers_with(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
}

pub fn available_providers_with(mut env_has: impl FnMut(&str) -> bool) -> Vec<ProviderInfo> {
    let catalog = model_catalog();
    provider_auth_options()
        .iter()
        .filter(|(provider, _)| catalog.contains_key(*provider))
        .flat_map(|(provider, auth_options)| {
            auth_options
                .iter()
                .filter(|(env_var, _)| env_has(env_var))
                .map(|(_, auth)| ProviderInfo {
                    provider: (*provider).to_string(),
                    label: provider_label(provider, auth),
                    auth: (*auth).to_string(),
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Per-model context window (tokens), as declared in
/// `config/model_context_limits.json`. Returns `None` for unknown model ids
/// — callers should fall back to a configured env default.
pub fn context_limit_for_model(model_id: &str) -> Option<u64> {
    static CONTEXT_LIMITS: OnceLock<BTreeMap<String, u64>> = OnceLock::new();
    let map = CONTEXT_LIMITS.get_or_init(|| {
        let raw = serde_json::from_str::<serde_json::Value>(EMBEDDED_CONTEXT_LIMITS).ok();
        let Some(serde_json::Value::Object(mut object)) = raw else {
            return BTreeMap::new();
        };
        object.retain(|key, _| !key.starts_with('_'));
        object
            .into_iter()
            .filter_map(|(key, value)| value.as_u64().map(|tokens| (key, tokens)))
            .collect()
    });
    let key = model_id.trim();
    map.get(key).copied()
}

/// OpenRouter model ids are namespaced (`openrouter/<vendor>/<model>`); prepend
/// the prefix when a bare id is given for the OpenRouter provider so a short id
/// like `moonshotai/kimi-k2` still resolves. Ids for other providers,
/// already-prefixed OpenRouter ids, and empty input pass through unchanged.
pub fn normalize_model_id(provider: &str, id: &str) -> String {
    let trimmed = id.trim();
    if provider == "openrouter" && !trimmed.is_empty() && !trimmed.starts_with("openrouter/") {
        format!("openrouter/{trimmed}")
    } else if provider == "opencode-go"
        && !trimmed.is_empty()
        && !trimmed.starts_with("opencode-go/")
    {
        format!("opencode-go/{trimmed}")
    } else {
        id.to_string()
    }
}

pub fn provider_for_model(model_id: &str) -> Option<&'static str> {
    let model_id = model_id.trim();
    if model_id.is_empty() {
        return None;
    }
    for (provider, groups) in model_catalog() {
        for entries in groups.values() {
            if entries.iter().any(|entry| entry.model_id() == model_id) {
                return Some(provider.as_str());
            }
        }
    }
    provider_for_model_fallback(model_id)
}

fn load_embedded_catalog() -> ModelCatalog {
    let raw = serde_json::from_str::<serde_json::Value>(EMBEDDED_MODEL_CATALOG).ok();
    let Some(serde_json::Value::Object(mut object)) = raw else {
        return ModelCatalog::new();
    };
    object.retain(|key, _| !key.starts_with('_'));
    serde_json::from_value(serde_json::Value::Object(object)).unwrap_or_default()
}

/// Look up the wire protocol for a model id in the catalog. Returns `"openai"`,
/// `"anthropic"`, or `""` (use provider default). Used by the router to select
/// the correct genai adapter for multi-protocol providers like OpenCode Go.
pub fn protocol_for_model(model_id: &str) -> &'static str {
    let model_id = model_id.trim();
    if model_id.is_empty() {
        return "";
    }
    for (_provider, groups) in model_catalog() {
        for entries in groups.values() {
            if let Some(entry) = entries.iter().find(|entry| entry.model_id() == model_id) {
                return entry.protocol();
            }
        }
    }
    ""
}

fn provider_for_model_fallback(model_id: &str) -> Option<&'static str> {
    let lower = model_id.to_ascii_lowercase();
    if lower.starts_with("openrouter/") {
        Some("openrouter")
    } else if lower.starts_with("opencode-go/") {
        Some("opencode-go")
    } else if lower.contains("claude") {
        Some("anthropic")
    } else if lower.contains("gpt") {
        Some("openai")
    } else {
        None
    }
}

fn provider_auth_options() -> &'static [(&'static str, &'static [(&'static str, &'static str)])] {
    &[
        ("openrouter", &[("OPENROUTER_API_KEY", "API")]),
        ("anthropic", &[("ANTHROPIC_API_KEY", "API")]),
        ("openai", &[("OPENAI_API_KEY", "API")]),
        ("opencode-go", &[("OPENCODE_GO_API_KEY", "API")]),
    ]
}

fn provider_label(provider: &str, auth: &str) -> String {
    let base = match provider {
        "openrouter" => "OpenRouter",
        "anthropic" => "Anthropic",
        "openai" => "OpenAI",
        "opencode-go" => "OpenCode Go",
        _ => provider,
    };
    if provider == "openrouter" {
        return base.to_string();
    }
    let suffix = match auth {
        "API" => "API key",
        _ => auth,
    };
    format!("{base} ({suffix})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_strips_metadata_and_loads_models() {
        let catalog = model_catalog();

        assert!(!catalog.contains_key("_updated"));
        assert!(catalog.contains_key("openrouter"));
        assert!(
            catalog["openrouter"]["main"]
                .iter()
                .any(|entry| entry.model_id().starts_with("openrouter/"))
        );
    }

    #[test]
    fn normalize_prefixes_bare_openrouter_ids_once() {
        assert_eq!(
            normalize_model_id("openrouter", "moonshotai/kimi-k2.6"),
            "openrouter/moonshotai/kimi-k2.6"
        );
        assert_eq!(
            normalize_model_id("openrouter", "openrouter/anthropic/claude-opus-4.7"),
            "openrouter/anthropic/claude-opus-4.7"
        );
        assert_eq!(
            normalize_model_id("anthropic", "claude-opus-4-8"),
            "claude-opus-4-8"
        );
        // Empty input must not become a bare "openrouter/" prefix.
        assert_eq!(normalize_model_id("openrouter", "  "), "  ");
    }

    #[test]
    fn provider_lookup_uses_catalog_then_fallbacks() {
        assert_eq!(provider_for_model("claude-haiku-4-5"), Some("anthropic"));
        assert_eq!(
            provider_for_model("openrouter/openai/gpt-5.4-nano"),
            Some("openrouter")
        );
        assert_eq!(provider_for_model("gpt-future"), Some("openai"));
        assert_eq!(provider_for_model("unknown-model"), None);
    }

    #[test]
    fn available_providers_follow_configured_auth_order() {
        let available =
            available_providers_with(|key| matches!(key, "ANTHROPIC_API_KEY" | "OPENAI_API_KEY"));

        assert_eq!(
            available,
            vec![
                ProviderInfo {
                    provider: "anthropic".to_string(),
                    label: "Anthropic (API key)".to_string(),
                    auth: "API".to_string(),
                },
                ProviderInfo {
                    provider: "openai".to_string(),
                    label: "OpenAI (API key)".to_string(),
                    auth: "API".to_string(),
                },
            ]
        );
    }

    #[test]
    fn model_entry_deserialize_three_elements() {
        let entry: ModelEntry = serde_json::from_str(
            r#"["Kimi K2.6", "openrouter/moonshotai/kimi-k2.6", "$0.60/$2.80"]"#,
        )
        .unwrap();
        assert_eq!(entry.name(), "Kimi K2.6");
        assert_eq!(entry.model_id(), "openrouter/moonshotai/kimi-k2.6");
        assert_eq!(entry.price(), "$0.60/$2.80");
        assert_eq!(entry.protocol(), "");
    }

    #[test]
    fn model_entry_deserialize_four_elements() {
        let entry: ModelEntry = serde_json::from_str(
            r#"["Kimi K2.6", "opencode-go/kimi-k2.6", "$0.60/$2.80", "openai"]"#,
        )
        .unwrap();
        assert_eq!(entry.name(), "Kimi K2.6");
        assert_eq!(entry.model_id(), "opencode-go/kimi-k2.6");
        assert_eq!(entry.price(), "$0.60/$2.80");
        assert_eq!(entry.protocol(), "openai");
    }

    #[test]
    fn model_entry_serialize_three_when_empty_protocol() {
        let entry = ModelEntry("Test".into(), "test-id".into(), "$1".into(), String::new());
        let json = serde_json::to_string(&entry).unwrap();
        assert_eq!(json, r#"["Test","test-id","$1"]"#);
    }

    #[test]
    fn model_entry_serialize_four_when_protocol_set() {
        let entry = ModelEntry(
            "Test".into(),
            "test-id".into(),
            "$1".into(),
            "openai".into(),
        );
        let json = serde_json::to_string(&entry).unwrap();
        assert_eq!(json, r#"["Test","test-id","$1","openai"]"#);
    }

    #[test]
    fn normalize_prefixes_bare_opencode_go_ids_once() {
        assert_eq!(
            normalize_model_id("opencode-go", "kimi-k2.6"),
            "opencode-go/kimi-k2.6"
        );
        assert_eq!(
            normalize_model_id("opencode-go", "opencode-go/kimi-k2.6"),
            "opencode-go/kimi-k2.6"
        );
        assert_eq!(normalize_model_id("opencode-go", "  "), "  ");
    }

    #[test]
    fn provider_lookup_finds_opencode_go_prefix() {
        assert_eq!(
            provider_for_model("opencode-go/kimi-k2.6"),
            Some("opencode-go")
        );
        assert_eq!(provider_for_model("kimi-k2.6"), None);
    }

    #[test]
    fn protocol_for_model_returns_wire_protocol() {
        let openai_model = ModelEntry(
            "Kimi K2.6".into(),
            "opencode-go/kimi-k2.6".into(),
            "$0.60/$2.80".into(),
            "openai".into(),
        );
        assert_eq!(openai_model.protocol(), "openai");

        let anthropic_model = ModelEntry(
            "Qwen3.7 Max".into(),
            "opencode-go/qwen3.7-max".into(),
            "$0.50/$2".into(),
            "anthropic".into(),
        );
        assert_eq!(anthropic_model.protocol(), "anthropic");

        let no_protocol = ModelEntry(
            "Claude Opus 4.7".into(),
            "claude-opus-4-7".into(),
            "$5/$25".into(),
            String::new(),
        );
        assert_eq!(no_protocol.protocol(), "");
    }
}
