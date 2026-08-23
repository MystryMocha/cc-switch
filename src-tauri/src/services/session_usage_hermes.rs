//! Hermes Agent session usage importer.
//!
//! Hermes stores cumulative per-route usage in `state.db`. This importer
//! keeps direct (non-proxy) Hermes usage visible in the shared dashboard.
//!
//! ## Data flow
//! ```text
//! ~/.hermes/state.db  (Windows: %LOCALAPPDATA%\hermes\state.db)
//!   → session_model_usage (preferred) or sessions (legacy)
//!   → per-key cumulative watermark
//!   → delta rows in proxy_request_logs
//! ```
//!
//! Hermes only persists running totals, not per-call events. Each dashboard
//! row is therefore a (session, model, billing route, task) bucket: the first
//! import writes the snapshot, later syncs write only the increase. Request
//! count is the number of those buckets / increments, not Hermes `api_call_count`.

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use crate::hermes_config::get_hermes_dir;
use crate::proxy::usage::calculator::CostCalculator;
use crate::proxy::usage::parser::TokenUsage;
use crate::services::session_usage::{
    get_sync_state, metadata_modified_nanos, update_sync_state_on_conn, SessionSyncResult,
};
use crate::services::sql_helpers::INPUT_TOKEN_SEMANTICS_FRESH;
use crate::services::usage_stats::find_model_pricing;
use rusqlite::types::ValueRef;
use rusqlite::OptionalExtension;
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::str::FromStr;
use std::time::SystemTime;

const APP_TYPE: &str = "hermes";
const DATA_SOURCE: &str = "hermes_session";
const PROVIDER_PLACEHOLDER: &str = "_hermes_session";
const UNKNOWN_MODEL: &str = "unknown";
const WATERMARK_PREFIX: &str = "hermes_usage:";

#[derive(Debug, Clone)]
struct HermesUsageRow {
    session_id: String,
    model: String,
    billing_provider: String,
    billing_base_url: String,
    billing_mode: String,
    task: String,
    input_tokens: u32,
    output_tokens: u32,
    cache_read_tokens: u32,
    cache_write_tokens: u32,
    estimated_cost: Decimal,
    actual_cost: Decimal,
    cost_included: bool,
    first_seen_secs: i64,
    last_seen_secs: i64,
}

#[derive(Debug, Clone, Copy, Default)]
struct HermesWatermark {
    last_seen_millis: i64,
    input_tokens: u32,
    output_tokens: u32,
    cache_read_tokens: u32,
    cache_write_tokens: u32,
}

/// Import Hermes usage from the standard `state.db` path.
pub fn sync_hermes_usage(db: &Database) -> Result<SessionSyncResult, AppError> {
    sync_hermes_db(db, &get_hermes_dir().join("state.db"))
}

fn sync_hermes_db(db: &Database, db_path: &Path) -> Result<SessionSyncResult, AppError> {
    if !db_path.exists() {
        return Ok(SessionSyncResult::default());
    }

    let db_path_str = db_path.to_string_lossy().to_string();
    let file_modified = hermes_db_modified_nanos(db_path)?;
    let (last_modified, _) = get_sync_state(db, &db_path_str)?;
    if file_modified > 0 && file_modified <= last_modified {
        return Ok(SessionSyncResult {
            files_scanned: 1,
            ..SessionSyncResult::default()
        });
    }

    let hermes_conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| AppError::Database(format!("无法打开 Hermes state.db: {e}")))?;

    let rows = query_usage_rows(&hermes_conn)?;
    drop(hermes_conn);

    let conn = lock_conn!(db.conn);
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| AppError::Database(format!("启动 Hermes 用量导入事务失败: {e}")))?;

    let mut result = SessionSyncResult {
        files_scanned: 1,
        ..SessionSyncResult::default()
    };
    let mut has_sync_errors = false;

    for row in &rows {
        match import_usage_row(&tx, row) {
            Ok(true) => result.imported = result.imported.saturating_add(1),
            Ok(false) => result.skipped = result.skipped.saturating_add(1),
            Err(e) => {
                let msg = format!("Hermes 用量导入失败 {}: {e}", row.session_id);
                log::warn!("[HERMES-SYNC] {msg}");
                result.errors.push(msg);
                result.skipped = result.skipped.saturating_add(1);
                has_sync_errors = true;
            }
        }
    }

    if !has_sync_errors {
        update_sync_state_on_conn(&tx, &db_path_str, file_modified, 0)?;
    }

    tx.commit()
        .map_err(|e| AppError::Database(format!("提交 Hermes 用量导入事务失败: {e}")))?;

    if result.imported > 0 {
        log::info!(
            "[HERMES-SYNC] 同步完成: 导入 {} 条, 跳过 {} 条",
            result.imported,
            result.skipped
        );
    }

    Ok(result)
}

