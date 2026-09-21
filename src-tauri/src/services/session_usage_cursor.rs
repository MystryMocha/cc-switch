//! Import Cursor IDE usage from the unofficial DashboardService API.
//!
//! Local agent transcripts do not carry billed token counts. Cursor stores
//! billed usage on their servers; this importer reads the access token the
//! IDE already has and copies `GetFilteredUsageEvents` into `proxy_request_logs`
//! so the shared dashboard can show a Cursor filter.

use crate::cursor_config::read_cursor_access_token;
use crate::database::{lock_conn, Database};
use crate::error::AppError;
use crate::services::session_usage::SessionSyncResult;
use crate::services::sql_helpers::INPUT_TOKEN_SEMANTICS_FRESH;
use rusqlite::params;
use serde_json::Value;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const APP_TYPE: &str = "cursor";
const DATA_SOURCE: &str = "cursor_dashboard";
const PROVIDER_PLACEHOLDER: &str = "_cursor_session";
const PROVIDER_FIRST_PARTY: &str = "_cursor_first_party";
const PROVIDER_THIRD_PARTY: &str = "_cursor_third_party";
const SYNC_KEY: &str = "cursor:dashboard:GetFilteredUsageEvents:v4";
const LEGACY_SYNC_KEYS: [&str; 3] = [
    "cursor:dashboard:GetFilteredUsageEvents",
    "cursor:dashboard:GetFilteredUsageEvents:v2",
    "cursor:dashboard:GetFilteredUsageEvents:v3",
];
const DASHBOARD_BASE: &str = "https://api2.cursor.sh/aiserver.v1.DashboardService";
const PAGE_SIZE: u32 = 100;
const MAX_PAGES_PER_RUN: u32 = 20;
const FIRST_SYNC_LOOKBACK_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const OVERLAP_MS: i64 = 60 * 60 * 1000;
const UNKNOWN_MODEL: &str = "unknown";

#[derive(Debug, Clone)]
struct CursorUsageEvent {
    request_id: String,
    timestamp_ms: i64,
    model: String,
    conversation_id: String,
    provider_id: String,
    input_tokens: u32,
    output_tokens: u32,
    cache_read_tokens: u32,
    cache_write_tokens: u32,
    cost_usd: String,
    status_code: i64,
}

#[derive(Debug)]
struct UsageEventsPage {
    events: Vec<CursorUsageEvent>,
    total: u64,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn json_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().map(|n| n as i64))
        .or_else(|| value.as_f64().map(|n| n as i64))
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
}

fn json_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_i64().map(|n| n as f64))
        .or_else(|| value.as_u64().map(|n| n as f64))
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
}

fn json_u32(value: Option<&Value>) -> u32 {
    value.and_then(json_i64).unwrap_or(0).max(0) as u32
}

fn json_string(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(other) => other.as_i64().map(|n| n.to_string()).unwrap_or_default(),
        None => String::new(),
    }
}

fn parse_numeric_cost(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => {
            let text = text.trim();
            if text.is_empty() || text == "-" {
                None
            } else {
                text.parse().ok()
            }
        }
        _ => None,
    }
}

/// Cursor 事件的美元数是套餐池消耗（模型标价），不是用户实付。
/// on-demand 优先用 `usageBasedCosts`；included 用 `chargedCents` / `totalCents`
/// 记入第一方或第三方池。
fn event_cost_usd(value: &Value) -> String {
    if let Some(cents) = parse_numeric_cost(value.get("usageBasedCosts")) {
        if cents > 0.0 {
            return format!("{:.8}", cents / 100.0);
        }
    }
    let charged = parse_numeric_cost(value.get("chargedCents")).or_else(|| {
        value
            .get("tokenUsage")
            .and_then(|usage| parse_numeric_cost(usage.get("totalCents")))
    });
    match charged {
        Some(cents) if cents > 0.0 => format!("{:.8}", cents / 100.0),
        _ => "0".to_string(),
    }
}

