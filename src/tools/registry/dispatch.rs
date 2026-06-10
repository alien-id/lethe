use serde_json::Value;

use crate::tools::spec::ToolExecutor;

use super::{ToolRegistry, find_def};

impl<'a> ToolRegistry<'a> {
    pub async fn execute_async(&self, name: &str, args: &Value) -> String {
        let name = name.trim();
        let Some(def) = find_def(name) else {
            return format!("Unknown tool: {name}");
        };
        match def.execute {
            ToolExecutor::Sync(f) => f(self, args),
            ToolExecutor::Async(f) => f(self, args).await,
        }
    }

    pub fn execute(&self, name: &str, args: &Value) -> String {
        let name = name.trim();
        let Some(def) = find_def(name) else {
            return format!("Unknown tool: {name}");
        };
        match def.execute {
            ToolExecutor::Sync(f) => f(self, args),
            ToolExecutor::Async(_) => {
                format!("Error: tool '{name}' requires async tool execution.")
            }
        }
    }
}
