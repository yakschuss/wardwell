use crate::config::loader::{self, config_dir};
use crate::install::detect;
use crate::install::mcp_config::{self, McpConfigPaths, RemoveResult};

/// Clean removal. Reverse of init.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    println!("wardwell uninstall\n");

    // 1. Remove MCP config entries
    let mcp_paths = McpConfigPaths::detect();

    print!("  Removing Claude Code MCP entry...   ");
    match mcp_config::remove_mcp_entry(&mcp_paths.claude_code) {
        Ok(RemoveResult::Removed) => println!("removed"),
        Ok(RemoveResult::NotFound) => println!("not found (ok)"),
        Err(e) => println!("error: {e}"),
    }

    print!("  Removing Desktop MCP entry...       ");
    match mcp_config::remove_mcp_entry(&mcp_paths.claude_desktop) {
        Ok(RemoveResult::Removed) => println!("removed"),
        Ok(RemoveResult::NotFound) => println!("not found (ok)"),
        Err(e) => println!("error: {e}"),
    }

    // 2. Remove CLAUDE.md markers
    let config = loader::load(Some(&config_dir().join("config.yml"))).ok();
    let domain_paths: Vec<String> = config
        .as_ref()
        .map(|c| {
            c.registry
                .all()
                .iter()
                .flat_map(|d| d.paths.iter().map(|p| p.as_str().to_string()))
                .collect()
        })
        .unwrap_or_default();

    let claude_md_files = detect::find_claude_md_files(&domain_paths);
    println!("  Removing CLAUDE.md markers...");
    for path in &claude_md_files {
        match remove_markers(path) {
            Ok(true) => println!("    cleaned {}", path.display()),
            Ok(false) => println!("    no markers in {}", path.display()),
            Err(e) => println!("    error {}: {e}", path.display()),
        }
    }

    // 3. Remove hooks from settings.json
    let home = dirs::home_dir().unwrap_or_default();
    let settings_path = home.join(".claude/settings.json");
    for event in &["SessionStart", "SessionEnd"] {
        print!("  Removing {event} hook...  ");
        match remove_hook(&settings_path, event) {
            Ok(true) => println!("removed"),
            Ok(false) => println!("not found (ok)"),
            Err(e) => println!("error: {e}"),
        }
    }

    // Also clean up legacy hook script if it exists
    let legacy_hook = home.join(".claude/hooks/wardwell-init.sh");
    if legacy_hook.exists() {
        let _ = std::fs::remove_file(&legacy_hook);
    }

    // 4. Remove generated databases (not user content)
    let index_db = config_dir().join("index.db");
    let sessions_db = config_dir().join("sessions.db");
    print!("  Removing index.db...                ");
    if index_db.exists() {
        match std::fs::remove_file(&index_db) {
            Ok(()) => println!("removed"),
            Err(e) => println!("error: {e}"),
        }
        // Also remove WAL/SHM files
        let _ = std::fs::remove_file(config_dir().join("index.db-wal"));
        let _ = std::fs::remove_file(config_dir().join("index.db-shm"));
    } else {
        println!("not found (ok)");
    }

    print!("  Removing sessions.db...             ");
    if sessions_db.exists() {
        match std::fs::remove_file(&sessions_db) {
            Ok(()) => println!("removed"),
            Err(e) => println!("error: {e}"),
        }
        let _ = std::fs::remove_file(config_dir().join("sessions.db-wal"));
        let _ = std::fs::remove_file(config_dir().join("sessions.db-shm"));
    } else {
        println!("not found (ok)");
    }

    println!();
    println!("  Removed MCP entries, hooks, markers, and databases.");
    println!("  Your vault and config preserved at {}.", config_dir().display());

    Ok(())
}

/// Remove wardwell hooks from a given event in settings.json.
fn remove_hook(settings_path: &std::path::Path, event: &str) -> Result<bool, std::io::Error> {
    if !settings_path.exists() {
        return Ok(false);
    }

    let content = std::fs::read_to_string(settings_path)?;
    let mut config: serde_json::Value = serde_json::from_str(&content)
        .unwrap_or_else(|_| serde_json::json!({}));

    let removed = if let Some(hooks) = config.get_mut("hooks")
        && let Some(event_hooks) = hooks.get_mut(event)
        && let Some(entries) = event_hooks.as_array_mut()
    {
        let before = entries.len();
        entries.retain(|entry| {
            let is_wardwell = entry.get("command").and_then(|c| c.as_str()).is_some_and(|c| c.contains("wardwell"))
                || entry.get("hooks").and_then(|h| h.as_array()).is_some_and(|hooks| {
                    hooks.iter().any(|h| {
                        h.get("command").and_then(|c| c.as_str()).is_some_and(|c| c.contains("wardwell"))
                    })
                });
            !is_wardwell
        });
        entries.len() < before
    } else {
        false
    };

    if removed {
        let json = serde_json::to_string_pretty(&config)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        std::fs::write(settings_path, json)?;
    }

    Ok(removed)
}

/// Remove wardwell markers and content between them from a CLAUDE.md file.
/// Returns true if markers were found and removed.
fn remove_markers(path: &std::path::Path) -> Result<bool, std::io::Error> {
    let content = std::fs::read_to_string(path)?;

    let start_marker = "<!-- wardwell:start -->";
    let end_marker = "<!-- wardwell:end -->";

    if let Some(start_pos) = content.find(start_marker)
        && let Some(end_pos) = content.find(end_marker)
    {
        let end_of_marker = end_pos + end_marker.len();

        let before = content[..start_pos].trim_end_matches('\n');
        let after = content[end_of_marker..].trim_start_matches('\n');

        let new_content = if before.is_empty() {
            after.to_string()
        } else if after.is_empty() {
            format!("{before}\n")
        } else {
            format!("{before}\n\n{after}")
        };

        std::fs::write(path, new_content)?;
        return Ok(true);
    }

    Ok(false)
}
