//! Cursor IDE / CLI configuration.
//!
//! Cursor does not expose a first-class provider config file the way Claude
//! Code or Codex do. CC Switch therefore:
//! - stores the active provider snapshot at `~/.cursor/cc-switch-provider.json`
//! - writes MCP servers to the documented `~/.cursor/mcp.json`
//! - projects skills into `~/.cursor/skills`
//!
//! Switching a provider also best-effort writes OpenAI BYOK keys into Cursor
//! IDE's `state.vscdb` when that database exists. Cursor still needs a restart
//! to pick the new key up; the live JSON snapshot is the source of truth for
//! CC Switch.

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{atomic_write_private, get_home_dir, write_json_file};
use crate::error::AppError;
use crate::provider::Provider;
use crate::settings::get_cursor_override_dir;

const MCP_FILE: &str = "mcp.json";
const LIVE_PROVIDER_FILE: &str = "cc-switch-provider.json";

pub fn get_cursor_dir() -> PathBuf {
    if let Some(custom) = get_cursor_override_dir() {
        return custom;
    }
    get_home_dir().join(".cursor")
}

pub fn get_cursor_mcp_path() -> PathBuf {
    get_cursor_dir().join(MCP_FILE)
}

pub fn get_cursor_live_provider_path() -> PathBuf {
    get_cursor_dir().join(LIVE_PROVIDER_FILE)
}

pub fn get_cursor_skills_dir() -> PathBuf {
    get_cursor_dir().join("skills")
}

fn cursor_dir_ready() -> bool {
    get_cursor_dir().exists() || get_cursor_mcp_path().exists()
}