fn hermes_db_modified_nanos(db_path: &Path) -> Result<i64, AppError> {
    let metadata = fs::metadata(db_path)
        .map_err(|e| AppError::Config(format!("无法读取 Hermes state.db 元数据: {e}")))?;
    let mut modified = metadata_modified_nanos(&metadata);
    let wal_path = db_path.with_extension("db-wal");
    if let Ok(wal_meta) = fs::metadata(&wal_path) {
        modified = modified.max(metadata_modified_nanos(&wal_meta));
    }
    Ok(modified)
}

fn query_usage_rows(conn: &rusqlite::Connection) -> Result<Vec<HermesUsageRow>, AppError> {
    if table_exists(conn, "session_model_usage")? {
        let cols = table_columns(conn, "session_model_usage")?;
        if cols.contains("session_id") {
            let rows = query_table_rows(conn, "session_model_usage", &cols, true)?;
            if !rows.is_empty() || !table_exists(conn, "sessions")? {
                return Ok(rows);
            }
        }
    }
    if table_exists(conn, "sessions")? {
        let cols = table_columns(conn, "sessions")?;
        return query_table_rows(conn, "sessions", &cols, false);
    }
    Ok(Vec::new())
}

fn query_table_rows(
    conn: &rusqlite::Connection,
    table: &str,
    cols: &HashSet<String>,
    is_model_usage: bool,
) -> Result<Vec<HermesUsageRow>, AppError> {
    let sql = format!("SELECT * FROM {table}");
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| AppError::Database(format!("准备 Hermes {table} 查询失败: {e}")))?;
    let names: Vec<String> = stmt
        .column_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    let rows = stmt
        .query_map([], |row| {
            Ok(map_usage_row(row, &names, cols, is_model_usage))
        })
        .map_err(|e| AppError::Database(format!("查询 Hermes {table} 失败: {e}")))?;

    let mut out = Vec::new();
    for row in rows {
        let mapped =
            row.map_err(|e| AppError::Database(format!("读取 Hermes {table} 行失败: {e}")))?;
        if let Some(mapped) = mapped {
            out.push(mapped);
        }
    }
    Ok(out)
}

