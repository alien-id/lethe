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
use genai::chat::{ChatMessage, ContentPart, MessageContent, ToolCall, ToolResponse};
use regex::Regex;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::actor::{ActorError, ActorRunSpec, ActorRuntime, ActorTurnExecutor, ModelTier};
use crate::config::Settings;
use crate::llm::{LlmAttachment, LlmMessage, LlmRouter, build_chat_request};
use crate::memory::MessageRole;
use crate::memory::MemoryStore;
use crate::tools::registry::{ActorToolContext, BoxToolFuture, ToolRegistry, ToolRuntime};
use crate::tools::shell::ShellTools;

use super::{AgentError, AgentResult};

/// Max LLM/tool round-trips per turn. Python `main` runs up to 10 per batch
/// with up to 5 auto-continuation batches (~50 total); we collapse that into
/// a single flat cap.
pub(super) const MAX_TOOL_ITERATIONS: usize = 50;
const MAX_TOOL_ERRORS: usize = 8;
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
    "note_search",
    "note_get",
    // Telegram
    "telegram_send_message",
    "telegram_send_file",
    "telegram_react",
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
const SKIP_TOOL_LOG_TOOLS: &[&str] = &["conversation_search", "archival_search"];

fn is_free_tool(name: &str) -> bool {
    FREE_TOOL_NAMES.contains(&name)
}

fn skip_tool_log(name: &str) -> bool {
    SKIP_TOOL_LOG_TOOLS.contains(&name)
}

fn is_error_result(result: &str) -> bool {
    result.starts_with("Error:") || result.starts_with("Unknown tool:")
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

fn truncate_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    value.chars().take(limit).collect()
}