pub fn extract_openai_env(settings: &Value) -> (String, String, String) {
    let env = settings.get("env").and_then(Value::as_object);
    let get = |key: &str| -> String {
        env.and_then(|env| env.get(key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or_default()
            .to_string()
    };
    (
        get("OPENAI_BASE_URL").trim_end_matches('/').to_string(),
        get("OPENAI_API_KEY"),
        get("OPENAI_MODEL"),
    )
}

pub fn validate_provider_settings(settings: &Value) -> Result<(), AppError> {
    let obj = settings.as_object().ok_or_else(|| {
        AppError::localized(
            "provider.cursor.settings.not_object",
            "Cursor 配置必须是 JSON 对象",
            "Cursor configuration must be a JSON object",
        )
    })?;
    let env = obj.get("env").and_then(Value::as_object).ok_or_else(|| {
        AppError::localized(
            "provider.cursor.env.missing",
            "Cursor 配置缺少 env 字段",
            "Cursor configuration is missing the env field",
        )
    })?;
    let base_url = env
        .get("OPENAI_BASE_URL")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if !url::Url::parse(base_url)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
    {
        return Err(AppError::localized(
            "provider.cursor.base_url.invalid",
            "请填写有效的 Cursor Override OpenAI Base URL（需包含 http/https）",
            "Enter a valid Cursor Override OpenAI Base URL (http or https)",
        ));
    }
    let api_key = env
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if api_key.is_empty() {
        return Err(AppError::localized(
            "provider.cursor.api_key.missing",
            "请填写 Cursor 的 OpenAI API Key",
            "Enter a Cursor OpenAI API key",
        ));
    }
    Ok(())
}

pub fn write_cursor_provider_live(provider: &Provider) -> Result<(), AppError> {
    validate_provider_settings(&provider.settings_config)?;
    let dir = get_cursor_dir();
    fs::create_dir_all(&dir).map_err(|e| AppError::io(&dir, e))?;

    let (base_url, api_key, model) = extract_openai_env(&provider.settings_config);
    let mut live = provider.settings_config.clone();
    if let Some(root) = live.as_object_mut() {
        root.insert(
            "ccSwitch".into(),
            json!({
                "providerId": provider.id,
                "providerName": provider.name,
            }),
        );
    }
    let path = get_cursor_live_provider_path();
    let bytes = serde_json::to_vec_pretty(&live).map_err(|e| AppError::json(&path, e))?;
    atomic_write_private(&path, &bytes)?;

    if let Err(error) = write_ide_byok(&base_url, &api_key, &model) {
        log::warn!("Cursor IDE BYOK write skipped: {error}");
    }
    Ok(())
}

pub fn read_cursor_live_settings() -> Result<Value, AppError> {
    let path = get_cursor_live_provider_path();
    if !path.exists() {
        return Err(AppError::localized(
            "cursor.config.missing",
            "Cursor 配置文件不存在",
            "Cursor configuration file not found",
        ));
    }
    let text = fs::read_to_string(&path).map_err(|e| AppError::io(&path, e))?;
    serde_json::from_str(&text).map_err(|e| AppError::json(&path, e))
}

pub fn read_mcp_servers_map() -> Result<HashMap<String, Value>, AppError> {
    let path = get_cursor_mcp_path();
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let text = fs::read_to_string(&path).map_err(|e| AppError::io(&path, e))?;
    if text.trim().is_empty() {
        return Ok(HashMap::new());
    }
    let value: Value = serde_json::from_str(&text).map_err(|e| AppError::json(&path, e))?;
    let Some(map) = value.get("mcpServers").and_then(Value::as_object) else {
        return Ok(HashMap::new());
    };
    Ok(map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

pub fn set_mcp_servers_map(servers: &HashMap<String, Value>) -> Result<(), AppError> {
    let path = get_cursor_mcp_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    }
    let mut mcp_servers = Map::new();
    for (id, spec) in servers {
        mcp_servers.insert(id.clone(), spec.clone());
    }
    let document = json!({ "mcpServers": mcp_servers });
    write_json_file(&path, &document)
}

fn write_ide_byok(base_url: &str, api_key: &str, model: &str) -> Result<(), AppError> {
    let Some(db_path) = cursor_ide_state_db() else {
        return Ok(());
    };
    if !db_path.exists() {
        return Ok(());
    }
    write_item_table_byok(&db_path, base_url, api_key, model)
}

fn cursor_ide_state_db() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let appdata = std::env::var_os("APPDATA")?;
        Some(
            PathBuf::from(appdata)
                .join("Cursor")
                .join("User")
                .join("globalStorage")
                .join("state.vscdb"),
        )
    }
    #[cfg(target_os = "macos")]
    {
        Some(
            get_home_dir()
                .join("Library")
                .join("Application Support")
                .join("Cursor")
                .join("User")
                .join("globalStorage")
                .join("state.vscdb"),
        )
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Some(
            get_home_dir()
                .join(".config")
                .join("Cursor")
                .join("User")
                .join("globalStorage")
                .join("state.vscdb"),
        )
    }
}

fn write_item_table_byok(
    db_path: &Path,
    base_url: &str,
    api_key: &str,
    model: &str,
) -> Result<(), AppError> {
    let conn = rusqlite::Connection::open(db_path).map_err(|e| {
        AppError::Message(format!(
            "Cannot open Cursor state database {}: {e}",
            db_path.display()
        ))
    })?;
    let table_exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='ItemTable'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if table_exists == 0 {
        return Ok(());
    }

    let pairs: Vec<(&str, Value)> = vec![
        ("useOpenAIKey", json!(true)),
        ("openaiKey", json!(api_key)),
        ("openaiBaseUrl", json!(base_url)),
        ("openaiApiKey", json!(api_key)),
        ("cursorai/openaiKey", json!(api_key)),
        ("cursorai/openaiBaseUrl", json!(base_url)),
        ("cursorai/useOpenAIKey", json!(true)),
    ];

    let mut stmt = conn
        .prepare(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .map_err(|e| AppError::Message(format!("Cursor ItemTable upsert prepare failed: {e}")))?;
    for (key, value) in pairs {
        let encoded = serde_json::to_vec(&value).unwrap_or_default();
        stmt.execute(rusqlite::params![key, encoded])
            .map_err(|e| AppError::Message(format!("Cursor ItemTable write failed: {e}")))?;
    }
    if !model.is_empty() {
        let encoded = serde_json::to_vec(&json!(model)).unwrap_or_default();
        stmt.execute(rusqlite::params!["openaiModel", encoded])
            .map_err(|e| AppError::Message(format!("Cursor ItemTable write failed: {e}")))?;
    }
    Ok(())
}

pub(crate) fn should_sync_cursor_files() -> bool {
    cursor_dir_ready()
}

fn cursor_auth_json_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    #[cfg(windows)]
    if let Some(appdata) = std::env::var_os("APPDATA") {
        paths.push(PathBuf::from(appdata).join("Cursor").join("auth.json"));
    }
    #[cfg(target_os = "macos")]
    {
        paths.push(
            get_home_dir()
                .join("Library")
                .join("Application Support")
                .join("Cursor")
                .join("auth.json"),
        );
        paths.push(get_home_dir().join(".cursor").join("auth.json"));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        paths.push(
            get_home_dir()
                .join(".config")
                .join("Cursor")
                .join("auth.json"),
        );
        paths.push(get_home_dir().join(".cursor").join("auth.json"));
    }
    paths.push(get_cursor_dir().join("auth.json"));
    paths
}

fn token_from_json_value(value: &Value) -> Option<String> {
    value
        .get("accessToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

fn token_from_item_table_blob(raw: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?.trim();
    if text.starts_with("eyJ") {
        return (!text.is_empty()).then(|| text.to_string());
    }
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| match value {
            Value::String(token) => {
                let token = token.trim();
                (!token.is_empty()).then(|| token.to_string())
            }
            other => token_from_json_value(&other),
        })
}

fn read_token_from_auth_json() -> Option<String> {
    for path in cursor_auth_json_paths() {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if let Some(token) = token_from_json_value(&value) {
            return Some(token);
        }
    }
    None
}

fn read_token_from_state_vscdb() -> Option<String> {
    let path = cursor_ide_state_db()?;
    if !path.exists() {
        return None;
    }
    let uri = format!("file:{}?mode=ro", path.to_string_lossy().replace('\\', "/"));
    let conn = rusqlite::Connection::open_with_flags(
        &uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()?;
    let raw: Vec<u8> = conn
        .query_row(
            "SELECT value FROM ItemTable WHERE key = 'cursorAuth/accessToken'",
            [],
            |row| row.get(0),
        )
        .ok()?;
    token_from_item_table_blob(&raw)
}

/// Session token Cursor IDE already has locally. Used only to call
/// api2.cursor.sh DashboardService; never persisted by CC Switch.
pub fn read_cursor_access_token() -> Option<String> {
    read_token_from_auth_json().or_else(read_token_from_state_vscdb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn with_test_home<T>(f: impl FnOnce(&Path) -> T) -> T {
        let dir = tempdir().expect("tempdir");
        let original = std::env::var_os("CC_SWITCH_TEST_HOME");
        std::env::set_var("CC_SWITCH_TEST_HOME", dir.path());
        let result = f(dir.path());
        match original {
            Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
            None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
        }
        result
    }

    #[test]
    fn mcp_roundtrip_writes_mcp_servers() {
        with_test_home(|home| {
            let mut servers = HashMap::new();
            servers.insert(
                "filesystem".into(),
                json!({
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-filesystem"]
                }),
            );
            set_mcp_servers_map(&servers).expect("write mcp");
            let path = home.join(".cursor").join("mcp.json");
            assert!(path.exists());
            let read = read_mcp_servers_map().expect("read mcp");
            assert_eq!(read["filesystem"]["command"], "npx");
        });
    }

    #[test]
    fn live_provider_roundtrip_and_validation() {
        with_test_home(|_| {
            let provider = Provider::with_id(
                "deepseek".into(),
                "DeepSeek".into(),
                json!({
                    "env": {
                        "OPENAI_API_KEY": "sk-test",
                        "OPENAI_BASE_URL": "https://api.deepseek.com/v1",
                        "OPENAI_MODEL": "deepseek-chat"
                    }
                }),
                None,
            );
            write_cursor_provider_live(&provider).expect("write live");
            let live = read_cursor_live_settings().expect("read live");
            assert_eq!(live["env"]["OPENAI_API_KEY"], "sk-test");
            assert_eq!(live["ccSwitch"]["providerId"], "deepseek");
            let (base, key, model) = extract_openai_env(&live);
            assert_eq!(base, "https://api.deepseek.com/v1");
            assert_eq!(key, "sk-test");
            assert_eq!(model, "deepseek-chat");
        });
    }

    #[test]
    fn validate_rejects_missing_key() {
        let err = validate_provider_settings(&json!({
            "env": { "OPENAI_BASE_URL": "https://api.deepseek.com/v1" }
        }))
        .expect_err("missing key");
        assert!(err.to_string().contains("API"));
    }
}
