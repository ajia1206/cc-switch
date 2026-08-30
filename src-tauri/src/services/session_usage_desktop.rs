//! 第三方桌面 Agent 会话用量追踪。
//!
//! 当前支持：
//! - Maka: `runtime.sqlite/usage_model_call_attempts`，每行是一轮真实 provider 尝试。
//! - CodePilot: `~/.codepilot/codepilot.db/messages.token_usage`，每行是一条已完成回复。
//! - DeepSeek Harness: `~/.dsh/sessions/**/session.jsonl.zstd`，每条最终
//!   `assistant/message` 事件是一轮模型调用。
//!
//! 所有来源都只读打开，并使用来源内的稳定主键生成 request_id，避免定时同步重复入账。

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use crate::proxy::usage::calculator::CostCalculator;
use crate::proxy::usage::parser::TokenUsage;
use crate::services::session_usage::{
    get_sync_state, metadata_modified_nanos, update_sync_state, SessionSyncResult,
};
use crate::services::sql_helpers::INPUT_TOKEN_SEMANTICS_FRESH;
use crate::services::usage_stats::{
    find_model_pricing, should_skip_session_insert, DedupKey, CINDY_MIRROR_DATA_SOURCE,
    CINDY_MIRROR_REQUEST_MODEL,
};
use chrono::{Local, NaiveDate, TimeZone};
use rust_decimal::Decimal;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

const MAKA_APP_TYPE: &str = "maka";
const MAKA_DATA_SOURCE: &str = "maka_session";
const CODEPILOT_APP_TYPE: &str = "codepilot";
const CODEPILOT_DATA_SOURCE: &str = "codepilot_session";
const DEEPSEEK_HARNESS_APP_TYPE: &str = "deepseek_harness";
const DEEPSEEK_HARNESS_DATA_SOURCE: &str = "deepseek_harness_session";
const CINDY_APP_TYPE: &str = "cindy";
const CINDY_LEGACY_DATA_SOURCE: &str = "cindy_daily";
const CINDY_DETAIL_DATA_SOURCE: &str = "cindy_turn";
const CINDY_PROVIDER_ID: &str = "_cindy_session";
const CINDY_SYNC_VERSION: &str = "v5";
const CINDY_SOURCE_SET_SYNC_KEY: &str = "desktop:cindy:v5:source-set";
const CINDY_DETAIL_RETAIN_DAYS: i64 = 30;

#[derive(Debug)]
struct DesktopUsageRecord {
    request_id: String,
    app_type: &'static str,
    data_source: &'static str,
    provider_id: &'static str,
    model: String,
    session_id: Option<String>,
    input_tokens: u32,
    output_tokens: u32,
    cache_read_tokens: u32,
    cache_creation_tokens: u32,
    total_cost_usd: Option<f64>,
    latency_ms: i64,
    duration_ms: Option<i64>,
    first_token_ms: Option<i64>,
    status_code: i64,
    error_message: Option<String>,
    created_at: i64,
    upstream_dedup: Option<UpstreamDedup>,
}

