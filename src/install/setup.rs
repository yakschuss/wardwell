use crate::install::detect;
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

/// Repair agent MCP configuration without reading or changing the vault.
pub fn run(dry_run: bool, yes: bool) -> Result<(), Box<dyn std::error::Error>> {
    println!("wardwell setup\n");
    println!("  Scope: agent MCP configuration on this computer only.");
    println!("  Vault files, briefs, and canonical context will not be changed.\n");

    let paths = McpConfigPaths::detect();
    let binary_path = detect::find_binary_path();
    let clients = detected_clients(&paths);

    if clients.is_empty() {
        println!("  No supported agent clients were detected; nothing changed.");
        return Ok(());
    }

    // Preflight every client before asking for consent or making any changes.
    // A malformed or conflicting late config must not leave a partial setup.
    let mut previews = Vec::new();
    for client in &clients {
        previews.push((*client, reconcile(*client, &paths, &binary_path, true)?));
    }

    println!("  Proposed changes:");
    for (client, result) in &previews {
        print_plan(*client, result, path_for(*client, &paths));
    }

    if dry_run {
        println!("\n  Dry run complete. Nothing changed.");
        return Ok(());
    }

    if !yes && !confirm()? {
        println!("\n  Cancelled. Nothing changed.");
        return Ok(());
    }

    println!();
    for client in clients {
        let result = reconcile(client, &paths, &binary_path, false)?;
        print_applied(client, &result);
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
    print!("\n  Apply these agent-configuration changes? [Y/n] ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim();
    Ok(answer.is_empty() || answer.eq_ignore_ascii_case("y"))
}
