use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::actor::{ActorState, ActorToolCommand, Outcome, SpawnReport, SpawnSubagent};
use crate::llm::truncate::truncate_with_ellipsis;
use crate::tools::registry::ToolRegistry;
use crate::tools::registry::args::{
    bool_arg, nonempty_string, string_arg, string_arg_default, usize_arg,
};
use crate::tools::spec::{
    ToolCategory, ToolDef, ToolExecutor, p_bool, p_enum, p_int, p_str, p_str_req,
};

const TASK_STATE_VALUES: &[&str] = &["planned", "running", "blocked", "done"];
const MISSING_ACTOR_CONTEXT: &str = "Actor context not set. This tool only works inside an actor.";

async fn run_actor_command(
    registry: &ToolRegistry<'_>,
    build: impl FnOnce(String) -> ActorToolCommand,
) -> String {
    let Some(context) = registry.runtime.actor.as_ref() else {
        return MISSING_ACTOR_CONTEXT.to_string();
    };
    let command = build(context.actor_id.clone());
    context.runtime.execute_actor_tool(command).await
}

fn exec_send_message<'a>(
    registry: &'a ToolRegistry<'a>,
    args: &'a Value,
) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
    Box::pin(run_actor_command(registry, move |actor_id| {
        ActorToolCommand::SendMessage {
            actor_id,
            target_id: string_arg(args, "actor_id"),
            content: string_arg(args, "content"),
            reply_to: nonempty_string(args, "reply_to"),
            channel: string_arg_default(args, "channel", ""),
            kind: string_arg_default(args, "kind", ""),
        }
    }))
}

fn exec_wait_for_response<'a>(
    registry: &'a ToolRegistry<'a>,
    _args: &'a Value,
) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
    Box::pin(run_actor_command(registry, |actor_id| {
        ActorToolCommand::WaitForResponse { actor_id }
    }))
}

fn exec_discover_actors<'a>(
    registry: &'a ToolRegistry<'a>,
    args: &'a Value,
) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
    Box::pin(run_actor_command(registry, move |actor_id| {
        ActorToolCommand::DiscoverActors {
            actor_id,
            group: nonempty_string(args, "group"),
            include_terminated: bool_arg(args, "include_terminated", false),
        }
    }))
}

fn exec_spawn_actor<'a>(
    registry: &'a ToolRegistry<'a>,
    args: &'a Value,
) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
    Box::pin(run_actor_command(registry, move |actor_id| {
        ActorToolCommand::SpawnActor {
            actor_id,
            name: string_arg(args, "name"),
            goals: string_arg(args, "goals"),
            group: nonempty_string(args, "group"),
            tools: string_arg_default(args, "tools", ""),
            model: string_arg_default(args, "model", "aux"),
            max_turns: usize_arg(args, "max_turns", 20),
        }
    }))
}

fn exec_ping_actor<'a>(
    registry: &'a ToolRegistry<'a>,
    args: &'a Value,
) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
    Box::pin(run_actor_command(registry, move |actor_id| {
        ActorToolCommand::PingActor {
            actor_id,
            target_id: string_arg(args, "actor_id"),
        }
    }))
}

fn exec_kill_actor<'a>(
    registry: &'a ToolRegistry<'a>,
    args: &'a Value,
) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
    Box::pin(run_actor_command(registry, move |actor_id| {
        ActorToolCommand::KillActor {
            actor_id,
            target_id: string_arg(args, "actor_id"),
        }
    }))
}

fn exec_update_task_state<'a>(
    registry: &'a ToolRegistry<'a>,
    args: &'a Value,
) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
    Box::pin(run_actor_command(registry, move |actor_id| {
        ActorToolCommand::UpdateTaskState {
            actor_id,
            state: string_arg(args, "state"),
            note: string_arg_default(args, "note", ""),
        }
    }))
}

fn exec_get_task_state<'a>(
    registry: &'a ToolRegistry<'a>,
    _args: &'a Value,
) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
    Box::pin(run_actor_command(registry, |actor_id| {
        ActorToolCommand::GetTaskState { actor_id }
    }))
}

fn exec_terminate<'a>(
    registry: &'a ToolRegistry<'a>,
    args: &'a Value,
) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
    Box::pin(run_actor_command(registry, move |actor_id| {
        ActorToolCommand::Terminate {
            actor_id,
            result: string_arg_default(args, "result", ""),
            outcome: string_arg_default(args, "outcome", "success"),
            files_touched: string_arg_default(args, "files_touched", ""),
            follow_up: string_arg_default(args, "follow_up", ""),
        }
    }))
}

fn exec_restart_self<'a>(
    registry: &'a ToolRegistry<'a>,
    args: &'a Value,
) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
    Box::pin(run_actor_command(registry, move |actor_id| {
        ActorToolCommand::RestartSelf {
            actor_id,
            new_goals: string_arg(args, "new_goals"),
        }
    }))
}