fn map_usage_row(
    row: &rusqlite::Row<'_>,
    names: &[String],
    cols: &HashSet<String>,
    is_model_usage: bool,
) -> Option<HermesUsageRow> {
    let session_id = if is_model_usage {
        cell_string(row, names, cols, "session_id")
    } else {
        cell_string(row, names, cols, "id")
    };
    if session_id.is_empty() {
        return None;
    }

    let input_tokens = cell_u32(row, names, cols, "input_tokens");
    let output_tokens = cell_u32(row, names, cols, "output_tokens");
    let cache_read_tokens = cell_u32(row, names, cols, "cache_read_tokens");
    let cache_write_tokens = cell_u32(row, names, cols, "cache_write_tokens");
    let reasoning_tokens = cell_u32(row, names, cols, "reasoning_tokens");
    let output_with_reasoning = output_tokens.saturating_add(reasoning_tokens);
    let estimated_cost = cell_decimal(row, names, cols, "estimated_cost_usd");
    let actual_cost = cell_decimal(row, names, cols, "actual_cost_usd");
    let cost_status = cell_string(row, names, cols, "cost_status");
    let billing_mode = cell_string(row, names, cols, "billing_mode");
    let cost_included = cost_status.eq_ignore_ascii_case("included")
        || billing_mode.eq_ignore_ascii_case("subscription_included");

    if input_tokens == 0
        && output_with_reasoning == 0
        && cache_read_tokens == 0
        && cache_write_tokens == 0
        && actual_cost <= Decimal::ZERO
        && estimated_cost <= Decimal::ZERO
        && !cost_included
    {
        return None;
    }

    let first_seen_secs = cell_unix_secs(row, names, cols, "first_seen")
        .or_else(|| cell_unix_secs(row, names, cols, "started_at"))
        .unwrap_or(0);
    let last_seen_secs = cell_unix_secs(row, names, cols, "last_seen")
        .or_else(|| cell_unix_secs(row, names, cols, "last_activity_at"))
        .or_else(|| cell_unix_secs(row, names, cols, "ended_at"))
        .unwrap_or(first_seen_secs);

    let model = nonempty_or(cell_string(row, names, cols, "model"), UNKNOWN_MODEL);
    Some(HermesUsageRow {
        session_id,
        model,
        billing_provider: cell_string(row, names, cols, "billing_provider"),
        billing_base_url: cell_string(row, names, cols, "billing_base_url"),
        billing_mode,
        task: cell_string(row, names, cols, "task"),
        input_tokens,
        output_tokens: output_with_reasoning,
        cache_read_tokens,
        cache_write_tokens,
        estimated_cost,
        actual_cost,
        cost_included,
        first_seen_secs,
        last_seen_secs,
    })
}

fn import_usage_row(conn: &rusqlite::Connection, row: &HermesUsageRow) -> Result<bool, AppError> {
    let fingerprint = usage_fingerprint(row);
    let watermark_key = format!("{WATERMARK_PREFIX}{}:{fingerprint}", row.session_id);
    let previous = get_hermes_watermark(conn, &watermark_key)?;
    let last_seen_millis = secs_to_millis(row.last_seen_secs);

    let (delta_input, delta_output, delta_cache_read, delta_cache_write, created_at, is_first) =
        match previous {
            Some(prev)
                if prev.last_seen_millis == last_seen_millis
                    && prev.input_tokens == row.input_tokens
                    && prev.output_tokens == row.output_tokens
                    && prev.cache_read_tokens == row.cache_read_tokens
                    && prev.cache_write_tokens == row.cache_write_tokens =>
            {
                return Ok(false);
            }
            Some(prev) => {
                let delta_input = row.input_tokens.saturating_sub(prev.input_tokens);
                let delta_output = row.output_tokens.saturating_sub(prev.output_tokens);
                let delta_cache_read = row.cache_read_tokens.saturating_sub(prev.cache_read_tokens);
                let delta_cache_write = row
                    .cache_write_tokens
                    .saturating_sub(prev.cache_write_tokens);
                if delta_input == 0
                    && delta_output == 0
                    && delta_cache_read == 0
                    && delta_cache_write == 0
                {
                    update_hermes_watermark(
                        conn,
                        &watermark_key,
                        HermesWatermark {
                            last_seen_millis,
                            ..prev
                        },
                    )?;
                    return Ok(false);
                }
                (
                    delta_input,
                    delta_output,
                    delta_cache_read,
                    delta_cache_write,
                    if row.last_seen_secs > 0 {
                        row.last_seen_secs
                    } else {
                        now_unix_secs()
                    },
                    false,
                )
            }
            None => (
                row.input_tokens,
                row.output_tokens,
                row.cache_read_tokens,
                row.cache_write_tokens,
                if row.first_seen_secs > 0 {
                    row.first_seen_secs
                } else if row.last_seen_secs > 0 {
                    row.last_seen_secs
                } else {
                    now_unix_secs()
                },
                true,
            ),
        };

    let request_id = format!(
        "hermes_session:{}:{fingerprint}:{last_seen_millis}:{}:{}",
        row.session_id, delta_input, delta_output
    );
    let provider_id = {
        let trimmed = row.billing_provider.trim();
        if trimmed.is_empty() {
            PROVIDER_PLACEHOLDER.to_string()
        } else {
            trimmed.to_string()
        }
    };

    let usage = TokenUsage {
        input_tokens: delta_input,
        output_tokens: delta_output,
        cache_read_tokens: delta_cache_read,
        cache_creation_tokens: delta_cache_write,
        model: Some(row.model.clone()),
        message_id: None,
    };
    let (input_cost, output_cost, cache_read_cost, cache_creation_cost, total_cost) =
        costs_for_row(conn, row, &usage, is_first);

    conn.execute(
        "INSERT OR IGNORE INTO proxy_request_logs (
            request_id, provider_id, app_type, model, request_model, pricing_model,
            input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
            input_token_semantics,
            input_cost_usd, output_cost_usd, cache_read_cost_usd,
            cache_creation_cost_usd, total_cost_usd,
            latency_ms, first_token_ms, status_code, error_message, session_id,
            provider_type, is_streaming, cost_multiplier, created_at, data_source
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
            ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26
        )",
        rusqlite::params![
            request_id,
            provider_id,
            APP_TYPE,
            row.model,
            row.model,
            row.model,
            delta_input,
            delta_output,
            delta_cache_read,
            delta_cache_write,
            INPUT_TOKEN_SEMANTICS_FRESH,
            input_cost.to_string(),
            output_cost.to_string(),
            cache_read_cost.to_string(),
            cache_creation_cost.to_string(),
            total_cost.to_string(),
            0i64,
            Option::<i64>::None,
            200i64,
            Option::<String>::None,
            row.session_id,
            Some(DATA_SOURCE),
            1i64,
            "1.0",
            created_at,
            DATA_SOURCE,
        ],
    )
    .map_err(|e| AppError::Database(format!("插入 Hermes 会话用量失败: {e}")))?;

    update_hermes_watermark(
        conn,
        &watermark_key,
        HermesWatermark {
            last_seen_millis,
            input_tokens: row.input_tokens,
            output_tokens: row.output_tokens,
            cache_read_tokens: row.cache_read_tokens,
            cache_write_tokens: row.cache_write_tokens,
        },
    )?;
    Ok(true)
}