#[derive(Debug)]
struct UpstreamDedup {
    app_type: &'static str,
    session_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum DesktopUsageInsertOutcome {
    Inserted,
    AccountedInSameApp,
    AccountedInUpstreamApp {
        total_cost_usd: f64,
        mirror_inserted: bool,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MakaAttempt {
    session_id: Option<String>,
    model_id: String,
    status: String,
    usage_basis: String,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_miss_input_tokens: Option<u64>,
    cache_write_input_tokens: Option<u64>,
    cost_basis: String,
    cost_usd: Option<f64>,
    latency_ms: Option<u64>,
    time_to_first_token_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct CodePilotTokenUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cost_usd: Option<f64>,
    usage_model_id: Option<String>,
    context_accounting: Option<CodePilotContextAccounting>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodePilotContextAccounting {
    provider_backend: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeepSeekHarnessUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
}

#[derive(Debug)]
struct CindyDailyUsage {
    day: String,
    agent_kind: String,
    model: String,
    cost_usd: f64,
    cost_amount: f64,
    cost_currency: String,
    cost_is_approximate: bool,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_create_tokens: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CindyTurnUsageDetails {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_create_tokens: Option<u64>,
    model: Option<String>,
    #[serde(default)]
    models: Vec<String>,
    duration_ms: Option<u64>,
    turn_duration_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CindyTurnCost {
    approximate: Option<bool>,
    kind: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CindyBillingKind {
    Api,
    Subscription,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CindyAgentMeta {
    turn_completed: Option<bool>,
    turn_usage_details: Option<CindyTurnUsageDetails>,
    turn_cost_usd: Option<f64>,
    turn_cost: Option<CindyTurnCost>,
}

#[derive(Debug)]
struct CindyTurnUsage {
    request_id: String,
    day: String,
    agent_kind: String,
    canonical_model: String,
    billing_hint: Option<CindyBillingKind>,
    session_id: String,
    sdk_session_id: Option<String>,
    created_at: i64,
    duration_ms: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_create_tokens: i64,
}

#[derive(Debug)]
struct CindyUsageSnapshot {
    account_id: String,
    daily: Vec<CindyDailyUsage>,
    turns: Vec<CindyTurnUsage>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CindyUsageKey {
    account_id: String,
    day: String,
    agent_kind: String,
    canonical_model: String,
}

#[derive(Debug, Default)]
struct CindyDailyAggregate {
    source_models: HashMap<String, CindyDailyModelAggregate>,
}

#[derive(Debug, Default)]
struct CindyDailyModelAggregate {
    tokens: CindyTokenBuckets,
    total_cost_usd: f64,
}

#[derive(Debug)]
struct CindyArchivedRollupMetrics {
    date: String,
    provider_id: String,
    model: String,
    request_model: String,
    pricing_model: String,
    request_count: i64,
    success_count: i64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CindyTokenBuckets {
    input: i64,
    output: i64,
    cache_read: i64,
    cache_create: i64,
}

impl CindyTokenBuckets {
    fn from_daily(usage: &CindyDailyUsage) -> Self {
        Self {
            input: cindy_token_i64(usage.input_tokens),
            output: cindy_token_i64(usage.output_tokens),
            cache_read: cindy_token_i64(usage.cache_read_tokens),
            cache_create: cindy_token_i64(usage.cache_create_tokens),
        }
    }

    fn from_turn(usage: &CindyTurnUsage) -> Self {
        Self {
            input: cindy_token_i64(usage.input_tokens),
            output: cindy_token_i64(usage.output_tokens),
            cache_read: cindy_token_i64(usage.cache_read_tokens),
            cache_create: cindy_token_i64(usage.cache_create_tokens),
        }
    }

    fn add_assign(&mut self, other: Self) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_create = self.cache_create.saturating_add(other.cache_create);
    }

    fn fits_within(self, daily: Self) -> bool {
        self.input <= daily.input
            && self.output <= daily.output
            && self.cache_read <= daily.cache_read
            && self.cache_create <= daily.cache_create
    }

    fn subtract(self, covered: Self) -> Self {
        Self {
            input: self.input - covered.input,
            output: self.output - covered.output,
            cache_read: self.cache_read - covered.cache_read,
            cache_create: self.cache_create - covered.cache_create,
        }
    }

    fn is_zero(self) -> bool {
        self.input == 0 && self.output == 0 && self.cache_read == 0 && self.cache_create == 0
    }
}

fn assign_cindy_turn_models(
    daily: &CindyDailyAggregate,
    turns: &[&CindyTurnUsage],
) -> Option<HashMap<String, String>> {
    if daily.source_models.is_empty() {
        return None;
    }

    if daily.source_models.len() == 1 {
        let (model, source) = daily.source_models.iter().next()?;
        let mut turn_tokens = CindyTokenBuckets::default();
        for turn in turns {
            turn_tokens.add_assign(CindyTokenBuckets::from_turn(turn));
        }
        if !turn_tokens.fits_within(source.tokens) {
            return None;
        }
        return Some(
            turns
                .iter()
                .map(|turn| (turn.request_id.clone(), model.clone()))
                .collect(),
        );
    }

    let mut assignments = HashMap::new();
    let mut assigned_tokens: HashMap<String, CindyTokenBuckets> = HashMap::new();
    let mut unresolved = Vec::new();
    for turn in turns {
        let Some(hint) = turn.billing_hint else {
            unresolved.push(*turn);
            continue;
        };
        let mut candidates = daily
            .source_models
            .keys()
            .filter(|model| cindy_billing_kind_from_model(model) == Some(hint));
        let model = candidates.next()?.clone();
        if candidates.next().is_some() {
            return None;
        }
        assignments.insert(turn.request_id.clone(), model.clone());
        assigned_tokens
            .entry(model)
            .or_default()
            .add_assign(CindyTokenBuckets::from_turn(turn));
    }

    for (model, tokens) in &assigned_tokens {
        if !tokens.fits_within(daily.source_models.get(model)?.tokens) {
            return None;
        }
    }

    if !unresolved.is_empty() {
        let mut unresolved_tokens = CindyTokenBuckets::default();
        for turn in &unresolved {
            unresolved_tokens.add_assign(CindyTokenBuckets::from_turn(turn));
        }
        let mut candidates = daily.source_models.iter().filter_map(|(model, source)| {
            let covered = assigned_tokens.get(model).copied().unwrap_or_default();
            if covered.fits_within(source.tokens)
                && source.tokens.subtract(covered) == unresolved_tokens
            {
                Some(model.clone())
            } else {
                None
            }
        });
        let model = candidates.next()?;
        if candidates.next().is_some() {
            return None;
        }
        for turn in unresolved {
            assignments.insert(turn.request_id.clone(), model.clone());
            assigned_tokens
                .entry(model.clone())
                .or_default()
                .add_assign(CindyTokenBuckets::from_turn(turn));
        }
    }

    (assignments.len() == turns.len()).then_some(assignments)
}

fn empty_result(files_scanned: u32) -> SessionSyncResult {
    SessionSyncResult {
        files_scanned,
        ..SessionSyncResult::default()
    }
}

fn token_u32(value: Option<u64>) -> u32 {
    value.unwrap_or(0).min(u32::MAX as u64) as u32
}

fn duration_i64(value: Option<u64>) -> i64 {
    value.unwrap_or(0).min(i64::MAX as u64) as i64
}

fn finite_non_negative(value: Option<f64>) -> Option<f64> {
    value.filter(|cost| cost.is_finite() && *cost >= 0.0)
}

fn source_modified_nanos(path: &Path) -> Result<i64, AppError> {
    let metadata = fs::metadata(path)
        .map_err(|e| AppError::Config(format!("无法读取 {} 元数据: {e}", path.display())))?;
    let mut modified = metadata_modified_nanos(&metadata);
    let wal_path = PathBuf::from(format!("{}-wal", path.to_string_lossy()));
    if let Ok(wal_metadata) = fs::metadata(wal_path) {
        modified = modified.max(metadata_modified_nanos(&wal_metadata));
    }
    Ok(modified)
}

fn open_source_db(path: &Path, source: &str) -> Result<rusqlite::Connection, AppError> {
    rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| AppError::Database(format!("无法只读打开 {source} 数据库: {e}")))
}

fn parse_maka_attempt(
    attempt_id: &str,
    completed_at_ms: i64,
    json: &str,
) -> Result<Option<DesktopUsageRecord>, AppError> {
    let attempt: MakaAttempt = serde_json::from_str(json)
        .map_err(|e| AppError::Config(format!("Maka 用量记录格式无效: {e}")))?;

    if attempt.usage_basis == "missing" {
        return Ok(None);
    }

    let input_tokens = token_u32(attempt.cache_miss_input_tokens.or(attempt.input_tokens));
    let output_tokens = token_u32(attempt.output_tokens);
    let cache_read_tokens = token_u32(attempt.cache_read_input_tokens);
    let cache_creation_tokens = token_u32(attempt.cache_write_input_tokens);
    if input_tokens == 0
        && output_tokens == 0
        && cache_read_tokens == 0
        && cache_creation_tokens == 0
    {
        return Ok(None);
    }

    let completed = attempt.status == "completed";
    let status_code = match attempt.status.as_str() {
        "completed" => 200,
        "interrupted" | "aborted" => 499,
        _ => 500,
    };
    let model = if attempt.model_id.trim().is_empty() {
        "unknown".to_string()
    } else {
        attempt.model_id
    };

    Ok(Some(DesktopUsageRecord {
        request_id: format!("maka_attempt:{attempt_id}"),
        app_type: MAKA_APP_TYPE,
        data_source: MAKA_DATA_SOURCE,
        provider_id: "_maka_session",
        model,
        session_id: attempt.session_id,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_creation_tokens,
        total_cost_usd: if attempt.cost_basis == "priced" {
            finite_non_negative(attempt.cost_usd)
        } else {
            None
        },
        latency_ms: duration_i64(attempt.latency_ms),
        duration_ms: None,
        first_token_ms: attempt
            .time_to_first_token_ms
            .map(|value| duration_i64(Some(value))),
        status_code,
        error_message: (!completed).then_some(attempt.status),
        created_at: completed_at_ms / 1000,
        upstream_dedup: None,
    }))
}

fn parse_codepilot_timestamp(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.timestamp())
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|timestamp| timestamp.and_utc().timestamp())
        })
}

fn parse_codepilot_message(
    message_id: &str,
    session_id: &str,
    created_at: &str,
    usage_json: &str,
    session_model: &str,
    codex_thread_id: &str,
) -> Result<Option<DesktopUsageRecord>, AppError> {
    let usage: CodePilotTokenUsage = serde_json::from_str(usage_json)
        .map_err(|e| AppError::Config(format!("CodePilot token_usage 格式无效: {e}")))?;
    let input_tokens = token_u32(usage.input_tokens);
    let output_tokens = token_u32(usage.output_tokens);
    let cache_read_tokens = token_u32(usage.cache_read_input_tokens);
    let cache_creation_tokens = token_u32(usage.cache_creation_input_tokens);
    if input_tokens == 0
        && output_tokens == 0
        && cache_read_tokens == 0
        && cache_creation_tokens == 0
    {
        return Ok(None);
    }

    let timestamp = parse_codepilot_timestamp(created_at)
        .ok_or_else(|| AppError::Config(format!("CodePilot 消息时间格式无效: {created_at}")))?;
    let model = usage
        .usage_model_id
        .filter(|model| !model.trim().is_empty())
        .or_else(|| (!session_model.trim().is_empty()).then(|| session_model.to_string()))
        .unwrap_or_else(|| "unknown".to_string());
    let is_codex_account = usage
        .context_accounting
        .as_ref()
        .and_then(|context| context.provider_backend.as_deref())
        == Some("codex_account");

    Ok(Some(DesktopUsageRecord {
        request_id: format!("codepilot_message:{message_id}"),
        app_type: CODEPILOT_APP_TYPE,
        data_source: CODEPILOT_DATA_SOURCE,
        provider_id: "_codepilot_session",
        model,
        session_id: Some(session_id.to_string()),
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_creation_tokens,
        total_cost_usd: finite_non_negative(usage.cost_usd),
        latency_ms: 0,
        duration_ms: None,
        first_token_ms: None,
        status_code: 200,
        error_message: None,
        created_at: timestamp,
        upstream_dedup: (is_codex_account && !codex_thread_id.trim().is_empty()).then(|| {
            UpstreamDedup {
                app_type: "codex",
                session_id: codex_thread_id.to_string(),
            }
        }),
    }))
}

fn calculated_costs(
    conn: &rusqlite::Connection,
    record: &DesktopUsageRecord,
) -> (String, String, String, String, String) {
    if let Some(total) = record.total_cost_usd {
        return (
            "0".to_string(),
            "0".to_string(),
            "0".to_string(),
            "0".to_string(),
            total.to_string(),
        );
    }

    let usage = TokenUsage {
        input_tokens: record.input_tokens,
        output_tokens: record.output_tokens,
        cache_read_tokens: record.cache_read_tokens,
        cache_creation_tokens: record.cache_creation_tokens,
        model: Some(record.model.clone()),
        message_id: None,
    };
    match find_model_pricing(conn, &record.model) {
        Some(pricing) => {
            // 两个桌面来源在落库前都已归一为 Anthropic 风格：input 仅表示
            // cache miss，cache read/write 分桶单列，因此这里不能再次扣缓存。
            let cost = CostCalculator::calculate(&usage, &pricing, Decimal::ONE);
            (
                cost.input_cost.to_string(),
                cost.output_cost.to_string(),
                cost.cache_read_cost.to_string(),
                cost.cache_creation_cost.to_string(),
                cost.total_cost.to_string(),
            )
        }
        None => (
            "0".to_string(),
            "0".to_string(),
            "0".to_string(),
            "0".to_string(),
            "0".to_string(),
        ),
    }
}

fn upstream_duplicate_cost(
    conn: &rusqlite::Connection,
    record: &DesktopUsageRecord,
) -> Result<Option<f64>, AppError> {
    let Some(upstream) = &record.upstream_dedup else {
        return Ok(None);
    };
    let mut statement = conn
        .prepare_cached(
            "SELECT model, total_cost_usd FROM proxy_request_logs
             WHERE app_type = ?1
               AND session_id = ?2
               AND created_at BETWEEN ?3 AND ?4
               AND input_tokens = ?5
               AND output_tokens = ?6
               AND cache_read_tokens = ?7
               AND cache_creation_tokens = ?8",
        )
        .map_err(|e| AppError::Database(format!("准备跨客户端重复查询失败: {e}")))?;
    let rows = statement
        .query_map(
            rusqlite::params![
                upstream.app_type,
                upstream.session_id,
                record.created_at.saturating_sub(120),
                record.created_at.saturating_add(120),
                record.input_tokens,
                record.output_tokens,
                record.cache_read_tokens,
                record.cache_creation_tokens,
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|e| AppError::Database(format!("查询跨客户端重复用量失败: {e}")))?;
    let record_model = canonical_cindy_model(&record.model);
    for row in rows {
        let (candidate, total_cost_usd) =
            row.map_err(|e| AppError::Database(format!("读取跨客户端重复模型失败: {e}")))?;
        let candidate_model = canonical_cindy_model(&candidate);
        if record_model.eq_ignore_ascii_case("unknown")
            || candidate_model.eq_ignore_ascii_case("unknown")
            || record_model.eq_ignore_ascii_case(&candidate_model)
        {
            let total_cost_usd = total_cost_usd
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite() && *value >= 0.0)
                .unwrap_or(0.0);
            return Ok(Some(total_cost_usd));
        }
    }
    Ok(None)
}

fn has_upstream_duplicate(
    conn: &rusqlite::Connection,
    record: &DesktopUsageRecord,
) -> Result<bool, AppError> {
    Ok(upstream_duplicate_cost(conn, record)?.is_some())
}

fn remove_desktop_usage_if_upstream_duplicate(
    db: &Database,
    record: &DesktopUsageRecord,
) -> Result<bool, AppError> {
    let conn = lock_conn!(db.conn);
    if !has_upstream_duplicate(&conn, record)? {
        return Ok(false);
    }
    let deleted = conn
        .execute(
            "DELETE FROM proxy_request_logs
             WHERE request_id = ?1 AND app_type = ?2 AND data_source = ?3",
            rusqlite::params![record.request_id, record.app_type, record.data_source],
        )
        .map_err(|e| AppError::Database(format!("清理跨客户端重复用量失败: {e}")))?;
    Ok(deleted > 0)
}

fn insert_desktop_usage_row_on_conn(
    conn: &rusqlite::Connection,
    record: &DesktopUsageRecord,
    request_model: &str,
    data_source: &str,
    total_cost_override: Option<f64>,
) -> Result<bool, AppError> {
    let (input_cost, output_cost, cache_read_cost, cache_creation_cost, total_cost) =
        if let Some(total_cost) = total_cost_override {
            (
                "0".to_string(),
                "0".to_string(),
                "0".to_string(),
                "0".to_string(),
                total_cost.to_string(),
            )
        } else {
            calculated_costs(conn, record)
        };
    conn.execute(
        "INSERT OR IGNORE INTO proxy_request_logs (
            request_id, provider_id, app_type, model, request_model, pricing_model,
            input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
            input_token_semantics,
            input_cost_usd, output_cost_usd, cache_read_cost_usd,
            cache_creation_cost_usd, total_cost_usd,
            latency_ms, first_token_ms, duration_ms, status_code, error_message, session_id,
            provider_type, is_streaming, cost_multiplier, created_at, data_source
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
            ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27
        )",
        rusqlite::params![
            record.request_id,
            record.provider_id,
            record.app_type,
            record.model,
            request_model,
            (record.app_type == CINDY_APP_TYPE).then_some(record.model.as_str()),
            record.input_tokens,
            record.output_tokens,
            record.cache_read_tokens,
            record.cache_creation_tokens,
            INPUT_TOKEN_SEMANTICS_FRESH,
            input_cost,
            output_cost,
            cache_read_cost,
            cache_creation_cost,
            total_cost,
            record.latency_ms,
            record.first_token_ms,
            record.duration_ms,
            record.status_code,
            record.error_message,
            record.session_id,
            data_source,
            1i64,
            "1.0",
            record.created_at,
            data_source,
        ],
    )
    .map(|inserted| inserted > 0)
    .map_err(|e| AppError::Database(format!("插入桌面 Agent 会话用量失败: {e}")))
}

fn insert_desktop_usage_outcome_on_conn(
    conn: &rusqlite::Connection,
    record: &DesktopUsageRecord,
) -> Result<DesktopUsageInsertOutcome, AppError> {
    if let Some(total_cost_usd) = upstream_duplicate_cost(conn, record)? {
        let mirror_inserted = record.app_type == CINDY_APP_TYPE
            && insert_desktop_usage_row_on_conn(
                conn,
                record,
                CINDY_MIRROR_REQUEST_MODEL,
                CINDY_MIRROR_DATA_SOURCE,
                Some(total_cost_usd),
            )?;
        return Ok(DesktopUsageInsertOutcome::AccountedInUpstreamApp {
            total_cost_usd,
            mirror_inserted,
        });
    }
    let dedup_key = DedupKey {
        app_type: record.app_type,
        model: &record.model,
        input_tokens: record.input_tokens,
        output_tokens: record.output_tokens,
        cache_read_tokens: record.cache_read_tokens,
        cache_creation_tokens: record.cache_creation_tokens,
        created_at: record.created_at,
    };
    if should_skip_session_insert(conn, &record.request_id, &dedup_key)? {
        return Ok(DesktopUsageInsertOutcome::AccountedInSameApp);
    }
    let inserted =
        insert_desktop_usage_row_on_conn(conn, record, &record.model, record.data_source, None)?;
    Ok(if inserted {
        DesktopUsageInsertOutcome::Inserted
    } else {
        DesktopUsageInsertOutcome::AccountedInSameApp
    })
}

fn insert_desktop_usage_on_conn(
    conn: &rusqlite::Connection,
    record: &DesktopUsageRecord,
) -> Result<bool, AppError> {
    Ok(matches!(
        insert_desktop_usage_outcome_on_conn(conn, record)?,
        DesktopUsageInsertOutcome::Inserted
    ))
}

fn insert_desktop_usage(db: &Database, record: &DesktopUsageRecord) -> Result<bool, AppError> {
    let conn = lock_conn!(db.conn);
    insert_desktop_usage_on_conn(&conn, record)
}

fn cindy_user_data_dirs() -> Vec<PathBuf> {
    if let Some(path) = std::env::var_os("CINDY_USER_DATA_DIR") {
        if !path.is_empty() {
            return vec![PathBuf::from(path)];
        }
    }

    #[cfg(target_os = "linux")]
    let base = dirs::config_dir();
    #[cfg(not(target_os = "linux"))]
    let base = dirs::data_dir();

    base.map(|dir| vec![dir.join("CindyGlobal"), dir.join("Cindy")])
        .unwrap_or_default()
}

fn collect_cindy_databases(dir: &Path) -> Result<Vec<PathBuf>, AppError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut databases = Vec::new();
    for entry in
        fs::read_dir(dir).map_err(|e| AppError::Config(format!("无法读取 Cindy 数据目录: {e}")))?
    {
        let entry =
            entry.map_err(|e| AppError::Config(format!("无法读取 Cindy 数据目录项: {e}")))?;
        let file_type = entry
            .file_type()
            .map_err(|e| AppError::Config(format!("无法读取 Cindy 数据库文件类型: {e}")))?;
        if !file_type.is_file() {
            continue;
        }
        let filename = entry.file_name();
        let filename = filename.to_string_lossy();
        if filename.starts_with("cindy-") && filename.ends_with(".db") {
            databases.push(entry.path());
        }
    }
    databases.sort();
    Ok(databases)
}

fn discover_cindy_databases(dirs: &[PathBuf]) -> Result<Vec<PathBuf>, AppError> {
    let mut databases = Vec::new();
    let mut seen_accounts = std::collections::HashSet::new();
    for dir in dirs {
        for path in collect_cindy_databases(dir)? {
            let Some(account_filename) = path.file_name().map(|name| name.to_os_string()) else {
                continue;
            };
            if seen_accounts.insert(account_filename) {
                databases.push(path);
            }
        }
    }
    Ok(databases)
}

fn cindy_hash(parts: &[&str]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    hasher.finalize().into()
}

fn cindy_short_hash(parts: &[&str]) -> String {
    cindy_hash(parts)[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn cindy_sync_key(path: &Path) -> String {
    let path = path.to_string_lossy();
    format!(
        "desktop:cindy:{CINDY_SYNC_VERSION}:{}",
        cindy_short_hash(&[path.as_ref()])
    )
}

fn cindy_source_set_marker(paths: &[PathBuf]) -> i64 {
    let path_strings = paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let parts = path_strings.iter().map(String::as_str).collect::<Vec<_>>();
    let digest = cindy_hash(&parts);
    i64::from_be_bytes(digest[..8].try_into().expect("SHA-256 prefix length")) & i64::MAX
}

fn cindy_provider_id(agent_kind: &str) -> &'static str {
    match agent_kind {
        "claude-code" | "cc" => "_cindy_claude_code",
        "codex" => "_cindy_codex",
        "pi" => "_cindy_pi",
        _ => CINDY_PROVIDER_ID,
    }
}

fn canonical_cindy_agent_kind(agent_kind: &str) -> &str {
    match agent_kind {
        "cc" | "claude-code" => "claude-code",
        _ => agent_kind,
    }
}

fn canonical_cindy_model(model: &str) -> String {
    let mut canonical = model.trim().to_string();
    if let Some((base, _)) = canonical.split_once("#billing=") {
        canonical = base.trim().to_string();
    }
    if let Some(base) = canonical.strip_suffix("[1m]") {
        canonical = base.trim_end().to_string();
    }
    if canonical.is_empty() {
        "unknown".to_string()
    } else {
        canonical
    }
}

fn cindy_billing_kind_from_model(model: &str) -> Option<CindyBillingKind> {
    let (_, billing) = model.rsplit_once("#billing=")?;
    match billing.trim().to_ascii_lowercase().as_str() {
        "api" => Some(CindyBillingKind::Api),
        "subscription" => Some(CindyBillingKind::Subscription),
        _ => None,
    }
}

fn cindy_turn_billing_hint(meta: &CindyAgentMeta) -> Option<CindyBillingKind> {
    if let Some(cost) = meta.turn_cost.as_ref() {
        if cost.approximate == Some(true)
            || cost
                .kind
                .as_deref()
                .is_some_and(|kind| kind.eq_ignore_ascii_case("value-estimate"))
        {
            return Some(CindyBillingKind::Subscription);
        }
        if cost.approximate == Some(false) {
            return Some(CindyBillingKind::Api);
        }
    }
    meta.turn_cost_usd
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
        .map(|_| CindyBillingKind::Subscription)
}

fn cindy_account_id(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| cindy_short_hash(&[path.to_string_lossy().as_ref()]))
}

fn cindy_cost_usd(usage: &CindyDailyUsage) -> f64 {
    let legacy_usd = finite_non_negative(Some(usage.cost_usd)).unwrap_or(0.0);
    let current_amount = finite_non_negative(Some(usage.cost_amount)).unwrap_or(0.0);
    if usage.cost_is_approximate {
        // Cindy marks subscription/list-price projections as approximate.
        // Provider stats retain the amount and expose that distinction to the UI.
        return if usage.cost_currency.eq_ignore_ascii_case("USD") {
            legacy_usd + current_amount
        } else {
            0.0
        };
    }
    if usage.cost_currency.eq_ignore_ascii_case("USD") {
        legacy_usd + current_amount
    } else if current_amount > 0.0 {
        // CC Switch's usage ledger is USD-only. Do not relabel a real CNY amount
        // as USD or replace it with a local price-table estimate.
        0.0
    } else {
        legacy_usd
    }
}

fn cindy_token_i64(value: i64) -> i64 {
    value.max(0)
}

fn cindy_token_from_u64(value: Option<u64>) -> i64 {
    value.unwrap_or(0).min(u32::MAX as u64) as i64
}

fn source_table_columns(
    source: &rusqlite::Connection,
    table: &str,
) -> Result<HashSet<String>, AppError> {
    let mut statement = source
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| AppError::Database(format!("检查 Cindy {table} 表失败: {e}")))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| AppError::Database(format!("读取 Cindy {table} 表结构失败: {e}")))?;
    let mut columns = HashSet::new();
    for row in rows {
        columns.insert(
            row.map_err(|e| AppError::Database(format!("读取 Cindy {table} 列失败: {e}")))?,
        );
    }
    Ok(columns)
}

