use std::collections::HashMap;

use chrono::{DateTime, Local, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Settings;
use crate::llm::prompts::PromptStore;

pub const DEFAULT_HEARTBEAT_INTERVAL_SECONDS: u64 = 60 * 60;
pub const FULL_CONTEXT_INTERVAL_SECONDS: u64 = 2 * 60 * 60;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatConfig {
    pub interval_seconds: u64,
    pub full_context_interval_seconds: u64,
    pub enabled: bool,
}

impl HeartbeatConfig {
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            interval_seconds: settings.background.heartbeat_interval_seconds,
            full_context_interval_seconds: FULL_CONTEXT_INTERVAL_SECONDS,
            enabled: settings.background.heartbeat_enabled,
        }
    }
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            interval_seconds: DEFAULT_HEARTBEAT_INTERVAL_SECONDS,
            full_context_interval_seconds: FULL_CONTEXT_INTERVAL_SECONDS,
            enabled: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatPrompt {
    pub message: String,
    pub use_full_context: bool,
    pub first_tick: bool,
    pub timestamp: String,
    pub heartbeat_count: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeartbeatAction {
    Empty,
    Idle,
    Internal,
    Send,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatOutcome {
    pub action: HeartbeatAction,
    pub message: String,
    pub idle_minutes: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct Heartbeat {
    config: HeartbeatConfig,
    last_full_context: Option<DateTime<Utc>>,
    heartbeat_count: u64,
    idle_minutes_accum: u64,
}

impl Heartbeat {
    pub fn new(config: HeartbeatConfig) -> Self {
        Self {
            config,
            last_full_context: None,
            heartbeat_count: 0,
            idle_minutes_accum: 0,
        }
    }

    pub fn config(&self) -> &HeartbeatConfig {
        &self.config
    }

    pub fn heartbeat_count(&self) -> u64 {
        self.heartbeat_count
    }

    pub fn idle_minutes_accum(&self) -> u64 {
        self.idle_minutes_accum
    }

    pub fn reset_idle_timer(&mut self) {
        self.idle_minutes_accum = 0;
    }

    pub fn trigger(
        &mut self,
        prompts: &PromptStore,
        reminders: &str,
        open_work: &str,
    ) -> HeartbeatPrompt {
        let now = Local::now();
        self.trigger_at(
            prompts,
            reminders,
            open_work,
            now.with_timezone(&Utc),
            &now.format("%Y-%m-%d %H:%M %Z").to_string(),
        )
    }

    pub fn trigger_at(
        &mut self,
        prompts: &PromptStore,
        reminders: &str,
        open_work: &str,
        now_utc: DateTime<Utc>,
        timestamp: &str,
    ) -> HeartbeatPrompt {
        let first_tick = self.heartbeat_count == 0;
        let use_full_context = self.should_use_full_context(now_utc);
        if use_full_context {
            self.last_full_context = Some(now_utc);
        }

        let mut variables = HashMap::new();
        variables.insert("timestamp".to_string(), timestamp.to_string());
        variables.insert("reminders".to_string(), format_reminder_block(reminders));
        variables.insert("open_work".to_string(), format_open_work_block(open_work));

        let (name, fallback) = if use_full_context {
            (
                "heartbeat_message_full",
                "[System: heartbeat - {timestamp}]\n\n{reminders}{open_work}\nFull review. Respond with typed JSON only.",
            )
        } else {
            (
                "heartbeat_message",
                "[System: heartbeat - {timestamp}]\n\n{reminders}{open_work}\nRespond with typed JSON only.",
            )
        };
        let message = prompts.render(name, &variables, fallback).text;

        HeartbeatPrompt {
            message,
            use_full_context,
            first_tick,
            timestamp: timestamp.to_string(),
            heartbeat_count: self.heartbeat_count,
        }
    }

    pub fn finish_response(&mut self, response: &str, evaluated: Option<&str>) -> HeartbeatOutcome {
        let first_tick = self.heartbeat_count == 0;
        let cleaned_response = strip_model_tags(response);
        let final_response = if let Some(evaluated) = evaluated {
            let cleaned = strip_model_tags(evaluated);
            if cleaned.trim().is_empty() {
                r#"{"action":"idle","message":""}"#.to_string()
            } else {
                cleaned
            }
        } else {
            cleaned_response
        };

        self.heartbeat_count += 1;

        let final_response = final_response.trim();
        if final_response.is_empty() {
            return HeartbeatOutcome {
                action: HeartbeatAction::Empty,
                message: String::new(),
                idle_minutes: None,
            };
        }

        if let Some(parsed) = parse_typed_response(final_response) {
            return self.finish_parsed_response(first_tick, parsed);
        }

        if final_response.eq_ignore_ascii_case("ok") {
            return self.finish_idle_response(first_tick);
        }

        // Untyped heartbeat prose is internal by default. This prevents
        // reflective background text ending in "ok" from leaking to the user.
        HeartbeatOutcome {
            action: HeartbeatAction::Internal,
            message: final_response.to_string(),
            idle_minutes: None,
        }
    }

    fn finish_parsed_response(
        &mut self,
        first_tick: bool,
        parsed: ParsedHeartbeatResponse,
    ) -> HeartbeatOutcome {
        match parsed.action {
            ParsedHeartbeatAction::Empty => HeartbeatOutcome {
                action: HeartbeatAction::Empty,
                message: String::new(),
                idle_minutes: None,
            },
            ParsedHeartbeatAction::Idle => self.finish_idle_response(first_tick),
            ParsedHeartbeatAction::Internal => HeartbeatOutcome {
                action: HeartbeatAction::Internal,
                message: parsed.message,
                idle_minutes: None,
            },
            ParsedHeartbeatAction::Send => {
                if parsed.message.trim().is_empty() {
                    return HeartbeatOutcome {
                        action: HeartbeatAction::Internal,
                        message: String::new(),
                        idle_minutes: None,
                    };
                }
                self.idle_minutes_accum = 0;
                HeartbeatOutcome {
                    action: HeartbeatAction::Send,
                    message: parsed.message.trim().to_string(),
                    idle_minutes: None,
                }
            }
        }
    }

    fn finish_idle_response(&mut self, first_tick: bool) -> HeartbeatOutcome {
        let idle_minutes = if first_tick {
            None
        } else {
            let interval_minutes = ((self.config.interval_seconds + 30) / 60).max(1);
            self.idle_minutes_accum += interval_minutes;
            Some(self.idle_minutes_accum)
        };
        HeartbeatOutcome {
            action: HeartbeatAction::Idle,
            message: String::new(),
            idle_minutes,
        }
    }

    fn should_use_full_context(&self, now_utc: DateTime<Utc>) -> bool {
        if self.config.full_context_interval_seconds == 0 {
            return false;
        }
        self.last_full_context.is_none_or(|last| {
            now_utc.signed_duration_since(last).num_seconds()
                >= self.config.full_context_interval_seconds as i64
        })
    }
}

pub fn render_summary_prompt(prompts: &PromptStore, response: &str) -> String {
    let mut variables = HashMap::new();
    variables.insert("response".to_string(), response.to_string());
    prompts
        .render(
            "heartbeat_summarize",
            &variables,
            "MESSAGE:\n{response}\n\nReply with typed heartbeat JSON only.",
        )
        .text
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParsedHeartbeatAction {
    Empty,
    Idle,
    Internal,
    Send,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedHeartbeatResponse {
    action: ParsedHeartbeatAction,
    message: String,
}

fn parse_typed_response(response: &str) -> Option<ParsedHeartbeatResponse> {
    let value = parse_json_object(response)?;
    let action = value
        .get("action")
        .or_else(|| value.get("type"))
        .or_else(|| value.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let message = value
        .get("message")
        .or_else(|| value.get("text"))
        .or_else(|| value.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let action = match action.as_str() {
        "" => {
            if message.is_empty() {
                ParsedHeartbeatAction::Idle
            } else {
                ParsedHeartbeatAction::Internal
            }
        }
        "empty" => ParsedHeartbeatAction::Empty,
        "idle" | "ok" | "noop" | "none" | "no_op" | "no-op" => ParsedHeartbeatAction::Idle,
        "internal" | "note" | "logged" | "recorded" | "observe" | "observed" => {
            ParsedHeartbeatAction::Internal
        }
        "send" | "escalate" | "notify" | "message" => ParsedHeartbeatAction::Send,
        _ => ParsedHeartbeatAction::Internal,
    };
    Some(ParsedHeartbeatResponse { action, message })
}

fn parse_json_object(response: &str) -> Option<Value> {
    let trimmed = response.trim();
    if let Ok(value @ Value::Object(_)) = serde_json::from_str::<Value>(trimmed) {
        return Some(value);
    }
    let without_fence = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|value| value.strip_suffix("```"))
        .map(str::trim);
    if let Some(without_fence) = without_fence
        && let Ok(value @ Value::Object(_)) = serde_json::from_str::<Value>(without_fence)
    {
        return Some(value);
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<Value>(&trimmed[start..=end])
        .ok()
        .filter(Value::is_object)
}

pub fn strip_model_tags(content: &str) -> String {
    let mut cleaned = content.to_string();
    for pattern in [
        r"(?s)<think>.*?</think>",
        r"(?s)<thinking>.*?</thinking>",
        r"<result>\s*",
        r"\s*</result>",
        r"(?s)<\|tool_calls_section_begin\|>.*",
        r"(?s)<\|tool_call_begin\|>.*",
        r"(?s)<tool_call:.*?>",
        r"(?s)<\|?tool_call\|?>.*",
        r"(?s)<\|?tool_response\|?>.*",
    ] {
        cleaned = Regex::new(pattern)
            .expect("valid model-tag regex")
            .replace_all(&cleaned, "")
            .to_string();
    }
    cleaned.trim().to_string()
}

fn format_reminder_block(reminders: &str) -> String {
    let reminders = reminders.trim();
    if reminders.is_empty() {
        String::new()
    } else {
        format!("Active reminders:\n{reminders}\n\n")
    }
}

fn format_open_work_block(open_work: &str) -> String {
    let open_work = open_work.trim();
    if open_work.is_empty() {
        String::new()
    } else {
        format!(
            "Open work — unfinished subagents and in-progress/overdue todos. \
             Anything BLOCKED or stalled here needs a decision from you \
             (unblock it, message the subagent, escalate, or close it out):\n{open_work}\n\n"
        )
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use tempfile::tempdir;

    use super::*;

    fn prompts() -> (tempfile::TempDir, PromptStore) {
        let tmp = tempdir().unwrap();
        let store = PromptStore::new(tmp.path().join("workspace"), tmp.path().join("config"));
        (tmp, store)
    }

    #[test]
    fn first_trigger_uses_full_context_and_renders_reminders() {
        let (_tmp, prompts) = prompts();
        let mut heartbeat = Heartbeat::new(HeartbeatConfig::default());
        let prompt = heartbeat.trigger_at(
            &prompts,
            "- [high] Submit report",
            "",
            Utc.with_ymd_and_hms(2026, 5, 22, 12, 0, 0).unwrap(),
            "2026-05-22 14:00 CEST",
        );

        assert!(prompt.use_full_context);
        assert!(prompt.first_tick);
        assert!(prompt.message.contains("[System: heartbeat - 2026-05-22"));
        assert!(prompt.message.contains("Active reminders:"));
        assert!(prompt.message.contains("Submit report"));
        assert!(prompt.message.contains("deeper check-in"));
        // No open work — the block (and its leftover placeholder) is absent.
        assert!(!prompt.message.contains("Open work"));
        assert!(!prompt.message.contains("{open_work}"));
    }

    #[test]
    fn trigger_renders_open_work_block_with_unfinished_items() {
        let (_tmp, prompts) = prompts();
        let mut heartbeat = Heartbeat::new(HeartbeatConfig::default());
        let open_work = "- subagent 'researcher' (id=abc, state=waiting, task=blocked, turns 5/20, age 3h2m) — BLOCKED, needs attention: waiting for API key\n- todo #7 [in_progress] (high) port the parser";
        let prompt = heartbeat.trigger_at(
            &prompts,
            "",
            open_work,
            Utc.with_ymd_and_hms(2026, 5, 22, 12, 0, 0).unwrap(),
            "2026-05-22 14:00 CEST",
        );

        // First tick renders the full-context template.
        assert!(prompt.use_full_context);
        assert!(prompt.message.contains("Open work"));
        assert!(prompt.message.contains("BLOCKED, needs attention"));
        assert!(prompt.message.contains("port the parser"));
        assert!(prompt.message.contains("resumes itself"));
        heartbeat.finish_response("ok", None);

        // Second tick (within the full-context interval) renders the minimal
        // template — open work must survive there too.
        let prompt = heartbeat.trigger_at(
            &prompts,
            "",
            open_work,
            Utc.with_ymd_and_hms(2026, 5, 22, 13, 0, 0).unwrap(),
            "2026-05-22 15:00 CEST",
        );
        assert!(!prompt.use_full_context);
        assert!(prompt.message.contains("Open work"));
        assert!(prompt.message.contains("BLOCKED, needs attention"));
        assert!(prompt.message.contains("resume itself"));
        assert!(!prompt.message.contains("{open_work}"));
    }

    #[test]
    fn full_context_interval_controls_prompt_choice() {
        let (_tmp, prompts) = prompts();
        let mut heartbeat = Heartbeat::new(HeartbeatConfig {
            interval_seconds: 900,
            full_context_interval_seconds: 7200,
            enabled: true,
        });
        let start = Utc.with_ymd_and_hms(2026, 5, 22, 12, 0, 0).unwrap();

        assert!(
            heartbeat
                .trigger_at(&prompts, "", "", start, "first")
                .use_full_context
        );
        heartbeat.finish_response("ok", None);
        assert!(
            !heartbeat
                .trigger_at(
                    &prompts,
                    "",
                    "",
                    start + chrono::Duration::hours(1),
                    "second"
                )
                .use_full_context
        );
        heartbeat.finish_response("ok", None);
        assert!(
            heartbeat
                .trigger_at(
                    &prompts,
                    "",
                    "",
                    start + chrono::Duration::hours(2),
                    "third"
                )
                .use_full_context
        );
    }

    #[test]
    fn finish_response_tracks_idle_minutes_and_reset() {
        let mut heartbeat = Heartbeat::new(HeartbeatConfig {
            interval_seconds: 15 * 60,
            ..Default::default()
        });

        let first = heartbeat.finish_response("ok", None);
        assert_eq!(first.action, HeartbeatAction::Idle);
        assert_eq!(first.idle_minutes, None);

        let second = heartbeat.finish_response("ok", None);
        assert_eq!(second.idle_minutes, Some(15));
        heartbeat.reset_idle_timer();

        let third = heartbeat.finish_response("ok", None);
        assert_eq!(third.idle_minutes, Some(15));
    }

    #[test]
    fn finish_response_strips_wrappers_and_honors_evaluation() {
        let mut heartbeat = Heartbeat::new(HeartbeatConfig::default());
        let send = heartbeat.finish_response(
            r#"<think>hidden</think><result>{"action":"escalate","message":"Ping"}</result>"#,
            None,
        );
        assert_eq!(send.action, HeartbeatAction::Send);
        assert_eq!(send.message, "Ping");

        let idle = heartbeat.finish_response(
            "Something generic",
            Some(r#"{"action":"idle","message":""}"#),
        );
        assert_eq!(idle.action, HeartbeatAction::Idle);
    }

    #[test]
    fn finish_response_suppresses_untyped_internal_prose() {
        let mut heartbeat = Heartbeat::new(HeartbeatConfig::default());

        let outcome = heartbeat.finish_response(
            "Good. Questions note created.\n\nNo need to message him again.\n\nok",
            None,
        );

        assert_eq!(outcome.action, HeartbeatAction::Internal);
        assert!(outcome.message.contains("Questions note created"));
    }

    #[test]
    fn summary_prompt_uses_embedded_template() {
        let (_tmp, prompts) = prompts();
        let prompt = render_summary_prompt(&prompts, "Reminder due");

        assert!(prompt.contains("heartbeat result"));
        assert!(prompt.contains("Reminder due"));
    }
}
