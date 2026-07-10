use genai::chat::Tool;

use crate::tools::hosted_plugins::RemoteToolExposure;
use crate::tools::spec::{ToolCategory, ToolDef};
use crate::tools::{agent_id, browser, filesystem, image, knowledge_graph, research, shell, web};

use super::ToolRegistry;
use super::{actor_specs, builtin_specs, telegram_specs};

/// All tool descriptors known to the runtime, in declaration order.
pub fn all_defs() -> impl Iterator<Item = &'static ToolDef> {
    filesystem::TOOL_DEFS
        .iter()
        .chain(image::TOOL_DEFS.iter())
        .chain(shell::TOOL_DEFS.iter())
        .chain(web::TOOL_DEFS.iter())
        .chain(browser::TOOL_DEFS.iter())
        .chain(builtin_specs::TOOL_DEFS.iter())
        .chain(actor_specs::TOOL_DEFS.iter())
        .chain(research::TOOL_DEFS.iter())
        .chain(telegram_specs::TOOL_DEFS.iter())
        .chain(knowledge_graph::TOOL_DEFS.iter())
        .chain(agent_id::TOOL_DEFS.iter())
}

pub fn find_def(name: &str) -> Option<&'static ToolDef> {
    let name = name.trim();
    all_defs().find(|def| def.name == name)
}

impl<'a> ToolRegistry<'a> {
    pub fn tools(&self) -> Vec<Tool> {
        let mut tools = all_defs()
            .filter(|def| self.runtime.policy.allows_builtin(def.name))
            .filter(|def| self.def_is_visible(def))
            .filter(|def| {
                !self
                    .runtime
                    .hosted_plugins
                    .as_ref()
                    .is_some_and(|client| client.replaces_builtin(def.name))
            })
            .map(ToolDef::to_genai_tool)
            .collect::<Vec<_>>();
        if let Some(client) = self.runtime.hosted_plugins.as_ref() {
            tools.extend(client.tools().into_iter().map(|tool| {
                Tool::new(tool.name)
                    .with_description(tool.description)
                    .with_schema(tool.input_schema)
            }));
        }
        tools
    }

    /// A def is visible (offered to the model in any form) when its category is
    /// compatible with the currently attached runtime contexts. CortexOnly is
    /// requestable from anywhere — it just isn't loaded initially in subagents.
    pub(super) fn def_is_visible(&self, def: &ToolDef) -> bool {
        if !self.runtime.policy.allows_builtin(def.name) {
            return false;
        }
        match def.category {
            ToolCategory::Initial | ToolCategory::Requestable | ToolCategory::CortexOnly => true,
            // The built-in browser hides when the vault-sealed browser is active,
            // so the agent is never offered two competing browsers.
            ToolCategory::BrowserBuiltin => !crate::agent_id::browser_tools_available(),
            ToolCategory::Actor => self.runtime.actor.is_some(),
            ToolCategory::ActorSubagent => self
                .runtime
                .actor
                .as_ref()
                .is_some_and(|context| context.is_subagent),
            ToolCategory::Transport => self.runtime.telegram.is_some(),
            ToolCategory::TransportClient => {
                self.runtime.client.is_some() && self.runtime.telegram.is_none()
            }
            ToolCategory::KnowledgeGraph => knowledge_graph::is_configured(),
            ToolCategory::AgentId => crate::agent_id::vault_tools_available(),
            ToolCategory::AgentIdBrowser => crate::agent_id::browser_tools_available(),
        }
    }

    /// A def is "initial" (loaded without `request_tool`) when both its
    /// category is initial-like AND any required runtime context is present.
    pub(super) fn def_is_initial(&self, def: &ToolDef) -> bool {
        match def.category {
            ToolCategory::Initial => true,
            ToolCategory::Requestable | ToolCategory::BrowserBuiltin => false,
            ToolCategory::CortexOnly => !self.is_subagent_context(),
            // Actor-orchestration tools stay discoverable (def_is_visible) but
            // are only loaded up front for actual subagents — the top-level
            // agent requests them on demand, keeping its initial tool set small.
            ToolCategory::Actor => self.is_subagent_context(),
            ToolCategory::ActorSubagent => self
                .runtime
                .actor
                .as_ref()
                .is_some_and(|context| context.is_subagent),
            ToolCategory::Transport => self.runtime.telegram.is_some(),
            ToolCategory::TransportClient => {
                self.runtime.client.is_some() && self.runtime.telegram.is_none()
            }
            ToolCategory::KnowledgeGraph => knowledge_graph::is_configured(),
            // Identity/vault/browser tools stay discoverable but are requested on
            // demand — they're used rarely, so keep them out of the initial set.
            ToolCategory::AgentId | ToolCategory::AgentIdBrowser => false,
        }
    }

    fn is_subagent_context(&self) -> bool {
        self.runtime
            .actor
            .as_ref()
            .is_some_and(|context| context.is_subagent)
    }

    pub(super) fn remote_is_initial(&self, name: &str) -> bool {
        self.runtime
            .hosted_plugins
            .as_ref()
            .and_then(|client| client.tool(name))
            .is_some_and(|tool| tool.exposure == RemoteToolExposure::Initial)
    }
}
