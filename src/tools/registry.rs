use std::collections::HashSet;

use crate::actor::ActorRuntime;
use crate::interfaces::telegram::TelegramToolContext;
use crate::memory::MemoryStore;
use crate::tools::browser::BrowserTools;
use crate::tools::filesystem::FileTools;
use crate::tools::hosted_plugins::HostedPluginClient;
use crate::tools::image::ImageTools;
use crate::tools::shell::ShellTools;
use crate::tools::web::WebTools;

mod actor_specs;
pub(crate) mod args;
mod builtin_specs;
mod catalog;
mod client;
mod dispatch;
mod egress;
mod observer;
mod payload;
mod telegram_specs;

pub use egress::MessageEgress;
pub use observer::{BoxToolFuture, SharedTurnObserver, TurnObserver};

pub use catalog::find_def;
pub use client::{ClientToolContext, ClientToolEvent};

#[cfg(test)]
mod tests;

pub type SharedActorRegistry = ActorRuntime;

/// Controls which built-in capabilities exist for a turn. `HostedSafe` is an
/// allowlist enforced both while schemas are assembled and again at dispatch,
/// so a model cannot invoke a hidden local capability by name. It retains the
/// headless browser and Alien identity/vault families; callers must provide
/// tenant-private cache and agent-id state paths.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ToolPolicy {
    #[default]
    Full,
    HostedSafe,
}

impl ToolPolicy {
    fn allows_builtin(self, name: &str) -> bool {
        match self {
            Self::Full => true,
            Self::HostedSafe => {
                name == "request_tool"
                    || name == "chat_send_message"
                    || name.starts_with("memory_")
                    || name.starts_with("archival_")
                    || name.starts_with("conversation_")
                    || name.starts_with("note_")
                    || name.starts_with("todo_")
                    // NOTE: the built-in `browser_*` tools (standalone
                    // `agent-browser` CLI) are intentionally NOT allowed here.
                    // They are not tenant-scoped through the hosted secure path
                    // and bypass the host's browser-concurrency gate; hosted
                    // browsing is exclusively the vault-sealed `alien_browser_*`.
                    || name.starts_with("agent_id_")
                    || name.starts_with("vault_")
                    || name.starts_with("alien_browser_")
                    // Workspace file access. Admissible only because
                    // `ToolRegistry::with_runtime` constructs the jailed
                    // (sandboxed) FileTools/ImageTools under this policy, so
                    // every path is confined to the tenant workspace.
                    || matches!(
                        name,
                        "read_file"
                            | "write_file"
                            | "edit_file"
                            | "list_directory"
                            | "glob_search"
                            | "grep_search"
                            | "view_image"
                    )
                    // Subagent orchestration (spawn/message/terminate/...) is
                    // allowed: these tools only manage internal LLM workers and
                    // never touch host resources directly. Every subagent turn
                    // re-enters this same policy gate for its own tool catalog,
                    // so a hosted subagent cannot escalate past HostedSafe.
                    || find_def(name).is_some_and(|def| {
                        matches!(
                            def.category,
                            crate::tools::spec::ToolCategory::Actor
                                | crate::tools::spec::ToolCategory::ActorSubagent
                        )
                    })
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct ActorToolContext {
    pub runtime: SharedActorRegistry,
    pub actor_id: String,
    pub is_subagent: bool,
}

#[derive(Clone, Default)]
pub struct ToolRuntime {
    pub telegram: Option<TelegramToolContext>,
    pub client: Option<ClientToolContext>,
    pub actor: Option<ActorToolContext>,
    pub observer: Option<SharedTurnObserver>,
    /// Present only in hosted secure-prompt mode: lets the agent-id tools raise
    /// end-to-end-sealed credential cards in the frontend and emit identity
    /// lifecycle events.
    pub secure_prompt: Option<crate::agent_id::secure_prompt::SecurePromptHub>,
    /// Tenant-private agent-id identity, vault, and sealed-browser state. A
    /// multiplexed host must set this on every turn; standalone Lethe falls
    /// back to its process-local configured state directory.
    pub agent_id_state_dir: Option<std::path::PathBuf>,
    /// Trusted remote tools advertised by the lethe-hosted plugin gateway.
    /// The client owns a short-lived catalog cache, so the synchronous schema
    /// assembly path never performs network I/O.
    pub hosted_plugins: Option<std::sync::Arc<HostedPluginClient>>,
    /// Capability boundary for the current turn. Hosted multiplexers use the
    /// allowlisted policy; the standalone binary retains the full catalog.
    pub policy: ToolPolicy,
    pub requested_tools: Vec<String>,
}

impl std::fmt::Debug for ToolRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRuntime")
            .field("telegram", &self.telegram.is_some())
            .field("client", &self.client.is_some())
            .field("actor", &self.actor.is_some())
            .field("observer", &self.observer.is_some())
            .field("secure_prompt", &self.secure_prompt.is_some())
            .field("agent_id_state_dir", &self.agent_id_state_dir)
            .field("hosted_plugins", &self.hosted_plugins.is_some())
            .field("policy", &self.policy)
            .field("requested_tools", &self.requested_tools)
            .finish()
    }
}

#[derive(Clone)]
pub struct ToolRegistry<'a> {
    pub(crate) memory: &'a MemoryStore,
    pub(crate) files: FileTools,
    pub(crate) image: ImageTools,
    pub(crate) shell: &'a ShellTools,
    pub(crate) web: WebTools,
    pub(crate) browser: BrowserTools,
    pub(crate) runtime: ToolRuntime,
}

impl<'a> ToolRegistry<'a> {
    pub fn new(
        memory: &'a MemoryStore,
        workspace_dir: impl Into<std::path::PathBuf>,
        cache_dir: impl Into<std::path::PathBuf>,
        shell: &'a ShellTools,
    ) -> Self {
        Self::with_runtime(
            memory,
            workspace_dir,
            cache_dir,
            shell,
            ToolRuntime::default(),
        )
    }

