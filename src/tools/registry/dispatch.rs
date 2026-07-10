use serde_json::Value;

use crate::tools::spec::ToolExecutor;

use super::{ToolRegistry, find_def};

impl<'a> ToolRegistry<'a> {
    pub async fn execute_async(&self, name: &str, args: &Value) -> String {
        self.execute_async_with_call_id(name, args, None).await
    }

    pub async fn execute_async_with_call_id(
        &self,
        name: &str,
        args: &Value,
        call_id: Option<&str>,
    ) -> String {
        let name = name.trim();
        if let Some(client) = self.runtime.hosted_plugins.as_ref()
            && client.tool(name).is_some()
        {
            return client.invoke(name, args, call_id).await;
        }
        if self
            .runtime
            .hosted_plugins
            .as_ref()
            .is_some_and(|client| client.replaces_builtin(name))
        {
            return format!(
                "Error: hosted replacement for tool '{name}' is unavailable; local fallback is disabled."
            );
        }
        let Some(def) = find_def(name) else {
            return format!("Unknown tool: {name}");
        };
        if !self.policy_allows(def.name) {
            return format!("Error: tool '{name}' is disabled by the active capability policy.");
        }
        match def.execute {
            ToolExecutor::Sync(f) => f(self, args),
            ToolExecutor::Async(f) => f(self, args).await,
        }
    }

    pub fn execute(&self, name: &str, args: &Value) -> String {
        let name = name.trim();
        if self
            .runtime
            .hosted_plugins
            .as_ref()
            .is_some_and(|client| client.tool(name).is_some())
        {
            return format!("Error: hosted plugin tool '{name}' requires async tool execution.");
        }
        if self
            .runtime
            .hosted_plugins
            .as_ref()
            .is_some_and(|client| client.replaces_builtin(name))
        {
            return format!(
                "Error: hosted replacement for tool '{name}' is unavailable; local fallback is disabled."
            );
        }
        let Some(def) = find_def(name) else {
            return format!("Unknown tool: {name}");
        };
        if !self.policy_allows(def.name) {
            return format!("Error: tool '{name}' is disabled by the active capability policy.");
        }
        match def.execute {
            ToolExecutor::Sync(f) => f(self, args),
            ToolExecutor::Async(_) => {
                format!("Error: tool '{name}' requires async tool execution.")
            }
        }
    }
}
