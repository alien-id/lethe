use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use chrono::Local;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Total per-request timeout. The old `reqwest::blocking` client defaulted to
/// 30s; the async client defaults to none, which would let a stuck Exa call
/// hang the whole turn — so the old bound is kept explicitly.
const EXA_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct WebTools {
    cache_dir: PathBuf,
}

impl WebTools {
    pub fn new(cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            cache_dir: cache_dir.into(),
        }
    }

    pub fn is_available() -> bool {
        exa_api_key().is_some()
    }

    pub async fn web_search(
        &self,
        query: &str,
        num_results: usize,
        include_text: bool,
        category: &str,
    ) -> String {
        let Some(api_key) = exa_api_key() else {
            return error_json("Exa API not configured. Set EXA_API_KEY environment variable.");
        };
        if query.trim().is_empty() {
            return error_json("web_search requires a non-empty `query`.");
        }
        let num_results = num_results.clamp(1, 20);
        let mut payload = json!({
            "query": query,
            "numResults": num_results,
            "type": "auto",
            "contents": {
                "text": if include_text { json!({"maxCharacters": 2000}) } else { json!(false) },
                "highlights": {"numSentences": 3},
                "summary": {"query": query},
            }
        });
        if valid_category(category) {
            payload["category"] = json!(category.to_ascii_lowercase());
        }

        let response = match Client::new()
            .post("https://api.exa.ai/search")
            .header("x-api-key", api_key)
            .header("Content-Type", "application/json")
            .timeout(EXA_TIMEOUT)
            .json(&payload)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => return error_json(&format!("Request failed: {error}")),
        };

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return error_json(&format!("Exa API error: {status} - {}", trim(&body, 200)));
        }

        let data = match response.json::<ExaSearchResponse>().await {
            Ok(data) => data,
            Err(error) => return error_json(&format!("Invalid Exa response: {error}")),
        };
        if data.results.is_empty() {
            return serde_json::to_string_pretty(&json!({
                "status": "OK",
                "query": query,
                "message": "No results found.",
            }))
            .unwrap();
        }

        let raw_file = self
            .save_raw_results(query, &data.results)
            .unwrap_or_else(|error| format!("Error saving raw results: {error}"));
        let mut formatted = format_raw_results(query, &data.results, 3_000);
        formatted.push_str(&format!("\n\n(Raw results: {raw_file})"));
        formatted
    }

    pub async fn fetch_webpage(&self, url: &str, max_chars: usize) -> String {
        let Some(api_key) = exa_api_key() else {
            return error_json("Exa API not configured. Set EXA_API_KEY environment variable.");
        };
        let payload = json!({
            "ids": [url],
            "text": {"maxCharacters": max_chars.clamp(1, 50_000)},
        });
        let response = match Client::new()
            .post("https://api.exa.ai/contents")
            .header("x-api-key", api_key)
            .header("Content-Type", "application/json")
            .timeout(EXA_TIMEOUT)
            .json(&payload)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => return error_json(&format!("Request failed: {error}")),
        };
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return error_json(&format!("Exa API error: {status} - {}", trim(&body, 200)));
        }
        let data = match response.json::<ExaSearchResponse>().await {
            Ok(data) => data,
            Err(error) => return error_json(&format!("Invalid Exa response: {error}")),
        };
        let Some(result) = data.results.into_iter().next() else {
            return error_json(&format!("Could not fetch content from {url}"));
        };
        serde_json::to_string_pretty(&json!({
            "status": "OK",
            "url": result.url.unwrap_or_else(|| url.to_string()),
            "title": result.title.unwrap_or_default(),
            "text": result.text.unwrap_or_default(),
        }))
        .unwrap()
    }

    fn save_raw_results(&self, query: &str, results: &[ExaResult]) -> std::io::Result<String> {
        let dir = self.cache_dir.join("web_search");
        fs::create_dir_all(&dir)?;
        let timestamp = Local::now().format("%Y%m%d_%H%M%S").to_string();
        let filename = format!("{}_{}.json", timestamp, safe_filename(query));
        let path = dir.join(filename);
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "query": query,
                "timestamp": timestamp,
                "num_results": results.len(),
                "results": results,
            }))
            .unwrap(),
        )?;
        Ok(path.display().to_string())
    }
}

