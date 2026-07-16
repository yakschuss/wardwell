use std::io::{Error, ErrorKind};
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde_json::{Map, Value, json};
use toml_edit::{Array, DocumentMut, Item, Table, Value as TomlValue, value};

pub const LOCAL_NAME: &str = "wardwell-context";
pub const REMOTE_NAME: &str = "wardwell";
pub const LEGACY_REMOTE_NAME: &str = "switchboard";
pub const REMOTE_URL: &str = "https://api.wardwell.app/mcp";

/// Configuration owned by agent clients on one computer.
pub struct McpConfigPaths {
    pub claude_desktop: PathBuf,
    pub claude_code: PathBuf,
    pub codex: PathBuf,
}

impl McpConfigPaths {
    pub fn detect() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        Self::for_home(&home)
    }

    fn for_home(home: &Path) -> Self {
        Self {
            claude_desktop: home
                .join("Library/Application Support/Claude/claude_desktop_config.json"),
            // User-scoped Claude Code MCP servers live here. `.claude/settings.json`
            // owns hooks and permissions, not MCP server definitions.
            claude_code: home.join(".claude.json"),
            codex: home.join(".codex/config.toml"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeStatus {
    Created,
    Updated,
    Unchanged,
    DryRunCreate,
    DryRunUpdate,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ReconcileResult {
    pub status: ChangeStatus,
    pub backup_path: Option<PathBuf>,
    pub migrated_legacy_remote: bool,
    pub migrated_local_name: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RemovalResult {
    pub removed: bool,
    pub backup_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryStatus {
    ConfigMissing,
    ParseError,
    Missing,
    Configured,
    WrongTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientStatus {
    pub local: EntryStatus,
    pub remote: EntryStatus,
    pub legacy_remote: bool,
}

/// Reconcile Claude Code's user configuration. The local vault server and the
/// hosted app are deliberately separate entries; neither owns the other's data.
pub fn reconcile_claude_code(
    config_path: &Path,
    binary_path: &Path,
    dry_run: bool,
) -> Result<ReconcileResult, Error> {
    reconcile_json(config_path, binary_path, true, dry_run)
}

/// Claude Desktop may run the local context server from its machine config.
/// Hosted remote connectors are account-owned and must be added through Claude's
/// connector UI, so legacy proxy entries are removed rather than perpetuated.
pub fn reconcile_claude_desktop(
    config_path: &Path,
    binary_path: &Path,
    dry_run: bool,
) -> Result<ReconcileResult, Error> {
    reconcile_json(config_path, binary_path, false, dry_run)
}

pub fn reconcile_codex(
    config_path: &Path,
    binary_path: &Path,
    dry_run: bool,
) -> Result<ReconcileResult, Error> {
    let previous = read_optional(config_path)?;
    let mut document = match previous.as_deref() {
        Some(content) => content.parse::<DocumentMut>().map_err(invalid_config)?,
        None => DocumentMut::new(),
    };

    if !document.contains_key("mcp_servers") {
        document["mcp_servers"] = Item::Table(Table::new());
    }
    let servers = document["mcp_servers"]
        .as_table_mut()
        .ok_or_else(|| invalid_data("mcp_servers is not a TOML table"))?;

    let mut migrated_local_name = false;
    let mut migrated_legacy_remote = false;

    if let Some(current) = servers.get(REMOTE_NAME) {
        if is_local_toml(current) {
            if servers.contains_key(LOCAL_NAME) {
                return Err(invalid_data(
                    "both wardwell and wardwell-context define local MCP servers",
                ));
            }
            let local = servers
                .remove(REMOTE_NAME)
                .ok_or_else(|| invalid_data("could not move the local Wardwell MCP entry"))?;
            servers.insert(LOCAL_NAME, local);
            migrated_local_name = true;
        } else if !is_remote_toml(current) {
            return Err(invalid_data(
                "the Codex MCP name `wardwell` is owned by another server",
            ));
        }
    }

    match servers.get_mut(LOCAL_NAME) {
        Some(item) if is_local_toml(item) => update_local_toml(item, binary_path)?,
        Some(_) => {
            return Err(invalid_data(
                "the Codex MCP name `wardwell-context` is owned by another server",
            ));
        }
        None => {
            servers.insert(LOCAL_NAME, local_toml_entry(binary_path));
        }
    }

    if servers.get(LEGACY_REMOTE_NAME).is_some_and(is_remote_toml) {
        servers.remove(LEGACY_REMOTE_NAME);
        migrated_legacy_remote = true;
    }

    match servers.get_mut(REMOTE_NAME) {
        Some(item) if is_remote_toml(item) => sanitize_remote_toml(item)?,
        Some(_) => {
            return Err(invalid_data(
                "the Codex MCP name `wardwell` is owned by another server",
            ));
        }
        None => {
            servers.insert(REMOTE_NAME, remote_toml_entry());
        }
    }

    let rendered = document.to_string();
    let (status, backup_path) =
        write_if_changed(config_path, previous.as_deref(), &rendered, dry_run)?;
    Ok(ReconcileResult {
        status,
        backup_path,
        migrated_legacy_remote,
        migrated_local_name,
    })
}

pub fn inspect_claude_code(config_path: &Path, binary_path: &Path) -> ClientStatus {
    inspect_json(config_path, binary_path, true)
}

pub fn inspect_claude_desktop(config_path: &Path, binary_path: &Path) -> ClientStatus {
    inspect_json(config_path, binary_path, false)
}

pub fn inspect_codex(config_path: &Path, binary_path: &Path) -> ClientStatus {
    let content = match std::fs::read_to_string(config_path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => return missing_client_status(),
        Err(_) => return parse_error_client_status(),
    };
    let document = match content.parse::<DocumentMut>() {
        Ok(document) => document,
        Err(_) => return parse_error_client_status(),
    };
    let Some(servers) = document.get("mcp_servers").and_then(Item::as_table) else {
        return ClientStatus {
            local: EntryStatus::Missing,
            remote: EntryStatus::Missing,
            legacy_remote: false,
        };
    };

    ClientStatus {
        local: inspect_local_toml(servers.get(LOCAL_NAME), binary_path),
        remote: inspect_remote_toml(servers.get(REMOTE_NAME)),
        legacy_remote: servers.get(LEGACY_REMOTE_NAME).is_some_and(is_remote_toml),
    }
}

/// Remove only Wardwell-owned JSON entries. Unrelated MCPs and the config file
/// itself are always preserved, and a backup precedes any changed write.
pub fn remove_owned_json_entries(
    config_path: &Path,
    include_remote: bool,
) -> Result<RemovalResult, Error> {
    let Some(previous) = read_optional(config_path)? else {
        return Ok(RemovalResult {
            removed: false,
            backup_path: None,
        });
    };
    let mut config = serde_json::from_str::<Value>(&previous).map_err(invalid_config)?;
    let root = config
        .as_object_mut()
        .ok_or_else(|| invalid_data("config is not a JSON object"))?;
    let Some(servers) = root.get_mut("mcpServers").and_then(Value::as_object_mut) else {
        return Ok(RemovalResult {
            removed: false,
            backup_path: None,
        });
    };

    let mut removed = remove_json_if(servers, LOCAL_NAME, is_local_json);
    // Pre-migration installs used `wardwell` for the local server.
    removed |= remove_json_if(servers, REMOTE_NAME, is_local_json);
    if include_remote {
        removed |= remove_json_if(servers, REMOTE_NAME, is_remote_json);
        removed |= remove_json_if(servers, LEGACY_REMOTE_NAME, is_remote_json);
    }
    if !removed {
        return Ok(RemovalResult {
            removed: false,
            backup_path: None,
        });
    }

    let rendered = format!(
        "{}\n",
        serde_json::to_string_pretty(&config).map_err(|error| Error::other(error.to_string()))?
    );
    let (_, backup_path) = write_if_changed(config_path, Some(&previous), &rendered, false)?;
    Ok(RemovalResult {
        removed: true,
        backup_path,
    })
}

pub fn remove_owned_codex_entries(config_path: &Path) -> Result<RemovalResult, Error> {
    let Some(previous) = read_optional(config_path)? else {
        return Ok(RemovalResult {
            removed: false,
            backup_path: None,
        });
    };
    let mut document = previous.parse::<DocumentMut>().map_err(invalid_config)?;
    let Some(servers) = document.get_mut("mcp_servers").and_then(Item::as_table_mut) else {
        return Ok(RemovalResult {
            removed: false,
            backup_path: None,
        });
    };

    let mut removed = remove_toml_if(servers, LOCAL_NAME, is_local_toml);
    removed |= remove_toml_if(servers, REMOTE_NAME, is_local_toml);
    removed |= remove_toml_if(servers, REMOTE_NAME, is_remote_toml);
    removed |= remove_toml_if(servers, LEGACY_REMOTE_NAME, is_remote_toml);
    if !removed {
        return Ok(RemovalResult {
            removed: false,
            backup_path: None,
        });
    }

    let rendered = document.to_string();
    let (_, backup_path) = write_if_changed(config_path, Some(&previous), &rendered, false)?;
    Ok(RemovalResult {
        removed: true,
        backup_path,
    })
}

fn reconcile_json(
    config_path: &Path,
    binary_path: &Path,
    include_remote: bool,
    dry_run: bool,
) -> Result<ReconcileResult, Error> {
    let previous = read_optional(config_path)?;
    let mut config = match previous.as_deref() {
        Some(content) => serde_json::from_str::<Value>(content).map_err(invalid_config)?,
        None => json!({}),
    };
    let root = config
        .as_object_mut()
        .ok_or_else(|| invalid_data("config is not a JSON object"))?;
    let servers_value = root
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    let servers = servers_value
        .as_object_mut()
        .ok_or_else(|| invalid_data("mcpServers is not a JSON object"))?;

    let mut migrated_local_name = false;
    let mut migrated_legacy_remote = false;

    if let Some(current) = servers.get(REMOTE_NAME) {
        if is_local_json(current) {
            if servers.contains_key(LOCAL_NAME) {
                return Err(invalid_data(
                    "both wardwell and wardwell-context define local MCP servers",
                ));
            }
            let local = servers
                .remove(REMOTE_NAME)
                .ok_or_else(|| invalid_data("could not move the local Wardwell MCP entry"))?;
            servers.insert(LOCAL_NAME.to_string(), local);
            migrated_local_name = true;
        } else if !is_remote_json(current) {
            return Err(invalid_data(
                "the MCP name `wardwell` is owned by another server",
            ));
        }
    }

    match servers.get(LOCAL_NAME) {
        Some(item) if !is_local_json(item) => {
            return Err(invalid_data(
                "the MCP name `wardwell-context` is owned by another server",
            ));
        }
        _ => {
            servers.insert(LOCAL_NAME.to_string(), local_json_entry(binary_path));
        }
    }

    if servers.get(LEGACY_REMOTE_NAME).is_some_and(is_remote_json) {
        servers.remove(LEGACY_REMOTE_NAME);
        migrated_legacy_remote = true;
    }

    if include_remote {
        match servers.get(REMOTE_NAME) {
            Some(item) if !is_remote_json(item) => {
                return Err(invalid_data(
                    "the MCP name `wardwell` is owned by another server",
                ));
            }
            _ => {
                // Replacing the complete owned entry guarantees that copied bearer
                // headers and proxy environment secrets do not survive migration.
                servers.insert(REMOTE_NAME.to_string(), remote_json_entry());
            }
        }
    }

    let rendered =
        serde_json::to_string_pretty(&config).map_err(|error| Error::other(error.to_string()))?;
    let rendered = format!("{rendered}\n");
    let (status, backup_path) =
        write_if_changed(config_path, previous.as_deref(), &rendered, dry_run)?;

    Ok(ReconcileResult {
        status,
        backup_path,
        migrated_legacy_remote,
        migrated_local_name,
    })
}

fn inspect_json(config_path: &Path, binary_path: &Path, include_remote: bool) -> ClientStatus {
    let content = match std::fs::read_to_string(config_path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => return missing_client_status(),
        Err(_) => return parse_error_client_status(),
    };
    let config = match serde_json::from_str::<Value>(&content) {
        Ok(config) => config,
        Err(_) => return parse_error_client_status(),
    };
    let servers = config.get("mcpServers").and_then(Value::as_object);
    let Some(servers) = servers else {
        return ClientStatus {
            local: EntryStatus::Missing,
            remote: EntryStatus::Missing,
            legacy_remote: false,
        };
    };

    ClientStatus {
        local: inspect_local_json(servers.get(LOCAL_NAME), binary_path),
        remote: if include_remote {
            inspect_remote_json(servers.get(REMOTE_NAME))
        } else {
            EntryStatus::Configured
        },
        legacy_remote: servers.get(LEGACY_REMOTE_NAME).is_some_and(is_remote_json),
    }
}

fn read_optional(path: &Path) -> Result<Option<String>, Error> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn write_if_changed(
    path: &Path,
    previous: Option<&str>,
    rendered: &str,
    dry_run: bool,
) -> Result<(ChangeStatus, Option<PathBuf>), Error> {
    if previous == Some(rendered) {
        return Ok((ChangeStatus::Unchanged, None));
    }
    if dry_run {
        return Ok((
            if previous.is_some() {
                ChangeStatus::DryRunUpdate
            } else {
                ChangeStatus::DryRunCreate
            },
            None,
        ));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let backup_path = if path.exists() {
        let backup = backup_path(path);
        std::fs::copy(path, &backup)?;
        Some(backup)
    } else {
        None
    };

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config");
    let temporary = path.with_file_name(format!(
        ".{file_name}.wardwell-{}.tmp",
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&temporary, rendered)?;
    if let Ok(metadata) = std::fs::metadata(path) {
        std::fs::set_permissions(&temporary, metadata.permissions())?;
    }
    std::fs::rename(&temporary, path)?;

    Ok((
        if previous.is_some() {
            ChangeStatus::Updated
        } else {
            ChangeStatus::Created
        },
        backup_path,
    ))
}

fn backup_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config");
    let stamp = Utc::now().format("%Y%m%dT%H%M%S%.6fZ");
    path.with_file_name(format!("{name}.wardwell-backup-{stamp}"))
}

fn local_json_entry(binary_path: &Path) -> Value {
    json!({
        "command": binary_path.to_string_lossy(),
        "args": ["serve"]
    })
}

fn remote_json_entry() -> Value {
    json!({
        "type": "http",
        "url": REMOTE_URL
    })
}

fn local_toml_entry(binary_path: &Path) -> Item {
    let mut table = Table::new();
    table["command"] = value(binary_path.to_string_lossy().to_string());
    let mut args = Array::new();
    args.push("serve");
    table["args"] = Item::Value(TomlValue::Array(args));
    Item::Table(table)
}

fn remote_toml_entry() -> Item {
    let mut table = Table::new();
    table["url"] = value(REMOTE_URL);
    Item::Table(table)
}

fn update_local_toml(item: &mut Item, binary_path: &Path) -> Result<(), Error> {
    let table = item
        .as_table_mut()
        .ok_or_else(|| invalid_data("local Wardwell MCP entry is not a table"))?;
    table["command"] = value(binary_path.to_string_lossy().to_string());
    let mut args = Array::new();
    args.push("serve");
    table["args"] = Item::Value(TomlValue::Array(args));
    Ok(())
}

fn sanitize_remote_toml(item: &mut Item) -> Result<(), Error> {
    let table = item
        .as_table_mut()
        .ok_or_else(|| invalid_data("hosted Wardwell MCP entry is not a table"))?;
    for key in [
        "command",
        "args",
        "env",
        "env_vars",
        "http_headers",
        "env_http_headers",
        "bearer_token_env_var",
    ] {
        table.remove(key);
    }
    table["url"] = value(REMOTE_URL);
    Ok(())
}

fn is_local_json(item: &Value) -> bool {
    item.get("command")
        .and_then(Value::as_str)
        .is_some_and(is_wardwell_binary)
}

fn is_remote_json(item: &Value) -> bool {
    item.get("url").and_then(Value::as_str) == Some(REMOTE_URL)
        || item
            .get("args")
            .and_then(Value::as_array)
            .is_some_and(|args| args.iter().any(|arg| arg.as_str() == Some(REMOTE_URL)))
}

fn is_local_toml(item: &Item) -> bool {
    item.get("command")
        .and_then(Item::as_str)
        .is_some_and(is_wardwell_binary)
}

fn is_remote_toml(item: &Item) -> bool {
    item.get("url").and_then(Item::as_str) == Some(REMOTE_URL)
}

fn is_wardwell_binary(command: &str) -> bool {
    Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        == Some("wardwell")
}

fn remove_json_if(
    servers: &mut Map<String, Value>,
    name: &str,
    predicate: fn(&Value) -> bool,
) -> bool {
    if servers.get(name).is_some_and(predicate) {
        servers.remove(name);
        true
    } else {
        false
    }
}

fn remove_toml_if(servers: &mut Table, name: &str, predicate: fn(&Item) -> bool) -> bool {
    if servers.get(name).is_some_and(predicate) {
        servers.remove(name);
        true
    } else {
        false
    }
}

fn inspect_local_json(item: Option<&Value>, binary_path: &Path) -> EntryStatus {
    match item {
        None => EntryStatus::Missing,
        Some(item) if !is_local_json(item) => EntryStatus::WrongTarget,
        Some(item)
            if item.get("command").and_then(Value::as_str)
                == Some(binary_path.to_string_lossy().as_ref()) =>
        {
            EntryStatus::Configured
        }
        Some(_) => EntryStatus::WrongTarget,
    }
}

fn inspect_remote_json(item: Option<&Value>) -> EntryStatus {
    match item {
        None => EntryStatus::Missing,
        Some(item) if is_remote_json(item) && item.get("headers").is_none() => {
            EntryStatus::Configured
        }
        Some(_) => EntryStatus::WrongTarget,
    }
}

fn inspect_local_toml(item: Option<&Item>, binary_path: &Path) -> EntryStatus {
    match item {
        None => EntryStatus::Missing,
        Some(item) if !is_local_toml(item) => EntryStatus::WrongTarget,
        Some(item)
            if item.get("command").and_then(Item::as_str)
                == Some(binary_path.to_string_lossy().as_ref()) =>
        {
            EntryStatus::Configured
        }
        Some(_) => EntryStatus::WrongTarget,
    }
}

fn inspect_remote_toml(item: Option<&Item>) -> EntryStatus {
    match item {
        None => EntryStatus::Missing,
        Some(item) if is_remote_toml(item) => EntryStatus::Configured,
        Some(_) => EntryStatus::WrongTarget,
    }
}

fn missing_client_status() -> ClientStatus {
    ClientStatus {
        local: EntryStatus::ConfigMissing,
        remote: EntryStatus::ConfigMissing,
        legacy_remote: false,
    }
}

fn parse_error_client_status() -> ClientStatus {
    ClientStatus {
        local: EntryStatus::ParseError,
        remote: EntryStatus::ParseError,
        legacy_remote: false,
    }
}

fn invalid_config(error: impl std::fmt::Display) -> Error {
    invalid_data(format!(
        "refusing to replace malformed configuration: {error}"
    ))
}

fn invalid_data(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn backups_for(path: &Path) -> Vec<PathBuf> {
        let parent = path.parent().unwrap();
        let prefix = format!(
            "{}.wardwell-backup-",
            path.file_name().unwrap().to_string_lossy()
        );
        std::fs::read_dir(parent)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|candidate| {
                candidate
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&prefix))
            })
            .collect()
    }

    #[test]
    fn claude_code_reconciles_legacy_entries_without_preserving_static_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        std::fs::write(
            &path,
            r#"{
  "theme": "dark",
  "mcpServers": {
    "wardwell": {"command":"/old/wardwell","args":["serve"]},
    "switchboard": {"type":"http","url":"https://api.wardwell.app/mcp","headers":{"Authorization":"Bearer secret"}},
    "other": {"command":"other-server"}
  }
}"#,
        )
        .unwrap();

        let result = reconcile_claude_code(&path, Path::new("/new/wardwell"), false).unwrap();
        assert_eq!(result.status, ChangeStatus::Updated);
        assert!(result.migrated_local_name);
        assert!(result.migrated_legacy_remote);
        assert!(
            result
                .backup_path
                .as_ref()
                .is_some_and(|backup| backup.exists())
        );

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["theme"], "dark");
        assert_eq!(written["mcpServers"]["other"]["command"], "other-server");
        assert_eq!(
            written["mcpServers"][LOCAL_NAME]["command"],
            "/new/wardwell"
        );
        assert_eq!(written["mcpServers"][REMOTE_NAME]["url"], REMOTE_URL);
        assert!(written["mcpServers"].get(LEGACY_REMOTE_NAME).is_none());
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("Bearer secret")
        );
    }

    #[test]
    fn claude_code_rerun_is_byte_identical_and_creates_no_second_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        let binary = Path::new("/opt/homebrew/bin/wardwell");

        reconcile_claude_code(&path, binary, false).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        let result = reconcile_claude_code(&path, binary, false).unwrap();

        assert_eq!(result.status, ChangeStatus::Unchanged);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
        assert!(backups_for(&path).is_empty());
    }

    #[test]
    fn malformed_json_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        std::fs::write(&path, "{broken").unwrap();

        let error = reconcile_claude_code(&path, Path::new("wardwell"), false).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{broken");
        assert!(backups_for(&path).is_empty());
    }

    #[test]
    fn claude_desktop_keeps_other_servers_and_does_not_install_remote_mcp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude_desktop_config.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"other":{"command":"other"},"switchboard":{"command":"npx","args":["mcp-remote","https://api.wardwell.app/mcp"]}}}"#,
        )
        .unwrap();

        reconcile_claude_desktop(&path, Path::new("/bin/wardwell"), false).unwrap();
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["mcpServers"]["other"]["command"], "other");
        assert!(written["mcpServers"].get(REMOTE_NAME).is_none());
        assert!(written["mcpServers"].get(LEGACY_REMOTE_NAME).is_none());
        assert_eq!(
            written["mcpServers"][LOCAL_NAME]["command"],
            "/bin/wardwell"
        );
    }

    #[test]
    fn codex_reconciliation_preserves_comments_and_nested_tool_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"# personal setting