fn costs_for_row(
    conn: &rusqlite::Connection,
    row: &HermesUsageRow,
    usage: &TokenUsage,
    is_first: bool,
) -> (Decimal, Decimal, Decimal, Decimal, Decimal) {
    if row.cost_included {
        return (
            Decimal::ZERO,
            Decimal::ZERO,
            Decimal::ZERO,
            Decimal::ZERO,
            Decimal::ZERO,
        );
    }
    if is_first {
        let reported = if row.actual_cost > Decimal::ZERO {
            Some(row.actual_cost)
        } else if row.estimated_cost > Decimal::ZERO {
            Some(row.estimated_cost)
        } else {
            None
        };
        if let Some(total) = reported {
            return (
                Decimal::ZERO,
                Decimal::ZERO,
                Decimal::ZERO,
                Decimal::ZERO,
                total,
            );
        }
    }
    match find_model_pricing(conn, &row.model) {
        Some(pricing) => {
            let cost =
                CostCalculator::calculate_for_app(APP_TYPE, usage, &pricing, Decimal::from(1));
            (
                cost.input_cost,
                cost.output_cost,
                cost.cache_read_cost,
                cost.cache_creation_cost,
                cost.total_cost,
            )
        }
        None => (
            Decimal::ZERO,
            Decimal::ZERO,
            Decimal::ZERO,
            Decimal::ZERO,
            Decimal::ZERO,
        ),
    }
}

fn usage_fingerprint(row: &HermesUsageRow) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, b"hermes-usage-v1");
    hash_field(&mut hasher, row.session_id.as_bytes());
    hash_field(&mut hasher, row.model.as_bytes());
    hash_field(&mut hasher, row.billing_provider.as_bytes());
    hash_field(&mut hasher, row.billing_base_url.as_bytes());
    hash_field(&mut hasher, row.billing_mode.as_bytes());
    hash_field(&mut hasher, row.task.as_bytes());
    let hex = format!("{:x}", hasher.finalize());
    hex[..16].to_string()
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn get_hermes_watermark(
    conn: &rusqlite::Connection,
    key: &str,
) -> Result<Option<HermesWatermark>, AppError> {
    let row = conn
        .query_row(
            "SELECT last_modified, last_line_offset, last_synced_at
             FROM session_log_sync WHERE file_path = ?1",
            rusqlite::params![key],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|e| AppError::Database(format!("读取 Hermes 用量水位失败: {e}")))?;
    let Some((last_seen_millis, packed_io, packed_cache)) = row else {
        return Ok(None);
    };
    if last_seen_millis <= 0 {
        return Ok(None);
    }
    let (input_tokens, output_tokens) = unpack_u32_pair(packed_io);
    let (cache_read_tokens, cache_write_tokens) = unpack_u32_pair(packed_cache);
    Ok(Some(HermesWatermark {
        last_seen_millis,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
    }))
}