fn read_cindy_daily_usage_from_conn(
    source: &rusqlite::Connection,
) -> Result<Option<Vec<CindyDailyUsage>>, AppError> {
    let columns = source_table_columns(source, "daily_model_usage")?;
    let required = [
        "day",
        "agent_kind",
        "model",
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_create_tokens",
    ];
    if required.iter().any(|column| !columns.contains(*column)) {
        return Ok(None);
    }

    let cost_usd = if columns.contains("cost_usd") {
        "cost_usd"
    } else {
        "0"
    };
    let cost_amount = if columns.contains("cost_amount") {
        "cost_amount"
    } else {
        "0"
    };
    let cost_currency = if columns.contains("cost_currency") {
        "COALESCE(cost_currency, 'USD')"
    } else {
        "'USD'"
    };
    let cost_is_approximate = if columns.contains("cost_is_approximate") {
        "cost_is_approximate"
    } else {
        "0"
    };
    let sql = format!(
        "SELECT day, agent_kind, model, {cost_usd}, {cost_amount}, {cost_currency},
                {cost_is_approximate}, input_tokens, output_tokens,
                cache_read_tokens, cache_create_tokens
         FROM daily_model_usage
         ORDER BY day, agent_kind, model, {cost_currency}"
    );
    let mut statement = source
        .prepare(&sql)
        .map_err(|e| AppError::Database(format!("准备 Cindy 用量查询失败: {e}")))?;
    let rows = statement
        .query_map([], |row| {
            Ok(CindyDailyUsage {
                day: row.get(0)?,
                agent_kind: row.get(1)?,
                model: row.get(2)?,
                cost_usd: row.get(3)?,
                cost_amount: row.get(4)?,
                cost_currency: row.get(5)?,
                cost_is_approximate: row.get(6)?,
                input_tokens: row.get(7)?,
                output_tokens: row.get(8)?,
                cache_read_tokens: row.get(9)?,
                cache_create_tokens: row.get(10)?,
            })
        })
        .map_err(|e| AppError::Database(format!("查询 Cindy 用量失败: {e}")))?;

    let mut usage = Vec::new();
    for row in rows {
        usage.push(row.map_err(|e| AppError::Database(format!("读取 Cindy 用量行失败: {e}")))?);
    }
    Ok(Some(usage))
}

fn read_cindy_turn_usage_from_conn(
    source: &rusqlite::Connection,
    account_id: &str,
    detail_cutoff: i64,
) -> Result<Vec<CindyTurnUsage>, AppError> {
    let message_columns = source_table_columns(source, "messages")?;
    let session_columns = source_table_columns(source, "sessions")?;
    let message_required = ["id", "session_id", "role", "created_at", "agent_meta"];
    if message_required
        .iter()
        .any(|column| !message_columns.contains(*column))
        || !session_columns.contains("id")
    {
        return Ok(Vec::new());
    }

    let message_agent = if message_columns.contains("agent_kind") {
        "m.agent_kind"
    } else {
        "NULL"
    };
    let session_agent = if session_columns.contains("agent_kind") {
        "COALESCE(s.agent_kind, 'unknown')"
    } else {
        "'unknown'"
    };
    let session_model = if session_columns.contains("model") {
        "COALESCE(s.model, 'unknown')"
    } else {
        "'unknown'"
    };
    let sdk_session_id = if session_columns.contains("sdk_session_id") {
        "s.sdk_session_id"
    } else {
        "NULL"
    };
    let parent_session_id = if session_columns.contains("parent_session_id") {
        "s.parent_session_id"
    } else {
        "NULL"
    };
    let session_created_at = if session_columns.contains("created_at") {
        "s.created_at"
    } else {
        "0"
    };
    let sql = format!(
        "SELECT m.id, m.session_id, m.created_at, m.agent_meta,
                {message_agent}, {session_agent}, {session_model}, {sdk_session_id},
                {parent_session_id}, {session_created_at}
         FROM messages m
         JOIN sessions s ON s.id = m.session_id
         WHERE m.role = 'assistant' AND m.agent_meta IS NOT NULL
           AND m.created_at >= ?1
         ORDER BY m.created_at, m.id"
    );
    let cutoff_ms = detail_cutoff.saturating_mul(1000);
    let mut statement = source
        .prepare(&sql)
        .map_err(|e| AppError::Database(format!("准备 Cindy turn 查询失败: {e}")))?;
    let rows = statement
        .query_map([cutoff_ms], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, i64>(9)?,
            ))
        })
        .map_err(|e| AppError::Database(format!("查询 Cindy turn 失败: {e}")))?;

    let account_hash = cindy_short_hash(&[account_id]);
    let mut turns = Vec::new();
    for row in rows {
        let (
            message_id,
            session_id,
            created_at_ms,
            agent_meta,
            message_agent,
            session_agent,
            session_model,
            sdk_session_id,
            parent_session_id,
            session_created_at,
        ) = row.map_err(|e| AppError::Database(format!("读取 Cindy turn 行失败: {e}")))?;
        if parent_session_id.is_some() && created_at_ms < session_created_at {
            continue;
        }
        let meta: CindyAgentMeta = match serde_json::from_str(&agent_meta) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if meta.turn_completed != Some(true) {
            continue;
        }
        let billing_hint = cindy_turn_billing_hint(&meta);
        let Some(details) = meta.turn_usage_details else {
            continue;
        };
        if details.models.len() > 1 {
            continue;
        }
        let input_tokens = cindy_token_from_u64(details.input_tokens);
        let output_tokens = cindy_token_from_u64(details.output_tokens);
        let cache_read_tokens = cindy_token_from_u64(details.cache_read_tokens);
        let cache_create_tokens = cindy_token_from_u64(details.cache_create_tokens);
        if input_tokens == 0
            && output_tokens == 0
            && cache_read_tokens == 0
            && cache_create_tokens == 0
        {
            continue;
        }
        let Some(created_at) = Local.timestamp_millis_opt(created_at_ms).single() else {
            continue;
        };
        let agent_kind = message_agent
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(session_agent);
        let model = details
            .model
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(session_model);
        let canonical_model = canonical_cindy_model(&model);
        turns.push(CindyTurnUsage {
            request_id: format!(
                "cindy:turn:{}",
                cindy_short_hash(&[account_id, "turn", &message_id])
            ),
            day: created_at.format("%Y-%m-%d").to_string(),
            agent_kind,
            canonical_model,
            billing_hint,
            session_id: format!("cindy:{account_hash}:{session_id}"),
            sdk_session_id,
            created_at: created_at.timestamp(),
            duration_ms: duration_i64(details.duration_ms.or(details.turn_duration_ms)),
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_create_tokens,
        });
    }
    Ok(turns)
}

fn read_cindy_usage_snapshot(
    source_path: &Path,
    detail_cutoff: i64,
) -> Result<Option<CindyUsageSnapshot>, AppError> {
    let source = open_source_db(source_path, "Cindy")?;
    source
        .execute_batch("BEGIN DEFERRED TRANSACTION")
        .map_err(|e| AppError::Database(format!("开始 Cindy 只读快照失败: {e}")))?;
    let Some(daily) = read_cindy_daily_usage_from_conn(&source)? else {
        return Ok(None);
    };
    let account_id = cindy_account_id(source_path);
    let turns = read_cindy_turn_usage_from_conn(&source, &account_id, detail_cutoff)?;
    source
        .execute_batch("COMMIT")
        .map_err(|e| AppError::Database(format!("提交 Cindy 只读快照失败: {e}")))?;
    Ok(Some(CindyUsageSnapshot {
        account_id,
        daily,
        turns,
    }))
}

fn insert_cindy_daily_rollup(
    conn: &rusqlite::Connection,
    key: &CindyUsageKey,
    model: &str,
    tokens: CindyTokenBuckets,
    total_cost_usd: f64,
) -> Result<(), AppError> {
    let request_model = if matches!(key.agent_kind.as_str(), "claude-code" | "cc") {
        CINDY_MIRROR_REQUEST_MODEL
    } else {
        model
    };
    conn.execute(
        "INSERT INTO usage_daily_rollups (
            date, app_type, provider_id, model, request_model, pricing_model,
            request_count, success_count, input_tokens, output_tokens,
            cache_read_tokens, cache_creation_tokens, input_token_semantics,
            total_cost_usd, avg_latency_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?4, 0, 0, ?6, ?7, ?8, ?9, ?10, ?11, 0)
         ON CONFLICT(date, app_type, provider_id, model, request_model, pricing_model)
         DO UPDATE SET
            input_tokens = input_tokens + excluded.input_tokens,
            output_tokens = output_tokens + excluded.output_tokens,
            cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
            cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
            input_token_semantics = excluded.input_token_semantics,
            total_cost_usd = CAST(
                CAST(total_cost_usd AS REAL) + CAST(excluded.total_cost_usd AS REAL)
                AS TEXT
            )",
        rusqlite::params![
            key.day,
            CINDY_APP_TYPE,
            cindy_provider_id(&key.agent_kind),
            model,
            request_model,
            tokens.input,
            tokens.output,
            tokens.cache_read,
            tokens.cache_create,
            INPUT_TOKEN_SEMANTICS_FRESH,
            total_cost_usd.to_string(),
        ],
    )
    .map_err(|e| AppError::Database(format!("写入 Cindy 日补差失败: {e}")))?;
    Ok(())
}