model = "gpt"

[mcp_servers.other]
command = "other"

[mcp_servers.wardwell]
command = "/old/wardwell"
args = ["serve"]

[mcp_servers.wardwell.tools.wardwell_search]
approval_mode = "approve"
"#,
        )
        .unwrap();

        let result = reconcile_codex(&path, Path::new("/new/wardwell"), false).unwrap();
        assert!(result.migrated_local_name);
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("# personal setting"));
        assert!(written.contains("[mcp_servers.other]"));
        assert!(written.contains("[mcp_servers.wardwell-context.tools.wardwell_search]"));
        assert!(written.contains("approval_mode = \"approve\""));

        let document = written.parse::<DocumentMut>().unwrap();
        assert_eq!(
            document["mcp_servers"][LOCAL_NAME]["command"].as_str(),
            Some("/new/wardwell")
        );
        assert_eq!(
            document["mcp_servers"][REMOTE_NAME]["url"].as_str(),
            Some(REMOTE_URL)
        );
    }

    #[test]
    fn dry_run_never_creates_or_changes_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("config.toml");
        let result = reconcile_codex(&missing, Path::new("wardwell"), true).unwrap();
        assert_eq!(result.status, ChangeStatus::DryRunCreate);
        assert!(!missing.exists());

        std::fs::write(&missing, "model = \"gpt\"\n").unwrap();
        let before = std::fs::read_to_string(&missing).unwrap();
        let result = reconcile_codex(&missing, Path::new("wardwell"), true).unwrap();
        assert_eq!(result.status, ChangeStatus::DryRunUpdate);
        assert_eq!(std::fs::read_to_string(&missing).unwrap(), before);
        assert!(backups_for(&missing).is_empty());
    }

    #[test]
    fn conflicting_owned_name_aborts_without_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        let original = r#"{"mcpServers":{"wardwell":{"command":"someone-else"}}}"#;
        std::fs::write(&path, original).unwrap();

        let error = reconcile_claude_code(&path, Path::new("wardwell"), false).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn path_detection_uses_real_client_config_locations() {
        let paths = McpConfigPaths::for_home(Path::new("/Users/example"));
        assert_eq!(paths.claude_code, Path::new("/Users/example/.claude.json"));
        assert_eq!(paths.codex, Path::new("/Users/example/.codex/config.toml"));
    }

    #[test]
    fn removal_preserves_unrelated_json_and_creates_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"wardwell-context":{"command":"wardwell","args":["serve"]},"wardwell":{"type":"http","url":"https://api.wardwell.app/mcp"},"other":{"command":"other"}}}"#,
        )
        .unwrap();

        let result = remove_owned_json_entries(&path, true).unwrap();
        assert!(result.removed);
        assert!(
            result
                .backup_path
                .as_ref()
                .is_some_and(|backup| backup.exists())
        );
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["mcpServers"]["other"]["command"], "other");
        assert!(written["mcpServers"].get(LOCAL_NAME).is_none());
        assert!(written["mcpServers"].get(REMOTE_NAME).is_none());
    }
}
