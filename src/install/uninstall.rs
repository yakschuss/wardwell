use crate::config::loader::{self, config_dir};
use crate::install::detect;
use crate::install::installer;
use crate::tracker::schedule::{SystemRunner, current_uid};
use crate::install::mcp_config::{self, McpConfigPaths, RemovalResult};

/// Removes only Wardwell's entries, never the Wardwell folder. The summary
/// is built from what happened; any failed step makes the command fail.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    println!("wardwell uninstall\n");
    let mut report = Report::default();

    // 1. Remove connection entries
    let mcp_paths = McpConfigPaths::detect();
    report.removal("Claude Code connection entries", mcp_config::remove_owned_json_entries(&mcp_paths.claude_code, true));
    report.removal("Claude Desktop connection entries", mcp_config::remove_owned_json_entries(&mcp_paths.claude_desktop, true));
    report.removal("Codex connection entries", mcp_config::remove_owned_codex_entries(&mcp_paths.codex));

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
    println!("  Removing CLAUDE.md markers...");
    for path in &detect::find_claude_md_files(&domain_paths) {
        match remove_markers(path) {
            Ok(true) => {
                println!("    cleaned {}", path.display());
                report.done.push(format!("CLAUDE.md markers in {}", path.display()));
            }
            Ok(false) => println!("    no markers in {}", path.display()),
            Err(e) => {
                println!("    error {}: {e}", path.display());
                report.failed.push(format!("CLAUDE.md markers in {}: {e}", path.display()));
            }
        }
    }

    // 3. Remove Wardwell's hooks, the deny entries it added, and the pull
    // service, matched exactly, with a backup. The Wardwell folder stays.
    let home = dirs::home_dir().unwrap_or_default();
    println!("  Removing hooks, Wardwell's deny entries, and the tracker pull...");
    let plan = installer::uninstall_plan(&home, &config_dir());
    for line in &plan.lines {
        println!("{}", line.render());
    }
    report.failed.extend(plan.failures.iter().cloned());
    match installer::apply(&plan, &SystemRunner, &current_uid) {
        Ok(lines) => {
            lines.iter().for_each(|line| println!("{line}"));
            report.done.extend(plan.removals());
        }
        Err(failed) => {
            failed.lines.iter().for_each(|line| println!("{line}"));
            println!("    unchanged: {}", failed.message);
            report.failed.push(failed.message);
        }
    }

    // Also clean up legacy hook script if it exists
    let legacy_hook = home.join(".claude/hooks/wardwell-init.sh");
    if legacy_hook.exists() {
        let _ = std::fs::remove_file(&legacy_hook);
    }

    // 4. Remove generated databases (not user content)
    for name in ["index.db", "sessions.db"] {
        report.database(name);
    }

    report.finish()
}

/// What uninstall did and what failed, for the closing summary.
#[derive(Default)]
struct Report {
    done: Vec<String>,
    failed: Vec<String>,
}

impl Report {
    fn removal(&mut self, label: &str, result: Result<RemovalResult, std::io::Error>) {
        if let Some(error) = print_removal(label, &result) {
            self.failed.push(format!("{label}: {error}"));
        } else if result.is_ok_and(|r| r.removed) {
            self.done.push(label.to_string());
        }
    }

    fn database(&mut self, name: &str) {
        let path = config_dir().join(name);
        print!("  Removing {name}...{:width$}", "", width = 20usize.saturating_sub(name.len()));
        if !path.exists() {
            println!("not found (ok)");
            return;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                println!("removed");
                self.done.push(name.to_string());
            }
            Err(e) => {
                println!("error: {e}");
                self.failed.push(format!("{name}: {e}"));
            }
        }
        let _ = std::fs::remove_file(config_dir().join(format!("{name}-wal")));
        let _ = std::fs::remove_file(config_dir().join(format!("{name}-shm")));
    }

    fn finish(self) -> Result<(), Box<dyn std::error::Error>> {
        println!();
        match self.done.is_empty() {
            true => println!("  Removed nothing; no Wardwell entries were found to remove."),
            false => println!("  Removed: {}.", self.done.join(", ")),
        }
        println!("  Sessions already running keep their hooks until they end.");
        println!("  Your vault and config preserved at {}.", config_dir().display());
        if self.failed.is_empty() {
            return Ok(());
        }
        println!("\n  Uninstall did not finish. These steps failed:");
        for failure in &self.failed {
            println!("    - {failure}");
        }
        Err(format!("uninstall did not finish: {} step(s) failed", self.failed.len()).into())
    }
}

/// Print one connection-removal outcome; the error text when it failed.
fn print_removal(label: &str, result: &Result<RemovalResult, std::io::Error>) -> Option<String> {
    print!("  Removing {label}...  ");
    match result {
        Ok(RemovalResult { removed: true, backup_path }) => {
            println!("removed");
            if let Some(backup) = backup_path {
                println!("    backup: {}", backup.display());
            }
            None
        }
        Ok(RemovalResult { removed: false, .. }) => {
            println!("not found (ok)");
            None
        }
        Err(error) => {
            println!("unchanged: {error}");
            Some(error.to_string())
        }
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