fn replace_cindy_usage(
    db: &Database,
    sources: &[CindyUsageSnapshot],
    detail_cutoff: i64,
) -> Result<(u32, bool), AppError> {
    let mut conn = lock_conn!(db.conn);
    let detail_cutoff_day = Local
        .timestamp_opt(detail_cutoff, 0)
        .single()
        .ok_or_else(|| AppError::Database("Cindy 明细截止时间无效".to_string()))?
        .format("%Y-%m-%d")
        .to_string();
    let archived_metrics = {
        let mut statement = conn
            .prepare(
                "SELECT date, provider_id, model, request_model, pricing_model,
                        request_count, success_count
                 FROM usage_daily_rollups
                 WHERE app_type = ?1 AND date < ?2 AND request_count > 0",
            )
            .map_err(|e| AppError::Database(format!("准备 Cindy 归档指标查询失败: {e}")))?;
        let rows = statement
            .query_map(
                rusqlite::params![CINDY_APP_TYPE, detail_cutoff_day],
                |row| {
                    Ok(CindyArchivedRollupMetrics {
                        date: row.get(0)?,
                        provider_id: row.get(1)?,
                        model: row.get(2)?,
                        request_model: row.get(3)?,
                        pricing_model: row.get(4)?,
                        request_count: row.get(5)?,
                        success_count: row.get(6)?,
                    })
                },
            )
            .map_err(|e| AppError::Database(format!("查询 Cindy 归档指标失败: {e}")))?;
        let mut metrics = Vec::new();
        for row in rows {
            metrics.push(
                row.map_err(|e| AppError::Database(format!("读取 Cindy 归档指标失败: {e}")))?,
            );
        }
        metrics
    };
    let previous_rows: i64 = conn
        .query_row(
            "SELECT
                (SELECT COUNT(*) FROM proxy_request_logs
                 WHERE app_type = ?1 AND data_source IN (?2, ?3, ?4)) +
                (SELECT COUNT(*) FROM usage_daily_rollups WHERE app_type = ?1) +
                (SELECT COUNT(*) FROM usage_daily_activity_rollups WHERE app_type = ?1) +
                (SELECT COUNT(*) FROM usage_daily_activity_session_rollups WHERE app_type = ?1)",
            rusqlite::params![
                CINDY_APP_TYPE,
                CINDY_LEGACY_DATA_SOURCE,
                CINDY_DETAIL_DATA_SOURCE,
                CINDY_MIRROR_DATA_SOURCE
            ],
            |row| row.get(0),
        )
        .map_err(|e| AppError::Database(format!("检查旧 Cindy 用量失败: {e}")))?;
    let transaction = conn
        .transaction()
        .map_err(|e| AppError::Database(format!("开始 Cindy 用量同步事务失败: {e}")))?;
    transaction
        .execute(
            "DELETE FROM proxy_request_logs
             WHERE app_type = ?1 AND data_source IN (?2, ?3, ?4)",
            rusqlite::params![
                CINDY_APP_TYPE,
                CINDY_LEGACY_DATA_SOURCE,
                CINDY_DETAIL_DATA_SOURCE,
                CINDY_MIRROR_DATA_SOURCE
            ],
        )
        .map_err(|e| AppError::Database(format!("清理 Cindy turn 用量失败: {e}")))?;
    transaction
        .execute(
            "DELETE FROM usage_daily_rollups WHERE app_type = ?1",
            [CINDY_APP_TYPE],
        )
        .map_err(|e| AppError::Database(format!("清理 Cindy 历史用量失败: {e}")))?;
    transaction
        .execute(
            "DELETE FROM usage_daily_activity_rollups
             WHERE app_type = ?1 AND date >= ?2",
            rusqlite::params![CINDY_APP_TYPE, detail_cutoff_day],
        )
        .map_err(|e| AppError::Database(format!("清理 Cindy 活跃用量失败: {e}")))?;
    transaction
        .execute(
            "DELETE FROM usage_daily_activity_session_rollups
             WHERE app_type = ?1 AND date >= ?2",
            rusqlite::params![CINDY_APP_TYPE, detail_cutoff_day],
        )
        .map_err(|e| AppError::Database(format!("清理 Cindy 活跃会话失败: {e}")))?;

    let mut daily_by_key: HashMap<CindyUsageKey, CindyDailyAggregate> = HashMap::new();
    let mut turns_by_key: HashMap<CindyUsageKey, Vec<&CindyTurnUsage>> = HashMap::new();
    for source in sources {
        for usage in &source.daily {
            NaiveDate::parse_from_str(&usage.day, "%Y-%m-%d").map_err(|e| {
                AppError::Config(format!("Cindy 用量日期格式无效 {}: {e}", usage.day))
            })?;
            let source_model = if usage.model.trim().is_empty() {
                "unknown".to_string()
            } else {
                usage.model.clone()
            };
            let key = CindyUsageKey {
                account_id: source.account_id.clone(),
                day: usage.day.clone(),
                agent_kind: canonical_cindy_agent_kind(&usage.agent_kind).to_string(),
                canonical_model: canonical_cindy_model(&source_model),
            };
            let aggregate = daily_by_key.entry(key).or_default();
            let tokens = CindyTokenBuckets::from_daily(usage);
            let total_cost_usd = cindy_cost_usd(usage);
            let source_model_aggregate = aggregate.source_models.entry(source_model).or_default();
            source_model_aggregate.tokens.add_assign(tokens);
            source_model_aggregate.total_cost_usd += total_cost_usd;
        }
        for usage in &source.turns {
            let key = CindyUsageKey {
                account_id: source.account_id.clone(),
                day: usage.day.clone(),
                agent_kind: canonical_cindy_agent_kind(&usage.agent_kind).to_string(),
                canonical_model: usage.canonical_model.clone(),
            };
            turns_by_key.entry(key).or_default().push(usage);
        }
    }

    let mut accepted_turn_models = HashMap::new();
    for (key, turns) in &turns_by_key {
        if let Some(assignments) = daily_by_key
            .get(key)
            .and_then(|daily| assign_cindy_turn_models(daily, turns))
        {
            accepted_turn_models.extend(assignments);
        } else {
            log::warn!(
                "[CINDY-SYNC] 明细无法安全映射到日表，回退日汇总: day={}, agent={}, model={}",
                key.day,
                key.agent_kind,
                key.canonical_model
            );
        }
    }

    let mut imported = 0u32;
    let mut accounted_turn_totals: HashMap<(CindyUsageKey, String), CindyTokenBuckets> =
        HashMap::new();
    let mut upstream_turn_costs: HashMap<(CindyUsageKey, String), f64> = HashMap::new();
    for (key, turns) in &turns_by_key {
        for usage in turns {
            let Some(model) = accepted_turn_models.get(&usage.request_id).cloned() else {
                continue;
            };
            let model_key = (key.clone(), model.clone());
            let upstream_dedup = usage
                .sdk_session_id
                .as_ref()
                .filter(|session_id| !session_id.trim().is_empty())
                .and_then(|session_id| {
                    let app_type = match usage.agent_kind.as_str() {
                        "codex" => Some("codex"),
                        "claude-code" | "cc" => Some("claude"),
                        "pi" => Some("pi"),
                        _ => None,
                    }?;
                    Some(UpstreamDedup {
                        app_type,
                        session_id: session_id.clone(),
                    })
                });
            let record = DesktopUsageRecord {
                request_id: usage.request_id.clone(),
                app_type: CINDY_APP_TYPE,
                data_source: CINDY_DETAIL_DATA_SOURCE,
                provider_id: cindy_provider_id(&usage.agent_kind),
                model,
                session_id: Some(usage.session_id.clone()),
                input_tokens: usage.input_tokens as u32,
                output_tokens: usage.output_tokens as u32,
                cache_read_tokens: usage.cache_read_tokens as u32,
                cache_creation_tokens: usage.cache_create_tokens as u32,
                // Keep Cindy's authoritative cost on the daily row. Turn-level
                // cost is optional/estimated for several agents and mixing the
                // two sources would make the daily total drift.
                total_cost_usd: Some(0.0),
                // Cindy exposes whole-turn wall-clock duration, not provider
                // request latency. Preserve it separately and keep latency unknown.
                latency_ms: 0,
                duration_ms: Some(usage.duration_ms),
                first_token_ms: None,
                status_code: 200,
                error_message: None,
                created_at: usage.created_at,
                upstream_dedup,
            };
            match insert_desktop_usage_outcome_on_conn(&transaction, &record)? {
                DesktopUsageInsertOutcome::Inserted => {
                    imported = imported.saturating_add(1);
                    accounted_turn_totals
                        .entry(model_key)
                        .or_default()
                        .add_assign(CindyTokenBuckets::from_turn(usage));
                }
                DesktopUsageInsertOutcome::AccountedInSameApp => {
                    // A Cindy proxy/detail row already carries this usage, so
                    // subtract it from the daily residual without adding a
                    // second Cindy row.
                    accounted_turn_totals
                        .entry(model_key)
                        .or_default()
                        .add_assign(CindyTokenBuckets::from_turn(usage));
                }
                DesktopUsageInsertOutcome::AccountedInUpstreamApp {
                    total_cost_usd,
                    mirror_inserted,
                } => {
                    // The native Codex/Pi row already contributes this turn to
                    // All usage. Count it as accounted so Cindy's residual does
                    // not reintroduce a cross-app duplicate.
                    accounted_turn_totals
                        .entry(model_key.clone())
                        .or_default()
                        .add_assign(CindyTokenBuckets::from_turn(usage));
                    *upstream_turn_costs.entry(model_key).or_default() += total_cost_usd;
                    if mirror_inserted {
                        imported = imported.saturating_add(1);
                    }
                }
            }
        }
    }

    for (key, daily) in &daily_by_key {
        for (model, source_model) in &daily.source_models {
            let model_key = (key.clone(), model.clone());
            let covered = accounted_turn_totals
                .get(&model_key)
                .copied()
                .unwrap_or_default();
            if !covered.fits_within(source_model.tokens) {
                return Err(AppError::Database(format!(
                    "Cindy 明细补差超过日表: day={}, agent={}, model={model}",
                    key.day, key.agent_kind
                )));
            }
            let residual = source_model.tokens.subtract(covered);
            let upstream_cost = upstream_turn_costs.get(&model_key).copied().unwrap_or(0.0);
            let residual_cost = (source_model.total_cost_usd - upstream_cost).max(0.0);
            if residual.is_zero() && residual_cost == 0.0 {
                continue;
            }
            insert_cindy_daily_rollup(&transaction, key, model, residual, residual_cost)?;
            imported = imported.saturating_add(1);
        }
    }

    for metrics in archived_metrics {
        transaction
            .execute(
                "UPDATE usage_daily_rollups
                 SET request_count = ?1, success_count = ?2, avg_latency_ms = 0
                 WHERE date = ?3 AND app_type = ?4 AND provider_id = ?5
                   AND model = ?6 AND request_model = ?7 AND pricing_model = ?8",
                rusqlite::params![
                    metrics.request_count,
                    metrics.success_count,
                    metrics.date,
                    CINDY_APP_TYPE,
                    metrics.provider_id,
                    metrics.model,
                    metrics.request_model,
                    metrics.pricing_model,
                ],
            )
            .map_err(|e| AppError::Database(format!("恢复 Cindy 归档指标失败: {e}")))?;
    }

    transaction
        .execute(
            "INSERT INTO usage_daily_activity_rollups (
                date, app_type, request_count, session_count,
                input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                input_token_semantics, total_cost_usd
             )
             SELECT date, app_type, 0, 0,
                    SUM(input_tokens), SUM(output_tokens), SUM(cache_read_tokens),
                    SUM(cache_creation_tokens), ?2,
                    CAST(SUM(CAST(total_cost_usd AS REAL)) AS TEXT)
             FROM usage_daily_rollups
             WHERE app_type = ?1 AND request_model <> ?3
             GROUP BY date, app_type
             ON CONFLICT(date, app_type) DO UPDATE SET
                input_tokens = excluded.input_tokens,
                output_tokens = excluded.output_tokens,
                cache_read_tokens = excluded.cache_read_tokens,
                cache_creation_tokens = excluded.cache_creation_tokens,
                input_token_semantics = excluded.input_token_semantics,
                total_cost_usd = excluded.total_cost_usd",
            rusqlite::params![
                CINDY_APP_TYPE,
                INPUT_TOKEN_SEMANTICS_FRESH,
                CINDY_MIRROR_REQUEST_MODEL
            ],
        )
        .map_err(|e| AppError::Database(format!("汇总 Cindy 活跃用量失败: {e}")))?;
    transaction
        .execute(
            "DELETE FROM usage_daily_activity_rollups
             WHERE app_type = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM usage_daily_rollups r
                   WHERE r.app_type = ?1
                     AND r.date = usage_daily_activity_rollups.date
               )",
            [CINDY_APP_TYPE],
        )
        .map_err(|e| AppError::Database(format!("清理失效 Cindy 活跃日期失败: {e}")))?;
    transaction
        .execute(
            "DELETE FROM usage_daily_activity_session_rollups
             WHERE app_type = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM usage_daily_rollups r
                   WHERE r.app_type = ?1
                     AND r.date = usage_daily_activity_session_rollups.date
               )",
            [CINDY_APP_TYPE],
        )
        .map_err(|e| AppError::Database(format!("清理失效 Cindy 活跃会话失败: {e}")))?;
    transaction
        .commit()
        .map_err(|e| AppError::Database(format!("提交 Cindy 用量同步失败: {e}")))?;
    Ok((imported, previous_rows > 0 || imported > 0))
}

fn sync_cindy_usage_from_paths_with_force(
    db: &Database,
    source_paths: &[PathBuf],
    force_refresh: bool,
) -> Result<SessionSyncResult, AppError> {
    let files_scanned = source_paths.len().min(u32::MAX as usize) as u32;
    let source_set_marker = cindy_source_set_marker(source_paths);
    let mut needs_refresh =
        force_refresh || get_sync_state(db, CINDY_SOURCE_SET_SYNC_KEY)?.0 != source_set_marker;
    let mut modified_times = Vec::with_capacity(source_paths.len());
    let mut result = empty_result(files_scanned);

    for path in source_paths {
        match source_modified_nanos(path) {
            Ok(modified) => {
                if modified != get_sync_state(db, &cindy_sync_key(path))?.0 {
                    needs_refresh = true;
                }
                modified_times.push((path.clone(), modified));
            }
            Err(_) => {
                let path = path.to_string_lossy();
                let source_id = cindy_short_hash(&[path.as_ref()]);
                result
                    .errors
                    .push(format!("Cindy 数据库元数据读取失败 ({source_id})"));
            }
        }
    }
    if !needs_refresh {
        return Ok(result);
    }

    let detail_cutoff = Database::usage_rollup_cutoff(CINDY_DETAIL_RETAIN_DAYS)?;
    let mut sources = Vec::with_capacity(source_paths.len());
    let mut transient_failure = modified_times.len() != source_paths.len();
    for (path, _) in &modified_times {
        match read_cindy_usage_snapshot(path, detail_cutoff) {
            Ok(Some(usage)) => sources.push(usage),
            Ok(None) => result
                .errors
                .push("Cindy 账号用量跳过: 数据库版本尚无 daily_model_usage 表".to_string()),
            Err(error) => {
                transient_failure = true;
                result
                    .errors
                    .push(format!("Cindy 账号用量读取失败: {error}"));
            }
        }
    }

    if transient_failure {
        return Ok(result);
    }

    let (imported, data_changed) = replace_cindy_usage(db, &sources, detail_cutoff)?;
    result.imported = imported;
    result.data_changed = data_changed;
    for (path, modified) in modified_times {
        update_sync_state(db, &cindy_sync_key(&path), modified, 0)?;
    }
    update_sync_state(db, CINDY_SOURCE_SET_SYNC_KEY, source_set_marker, 0)?;

    if result.imported > 0 {
        log::info!(
            "[CINDY-SYNC] 同步完成: 导入 {} 条 turn/日补差记录, 扫描 {} 个账号数据库",
            result.imported,
            source_paths.len()
        );
    }
    Ok(result)
}

fn sync_cindy_usage_from_paths(
    db: &Database,
    source_paths: &[PathBuf],
) -> Result<SessionSyncResult, AppError> {
    sync_cindy_usage_from_paths_with_force(db, source_paths, false)
}

pub fn sync_cindy_usage(db: &Database) -> Result<SessionSyncResult, AppError> {
    let databases = discover_cindy_databases(&cindy_user_data_dirs())?;
    sync_cindy_usage_from_paths(db, &databases)
}

pub(crate) fn reconcile_cindy_usage(db: &Database) -> Result<SessionSyncResult, AppError> {
    let databases = discover_cindy_databases(&cindy_user_data_dirs())?;
    sync_cindy_usage_from_paths_with_force(db, &databases, true)
}

fn maka_db_path() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join("Maka/workspaces/default/runtime.sqlite"))
}

fn codepilot_db_path() -> Option<PathBuf> {
    dirs::home_dir().map(|dir| dir.join(".codepilot/codepilot.db"))
}

fn deepseek_harness_sessions_dir() -> Option<PathBuf> {
    std::env::var_os("DSH_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|dir| dir.join(".dsh")))
        .map(|dir| dir.join("sessions"))
}

fn collect_deepseek_harness_session_files(
    dir: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<(), AppError> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)
        .map_err(|e| AppError::Config(format!("无法读取 DeepSeek Harness 会话目录: {e}")))?
    {
        let entry = entry
            .map_err(|e| AppError::Config(format!("无法读取 DeepSeek Harness 会话目录项: {e}")))?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|e| {
            AppError::Config(format!("无法读取 DeepSeek Harness 会话文件类型: {e}"))
        })?;
        if file_type.is_dir() {
            collect_deepseek_harness_session_files(&path, files)?;
        } else if file_type.is_file()
            && path.file_name().and_then(|name| name.to_str()) == Some("session.jsonl.zstd")
        {
            files.push(path);
        }
    }
    Ok(())
}