fn exec_spawn_chain<'a>(
    registry: &'a ToolRegistry<'a>,
    args: &'a Value,
) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
    Box::pin(spawn_chain(registry, args))
}

async fn spawn_chain(registry: &ToolRegistry<'_>, args: &Value) -> String {
    const MAX_CHAIN_STEPS: usize = 5;
    const STEP_WAIT_TIMEOUT: Duration = Duration::from_secs(60 * 60);
    const STEP_POLL_INTERVAL: Duration = Duration::from_secs(1);

    let Some(context) = registry.runtime.actor.as_ref() else {
        return MISSING_ACTOR_CONTEXT.to_string();
    };
    let steps_text = string_arg(args, "steps");
    let steps = match serde_json::from_str::<Vec<ChainStep>>(&steps_text) {
        Ok(steps) if !steps.is_empty() => steps,
        Ok(_) => {
            return "Error: steps must be a non-empty JSON array of {name, goals} objects."
                .to_string();
        }
        Err(error) => {
            return format!("Error: steps must be valid JSON array. Parse error: {error}");
        }
    };
    if steps.len() > MAX_CHAIN_STEPS {
        return format!(
            "Error: max {MAX_CHAIN_STEPS} steps in a chain (got {}).",
            steps.len()
        );
    }

    let tools = string_arg_default(args, "tools", "");
    let model = string_arg_default(args, "model", "aux");
    let max_turns = usize_arg(args, "max_turns", 20).max(1);
    let continue_on_failure = bool_arg(args, "continue_on_failure", false);
    let chain_id = Uuid::new_v4().to_string()[..6].to_string();
    let mut previous_result = String::new();
    let mut summaries = Vec::new();
    let mut failed_steps: Vec<usize> = Vec::new();

    for (index, step) in steps.into_iter().enumerate() {
        let base_name = step
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or("chain-step");
        let name = format!("{base_name}-{}-{}", index + 1, chain_id);
        let goals = step.goals.trim();
        if goals.is_empty() {
            return format!("Error: step {} has no goals.", index + 1);
        }
        let goals = goals.replace("{previous}", &previous_result);
        let spawn = context
            .runtime
            .spawn_subagent(SpawnSubagent {
                actor_id: context.actor_id.clone(),
                name: name.clone(),
                goals,
                group: None,
                tools: tools.clone(),
                model: model.clone(),
                max_turns,
            })
            .await;
        let child_id = match spawn {
            Ok(SpawnReport::Spawned { actor_id, .. }) => actor_id,
            Ok(SpawnReport::Rejected { message }) | Err(message) => {
                // Spawn failure is treated as a step failure; honors continue_on_failure
                // and the per-step `critical` override.
                summaries.push(format!(
                    "Step {} ({name}, spawn-rejected): {message}",
                    index + 1
                ));
                failed_steps.push(index + 1);
                if !continue_on_failure || step.critical {
                    return format!(
                        "Chain stopped at step {} ({name}):\n{}",
                        index + 1,
                        summaries.join("\n")
                    );
                }
                previous_result.clear();
                continue;
            }
        };

        let deadline = Instant::now() + STEP_WAIT_TIMEOUT;
        let (result, outcome) = loop {
            if Instant::now() >= deadline {
                return format!(
                    "Chain timed out at step {} ({name}) after {} seconds.",
                    index + 1,
                    STEP_WAIT_TIMEOUT.as_secs()
                );
            }
            match context.runtime.actor_info(&child_id).await {
                Some(info) if info.state == ActorState::Terminated => {
                    let outcome = info.outcome.unwrap_or(Outcome::Success);
                    break (
                        info.result.unwrap_or_else(|| "No result".to_string()),
                        outcome,
                    );
                }
                Some(_) => {
                    tokio::time::sleep(STEP_POLL_INTERVAL).await;
                }
                None => {
                    return format!(
                        "Chain stopped at step {} ({name}): actor {child_id} disappeared.",
                        index + 1
                    );
                }
            }
        };

        summaries.push(format!(
            "Step {} ({name}, {}): {}",
            index + 1,
            outcome.as_str(),
            truncate_with_ellipsis(&result, 200)
        ));
        if outcome.is_failed() {
            failed_steps.push(index + 1);
            if !continue_on_failure || step.critical {
                return format!(
                    "Chain stopped at step {} ({name}):\n{}",
                    index + 1,
                    summaries.join("\n")
                );
            }
            // Continue: clear forward-passed result so the next step's
            // {previous} doesn't carry stale data from a failed step.
            previous_result.clear();
            continue;
        }
        previous_result = result;
    }

    let header = if failed_steps.is_empty() {
        format!("Chain complete ({} steps):", summaries.len())
    } else {
        format!(
            "Chain finished ({} steps, {} failed: {}):",
            summaries.len(),
            failed_steps.len(),
            failed_steps
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    format!(
        "{header}\n{}\n\nFinal result:\n{}",
        summaries.join("\n"),
        truncate_with_ellipsis(&previous_result, 1000)
    )
}

#[derive(Debug, Deserialize)]
struct ChainStep {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    goals: String,
    /// When true, this step's failure aborts the whole chain even if
    /// `continue_on_failure` is set on the parent call. Default false.
    #[serde(default)]
    critical: bool,
}

pub const TOOL_DEFS: &[ToolDef] = &[
    ToolDef {
        name: "send_message",
        description: "Send a message to another actor in the group.",
        params: &[
            p_str_req("actor_id", "Recipient id."),
            p_str_req("content", "Content."),
            p_str("reply_to", "Message id this replies to."),
            p_str("channel", "Channel (e.g. task_update)."),
            p_str("kind", "Kind (e.g. progress, done)."),
        ],
        category: ToolCategory::Actor,
        execute: ToolExecutor::Async(exec_send_message),
    },
    ToolDef {
        name: "wait_for_response",
        description: "Pop the next message from this actor's inbox.",
        params: &[p_int("timeout", "Timeout seconds (compat).")],
        category: ToolCategory::Actor,
        execute: ToolExecutor::Async(exec_wait_for_response),
    },
    ToolDef {
        name: "discover_actors",
        description: "List actors in the current or named group.",
        params: &[
            p_str("group", "Group (empty = current)."),
            p_bool("include_terminated", "Include recently terminated."),
        ],
        category: ToolCategory::Actor,
        execute: ToolExecutor::Async(exec_discover_actors),
    },
    ToolDef {
        name: "spawn_actor",
        description: "Spawn a subagent for a delegated task.",
        params: &[
            p_str_req("name", "Short name."),
            p_str_req("goals", "Task goals and context."),
            p_str("group", "Group (empty = current)."),
            p_str("tools", "Extra tool names, comma-separated."),
            p_str("model", "main or aux."),
            p_int("max_turns", "Max LLM turns."),
        ],
        category: ToolCategory::Actor,
        execute: ToolExecutor::Async(exec_spawn_actor),
    },
    ToolDef {
        name: "spawn_chain",
        description: "Run subagents sequentially, passing each result forward.",
        params: &[
            p_str_req(
                "steps",
                "JSON [{name, goals, critical?}]; {previous} expands to last result. \
                 Set step.critical=true to force-abort even when continue_on_failure is set.",
            ),
            p_str("tools", "Extra tools for all steps."),
            p_str("model", "main or aux."),
            p_int("max_turns", "Max LLM turns per step."),
            p_bool(
                "continue_on_failure",
                "If true, record step failures and advance to the next step \
                 (clearing {previous}). Steps marked critical:true still abort the chain.",
            ),
        ],
        category: ToolCategory::Actor,
        execute: ToolExecutor::Async(exec_spawn_chain),
    },
    ToolDef {
        name: "ping_actor",
        description: "Inspect an actor's state and result.",
        params: &[p_str_req("actor_id", "Actor id.")],
        category: ToolCategory::Actor,
        execute: ToolExecutor::Async(exec_ping_actor),
    },
    ToolDef {
        name: "kill_actor",
        description: "Terminate an immediate child.",
        params: &[p_str_req("actor_id", "Child id.")],
        category: ToolCategory::Actor,
        execute: ToolExecutor::Async(exec_kill_actor),
    },
    ToolDef {
        name: "update_task_state",
        description: "Record this actor's task state.",
        params: &[
            p_enum("state", "State.", TASK_STATE_VALUES),
            p_str("note", "Checkpoint/blocker note."),
        ],
        category: ToolCategory::Actor,
        execute: ToolExecutor::Async(exec_update_task_state),
    },
    ToolDef {
        name: "get_task_state",
        description: "Return this actor's current task state.",
        params: &[],
        category: ToolCategory::Actor,
        execute: ToolExecutor::Async(exec_get_task_state),
    },
    ToolDef {
        name: "terminate",
        description: "End this actor with a structured result.",
        params: &[
            p_str("result", "Deliverable or status."),
            p_str("outcome", "success, failure, or partial."),
            p_str("files_touched", "Comma-separated paths."),
            p_str("follow_up", "Follow-up suggestion."),
        ],
        category: ToolCategory::Actor,
        execute: ToolExecutor::Async(exec_terminate),
    },
    ToolDef {
        name: "restart_self",
        description: "Terminate with revised goals for a parent respawn.",
        params: &[p_str_req("new_goals", "Revised goals.")],
        category: ToolCategory::ActorSubagent,
        execute: ToolExecutor::Async(exec_restart_self),
    },
];
