//! Knowledge-graph tools: the user's personal graph of people, places and
//! companies extracted from conversations (with notes, contacts and mention
//! history). Backed by a hosted control-plane HTTP API; configured via
//! `KG_API_BASE` + `KG_API_TOKEN` (injected by the hosted supervisor when the
//! feature is enabled for the user). When unconfigured, the kg_* tools are
//! hidden from the model entirely (see `ToolCategory::KnowledgeGraph`).

use std::env;
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;
use std::time::Duration;

use reqwest::Client;
use serde_json::{Value, json};

use crate::tools::registry::ToolRegistry;
use crate::tools::registry::args::{bool_arg, string_arg, u64_arg, usize_arg};
use crate::tools::spec::{
    ToolCategory, ToolDef, ToolExecutor, p_bool, p_enum, p_int, p_int_req, p_str, p_str_req,
};

fn kg_config() -> Option<(String, String)> {
    let base = env::var("KG_API_BASE")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())?;
    let token = env::var("KG_API_TOKEN")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())?;
    Some((base, token))
}

/// Whether the knowledge-graph backend is configured (cached: the env is
/// fixed for the process lifetime — containers are recreated to change it).
pub fn is_configured() -> bool {
    static CONFIGURED: OnceLock<bool> = OnceLock::new();
    *CONFIGURED.get_or_init(|| kg_config().is_some())
}

fn error_json(message: &str) -> String {
    json!({ "error": message }).to_string()
}

async fn handle(result: reqwest::Result<reqwest::Response>) -> String {
    match result {
        Ok(response) => {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if status.is_success() {
                text
            } else {
                // The API returns {"error":{"message":...}} — surface it as-is,
                // prefixed so the tool loop's failure detection sees an error.
                let trimmed: String = text.chars().take(300).collect();
                format!("Error: knowledge graph API {status}: {trimmed}")
            }
        }
        Err(error) => format!("Error: knowledge graph request failed: {error}"),
    }
}

async fn kg_get(path: &str, query: &[(&str, String)]) -> String {
    let Some((base, token)) = kg_config() else {
        return error_json("Knowledge graph not configured (KG_API_BASE/KG_API_TOKEN unset).");
    };
    handle(
        Client::new()
            .get(format!("{base}{path}"))
            .bearer_auth(token)
            .query(query)
            .timeout(Duration::from_secs(20))
            .send()
            .await,
    )
    .await
}

async fn kg_post(path: &str, body: Value) -> String {
    let Some((base, token)) = kg_config() else {
        return error_json("Knowledge graph not configured (KG_API_BASE/KG_API_TOKEN unset).");
    };
    handle(
        Client::new()
            .post(format!("{base}{path}"))
            .bearer_auth(token)
            .json(&body)
            .timeout(Duration::from_secs(20))
            .send()
            .await,
    )
    .await
}

// --- Executors ---
//
// Async, not Sync: a Sync executor runs inline on the tokio worker, and
// blocking HTTP there panics the turn (reqwest::blocking's internal runtime
// cannot be dropped in async context).

type ToolFuture<'a> = Pin<Box<dyn Future<Output = String> + Send + 'a>>;

fn exec_kg_search<'a>(_registry: &'a ToolRegistry<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let query = string_arg(args, "query");
        if query.trim().is_empty() {
            return error_json("'query' is required.");
        }
        let limit = usize_arg(args, "limit", 20).clamp(1, 50);
        kg_get("/search", &[("q", query), ("limit", limit.to_string())]).await
    })
}

fn exec_kg_get<'a>(_registry: &'a ToolRegistry<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let id = u64_arg(args, "id", 0);
        if id == 0 {
            return error_json("'id' is required (use kg_search to find it).");
        }
        kg_get("/entity", &[("id", id.to_string())]).await
    })
}

fn exec_kg_add<'a>(_registry: &'a ToolRegistry<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let name = string_arg(args, "name");
        let entity_type = string_arg(args, "type");
        if name.trim().is_empty() || entity_type.trim().is_empty() {
            return error_json("'name' and 'type' are required.");
        }
        let mut body = json!({ "name": name, "type": entity_type });
        let notes = string_arg(args, "notes");
        if !notes.trim().is_empty() {
            body["notes"] = Value::String(notes);
        }
        kg_post("/add", body).await
    })
}

