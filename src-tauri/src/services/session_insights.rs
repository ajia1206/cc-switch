//! Runtime metrics extracted from native Codex JSONL session logs.

use crate::codex_config::get_codex_config_dir;
use crate::services::session_usage_codex::collect_codex_session_files;
use chrono::DateTime;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

static SKILL_PATH_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:^|[/\\])skills[/\\]([^/\\]+)[/\\]SKILL\.md").expect("valid skill path regex")
});

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PercentileStats {
    pub p50: Option<f64>,
    pub p95: Option<f64>,
    pub p99: Option<f64>,
    pub max: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NamedCount {
    pub name: String,
    pub count: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSessionInsights {
    pub completed_turns: u64,
    pub session_count: u64,
    pub model_requests: u64,
    pub calls_per_turn: f64,
    pub rpm: f64,
    pub tpm: f64,
    pub cache_hit_rate: f64,
    pub weighted_effective_tps: Option<f64>,
    pub ttft_ms: PercentileStats,
    pub total_latency_ms: PercentileStats,
    pub mcp_calls: Vec<NamedCount>,
    pub skill_calls: Vec<NamedCount>,
}

#[derive(Debug, Clone, Default)]
struct TokenSnapshot {
    input: u64,
    cached_input: u64,
    output: u64,
}

#[derive(Debug, Clone, Default)]
struct ActiveTurn {
    turn_id: String,
    session_id: String,
    started_at_ms: i64,
    base_tokens: TokenSnapshot,
    last_tokens: TokenSnapshot,
    model_requests: u64,
    mcp_calls: HashMap<String, u64>,
    skill_calls: HashMap<String, u64>,
    seen_call_ids: HashSet<String>,
}

#[derive(Debug, Clone)]
struct CompletedTurn {
    session_id: String,
    started_at_ms: i64,
    completed_at_ms: i64,
    duration_ms: f64,
    ttft_ms: f64,
    model_requests: u64,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    mcp_calls: HashMap<String, u64>,
    skill_calls: HashMap<String, u64>,
}

pub fn get_codex_session_insights(
    start_date: Option<i64>,
    end_date: Option<i64>,
) -> CodexSessionInsights {
    let mut files = collect_codex_session_files(&get_codex_config_dir());
    // A turn can begin before the selected window and complete inside it, so
    // keep one day of mtime slack while avoiding a full-history scan on every
    // tray refresh.
    if let Some(start) = start_date {
        let cutoff = start.saturating_sub(24 * 60 * 60).max(0) as u64;
        files.retain(|path| {
            fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .is_some_and(|modified| modified.as_secs() >= cutoff)
        });
    }
    aggregate_codex_session_files(&files, start_date, end_date)
}

fn aggregate_codex_session_files(
    files: &[PathBuf],
    start_date: Option<i64>,
    end_date: Option<i64>,
) -> CodexSessionInsights {
    let start_ms = start_date.map(|value| value.saturating_mul(1_000));
    let end_ms = end_date.map(|value| value.saturating_mul(1_000));
    let mut turns = HashMap::<String, CompletedTurn>::new();

    for file in files {
        collect_file_turns(file, start_ms, end_ms, &mut turns);
    }

    summarize_turns(turns.into_values().collect(), start_ms, end_ms)
}

fn collect_file_turns(
    path: &Path,
    start_ms: Option<i64>,
    end_ms: Option<i64>,
    turns: &mut HashMap<String, CompletedTurn>,
) {
    let Ok(file) = fs::File::open(path) else {
        return;
    };
    let mut session_id = path.to_string_lossy().to_string();
    let mut latest_tokens = TokenSnapshot::default();
    let mut active: Option<ActiveTurn> = None;

    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let event_type = value.get("type").and_then(|value| value.as_str());
        let payload = value.get("payload").unwrap_or(&serde_json::Value::Null);

        if event_type == Some("session_meta") {
            if let Some(id) = payload
                .get("id")
                .or_else(|| payload.get("thread_id"))
                .and_then(|value| value.as_str())
            {
                session_id = id.to_string();
            }
            continue;
        }

        if event_type == Some("event_msg") {
            match payload.get("type").and_then(|value| value.as_str()) {
                Some("task_started") => {
                    let Some(turn_id) = payload.get("turn_id").and_then(|value| value.as_str())
                    else {
                        continue;
                    };
                    let Some(timestamp_ms) = parse_timestamp_ms(&value) else {
                        continue;
                    };
                    active = Some(ActiveTurn {
                        turn_id: turn_id.to_string(),
                        session_id: session_id.clone(),
                        started_at_ms: timestamp_ms,
                        base_tokens: latest_tokens.clone(),
                        last_tokens: latest_tokens.clone(),
                        ..ActiveTurn::default()
                    });
                }
                Some("token_count") => {
                    if let Some(snapshot) = parse_token_snapshot(payload) {
                        latest_tokens = snapshot.clone();
                        if let Some(turn) = active.as_mut() {
                            turn.last_tokens = snapshot;
                            turn.model_requests += 1;
                        }
                    }
                }
                Some("task_complete") => {
                    let Some(turn) = active.take() else {
                        continue;
                    };
                    let Some(completed_at_ms) = parse_timestamp_ms(&value) else {
                        continue;
                    };
                    if start_ms.is_some_and(|start| completed_at_ms < start)
                        || end_ms.is_some_and(|end| completed_at_ms > end)
                    {
                        continue;
                    }
                    let Some(duration_ms) =
                        payload.get("duration_ms").and_then(|value| value.as_f64())
                    else {
                        continue;
                    };
                    let Some(ttft_ms) = payload
                        .get("time_to_first_token_ms")
                        .and_then(|value| value.as_f64())
                    else {
                        continue;
                    };
                    let completed = CompletedTurn {
                        session_id: turn.session_id,
                        started_at_ms: turn.started_at_ms,
                        completed_at_ms,
                        duration_ms,
                        ttft_ms,
                        model_requests: turn.model_requests,
                        input_tokens: turn
                            .last_tokens
                            .input
                            .saturating_sub(turn.base_tokens.input),
                        cached_input_tokens: turn
                            .last_tokens
                            .cached_input
                            .saturating_sub(turn.base_tokens.cached_input),
                        output_tokens: turn
                            .last_tokens
                            .output
                            .saturating_sub(turn.base_tokens.output),
                        mcp_calls: turn.mcp_calls,
                        skill_calls: turn.skill_calls,
                    };
                    // Forked/copied histories can repeat a turn. Prefer the copy with
                    // the most complete request/tool trace, never count both.
                    let replace = turns.get(&turn.turn_id).is_none_or(|existing| {
                        completed.model_requests > existing.model_requests
                            || completed.mcp_calls.values().sum::<u64>()
                                > existing.mcp_calls.values().sum::<u64>()
                    });
                    if replace {
                        turns.insert(turn.turn_id, completed);
                    }
                }
                _ => {}
            }
            continue;
        }

        if event_type == Some("response_item") {
            let item = payload.get("payload").unwrap_or(payload);
            let call_type = item.get("type").and_then(|value| value.as_str());
            if !matches!(call_type, Some("custom_tool_call" | "function_call")) {
                continue;
            }
            let Some(turn) = active.as_mut() else {
                continue;
            };
            if let Some(call_id) = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(|value| value.as_str())
            {
                if !turn.seen_call_ids.insert(call_id.to_string()) {
                    continue;
                }
            }
            if let Some(name) = item.get("name").and_then(|value| value.as_str()) {
                if let Some(server) = mcp_server_name(name) {
                    *turn.mcp_calls.entry(server.to_string()).or_default() += 1;
                }
            }
            if let Some(input) = item
                .get("input")
                .or_else(|| item.get("arguments"))
                .and_then(|value| value.as_str())
            {
                let mut loaded = HashSet::new();
                for captures in SKILL_PATH_RE.captures_iter(input) {
                    if let Some(name) = captures.get(1) {
                        loaded.insert(name.as_str().to_string());
                    }
                }
                for name in loaded {
                    *turn.skill_calls.entry(name).or_default() += 1;
                }
            }
        }
    }
}

fn parse_timestamp_ms(value: &serde_json::Value) -> Option<i64> {
    DateTime::parse_from_rfc3339(value.get("timestamp")?.as_str()?)
        .ok()
        .map(|timestamp| timestamp.timestamp_millis())
}

fn parse_token_snapshot(payload: &serde_json::Value) -> Option<TokenSnapshot> {
    let total = payload.get("info")?.get("total_token_usage")?;
    Some(TokenSnapshot {
        input: total.get("input_tokens")?.as_u64()?,
        cached_input: total
            .get("cached_input_tokens")
            .or_else(|| total.get("cache_read_input_tokens"))
            .and_then(|value| value.as_u64())
            .unwrap_or(0),
        output: total.get("output_tokens")?.as_u64()?,
    })
}

fn mcp_server_name(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("mcp__")?;
    rest.split("__").next().filter(|name| !name.is_empty())
}

fn summarize_turns(
    turns: Vec<CompletedTurn>,
    start_ms: Option<i64>,
    end_ms: Option<i64>,
) -> CodexSessionInsights {
    if turns.is_empty() {
        return CodexSessionInsights::default();
    }

    let completed_turns = turns.len() as u64;
    let sessions = turns
        .iter()
        .map(|turn| turn.session_id.as_str())
        .collect::<HashSet<_>>()
        .len() as u64;
    let model_requests = turns.iter().map(|turn| turn.model_requests).sum::<u64>();
    let input_tokens = turns.iter().map(|turn| turn.input_tokens).sum::<u64>();
    let cached_input_tokens = turns
        .iter()
        .map(|turn| turn.cached_input_tokens)
        .sum::<u64>();
    let output_tokens = turns.iter().map(|turn| turn.output_tokens).sum::<u64>();
    let active_ms = turns.iter().map(|turn| turn.duration_ms).sum::<f64>();
    let observed_start = start_ms.unwrap_or_else(|| {
        turns
            .iter()
            .map(|turn| turn.started_at_ms)
            .min()
            .unwrap_or(0)
    });
    let observed_end = end_ms.unwrap_or_else(|| {
        turns
            .iter()
            .map(|turn| turn.completed_at_ms)
            .max()
            .unwrap_or(observed_start)
    });
    let observed_minutes = ((observed_end - observed_start) as f64 / 60_000.0).max(1.0);

    let mut mcp_calls = HashMap::new();
    let mut skill_calls = HashMap::new();
    for turn in &turns {
        merge_counts(&mut mcp_calls, &turn.mcp_calls);
        merge_counts(&mut skill_calls, &turn.skill_calls);
    }

    CodexSessionInsights {
        completed_turns,
        session_count: sessions,
        model_requests,
        calls_per_turn: model_requests as f64 / completed_turns as f64,
        rpm: model_requests as f64 / observed_minutes,
        tpm: (input_tokens + output_tokens) as f64 / observed_minutes,
        cache_hit_rate: if input_tokens > 0 {
            cached_input_tokens as f64 / input_tokens as f64
        } else {
            0.0
        },
        weighted_effective_tps: (active_ms > 0.0)
            .then_some(output_tokens as f64 / (active_ms / 1_000.0)),
        ttft_ms: percentile_stats(turns.iter().map(|turn| turn.ttft_ms).collect()),
        total_latency_ms: percentile_stats(turns.iter().map(|turn| turn.duration_ms).collect()),
        mcp_calls: sorted_counts(mcp_calls),
        skill_calls: sorted_counts(skill_calls),
    }
}

fn merge_counts(target: &mut HashMap<String, u64>, source: &HashMap<String, u64>) {
    for (name, count) in source {
        *target.entry(name.clone()).or_default() += count;
    }
}

fn sorted_counts(counts: HashMap<String, u64>) -> Vec<NamedCount> {
    let mut items = counts
        .into_iter()
        .map(|(name, count)| NamedCount { name, count })
        .collect::<Vec<_>>();
    items.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.name.cmp(&right.name))
    });
    items
}