fn parse_deepseek_harness_file(
    db: &Database,
    source_path: &Path,
) -> Result<SessionSyncResult, AppError> {
    let modified = source_modified_nanos(source_path)?;
    let sync_key = format!("desktop:deepseek-harness:{}", source_path.to_string_lossy());
    if modified <= get_sync_state(db, &sync_key)?.0 {
        return Ok(empty_result(1));
    }

    let file = fs::File::open(source_path).map_err(|e| {
        AppError::Config(format!(
            "无法打开 DeepSeek Harness 会话 {}: {e}",
            source_path.display()
        ))
    })?;
    let decoder = zstd::stream::read::Decoder::new(file).map_err(|e| {
        AppError::Config(format!(
            "无法解压 DeepSeek Harness 会话 {}: {e}",
            source_path.display()
        ))
    })?;
    let reader = BufReader::new(decoder);
    let fallback_session_id = source_path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
        .to_string();
    let mut session_id = fallback_session_id;
    let mut current_model = "unknown".to_string();
    let mut result = empty_result(1);

    for (line_index, line) in reader.lines().enumerate() {
        let line_number = line_index + 1;
        let line = line.map_err(|e| {
            AppError::Config(format!(
                "读取 DeepSeek Harness 会话 {} 第 {line_number} 行失败: {e}",
                source_path.display()
            ))
        })?;
        if line.trim().is_empty() {
            continue;
        }
        let event: serde_json::Value = serde_json::from_str(&line).map_err(|e| {
            AppError::Config(format!(
                "DeepSeek Harness 会话 {} 第 {line_number} 行格式无效: {e}",
                source_path.display()
            ))
        })?;
        match event.get("type").and_then(|value| value.as_str()) {
            Some("session") => {
                if let Some(id) = event.get("id").and_then(|value| value.as_str()) {
                    if !id.trim().is_empty() {
                        session_id = id.to_string();
                    }
                }
            }
            Some("request/context") => {
                if let Some(model) = event
                    .pointer("/data/model")
                    .and_then(|value| value.as_str())
                {
                    if !model.trim().is_empty() {
                        current_model = model.to_string();
                    }
                }
            }
            Some("assistant/message") => {
                let Some(usage_value) = event.pointer("/data/usage") else {
                    continue;
                };
                let usage: DeepSeekHarnessUsage = serde_json::from_value(usage_value.clone())
                    .map_err(|e| {
                        AppError::Config(format!(
                            "DeepSeek Harness 会话 {} 第 {line_number} 行 usage 无效: {e}",
                            source_path.display()
                        ))
                    })?;
                let input_tokens = token_u32(usage.input_tokens);
                let output_tokens = token_u32(usage.output_tokens);
                let cache_read_tokens = token_u32(usage.cache_read_tokens);
                let cache_creation_tokens = token_u32(usage.cache_write_tokens);
                if input_tokens == 0
                    && output_tokens == 0
                    && cache_read_tokens == 0
                    && cache_creation_tokens == 0
                {
                    result.skipped = result.skipped.saturating_add(1);
                    continue;
                }
                let seq = event
                    .get("seq")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(line_number as u64);
                let created_at = event
                    .get("time")
                    .and_then(|value| value.as_i64())
                    .unwrap_or(0)
                    / 1000;
                if created_at <= 0 {
                    result.errors.push(format!(
                        "DeepSeek Harness session {session_id} seq {seq}: 缺少有效时间戳"
                    ));
                    continue;
                }
                let record = DesktopUsageRecord {
                    request_id: format!("deepseek_harness_message:{session_id}:{seq}"),
                    app_type: DEEPSEEK_HARNESS_APP_TYPE,
                    data_source: DEEPSEEK_HARNESS_DATA_SOURCE,
                    provider_id: "_deepseek_harness_session",
                    model: current_model.clone(),
                    session_id: Some(session_id.clone()),
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    cache_creation_tokens,
                    total_cost_usd: None,
                    latency_ms: 0,
                    duration_ms: None,
                    first_token_ms: None,
                    status_code: 200,
                    error_message: None,
                    created_at,
                    upstream_dedup: None,
                };
                if insert_desktop_usage(db, &record)? {
                    result.imported = result.imported.saturating_add(1);
                } else {
                    result.skipped = result.skipped.saturating_add(1);
                }
            }
            _ => {}
        }
    }

    if result.errors.is_empty() {
        update_sync_state(db, &sync_key, modified, 0)?;
    }
    Ok(result)
}

pub fn sync_deepseek_harness_usage(db: &Database) -> Result<SessionSyncResult, AppError> {
    let Some(sessions_dir) = deepseek_harness_sessions_dir() else {
        return Ok(empty_result(0));
    };
    let mut files = Vec::new();
    collect_deepseek_harness_session_files(&sessions_dir, &mut files)?;
    files.sort();

    let mut result = SessionSyncResult::default();
    for file in files {
        match parse_deepseek_harness_file(db, &file) {
            Ok(file_result) => result.merge(file_result),
            Err(error) => result
                .errors
                .push(format!("DeepSeek Harness {}: {error}", file.display())),
        }
    }
    if result.imported > 0 {
        log::info!(
            "[DEEPSEEK-HARNESS-SYNC] 同步完成: 导入 {} 条, 跳过 {} 条, 扫描 {} 个文件",
            result.imported,
            result.skipped,
            result.files_scanned
        );
    }
    Ok(result)
}

pub fn sync_maka_usage(db: &Database) -> Result<SessionSyncResult, AppError> {
    match maka_db_path() {
        Some(path) => sync_maka_usage_from_path(db, &path),
        None => Ok(empty_result(0)),
    }
}

fn sync_maka_usage_from_path(
    db: &Database,
    source_path: &Path,
) -> Result<SessionSyncResult, AppError> {
    if !source_path.exists() {
        return Ok(empty_result(0));
    }
    let modified = source_modified_nanos(source_path)?;
    let sync_key = format!("desktop:maka:{}", source_path.to_string_lossy());
    if modified <= get_sync_state(db, &sync_key)?.0 {
        return Ok(empty_result(1));
    }

    let source = open_source_db(source_path, "Maka")?;
    let mut stmt = source
        .prepare(
            "SELECT attempt_id, completed_at, record_json
             FROM usage_model_call_attempts
             ORDER BY completed_at, attempt_id",
        )
        .map_err(|e| AppError::Database(format!("准备 Maka 用量查询失败: {e}")))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|e| AppError::Database(format!("查询 Maka 用量失败: {e}")))?;

    let mut result = empty_result(1);
    let mut failed = false;
    for row in rows {
        let (attempt_id, completed_at, json) =
            row.map_err(|e| AppError::Database(format!("读取 Maka 用量行失败: {e}")))?;
        match parse_maka_attempt(&attempt_id, completed_at, &json)
            .and_then(|record| record.map_or(Ok(false), |record| insert_desktop_usage(db, &record)))
        {
            Ok(true) => result.imported = result.imported.saturating_add(1),
            Ok(false) => result.skipped = result.skipped.saturating_add(1),
            Err(error) => {
                failed = true;
                result
                    .errors
                    .push(format!("Maka attempt {attempt_id}: {error}"));
            }
        }
    }
    if !failed {
        update_sync_state(db, &sync_key, modified, 0)?;
    }
    if result.imported > 0 {
        log::info!(
            "[MAKA-SYNC] 同步完成: 导入 {} 条, 跳过 {} 条",
            result.imported,
            result.skipped
        );
    }
    Ok(result)
}

pub fn sync_codepilot_usage(db: &Database) -> Result<SessionSyncResult, AppError> {
    match codepilot_db_path() {
        Some(path) => sync_codepilot_usage_from_path(db, &path),
        None => Ok(empty_result(0)),
    }
}

fn reconcile_codepilot_codex_usage(
    db: &Database,
    source: &rusqlite::Connection,
) -> Result<bool, AppError> {
    let mut statement = source
        .prepare(
            "SELECT m.id, m.session_id, m.created_at, m.token_usage,
                    COALESCE(s.model, ''), COALESCE(s.codex_thread_id, '')
             FROM messages m
             JOIN chat_sessions s ON s.id = m.session_id
             WHERE m.role = 'assistant'
               AND m.stream_status = 'completed'
               AND m.token_usage IS NOT NULL
               AND COALESCE(s.codex_thread_id, '') <> ''",
        )
        .map_err(|e| AppError::Database(format!("准备 CodePilot 去重查询失败: {e}")))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(|e| AppError::Database(format!("查询 CodePilot 去重记录失败: {e}")))?;

    let mut data_changed = false;
    for row in rows {
        let (message_id, session_id, created_at, usage_json, session_model, codex_thread_id) =
            row.map_err(|e| AppError::Database(format!("读取 CodePilot 去重记录失败: {e}")))?;
        if let Some(record) = parse_codepilot_message(
            &message_id,
            &session_id,
            &created_at,
            &usage_json,
            &session_model,
            &codex_thread_id,
        )? {
            data_changed |= remove_desktop_usage_if_upstream_duplicate(db, &record)?;
        }
    }
    Ok(data_changed)
}

