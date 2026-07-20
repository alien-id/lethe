//! The LLM tool-use iteration: runs the model, dispatches each requested
//! tool, surfaces results (and inline images) back into the next turn, and
//! gives observers (e.g. Telegram typing) a chance to wrap each tool call.
//!
//! Lives next to [`Agent`](super::Agent) but stays as free functions so it
//! can be shared between the user-facing chat path and the actor turn
//! executor without dragging Agent itself across the kameo boundary.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use anyhow::anyhow;
use genai::chat::{ChatMessage, ChatRole, ContentPart, MessageContent, ToolCall, ToolResponse};
use regex::Regex;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::actor::{ActorError, ActorRunSpec, ActorRuntime, ActorTurnExecutor, ModelTier};
use crate::config::Settings;
use crate::llm::{LlmAttachment, LlmMessage, LlmRouter, build_chat_request};
use crate::memory::MemoryStore;
use crate::memory::MessageRole;
use crate::tools::registry::{ActorToolContext, BoxToolFuture, ToolRegistry, ToolRuntime};
use crate::tools::shell::ShellTools;

use super::{AgentError, AgentResult};

/// Max LLM/tool round-trips per turn. Python `main` runs up to 10 per batch
/// with up to 5 auto-continuation batches (~50 total); we collapse that into
/// a single flat cap.
pub(super) const MAX_TOOL_ITERATIONS: usize = 50;
/// Error budget for the turn, in weighted units: a permanent error costs 2,
/// a transient one (network flap, timeout, rate limit) costs 1. Eight
/// permanent failures trip the breaker exactly as the old flat cap did, but
/// a long task using flaky external tools gets twice the slack before being
/// cut off.
const MAX_TOOL_ERROR_PRESSURE: usize = 16;
const MAX_REPEATED_TOOL_CALLS: usize = 4;
const MAX_NO_PROGRESS_TURNS: usize = 4;
const MAX_EMPTY_RESPONSES: usize = 2;

/// Tools that don't count as "work" against [`total_tool_calls`] — memory
/// reads/writes, telegram side effects, actor lifecycle. Matches Python's
/// `FREE_TOOL_NAMES`.
const FREE_TOOL_NAMES: &[&str] = &[
    // Memory
    "memory_read",
    "memory_update",
    "memory_append",
    "archival_search",
    "archival_insert",
    "conversation_search",
    "conversation_recent",
    "note_search",
    "note_get",
    // Chat egress (Telegram-branded + the client-transport alias)
    "telegram_send_message",
    "telegram_send_file",
    "telegram_react",
    "chat_send_message",
    // Actor lifecycle
    "send_message",
    "user_notify",
    "terminate",
    "ping_actor",
    "wait_for_response",
    "discover_actors",
    "discover_recently_finished",
    "spawn_actor",
    "spawn_chain",
    "kill_actor",
    "update_task_state",
    "get_task_state",
    "restart_self",
];

/// Tools whose results we skip recording in the per-turn tool log
/// (search results are recursive bloat). Matches Python's
/// `SEARCH_RESULT_SKIP_TOOL_NAMES`.
const SKIP_TOOL_LOG_TOOLS: &[&str] = &["conversation_search", "conversation_recent", "archival_search"];

fn is_free_tool(name: &str) -> bool {
    FREE_TOOL_NAMES.contains(&name)
}

fn skip_tool_log(name: &str) -> bool {
    SKIP_TOOL_LOG_TOOLS.contains(&name)
}

fn is_error_result(result: &str) -> bool {
    let trimmed = result.trim_start();
    if trimmed.starts_with("Error:")
        || trimmed.starts_with("Unknown tool:")
        || trimmed.starts_with("✗")
        || trimmed
            .strip_prefix("Exit code:")
            .and_then(|tail| tail.split_whitespace().next())
            .and_then(|code| code.parse::<i32>().ok())
            .is_some_and(|code| code != 0)
    {
        return true;
    }
    serde_json::from_str::<serde_json::Value>(trimmed)
        .ok()
        .is_some_and(|value| {
            value.get("ok").and_then(Value::as_bool) == Some(false)
                || value.get("error").is_some_and(|error| !error.is_null())
                || value
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| status.eq_ignore_ascii_case("error"))
        })
}

/// Re-apply a hard context ceiling on every tool iteration. The initial turn
/// clamp cannot account for schemas loaded later via `request_tool`, nor for a
/// long chain of assistant/tool-result pairs appended after assembly. Prefer
/// dropping the oldest completed tool exchanges; if needed, then drop oldest
/// pre-turn history while preserving system messages and the current user ask.
fn clamp_chat_request_to_budget(
    request: &mut genai::chat::ChatRequest,
    current_user_index: &mut usize,
    max_total_chars: usize,
) -> usize {
    if max_total_chars == 0 {
        return 0;
    }
    let tool_chars = request
        .tools
        .as_ref()
        .map(|tools| tools.iter().map(genai::chat::Tool::size).sum::<usize>())
        .unwrap_or_default();
    let message_budget = max_total_chars.saturating_sub(tool_chars);
    let total = |messages: &[ChatMessage]| messages.iter().map(ChatMessage::size).sum::<usize>();
    let mut dropped = 0;

    // Completed exchanges appended after the current user request are the most
    // disposable and are usually the source of runaway form/research turns.
    while total(&request.messages) > message_budget
        && *current_user_index + 3 < request.messages.len()
    {
        request.messages.remove(*current_user_index + 1);
        dropped += 1;
        while *current_user_index + 1 < request.messages.len().saturating_sub(1)
            && request.messages[*current_user_index + 1].role == ChatRole::Tool
        {
            request.messages.remove(*current_user_index + 1);
            dropped += 1;
        }
    }

    // If schemas themselves grew enough to squeeze the initial prompt, prune
    // oldest non-system history but never the current user message.
    let history_start = request
        .messages
        .iter()
        .position(|message| message.role != ChatRole::System)
        .unwrap_or(request.messages.len());
    while total(&request.messages) > message_budget && history_start < *current_user_index {
        request.messages.remove(history_start);
        *current_user_index = current_user_index.saturating_sub(1);
        dropped += 1;
        while history_start < *current_user_index
            && request.messages[history_start].role == ChatRole::Tool
        {
            request.messages.remove(history_start);
            *current_user_index = current_user_index.saturating_sub(1);
            dropped += 1;
        }
    }
    dropped
}

