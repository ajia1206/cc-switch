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
use crate::services::usage_stats::{find_model_pricing, should_skip_session_insert, DedupKey};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Deserialize;
use sha2::{Digest, Sha256};
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
const CINDY_DATA_SOURCE: &str = "cindy_daily";
const CINDY_PROVIDER_ID: &str = "_cindy_session";
const CINDY_SOURCE_SET_SYNC_KEY: &str = "desktop:cindy:source-set";

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

fn has_upstream_duplicate(
    conn: &rusqlite::Connection,
    record: &DesktopUsageRecord,
) -> Result<bool, AppError> {
    let Some(upstream) = &record.upstream_dedup else {
        return Ok(false);
    };
    conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM proxy_request_logs
            WHERE app_type = ?1
              AND session_id = ?2
              AND created_at BETWEEN ?3 AND ?4
              AND output_tokens = ?5
        )",
        rusqlite::params![
            upstream.app_type,
            upstream.session_id,
            record.created_at.saturating_sub(120),
            record.created_at.saturating_add(120),
            record.output_tokens,
        ],
        |row| row.get(0),
    )
    .map_err(|e| AppError::Database(format!("检查跨客户端重复用量失败: {e}")))
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

fn insert_desktop_usage(db: &Database, record: &DesktopUsageRecord) -> Result<bool, AppError> {
    let conn = lock_conn!(db.conn);
    if has_upstream_duplicate(&conn, record)? {
        return Ok(false);
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
    if should_skip_session_insert(&conn, &record.request_id, &dedup_key)? {
        return Ok(false);
    }

    let (input_cost, output_cost, cache_read_cost, cache_creation_cost, total_cost) =
        calculated_costs(&conn, record);
    let inserted = conn
        .execute(
            "INSERT OR IGNORE INTO proxy_request_logs (
                request_id, provider_id, app_type, model, request_model,
                input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                input_cost_usd, output_cost_usd, cache_read_cost_usd,
                cache_creation_cost_usd, total_cost_usd,
                latency_ms, first_token_ms, status_code, error_message, session_id,
                provider_type, is_streaming, cost_multiplier, created_at, data_source
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24
            )",
            rusqlite::params![
                record.request_id,
                record.provider_id,
                record.app_type,
                record.model,
                record.model,
                record.input_tokens,
                record.output_tokens,
                record.cache_read_tokens,
                record.cache_creation_tokens,
                input_cost,
                output_cost,
                cache_read_cost,
                cache_creation_cost,
                total_cost,
                record.latency_ms,
                record.first_token_ms,
                record.status_code,
                record.error_message,
                record.session_id,
                record.data_source,
                1i64,
                "1.0",
                record.created_at,
                record.data_source,
            ],
        )
        .map_err(|e| AppError::Database(format!("插入桌面 Agent 会话用量失败: {e}")))?;
    Ok(inserted > 0)
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
    format!("desktop:cindy:{}", cindy_short_hash(&[path.as_ref()]))
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

fn cindy_cost_usd(usage: &CindyDailyUsage) -> f64 {
    if usage.cost_is_approximate {
        // CC Switch's USD-only ledger has no approximation marker. Importing this
        // amount would present an estimate as exact spend, so keep only its tokens.
        return 0.0;
    }
    let legacy_usd = finite_non_negative(Some(usage.cost_usd)).unwrap_or(0.0);
    let current_amount = finite_non_negative(Some(usage.cost_amount)).unwrap_or(0.0);
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

fn read_cindy_daily_usage(source_path: &Path) -> Result<Option<Vec<CindyDailyUsage>>, AppError> {
    let source = open_source_db(source_path, "Cindy")?;
    let columns = {
        let mut statement = source
            .prepare("PRAGMA table_info(daily_model_usage)")
            .map_err(|e| AppError::Database(format!("检查 Cindy 用量表失败: {e}")))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|e| AppError::Database(format!("读取 Cindy 用量表结构失败: {e}")))?;
        let mut columns = std::collections::HashSet::new();
        for row in rows {
            columns.insert(
                row.map_err(|e| AppError::Database(format!("读取 Cindy 用量列失败: {e}")))?,
            );
        }
        columns
    };
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

fn replace_cindy_usage(
    db: &Database,
    sources: &[(PathBuf, Vec<CindyDailyUsage>)],
) -> Result<(u32, bool), AppError> {
    let mut conn = lock_conn!(db.conn);
    let previous_rows: i64 = conn
        .query_row(
            "SELECT
                (SELECT COUNT(*) FROM proxy_request_logs WHERE app_type = ?1 AND data_source = ?2) +
                (SELECT COUNT(*) FROM usage_daily_rollups WHERE app_type = ?1) +
                (SELECT COUNT(*) FROM usage_daily_activity_rollups WHERE app_type = ?1)",
            rusqlite::params![CINDY_APP_TYPE, CINDY_DATA_SOURCE],
            |row| row.get(0),
        )
        .map_err(|e| AppError::Database(format!("检查旧 Cindy 用量失败: {e}")))?;
    let transaction = conn
        .transaction()
        .map_err(|e| AppError::Database(format!("开始 Cindy 用量同步事务失败: {e}")))?;
    transaction
        .execute(
            "DELETE FROM proxy_request_logs WHERE app_type = ?1 AND data_source = ?2",
            rusqlite::params![CINDY_APP_TYPE, CINDY_DATA_SOURCE],
        )
        .map_err(|e| AppError::Database(format!("清理 Cindy 当日用量失败: {e}")))?;
    transaction
        .execute(
            "DELETE FROM usage_daily_rollups WHERE app_type = ?1",
            [CINDY_APP_TYPE],
        )
        .map_err(|e| AppError::Database(format!("清理 Cindy 历史用量失败: {e}")))?;
    transaction
        .execute(
            "DELETE FROM usage_daily_activity_rollups WHERE app_type = ?1",
            [CINDY_APP_TYPE],
        )
        .map_err(|e| AppError::Database(format!("清理 Cindy 活跃用量失败: {e}")))?;
    transaction
        .execute(
            "DELETE FROM usage_daily_activity_session_rollups WHERE app_type = ?1",
            [CINDY_APP_TYPE],
        )
        .map_err(|e| AppError::Database(format!("清理 Cindy 活跃会话失败: {e}")))?;

    let mut imported = 0u32;
    for (_, rows) in sources {
        for usage in rows {
            // Cindy's packaged Claude runtime writes the same transcripts under
            // ~/.claude that CC Switch already imports as app_type='claude'.
            // Re-importing its daily bucket here would double-count All totals.
            if matches!(usage.agent_kind.as_str(), "claude-code" | "cc") {
                continue;
            }
            NaiveDate::parse_from_str(&usage.day, "%Y-%m-%d").map_err(|e| {
                AppError::Config(format!("Cindy 用量日期格式无效 {}: {e}", usage.day))
            })?;
            let model = if usage.model.trim().is_empty() {
                "unknown".to_string()
            } else {
                usage.model.clone()
            };
            let input_tokens = cindy_token_i64(usage.input_tokens);
            let output_tokens = cindy_token_i64(usage.output_tokens);
            let cache_read_tokens = cindy_token_i64(usage.cache_read_tokens);
            let cache_create_tokens = cindy_token_i64(usage.cache_create_tokens);
            let total_cost = cindy_cost_usd(usage).to_string();

            transaction
                .execute(
                    "INSERT INTO usage_daily_rollups (
                        date, app_type, provider_id, model, request_model, pricing_model,
                        request_count, success_count, input_tokens, output_tokens,
                        cache_read_tokens, cache_creation_tokens, input_token_semantics,
                        total_cost_usd, avg_latency_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?4, ?4, 0, 0, ?5, ?6, ?7, ?8, 0, ?9, 0)
                     ON CONFLICT(date, app_type, provider_id, model, request_model, pricing_model)
                     DO UPDATE SET
                        input_tokens = input_tokens + excluded.input_tokens,
                        output_tokens = output_tokens + excluded.output_tokens,
                        cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
                        cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
                        total_cost_usd = CAST(
                            CAST(total_cost_usd AS REAL) + CAST(excluded.total_cost_usd AS REAL)
                            AS TEXT
                        )",
                    rusqlite::params![
                        usage.day,
                        CINDY_APP_TYPE,
                        cindy_provider_id(&usage.agent_kind),
                        model,
                        input_tokens,
                        output_tokens,
                        cache_read_tokens,
                        cache_create_tokens,
                        total_cost,
                    ],
                )
                .map_err(|e| AppError::Database(format!("写入 Cindy 日用量失败: {e}")))?;
            imported = imported.saturating_add(1);
        }
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
                    SUM(cache_creation_tokens), 0,
                    CAST(SUM(CAST(total_cost_usd AS REAL)) AS TEXT)
             FROM usage_daily_rollups
             WHERE app_type = ?1
             GROUP BY date, app_type",
            [CINDY_APP_TYPE],
        )
        .map_err(|e| AppError::Database(format!("汇总 Cindy 活跃用量失败: {e}")))?;
    transaction
        .commit()
        .map_err(|e| AppError::Database(format!("提交 Cindy 用量同步失败: {e}")))?;
    Ok((imported, previous_rows > 0 || imported > 0))
}

fn sync_cindy_usage_from_paths(
    db: &Database,
    source_paths: &[PathBuf],
) -> Result<SessionSyncResult, AppError> {
    let files_scanned = source_paths.len().min(u32::MAX as usize) as u32;
    let source_set_marker = cindy_source_set_marker(source_paths);
    let mut needs_refresh = get_sync_state(db, CINDY_SOURCE_SET_SYNC_KEY)?.0 != source_set_marker;
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

    let mut sources = Vec::with_capacity(source_paths.len());
    let mut transient_failure = modified_times.len() != source_paths.len();
    for (path, _) in &modified_times {
        match read_cindy_daily_usage(path) {
            Ok(Some(usage)) => sources.push((path.clone(), usage)),
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

    let (imported, data_changed) = replace_cindy_usage(db, &sources)?;
    result.imported = imported;
    result.data_changed = data_changed;
    for (path, modified) in modified_times {
        update_sync_state(db, &cindy_sync_key(&path), modified, 0)?;
    }
    update_sync_state(db, CINDY_SOURCE_SET_SYNC_KEY, source_set_marker, 0)?;

    if result.imported > 0 {
        log::info!(
            "[CINDY-SYNC] 同步完成: 导入 {} 条日模型聚合, 扫描 {} 个账号数据库",
            result.imported,
            source_paths.len()
        );
    }
    Ok(result)
}

pub fn sync_cindy_usage(db: &Database) -> Result<SessionSyncResult, AppError> {
    let databases = discover_cindy_databases(&cindy_user_data_dirs())?;
    sync_cindy_usage_from_paths(db, &databases)
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
    fn cindy_approximate_cost_is_not_presented_as_exact_usd() {
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
        assert_eq!(cindy_cost_usd(&usage), 0.0);
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
        assert_eq!(first.imported, 1);
        assert_eq!(first.files_scanned, 1);
        let second = sync_cindy_usage_from_paths(&db, std::slice::from_ref(&source_path)).unwrap();
        assert_eq!(second.imported, 0);

        {
            let conn = db.conn.lock().unwrap();
            let row: (String, String, String, i64, i64, i64, String, i64) = conn
                .query_row(
                    "SELECT app_type, provider_id, model, input_tokens,
                            cache_read_tokens, cache_creation_tokens, total_cost_usd, request_count
                     FROM usage_daily_rollups WHERE app_type = 'cindy'",
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
        assert_eq!(changed.imported, 1);
        let conn = db.conn.lock().unwrap();
        let row: (i64, i64, String, i64) = conn
            .query_row(
                "SELECT input_tokens, output_tokens, total_cost_usd, COUNT(*)
                 FROM usage_daily_rollups WHERE app_type = 'cindy'",
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
        source
            .execute(
                "INSERT INTO daily_model_usage VALUES (
                    '2020-01-02', 'codex', 'gpt-history', 0, 0.3, 'USD', 0,
                    5000000000, 30, 20, 10, 1577923200000
                 )",
                [],
            )
            .unwrap();
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