fn sync_codepilot_usage_from_path(
    db: &Database,
    source_path: &Path,
) -> Result<SessionSyncResult, AppError> {
    if !source_path.exists() {
        return Ok(empty_result(0));
    }
    let modified = source_modified_nanos(source_path)?;
    let sync_key = format!("desktop:codepilot:{}", source_path.to_string_lossy());
    let source = open_source_db(source_path, "CodePilot")?;
    let mut result = empty_result(1);
    result.data_changed = reconcile_codepilot_codex_usage(db, &source)?;
    if modified <= get_sync_state(db, &sync_key)?.0 {
        return Ok(result);
    }

    let mut stmt = source
        .prepare(
            "SELECT m.id, m.session_id, m.created_at, m.token_usage,
                    COALESCE(s.model, ''), COALESCE(s.codex_thread_id, '')
             FROM messages m
             LEFT JOIN chat_sessions s ON s.id = m.session_id
             WHERE m.role = 'assistant'
               AND m.stream_status = 'completed'
               AND m.token_usage IS NOT NULL
             ORDER BY m.created_at, m.id",
        )
        .map_err(|e| AppError::Database(format!("准备 CodePilot 用量查询失败: {e}")))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(|e| AppError::Database(format!("查询 CodePilot 用量失败: {e}")))?;

    let mut failed = false;
    for row in rows {
        let (message_id, session_id, created_at, usage_json, session_model, codex_thread_id) =
            row.map_err(|e| AppError::Database(format!("读取 CodePilot 用量行失败: {e}")))?;
        match parse_codepilot_message(
            &message_id,
            &session_id,
            &created_at,
            &usage_json,
            &session_model,
            &codex_thread_id,
        )
        .and_then(|record| record.map_or(Ok(false), |record| insert_desktop_usage(db, &record)))
        {
            Ok(true) => result.imported = result.imported.saturating_add(1),
            Ok(false) => result.skipped = result.skipped.saturating_add(1),
            Err(error) => {
                failed = true;
                result
                    .errors
                    .push(format!("CodePilot message {message_id}: {error}"));
            }
        }
    }
    if !failed {
        update_sync_state(db, &sync_key, modified, 0)?;
    }
    if result.imported > 0 {
        log::info!(
            "[CODEPILOT-SYNC] 同步完成: 导入 {} 条, 跳过 {} 条",
            result.imported,
            result.skipped
        );
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;
    use rusqlite::Connection;
    use tempfile::tempdir;

    fn create_cindy_usage_table(source: &Connection) {
        source
            .execute_batch(
                "CREATE TABLE daily_model_usage (
                    day TEXT NOT NULL,
                    agent_kind TEXT NOT NULL,
                    model TEXT NOT NULL,
                    cost_usd REAL NOT NULL DEFAULT 0,
                    cost_amount REAL NOT NULL DEFAULT 0,
                    cost_currency TEXT DEFAULT 'USD',
                    cost_is_approximate INTEGER NOT NULL DEFAULT 0,
                    input_tokens INTEGER NOT NULL DEFAULT 0,
                    output_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_create_tokens INTEGER NOT NULL DEFAULT 0,
                    updated_at INTEGER NOT NULL,
                    PRIMARY KEY (day, agent_kind, model, cost_currency)
                 );",
            )
            .unwrap();
    }

    fn create_cindy_turn_tables(source: &Connection) {
        source
            .execute_batch(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY,
                    model TEXT NOT NULL,
                    sdk_session_id TEXT,
                    agent_kind TEXT NOT NULL,
                    parent_session_id TEXT,
                    created_at INTEGER NOT NULL
                 );
                 CREATE TABLE messages (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    role TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    agent_meta TEXT,
                    agent_kind TEXT
                 );",
            )
            .unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_cindy_turn(
        source: &Connection,
        message_id: &str,
        session_id: &str,
        created_at_ms: i64,
        agent_kind: &str,
        model: &str,
        input_tokens: i64,
        output_tokens: i64,
        cache_read_tokens: i64,
        cache_create_tokens: i64,
    ) {
        insert_cindy_turn_with_cost(
            source,
            message_id,
            session_id,
            created_at_ms,
            agent_kind,
            model,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_create_tokens,
            None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_cindy_turn_with_cost(
        source: &Connection,
        message_id: &str,
        session_id: &str,
        created_at_ms: i64,
        agent_kind: &str,
        model: &str,
        input_tokens: i64,
        output_tokens: i64,
        cache_read_tokens: i64,
        cache_create_tokens: i64,
        turn_cost_usd: Option<f64>,
    ) {
        source
            .execute(
                "INSERT OR IGNORE INTO sessions
                 (id, model, sdk_session_id, agent_kind, parent_session_id, created_at)
                 VALUES (?1, ?2, NULL, ?3, NULL, ?4)",
                rusqlite::params![session_id, model, agent_kind, created_at_ms - 1],
            )
            .unwrap();
        let mut meta = serde_json::json!({
            "turnCompleted": true,
            "turnUsageDetails": {
                "inputTokens": input_tokens,
                "outputTokens": output_tokens,
                "cacheReadTokens": cache_read_tokens,
                "cacheCreateTokens": cache_create_tokens,
                "totalTokens": input_tokens + output_tokens + cache_read_tokens + cache_create_tokens,
                "model": model,
                "turnDurationMs": 1234
            }
        });
        if let Some(cost) = turn_cost_usd {
            meta["turnCostUsd"] = serde_json::json!(cost);
            meta["turnCost"] = serde_json::json!({
                "amount": cost,
                "currency": "USD",
                "approximate": true,
                "kind": "value-estimate"
            });
        }
        let meta = meta.to_string();
        source
            .execute(
                "INSERT INTO messages
                 (id, session_id, role, created_at, agent_meta, agent_kind)
                 VALUES (?1, ?2, 'assistant', ?3, ?4, ?5)",
                rusqlite::params![message_id, session_id, created_at_ms, meta, agent_kind],
            )
            .unwrap();
    }

    #[test]
    fn maka_uses_cache_miss_as_fresh_input() {
        let record = parse_maka_attempt(
            "attempt-1",
            1_700_000_000_000,
            r#"{
                "sessionId":"session-1","modelId":"gpt-test","status":"completed",
                "usageBasis":"reported","inputTokens":1000,"outputTokens":50,
                "cacheReadInputTokens":700,"cacheMissInputTokens":200,
                "cacheWriteInputTokens":100,"costBasis":"priced","costUsd":0.25,
                "latencyMs":1200,"timeToFirstTokenMs":150
            }"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(record.input_tokens, 200);
        assert_eq!(record.cache_read_tokens, 700);
        assert_eq!(record.cache_creation_tokens, 100);
        assert_eq!(record.total_cost_usd, Some(0.25));
        assert_eq!(record.created_at, 1_700_000_000);
    }

    #[test]
    fn codepilot_prefers_usage_model_and_falls_back_to_session_model() {
        let explicit = parse_codepilot_message(
            "message-1",
            "session-1",
            "2026-08-03 05:34:41",
            r#"{"input_tokens":10,"output_tokens":2,"usage_model_id":"model-a"}"#,
            "session-model",
            "",
        )
        .unwrap()
        .unwrap();
        assert_eq!(explicit.model, "model-a");

        let fallback = parse_codepilot_message(
            "message-2",
            "session-1",
            "2026-08-03T05:34:41Z",
            r#"{"input_tokens":10,"output_tokens":2}"#,
            "session-model",
            "",
        )
        .unwrap()
        .unwrap();
        assert_eq!(fallback.model, "session-model");
    }

    #[test]
    fn cindy_approximate_usd_cost_is_preserved_for_estimated_display() {
        let usage = CindyDailyUsage {
            day: "2020-01-02".to_string(),
            agent_kind: "pi".to_string(),
            model: "gpt-test".to_string(),
            cost_usd: 0.1,
            cost_amount: 0.2,
            cost_currency: "USD".to_string(),
            cost_is_approximate: true,
            input_tokens: 10,
            output_tokens: 2,
            cache_read_tokens: 0,
            cache_create_tokens: 0,
        };
        assert!((cindy_cost_usd(&usage) - 0.3).abs() < f64::EPSILON * 2.0);
    }

    #[test]
    fn cindy_database_discovery_only_accepts_root_account_databases() {
        let dir = tempdir().unwrap();
        let account_db = dir.path().join("cindy-user-a.db");
        let second_account_db = dir.path().join("cindy-user-b.db");
        fs::write(&account_db, []).unwrap();
        fs::write(&second_account_db, []).unwrap();
        fs::write(dir.path().join("cindy-user-a.db-wal"), []).unwrap();
        fs::write(
            dir.path().join("cindy-user-a.db.migration-runtime.json"),
            [],
        )
        .unwrap();
        fs::write(dir.path().join("other.db"), []).unwrap();
        let backup_dir = dir.path().join("config-backups/snapshot");
        fs::create_dir_all(&backup_dir).unwrap();
        fs::write(backup_dir.join("cindy-backup.db"), []).unwrap();

        let databases = collect_cindy_databases(dir.path()).unwrap();
        assert_eq!(databases, vec![account_db, second_account_db]);
    }

    #[test]
    fn cindy_discovery_prefers_canonical_copy_for_the_same_account() {
        let root = tempdir().unwrap();
        let canonical = root.path().join("CindyGlobal");
        let legacy = root.path().join("Cindy");
        fs::create_dir_all(&canonical).unwrap();
        fs::create_dir_all(&legacy).unwrap();
        let canonical_account = canonical.join("cindy-user-a.db");
        let legacy_account = legacy.join("cindy-user-a.db");
        let legacy_only = legacy.join("cindy-user-b.db");
        fs::write(&canonical_account, []).unwrap();
        fs::write(&legacy_account, []).unwrap();
        fs::write(&legacy_only, []).unwrap();

        let databases = discover_cindy_databases(&[canonical, legacy]).unwrap();

        assert_eq!(databases, vec![canonical_account, legacy_only]);
    }

    #[test]
    fn cindy_sync_prefers_turn_details_and_writes_only_daily_residual() {
        let dir = tempdir().unwrap();
        let source_path = dir.path().join("cindy-user-a.db");
        let source = Connection::open(&source_path).unwrap();
        create_cindy_usage_table(&source);
        create_cindy_turn_tables(&source);
        let now = Local::now();
        let day = now.format("%Y-%m-%d").to_string();
        source
            .execute(
                "INSERT INTO daily_model_usage VALUES (
                    ?1, 'pi', 'gpt-test#billing=api', 0, 0, 'USD', 0,
                    100, 20, 70, 10, ?2
                 )",
                rusqlite::params![day, now.timestamp_millis()],
            )
            .unwrap();
        insert_cindy_turn(
            &source,
            "message-1",
            "session-1",
            now.timestamp_millis(),
            "pi",
            "gpt-test",
            60,
            10,
            40,
            5,
        );
        drop(source);

        let db = Database::memory().unwrap();
        let result = sync_cindy_usage_from_paths(&db, std::slice::from_ref(&source_path)).unwrap();
        assert_eq!(result.imported, 2);

        let conn = db.conn.lock().unwrap();
        let detail: (String, i64, i64, i64, i64, i64, i64, Option<i64>) = conn
            .query_row(
                "SELECT model, input_tokens, output_tokens, cache_read_tokens,
                        cache_creation_tokens, input_token_semantics,
                        latency_ms, duration_ms
                 FROM proxy_request_logs
                 WHERE app_type = 'cindy' AND data_source = 'cindy_turn'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            detail,
            (
                "gpt-test#billing=api".to_string(),
                60,
                10,
                40,
                5,
                2,
                0,
                Some(1234),
            )
        );
        let residual: (i64, i64, i64, i64, i64) = conn
            .query_row(
                "SELECT input_tokens, output_tokens, cache_read_tokens,
                        cache_creation_tokens, input_token_semantics
                 FROM usage_daily_rollups WHERE app_type = 'cindy'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(residual, (40, 10, 30, 5, 2));
        let totals: (i64, i64, i64, i64) = conn
            .query_row(
                "SELECT SUM(input_tokens), SUM(output_tokens),
                        SUM(cache_read_tokens), SUM(cache_creation_tokens)
                 FROM (
                    SELECT input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens
                    FROM proxy_request_logs WHERE app_type = 'cindy'
                    UNION ALL
                    SELECT input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens
                    FROM usage_daily_rollups WHERE app_type = 'cindy'
                 )",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(totals, (100, 20, 70, 10));
    }

    #[test]
    fn cindy_sync_omits_residual_when_turn_fully_covers_daily_tokens() {
        let dir = tempdir().unwrap();
        let source_path = dir.path().join("cindy-user-a.db");
        let source = Connection::open(&source_path).unwrap();
        create_cindy_usage_table(&source);
        create_cindy_turn_tables(&source);
        let now = Local::now();
        let day = now.format("%Y-%m-%d").to_string();
        source
            .execute(
                "INSERT INTO daily_model_usage VALUES (
                    ?1, 'codex', 'gpt-test', 0, 0, 'USD', 0,
                    60, 10, 40, 5, ?2
                 )",
                rusqlite::params![day, now.timestamp_millis()],
            )
            .unwrap();
        insert_cindy_turn(
            &source,
            "message-1",
            "session-1",
            now.timestamp_millis(),
            "codex",
            "gpt-test",
            60,
            10,
            40,
            5,
        );
        drop(source);

        let db = Database::memory().unwrap();
        let result = sync_cindy_usage_from_paths(&db, &[source_path]).unwrap();
        assert_eq!(result.imported, 1);
        let conn = db.conn.lock().unwrap();
        let counts: (i64, i64) = conn
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM proxy_request_logs WHERE app_type = 'cindy'),
                    (SELECT COUNT(*) FROM usage_daily_rollups WHERE app_type = 'cindy')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(counts, (1, 0));
    }

    #[test]
    fn cindy_sync_falls_back_to_daily_when_turn_tokens_exceed_daily() {
        let dir = tempdir().unwrap();
        let source_path = dir.path().join("cindy-user-a.db");
        let source = Connection::open(&source_path).unwrap();
        create_cindy_usage_table(&source);
        create_cindy_turn_tables(&source);
        let now = Local::now();
        let day = now.format("%Y-%m-%d").to_string();
        source
            .execute(
                "INSERT INTO daily_model_usage VALUES (
                    ?1, 'pi', 'gpt-test', 0, 0, 'USD', 0,
                    50, 10, 20, 5, ?2
                 )",
                rusqlite::params![day, now.timestamp_millis()],
            )
            .unwrap();
        insert_cindy_turn(
            &source,
            "message-1",
            "session-1",
            now.timestamp_millis(),
            "pi",
            "gpt-test",
            60,
            10,
            20,
            5,
        );
        drop(source);

        let db = Database::memory().unwrap();
        sync_cindy_usage_from_paths(&db, &[source_path]).unwrap();
        let conn = db.conn.lock().unwrap();
        let detail_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM proxy_request_logs WHERE app_type = 'cindy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let fallback: (i64, i64, i64, i64) = conn
            .query_row(
                "SELECT input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens
                 FROM usage_daily_rollups WHERE app_type = 'cindy'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(detail_count, 0);
        assert_eq!(fallback, (50, 10, 20, 5));
    }

    #[test]
    fn cindy_sync_falls_back_to_daily_when_turn_has_no_matching_daily_key() {
        let dir = tempdir().unwrap();
        let source_path = dir.path().join("cindy-user-a.db");
        let source = Connection::open(&source_path).unwrap();
        create_cindy_usage_table(&source);
        create_cindy_turn_tables(&source);
        let now = Local::now();
        let day = now.format("%Y-%m-%d").to_string();
        source
            .execute(
                "INSERT INTO daily_model_usage VALUES (
                    ?1, 'pi', 'daily-model', 0, 0, 'USD', 0,
                    50, 10, 20, 5, ?2
                 )",
                rusqlite::params![day, now.timestamp_millis()],
            )
            .unwrap();
        insert_cindy_turn(
            &source,
            "message-1",
            "session-1",
            now.timestamp_millis(),
            "pi",
            "turn-only-model",
            20,
            5,
            10,
            2,
        );
        drop(source);

        let db = Database::memory().unwrap();
        sync_cindy_usage_from_paths(&db, &[source_path]).unwrap();
        let conn = db.conn.lock().unwrap();
        let detail_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM proxy_request_logs WHERE app_type = 'cindy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let fallback: (String, i64, i64, i64, i64) = conn
            .query_row(
                "SELECT model, input_tokens, output_tokens,
                        cache_read_tokens, cache_creation_tokens
                 FROM usage_daily_rollups WHERE app_type = 'cindy'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(detail_count, 0);
        assert_eq!(fallback, ("daily-model".to_string(), 50, 10, 20, 5));
    }

    #[test]
    fn cindy_upstream_dedup_requires_model_and_all_token_buckets() {
        let db = Database::memory().unwrap();
        let conn = db.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO proxy_request_logs (
                request_id, provider_id, app_type, model,
                input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                total_cost_usd, latency_ms, status_code, session_id, created_at
             ) VALUES (
                'pi-upstream', 'pi-provider', 'pi', 'gpt-test',
                10, 2, 3, 4, '0', 0, 200, 'sdk-session', 1700000000
             )",
            [],
        )
        .unwrap();
        let mut record = DesktopUsageRecord {
            request_id: "cindy-turn".to_string(),
            app_type: CINDY_APP_TYPE,
            data_source: CINDY_DETAIL_DATA_SOURCE,
            provider_id: CINDY_PROVIDER_ID,
            model: "gpt-test".to_string(),
            session_id: Some("cindy-session".to_string()),
            input_tokens: 11,
            output_tokens: 2,
            cache_read_tokens: 3,
            cache_creation_tokens: 4,
            total_cost_usd: Some(0.0),
            latency_ms: 0,
            duration_ms: None,
            first_token_ms: None,
            status_code: 200,
            error_message: None,
            created_at: 1700000000,
            upstream_dedup: Some(UpstreamDedup {
                app_type: "pi",
                session_id: "sdk-session".to_string(),
            }),
        };

        assert!(!has_upstream_duplicate(&conn, &record).unwrap());
        record.input_tokens = 10;
        record.model = "different-model".to_string();
        assert!(!has_upstream_duplicate(&conn, &record).unwrap());
        record.model = "gpt-test#billing=subscription".to_string();
        assert!(has_upstream_duplicate(&conn, &record).unwrap());
    }

    #[test]
    fn forced_cindy_reconcile_keeps_pi_attribution_without_global_double_count() {
        let dir = tempdir().unwrap();
        let source_path = dir.path().join("cindy-user-a.db");
        let source = Connection::open(&source_path).unwrap();
        create_cindy_usage_table(&source);
        create_cindy_turn_tables(&source);
        let now = Local::now();
        let day = now.format("%Y-%m-%d").to_string();
        source
            .execute(
                "INSERT INTO daily_model_usage VALUES (
                    ?1, 'pi', 'gpt-test', 0, 0.25, 'USD', 0,
                    10, 2, 3, 4, ?2
                 )",
                rusqlite::params![day, now.timestamp_millis()],
            )
            .unwrap();
        insert_cindy_turn(
            &source,
            "message-1",
            "session-1",
            now.timestamp_millis(),
            "pi",
            "gpt-test",
            10,
            2,
            3,
            4,
        );
        source
            .execute(
                "UPDATE sessions SET sdk_session_id = 'sdk-session' WHERE id = 'session-1'",
                [],
            )
            .unwrap();
        drop(source);

        let db = Database::memory().unwrap();
        sync_cindy_usage_from_paths(&db, std::slice::from_ref(&source_path)).unwrap();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO proxy_request_logs (
                    request_id, provider_id, app_type, model,
                    input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                    total_cost_usd, latency_ms, status_code, session_id, created_at
                 ) VALUES (
                    'pi-upstream', 'pi-provider', 'pi', 'gpt-test',
                    10, 2, 3, 4, '0.2', 0, 200, 'sdk-session', ?1
                 )",
                [now.timestamp()],
            )
            .unwrap();
        }

        let unchanged =
            sync_cindy_usage_from_paths(&db, std::slice::from_ref(&source_path)).unwrap();
        assert_eq!(unchanged.imported, 0);
        let reconciled =
            sync_cindy_usage_from_paths_with_force(&db, std::slice::from_ref(&source_path), true)
                .unwrap();
        assert_eq!(reconciled.imported, 2);
        assert!(reconciled.data_changed);
        let conn = db.conn.lock().unwrap();
        let cindy_rows: i64 = conn
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM proxy_request_logs WHERE app_type = 'cindy') +
                    (SELECT COUNT(*) FROM usage_daily_rollups WHERE app_type = 'cindy')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cindy_rows, 2);
        drop(conn);
        let cindy_usage = db
            .get_usage_summary(None, None, Some("cindy"), None, None)
            .unwrap();
        assert_eq!(cindy_usage.total_requests, 1);
        assert_eq!(cindy_usage.total_input_tokens, 10);
        assert_eq!(cindy_usage.total_cost, "0.250000");
        let all_usage = db.get_usage_summary(None, None, None, None, None).unwrap();
        assert_eq!(all_usage.total_input_tokens, 10);
        assert_eq!(all_usage.total_cost, "0.250000");
    }

    #[test]
    fn cindy_sync_preserves_daily_model_variants_when_detail_mapping_is_ambiguous() {
        let dir = tempdir().unwrap();
        let source_path = dir.path().join("cindy-user-a.db");
        let source = Connection::open(&source_path).unwrap();
        create_cindy_usage_table(&source);
        create_cindy_turn_tables(&source);
        let now = Local::now();
        let day = now.format("%Y-%m-%d").to_string();
        for (model, input) in [
            ("gpt-test#billing=api", 40i64),
            ("gpt-test#billing=subscription", 60i64),
        ] {
            source
                .execute(
                    "INSERT INTO daily_model_usage VALUES (
                        ?1, 'codex', ?2, 0, 0, 'USD', 0,
                        ?3, 10, 20, 5, ?4
                     )",
                    rusqlite::params![day, model, input, now.timestamp_millis()],
                )
                .unwrap();
        }
        insert_cindy_turn(
            &source,
            "message-1",
            "session-1",
            now.timestamp_millis(),
            "codex",
            "gpt-test",
            100,
            20,
            40,
            10,
        );
        drop(source);

        let db = Database::memory().unwrap();
        sync_cindy_usage_from_paths(&db, &[source_path]).unwrap();
        let conn = db.conn.lock().unwrap();
        let detail_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM proxy_request_logs WHERE app_type = 'cindy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let rollup_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_daily_rollups WHERE app_type = 'cindy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(detail_count, 0);
        assert_eq!(rollup_count, 2);
    }

    #[test]
    fn cindy_sync_maps_billing_variants_from_turn_cost_metadata() {
        let dir = tempdir().unwrap();
        let source_path = dir.path().join("cindy-user-a.db");
        let source = Connection::open(&source_path).unwrap();
        create_cindy_usage_table(&source);
        create_cindy_turn_tables(&source);
        let now = Local::now();
        let day = now.format("%Y-%m-%d").to_string();
        for (model, cost, input, output, cache_read, cache_create) in [
            ("gpt-test#billing=api", 0.05, 10i64, 2i64, 3i64, 1i64),
            (
                "gpt-test#billing=subscription",
                0.75,
                20i64,
                4i64,
                6i64,
                2i64,
            ),
        ] {
            source
                .execute(
                    "INSERT INTO daily_model_usage VALUES (
                        ?1, 'codex', ?2, 0, ?3, 'USD', 0,
                        ?4, ?5, ?6, ?7, ?8
                     )",
                    rusqlite::params![
                        day,
                        model,
                        cost,
                        input,
                        output,
                        cache_read,
                        cache_create,
                        now.timestamp_millis()
                    ],
                )
                .unwrap();
        }
        insert_cindy_turn(
            &source,
            "api-message",
            "api-session",
            now.timestamp_millis(),
            "codex",
            "gpt-test",
            10,
            2,
            3,
            1,
        );
        insert_cindy_turn_with_cost(
            &source,
            "subscription-message",
            "subscription-session",
            now.timestamp_millis() + 1_000,
            "codex",
            "gpt-test",
            20,
            4,
            6,
            2,
            Some(0.75),
        );
        drop(source);

        let db = Database::memory().unwrap();
        let result = sync_cindy_usage_from_paths(&db, &[source_path]).unwrap();
        assert_eq!(result.imported, 4);
        {
            let conn = db.conn.lock().unwrap();
            let mut statement = conn
                .prepare(
                    "SELECT model, COUNT(*)
                     FROM proxy_request_logs
                     WHERE app_type = 'cindy' AND data_source = 'cindy_turn'
                     GROUP BY model ORDER BY model",
                )
                .unwrap();
            let assignments = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(
                assignments,
                vec![
                    ("gpt-test#billing=api".to_string(), 1),
                    ("gpt-test#billing=subscription".to_string(), 1),
                ]
            );
        }

        let provider = db
            .get_provider_stats(None, None, Some("cindy"), None, None)
            .unwrap()
            .into_iter()
            .find(|item| item.provider_id == "_cindy_codex")
            .unwrap();
        assert_eq!(provider.request_count, 2);
        assert_eq!(provider.total_tokens, 48);
        assert_eq!(provider.total_cost, "0.800000");
        assert!(provider.cost_is_approximate);
    }

    #[test]
    fn cindy_claude_duplicate_is_visible_in_cindy_but_not_global_totals() {
        let dir = tempdir().unwrap();
        let source_path = dir.path().join("cindy-user-a.db");
        let source = Connection::open(&source_path).unwrap();
        create_cindy_usage_table(&source);
        create_cindy_turn_tables(&source);
        let now = Local::now();
        let day = now.format("%Y-%m-%d").to_string();
        source
            .execute(
                "INSERT INTO daily_model_usage VALUES (
                    ?1, 'claude-code', 'claude-opus-4-1[1m]', 0, 0.57319375, 'USD', 0,
                    100, 20, 30, 10, ?2
                 )",
                rusqlite::params![day, now.timestamp_millis()],
            )
            .unwrap();
        insert_cindy_turn(
            &source,
            "claude-message",
            "claude-session",
            now.timestamp_millis(),
            "cc",
            "claude-opus-4-1",
            100,
            20,
            30,
            10,
        );
        source
            .execute(
                "UPDATE sessions SET sdk_session_id = 'shared-sdk-session'
                 WHERE id = 'claude-session'",
                [],
            )
            .unwrap();
        drop(source);

        let db = Database::memory().unwrap();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO proxy_request_logs (
                    request_id, provider_id, app_type, model,
                    input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                    total_cost_usd, latency_ms, status_code, session_id, created_at, data_source
                 ) VALUES (
                    'claude-upstream', 'claude-provider', 'claude', 'claude-opus-4-1[1m]',
                    100, 20, 30, 10, '0.57319375', 0, 200,
                    'shared-sdk-session', ?1, 'session_log'
                 )",
                [now.timestamp()],
            )
            .unwrap();
        }

        let result = sync_cindy_usage_from_paths(&db, &[source_path]).unwrap();
        assert_eq!(result.imported, 1);
        {
            let conn = db.conn.lock().unwrap();
            let mirror: (String, String, String) = conn
                .query_row(
                    "SELECT data_source, request_model, total_cost_usd
                     FROM proxy_request_logs
                     WHERE app_type = 'cindy'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!(mirror.0, CINDY_MIRROR_DATA_SOURCE);
            assert_eq!(mirror.1, CINDY_MIRROR_REQUEST_MODEL);
            assert_eq!(mirror.2, "0.57319375");
        }

        let cindy = db
            .get_usage_summary(None, None, Some("cindy"), None, None)
            .unwrap();
        assert_eq!(cindy.total_requests, 1);
        assert_eq!(cindy.real_total_tokens, 160);
        assert_eq!(cindy.total_cost, "0.573194");

        let all = db.get_usage_summary(None, None, None, None, None).unwrap();
        assert_eq!(all.total_requests, 1);
        assert_eq!(all.real_total_tokens, 160);
        assert_eq!(all.total_cost, "0.573194");

        let archived_at = (Local::now() - chrono::Duration::days(40)).timestamp();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "UPDATE proxy_request_logs SET created_at = ?1",
                [archived_at],
            )
            .unwrap();
        }
        assert_eq!(db.rollup_and_prune(30).unwrap(), 2);
        let cindy_after_rollup = db
            .get_usage_summary(None, None, Some("cindy"), None, None)
            .unwrap();
        assert_eq!(cindy_after_rollup.total_requests, 1);
        assert_eq!(cindy_after_rollup.real_total_tokens, 160);
        assert_eq!(cindy_after_rollup.total_cost, "0.573194");
        let all_after_rollup = db.get_usage_summary(None, None, None, None, None).unwrap();
        assert_eq!(all_after_rollup.total_requests, 1);
        assert_eq!(all_after_rollup.real_total_tokens, 160);
        assert_eq!(all_after_rollup.total_cost, "0.573194");
    }

    #[test]
    fn cindy_sync_ignores_historical_turns_copied_into_fork_sessions() {
        let dir = tempdir().unwrap();
        let source_path = dir.path().join("cindy-user-a.db");
        let source = Connection::open(&source_path).unwrap();
        create_cindy_usage_table(&source);
        create_cindy_turn_tables(&source);
        let now = Local::now();
        let day = now.format("%Y-%m-%d").to_string();
        source
            .execute(
                "INSERT INTO daily_model_usage VALUES (
                    ?1, 'codex', 'gpt-test', 0, 0, 'USD', 0,
                    60, 10, 40, 5, ?2
                 )",
                rusqlite::params![day, now.timestamp_millis()],
            )
            .unwrap();
        insert_cindy_turn(
            &source,
            "copied-message",
            "fork-session",
            now.timestamp_millis() - 1_000,
            "codex",
            "gpt-test",
            60,
            10,
            40,
            5,
        );
        source
            .execute(
                "UPDATE sessions
                 SET parent_session_id = 'parent-session', created_at = ?1
                 WHERE id = 'fork-session'",
                [now.timestamp_millis()],
            )
            .unwrap();
        drop(source);

        let db = Database::memory().unwrap();
        sync_cindy_usage_from_paths(&db, &[source_path]).unwrap();
        let conn = db.conn.lock().unwrap();
        let counts: (i64, i64) = conn
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM proxy_request_logs WHERE app_type = 'cindy'),
                    (SELECT COUNT(*) FROM usage_daily_rollups WHERE app_type = 'cindy')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(counts, (0, 1));
    }

    #[test]
    fn cindy_v5_sync_rebuilds_when_only_v4_markers_exist() {
        let dir = tempdir().unwrap();
        let source_path = dir.path().join("cindy-user-a.db");
        let source = Connection::open(&source_path).unwrap();
        create_cindy_usage_table(&source);
        source
            .execute(
                "INSERT INTO daily_model_usage VALUES (
                    '2020-01-02', 'pi', 'gpt-test', 0, 0, 'USD', 0,
                    10, 2, 0, 0, 1577923200000
                 )",
                [],
            )
            .unwrap();
        drop(source);

        let db = Database::memory().unwrap();
        let modified = source_modified_nanos(&source_path).unwrap();
        let path = source_path.to_string_lossy();
        let v4_key = format!("desktop:cindy:v4:{}", cindy_short_hash(&[path.as_ref()]));
        update_sync_state(&db, &v4_key, modified, 0).unwrap();
        update_sync_state(
            &db,
            "desktop:cindy:v4:source-set",
            cindy_source_set_marker(std::slice::from_ref(&source_path)),
            0,
        )
        .unwrap();

        let result = sync_cindy_usage_from_paths(&db, &[source_path]).unwrap();
        assert_eq!(result.imported, 1);
        let conn = db.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_daily_rollups WHERE app_type = 'cindy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn cindy_sync_imports_daily_model_usage_and_replaces_changed_totals() {
        let dir = tempdir().unwrap();
        let source_path = dir.path().join("cindy-user-a.db");
        let source = Connection::open(&source_path).unwrap();
        create_cindy_usage_table(&source);
        let today = Local::now().format("%Y-%m-%d").to_string();
        source
            .execute(
                "INSERT INTO daily_model_usage VALUES (
                    ?1, 'pi', 'gpt-test', 0.125, 0.125, 'USD', 0,
                    100, 20, 70, 10, 1776816000000
                 )",
                [today.clone()],
            )
            .unwrap();
        source
            .execute(
                "INSERT INTO daily_model_usage VALUES (
                    ?1, 'claude-code', 'claude-shared', 1, 1, 'USD', 0,
                    999, 99, 0, 0, 1776816000000
                 )",
                [today],
            )
            .unwrap();
        drop(source);

        let db = Database::memory().unwrap();
        let first = sync_cindy_usage_from_paths(&db, std::slice::from_ref(&source_path)).unwrap();
        assert_eq!(first.imported, 2);
        assert_eq!(first.files_scanned, 1);
        let second = sync_cindy_usage_from_paths(&db, std::slice::from_ref(&source_path)).unwrap();
        assert_eq!(second.imported, 0);

        {
            let conn = db.conn.lock().unwrap();
            let row: (String, String, String, i64, i64, i64, String, i64) = conn
                .query_row(
                    "SELECT app_type, provider_id, model, input_tokens,
                            cache_read_tokens, cache_creation_tokens, total_cost_usd, request_count
                     FROM usage_daily_rollups
                     WHERE app_type = 'cindy' AND provider_id = '_cindy_pi'",
                    [],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                        ))
                    },
                )
                .unwrap();
            assert_eq!(row.0, "cindy");
            assert_eq!(row.1, "_cindy_pi");
            assert_eq!(row.2, "gpt-test");
            assert_eq!(row.3, 100);
            assert_eq!(row.4, 70);
            assert_eq!(row.5, 10);
            assert_eq!(row.6, "0.25");
            assert_eq!(row.7, 0);
        }

        let source = Connection::open(&source_path).unwrap();
        source
            .execute(
                "UPDATE daily_model_usage
                 SET input_tokens = 140, output_tokens = 25, cost_usd = 0.2,
                     cost_amount = 0.2, updated_at = updated_at + 1",
                [],
            )
            .unwrap();
        drop(source);
        update_sync_state(&db, &cindy_sync_key(&source_path), 0, 0).unwrap();

        let changed = sync_cindy_usage_from_paths(&db, std::slice::from_ref(&source_path)).unwrap();
        assert_eq!(changed.imported, 2);
        let conn = db.conn.lock().unwrap();
        let row: (i64, i64, String, i64) = conn
            .query_row(
                "SELECT input_tokens, output_tokens, total_cost_usd, COUNT(*)
                 FROM usage_daily_rollups
                 WHERE app_type = 'cindy' AND provider_id = '_cindy_pi'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(row, (140, 25, "0.4".to_string(), 1));
        drop(conn);

        let cleared = sync_cindy_usage_from_paths(&db, &[]).unwrap();
        assert_eq!(cleared.imported, 0);
        assert!(cleared.data_changed);
        let conn = db.conn.lock().unwrap();
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_daily_rollups WHERE app_type = 'cindy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn cindy_sync_keeps_completed_days_in_zero_request_rollups() {
        let dir = tempdir().unwrap();
        let source_path = dir.path().join("cindy-user-a.db");
        let source = Connection::open(&source_path).unwrap();
        create_cindy_usage_table(&source);
        create_cindy_turn_tables(&source);
        source
            .execute(
                "INSERT INTO daily_model_usage VALUES (
                    '2020-01-02', 'codex', 'gpt-history', 0, 0.3, 'USD', 0,
                    5000000000, 30, 20, 10, 1577923200000
                 )",
                [],
            )
            .unwrap();
        insert_cindy_turn(
            &source,
            "historical-message",
            "historical-session",
            1_577_923_200_000,
            "codex",
            "gpt-history",
            100,
            30,
            20,
            10,
        );
        drop(source);

        let db = Database::memory().unwrap();
        let result = sync_cindy_usage_from_paths(&db, &[source_path]).unwrap();
        assert_eq!(result.imported, 1);
        let conn = db.conn.lock().unwrap();
        let row: (i64, i64, i64, String) = conn
            .query_row(
                "SELECT request_count, input_tokens, cache_read_tokens, total_cost_usd
                 FROM usage_daily_rollups
                 WHERE date = '2020-01-02' AND app_type = 'cindy'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(row, (0, 5_000_000_000, 20, "0.3".to_string()));
        let detail_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM proxy_request_logs WHERE app_type = 'cindy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(detail_count, 0);
    }

    #[test]
    fn cindy_resync_preserves_archived_request_and_session_metrics_without_fake_latency() {
        let dir = tempdir().unwrap();
        let source_path = dir.path().join("cindy-user-a.db");
        let source = Connection::open(&source_path).unwrap();
        create_cindy_usage_table(&source);
        create_cindy_turn_tables(&source);
        let now = Local::now();
        let current_day = now.format("%Y-%m-%d").to_string();
        source
            .execute(
                "INSERT INTO daily_model_usage VALUES (
                    ?1, 'pi', 'gpt-test', 0, 0, 'USD', 0,
                    100, 20, 30, 4, ?2
                 )",
                rusqlite::params![current_day, now.timestamp_millis()],
            )
            .unwrap();
        insert_cindy_turn(
            &source,
            "message-1",
            "session-1",
            now.timestamp_millis(),
            "pi",
            "gpt-test",
            100,
            20,
            30,
            4,
        );
        drop(source);

        let db = Database::memory().unwrap();
        sync_cindy_usage_from_paths(&db, std::slice::from_ref(&source_path)).unwrap();
        let archived_at = Local
            .with_ymd_and_hms(2020, 1, 2, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "UPDATE proxy_request_logs
                 SET created_at = ?1
                 WHERE app_type = 'cindy' AND data_source = 'cindy_turn'",
                [archived_at],
            )
            .unwrap();
        }
        assert_eq!(db.rollup_and_prune(30).unwrap(), 1);
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "UPDATE usage_daily_rollups
                 SET avg_latency_ms = 1234
                 WHERE date = '2020-01-02' AND app_type = 'cindy'",
                [],
            )
            .unwrap();
        }

        let source = Connection::open(&source_path).unwrap();
        source
            .execute(
                "UPDATE daily_model_usage
                 SET day = '2020-01-02', updated_at = updated_at + 1",
                [],
            )
            .unwrap();
        drop(source);
        update_sync_state(&db, &cindy_sync_key(&source_path), 0, 0).unwrap();

        sync_cindy_usage_from_paths(&db, &[source_path]).unwrap();
        let conn = db.conn.lock().unwrap();
        let rollup: (i64, i64, i64, i64, i64, i64, i64) = conn
            .query_row(
                "SELECT request_count, success_count, avg_latency_ms,
                        input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens
                 FROM usage_daily_rollups
                 WHERE date = '2020-01-02' AND app_type = 'cindy'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(rollup, (1, 1, 0, 100, 20, 30, 4));
        let activity: (i64, i64, i64, i64) = conn
            .query_row(
                "SELECT request_count, session_count, input_tokens, output_tokens
                 FROM usage_daily_activity_rollups
                 WHERE date = '2020-01-02' AND app_type = 'cindy'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(activity, (1, 1, 100, 20));
        let archived_sessions: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_daily_activity_session_rollups
                 WHERE date = '2020-01-02' AND app_type = 'cindy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(archived_sessions, 1);
    }

    #[test]
    fn cindy_sync_skips_incompatible_accounts_without_blocking_valid_usage() {
        let dir = tempdir().unwrap();
        let valid_path = dir.path().join("cindy-valid.db");
        let valid = Connection::open(&valid_path).unwrap();
        create_cindy_usage_table(&valid);
        valid
            .execute(
                "INSERT INTO daily_model_usage VALUES (
                    '2020-01-02', 'pi', 'valid-model', 0, 0, NULL, 0,
                    10, 2, 0, 0, 1577923200000
                 )",
                [],
            )
            .unwrap();
        drop(valid);
        let legacy_path = dir.path().join("cindy-legacy.db");
        Connection::open(&legacy_path).unwrap();

        let db = Database::memory().unwrap();
        let result = sync_cindy_usage_from_paths(&db, &[legacy_path, valid_path]).unwrap();
        assert_eq!(result.imported, 1);
        assert_eq!(result.errors.len(), 1);
        let conn = db.conn.lock().unwrap();
        let imported: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_daily_rollups WHERE app_type = 'cindy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(imported, 1);
    }

    #[test]
    fn maka_sync_is_incremental_and_idempotent() {
        let dir = tempdir().unwrap();
        let source_path = dir.path().join("runtime.sqlite");
        let source = Connection::open(&source_path).unwrap();
        source
            .execute_batch(
                "CREATE TABLE usage_model_call_attempts (
                    attempt_id TEXT PRIMARY KEY, completed_at INTEGER NOT NULL, record_json TEXT NOT NULL
                 );",
            )
            .unwrap();
        source
            .execute(
                "INSERT INTO usage_model_call_attempts VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    "attempt-1",
                    1_700_000_000_000i64,
                    r#"{"modelId":"gpt-test","status":"completed","usageBasis":"reported","inputTokens":30,"outputTokens":5,"cacheReadInputTokens":20,"cacheMissInputTokens":10,"cacheWriteInputTokens":0,"costBasis":"unpriced"}"#,
                ],
            )
            .unwrap();
        drop(source);

        let db = Database::memory().unwrap();
        let first = sync_maka_usage_from_path(&db, &source_path).unwrap();
        assert_eq!(first.imported, 1);
        let second = sync_maka_usage_from_path(&db, &source_path).unwrap();
        assert_eq!(second.imported, 0);

        let conn = db.conn.lock().unwrap();
        let row: (String, i64, i64, String) = conn
            .query_row(
                "SELECT app_type, input_tokens, cache_read_tokens, data_source
                 FROM proxy_request_logs WHERE request_id = 'maka_attempt:attempt-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            ("maka".to_string(), 10, 20, "maka_session".to_string())
        );
    }

    #[test]
    fn codepilot_sync_imports_completed_assistant_usage() {
        let dir = tempdir().unwrap();
        let source_path = dir.path().join("codepilot.db");
        let source = Connection::open(&source_path).unwrap();
        source
            .execute_batch(
                "CREATE TABLE chat_sessions (
                    id TEXT PRIMARY KEY, model TEXT NOT NULL, codex_thread_id TEXT NOT NULL
                 );
                 CREATE TABLE messages (
                    id TEXT PRIMARY KEY, session_id TEXT NOT NULL, role TEXT NOT NULL,
                    created_at TEXT NOT NULL, token_usage TEXT, stream_status TEXT NOT NULL
                 );
                 INSERT INTO chat_sessions VALUES ('session-1', 'model-fallback', '');",
            )
            .unwrap();
        source
            .execute(
                "INSERT INTO messages VALUES (?1, ?2, 'assistant', ?3, ?4, 'completed')",
                rusqlite::params![
                    "message-1",
                    "session-1",
                    "2026-08-03 05:34:41",
                    r#"{"input_tokens":11,"output_tokens":3,"cache_read_input_tokens":7,"cache_creation_input_tokens":2,"cost_usd":0.125}"#,
                ],
            )
            .unwrap();
        drop(source);

        let db = Database::memory().unwrap();
        let result = sync_codepilot_usage_from_path(&db, &source_path).unwrap();
        assert_eq!(result.imported, 1);
        let conn = db.conn.lock().unwrap();
        let row: (String, String, i64, String) = conn
            .query_row(
                "SELECT app_type, model, output_tokens, total_cost_usd
                 FROM proxy_request_logs WHERE request_id = 'codepilot_message:message-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(row.0, "codepilot");
        assert_eq!(row.1, "model-fallback");
        assert_eq!(row.2, 3);
        assert_eq!(row.3, "0.125");
    }

    #[test]
    fn codepilot_reconciles_a_later_codex_import_when_source_is_unchanged() {
        let dir = tempdir().unwrap();
        let source_path = dir.path().join("codepilot.db");
        let source = Connection::open(&source_path).unwrap();
        source
            .execute_batch(
                "CREATE TABLE chat_sessions (
                    id TEXT PRIMARY KEY, model TEXT NOT NULL, codex_thread_id TEXT NOT NULL
                 );
                 CREATE TABLE messages (
                    id TEXT PRIMARY KEY, session_id TEXT NOT NULL, role TEXT NOT NULL,
                    created_at TEXT NOT NULL, token_usage TEXT, stream_status TEXT NOT NULL
                 );
                 INSERT INTO chat_sessions VALUES ('session-1', 'gpt-test', 'thread-1');",
            )
            .unwrap();
        source
            .execute(
                "INSERT INTO messages VALUES (?1, ?2, 'assistant', ?3, ?4, 'completed')",
                rusqlite::params![
                    "message-1",
                    "session-1",
                    "2026-08-03 05:34:41",
                    r#"{"input_tokens":100,"output_tokens":20,"context_accounting":{"providerBackend":"codex_account"}}"#,
                ],
            )
            .unwrap();
        drop(source);

        let db = Database::memory().unwrap();
        let first = sync_codepilot_usage_from_path(&db, &source_path).unwrap();
        assert_eq!(first.imported, 1);
        let created_at = parse_codepilot_timestamp("2026-08-03 05:34:41").unwrap();
        let codex_record = DesktopUsageRecord {
            request_id: "codex-late".to_string(),
            app_type: "codex",
            data_source: "codex_session",
            provider_id: "_codex_session",
            model: "gpt-test".to_string(),
            session_id: Some("thread-1".to_string()),
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            total_cost_usd: None,
            latency_ms: 0,
            duration_ms: None,
            first_token_ms: None,
            status_code: 200,
            error_message: None,
            created_at,
            upstream_dedup: None,
        };
        assert!(insert_desktop_usage(&db, &codex_record).unwrap());

        let second = sync_codepilot_usage_from_path(&db, &source_path).unwrap();
        assert!(second.data_changed);
        let conn = db.conn.lock().unwrap();
        let codepilot_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM proxy_request_logs
                 WHERE request_id = 'codepilot_message:message-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(codepilot_count, 0);
        let total_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM proxy_request_logs
                 WHERE request_id IN ('codepilot_message:message-1', 'codex-late')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(total_count, 1);
    }

    #[test]
    fn deepseek_harness_sync_imports_final_assistant_usage_idempotently() {
        let dir = tempdir().unwrap();
        let session_dir = dir.path().join("session-1");
        fs::create_dir_all(&session_dir).unwrap();
        let source_path = session_dir.join("session.jsonl.zstd");
        let events = concat!(
            r#"{"type":"session","id":"session-1","createdAt":1700000000000}"#,
            "\n",
            r#"{"type":"request/context","seq":1,"time":1700000000100,"data":{"provider":"deepseek-official","model":"deepseek-v4-flash"}}"#,
            "\n",
            r#"{"type":"assistant/message","seq":2,"time":1700000001000,"data":{"turn":1,"step":1,"usage":{"inputTokens":10,"outputTokens":3,"cacheReadTokens":7},"message":{"id":"message-1","role":"assistant","content":[],"source":"agent"}}}"#,
            "\n",
        );
        let compressed = zstd::stream::encode_all(events.as_bytes(), 0).unwrap();
        fs::write(&source_path, compressed).unwrap();

        let db = Database::memory().unwrap();
        let first = parse_deepseek_harness_file(&db, &source_path).unwrap();
        assert_eq!(first.imported, 1);

        // Force a rescan to prove the stable session+seq request id remains idempotent.
        let sync_key = format!("desktop:deepseek-harness:{}", source_path.to_string_lossy());
        update_sync_state(&db, &sync_key, 0, 0).unwrap();
        let second = parse_deepseek_harness_file(&db, &source_path).unwrap();
        assert_eq!(second.imported, 0);

        let conn = db.conn.lock().unwrap();
        let row: (String, String, String, i64, i64, String) = conn
            .query_row(
                "SELECT app_type, provider_id, model, input_tokens, cache_read_tokens, data_source
                 FROM proxy_request_logs
                 WHERE request_id = 'deepseek_harness_message:session-1:2'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(row.0, "deepseek_harness");
        assert_eq!(row.1, "_deepseek_harness_session");
        assert_eq!(row.2, "deepseek-v4-flash");
        assert_eq!(row.3, 10);
        assert_eq!(row.4, 7);
        assert_eq!(row.5, "deepseek_harness_session");
    }

    #[test]
    fn deepseek_harness_directory_sync_discovers_nested_zstd_sessions() {
        let dir = tempdir().unwrap();
        let sessions_dir = dir.path().join("sessions");
        let session_dir = sessions_dir.join("workspace-a/session-a");
        fs::create_dir_all(&session_dir).unwrap();
        let source_path = session_dir.join("session.jsonl.zstd");
        let events = concat!(
            r#"{"type":"session","id":"session-a","createdAt":1700000000000}"#,
            "\n",
            r#"{"type":"request/context","seq":1,"time":1700000000100,"data":{"provider":"deepseek-official","model":"deepseek-v4-pro"}}"#,
            "\n",
            r#"{"type":"assistant/message","seq":2,"time":1700000001000,"data":{"usage":{"inputTokens":20,"outputTokens":4},"message":{"id":"message-a","role":"assistant","content":[],"source":"agent"}}}"#,
            "\n",
        );
        fs::write(
            &source_path,
            zstd::stream::encode_all(events.as_bytes(), 0).unwrap(),
        )
        .unwrap();

        let mut files = Vec::new();
        collect_deepseek_harness_session_files(&sessions_dir, &mut files).unwrap();
        assert_eq!(files, vec![source_path.clone()]);

        let db = Database::memory().unwrap();
        let result = parse_deepseek_harness_file(&db, &source_path).unwrap();
        assert_eq!(result.files_scanned, 1);
        assert_eq!(result.imported, 1);
    }

    #[test]
    fn codepilot_codex_backend_skips_matching_codex_rollout_usage() {
        let db = Database::memory().unwrap();
        let codex_record = DesktopUsageRecord {
            request_id: "codex-existing".to_string(),
            app_type: "codex",
            data_source: "codex_session",
            provider_id: "_codex_session",
            model: "gpt-test".to_string(),
            session_id: Some("thread-1".to_string()),
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            total_cost_usd: None,
            latency_ms: 0,
            duration_ms: None,
            first_token_ms: None,
            status_code: 200,
            error_message: None,
            created_at: 1_700_000_000,
            upstream_dedup: None,
        };
        assert!(insert_desktop_usage(&db, &codex_record).unwrap());

        let codepilot_record = DesktopUsageRecord {
            request_id: "codepilot-message".to_string(),
            app_type: CODEPILOT_APP_TYPE,
            data_source: CODEPILOT_DATA_SOURCE,
            provider_id: "_codepilot_session",
            model: "gpt-test".to_string(),
            session_id: Some("codepilot-session".to_string()),
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            total_cost_usd: None,
            latency_ms: 0,
            duration_ms: None,
            first_token_ms: None,
            status_code: 200,
            error_message: None,
            created_at: 1_700_000_030,
            upstream_dedup: Some(UpstreamDedup {
                app_type: "codex",
                session_id: "thread-1".to_string(),
            }),
        };
        assert!(!insert_desktop_usage(&db, &codepilot_record).unwrap());
    }
}