fn percentile_stats(mut values: Vec<f64>) -> PercentileStats {
    if values.is_empty() {
        return PercentileStats::default();
    }
    values.sort_by(f64::total_cmp);
    PercentileStats {
        p50: percentile(&values, 0.50),
        p95: percentile(&values, 0.95),
        p99: percentile(&values, 0.99),
        max: values.last().copied(),
    }
}

fn percentile(values: &[f64], ratio: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let position = (values.len() - 1) as f64 * ratio;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    if lower == upper {
        Some(values[lower])
    } else {
        Some(values[lower] + (values[upper] - values[lower]) * (position - lower as f64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_jsonl(path: &Path, values: &[serde_json::Value]) {
        let mut file = fs::File::create(path).unwrap();
        for value in values {
            writeln!(file, "{value}").unwrap();
        }
    }

    fn fixture(session: &str, turn: &str, request_count: usize) -> Vec<serde_json::Value> {
        let mut values = vec![
            serde_json::json!({"timestamp":"2026-07-15T01:00:00Z","type":"session_meta","payload":{"id":session}}),
            serde_json::json!({"timestamp":"2026-07-15T01:00:01Z","type":"event_msg","payload":{"type":"task_started","turn_id":turn}}),
        ];
        for index in 0..request_count {
            values.push(serde_json::json!({"timestamp":"2026-07-15T01:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000 + index * 100,"cached_input_tokens":800 + index * 80,"output_tokens":100 + index * 20}}}}));
        }
        values.extend([
            serde_json::json!({"timestamp":"2026-07-15T01:00:03Z","type":"response_item","payload":{"type":"custom_tool_call","name":"mcp__playwright__browser_open","input":"{}"}}),
            serde_json::json!({"timestamp":"2026-07-15T01:00:04Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"sed -n '1,20p' /tmp/skills/frontend-patterns/SKILL.md"}}),
            serde_json::json!({"timestamp":"2026-07-15T01:01:01Z","type":"event_msg","payload":{"type":"task_complete","turn_id":turn,"duration_ms":60000,"time_to_first_token_ms":5000}}),
        ]);
        values
    }

    #[test]
    fn aggregates_and_deduplicates_turn_metrics() {
        let temp = tempdir().unwrap();
        let first = temp.path().join("first.jsonl");
        let duplicate = temp.path().join("duplicate.jsonl");
        let second = temp.path().join("second.jsonl");
        write_jsonl(&first, &fixture("session-a", "turn-a", 2));
        write_jsonl(&duplicate, &fixture("session-copy", "turn-a", 2));
        write_jsonl(&second, &fixture("session-b", "turn-b", 3));

        let result = aggregate_codex_session_files(&[first, duplicate, second], None, None);

        assert_eq!(result.completed_turns, 2);
        assert_eq!(result.session_count, 2);
        assert_eq!(result.model_requests, 5);
        assert_eq!(result.mcp_calls[0].name, "playwright");
        assert_eq!(result.mcp_calls[0].count, 2);
        assert_eq!(result.skill_calls[0].name, "frontend-patterns");
        assert_eq!(result.skill_calls[0].count, 2);
        assert_eq!(result.ttft_ms.p50, Some(5_000.0));
        assert_eq!(result.total_latency_ms.p95, Some(60_000.0));
    }
}