fn exec_kg_delete<'a>(_registry: &'a ToolRegistry<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let id = u64_arg(args, "id", 0);
        if id == 0 {
            return error_json("'id' is required (use kg_search to find it).");
        }
        kg_post("/delete", json!({ "id": id })).await
    })
}

fn exec_kg_merge<'a>(_registry: &'a ToolRegistry<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let from = u64_arg(args, "from_id", 0);
        let into = u64_arg(args, "into_id", 0);
        if from == 0 || into == 0 {
            return error_json("'from_id' and 'into_id' are required (kg_search to find them).");
        }
        kg_post("/merge", json!({ "from": from, "into": into })).await
    })
}

fn exec_kg_set_notes<'a>(_registry: &'a ToolRegistry<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let id = u64_arg(args, "id", 0);
        if id == 0 {
            return error_json("'id' is required (use kg_search to find it).");
        }
        let content = string_arg(args, "content");
        let append = bool_arg(args, "append", false);
        kg_post(
            "/notes",
            json!({ "id": id, "content": content, "append": append }),
        )
        .await
    })
}

pub const TOOL_DEFS: &[ToolDef] = &[
    ToolDef {
        name: "kg_search",
        description: "Search the user's knowledge graph (people, places, companies extracted from your conversations). Matches names, aliases, notes and contact details; returns entities with ids for the other kg_* tools.",
        params: &[
            p_str_req(
                "query",
                "Search text (name, alias, or words from the notes).",
            ),
            p_int("limit", "Max results (1-50, default 20)."),
        ],
        category: ToolCategory::KnowledgeGraph,
        execute: ToolExecutor::Async(exec_kg_search),
    },
    ToolDef {
        name: "kg_get",
        description: "Full detail of one knowledge-graph entity: notes, aliases, contacts, and recent conversation mentions with snippets.",
        params: &[p_int_req("id", "Entity id (from kg_search).")],
        category: ToolCategory::KnowledgeGraph,
        execute: ToolExecutor::Async(exec_kg_get),
    },
    ToolDef {
        name: "kg_add",
        description: "Add an entity to the user's knowledge graph. Persons need a full name (given + family name). If the entity already exists it is returned, not duplicated.",
        params: &[
            p_str_req("name", "Canonical name (for a person: full name)."),
            p_enum("type", "Entity type.", &["person", "location", "company"]),
            p_str("notes", "Optional initial markdown notes."),
        ],
        category: ToolCategory::KnowledgeGraph,
        execute: ToolExecutor::Async(exec_kg_add),
    },
    ToolDef {
        name: "kg_delete",
        description: "Delete an entity (and its links/mentions) from the user's knowledge graph. Only when the user asks, or for clear extraction mistakes.",
        params: &[p_int_req("id", "Entity id (from kg_search).")],
        category: ToolCategory::KnowledgeGraph,
        execute: ToolExecutor::Async(exec_kg_delete),
    },
    ToolDef {
        name: "kg_merge",
        description: "Merge two knowledge-graph entities that refer to the same thing (duplicate names/spellings). Links, mentions, contacts and notes are folded into the target.",
        params: &[
            p_int_req("from_id", "Entity to fold in (disappears)."),
            p_int_req("into_id", "Entity to keep."),
        ],
        category: ToolCategory::KnowledgeGraph,
        execute: ToolExecutor::Async(exec_kg_merge),
    },
    ToolDef {
        name: "kg_set_notes",
        description: "Replace or append to an entity's markdown notes in the user's knowledge graph — the right place for durable profile facts about a person, place or company.",
        params: &[
            p_int_req("id", "Entity id (from kg_search)."),
            p_str_req("content", "Markdown notes content."),
            p_bool(
                "append",
                "Append below the existing notes instead of replacing (default false).",
            ),
        ],
        category: ToolCategory::KnowledgeGraph,
        execute: ToolExecutor::Async(exec_kg_set_notes),
    },
];