fn update_hermes_watermark(
    conn: &rusqlite::Connection,
    key: &str,
    watermark: HermesWatermark,
) -> Result<(), AppError> {
    update_sync_state_on_conn(
        conn,
        key,
        watermark.last_seen_millis,
        pack_u32_pair(watermark.input_tokens, watermark.output_tokens),
    )?;
    conn.execute(
        "UPDATE session_log_sync SET last_synced_at = ?2 WHERE file_path = ?1",
        rusqlite::params![
            key,
            pack_u32_pair(watermark.cache_read_tokens, watermark.cache_write_tokens)
        ],
    )
    .map_err(|e| AppError::Database(format!("更新 Hermes 用量水位失败: {e}")))?;
    Ok(())
}

fn pack_u32_pair(high: u32, low: u32) -> i64 {
    ((u64::from(high) << 32) | u64::from(low)) as i64
}

fn unpack_u32_pair(value: i64) -> (u32, u32) {
    let value = value as u64;
    ((value >> 32) as u32, value as u32)
}

fn table_exists(conn: &rusqlite::Connection, name: &str) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |row| row.get(0),
    )
    .map_err(|e| AppError::Database(format!("检查 Hermes 表 {name} 失败: {e}")))
}

fn table_columns(conn: &rusqlite::Connection, table: &str) -> Result<HashSet<String>, AppError> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| AppError::Database(format!("读取 Hermes {table} 列失败: {e}")))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| AppError::Database(format!("查询 Hermes {table} 列失败: {e}")))?;
    let mut cols = HashSet::new();
    for row in rows {
        cols.insert(row.map_err(|e| AppError::Database(format!("读取 Hermes 列名失败: {e}")))?);
    }
    Ok(cols)
}

fn column_index(names: &[String], cols: &HashSet<String>, name: &str) -> Option<usize> {
    if !cols.contains(name) {
        return None;
    }
    names.iter().position(|n| n == name)
}

fn cell_string(
    row: &rusqlite::Row<'_>,
    names: &[String],
    cols: &HashSet<String>,
    name: &str,
) -> String {
    let Some(idx) = column_index(names, cols, name) else {
        return String::new();
    };
    match row.get_ref(idx) {
        Ok(ValueRef::Text(text)) => String::from_utf8_lossy(text).trim().to_string(),
        Ok(ValueRef::Integer(value)) => value.to_string(),
        Ok(ValueRef::Real(value)) => value.to_string(),
        _ => String::new(),
    }
}

fn cell_u32(row: &rusqlite::Row<'_>, names: &[String], cols: &HashSet<String>, name: &str) -> u32 {
    cell_i64(row, names, cols, name).clamp(0, i64::from(u32::MAX)) as u32
}

fn cell_i64(row: &rusqlite::Row<'_>, names: &[String], cols: &HashSet<String>, name: &str) -> i64 {
    let Some(idx) = column_index(names, cols, name) else {
        return 0;
    };
    match row.get_ref(idx) {
        Ok(ValueRef::Integer(value)) => value,
        Ok(ValueRef::Real(value)) if value.is_finite() => value as i64,
        Ok(ValueRef::Text(text)) => String::from_utf8_lossy(text)
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .map(|v| v as i64)
            .unwrap_or(0),
        _ => 0,
    }
}