/// Transient failures — the kind that often succeed on a later attempt
/// (network flaps, timeouts, rate limits, upstream 5xx). These weigh half a
/// permanent error against the circuit breaker so a long task leaning on
/// flaky external tools isn't cut off as aggressively as one making real
/// mistakes (wrong paths, bad arguments).
fn is_transient_error(result: &str) -> bool {
    if !is_error_result(result) {
        return false;
    }
    let lower = result.to_ascii_lowercase();
    const TRANSIENT_MARKERS: &[&str] = &[
        "timeout",
        "timed out",
        "connection",
        "network",
        "temporarily",
        "temporary",
        "rate limit",
        "too many requests",
        "429",
        "500",
        "502",
        "503",
        "504",
        "unavailable",
        "try again",
        "reset by peer",
        "dns",
    ];
    TRANSIENT_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

/// Detects true call degeneration: the same tool invoked with the same
/// arguments returning the same result. Legitimate polling — identical calls
/// whose results change as the world moves — never accumulates a streak, so
/// it no longer trips the repeated-call circuit breaker.
#[derive(Debug, Default)]
struct RepeatTracker {
    last_signature: String,
    last_fingerprint: u64,
    streak: usize,
}

impl RepeatTracker {
    /// Record one executed call and return the current degeneration streak
    /// (1 = fresh, >=2 = exact repeat with identical output).
    fn observe(&mut self, signature: &str, result: &str) -> usize {
        let fingerprint = fingerprint_str(result);
        if signature == self.last_signature && fingerprint == self.last_fingerprint {
            self.streak += 1;
        } else {
            self.streak = 1;
        }
        self.last_signature = signature.to_string();
        self.last_fingerprint = fingerprint;
        self.streak
    }
}

fn fingerprint_str(value: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// Per-turn record of one tool execution. Currently used by circuit
/// breakers; future callers (auto-archival) can read this back.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct ToolLogEntry {
    name: String,
    args_preview: String,
    result_preview: String,
    success: bool,
}

/// Upper bound on the args/output preview streamed to UI clients (desktop
/// debug card, TUI). Generous so the card shows the whole real-world result
/// (memory, file reads, search), capped only to guard SSE/render against a
/// pathological multi-megabyte tool output. The model still gets the full,
/// untruncated result in its context.
const TOOL_PREVIEW_CHARS: usize = 16_384;

fn truncate_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    value.chars().take(limit).collect()
}

fn gemma_tool_call_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<tool_call:(\w+)\{(.+?)\}>").expect("valid regex"))
}

fn bracket_tool_call_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?s)\[tool_call:\s*(\w+)\s*\((.*?)\)\s*\]").expect("valid regex")
    })
}

