//! LLM-driven rolling summary of conversation turns that have aged out of
//! the live history window. Mirrors the Python `main` branch's Pass-2
//! compaction: when [`compact_history`](super::compact_history) drops
//! messages, the agent ships them through [`update_conversation_summary`]
//! which merges them into the persistent `conversation_summary` memory
//! block. The next turn's prompt includes that summary as a dedicated
//! `<conversation_summary>` block so the model never loses the thread of
//! older work, even when raw turns scroll off.

use std::sync::{Arc, RwLock};

use anyhow::{Result, anyhow};

use crate::llm::prompts::PromptStore;
use crate::llm::{LlmMessage, LlmRouter};
use crate::memory::MemoryStore;

/// Minimum char weight of dropped content that justifies an LLM round-trip
/// to update the summary. Below this we skip the call entirely — the
/// information loss is too small to justify the latency and tokens.
pub(super) const MIN_DROPPED_CHARS_FOR_SUMMARY: usize = 1_500;

/// Run the LLM summarizer against the dropped batch and merge the result
/// into the persistent `conversation_summary` block. Returns the new summary
/// text on success, or `None` when the drop was too small to summarize.
pub(super) async fn update_conversation_summary(
    memory: &MemoryStore,
    prompts: &PromptStore,
    router: Arc<RwLock<LlmRouter>>,
    dropped: &[LlmMessage],
) -> Result<Option<String>> {
    if dropped_chars(dropped) < MIN_DROPPED_CHARS_FOR_SUMMARY {
        return Ok(None);
    }
    let existing = memory
        .conversation_summary()
        .map_err(|error| anyhow!("failed to load conversation summary: {error}"))?;
    let new_summary = call_summarizer(prompts, router, &existing, dropped).await?;
    let new_summary = new_summary.trim().to_string();
    if new_summary.is_empty() {
        return Ok(None);
    }
    memory
        .set_conversation_summary(&new_summary)
        .map_err(|error| anyhow!("failed to persist conversation summary: {error}"))?;
    Ok(Some(new_summary))
}

fn dropped_chars(messages: &[LlmMessage]) -> usize {
    messages
        .iter()
        .map(|message| {
            message.content.chars().count()
                + message
                    .tool_responses
                    .iter()
                    .map(|response| response.content.chars().count())
                    .sum::<usize>()
        })
        .sum()
}

async fn call_summarizer(
    prompts: &PromptStore,
    router: Arc<RwLock<LlmRouter>>,
    existing: &str,
    dropped: &[LlmMessage],
) -> Result<String> {
    let system_text = prompts
        .load(
            "llm_summarize_system",
            "You are a context summarization assistant. Output ONLY the structured summary.",
        )
        .text;
    let user_text = build_user_payload(prompts, existing, dropped);
    let messages = vec![LlmMessage::system(system_text), LlmMessage::user(user_text)];
    let router = router
        .read()
        .map_err(|error| anyhow!("router lock poisoned: {error}"))?
        .clone();
    // Aux model is the cheap one — summarization doesn't need the main model.
    router.complete(messages, true).await
}

fn build_user_payload(prompts: &PromptStore, existing: &str, dropped: &[LlmMessage]) -> String {
    let template_name = if existing.trim().is_empty() {
        "llm_summarize"
    } else {
        "llm_summarize_update"
    };
    let instructions = prompts
        .load(template_name, "Summarize the conversation below.")
        .text;

    let mut payload = String::new();
    payload.push_str(instructions.trim());
    payload.push_str("\n\n");
    if !existing.trim().is_empty() {
        payload.push_str("<previous-summary>\n");
        payload.push_str(existing.trim());
        payload.push_str("\n</previous-summary>\n\n");
    }
    payload.push_str("<new-messages>\n");
    for message in dropped {
        payload.push_str(&render_dropped_message(message));
        payload.push('\n');
    }
    payload.push_str("</new-messages>\n");
    payload
}

fn render_dropped_message(message: &LlmMessage) -> String {
    use crate::llm::LlmRole;
    let role_label = match message.role {
        LlmRole::User => "user",
        LlmRole::Assistant => "assistant",
        LlmRole::System => "system",
    };
    let mut body = message.content.clone();
    if !message.tool_calls.is_empty() {
        let calls = message
            .tool_calls
            .iter()
            .map(|call| format!("{}({})", call.fn_name, call.fn_arguments))
            .collect::<Vec<_>>()
            .join("; ");
        if !body.trim().is_empty() {
            body.push('\n');
        }
        body.push_str(&format!("[tool_calls: {calls}]"));
    }
    if !message.tool_responses.is_empty() {
        let results = message
            .tool_responses
            .iter()
            .map(|response| {
                let preview = crate::llm::truncate::truncate_with_ellipsis(&response.content, 400);
                format!("{}: {}", response.call_id, preview)
            })
            .collect::<Vec<_>>()
            .join("\n  ");
        body.push_str(&format!("\n[tool_results:\n  {results}\n]"));
    }
    format!("{role_label}: {body}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{HistoricalToolCall, HistoricalToolResponse};
    use serde_json::json;

    #[test]
    fn dropped_chars_counts_content_and_tool_responses() {
        let messages = vec![
            LlmMessage::user("a".repeat(100)),
            LlmMessage::tool_results(vec![HistoricalToolResponse {
                call_id: "c".into(),
                content: "b".repeat(200),
                source_message_id: None,
            }]),
        ];
        assert_eq!(dropped_chars(&messages), 300);
    }

    #[test]
    fn render_dropped_message_includes_tool_calls_and_results() {
        let mut msg = LlmMessage::assistant_with_tool_calls(
            "reading the file",
            vec![HistoricalToolCall {
                call_id: "c1".into(),
                fn_name: "read_file".into(),
                fn_arguments: json!({"path": "foo.txt"}),
                thought_signatures: None,
            }],
        );
        msg.tool_responses = vec![]; // assistant message, only calls
        let rendered = render_dropped_message(&msg);
        assert!(rendered.starts_with("assistant: "));
        assert!(rendered.contains("read_file"));
        assert!(rendered.contains("foo.txt"));
    }
}
