use std::path::{Path, PathBuf};

/// Paths to MCP config files for different Claude interfaces.
pub struct McpConfigPaths {
    pub claude_desktop: PathBuf,
    pub claude_code: PathBuf,
}

impl McpConfigPaths {
    pub fn detect() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        Self {
            claude_desktop: home.join("Library/Application Support/Claude/claude_desktop_config.json"),
            claude_code: home.join(".claude/settings.json"),
        }
    }
}

/// Inject wardwell MCP server entry into a JSON config file.
/// Preserves all existing entries. Only adds/updates the wardwell entry.
pub fn inject_mcp_entry(config_path: &Path, binary_path: &Path) -> Result<InjectResult, std::io::Error> {
    let wardwell_entry = serde_json::json!({
        "command": binary_path.to_string_lossy(),
        "args": ["serve"]
    });

    let mut config: serde_json::Value = if config_path.exists() {
        let content = std::fs::read_to_string(config_path)?;
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let mcp_servers = config
        .as_object_mut()
        .ok_or_else(|| std::io::Error::other("config is not a JSON object"))?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));

    let already_exists = mcp_servers
        .as_object()
        .is_some_and(|m| m.contains_key("wardwell"));

    mcp_servers
        .as_object_mut()
        .ok_or_else(|| std::io::Error::other("mcpServers is not a JSON object"))?
        .insert("wardwell".to_string(), wardwell_entry);

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let json = serde_json::to_string_pretty(&config)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    std::fs::write(config_path, json)?;

    Ok(if already_exists {
        InjectResult::Updated
    } else {
        InjectResult::Created
    })
}

/// Remove the wardwell entry from an MCP config file.
/// Preserves all other entries.
pub fn remove_mcp_entry(config_path: &Path) -> Result<RemoveResult, std::io::Error> {
    if !config_path.exists() {
        return Ok(RemoveResult::NotFound);
    }

    let content = std::fs::read_to_string(config_path)?;
    let mut config: serde_json::Value = serde_json::from_str(&content)
        .unwrap_or_else(|_| serde_json::json!({}));

    let removed = if let Some(obj) = config.as_object_mut() {
        if let Some(servers) = obj.get_mut("mcpServers") {
            if let Some(servers_obj) = servers.as_object_mut() {
                servers_obj.remove("wardwell").is_some()
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };

    if removed {
        let json = serde_json::to_string_pretty(&config)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        std::fs::write(config_path, json)?;
        Ok(RemoveResult::Removed)
    } else {
        Ok(RemoveResult::NotFound)
    }
}

/// Check if wardwell entry exists in an MCP config and what binary path it points to.
pub fn check_mcp_entry(config_path: &Path) -> McpEntryStatus {
    if !config_path.exists() {
        return McpEntryStatus::ConfigMissing;
    }

    let content = match std::fs::read_to_string(config_path) {
        Ok(c) => c,
        Err(_) => return McpEntryStatus::ConfigMissing,
    };

    let config: serde_json::Value = match serde_json::from_str(&content) {
        Ok(c) => c,
        Err(_) => return McpEntryStatus::ConfigMissing,
    };

    let entry = config
        .get("mcpServers")
        .and_then(|s| s.get("wardwell"));

    match entry {
        None => McpEntryStatus::NotConfigured,
        Some(entry) => {
            let command = entry
                .get("command")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            McpEntryStatus::Configured { binary_path: command }
        }
    }
}

#[derive(Debug)]
pub enum InjectResult {
    Created,
    Updated,
}

#[derive(Debug)]
pub enum RemoveResult {
    Removed,
    NotFound,
}

#[derive(Debug)]
pub enum McpEntryStatus {
    ConfigMissing,
    NotConfigured,
    Configured { binary_path: String },
}
