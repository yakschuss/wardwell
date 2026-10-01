use crate::config::loader::{self, config_dir};
use crate::install::detect;
use crate::install::installer;
use crate::tracker::schedule::{SystemRunner, current_uid};
use crate::install::mcp_config::{self, McpConfigPaths, RemovalResult};

/// Clean removal. Reverse of init.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    println!("wardwell uninstall\n");

    // 1. Remove MCP config entries
    let mcp_paths = McpConfigPaths::detect();

    print_removal(
        "Claude Code connection entries",
        mcp_config::remove_owned_json_entries(&mcp_paths.claude_code, true),
    );
    print_removal(
        "Claude Desktop connection entries",
        mcp_config::remove_owned_json_entries(&mcp_paths.claude_desktop, true),
    );
    print_removal(
        "Codex connection entries",
        mcp_config::remove_owned_codex_entries(&mcp_paths.codex),
    );

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

    // 3. Remove Wardwell's hooks, the deny entries it added, and the pull
    // service, matched exactly, with a backup. The Wardwell folder stays.
    let home = dirs::home_dir().unwrap_or_default();
    println!("  Removing hooks, Wardwell's deny entries, and the tracker pull...");
    match installer::uninstall_plan(&home, &config_dir()) {
        Ok(plan) => {
            for line in &plan.lines {
                println!("{}", line.render());
            }
            match installer::apply(&plan, &SystemRunner, &current_uid) {
                Ok(report) => report.iter().for_each(|line| println!("{line}")),
                Err(failed) => {
                    failed.lines.iter().for_each(|line| println!("{line}"));
                    println!("    unchanged: {}", failed.message);
                }
            }
        }
        Err(error) => println!("    unchanged: {error}"),
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
    println!("  Removed connection entries, hooks, deny entries Wardwell added, the tracker pull, markers, and databases.");
    println!("  Sessions already running keep their hooks until they end.");
    println!(
        "  Your vault and config preserved at {}.",
        config_dir().display()
    );

    Ok(())
}

fn print_removal(label: &str, result: Result<RemovalResult, std::io::Error>) {
    print!("  Removing {label}...  ");
    match result {
        Ok(RemovalResult {
            removed: true,
            backup_path,
        }) => {
            println!("removed");
            if let Some(backup) = backup_path {
                println!("    backup: {}", backup.display());
            }
        }
        Ok(RemovalResult { removed: false, .. }) => println!("not found (ok)"),
        Err(error) => println!("unchanged: {error}"),
    }
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