/// Recover Gemma/llama.cpp text-embedded tool calls of the form
/// `<tool_call:func_name{key:"value", ...}>`. Returns native [`ToolCall`]s
/// when at least one is matched. Matches Python's `_extract_text_tool_calls`.
fn recover_text_tool_calls(content: &str) -> Vec<ToolCall> {
    static OUTER: OnceLock<Regex> = OnceLock::new();
    static QUOTED: OnceLock<Regex> = OnceLock::new();
    let outer = OUTER
        .get_or_init(|| Regex::new(r"(?s)<tool_call:(\w+)\{(.+?)\}>").expect("valid regex"));
    let quoted =
        QUOTED.get_or_init(|| Regex::new(r#"(\w+):"([^"]*)""#).expect("valid regex"));

    let mut calls = Vec::new();
    for caps in outer.captures_iter(content) {
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
    calls
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
}

/// Build the actor turn executor that the [`ActorRuntime`] supervisor calls
/// to run each subagent turn. Wraps the standard tool loop with actor wiring.
pub(super) fn actor_turn_executor(
    settings: Settings,
    memory: Arc<MemoryStore>,
    router: Arc<RwLock<LlmRouter>>,
    shell: ShellTools,
    last_prompt_tokens: Arc<AtomicU64>,
) -> ActorTurnExecutor {
    let context = TurnExecutionContext {
        settings,
        memory,
        router,
        shell,
        last_prompt_tokens,
    };
    Arc::new(move |spec: ActorRunSpec, runtime: ActorRuntime| {
        let context = context.clone();
        Box::pin(async move {
            let tool_runtime = ToolRuntime {
                actor: Some(ActorToolContext {
                    runtime: runtime.clone(),
                    actor_id: spec.actor_id.clone(),
                    is_subagent: true,
                }),
                requested_tools: spec.requested_tools.clone(),
                ..ToolRuntime::default()
            };
            let messages = vec![
                LlmMessage::system(spec.system_prompt.clone()),
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
    runtime: ToolRuntime,
    use_aux: bool,
    record_tool_messages: bool,
) -> AgentResult<String> {
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
    let mut last_text = String::new();
    let mut total_tool_calls: usize = 0;
    let mut total_tool_errors: usize = 0;
    let mut repeated_tool_call_streak: usize = 0;
    let mut last_tool_signature = String::new();
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
        let model_id = if entered_tool_chain && router.config().has_tool_model() {
            router.config().tool_model().to_string()
        } else {
            router.config().model_for(use_aux).to_string()
        };
        let observer_for_stream = registry.turn_observer().cloned();
        let response = match observer_for_stream {
            Some(observer) => {
                let on_delta = move |chunk: &str| observer.on_assistant_delta(chunk);
                router
                    .exec_chat_request_stream_with_model(request.clone(), &model_id, &on_delta)
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
        let text = response.first_text().unwrap_or_default().to_string();
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
                return Ok(text);
            }
            // Empty content + no tool calls — model stuck. Nudge once;
            // on second strike, fall through to the no-tools wrap-up.
            empty_count += 1;
            if empty_count >= MAX_EMPTY_RESPONSES {
                tracing::warn!(empty_count, "model stuck on empty responses, forcing wrap-up");
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
        // We have tool calls: from the next model call onward, run the rest of
        // this chain on the dedicated tool model (if configured). Log the
        // handoff once, the first time we cross into a tool chain this turn.
        if !entered_tool_chain {
            entered_tool_chain = true;
            let router = context
                .router
                .read()
                .map_err(|error| AgentError::Llm(anyhow!("router lock poisoned: {error}")))?;
            if router.config().has_tool_model() {
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
                    &truncate_chars(&args_string, 200),
                );
            }
            let tool_started_at = std::time::Instant::now();

            let signature = format!("{tool_name}:{args_string}");
            if signature == last_tool_signature {
                repeated_tool_call_streak += 1;
            } else {
                repeated_tool_call_streak = 1;
                last_tool_signature = signature;
            }

            let should_stop_after_tool =
                matches!(call.fn_name.as_str(), "terminate" | "restart_self");
            let raw_result = if call.fn_name == "request_tool" {
                request_tool_for_turn(&registry, &mut active_tools, &call.fn_arguments)
            } else if registry.tool_is_active(&call.fn_name, &active_tools) {
                let inner: BoxToolFuture<'_> =
                    Box::pin(registry.execute_async(&call.fn_name, &call.fn_arguments));
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
                    &truncate_chars(&result, 200),
                    tool_started_at.elapsed().as_millis(),
                );
            }
            if is_error {
                total_tool_errors += 1;
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
                return Ok(stop_result);
            }

            if total_tool_errors >= MAX_TOOL_ERRORS {
                circuit_breaker_reason = Some(format!(
                    "tool_error_cap hit ({total_tool_errors} errors >= {MAX_TOOL_ERRORS})"
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
        tool_log_entries = tool_log.len(),
        breaker = ?circuit_breaker_reason,
        "tool iteration limit / breaker — requesting final wrap-up response"
    );
    let nudge = format!(
        "[WRAP UP: You've made {total_tool_calls} tool calls. \
         You MUST respond to the user NOW with what you have. \
         No more tool calls.]"
    );
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
    let response = router
        .exec_chat_request_with_model(request, &wrap_model)
        .await?;
    if let Some(prompt_tokens) = response.usage.prompt_tokens {
        context
            .last_prompt_tokens
            .store(prompt_tokens as u64, Ordering::Relaxed);
    }
    let final_text = response.first_text().unwrap_or_default().to_string();
    if !final_text.trim().is_empty() {
        Ok(final_text)
    } else if !last_text.trim().is_empty() {
        Ok(last_text)
    } else {
        Ok("Task processing limit reached. The work done so far has been saved.".to_string())
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
    format!("Tool '{name}' is now available. You can use it in the next tool call.")
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
        let content =
            "let me check this. <tool_call:read_file{file_path:\"/tmp/foo.txt\"}> done.";
        let calls = recover_text_tool_calls(content);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].fn_name, "read_file");
        assert_eq!(
            calls[0].fn_arguments.get("file_path").and_then(Value::as_str),
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
        assert!(!is_error_result("Successfully wrote 12 bytes"));
        assert!(!is_error_result(""));
    }
}
