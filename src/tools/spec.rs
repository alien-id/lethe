use std::future::Future;
use std::pin::Pin;

use genai::chat::Tool;
use serde_json::{Map, Value, json};

use crate::tools::registry::ToolRegistry;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolCategory {
    /// Always loaded in every agent context.
    Initial,
    /// Always loaded for the user-facing cortex (no actor context, or
    /// principal actor) but requires `request_tool` from a subagent. Reduces
    /// the schema surface a freshly-spawned subagent has to parse.
    CortexOnly,
    /// Not in the initial set; loaded via `request_tool`.
    Requestable,
    /// Lethe's built-in generic browser (`browser_*`, backed by the external
    /// `agent-browser` CLI). Requestable, EXCEPT it's hidden when the agent-id
    /// vault-sealed browser is active — that one is a superset (its
    /// `alien_browser_act` covers snapshot/click/type/… and it adds vault-sealed
    /// credential injection), so exposing both would be two competing,
    /// session-isolated browsers. One browser at a time: the vault-sealed one when
    /// agent-id is on, this built-in one otherwise.
    BrowserBuiltin,
    /// Initial when an actor runtime context is attached.
    Actor,
    /// Like `Actor`, but only when the actor is a subagent.
    ActorSubagent,
    /// Initial when the TELEGRAM transport context is attached. These tools
    /// are Telegram-branded (keyboards, reactions) — a plain client/web chat
    /// gets the neutral `TransportClient` set instead.
    Transport,
    /// Initial when a client (web/desktop chat) transport context is attached
    /// and telegram is not. Transport-neutral chat egress.
    TransportClient,
    /// Initial when the hosted knowledge-graph backend is configured
    /// (KG_API_BASE/KG_API_TOKEN); hidden entirely otherwise.
    KnowledgeGraph,
    /// Alien agent-id identity + vault tools; visible when the agent-id-core and
    /// agent-id-vault CLIs are present and the integration is enabled.
    AgentId,
    /// Alien agent-id vault-sealed browser tools; visible only when the
    /// (marketplace-only) agent-id-browser CLI is additionally present.
    AgentIdBrowser,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParamKind {
    String,
    Integer,
    Bool,
    StringArray,
    Enum(&'static [&'static str]),
    /// Free-form JSON object (schema allows arbitrary keys). Used for flag
    /// pass-through params like `alien_browser_act.params`.
    Object,
}

#[derive(Clone, Copy, Debug)]
pub struct ParamSpec {
    pub name: &'static str,
    pub kind: ParamKind,
    pub description: &'static str,
    pub required: bool,
}

/// Per-tool executor stored in each [`ToolDef`]. Sync tools run inline; async
/// tools return a boxed future awaited by `ToolRegistry::execute_async`.
pub type SyncExecutor = fn(&ToolRegistry<'_>, &Value) -> String;
pub type AsyncExecutor = for<'a> fn(
    &'a ToolRegistry<'a>,
    &'a Value,
) -> Pin<Box<dyn Future<Output = String> + Send + 'a>>;

#[derive(Clone, Copy)]
pub enum ToolExecutor {
    Sync(SyncExecutor),
    Async(AsyncExecutor),
}

impl std::fmt::Debug for ToolExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolExecutor::Sync(_) => f.write_str("ToolExecutor::Sync"),
            ToolExecutor::Async(_) => f.write_str("ToolExecutor::Async"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub params: &'static [ParamSpec],
    pub category: ToolCategory,
    pub execute: ToolExecutor,
}

impl ToolDef {
    pub fn to_genai_tool(&self) -> Tool {
        Tool::new(self.name)
            .with_description(self.description)
            .with_schema(self.schema())
    }

    pub fn schema(&self) -> Value {
        let mut props = Map::new();
        let mut required = Vec::new();
        for param in self.params {
            props.insert(param.name.to_string(), param_schema(param));
            if param.required {
                required.push(param.name);
            }
        }
        json!({
            "type": "object",
            "properties": props,
            "required": required,
            "additionalProperties": false,
        })
    }
}

fn param_schema(param: &ParamSpec) -> Value {
    match param.kind {
        ParamKind::String => json!({"type": "string", "description": param.description}),
        ParamKind::Integer => json!({"type": "integer", "description": param.description}),
        ParamKind::Bool => json!({"type": "boolean", "description": param.description}),
        ParamKind::StringArray => json!({
            "type": "array",
            "items": {"type": "string"},
            "description": param.description,
        }),
        ParamKind::Enum(values) => json!({
            "type": "string",
            "description": param.description,
            "enum": values.iter().collect::<Vec<_>>(),
        }),
        ParamKind::Object => json!({
            "type": "object",
            "description": param.description,
            "additionalProperties": true,
        }),
    }
}

pub const fn p_str(name: &'static str, description: &'static str) -> ParamSpec {
    ParamSpec {
        name,
        kind: ParamKind::String,
        description,
        required: false,
    }
}

pub const fn p_str_req(name: &'static str, description: &'static str) -> ParamSpec {
    ParamSpec {
        name,
        kind: ParamKind::String,
        description,
        required: true,
    }
}

pub const fn p_int(name: &'static str, description: &'static str) -> ParamSpec {
    ParamSpec {
        name,
        kind: ParamKind::Integer,
        description,
        required: false,
    }
}

pub const fn p_int_req(name: &'static str, description: &'static str) -> ParamSpec {
    ParamSpec {
        name,
        kind: ParamKind::Integer,
        description,
        required: true,
    }
}

pub const fn p_bool(name: &'static str, description: &'static str) -> ParamSpec {
    ParamSpec {
        name,
        kind: ParamKind::Bool,
        description,
        required: false,
    }
}

pub const fn p_obj(name: &'static str, description: &'static str) -> ParamSpec {
    ParamSpec {
        name,
        kind: ParamKind::Object,
        description,
        required: false,
    }
}

pub const fn p_str_array(name: &'static str, description: &'static str) -> ParamSpec {
    ParamSpec {
        name,
        kind: ParamKind::StringArray,
        description,
        required: false,
    }
}

pub const fn p_enum(
    name: &'static str,
    description: &'static str,
    values: &'static [&'static str],
) -> ParamSpec {
    ParamSpec {
        name,
        kind: ParamKind::Enum(values),
        description,
        required: false,
    }
}
