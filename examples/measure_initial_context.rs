//! Estimate how many tokens the initial agent context consumes.
//!
//! Builds the cortex system prompt + tool schemas for several contexts and
//! prints byte / char / approx-token (~4 chars/token) sizes.
//!
//! Run with: `cargo run --example measure_initial_context`

use tempfile::tempdir;

use lethe::actor::{ActorConfig, ActorRegistry};
use lethe::agent::{Agent, TurnRequest};
use lethe::config::{Settings, test_settings};
use lethe::tools::registry::{
    ActorToolContext, ToolRegistry, ToolRuntime, requestable_tools_directory_for,
};
use lethe::tools::shell::ShellTools;

fn approx_tokens(text: &str) -> usize {
    // Conservative rule of thumb: ~4 chars per token for English + JSON.
    (text.chars().count() + 3) / 4
}

fn schema_size(registry: &ToolRegistry<'_>) -> (usize, usize, usize) {
    let tools = registry.tools_for_active(&std::collections::HashSet::new());
    let mut total_chars = 0;
    let mut total_bytes = 0;
    for tool in &tools {
        let json = serde_json::to_string(&serde_json::json!({
            "name": tool.name,
            "description": tool.description,
            "input_schema": tool.schema,
        }))
        .unwrap();
        total_chars += json.chars().count();
        total_bytes += json.len();
    }
    (total_chars, total_bytes, tools.len())
}