    pub fn with_runtime(
        memory: &'a MemoryStore,
        workspace_dir: impl Into<std::path::PathBuf>,
        cache_dir: impl Into<std::path::PathBuf>,
        shell: &'a ShellTools,
        runtime: ToolRuntime,
    ) -> Self {
        let workspace_dir = workspace_dir.into();
        let cache_dir = cache_dir.into();
        let hosted = runtime.policy == ToolPolicy::HostedSafe;
        let browser = if hosted {
            BrowserTools::hosted(cache_dir.clone())
        } else {
            BrowserTools::new(cache_dir.clone())
        };
        // Under the hosted policy the file/image tools are jailed to the
        // (tenant-private) workspace; the allowlist below admits them only
        // because this constructor guarantees the jailed instances.
        let (files, image) = if hosted {
            (
                FileTools::sandboxed(workspace_dir.clone()),
                ImageTools::sandboxed(workspace_dir),
            )
        } else {
            (
                FileTools::new(workspace_dir.clone()),
                ImageTools::new(workspace_dir),
            )
        };
        Self {
            memory,
            files,
            image,
            shell,
            web: WebTools::new(cache_dir.clone()),
            browser,
            runtime,
        }
    }

    pub fn tools_for_active(&self, active_tools: &HashSet<String>) -> Vec<genai::chat::Tool> {
        self.tools()
            .into_iter()
            .filter(|tool| {
                self.is_initial_tool(&tool.name) || active_tools.contains(tool.name.as_str())
            })
            .collect()
    }

    pub fn tool_is_available(&self, name: &str) -> bool {
        let name = name.trim();
        if self
            .runtime
            .hosted_plugins
            .as_ref()
            .is_some_and(|client| client.tool(name).is_some())
        {
            return true;
        }
        if self
            .runtime
            .hosted_plugins
            .as_ref()
            .is_some_and(|client| client.replaces_builtin(name))
        {
            return false;
        }
        find_def(name).is_some_and(|def| self.def_is_visible(def))
    }