/// Cursor 内部 CUA / grok-bot / 沙箱子代理会刷出大量 cache 命中，官方两个用量池都不计入。
fn is_internal_cursor_traffic(model: &str, conversation_id: &str) -> bool {
    let model = model.to_ascii_lowercase();
    let conversation_id = conversation_id.to_ascii_lowercase();
    model.contains("grok-bot") || conversation_id.starts_with("sand-subagent")
}

/// 去掉质量档和版本号后的模型族，用来把 `grok-4.6` 对上 bucket 里的 `grok-4.5`。
fn model_family(model: &str) -> String {
    const DROP: &[&str] = &[
        "fast", "slow", "low", "medium", "high", "xhigh", "thinking", "max", "mini", "preview",
    ];
    model
        .to_ascii_lowercase()
        .split('-')
        .filter(|part| {
            if DROP.contains(part) {
                return false;
            }
            let version_like = part.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
                && part.chars().any(|ch| ch.is_ascii_digit());
            !version_like
        })
        .collect::<Vec<_>>()
        .join("-")
}

fn looks_like_first_party_family(model: &str) -> bool {
    matches!(
        model_family(model).as_str(),
        "composer" | "vega" | "grok" | "cursor-grok" | "default"
    )
}

fn is_first_party_model(model: &str, auto_bucket: &[String]) -> bool {
    let model_lc = model.to_ascii_lowercase();
    if auto_bucket
        .iter()
        .any(|item| item.eq_ignore_ascii_case(&model_lc))
    {
        return true;
    }
    let family = model_family(&model_lc);
    if family.is_empty() {
        return false;
    }
    if auto_bucket.iter().any(|item| model_family(item) == family) {
        return true;
    }
    auto_bucket.is_empty() && looks_like_first_party_family(&model_lc)
}

fn pool_provider_id(model: &str, auto_bucket: &[String]) -> &'static str {
    if is_first_party_model(model, auto_bucket) {
        PROVIDER_FIRST_PARTY
    } else {
        PROVIDER_THIRD_PARTY
    }
}

fn parse_auto_bucket_models(value: &Value) -> Vec<String> {
    value
        .get("autoBucketModels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str())
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

fn parse_event(value: &Value, auto_bucket: &[String]) -> Option<CursorUsageEvent> {
    let timestamp_ms = json_i64(value.get("timestamp")?)?;
    if timestamp_ms <= 0 {
        return None;
    }
    let model = {
        let raw = json_string(value.get("model"));
        if raw.is_empty() {
            UNKNOWN_MODEL.to_string()
        } else {
            raw
        }
    };
    let conversation_id = {
        let raw = json_string(value.get("conversationId"));
        if raw.is_empty() {
            "none".to_string()
        } else {
            raw
        }
    };
    if is_internal_cursor_traffic(&model, &conversation_id) {
        return None;
    }
    let usage = value.get("tokenUsage").cloned().unwrap_or(Value::Null);
    let input_tokens = json_u32(usage.get("inputTokens"));
    let output_tokens = json_u32(usage.get("outputTokens"));
    let cache_read_tokens = json_u32(usage.get("cacheReadTokens"));
    let cache_write_tokens = json_u32(usage.get("cacheWriteTokens"));
    let kind = json_string(value.get("kind")).to_ascii_uppercase();
    let cost_usd = event_cost_usd(value);
    let status_code = if kind.contains("ERROR") { 500 } else { 200 };
    let request_id = format!("cursor:{conversation_id}:{timestamp_ms}:{model}");
    let provider_id = pool_provider_id(&model, auto_bucket).to_string();
    Some(CursorUsageEvent {
        request_id,
        timestamp_ms,
        model,
        conversation_id,
        provider_id,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        cost_usd,
        status_code,
    })
}

fn parse_events_page(body: &Value, auto_bucket: &[String]) -> UsageEventsPage {
    let total = json_i64(body.get("totalUsageEventsCount").unwrap_or(&Value::Null))
        .unwrap_or(0)
        .max(0) as u64;
    let events = body
        .get("usageEventsDisplay")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|event| parse_event(event, auto_bucket))
        .collect();
    UsageEventsPage { events, total }
}

fn dashboard_client() -> Result<reqwest::Client, AppError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| AppError::Message(format!("创建 Cursor HTTP 客户端失败: {e}")))
}