fn report(label: &str, prompt: &str, schema_chars: usize, tool_count: usize) {
    let prompt_chars = prompt.chars().count();
    let total_chars = prompt_chars + schema_chars;
    println!("--- {label} ---");
    println!(
        "  loaded tools:    {tool_count} ({schema_chars} schema chars, ~{} tokens)",
        approx_tokens(&" ".repeat(schema_chars))
    );
    println!(
        "  system prompt:   {prompt_chars} chars (~{} tokens)",
        approx_tokens(prompt)
    );
    println!(
        "  initial total:   {total_chars} chars (~{} tokens)\n",
        approx_tokens(&" ".repeat(total_chars))
    );
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let tmp = tempdir()?;

    // --- Cortex (no telegram, with actor runtime) ---
    let settings: Settings = {
        let mut s = test_settings(tmp.path());
        s.background.actors_enabled = true;
        s
    };
    let agent = Agent::from_settings(settings.clone())?;
    let turn = agent
        .prepare_turn(&TurnRequest::new("What's the status?"))
        .await?;
    // The system prompt is split into stable + volatile messages; sum them.
    use lethe::llm::LlmRole;
    let system_messages: Vec<&str> = turn
        .messages
        .iter()
        .filter(|m| m.role == LlmRole::System)
        .map(|m| m.content.as_str())
        .collect();
    let system = system_messages.join("\n\n");
    println!(
        "(prompt is split into {} system messages: stable + volatile)\n",
        system_messages.len()
    );

    let memory = lethe::memory::MemoryStore::from_settings(&settings)?;
    let shell = ShellTools::new(&settings.paths.workspace_dir);

    // Cortex registry: actor context with is_subagent = false, no transport.
    let mut registry = ActorRegistry::new();
    let cortex_id = registry.spawn(
        ActorConfig::new("cortex", "Serve the user").in_group("main"),
        None,
        true,
    );
    let actor_runtime = lethe::actor::ActorRuntime::new(registry);
    let cortex_tool_runtime = ToolRuntime {
        actor: Some(ActorToolContext {
            runtime: actor_runtime.clone(),
            actor_id: cortex_id.clone(),
            is_subagent: false,
        }),
        ..ToolRuntime::default()
    };
    let cortex_reg = ToolRegistry::with_runtime(
        &memory,
        settings.paths.workspace_dir.clone(),
        settings.paths.cache_dir.clone(),
        &shell,
        cortex_tool_runtime,
    );
    let (cortex_schema_chars, _, cortex_tool_count) = schema_size(&cortex_reg);
    report(
        "Cortex (actor on, no telegram)",
        &system,
        cortex_schema_chars,
        cortex_tool_count,
    );

    // --- Subagent ---
    let sub_tool_runtime = ToolRuntime {
        actor: Some(ActorToolContext {
            runtime: actor_runtime.clone(),
            actor_id: cortex_id.clone(),
            is_subagent: true,
        }),
        ..ToolRuntime::default()
    };
    let sub_reg = ToolRegistry::with_runtime(
        &memory,
        settings.paths.workspace_dir.clone(),
        settings.paths.cache_dir.clone(),
        &shell,
        sub_tool_runtime,
    );
    let (sub_schema_chars, _, sub_tool_count) = schema_size(&sub_reg);
    // Subagent system prompt is built by the actor registry, not by
    // Agent::prepare_turn. Spawn a child actor and ask the registry for its
    // system prompt directly so the measurement reflects what a real subagent
    // would see.
    let subagent_id = {
        let registry_handle = agent.actor_registry().unwrap();
        let principal = agent.principal_actor_id().unwrap().to_string();
        let spawn = registry_handle
            .spawn_subagent(lethe::actor::SpawnSubagent {
                actor_id: principal,
                name: "research-helper".to_string(),
                goals: "Find references to context-window sizing in the codebase.".to_string(),
                group: None,
                tools: String::new(),
                model: "aux".to_string(),
                max_turns: 10,
            })
            .await
            .unwrap();
        match spawn {
            lethe::actor::SpawnReport::Spawned { actor_id, .. } => actor_id,
            lethe::actor::SpawnReport::Rejected { message } => {
                panic!("spawn failed: {message}")
            }
        }
    };
    let sub_prompt = agent
        .actor_registry()
        .unwrap()
        .build_system_prompt(&subagent_id)
        .await
        .unwrap();
    report(
        "Subagent (no transport)",
        &sub_prompt,
        sub_schema_chars,
        sub_tool_count,
    );

    // --- Directory hint sizes ---
    let cortex_dir = requestable_tools_directory_for(&ToolRuntime {
        actor: Some(ActorToolContext {
            runtime: actor_runtime.clone(),
            actor_id: cortex_id.clone(),
            is_subagent: false,
        }),
        ..ToolRuntime::default()
    });
    let sub_dir = requestable_tools_directory_for(&ToolRuntime {
        actor: Some(ActorToolContext {
            runtime: actor_runtime.clone(),
            actor_id: cortex_id.clone(),
            is_subagent: true,
        }),
        ..ToolRuntime::default()
    });
    println!(
        "Directory hint (cortex):   {} chars (~{} tokens)",
        cortex_dir.chars().count(),
        approx_tokens(&cortex_dir),
    );
    println!(
        "Directory hint (subagent): {} chars (~{} tokens)",
        sub_dir.chars().count(),
        approx_tokens(&sub_dir),
    );

    // --- System prompt breakdown ---
    println!("\n--- System prompt breakdown ---");
    let prompts = lethe::llm::prompts::PromptStore::new(
        &settings.paths.workspace_dir,
        &settings.paths.config_dir,
    );
    let instructions = prompts.load("agent_instructions", "").text;
    let tools_doc = prompts.load("agent_tools", "").text;
    let memory_context = memory.get_context_for_prompt()?;
    println!(
        "  agent_instructions:   {} chars (~{} tokens)",
        instructions.chars().count(),
        approx_tokens(&instructions)
    );
    println!(
        "  agent_tools doc:      {} chars (~{} tokens)",
        tools_doc.chars().count(),
        approx_tokens(&tools_doc)
    );
    println!(
        "  memory_context:       {} chars (~{} tokens)",
        memory_context.chars().count(),
        approx_tokens(&memory_context)
    );
    println!(
        "  available_on_request: {} chars (~{} tokens)",
        cortex_dir.chars().count(),
        approx_tokens(&cortex_dir)
    );
    let actor_context = agent
        .actor_registry()
        .unwrap()
        .build_system_prompt(agent.principal_actor_id().unwrap())
        .await
        .unwrap_or_default();
    println!(
        "  actor_context:        {} chars (~{} tokens)",
        actor_context.chars().count(),
        approx_tokens(&actor_context)
    );

    std::fs::write("/tmp/lethe_cortex_prompt.txt", &system)?;
    std::fs::write("/tmp/lethe_subagent_prompt.txt", &sub_prompt)?;
    println!("\nWrote prompts to /tmp/lethe_cortex_prompt.txt and /tmp/lethe_subagent_prompt.txt");

    Ok(())
}