    pub fn tool_is_active(&self, name: &str, active_tools: &HashSet<String>) -> bool {
        self.is_initial_tool(name) || active_tools.contains(name)
    }

    pub fn turn_observer(&self) -> Option<&SharedTurnObserver> {
        self.runtime.observer.as_ref()
    }

    pub fn requestable_tool_names(&self) -> Vec<String> {
        let mut names = catalog::all_defs()
            .filter(|def| self.def_is_visible(def) && !self.def_is_initial(def))
            .filter(|def| {
                !self
                    .runtime
                    .hosted_plugins
                    .as_ref()
                    .is_some_and(|client| client.replaces_builtin(def.name))
            })
            .filter(|def| def.name != "request_tool")
            .map(|def| def.name.to_string())
            .collect::<Vec<_>>();
        if let Some(client) = self.runtime.hosted_plugins.as_ref() {
            names.extend(
                client
                    .tools()
                    .into_iter()
                    .filter(|tool| {
                        tool.exposure
                            == crate::tools::hosted_plugins::RemoteToolExposure::Requestable
                    })
                    .map(|tool| tool.name),
            );
        }
        names.sort();
        names.dedup();
        names
    }

    /// One-line per tool directory for the system prompt: `name — description`.
    /// Lists every tool the agent could `request_tool` for in the current
    /// context. Cheaper than loading full JSON schemas for tools the model may
    /// never use.
    pub fn requestable_tools_directory(&self) -> String {
        requestable_tools_directory_for(&self.runtime)
    }

