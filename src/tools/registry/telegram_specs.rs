use serde_json::Value;

use crate::tools::registry::ToolRegistry;
use crate::tools::registry::args::{bool_arg, i64_arg, string_arg, string_arg_default};
use crate::tools::registry::egress::NO_EGRESS_ERROR;
use crate::tools::spec::{ToolCategory, ToolDef, ToolExecutor, p_bool, p_int, p_str, p_str_req};

fn exec_telegram_send_message(registry: &ToolRegistry<'_>, args: &Value) -> String {
    match registry.message_egress() {
        Some(egress) => {
            // Accept `reply_markup_json` as either a JSON string (the documented
            // form) or a raw object/array — models frequently emit the latter.
            let reply_markup_json = match args.get("reply_markup_json") {
                Some(Value::String(text)) => text.clone(),
                Some(value) if !value.is_null() => value.to_string(),
                _ => String::new(),
            };
            egress.send_message(
                &string_arg(args, "text"),
                &string_arg_default(args, "parse_mode", ""),
                (!reply_markup_json.trim().is_empty()).then_some(reply_markup_json.as_str()),
            )
        }
        None => NO_EGRESS_ERROR.to_string(),
    }
}

fn exec_telegram_send_file(registry: &ToolRegistry<'_>, args: &Value) -> String {
    match registry.message_egress() {
        Some(egress) => egress.send_file(
            &string_arg(args, "file_path_or_url"),
            &string_arg_default(args, "caption", ""),
            bool_arg(args, "as_document", false),
        ),
        None => NO_EGRESS_ERROR.to_string(),
    }
}

fn exec_telegram_react(registry: &ToolRegistry<'_>, args: &Value) -> String {
    match registry.message_egress() {
        Some(egress) => egress.react(
            &string_arg_default(args, "emoji", "👍"),
            i64_arg(args, "message_id", 0),
        ),
        None => NO_EGRESS_ERROR.to_string(),
    }
}

pub const TOOL_DEFS: &[ToolDef] = &[
    // Transport-neutral chat egress for client (web/desktop) sessions. Shares
    // the executor with telegram_send_message — MessageEgress routes to the
    // attached transport. Only send_message is exposed for clients: the chat
    // UIs render intermediate `text` events, but drop `file`/`reaction`
    // events, so those tools would be silent no-ops outside Telegram.
    ToolDef {
        name: "chat_send_message",
        description: "Send an extra message to the user's chat during a long task — it appears immediately, before your final answer. Use it for progress notes on multi-minute work.",
        params: &[
            p_str_req("text", "Message text (markdown)."),
            p_str("parse_mode", "markdown, html, or empty."),
        ],
        category: ToolCategory::TransportClient,
        execute: ToolExecutor::Sync(exec_telegram_send_message),
    },
    ToolDef {
        name: "telegram_send_message",
        description: "Send an extra Telegram message during a long task. Optionally attach Telegram reply_markup_json. Use reply keyboards for short visible replies; they default to one_time_keyboard=true unless explicitly set, should usually include resize_keyboard=true, and are removed after a matching button text arrives (example: {\"keyboard\":[[{\"text\":\"Yes\"},{\"text\":\"No\"}]],\"resize_keyboard\":true,\"one_time_keyboard\":true}). Use inline keyboards for message-scoped actions with short non-secret callback_data; callbacks are consumed and buttons are removed after press (example: {\"inline_keyboard\":[[{\"text\":\"Start\",\"callback_data\":\"start_now\"}]]}).",
        params: &[
            p_str_req("text", "Message text."),
            p_str("parse_mode", "markdown, html, or empty."),
            p_str(
                "reply_markup_json",
                "Optional Telegram reply_markup JSON string.",
            ),
        ],
        category: ToolCategory::Transport,
        execute: ToolExecutor::Sync(exec_telegram_send_message),
    },
    ToolDef {
        name: "telegram_send_file",
        description: "Send a file, image, video, audio, or URL to the chat.",
        params: &[
            p_str_req("file_path_or_url", "Local path or HTTP(S) URL."),
            p_str("caption", "Caption."),
            p_bool("as_document", "Force document upload."),
        ],
        category: ToolCategory::Transport,
        execute: ToolExecutor::Sync(exec_telegram_send_file),
    },
    ToolDef {
        name: "telegram_react",
        description: "React to the user's last Telegram message.",
        params: &[
            p_str("emoji", "Emoji."),
            p_int("message_id", "Message id (0 = last inbound)."),
        ],
        category: ToolCategory::Transport,
        execute: ToolExecutor::Sync(exec_telegram_react),
    },
];