#[derive(Debug, Deserialize)]
struct ExaSearchResponse {
    #[serde(default)]
    results: Vec<ExaResult>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ExaResult {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    highlights: Vec<String>,
    #[serde(default, rename = "publishedDate")]
    published_date: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

fn exa_api_key() -> Option<String> {
    env::var("EXA_API_KEY")
        .ok()
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
}

fn format_raw_results(query: &str, results: &[ExaResult], max_chars: usize) -> String {
    let mut lines = vec![
        format!("Search: {query}"),
        format!("{} results:", results.len()),
        String::new(),
    ];
    for (index, result) in results.iter().enumerate() {
        lines.push(format!(
            "{}. {}",
            index + 1,
            trim(result.title.as_deref().unwrap_or("Untitled"), 80)
        ));
        if let Some(url) = result.url.as_deref() {
            lines.push(format!("   {url}"));
        }
        if let Some(summary) = result.summary.as_deref().filter(|value| !value.is_empty()) {
            lines.push(format!("   {}", trim(summary, 180)));
        }
        for highlight in result.highlights.iter().take(2) {
            lines.push(format!("   > {}", trim(highlight, 180)));
        }
        if let Some(published) = result.published_date.as_deref() {
            lines.push(format!("   Published: {published}"));
        }
        lines.push(String::new());
    }
    let text = lines.join("\n");
    crate::llm::truncate::truncate_with_ellipsis(&text, max_chars)
}

fn error_json(message: &str) -> String {
    serde_json::to_string_pretty(&json!({
        "status": "error",
        "message": message,
    }))
    .unwrap()
}

fn trim(value: &str, max_chars: usize) -> String {
    crate::llm::truncate::truncate_with_ellipsis(value, max_chars)
}

fn safe_filename(query: &str) -> String {
    let name = query
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ' ' | '-' | '_'))
        .collect::<String>()
        .trim()
        .chars()
        .take(50)
        .collect::<String>()
        .replace(' ', "_");
    if name.is_empty() {
        "query".to_string()
    } else {
        name
    }
}

fn valid_category(category: &str) -> bool {
    matches!(
        category.to_ascii_lowercase().as_str(),
        "company" | "research paper" | "news" | "pdf" | "github" | "tweet"
    )
}

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

use crate::tools::registry::ToolRegistry;
use crate::tools::registry::args::{bool_arg, string_arg, string_arg_default, usize_arg};
use crate::tools::spec::{ToolCategory, ToolDef, ToolExecutor, p_bool, p_int, p_str, p_str_req};

// Async executors, not Sync: a Sync executor runs inline on the tokio worker,
// and blocking HTTP there panics the turn ("Cannot drop a runtime in a context
// where blocking is not allowed" — reqwest::blocking's internal runtime).

fn exec_web_search<'a>(
    registry: &'a ToolRegistry<'a>,
    args: &'a Value,
) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
    Box::pin(async move {
        registry
            .web
            .web_search(
                &string_arg(args, "query"),
                usize_arg(args, "num_results", 10),
                bool_arg(args, "include_text", false),
                &string_arg_default(args, "category", ""),
            )
            .await
    })
}

fn exec_fetch_webpage<'a>(
    registry: &'a ToolRegistry<'a>,
    args: &'a Value,
) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
    Box::pin(async move {
        registry
            .web
            .fetch_webpage(&string_arg(args, "url"), usize_arg(args, "max_chars", 5000))
            .await
    })
}

pub const TOOL_DEFS: &[ToolDef] = &[
    ToolDef {
        name: "web_search",
        description: "Search the web for current external information.",
        params: &[
            p_str_req("query", "Search query."),
            p_int("num_results", "Max results (1-20)."),
            p_bool("include_text", "Include page text snippets."),
            p_str(
                "category",
                "Optional: company, research paper, news, pdf, github, tweet.",
            ),
        ],
        category: ToolCategory::Initial,
        execute: ToolExecutor::Async(exec_web_search),
    },
    ToolDef {
        name: "fetch_webpage",
        description: "Fetch a webpage's text.",
        params: &[
            p_str_req("url", "URL."),
            p_int("max_chars", "Max characters."),
        ],
        category: ToolCategory::Requestable,
        execute: ToolExecutor::Async(exec_fetch_webpage),
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_api_key_returns_structured_error() {
        unsafe {
            env::remove_var("EXA_API_KEY");
        }
        let tools = WebTools::new("/tmp/lethe-web-test");
        let result = tools.web_search("rust", 3, false, "").await;
        assert!(result.contains("\"status\": \"error\""));
        assert!(result.contains("EXA_API_KEY"));
    }

    #[test]
    fn formats_results_and_sanitizes_filenames() {
        let results = vec![ExaResult {
            title: Some("Rust Language".to_string()),
            url: Some("https://www.rust-lang.org/".to_string()),
            summary: Some("A language empowering everyone to build reliable software.".to_string()),
            highlights: vec!["Fast and memory-efficient.".to_string()],
            published_date: Some("2026-01-01".to_string()),
            text: None,
        }];

        let formatted = format_raw_results("rust language", &results, 1000);
        assert!(formatted.contains("Rust Language"));
        assert!(formatted.contains("https://www.rust-lang.org/"));
        assert_eq!(safe_filename("rust: language/api?"), "rust_languageapi");
        assert!(valid_category("research paper"));
        assert!(!valid_category("bad"));
    }

    #[tokio::test]
    async fn fetch_without_api_key_returns_error() {
        unsafe {
            env::remove_var("EXA_API_KEY");
        }
        let tools = WebTools::new("/tmp/lethe-web-test");
        assert!(
            tools
                .fetch_webpage("https://example.com", 100)
                .await
                .contains("EXA_API_KEY")
        );
    }
}