fn cell_decimal(
    row: &rusqlite::Row<'_>,
    names: &[String],
    cols: &HashSet<String>,
    name: &str,
) -> Decimal {
    let Some(idx) = column_index(names, cols, name) else {
        return Decimal::ZERO;
    };
    let raw = match row.get_ref(idx) {
        Ok(ValueRef::Real(value)) if value.is_finite() && value > 0.0 => value.to_string(),
        Ok(ValueRef::Integer(value)) if value > 0 => value.to_string(),
        Ok(ValueRef::Text(text)) => String::from_utf8_lossy(text).into_owned(),
        _ => return Decimal::ZERO,
    };
    Decimal::from_str(raw.trim())
        .or_else(|_| Decimal::from_scientific(raw.trim()))
        .ok()
        .filter(|v| *v > Decimal::ZERO)
        .unwrap_or(Decimal::ZERO)
}

fn cell_unix_secs(
    row: &rusqlite::Row<'_>,
    names: &[String],
    cols: &HashSet<String>,
    name: &str,
) -> Option<i64> {
    let Some(idx) = column_index(names, cols, name) else {
        return None;
    };
    let value = match row.get_ref(idx) {
        Ok(ValueRef::Real(value)) if value.is_finite() && value > 0.0 => value,
        Ok(ValueRef::Integer(value)) if value > 0 => value as f64,
        Ok(ValueRef::Text(text)) => String::from_utf8_lossy(text).parse::<f64>().ok()?,
        _ => return None,
    };
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    let secs = if value > 1_000_000_000_000.0 {
        value / 1000.0
    } else {
        value
    };
    Some(secs as i64)
}

