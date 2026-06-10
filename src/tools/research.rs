use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use serde_json::Value;
use uuid::Uuid;

use crate::actor::{ActorState, Outcome, SpawnReport, SpawnSubagent};
use crate::tools::registry::ActorToolContext;
use crate::tools::registry::ToolRegistry;
use crate::tools::registry::args::{string_arg, string_arg_default, usize_arg};
use crate::tools::spec::{ToolCategory, ToolDef, ToolExecutor, p_enum, p_int, p_str, p_str_req};

const FRAMER_PROMPT: &str = include_str!("../../config/prompts/actor_research_framer.md");
const HYPOTHESIS_PROMPT: &str = include_str!("../../config/prompts/actor_research_hypothesis.md");
const JUDGE_PROMPT: &str = include_str!("../../config/prompts/actor_research_judge.md");

const MIN_HYPOTHESES: usize = 2;
const MAX_HYPOTHESES: usize = 5;
const DEFAULT_HYPOTHESES: usize = 3;
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const FRAMER_MAX_TURNS: usize = 4;
const FRAMER_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const JUDGE_MAX_TURNS: usize = 6;
const JUDGE_TIMEOUT: Duration = Duration::from_secs(3 * 60);
const DEPTH_VALUES: &[&str] = &["shallow", "deep"];
const MISSING_ACTOR_CONTEXT: &str = "Error: research tool only works inside an actor.";

fn exec_research<'a>(
    registry: &'a ToolRegistry<'a>,
    args: &'a Value,
) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
    Box::pin(execute_research(registry, args))
}