    /// Tool families that form a single workflow (the vault-sealed browser's
    /// open/act/close/fill set, the agent-id identity+vault set, the built-in
    /// browser): requesting any member should load the whole family, so the
    /// model doesn't stall mid-flow on an "available but not loaded" sibling.
    pub fn group_siblings(&self, name: &str) -> Vec<String> {
        use crate::tools::spec::ToolCategory;
        let name = name.trim();
        if let Some(client) = self.runtime.hosted_plugins.as_ref()
            && client.tool(name).is_some()
        {
            return client.group_siblings(name);
        }
        let Some(def) = find_def(name) else {
            return Vec::new();
        };
        if !matches!(
            def.category,
            ToolCategory::AgentId | ToolCategory::AgentIdBrowser | ToolCategory::BrowserBuiltin
        ) {
            return Vec::new();
        }
        let mut names = catalog::all_defs()
            .filter(|sibling| sibling.category == def.category && self.def_is_visible(sibling))
            .map(|sibling| sibling.name.to_string())
            .filter(|sibling| sibling != name)
            .collect::<Vec<_>>();
        names.sort();
        names.dedup();
        names
    }
}

/// Shape inputs for [`requestable_tools_directory_for_shape`]. Lets callers
/// build the directory without constructing a `ToolRegistry` or `ToolRuntime`
/// (the registry needs a `MemoryStore` and `ShellTools` it doesn't use just to
/// list tool names; the runtime needs an `ActorRuntime` that isn't always
/// available at prompt-build time).
#[derive(Clone, Copy, Debug, Default)]
pub struct ToolContextShape {
    pub has_actor: bool,
    pub is_subagent: bool,
    /// Telegram transport attached (Telegram-branded egress tools).
    pub has_telegram: bool,
    /// Client (web/desktop chat) transport attached (neutral chat egress).
    pub has_client: bool,
    /// A tenant-private identity/vault directory is attached. Required by the
    /// hosted-safe policy so a mux can never fall back to process-global state.
    pub has_agent_id_state: bool,
    pub policy: ToolPolicy,
}

pub fn requestable_tools_directory_for(runtime: &ToolRuntime) -> String {
    let builtin = requestable_tools_directory_for_shape(ToolContextShape {
        has_actor: runtime.actor.is_some(),
        is_subagent: runtime
            .actor
            .as_ref()
            .is_some_and(|context| context.is_subagent),
        has_telegram: runtime.telegram.is_some(),
        has_client: runtime.client.is_some(),
        has_agent_id_state: runtime.agent_id_state_dir.is_some(),
        policy: runtime.policy,
    });
    let Some(client) = runtime.hosted_plugins.as_ref() else {
        return builtin;
    };
    let mut lines = builtin
        .lines()
        .filter(|line| {
            let name = line
                .strip_prefix("- ")
                .and_then(|line| line.split_once(" — "))
                .map(|(name, _)| name)
                .unwrap_or("");
            !client.replaces_builtin(name)
        })
        .map(str::to_string)
        .collect::<Vec<_>>();
    lines.extend(client.requestable_directory().lines().map(str::to_string));
    lines.sort();
    lines.dedup();
    lines.join("\n")
}

pub fn requestable_tools_directory_for_shape(shape: ToolContextShape) -> String {
    use crate::tools::spec::ToolCategory;
    let ToolContextShape {
        has_actor,
        is_subagent,
        has_telegram,
        has_client,
        has_agent_id_state,
        policy,
    } = shape;

    let visible = |def: &crate::tools::spec::ToolDef| match def.category {
        ToolCategory::Initial | ToolCategory::Requestable | ToolCategory::CortexOnly => true,
        // The built-in browser is never offered under the hosted policy — it is
        // not tenant-scoped through the secure path and isn't bounded by the
        // host's browser-concurrency gate, so hosted browsing is exclusively the
        // vault-sealed browser. Outside hosted, it hides when the vault-sealed
        // browser is active (so the agent is never shown two competing browsers).
        ToolCategory::BrowserBuiltin => {
            policy != ToolPolicy::HostedSafe && !crate::agent_id::browser_tools_available()
        }
        ToolCategory::Actor => has_actor,
        ToolCategory::ActorSubagent => is_subagent,
        ToolCategory::Transport => has_telegram,
        ToolCategory::TransportClient => has_client && !has_telegram,
        ToolCategory::KnowledgeGraph => crate::tools::knowledge_graph::is_configured(),
        ToolCategory::AgentId => {
            crate::agent_id::vault_tools_available()
                && (policy != ToolPolicy::HostedSafe || has_agent_id_state)
        }
        ToolCategory::AgentIdBrowser => {
            crate::agent_id::browser_tools_available()
                && (policy != ToolPolicy::HostedSafe || has_agent_id_state)
        }
    } && policy.allows_builtin(def.name);
    let initial = |def: &crate::tools::spec::ToolDef| match def.category {
        ToolCategory::Initial => true,
        ToolCategory::Requestable | ToolCategory::BrowserBuiltin => false,
        ToolCategory::CortexOnly => !is_subagent,
        ToolCategory::Actor => has_actor,
        ToolCategory::ActorSubagent => is_subagent,
        ToolCategory::Transport => has_telegram,
        ToolCategory::TransportClient => has_client && !has_telegram,
        ToolCategory::KnowledgeGraph => crate::tools::knowledge_graph::is_configured(),
        ToolCategory::AgentId | ToolCategory::AgentIdBrowser => false,
    };

    let mut lines = catalog::all_defs()
        .filter(|def| visible(def) && !initial(def))
        .filter(|def| def.name != "request_tool")
        .map(|def| format!("- {} — {}", def.name, def.description))
        .collect::<Vec<_>>();
    lines.sort();
    lines.dedup();
    lines.join("\n")
}

impl<'a> ToolRegistry<'a> {
    pub(super) fn is_initial_tool(&self, name: &str) -> bool {
        if self.remote_is_initial(name) {
            return true;
        }
        if self
            .runtime
            .hosted_plugins
            .as_ref()
            .is_some_and(|client| client.replaces_builtin(name))
        {
            return false;
        }
        find_def(name).is_some_and(|def| self.def_is_initial(def))
    }

    pub(super) fn policy_allows(&self, name: &str) -> bool {
        self.runtime.policy.allows_builtin(name)
    }
}
