//! Minimal, privacy-preserving usage handoff for the extracted Local AI Stack.
//! This file intentionally contains no prompt, response, command arguments, or credentials.

use crate::proxy::usage::logger::RequestLog;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;

#[derive(Debug, Serialize)]
struct SpoolEvent<'a> {
    schema_version: u8,
    event_id: String,
    occurred_at: i64,
    source_app: &'static str,
    source_version: &'static str,
    request_id: &'a str,
    session_id: Option<&'a str>,
    provider_id: &'a str,
    model: &'a str,
    input_tokens: u32,
    output_tokens: u32,
    cache_read_tokens: u32,
    cache_write_tokens: u32,
    latency_ms: u64,
    status_code: u16,
    reported_cost_usd: Option<String>,
    data_source: &'static str,
}

/// Best-effort append. A telemetry handoff must never make the user's request fail.
pub fn append_proxy_event(log: &RequestLog, occurred_at: i64) {
    let Some(home) = dirs::home_dir() else { return };
    let dir = home.join(".local-ai-stack/spool/cc-switch");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(format!(
        "events-{}.jsonl",
        chrono::Utc::now().format("%Y-%m-%d")
    ));
    let event = SpoolEvent {
        schema_version: 1,
        event_id: format!("ccswitch:{}", log.request_id),
        occurred_at,
        source_app: "cc-switch",
        source_version: env!("CARGO_PKG_VERSION"),
        request_id: &log.request_id,
        session_id: log.session_id.as_deref(),
        provider_id: &log.provider_id,
        model: &log.model,
        input_tokens: log.usage.input_tokens,
        output_tokens: log.usage.output_tokens,
        cache_read_tokens: log.usage.cache_read_tokens,
        cache_write_tokens: log.usage.cache_creation_tokens,
        latency_ms: log.latency_ms,
        status_code: log.status_code,
        reported_cost_usd: log.cost.as_ref().map(|cost| cost.total_cost.to_string()),
        data_source: "ccswitch_proxy",
    };
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    if let Ok(json) = serde_json::to_string(&event) {
        let _ = writeln!(file, "{json}");
    }
}

#[cfg(test)]
mod tests {
    use super::SpoolEvent;

    #[test]
    fn contract_contains_metadata_only() {
        let event = SpoolEvent {
            schema_version: 1,
            event_id: "ccswitch:test".into(),
            occurred_at: 1,
            source_app: "cc-switch",
            source_version: "test",
            request_id: "req",
            session_id: Some("session"),
            provider_id: "provider",
            model: "model",
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: 3,
            cache_write_tokens: 4,
            latency_ms: 5,
            status_code: 200,
            reported_cost_usd: None,
            data_source: "ccswitch_proxy",
        };
        let json = serde_json::to_string(&event).expect("event serializes");
        assert!(!json.contains("prompt"));
        assert!(!json.contains("response"));
        assert!(!json.contains("api_key"));
        assert!(json.contains("input_tokens"));
    }
}