async fn execute_research(registry: &ToolRegistry<'_>, args: &Value) -> String {
    let Some(context) = registry.runtime.actor.as_ref() else {
        return MISSING_ACTOR_CONTEXT.to_string();
    };

    let question = string_arg(args, "question").trim().to_string();
    if question.is_empty() {
        return "Error: question is required.".to_string();
    }

    let depth = string_arg_default(args, "depth", "shallow");
    let max_turns_per_hyp = if depth == "deep" { 12 } else { 8 };
    let hyp_timeout = if depth == "deep" {
        Duration::from_secs(8 * 60)
    } else {
        Duration::from_secs(3 * 60)
    };

    let session_id = short_id();

    let provided = string_arg_default(args, "hypotheses", "");
    let mut hypotheses: Vec<String> = if provided.trim().is_empty() {
        let n = usize_arg(args, "n", DEFAULT_HYPOTHESES);
        match generate_hypotheses(context, &question, n, &session_id).await {
            Ok(v) => v,
            Err(e) => return format!("Research: framing failed. {e}"),
        }
    } else {
        match serde_json::from_str::<Vec<String>>(&provided) {
            Ok(v) => v
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            Err(error) => {
                return format!(
                    "Error: hypotheses must be a JSON array of strings. Parse error: {error}"
                );
            }
        }
    };

    if hypotheses.len() < MIN_HYPOTHESES {
        return format!(
            "Research: need at least {MIN_HYPOTHESES} hypotheses, got {}.",
            hypotheses.len()
        );
    }
    hypotheses.truncate(MAX_HYPOTHESES);

    let mut child_ids: Vec<String> = Vec::with_capacity(hypotheses.len());
    for (index, hypothesis) in hypotheses.iter().enumerate() {
        let goals = HYPOTHESIS_PROMPT
            .replace("{hypothesis}", hypothesis)
            .replace("{question}", &question);
        let name = format!("research-hyp-{}-{session_id}", index + 1);
        let spawn = context
            .runtime
            .spawn_subagent(SpawnSubagent {
                actor_id: context.actor_id.clone(),
                name: name.clone(),
                goals,
                group: None,
                tools: "web_search,fetch_webpage".to_string(),
                model: "aux".to_string(),
                max_turns: max_turns_per_hyp,
            })
            .await;
        match spawn {
            Ok(SpawnReport::Spawned { actor_id, .. }) => child_ids.push(actor_id),
            Ok(SpawnReport::Rejected { message }) | Err(message) => {
                return format!(
                    "Research: spawning hypothesis {} failed: {message}",
                    index + 1
                );
            }
        }
    }

    let deadline = Instant::now() + hyp_timeout;
    let mut findings: Vec<Option<String>> = vec![None; child_ids.len()];
    loop {
        if findings.iter().all(Option::is_some) {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        for (index, id) in child_ids.iter().enumerate() {
            if findings[index].is_some() {
                continue;
            }
            match context.runtime.actor_info(id).await {
                Some(info) if info.state == ActorState::Terminated => {
                    findings[index] =
                        Some(info.result.unwrap_or_else(|| "[no result]".to_string()));
                }
                Some(_) => {}
                None => {
                    findings[index] =
                        Some(format!("[hypothesis {} actor {id} disappeared]", index + 1));
                }
            }
        }
        if findings.iter().any(Option::is_none) {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    let findings_str: Vec<String> = findings
        .into_iter()
        .enumerate()
        .map(|(index, opt)| {
            opt.unwrap_or_else(|| {
                format!(
                    "[hypothesis {} timed out after {} seconds]",
                    index + 1,
                    hyp_timeout.as_secs()
                )
            })
        })
        .collect();

    let findings_json = serde_json::to_string(&findings_str).unwrap_or_else(|_| "[]".to_string());
    let judge_goals = JUDGE_PROMPT
        .replace("{question}", &question)
        .replace("{findings}", &findings_json);
    let judge_name = format!("research-judge-{session_id}");
    let spawn = context
        .runtime
        .spawn_subagent(SpawnSubagent {
            actor_id: context.actor_id.clone(),
            name: judge_name,
            goals: judge_goals,
            group: None,
            tools: String::new(),
            model: "main".to_string(),
            max_turns: JUDGE_MAX_TURNS,
        })
        .await;
    let judge_id = match spawn {
        Ok(SpawnReport::Spawned { actor_id, .. }) => actor_id,
        Ok(SpawnReport::Rejected { message }) | Err(message) => {
            return format!(
                "Research: judge spawn failed: {message}\n\nRaw findings:\n{findings_json}"
            );
        }
    };

    match poll_until_terminated(context, &judge_id, JUDGE_TIMEOUT).await {
        Ok((verdict, _)) => verdict,
        Err(error) => format!("Research: judge failed: {error}\n\nRaw findings:\n{findings_json}"),
    }
}

async fn generate_hypotheses(
    context: &ActorToolContext,
    question: &str,
    n: usize,
    session_id: &str,
) -> Result<Vec<String>, String> {
    let n = n.clamp(MIN_HYPOTHESES, MAX_HYPOTHESES);
    let goals = format!(
        "{FRAMER_PROMPT}\n\n---\n\nQuestion: {question}\n\nProduce exactly {n} hypotheses."
    );
    let name = format!("research-framer-{session_id}");
    let spawn = context
        .runtime
        .spawn_subagent(SpawnSubagent {
            actor_id: context.actor_id.clone(),
            name,
            goals,
            group: None,
            tools: String::new(),
            model: "aux".to_string(),
            max_turns: FRAMER_MAX_TURNS,
        })
        .await;
    let framer_id = match spawn {
        Ok(SpawnReport::Spawned { actor_id, .. }) => actor_id,
        Ok(SpawnReport::Rejected { message }) | Err(message) => {
            return Err(format!("Framer spawn failed: {message}"));
        }
    };
    let (result, _outcome) = poll_until_terminated(context, &framer_id, FRAMER_TIMEOUT).await?;
    parse_framer_hypotheses(&result)
}

fn parse_framer_hypotheses(text: &str) -> Result<Vec<String>, String> {
    let trimmed = text.trim();
    let json_start = trimmed.find('{');
    let json_end = trimmed.rfind('}');
    let json_str = match (json_start, json_end) {
        (Some(start), Some(end)) if end >= start => &trimmed[start..=end],
        _ => return Err(format!("Framer returned no JSON object. Raw: {trimmed}")),
    };
    let value: Value = serde_json::from_str(json_str)
        .map_err(|error| format!("Framer JSON parse error: {error}. Raw: {json_str}"))?;
    let array = value
        .get("hypotheses")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("Framer JSON missing 'hypotheses' array. Got: {value}"))?;
    let result: Vec<String> = array
        .iter()
        .filter_map(|value| value.as_str().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect();
    if result.is_empty() {
        return Err("Framer returned empty hypotheses array.".to_string());
    }
    Ok(result)
}

async fn poll_until_terminated(
    context: &ActorToolContext,
    actor_id: &str,
    timeout: Duration,
) -> Result<(String, Outcome), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return Err(format!("timed out after {} seconds", timeout.as_secs()));
        }
        match context.runtime.actor_info(actor_id).await {
            Some(info) if info.state == ActorState::Terminated => {
                let outcome = info.outcome.unwrap_or(Outcome::Success);
                return Ok((
                    info.result.unwrap_or_else(|| "no result".to_string()),
                    outcome,
                ));
            }
            Some(_) => tokio::time::sleep(POLL_INTERVAL).await,
            None => return Err(format!("actor {actor_id} disappeared")),
        }
    }
}

fn short_id() -> String {
    Uuid::new_v4().to_string()[..6].to_string()
}

pub const TOOL_DEFS: &[ToolDef] = &[ToolDef {
    name: "research",
    description: "Investigate a question by spawning N parallel hypothesis subagents (web-search-enabled) and a judge that selects or synthesizes. Returns the judge's JSON verdict.",
    params: &[
        p_str_req("question", "The research question to investigate."),
        p_str(
            "hypotheses",
            "JSON array of 2-5 hypothesis strings. If omitted, a framer subagent generates them.",
        ),
        p_int(
            "n",
            "How many hypotheses to generate when 'hypotheses' is omitted (default 3, clamped 2-5).",
        ),
        p_enum(
            "depth",
            "shallow (max 8 turns per hypothesis) or deep (max 12).",
            DEPTH_VALUES,
        ),
    ],
    category: ToolCategory::CortexOnly,
    execute: ToolExecutor::Async(exec_research),
}];