async fn dashboard_post(
    client: &reqwest::Client,
    token: &str,
    method: &str,
    body: Value,
) -> Result<Value, AppError> {
    let url = format!("{DASHBOARD_BASE}/{method}");
    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .header("Connect-Protocol-Version", "1")
        .json(&body)
        .send()
        .await
        .map_err(|e| AppError::Message(format!("请求 Cursor {method} 失败: {e}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| AppError::Message(format!("读取 Cursor {method} 响应失败: {e}")))?;
    if !status.is_success() {
        return Err(AppError::Message(format!(
            "Cursor {method} HTTP {status}: {}",
            text.chars().take(240).collect::<String>()
        )));
    }
    if text.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    serde_json::from_str(&text)
        .map_err(|e| AppError::Message(format!("解析 Cursor {method} 响应失败: {e}")))
}

fn reset_inflated_cursor_usage(db: &Database) -> Result<bool, AppError> {
    let mut conn = lock_conn!(db.conn);
    let already: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session_log_sync WHERE file_path = ?1",
            [SYNC_KEY],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if already > 0 {
        return Ok(false);
    }
    let tx = conn
        .transaction()
        .map_err(|e| AppError::Database(format!("清理 Cursor 用量失败: {e}")))?;
    tx.execute(
        "DELETE FROM proxy_request_logs WHERE data_source = ?1",
        [DATA_SOURCE],
    )
    .map_err(|e| AppError::Database(format!("清理 Cursor 用量明细失败: {e}")))?;
    for provider_id in [
        PROVIDER_PLACEHOLDER,
        PROVIDER_FIRST_PARTY,
        PROVIDER_THIRD_PARTY,
    ] {
        let _ = tx.execute(
            "DELETE FROM usage_daily_rollups WHERE provider_id = ?1",
            [provider_id],
        );
    }
    for key in LEGACY_SYNC_KEYS {
        tx.execute("DELETE FROM session_log_sync WHERE file_path = ?1", [key])
            .map_err(|e| AppError::Database(format!("清理 Cursor 同步游标失败: {e}")))?;
    }
    tx.commit()
        .map_err(|e| AppError::Database(format!("提交 Cursor 用量清理失败: {e}")))?;
    log::info!("[CURSOR-SYNC] 已清除按模型标价累计的旧 Cursor 用量，将按订阅口径重导");
    Ok(true)
}

fn load_watermark(db: &Database) -> Result<i64, AppError> {
    let conn = lock_conn!(db.conn);
    let ts: Option<i64> = conn
        .query_row(
            "SELECT last_modified FROM session_log_sync WHERE file_path = ?1",
            [SYNC_KEY],
            |row| row.get(0),
        )
        .ok();
    Ok(ts.unwrap_or(0))
}

fn store_watermark(db: &Database, timestamp_ms: i64) -> Result<(), AppError> {
    let conn = lock_conn!(db.conn);
    conn.execute(
        "INSERT INTO session_log_sync (file_path, last_modified, last_line_offset, last_synced_at)
         VALUES (?1, ?2, 0, unixepoch())
         ON CONFLICT(file_path) DO UPDATE SET
           last_modified = MAX(session_log_sync.last_modified, excluded.last_modified),
           last_synced_at = excluded.last_synced_at",
        params![SYNC_KEY, timestamp_ms],
    )
    .map_err(|e| AppError::Database(format!("写入 Cursor 同步游标失败: {e}")))?;
    Ok(())
}

fn insert_events(db: &Database, events: &[CursorUsageEvent]) -> Result<(u32, u32), AppError> {
    if events.is_empty() {
        return Ok((0, 0));
    }
    let mut conn = lock_conn!(db.conn);
    let tx = conn
        .transaction()
        .map_err(|e| AppError::Database(format!("Cursor 用量事务失败: {e}")))?;
    let mut imported = 0u32;
    let mut skipped = 0u32;
    for event in events {
        let changed = tx
            .execute(
                "INSERT OR IGNORE INTO proxy_request_logs (
                    request_id, provider_id, app_type, model, request_model,
                    input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                    input_cost_usd, output_cost_usd, cache_read_cost_usd, cache_creation_cost_usd,
                    total_cost_usd, latency_ms, status_code, session_id, provider_type,
                    is_streaming, cost_multiplier, created_at, data_source, input_token_semantics
                 ) VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7, ?8,
                           '0', '0', '0', '0', ?9, 0, ?10, ?11, 'cursor_dashboard',
                           1, '1', ?12, ?13, ?14)",
                params![
                    event.request_id,
                    event.provider_id,
                    APP_TYPE,
                    event.model,
                    event.input_tokens,
                    event.output_tokens,
                    event.cache_read_tokens,
                    event.cache_write_tokens,
                    event.cost_usd,
                    event.status_code,
                    event.conversation_id,
                    event.timestamp_ms / 1000,
                    DATA_SOURCE,
                    INPUT_TOKEN_SEMANTICS_FRESH,
                ],
            )
            .map_err(|e| AppError::Database(format!("写入 Cursor 用量失败: {e}")))?;
        imported += changed as u32;
        skipped += u32::from(changed == 0);
    }
    tx.commit()
        .map_err(|e| AppError::Database(format!("提交 Cursor 用量失败: {e}")))?;
    Ok((imported, skipped))
}

async fn fetch_page(
    client: &reqwest::Client,
    token: &str,
    start_date_ms: i64,
    page: u32,
    auto_bucket: &[String],
) -> Result<UsageEventsPage, AppError> {
    let body = serde_json::json!({
        "page": page,
        "pageSize": PAGE_SIZE,
        "startDate": start_date_ms.to_string(),
        "endDate": now_ms().to_string(),
    });
    let value = dashboard_post(client, token, "GetFilteredUsageEvents", body).await?;
    Ok(parse_events_page(&value, auto_bucket))
}

/// Pull Cursor dashboard events into the shared usage table.
pub async fn sync_cursor_usage(db: &Database) -> Result<SessionSyncResult, AppError> {
    let Some(mut token) = read_cursor_access_token() else {
        log::info!("[CURSOR-SYNC] 未找到 Cursor 登录 token，跳过");
        return Ok(SessionSyncResult::default());
    };
    log::info!("[CURSOR-SYNC] 开始从 Dashboard 拉取用量");
    let client = dashboard_client()?;
    let _ = reset_inflated_cursor_usage(db)?;
    let watermark = load_watermark(db)?;
    let period = dashboard_post(
        &client,
        &token,
        "GetCurrentPeriodUsage",
        serde_json::json!({}),
    )
    .await
    .ok();
    let billing_start = period
        .as_ref()
        .and_then(|value| value.get("billingCycleStart").and_then(json_i64));
    let auto_bucket = period
        .as_ref()
        .map(parse_auto_bucket_models)
        .unwrap_or_default();
    let start_date_ms = if watermark > 0 {
        watermark.saturating_sub(OVERLAP_MS)
    } else {
        billing_start.unwrap_or_else(|| now_ms().saturating_sub(FIRST_SYNC_LOOKBACK_MS))
    };

    let mut result = SessionSyncResult::default();
    let mut max_ts = watermark;
    let mut page = 1u32;
    loop {
        let page_result = match fetch_page(&client, &token, start_date_ms, page, &auto_bucket).await
        {
            Ok(page) => page,
            Err(error) if page == 1 && error.to_string().contains("HTTP 401") => {
                // IDE may have rotated the token while we were running.
                token = read_cursor_access_token().ok_or(error)?;
                fetch_page(&client, &token, start_date_ms, page, &auto_bucket).await?
            }
            Err(error) => return Err(error),
        };
        result.files_scanned = result.files_scanned.saturating_add(1);
        log::info!(
            "[CURSOR-SYNC] 第 {page} 页: {} 条, total={}",
            page_result.events.len(),
            page_result.total
        );
        if page_result.events.is_empty() {
            break;
        }
        for event in &page_result.events {
            max_ts = max_ts.max(event.timestamp_ms);
        }
        let (imported, skipped) = insert_events(db, &page_result.events)?;
        result.imported = result.imported.saturating_add(imported);
        result.skipped = result.skipped.saturating_add(skipped);
        if max_ts > 0 {
            store_watermark(db, max_ts)?;
        }
        let fetched = (page as u64).saturating_mul(PAGE_SIZE as u64);
        if fetched >= page_result.total
            || (page_result.events.len() as u32) < PAGE_SIZE
            || page >= MAX_PAGES_PER_RUN
        {
            break;
        }
        page = page.saturating_add(1);
    }
    if result.imported > 0 {
        log::info!(
            "[CURSOR-SYNC] 同步完成: 导入 {} 条, 跳过 {} 条, 请求 {} 页",
            result.imported,
            result.skipped,
            result.files_scanned
        );
    }
    Ok(result)
}

pub(crate) async fn fetch_current_period_usage() -> Result<Value, AppError> {
    let Some(token) = read_cursor_access_token() else {
        return Err(AppError::Message("cursor_auth_missing".to_string()));
    };
    let client = dashboard_client()?;
    dashboard_post(
        &client,
        &token,
        "GetCurrentPeriodUsage",
        serde_json::json!({}),
    )
    .await
}

pub(crate) async fn fetch_aggregated_usage_events() -> Result<Value, AppError> {
    let Some(token) = read_cursor_access_token() else {
        return Err(AppError::Message("cursor_auth_missing".to_string()));
    };
    let client = dashboard_client()?;
    dashboard_post(
        &client,
        &token,
        "GetAggregatedUsageEvents",
        serde_json::json!({}),
    )
    .await
}

pub(crate) fn json_number_f64(value: Option<&Value>) -> Option<f64> {
    value.and_then(json_f64)
}

pub(crate) fn json_number_i64(value: Option<&Value>) -> Option<i64> {
    value.and_then(json_i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_auto_bucket() -> Vec<String> {
        vec![
            "default".into(),
            "composer-2.5".into(),
            "composer-2.5-fast".into(),
            "grok-4.5".into(),
            "cursor-grok-4.5".into(),
            "cursor-grok-4.5-high-fast".into(),
        ]
    }

    #[test]
    fn parse_event_reads_tokens_and_cents() {
        let event = parse_event(
            &json!({
                "timestamp": "1789915193025",
                "model": "composer-2.5",
                "kind": "USAGE_EVENT_KIND_INCLUDED_IN_ULTRA",
                "conversationId": "8767c4c9-d7e5-4415-9c2e-1b7887339862",
                "chargedCents": 15.4596,
                "tokenUsage": {
                    "inputTokens": 474,
                    "outputTokens": 14,
                    "cacheReadTokens": 152448,
                    "cacheWriteTokens": 0,
                    "totalCents": 15.4596
                }
            }),
            &sample_auto_bucket(),
        )
        .expect("event");
        assert_eq!(event.input_tokens, 474);
        assert_eq!(event.output_tokens, 14);
        assert_eq!(event.cache_read_tokens, 152448);
        assert_eq!(event.model, "composer-2.5");
        assert_eq!(event.provider_id, PROVIDER_FIRST_PARTY);
        assert_eq!(event.cost_usd, "0.15459600");
        assert_eq!(event.status_code, 200);
        assert!(event.request_id.contains("8767c4c9"));
    }

    #[test]
    fn parse_event_skips_internal_grok_bot_and_sandbox_subagents() {
        assert!(parse_event(
            &json!({
                "timestamp": 1,
                "model": "grok-bot-cua",
                "conversationId": "abc",
                "kind": "USAGE_EVENT_KIND_INCLUDED_IN_ULTRA",
                "chargedCents": 12.0,
                "tokenUsage": { "inputTokens": 3, "outputTokens": 10, "cacheReadTokens": 50000 }
            }),
            &sample_auto_bucket(),
        )
        .is_none());
        assert!(parse_event(
            &json!({
                "timestamp": 1,
                "model": "grok-bot-default",
                "conversationId": "abc",
                "kind": "USAGE_EVENT_KIND_INCLUDED_IN_ULTRA",
                "chargedCents": 12.0,
                "tokenUsage": { "inputTokens": 3, "outputTokens": 10, "cacheReadTokens": 50000 }
            }),
            &sample_auto_bucket(),
        )
        .is_none());
        assert!(parse_event(
            &json!({
                "timestamp": 1,
                "model": "composer-2.5",
                "conversationId": "sand-subagent-82162911-c",
                "kind": "USAGE_EVENT_KIND_INCLUDED_IN_ULTRA",
                "chargedCents": 12.0,
                "tokenUsage": { "inputTokens": 3, "outputTokens": 10, "cacheReadTokens": 50000 }
            }),
            &sample_auto_bucket(),
        )
        .is_none());
    }

    #[test]
    fn parse_event_keeps_usage_based_cost() {
        let event = parse_event(
            &json!({
                "timestamp": 2,
                "model": "claude-opus-5",
                "conversationId": "abc",
                "kind": "USAGE_EVENT_KIND_USAGE_BASED",
                "usageBasedCosts": 250.0,
                "chargedCents": 250.0,
                "tokenUsage": { "inputTokens": 10, "outputTokens": 4, "cacheReadTokens": 0 }
            }),
            &sample_auto_bucket(),
        )
        .expect("event");
        assert_eq!(event.cost_usd, "2.50000000");
        assert_eq!(event.provider_id, PROVIDER_THIRD_PARTY);
    }

    #[test]
    fn first_party_family_matches_newer_grok_against_bucket() {
        let bucket = sample_auto_bucket();
        assert!(is_first_party_model("cursor-grok-4.6-xhigh-fast", &bucket));
        assert!(is_first_party_model("composer-2.5-fast", &bucket));
        assert!(is_first_party_model("default", &bucket));
        assert!(!is_first_party_model(
            "claude-fable-5-1-thinking-high",
            &bucket
        ));
        assert!(!is_first_party_model("gemini-2.5-flash", &bucket));
        assert!(is_first_party_model("composer-2.5", &[]));
        assert!(!is_first_party_model("claude-opus-5", &[]));
    }

    #[test]
    fn insert_is_idempotent() {
        let db = Database::memory().unwrap();
        let event = parse_event(
            &json!({
                "timestamp": 1000,
                "model": "composer-2.5",
                "conversationId": "abc",
                "chargedCents": 10.0,
                "tokenUsage": { "inputTokens": 1, "outputTokens": 2, "cacheReadTokens": 3 }
            }),
            &sample_auto_bucket(),
        )
        .unwrap();
        assert_eq!(event.provider_id, PROVIDER_FIRST_PARTY);
        assert_eq!(insert_events(&db, &[event.clone()]).unwrap(), (1, 0));
        assert_eq!(insert_events(&db, &[event]).unwrap(), (0, 1));
        let conn = db.conn.lock().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM proxy_request_logs WHERE app_type='cursor'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        let provider: String = conn
            .query_row(
                "SELECT provider_id FROM proxy_request_logs WHERE app_type='cursor'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(provider, PROVIDER_FIRST_PARTY);
        let cost: String = conn
            .query_row(
                "SELECT total_cost_usd FROM proxy_request_logs WHERE app_type='cursor'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cost, "0.10000000");
        let semantics: i64 = conn
            .query_row(
                "SELECT input_token_semantics FROM proxy_request_logs WHERE app_type='cursor'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(semantics, INPUT_TOKEN_SEMANTICS_FRESH);
    }
}
