//! Cursor MCP sync and import.
//!
//! Cursor's documented user-level MCP file is `~/.cursor/mcp.json` with the
//! same `mcpServers` object Claude Code uses.

use serde_json::Value;
use std::collections::HashMap;

use crate::app_config::{McpApps, McpServer, MultiAppConfig};
use crate::cursor_config;
use crate::error::AppError;

use super::validation::validate_server_spec;

fn should_sync() -> bool {
    cursor_config::should_sync_cursor_files()
}

pub fn import_from_cursor(config: &mut MultiAppConfig) -> Result<usize, AppError> {
    if !should_sync() {
        return Ok(0);
    }
    let map = cursor_config::read_mcp_servers_map()?;
    if map.is_empty() {
        return Ok(0);
    }

    let servers = config
        .mcp
        .servers
        .get_or_insert_with(std::collections::HashMap::new);
    let mut changed = 0;
    for (id, spec) in map {
        if let Err(error) = validate_server_spec(&spec) {
            log::warn!("跳过无效 Cursor MCP 项 '{id}': {error}");
            continue;
        }
        if let Some(existing) = servers.get_mut(&id) {
            if !existing.apps.cursor {
                existing.apps.cursor = true;
                changed += 1;
            }
        } else {
            servers.insert(
                id.clone(),
                McpServer {
                    id: id.clone(),
                    name: id.clone(),
                    server: spec,
                    apps: McpApps {
                        cursor: true,
                        ..McpApps::default()
                    },
                    description: None,
                    homepage: None,
                    docs: None,
                    tags: Vec::new(),
                },
            );
            changed += 1;
        }
    }
    Ok(changed)
}

pub fn sync_single_server_to_cursor(
    _config: &MultiAppConfig,
    id: &str,
    server_spec: &Value,
) -> Result<(), AppError> {
    if !should_sync() {
        return Ok(());
    }
    let mut current = cursor_config::read_mcp_servers_map()?;
    current.insert(id.to_string(), server_spec.clone());
    cursor_config::set_mcp_servers_map(&current)
}

pub fn remove_server_from_cursor(id: &str) -> Result<(), AppError> {
    if !should_sync() {
        return Ok(());
    }
    let mut current = cursor_config::read_mcp_servers_map()?;
    current.remove(id);
    cursor_config::set_mcp_servers_map(&current)
}

#[allow(dead_code)]
pub fn sync_enabled_to_cursor(config: &MultiAppConfig) -> Result<(), AppError> {
    if !should_sync() {
        return Ok(());
    }
    let Some(servers) = config.mcp.servers.as_ref() else {
        return Ok(());
    };
    let mut enabled = HashMap::new();
    for (id, entry) in servers {
        if !entry.apps.cursor {
            continue;
        }
        if let Err(error) = validate_server_spec(&entry.server) {
            log::warn!("跳过无效 Cursor MCP 项 '{id}': {error}");
            continue;
        }
        enabled.insert(id.clone(), entry.server.clone());
    }
    cursor_config::set_mcp_servers_map(&enabled)
}