fn nonempty_or(value: String, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

fn secs_to_millis(secs: i64) -> i64 {
    secs.saturating_mul(1000)
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::lock_conn;
    use rusqlite::Connection;

    fn write_hermes_db(path: &Path, sql: &str) {
        let conn = Connection::open(path).expect("create hermes db");
        conn.execute_batch(sql).expect("seed hermes db");
    }

    fn model_usage_schema() -> &'static str {
        r#"
        CREATE TABLE sessions (
            id TEXT PRIMARY KEY,
            model TEXT,
            billing_provider TEXT,
            input_tokens INTEGER DEFAULT 0,
            output_tokens INTEGER DEFAULT 0,
            cache_read_tokens INTEGER DEFAULT 0,
            cache_write_tokens INTEGER DEFAULT 0,
            reasoning_tokens INTEGER DEFAULT 0,
            estimated_cost_usd REAL,
            actual_cost_usd REAL,
            cost_status TEXT,
            started_at REAL NOT NULL,
            ended_at REAL
        );
        CREATE TABLE session_model_usage (
            session_id TEXT NOT NULL,
            model TEXT NOT NULL,
            billing_provider TEXT NOT NULL DEFAULT '',
            billing_base_url TEXT NOT NULL DEFAULT '',
            billing_mode TEXT NOT NULL DEFAULT '',
            task TEXT NOT NULL DEFAULT '',
            api_call_count INTEGER NOT NULL DEFAULT 0,
            input_tokens INTEGER NOT NULL DEFAULT 0,
            output_tokens INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens INTEGER NOT NULL DEFAULT 0,
            cache_write_tokens INTEGER NOT NULL DEFAULT 0,
            reasoning_tokens INTEGER NOT NULL DEFAULT 0,
            estimated_cost_usd REAL NOT NULL DEFAULT 0,
            actual_cost_usd REAL NOT NULL DEFAULT 0,
            cost_status TEXT,
            cost_source TEXT,
            first_seen REAL,
            last_seen REAL,
            PRIMARY KEY (session_id, model, billing_provider, billing_base_url, billing_mode, task)
        );
        "#
    }

    #[test]
    fn missing_db_is_a_noop() -> Result<(), AppError> {
        let db = Database::memory()?;
        let result = sync_hermes_db(&db, Path::new("C:\\definitely-missing-hermes-state.db"))?;
        assert_eq!(result.imported, 0);
        assert_eq!(result.files_scanned, 0);
        Ok(())
    }

    #[test]
    fn imports_model_usage_including_aux_tasks() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("state.db");
        write_hermes_db(
            &path,
            &format!(
                "{schema}
                INSERT INTO sessions (id, model, billing_provider, input_tokens, output_tokens, started_at)
                VALUES ('sess-1', 'grok-4.6', 'xai-oauth', 10, 4, 1700000000);
                INSERT INTO session_model_usage (
                    session_id, model, billing_provider, billing_base_url, billing_mode, task,
                    api_call_count, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                    reasoning_tokens, estimated_cost_usd, actual_cost_usd, cost_status, first_seen, last_seen
                ) VALUES
                ('sess-1', 'grok-4.6', 'xai-oauth', 'https://api.x.ai/v1', '', '',
                 3, 10, 4, 8, 1, 2, 0.0125, 0, 'estimated', 1700000000, 1700000060),
                ('sess-1', 'vision-model', 'gemini', '', '', 'vision',
                 1, 5, 3, 0, 0, 0, 0.002, 0, NULL, 1700000030, 1700000030);
                ",
                schema = model_usage_schema()
            ),
        );

        let db = Database::memory()?;
        let result = sync_hermes_db(&db, &path)?;
        assert_eq!(result.imported, 2);
        assert!(result.errors.is_empty());

        {
            let conn = lock_conn!(db.conn);
            let totals: (i64, i64, i64, i64, i64, i64) = conn.query_row(
                "SELECT COUNT(*), SUM(input_tokens), SUM(output_tokens),
                        SUM(cache_read_tokens), SUM(cache_creation_tokens),
                        SUM(CASE WHEN model = 'grok-4.6' THEN 1 ELSE 0 END)
                 FROM proxy_request_logs WHERE data_source = 'hermes_session'",
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
            )?;
            // output includes reasoning (4+2)
            assert_eq!(totals, (2, 15, 9, 8, 1, 1));

            let main: (String, String, String, i64) = conn.query_row(
                "SELECT provider_id, app_type, total_cost_usd, created_at
                 FROM proxy_request_logs WHERE model = 'grok-4.6'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
            assert_eq!(main.0, "xai-oauth");
            assert_eq!(main.1, "hermes");
            assert_eq!(
                Decimal::from_str(&main.2).unwrap(),
                Decimal::from_str("0.0125").unwrap()
            );
            assert_eq!(main.3, 1_700_000_000);

            let semantics: i64 = conn.query_row(
                "SELECT input_token_semantics FROM proxy_request_logs WHERE model = 'grok-4.6'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(semantics, INPUT_TOKEN_SEMANTICS_FRESH);
        }

        let providers = db.get_provider_stats(None, None, Some("hermes"), None, None)?;
        assert!(providers.iter().any(|p| p.provider_id == "xai-oauth"));
        Ok(())
    }

    #[test]
    fn incremental_sync_inserts_only_the_delta() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("state.db");
        write_hermes_db(
            &path,
            &format!(
                "{schema}
                INSERT INTO session_model_usage (
                    session_id, model, billing_provider, billing_base_url, billing_mode, task,
                    api_call_count, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                    reasoning_tokens, estimated_cost_usd, first_seen, last_seen
                ) VALUES ('sess-2', 'm', 'p', '', '', '', 1, 10, 2, 4, 0, 0, 0, 1700000000, 1700000000);
                ",
                schema = model_usage_schema()
            ),
        );

        let db = Database::memory()?;
        let first = sync_hermes_db(&db, &path)?;
        assert_eq!(first.imported, 1);

        {
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "UPDATE session_model_usage
                 SET input_tokens = 25, output_tokens = 7, cache_read_tokens = 10,
                     last_seen = 1700000100
                 WHERE session_id = 'sess-2'",
                [],
            )
            .unwrap();
        }
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(later)
            .unwrap();

        let second = sync_hermes_db(&db, &path)?;
        assert_eq!(second.imported, 1);

        let third = sync_hermes_db(&db, &path)?;
        assert_eq!(third.imported, 0);

        {
            let conn = lock_conn!(db.conn);
            let totals: (i64, i64, i64, i64) = conn.query_row(
                "SELECT COUNT(*), SUM(input_tokens), SUM(output_tokens), SUM(cache_read_tokens)
                 FROM proxy_request_logs WHERE data_source = 'hermes_session'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
            assert_eq!(totals, (2, 25, 7, 10));
            let latest_created: i64 = conn.query_row(
                "SELECT MAX(created_at) FROM proxy_request_logs WHERE data_source = 'hermes_session'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(latest_created, 1_700_000_100);
        }
        Ok(())
    }

    #[test]
    fn subscription_included_cost_stays_zero() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("state.db");
        write_hermes_db(
            &path,
            &format!(
                "{schema}
                INSERT INTO session_model_usage (
                    session_id, model, billing_provider, billing_base_url, billing_mode, task,
                    input_tokens, output_tokens, estimated_cost_usd, actual_cost_usd,
                    cost_status, first_seen, last_seen
                ) VALUES ('sess-3', 'gpt-5.6-sol', 'openai-codex', '', 'subscription_included', '',
                          100, 20, 0, 0, 'included', 1700000000, 1700000000);
                ",
                schema = model_usage_schema()
            ),
        );

        let db = Database::memory()?;
        {
            let conn = lock_conn!(db.conn);
            conn.execute(
                "INSERT OR REPLACE INTO model_pricing (
                    model_id, display_name, input_cost_per_million,
                    output_cost_per_million, cache_read_cost_per_million,
                    cache_creation_cost_per_million
                ) VALUES ('gpt-5.6-sol', 'GPT', '5', '30', '0.5', '6.25')",
                [],
            )?;
        }

        let result = sync_hermes_db(&db, &path)?;
        assert_eq!(result.imported, 1);
        {
            let conn = lock_conn!(db.conn);
            let total: String = conn.query_row(
                "SELECT total_cost_usd FROM proxy_request_logs WHERE data_source = 'hermes_session'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(Decimal::from_str(&total).unwrap(), Decimal::ZERO);
            let provider: String = conn.query_row(
                "SELECT provider_id FROM proxy_request_logs WHERE data_source = 'hermes_session'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(provider, "openai-codex");
        }
        Ok(())
    }

    #[test]
    fn falls_back_to_sessions_table() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("state.db");
        write_hermes_db(
            &path,
            r#"
            CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                model TEXT,
                billing_provider TEXT,
                input_tokens INTEGER DEFAULT 0,
                output_tokens INTEGER DEFAULT 0,
                cache_read_tokens INTEGER DEFAULT 0,
                cache_write_tokens INTEGER DEFAULT 0,
                reasoning_tokens INTEGER DEFAULT 0,
                estimated_cost_usd REAL,
                started_at REAL NOT NULL
            );
            INSERT INTO sessions (
                id, model, billing_provider, input_tokens, output_tokens,
                cache_read_tokens, reasoning_tokens, estimated_cost_usd, started_at
            ) VALUES ('legacy', 'claude-sonnet-4-5', '', 12, 3, 7, 1, 0.01, 1700000000);
            "#,
        );

        let db = Database::memory()?;
        let result = sync_hermes_db(&db, &path)?;
        assert_eq!(result.imported, 1);
        {
            let conn = lock_conn!(db.conn);
            let row: (String, i64, i64, i64) = conn.query_row(
                "SELECT provider_id, input_tokens, output_tokens, cache_read_tokens
                 FROM proxy_request_logs WHERE data_source = 'hermes_session'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
            assert_eq!(row.0, PROVIDER_PLACEHOLDER);
            assert_eq!(row.1, 12);
            assert_eq!(row.2, 4); // output + reasoning
            assert_eq!(row.3, 7);
        }

        let providers = db.get_provider_stats(None, None, Some("hermes"), None, None)?;
        assert!(providers.iter().any(|p| {
            p.provider_id == PROVIDER_PLACEHOLDER && p.provider_name == "Hermes (Session)"
        }));
        Ok(())
    }

    #[test]
    fn skips_zero_token_rows() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("state.db");
        write_hermes_db(
            &path,
            &format!(
                "{schema}
                INSERT INTO session_model_usage (
                    session_id, model, billing_provider, task, input_tokens, output_tokens,
                    first_seen, last_seen
                ) VALUES ('empty', 'm', 'p', '', 0, 0, 1700000000, 1700000000);
                ",
                schema = model_usage_schema()
            ),
        );
        let db = Database::memory()?;
        let result = sync_hermes_db(&db, &path)?;
        assert_eq!(result.imported, 0);
        Ok(())
    }
}