/// Recover text-embedded tool calls that models emit instead of using the
/// native protocol field. Two shapes are recognized:
/// - Gemma/llama.cpp: `<tool_call:func_name{key:"value", ...}>` (matches
///   Python's `_extract_text_tool_calls`)
/// - `[tool_call: func_name(key="value", ...)]` — small models mimic this
///   notation when instruction examples describe tool calls in prose
/// Returns native [`ToolCall`]s when at least one is matched.
fn recover_text_tool_calls(content: &str) -> Vec<ToolCall> {
    static QUOTED: OnceLock<Regex> = OnceLock::new();
    static EQUALS_QUOTED: OnceLock<Regex> = OnceLock::new();
    let quoted = QUOTED.get_or_init(|| Regex::new(r#"(\w+):"([^"]*)""#).expect("valid regex"));
    let equals_quoted = EQUALS_QUOTED
        .get_or_init(|| Regex::new(r#"(\w+)\s*=\s*"((?:\\.|[^"\\])*)""#).expect("valid regex"));

    let mut calls = Vec::new();
    for caps in gemma_tool_call_regex().captures_iter(content) {
        let fn_name = caps.get(1).map_or("", |m| m.as_str()).trim().to_string();
        let raw_args = caps.get(2).map_or("", |m| m.as_str());
        let clean = raw_args.replace("<|\"|>", "\"");
        let mut args = Map::new();
        let mut pairs = quoted.captures_iter(&clean).peekable();
        if pairs.peek().is_some() {
            for pair in pairs {
                args.insert(
                    pair[1].trim().to_string(),
                    Value::String(pair[2].trim().to_string()),
                );
            }
        } else {
            // Fallback: split a comma-separated `key:value` list without
            // requiring quotes. Single-arg cases pass through unchanged.
            for piece in clean.split(',') {
                if let Some((key, value)) = piece.split_once(':') {
                    let key = key.trim();
                    if key.is_empty() {
                        continue;
                    }
                    args.insert(key.to_string(), Value::String(value.trim().to_string()));
                }
            }
        }
        calls.push(ToolCall {
            call_id: format!("call-{}", Uuid::new_v4().simple()),
            fn_name,
            fn_arguments: Value::Object(args),
            thought_signatures: None,
        });
    }
    for caps in bracket_tool_call_regex().captures_iter(content) {
        let fn_name = caps.get(1).map_or("", |m| m.as_str()).trim().to_string();
        let raw_args = caps.get(2).map_or("", |m| m.as_str());
        let mut args = Map::new();
        let mut pairs = equals_quoted.captures_iter(raw_args).peekable();
        if pairs.peek().is_some() {
            for pair in pairs {
                args.insert(
                    pair[1].trim().to_string(),
                    Value::String(pair[2].replace("\\\"", "\"").replace("\\\\", "\\")),
                );
            }
        } else {
            for piece in raw_args.split(',') {
                if let Some((key, value)) = piece.split_once('=') {
                    let key = key.trim();
                    if key.is_empty() {
                        continue;
                    }
                    args.insert(
                        key.to_string(),
                        Value::String(value.trim().trim_matches('"').to_string()),
                    );
                }
            }
        }
        // Non-empty parens that parsed to no arguments is a placeholder quote
        // (e.g. `edit_file(...)` in prose), not an executable call — skip it.
        if !raw_args.trim().is_empty() && args.is_empty() {
            continue;
        }
        calls.push(ToolCall {
            call_id: format!("call-{}", Uuid::new_v4().simple()),
            fn_name,
            fn_arguments: Value::Object(args),
            thought_signatures: None,
        });
    }
    calls
}

/// Remove recovered textual tool-call markers from assistant text so the fake
/// syntax neither reaches the user nor persists in history, where the model
/// would re-learn it as the way to invoke tools.
fn strip_text_tool_call_markers(content: &str) -> String {
    let stripped = gemma_tool_call_regex().replace_all(content, "");
    let stripped = bracket_tool_call_regex().replace_all(&stripped, "");
    // Tidy bubble separators orphaned by the removal.
    let mut text = stripped.trim().to_string();
    while let Some(rest) = text.strip_suffix("---") {
        text = rest.trim_end().to_string();
    }
    while let Some(rest) = text.strip_prefix("---") {
        text = rest.trim_start().to_string();
    }
    text
}

/// Result of one full tool-loop turn. `stop_reason` is set when the loop was
/// cut short (circuit breaker, iteration cap, empty-response stall) rather
/// than ending with a natural reply — callers use it to mark the response as
/// a checkpoint to resume from instead of a finished answer.
#[derive(Clone, Debug)]
pub(super) struct TurnOutput {
    pub text: String,
    pub stop_reason: Option<String>,
}

impl TurnOutput {
    fn complete(text: String) -> Self {
        Self {
            text,
            stop_reason: None,
        }
    }
}

/// Bag of clones used to thread the agent's dependencies into the free-fn
/// tool loop. Cheap to clone because all members are already shared handles.
#[derive(Clone)]
pub(super) struct TurnExecutionContext {
    pub settings: Settings,
    pub memory: Arc<MemoryStore>,
    pub router: Arc<RwLock<LlmRouter>>,
    pub shell: ShellTools,
    /// Shared atomic that captures `prompt_tokens` from each LLM response so
    /// the next turn's compaction budget reflects real usage instead of a
    /// crude char estimate. Zero means "no measurement yet".
    pub last_prompt_tokens: Arc<AtomicU64>,
    pub hosted_plugins: Option<Arc<crate::tools::hosted_plugins::HostedPluginClient>>,
}

/// Build the actor turn executor that the [`ActorRuntime`] supervisor calls
/// to run each subagent turn. Wraps the standard tool loop with actor wiring.
/// `tool_policy` is the hosting process's capability boundary: subagent turns
/// run under the exact same policy as the cortex turns that spawned them, so
/// a hosted subagent can never reach tools its host denies.
pub(super) fn actor_turn_executor(
    settings: Settings,
    memory: Arc<MemoryStore>,
    router: Arc<RwLock<LlmRouter>>,
    shell: ShellTools,
    last_prompt_tokens: Arc<AtomicU64>,
    hosted_plugins: Option<Arc<crate::tools::hosted_plugins::HostedPluginClient>>,
    tool_policy: crate::tools::registry::ToolPolicy,
    subagent_observer: crate::tools::registry::SubagentObserverSlot,
) -> ActorTurnExecutor {
    let context = TurnExecutionContext {
        settings,
        memory,
        router,
        shell,
        last_prompt_tokens,
        hosted_plugins,
    };
    Arc::new(move |spec: ActorRunSpec, runtime: ActorRuntime| {
        let context = context.clone();
        // Resolve the host's observer factory (if any) per turn, so a factory
        // installed after startup covers turns already in flight next cycle.
        let observer = subagent_observer
            .read()
            .ok()
            .and_then(|slot| slot.clone())
            .map(|factory| factory(&spec.actor_id));
        Box::pin(async move {
            let tool_runtime = ToolRuntime {
                actor: Some(ActorToolContext {
                    runtime: runtime.clone(),
                    actor_id: spec.actor_id.clone(),
                    is_subagent: true,
                }),
                requested_tools: spec.requested_tools.clone(),
                policy: tool_policy,
                observer,
                ..ToolRuntime::default()
            };
            let mut system_prompt = spec.system_prompt.clone();
            if let Some(client) = context.hosted_plugins.as_ref() {
                if let Err(error) = client.refresh_catalog().await {
                    tracing::warn!(error = %error, "hosted plugin catalog unavailable to subagent");
                }
                // ActorRegistry builds its requestable directory before a
                // ToolRuntime exists. Remove hosted-owned local definitions,
                // then append the dynamic directory and bounded plugin data.
                system_prompt = system_prompt
                    .lines()
                    .filter(|line| {
                        let name = line
                            .strip_prefix("- ")
                            .and_then(|line| line.split_once(" — "))
                            .map(|(name, _)| name.trim());
                        !name.is_some_and(|name| client.replaces_builtin(name))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let directory = client.requestable_directory();
                if !directory.is_empty() {
                    system_prompt.push_str(
                        "\n\n<available_on_request source=\"hosted_plugins\">\n\
                         Tools below are NOT loaded. Call request_tool(name=...) to enable one for this turn.\n",
                    );
                    system_prompt.push_str(&directory);
                    system_prompt.push_str("\n</available_on_request>");
                }
                match client.context_blocks().await {
                    Ok(plugin_context) if !plugin_context.trim().is_empty() => {
                        system_prompt.push_str("\n\n");
                        system_prompt.push_str(plugin_context.trim());
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(error = %error, "hosted plugin context unavailable to subagent")
                    }
                }
            }
            let messages = vec![
                LlmMessage::system(system_prompt),
                LlmMessage::user(actor_turn_instruction(&spec)),
            ];
            complete_turn_with_tools_config_shared(
                context,
                messages,
                tool_runtime,
                spec.model == ModelTier::Aux,
                false,
            )
            .await
            // A cut-short turn is marked in the recorded response so the
            // actor sees it on its next turn (via <your_previous_turn>) and
            // the parent sees it in any handoff — the checkpoint is explicit
            // instead of looking like a finished answer.
            .map(|output| match output.stop_reason {
                Some(reason) => format!(
                    "{}\n\n[turn ended early: {reason} — the text above is a checkpoint, not a finished result]",
                    output.text
                ),
                None => output.text,
            })
            .map_err(|error| ActorError::Runtime(error.to_string()))
        })
    })
}

/// Run the LLM/tool loop end to end. Iterates up to [`MAX_TOOL_ITERATIONS`]
/// times: each iteration asks the model for the next move, executes any
/// returned tool calls, and feeds the results (plus any inline `_image_view`
/// payloads) back into the next request. Returns the final assistant text.
pub(super) async fn complete_turn_with_tools_config_shared(
    context: TurnExecutionContext,
    messages: Vec<LlmMessage>,
    mut runtime: ToolRuntime,
    use_aux: bool,
    record_tool_messages: bool,
) -> AgentResult<TurnOutput> {
    if runtime.hosted_plugins.is_none() {
        runtime.hosted_plugins = context.hosted_plugins.clone();
    }
    if let Some(client) = runtime.hosted_plugins.as_ref()
        && let Err(error) = client.refresh_catalog().await
    {
        // Keep the last-known catalog. On a cold outage, configured replacement
        // families remain hidden rather than falling back to local state.
        tracing::warn!(error = %error, "hosted plugin catalog refresh failed");
    }
    let mut active_tools = runtime
        .requested_tools
        .iter()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect::<HashSet<_>>();
    let registry = ToolRegistry::with_runtime(
        context.memory.as_ref(),
        context.settings.paths.workspace_dir.clone(),
        context.settings.paths.cache_dir.clone(),
        &context.shell,
        runtime,
    );
    let mut request = build_chat_request(messages);
    let mut current_user_index = request.messages.len().saturating_sub(1);
    let mut last_text = String::new();
    let mut total_tool_calls: usize = 0;
    let mut total_tool_errors: usize = 0;
    let mut tool_error_pressure: usize = 0;
    let mut repeat_tracker = RepeatTracker::default();
    let mut no_progress_turns: usize = 0;
    let mut empty_count: usize = 0;
    let mut tool_log: Vec<ToolLogEntry> = Vec::new();
    let mut circuit_breaker_reason: Option<String> = None;
    // Dynamic tool-model routing: the turn starts on the base model selected by
    // `use_aux`. If a dedicated tool model (`LLM_MODEL_TOOL`) is configured, the
    // first time the model asks for a tool we switch to it for the rest of the
    // chain — including the post-chain reply — and the next turn starts on the
    // base model again. With no tool model configured this stays false and the
    // whole turn runs on the base model, exactly as before.
    let mut entered_tool_chain = false;
    let mut tool_model_disabled = false;

    for iteration in 0..MAX_TOOL_ITERATIONS {
        request.tools = Some(registry.tools_for_active(&active_tools));
        tracing::debug!(
            iteration,
            messages = request.messages.len(),
            tools = request.tools.as_ref().map_or(0, Vec::len),
            active_tools = ?active_tools,
            "llm tool loop iteration"
        );
        let router = context
            .router
            .read()
            .map_err(|error| AgentError::Llm(anyhow!("router lock poisoned: {error}")))?
            .clone();
        // Once we're inside a tool chain and a dedicated tool model is
        // configured, route to it; otherwise use the base model for this turn.
        let model_id =
            if entered_tool_chain && !tool_model_disabled && router.config().has_tool_model() {
                router.config().tool_model().to_string()
            } else {
                router.config().model_for(use_aux).to_string()
            };
        let max_total_chars = context
            .settings
            .llm
            .context_limit_for(&model_id)
            .saturating_sub(context.settings.llm.llm_max_output as u64)
            .saturating_mul(4) as usize;
        let dropped =
            clamp_chat_request_to_budget(&mut request, &mut current_user_index, max_total_chars);
        if dropped > 0 {
            tracing::warn!(iteration, dropped, model = %model_id, "trimmed tool-loop context to budget");
        }
        let observer_for_stream = registry.turn_observer().cloned();
        let response = match observer_for_stream {
            Some(observer) => {
                let observer_reasoning = observer.clone();
                let on_delta = move |chunk: &str| observer.on_assistant_delta(chunk);
                let on_reasoning = move |chunk: &str| observer_reasoning.on_reasoning_delta(chunk);
                router
                    .exec_chat_request_stream_with_model(
                        request.clone(),
                        &model_id,
                        &on_delta,
                        Some(&on_reasoning),
                    )
                    .await?
            }
            None => {
                router
                    .exec_chat_request_with_model(request.clone(), &model_id)
                    .await?
            }
        };
        if let Some(prompt_tokens) = response.usage.prompt_tokens {
            context
                .last_prompt_tokens
                .store(prompt_tokens as u64, Ordering::Relaxed);
        }
        let mut text = response.first_text().unwrap_or_default().to_string();
        let mut tool_calls = response
            .tool_calls()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        if tool_calls.is_empty() && !text.is_empty() {
            // Gemma 4 / llama.cpp sometimes emit tool calls embedded in
            // assistant text instead of the native protocol field.
            let recovered = recover_text_tool_calls(&text);
            if !recovered.is_empty() {
                tracing::info!(
                    iteration,
                    recovered = recovered.len(),
                    "recovered text-embedded tool calls"
                );
                tool_calls = recovered;
                text = strip_text_tool_call_markers(&text);
            }
        }
        tracing::info!(
            iteration,
            text_chars = text.chars().count(),
            tool_calls = tool_calls.len(),
            "llm response received"
        );

        if tool_calls.is_empty() {
            if !text.trim().is_empty() {
                return Ok(TurnOutput::complete(flag_truncated_output(
                    text,
                    response.usage.completion_tokens,
                    context.settings.llm.llm_max_output,
                )));
            }
            // Empty content + no tool calls — model stuck. Nudge once;
            // on second strike, fall through to the no-tools wrap-up.
            empty_count += 1;
            if entered_tool_chain && !tool_model_disabled && router.config().has_tool_model() {
                tool_model_disabled = true;
                tracing::warn!(
                    tool_model = %router.config().tool_model(),
                    "tool model returned empty; falling back to the base model for this chain"
                );
                request.messages.push(ChatMessage::user(
                    "[The tool model returned an empty response. Continue the task from the tool results above.]"
                        .to_string(),
                ));
                continue;
            }
            if empty_count >= MAX_EMPTY_RESPONSES {
                tracing::warn!(
                    empty_count,
                    "model stuck on empty responses, forcing wrap-up"
                );
                circuit_breaker_reason = Some(format!(
                    "empty_response_stall ({empty_count} empty responses)"
                ));
                break;
            }
            tracing::warn!(empty_count, "empty response, nudging model");
            request.messages.push(ChatMessage::user(
                "[You returned an empty response. Respond to the user with what you know so far.]"
                    .to_string(),
            ));
            continue;
        }

        empty_count = 0;
        // We have tool calls: execute the base model's call, and from the next
        // model call onward run the rest of this chain on the dedicated tool
        // model (if configured). Log the handoff once, the first time we cross
        // into a tool chain this turn.
        if !entered_tool_chain {
            entered_tool_chain = true;
            let router = context
                .router
                .read()
                .map_err(|error| AgentError::Llm(anyhow!("router lock poisoned: {error}")))?;
            if router.config().has_tool_model() && !tool_model_disabled {
                tracing::info!(
                    tool_model = %router.config().tool_model(),
                    "tool call detected — switching to the tool model for the rest of the chain"
                );
            }
        }
        let billable = tool_calls
            .iter()
            .filter(|call| !is_free_tool(&call.fn_name))
            .count();
        total_tool_calls += billable;

        if !text.trim().is_empty() {
            last_text = text.clone();
        }
        if record_tool_messages {
            context.memory.messages.add(
                MessageRole::Assistant,
                &text,
                Some(json!({ "tool_calls": tool_calls_metadata(&tool_calls) })),
            )?;
        }
        request
            .messages
            .push(assistant_tool_message(text, tool_calls.clone()));

        let mut image_views = Vec::new();
        let mut turn_had_successful_tool = false;
        for call in tool_calls {
            let call_id = call.call_id.clone();
            let tool_name = call.fn_name.clone();
            let args_string = call.fn_arguments.to_string();
            tracing::info!(
                iteration,
                tool = %tool_name,
                call_id = %call_id,
                args = %truncate_log_text(&args_string, 1200),
                "tool call started"
            );

            let observer_for_tool = registry.turn_observer().cloned();
            if let Some(observer) = observer_for_tool.as_ref() {
                observer.on_tool_start(
                    &tool_name,
                    &call_id,
                    &truncate_chars(&args_string, TOOL_PREVIEW_CHARS),
                );
            }
            let tool_started_at = std::time::Instant::now();

            let signature = format!("{tool_name}:{args_string}");
            let should_stop_after_tool =
                matches!(call.fn_name.as_str(), "terminate" | "restart_self");
            let raw_result = if call.fn_name == "request_tool" {
                request_tool_for_turn(&registry, &mut active_tools, &call.fn_arguments)
            } else if registry.tool_is_active(&call.fn_name, &active_tools) {
                let inner: BoxToolFuture<'_> = Box::pin(registry.execute_async_with_call_id(
                    &call.fn_name,
                    &call.fn_arguments,
                    Some(&call_id),
                ));
                match registry.turn_observer() {
                    Some(observer) => observer.wrap_tool_call(&call.fn_name, inner).await,
                    None => inner.await,
                }
            } else if registry.tool_is_available(&call.fn_name) {
                format!(
                    "Tool '{}' is available but not loaded. Call request_tool(name=\"{}\") first.",
                    call.fn_name, call.fn_name
                )
            } else {
                format!("Unknown tool: {}", call.fn_name)
            };
            let (result, views) = extract_image_views(raw_result);
            // Identical-call degeneration: small models re-issue the same call
            // (observed: 5x the same web_search) until the circuit breaker cuts
            // the whole turn. The streak only grows when call AND result are
            // identical — polling whose output changes is real work, not
            // degeneration. Tell the model in the result itself, where it
            // actually looks, before the breaker has to fire.
            let repeated_tool_call_streak = repeat_tracker.observe(&signature, &result);
            let result = if repeated_tool_call_streak >= 2 {
                format!(
                    "{result}\n\n[You have made this exact call {repeated_tool_call_streak} times — \
                     the result does not change. Do not repeat it; continue with what you have.]"
                )
            } else {
                result
            };
            tracing::info!(
                iteration,
                tool = %tool_name,
                call_id = %call_id,
                result_chars = result.chars().count(),
                result = %truncate_log_text(&result, 1200),
                "tool call completed"
            );
            image_views.extend(views);

            let is_error = is_error_result(&result);
            if let Some(observer) = observer_for_tool.as_ref() {
                observer.on_tool_end(
                    &tool_name,
                    &call_id,
                    !is_error,
                    &truncate_chars(&result, TOOL_PREVIEW_CHARS),
                    tool_started_at.elapsed().as_millis(),
                );
            }
            if is_error {
                total_tool_errors += 1;
                tool_error_pressure += if is_transient_error(&result) { 1 } else { 2 };
            } else {
                turn_had_successful_tool = true;
            }
            if !skip_tool_log(&tool_name) {
                tool_log.push(ToolLogEntry {
                    name: tool_name.clone(),
                    args_preview: truncate_chars(&args_string, 100),
                    result_preview: truncate_chars(&result, 200),
                    success: !is_error,
                });
            }

            if record_tool_messages {
                context.memory.messages.add(
                    MessageRole::Tool,
                    &result,
                    Some(json!({
                        "tool_call_id": call.call_id.clone(),
                        "name": call.fn_name.clone(),
                    })),
                )?;
            }
            let stop_result = result.clone();
            request
                .messages
                .push(ChatMessage::from(ToolResponse::new(call.call_id, result)));
            if should_stop_after_tool {
                return Ok(TurnOutput::complete(stop_result));
            }

            if tool_error_pressure >= MAX_TOOL_ERROR_PRESSURE {
                circuit_breaker_reason = Some(format!(
                    "tool_error_cap hit ({total_tool_errors} errors, weighted pressure {tool_error_pressure} >= {MAX_TOOL_ERROR_PRESSURE})"
                ));
                break;
            }
            if repeated_tool_call_streak >= MAX_REPEATED_TOOL_CALLS {
                circuit_breaker_reason = Some(format!(
                    "repeated_tool_call_cap hit ({repeated_tool_call_streak} >= {MAX_REPEATED_TOOL_CALLS})"
                ));
                break;
            }
        }

        if turn_had_successful_tool {
            no_progress_turns = 0;
        } else {
            no_progress_turns += 1;
            if circuit_breaker_reason.is_none() && no_progress_turns >= MAX_NO_PROGRESS_TURNS {
                circuit_breaker_reason = Some(format!(
                    "no_progress_cap hit ({no_progress_turns} turns >= {MAX_NO_PROGRESS_TURNS})"
                ));
            }
        }

        for image_view in image_views {
            request.messages.push(image_view_message(image_view));
        }

        if let Some(reason) = &circuit_breaker_reason {
            tracing::warn!(reason = %reason, "circuit breaker triggered");
            request.messages.push(ChatMessage::user(
                "[Circuit breaker triggered. Stop tool exploration. Respond with a concise partial report now.]"
                    .to_string(),
            ));
            break;
        }
    }

    tracing::warn!(
        total_tool_calls,
        total_tool_errors,
        tool_error_pressure,
        tool_log_entries = tool_log.len(),
        breaker = ?circuit_breaker_reason,
        "tool iteration limit / breaker — requesting final wrap-up response"
    );
    let stop_reason = circuit_breaker_reason
        .clone()
        .unwrap_or_else(|| format!("tool_iteration_cap ({MAX_TOOL_ITERATIONS} iterations)"));
    let prompts = crate::llm::prompts::PromptStore::new(
        &context.settings.paths.workspace_dir,
        &context.settings.paths.config_dir,
    );
    let nudge = wrap_up_nudge(&prompts, total_tool_calls, &stop_reason);
    request.messages.push(ChatMessage::user(nudge));
    request.tools = None;
    let router = context
        .router
        .read()
        .map_err(|error| AgentError::Llm(anyhow!("router lock poisoned: {error}")))?
        .clone();
    // This wrap-up reply follows a tool chain, so keep it on the tool model when
    // one is configured (the strong model writes the final answer).
    let wrap_model = if entered_tool_chain && router.config().has_tool_model() {
        router.config().tool_model().to_string()
    } else {
        router.config().model_for(use_aux).to_string()
    };
    // Stream the wrap-up like any loop iteration: it IS the user-facing answer
    // for this turn, and a non-streaming call here meant minutes of dead air
    // followed by a reply the streaming UI had no deltas for.
    let observer_for_stream = registry.turn_observer().cloned();
    let response = match observer_for_stream {
        Some(observer) => {
            let observer_reasoning = observer.clone();
            let on_delta = move |chunk: &str| observer.on_assistant_delta(chunk);
            let on_reasoning = move |chunk: &str| observer_reasoning.on_reasoning_delta(chunk);
            router
                .exec_chat_request_stream_with_model(
                    request,
                    &wrap_model,
                    &on_delta,
                    Some(&on_reasoning),
                )
                .await?
        }
        None => {
            router
                .exec_chat_request_with_model(request, &wrap_model)
                .await?
        }
    };
    if let Some(prompt_tokens) = response.usage.prompt_tokens {
        context
            .last_prompt_tokens
            .store(prompt_tokens as u64, Ordering::Relaxed);
    }
    let final_text = response.first_text().unwrap_or_default().to_string();
    let text = if !final_text.trim().is_empty() {
        flag_truncated_output(
            final_text,
            response.usage.completion_tokens,
            context.settings.llm.llm_max_output,
        )
    } else if !last_text.trim().is_empty() {
        last_text
    } else {
        "Task processing limit reached. The work done so far has been saved.".to_string()
    };
    Ok(TurnOutput {
        text,
        stop_reason: Some(stop_reason),
    })
}

/// The forced wrap-up is not just "stop talking" — it demands a resumable
/// checkpoint. The reply is persisted (conversation history for the cortex,
/// the actor's own message log for subagents), so a structured GOAL / DONE /
/// REMAINING / NEXT block is what lets the next turn — or a successor agent —
/// pick the work up instead of re-deriving it. Text lives in the overridable
/// `tool_loop_wrap_up` template.
fn wrap_up_nudge(
    prompts: &crate::llm::prompts::PromptStore,
    total_tool_calls: usize,
    stop_reason: &str,
) -> String {
    let mut variables = std::collections::HashMap::new();
    variables.insert("tool_calls".to_string(), total_tool_calls.to_string());
    variables.insert("stop_reason".to_string(), stop_reason.to_string());
    prompts
        .render(
            "tool_loop_wrap_up",
            &variables,
            "[WRAP UP ({stop_reason}) after {tool_calls} tool calls. No more tool calls. \
             Respond NOW with a resumable checkpoint: GOAL, DONE, REMAINING, NEXT.]",
        )
        .text
}

/// The genai layer doesn't surface `finish_reason`, so detect output-limit
/// truncation from usage instead: a completion that consumed the entire output
/// budget was almost certainly cut mid-thought. Mark it so the user (and the
/// model, via history) can tell a truncated reply from a complete one.
fn flag_truncated_output(text: String, completion_tokens: Option<i32>, max_output: u32) -> String {
    match completion_tokens {
        Some(used) if used >= max_output as i32 => {
            tracing::warn!(
                used,
                max_output,
                "completion consumed the full output budget — reply likely truncated"
            );
            format!("{text}\n\n*[reply cut off at the output token limit]*")
        }
        _ => text,
    }
}

pub(super) fn actor_turn_instruction(spec: &ActorRunSpec) -> String {
    let inbox = if spec.has_pending_messages {
        "You have pending inbox messages in the actor context. Account for them before acting."
    } else {
        "No pending inbox messages are visible beyond the actor context."
    };
    if spec.turn_number == 1 {
        format!(
            "Begin your actor task now. {inbox}\nUse tools as needed. If you finish, call terminate(result=..., outcome=\"success\"). This is turn {}/{}.",
            spec.turn_number, spec.max_turns
        )
    } else {
        format!(
            "Continue your actor task. {inbox}\nReport progress with send_message(..., channel=\"task_update\", kind=\"progress\") when useful, and call terminate(...) when done. This is turn {}/{}.",
            spec.turn_number, spec.max_turns
        )
    }
}

fn request_tool_for_turn(
    registry: &ToolRegistry<'_>,
    active_tools: &mut HashSet<String>,
    args: &Value,
) -> String {
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if name.is_empty() {
        return "Error: tool name is required.".to_string();
    }
    if !registry.tool_is_available(name) {
        let available = registry.requestable_tool_names().join(", ");
        return format!("Unknown tool: {name}. Available extended tools: {available}");
    }
    if registry.tool_is_active(name, active_tools) {
        return format!("Tool '{name}' is already available. You can use it now.");
    }
    active_tools.insert(name.to_string());
    let siblings = registry.group_siblings(name);
    if siblings.is_empty() {
        return format!("Tool '{name}' is now available. You can use it in the next tool call.");
    }
    for sibling in &siblings {
        active_tools.insert(sibling.clone());
    }
    format!(
        "Tool '{name}' is now available, together with its whole family: {}. You can use them from the next tool call on without further request_tool calls.",
        siblings.join(", ")
    )
}

fn truncate_log_text(value: &str, limit: usize) -> String {
    let mut truncated = value.chars().take(limit).collect::<String>();
    if value.chars().count() > limit {
        truncated.push_str("...[truncated]");
    }
    truncated
}

fn assistant_tool_message(text: String, tool_calls: Vec<ToolCall>) -> ChatMessage {
    let mut parts = Vec::new();
    if !text.trim().is_empty() {
        parts.push(ContentPart::Text(text));
    }
    parts.extend(tool_calls.into_iter().map(ContentPart::ToolCall));
    ChatMessage::assistant(MessageContent::from_parts(parts))
}

pub(super) fn tool_calls_metadata(tool_calls: &[ToolCall]) -> Vec<Value> {
    tool_calls
        .iter()
        .map(|call| {
            let mut entry = json!({
                "id": call.call_id,
                "type": "function",
                "function": {
                    "name": call.fn_name,
                    "arguments": call.fn_arguments.to_string(),
                },
                "call_id": call.call_id,
                "fn_name": call.fn_name,
                "fn_arguments": call.fn_arguments,
            });
            // Carry thought_signatures (Gemini thinking) through history so
            // multi-turn reasoning chains stay connected across persistence.
            if let Some(signatures) = &call.thought_signatures
                && !signatures.is_empty()
                && let Some(map) = entry.as_object_mut()
            {
                map.insert("thought_signatures".to_string(), json!(signatures));
            }
            entry
        })
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ImageView {
    pub path: String,
    pub attachment: LlmAttachment,
}

pub(super) fn extract_image_views(result: String) -> (String, Vec<ImageView>) {
    let Ok(mut value) = serde_json::from_str::<Value>(&result) else {
        return (result, vec![]);
    };
    let Some(object) = value.as_object_mut() else {
        return (result, vec![]);
    };
    let Some(image) = object.remove("_image_view") else {
        return (result, vec![]);
    };

    let Some(data) = image.get("data").and_then(Value::as_str) else {
        return (json_without_image_view(value, result), vec![]);
    };
    let Some(mime_type) = image.get("mime_type").and_then(Value::as_str) else {
        return (json_without_image_view(value, result), vec![]);
    };
    let path = image
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("image")
        .to_string();
    let name = image
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            std::path::Path::new(&path)
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
        });
    let attachment = LlmAttachment {
        content_type: mime_type.to_string(),
        base64_content: data.to_string(),
        name,
    };
    (
        json_without_image_view(value, result),
        vec![ImageView { path, attachment }],
    )
}

fn json_without_image_view(value: Value, fallback: String) -> String {
    serde_json::to_string(&value).unwrap_or(fallback)
}

pub(super) fn image_view_message(image: ImageView) -> ChatMessage {
    ChatMessage::user(MessageContent::from_parts(vec![
        ContentPart::Text(format!("[Image from {}]", image.path)),
        ContentPart::from_binary_base64(
            image.attachment.content_type,
            image.attachment.base64_content,
            image.attachment.name,
        ),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recover_text_tool_calls_parses_gemma_style() {
        let content = "let me check this. <tool_call:read_file{file_path:\"/tmp/foo.txt\"}> done.";
        let calls = recover_text_tool_calls(content);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].fn_name, "read_file");
        assert_eq!(
            calls[0]
                .fn_arguments
                .get("file_path")
                .and_then(Value::as_str),
            Some("/tmp/foo.txt")
        );
    }

    #[test]
    fn recover_text_tool_calls_handles_escaped_quotes() {
        let content = r#"<tool_call:grep_search{query:<|"|>foo<|"|>, path:<|"|>src<|"|>}>"#;
        let calls = recover_text_tool_calls(content);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].fn_name, "grep_search");
        assert_eq!(
            calls[0].fn_arguments.get("query").and_then(Value::as_str),
            Some("foo")
        );
        assert_eq!(
            calls[0].fn_arguments.get("path").and_then(Value::as_str),
            Some("src")
        );
    }

    #[test]
    fn recover_text_tool_calls_returns_empty_for_plain_text() {
        assert!(recover_text_tool_calls("just a normal sentence").is_empty());
    }

    #[test]
    fn recover_text_tool_calls_parses_bracket_prompt_style() {
        // Verbatim failure observed in prod 2026-06-10: gemma imitated the
        // bracket notation formerly used in agent_instructions.md examples.
        let content = "hey! ❤️\n\n---\n\nlet me check my current state...\n\n---\n\n[tool_call: bash(command=\"lethe --version\")]";
        let calls = recover_text_tool_calls(content);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].fn_name, "bash");
        assert_eq!(
            calls[0].fn_arguments.get("command").and_then(Value::as_str),
            Some("lethe --version")
        );
    }

    #[test]
    fn recover_text_tool_calls_bracket_handles_multiple_and_escaped_args() {
        let content = r#"[tool_call: edit_file(path="run.ts", new_string="const x = \"y\";")]"#;
        let calls = recover_text_tool_calls(content);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].fn_name, "edit_file");
        assert_eq!(
            calls[0].fn_arguments.get("path").and_then(Value::as_str),
            Some("run.ts")
        );
        assert_eq!(
            calls[0]
                .fn_arguments
                .get("new_string")
                .and_then(Value::as_str),
            Some(r#"const x = "y";"#)
        );
    }

    #[test]
    fn recover_text_tool_calls_skips_bracket_placeholder_quotes() {
        // Prose mentioning the notation with elided args must not execute.
        let calls = recover_text_tool_calls("never write [tool_call: edit_file(...)] as text");
        assert!(calls.is_empty());
    }

    #[test]
    fn strip_text_tool_call_markers_removes_call_and_orphaned_separators() {
        let content = "let me check my current state...\n\n---\n\n[tool_call: bash(command=\"lethe --version\")]";
        assert_eq!(
            strip_text_tool_call_markers(content),
            "let me check my current state..."
        );
    }

    #[test]
    fn free_tool_carve_out_recognizes_memory_and_telegram() {
        assert!(is_free_tool("archival_search"));
        assert!(is_free_tool("note_search"));
        assert!(is_free_tool("telegram_send_message"));
        assert!(is_free_tool("terminate"));
        assert!(!is_free_tool("bash"));
        assert!(!is_free_tool("read_file"));
        assert!(!is_free_tool("edit_file"));
    }

    #[test]
    fn is_error_result_detects_standard_error_prefixes() {
        assert!(is_error_result("Error: file not found"));
        assert!(is_error_result("Unknown tool: foo"));
        assert!(is_error_result("Exit code: 1\n✗ invalid selector"));
        assert!(is_error_result("✗ command failed"));
        assert!(is_error_result(
            r#"{"status":"error","message":"browser failed"}"#
        ));
        assert!(is_error_result(r#"{"ok":false,"error":"stale ref"}"#));
        assert!(is_error_result(r#"{"error":"browser failed"}"#));
        assert!(!is_error_result(r#"{"ok":true,"error":null}"#));
        assert!(!is_error_result(r#"{"status":"OK","message":"opened"}"#));
        assert!(!is_error_result("Successfully wrote 12 bytes"));
        assert!(!is_error_result(""));
    }

    #[test]
    fn tool_loop_budget_counts_schemas_and_drops_old_exchanges() {
        let mut request = genai::chat::ChatRequest::new(vec![
            ChatMessage::system("system"),
            ChatMessage::user("current request"),
            ChatMessage::assistant("old assistant output that is intentionally long"),
            ChatMessage::from(ToolResponse::new(
                "call-1",
                "old tool result that is intentionally long",
            )),
            ChatMessage::assistant("latest answer"),
        ]);
        request.tools = Some(vec![
            genai::chat::Tool::new("large_tool").with_description("x".repeat(80)),
        ]);
        let mut current_user_index = 1;
        let dropped = clamp_chat_request_to_budget(&mut request, &mut current_user_index, 125);
        assert!(dropped >= 2, "old assistant/tool pair should be removed");
        assert_eq!(request.messages[0].role, ChatRole::System);
        assert_eq!(request.messages[current_user_index].role, ChatRole::User);
        assert_eq!(request.messages.last().unwrap().role, ChatRole::Assistant);
    }

    #[test]
    fn transient_errors_are_distinguished_from_permanent_ones() {
        // Transient: network/timeout/rate-limit shapes.
        assert!(is_transient_error("Error: request timed out after 30s"));
        assert!(is_transient_error("Error: connection reset by peer"));
        assert!(is_transient_error("Error: HTTP 429 Too Many Requests"));
        assert!(is_transient_error("Error: upstream returned 503"));
        // Permanent: caller mistakes don't get the discount.
        assert!(!is_transient_error("Error: file not found"));
        assert!(!is_transient_error("Error: invalid arguments"));
        assert!(!is_transient_error("Unknown tool: frobnicate"));
        // Success text is never an error, transient or otherwise.
        assert!(!is_transient_error("downloaded in 3s (no timeout)"));
    }

    #[test]
    fn transient_errors_weigh_half_against_the_breaker() {
        // 8 permanent errors trip the old cap; 8 transient ones do not.
        let permanent: usize = (0..8).map(|_| 2).sum();
        let transient: usize = (0..8).map(|_| 1).sum();
        assert!(permanent >= MAX_TOOL_ERROR_PRESSURE);
        assert!(transient < MAX_TOOL_ERROR_PRESSURE);
    }

    #[test]
    fn repeat_tracker_counts_only_identical_call_and_result() {
        let mut tracker = RepeatTracker::default();

        // True degeneration: same call, same output → the streak grows one
        // per observation and reaches the breaker threshold.
        let mut streak = 0;
        for expected in 1..=MAX_REPEATED_TOOL_CALLS {
            streak = tracker.observe("web_search:{\"q\":\"rust\"}", "10 results");
            assert_eq!(streak, expected);
        }
        assert!(
            streak >= MAX_REPEATED_TOOL_CALLS,
            "degenerate repeats must trip the breaker"
        );

        // Legitimate polling: same call, changing output → streak never grows,
        // so the breaker doesn't kill a poll loop that is observing progress.
        let mut poller = RepeatTracker::default();
        assert_eq!(
            poller.observe("bash:{\"cmd\":\"job-status\"}", "running 10%"),
            1
        );
        assert_eq!(
            poller.observe("bash:{\"cmd\":\"job-status\"}", "running 40%"),
            1
        );
        assert_eq!(
            poller.observe("bash:{\"cmd\":\"job-status\"}", "running 90%"),
            1
        );
        assert_eq!(poller.observe("bash:{\"cmd\":\"job-status\"}", "done"), 1);

        // A different call resets the streak.
        assert_eq!(tracker.observe("read_file:{\"p\":\"a\"}", "contents"), 1);
    }

    #[test]
    fn wrap_up_nudge_demands_resumable_checkpoint() {
        // Empty paths → the embedded tool_loop_wrap_up template is used.
        let prompts = crate::llm::prompts::PromptStore::new("", "");
        let nudge = wrap_up_nudge(&prompts, 50, "tool_iteration_cap (50 iterations)");
        assert!(nudge.contains("GOAL"));
        assert!(nudge.contains("DONE"));
        assert!(nudge.contains("REMAINING"));
        assert!(nudge.contains("NEXT"));
        assert!(nudge.contains("tool_iteration_cap"));
        assert!(nudge.contains("50 tool calls"));
        assert!(nudge.contains("No more tool calls"));
        // All variables substituted — no leftover placeholders.
        assert!(!nudge.contains('{'));

        let breaker_nudge = wrap_up_nudge(
            &prompts,
            12,
            "tool_error_cap hit (6 errors, weighted pressure 12 >= 16)",
        );
        assert!(breaker_nudge.contains("tool_error_cap"));
    }
}
