use crate::config::loader::{self, TrackerBinding};
use crate::install::detect;
use crate::install::installer;
use crate::tracker::schedule::{SystemRunner, current_uid};
use crate::install::mcp_config::{self, ChangeStatus, McpConfigPaths, ReconcileResult};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
enum Client {
    ClaudeCode,
    ClaudeDesktop,
    Codex,
}

impl Client {
    fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::ClaudeDesktop => "Claude Desktop",
            Self::Codex => "Codex",
        }
    }
}

/// Set up or repair this computer's agent configuration without reading or
/// changing the vault: MCP entries, then the installer's two tiers of hooks
/// and permissions, and the tracker pull service when a binding exists.
pub fn run(dry_run: bool, yes: bool) -> Result<(), Box<dyn std::error::Error>> {
    println!("wardwell setup\n");
    println!("  Scope: agent configuration on this computer only.");
    println!("  Vault files, briefs, and canonical context will not be changed.\n");

    let home = dirs::home_dir().ok_or("Could not find the home directory")?;
    let config_dir = loader::config_dir();
    let paths = McpConfigPaths::detect();
    let binary_path = detect::find_binary_path();
    let clients = detected_clients(&paths);
    let trackers = tracker_bindings(&config_dir)?;

    // Preflight every client and every settings file before asking for
    // consent or making any change. A malformed or conflicting file must not
    // leave a partial setup.
    let mut previews = Vec::new();
    for client in &clients {
        previews.push((*client, reconcile(*client, &paths, &binary_path, true)?));
    }
    let inputs = installer::Inputs {
        home: &home,
        config_dir: &config_dir,
        binary: &binary_path,
        trackers: &trackers,
        claude_code: clients.iter().any(|c| matches!(c, Client::ClaudeCode))
            || installer::settings_path(&home).exists(),
        launchd: cfg!(target_os = "macos"),
    };
    let plan = installer::plan(&inputs)?;

    if clients.is_empty() && plan.lines.is_empty() {
        println!("  No supported agent clients were detected; nothing changed.");
        return Ok(());
    }

    println!("  Proposed changes:");
    for (client, result) in &previews {
        print_plan(*client, result, path_for(*client, &paths));
    }
    for line in &plan.lines {
        println!("{}", line.render());
    }

    let mcp_unchanged = previews.iter().all(|(_, result)| result.status == ChangeStatus::Unchanged);
    if mcp_unchanged && plan.is_noop() {
        println!("\n  Nothing to change. Wardwell is already set up.");
        return Ok(());
    }

    if dry_run {
        println!("\n  Dry run complete. Nothing changed.");
        return Ok(());
    }

    if !yes && !confirm()? {
        println!("\n  Cancelled. Nothing changed.");
        return Ok(());
    }

    // The installer stages and renames its files first. Only when that phase
    // succeeds are the connection entries written, so "nothing was written"
    // is true when it says so, and no outcome prints before it.
    println!();
    let installed = match installer::apply(&plan, &SystemRunner, &current_uid) {
        Ok(lines) => lines,
        Err(failed) => {
            failed.lines.iter().for_each(|line| println!("{line}"));
            return Err(failed.message.into());
        }
    };
    for client in clients {
        let result = reconcile(client, &paths, &binary_path, false)?;
        print_applied(client, &result);
    }
    installed.iter().for_each(|line| println!("{line}"));

    println!();
    for note in plan.activation() {
        println!("  {note}");
    }
    println!("\n  Authorization still requires your consent:");
    if detect::command_available("codex") {
        println!("    Codex: run `codex mcp login wardwell`");
    }
    if detect::command_available("claude") {
        println!("    Claude: add Wardwell as an account connector in Settings > Connectors");
        println!("            URL: {}", mcp_config::REMOTE_URL);
    }
    println!("\n  Then run `wardwell doctor` and publish or refresh one brief as a live proof.");

    Ok(())
}

/// The tracker bindings in config.yml. No config means none; a config that
/// does not parse stops setup before anything changes.
pub(crate) fn tracker_bindings(config_dir: &Path) -> Result<Vec<TrackerBinding>, String> {
    let path = config_dir.join("config.yml");
    if !path.exists() {
        return Ok(Vec::new());
    }
    loader::load(Some(&path))
        .map(|config| config.trackers)
        .map_err(|error| format!("{} could not be read ({error}); no files changed", path.display()))
}

fn detected_clients(paths: &McpConfigPaths) -> Vec<Client> {
    let mut clients = Vec::new();
    if detect::command_available("claude") || paths.claude_code.exists() {
        clients.push(Client::ClaudeCode);
    }
    if Path::new("/Applications/Claude.app").exists() || paths.claude_desktop.exists() {
        clients.push(Client::ClaudeDesktop);
    }
    if detect::command_available("codex") || paths.codex.exists() {
        clients.push(Client::Codex);
    }
    clients
}

fn reconcile(
    client: Client,
    paths: &McpConfigPaths,
    binary_path: &Path,
    dry_run: bool,
) -> Result<ReconcileResult, io::Error> {
    match client {
        Client::ClaudeCode => {
            mcp_config::reconcile_claude_code(&paths.claude_code, binary_path, dry_run)
        }
        Client::ClaudeDesktop => {
            mcp_config::reconcile_claude_desktop(&paths.claude_desktop, binary_path, dry_run)
        }
        Client::Codex => mcp_config::reconcile_codex(&paths.codex, binary_path, dry_run),
    }
}

fn path_for(client: Client, paths: &McpConfigPaths) -> &PathBuf {
    match client {
        Client::ClaudeCode => &paths.claude_code,
        Client::ClaudeDesktop => &paths.claude_desktop,
        Client::Codex => &paths.codex,
    }
}

fn print_plan(client: Client, result: &ReconcileResult, path: &Path) {
    let action = match result.status {
        ChangeStatus::DryRunCreate => "CREATE",
        ChangeStatus::DryRunUpdate => "UPDATE + BACKUP",
        ChangeStatus::Unchanged => "UNCHANGED",
        ChangeStatus::Created | ChangeStatus::Updated => "UPDATE",
    };
    println!("    {action:<15} {} → {}", client.label(), path.display());
    if result.migrated_local_name {
        println!("                    migrate the local vault entry to `wardwell-context`");
    }
    if result.migrated_legacy_remote {
        println!(
            "                    replace the legacy hosted entry; copied credentials are removed"
        );
    }
}

fn print_applied(client: Client, result: &ReconcileResult) {
    let outcome = match result.status {
        ChangeStatus::Created => "configured",
        ChangeStatus::Updated => "updated",
        ChangeStatus::Unchanged => "already configured",
        ChangeStatus::DryRunCreate | ChangeStatus::DryRunUpdate => "previewed",
    };
    println!("  OK {}: {outcome}", client.label());
    if let Some(backup) = &result.backup_path {
        println!("    backup: {}", backup.display());
    }
}

fn confirm() -> Result<bool, io::Error> {
    print!("\n  Apply these changes? [Y/n] ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim();
    Ok(answer.is_empty() || answer.eq_ignore_ascii_case("y"))
}
