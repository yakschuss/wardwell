use crate::config::loader::config_dir;
use crate::install::detect;
use crate::install::installer;
use crate::tracker::schedule::{SystemRunner, current_uid};
use crate::install::mcp_config::{self, ChangeStatus, McpConfigPaths, ReconcileResult};
use std::path::{Path, PathBuf};

/// Read one line from stdin, trimmed.
fn prompt_line() -> String {
    let mut buf = String::new();
    let _ = std::io::stdin().read_line(&mut buf);
    buf.trim().to_string()
}

/// Print label and wait for Enter (returns true) or 's' to skip (returns false).
fn prompt_pause(label: &str) -> bool {
    println!("\n  {label}");
    print!("  Press Enter to continue, or 's' to skip: ");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let input = prompt_line();
    !input.eq_ignore_ascii_case("s")
}

/// Detect vault path interactively. Returns validated PathBuf.
fn detect_vault_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    // Check if config already exists with a vault path
    let config_path = config_dir().join("config.yml");
    if config_path.exists()
        && let Ok(config) = crate::config::loader::load(Some(&config_path))
        && config.vault_path.exists()
    {
        println!("  Existing vault: {}", config.vault_path.display());
        print!("  Press Enter to keep, or paste a new path: ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let input = prompt_line();
        if input.is_empty() {
            return Ok(config.vault_path);
        }
        let p = expand_path(&input);
        if p.exists() {
            return Ok(p);
        }
        eprintln!("  Path does not exist: {}", p.display());
    }

    // Try auto-detect via Obsidian
    let obsidian = detect::scan_obsidian_vaults();
    if !obsidian.is_empty() {
        println!("  Found Obsidian vault(s):");
        for (i, v) in obsidian.iter().enumerate() {
            println!("    [{i}] {}", v.display());
        }
        print!("  Enter number to select, or paste a path: ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let input = prompt_line();

        if let Ok(idx) = input.parse::<usize>()
            && idx < obsidian.len()
        {
            return Ok(obsidian[idx].clone());
        }
        if !input.is_empty() {
            let p = expand_path(&input);
            if p.exists() {
                return Ok(p);
            }
            eprintln!("  Path does not exist: {}", p.display());
        } else if obsidian.len() == 1 {
            return Ok(obsidian[0].clone());
        }
    }

    // Manual entry loop
    loop {
        print!("  Enter vault path: ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let input = prompt_line();
        if input.is_empty() {
            // Default to ~/.wardwell/vault
            let default = config_dir().join("vault");
            println!("  Using default: {}", default.display());
            return Ok(default);
        }
        let p = expand_path(&input);
        if p.exists() {
            return Ok(p);
        }
        eprintln!("  Path does not exist: {}. Try again.", p.display());
    }
}

fn expand_path(input: &str) -> PathBuf {
    if let Some(rest) = input.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(input)
}

/// Walk vault 2 levels deep, display directory structure with file counts.
fn scan_and_display_vault(vault_path: &Path) {
    println!("\n  Vault contents:");
    let entries = match std::fs::read_dir(vault_path) {
        Ok(e) => e,
        Err(_) => {
            println!("    (empty or unreadable)");
            return;
        }
    };

    let mut dirs: Vec<(String, usize)> = Vec::new();
    let mut root_files = 0usize;

    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.'))
        {
            continue;
        }
        if path.is_dir() {
            let count = count_files_recursive(&path);
            dirs.push((entry.file_name().to_string_lossy().to_string(), count));
        } else {
            root_files += 1;
        }
    }

    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, count) in &dirs {
        println!("    {name}/ ({count} files)");
    }
    if root_files > 0 {
        println!("    ({root_files} files in root)");
    }
    if dirs.is_empty() && root_files == 0 {
        println!("    (empty)");
    }
}

fn count_files_recursive(dir: &Path) -> usize {
    let mut count = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                count += count_files_recursive(&path);
            } else {
                count += 1;
            }
        }
    }
    count
}

/// Print preview of all mutations, return true if user confirms.
fn preview_and_confirm(vault_path: &Path, config_path: &Path, binary_path: &Path) -> bool {
    println!("\n  wardwell will perform the following:");
    println!();

    if !config_path.exists() {
        println!("    CREATE  {}", config_path.display());
    } else {
        println!("    UPDATE  {} (vault_path)", config_path.display());
    }

    println!("    CREATE  ~/.wardwell/summaries/");

    let mcp_paths = McpConfigPaths::detect();
    println!(
        "    RECONCILE  Claude Code connection → {}",
        mcp_paths.claude_code.display()
    );
    println!(
        "    RECONCILE  Claude Desktop connection → {}",
        mcp_paths.claude_desktop.display()
    );
    println!("    RECONCILE  Codex connection → {}", mcp_paths.codex.display());

    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    match hooks_plan(binary_path) {
        Ok(plan) => plan.lines.iter().for_each(|line| println!("{}", line.render())),
        Err(error) => println!("    BLOCKED  Claude Code hooks: {error}"),
    }
    println!(
        "    INJECT  CLAUDE.md markers → {}",
        home.join(".claude/CLAUDE.md").display()
    );
    println!(
        "    INDEX   {} → ~/.wardwell/index.db",
        vault_path.display()
    );
    println!("    BINARY  {}", binary_path.display());

    print!("\n  Proceed? [Y/n] ");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let input = prompt_line();
    input.is_empty() || input.eq_ignore_ascii_case("y")
}

/// Interactive init. Walks user through vault selection, previews mutations,
/// step-by-step with pauses.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    println!("wardwell init\n");

    // 1. Detect vault path
    let vault_path = detect_vault_path()?;
    let config_path = config_dir().join("config.yml");
    let binary_path = detect::find_binary_path();

    // 2. Scan and display vault contents
    scan_and_display_vault(&vault_path);

    // 3. Preview and confirm
    if !preview_and_confirm(&vault_path, &config_path, &binary_path) {
        println!("\n  Cancelled.");
        return Ok(());
    }

    let mut skipped: Vec<String> = Vec::new();

    // 4. Create dirs + write config
    println!();
    for dir in &[config_dir().to_path_buf(), config_dir().join("summaries")] {
        std::fs::create_dir_all(dir)?;
    }

    if config_path.exists() {
        // Only update vault_path if it actually changed
        let existing_vault = crate::config::loader::load(Some(&config_path))
            .ok()
            .map(|c| c.vault_path);
        if existing_vault.as_ref() == Some(&vault_path) {
            println!("  \u{2713} Existing config (vault_path unchanged)");
        } else {
            println!("  \u{2713} Existing config found. Updating vault_path.");
            update_config_vault_path(&config_path, &vault_path)?;
        }
    } else {
        write_minimal_config(&config_path, &vault_path)?;
        println!("  \u{2713} Config written: {}", config_path.display());
    }

    // 5. Agent-client MCP configuration. The global preview above is the one
    // consent gate; each client mutation is idempotent and backed up.
    let mcp_paths = McpConfigPaths::detect();
    if detect::command_available("claude") {
        report_reconciliation(
            "Claude Code",
            mcp_config::reconcile_claude_code(&mcp_paths.claude_code, &binary_path, false),
            &mut skipped,
        );
    } else {
        println!("  - Claude Code not installed; configuration unchanged");
    }

    let claude_desktop_installed =
        Path::new("/Applications/Claude.app").exists() || mcp_paths.claude_desktop.exists();
    if claude_desktop_installed {
        report_reconciliation(
            "Claude Desktop",
            mcp_config::reconcile_claude_desktop(&mcp_paths.claude_desktop, &binary_path, false),
            &mut skipped,
        );
    } else {
        println!("  - Claude Desktop not installed; configuration unchanged");
    }

    if detect::command_available("codex") {
        report_reconciliation(
            "Codex",
            mcp_config::reconcile_codex(&mcp_paths.codex, &binary_path, false),
            &mut skipped,
        );
    } else {
        println!("  - Codex not installed; configuration unchanged");
    }

    // 7. Claude Code hooks and permissions, through the same careful
    // installer as `setup`.
    if prompt_pause("Install Wardwell's Claude Code hooks?") {
        match install_hooks(&binary_path) {
            Ok(notes) => notes.iter().for_each(|note| println!("{note}")),
            Err(e) => {
                println!("  \u{2717} Hook install failed: {e}");
                skipped.push("Hooks: run `wardwell setup` to install them".to_string());
            }
        }
    } else {
        skipped.push("Hooks: run `wardwell setup` to install them".to_string());
    }

    // 8. CLAUDE.md injection
    if prompt_pause("Inject wardwell context into CLAUDE.md?") {
        inject_claude_md_pointer();
        println!("  \u{2713} CLAUDE.md markers injected");
    } else {
        skipped.push("CLAUDE.md: manually add wardwell markers to ~/.claude/CLAUDE.md".to_string());
    }

    // 9. Build index (with exclude list from config)
    if vault_path.exists() {
        println!("\n  Building index...");
        let exclude = crate::config::loader::load(Some(&config_path))
            .map(|c| c.exclude)
            .unwrap_or_default();
        let index_path = config_dir().join("index.db");
        if let Ok(index) = crate::index::store::IndexStore::open(&index_path) {
            match crate::index::builder::IndexBuilder::build_filtered(
                &index,
                &vault_path,
                &exclude,
                None,
            ) {
                Ok(stats) => println!(
                    "  \u{2713} Indexed {} files ({} skipped, {} errors)",
                    stats.indexed, stats.skipped, stats.errors
                ),
                Err(e) => println!("  \u{2717} Index build failed: {e}"),
            }
        }
    }

    // 10. Migrate config domains if needed
    if let Ok(config) = crate::config::loader::load(Some(&config_path))
        && !config.registry.is_empty()
    {
        let vault_domains_dir = vault_path.join("domains");
        let has_vault_domains = vault_domains_dir.exists()
            && std::fs::read_dir(&vault_domains_dir)
                .map(|e| {
                    e.flatten()
                        .any(|f| f.path().extension().and_then(|e| e.to_str()) == Some("md"))
                })
                .unwrap_or(false);

        if !has_vault_domains {
            migrate_config_domains(&config, &vault_path);
        }
    }

    // 11. Summary
    println!("\n  Done.");
    if !skipped.is_empty() {
        println!("\n  Skipped steps (manual instructions):");
        for s in &skipped {
            println!("    - {s}");
        }
    }
    println!("\n  Authenticate hosted Wardwell when each client asks for OAuth approval.");
    println!("  Claude account connector: https://claude.ai/customize/connectors");
    println!("  Codex: codex mcp login wardwell");

    Ok(())
}

fn report_reconciliation(
    client: &str,
    result: Result<ReconcileResult, std::io::Error>,
    skipped: &mut Vec<String>,
) {
    match result {
        Ok(result) => {
            let outcome = match result.status {
                ChangeStatus::Created => "configured",
                ChangeStatus::Updated => "updated",
                ChangeStatus::Unchanged => "already current",
                ChangeStatus::DryRunCreate | ChangeStatus::DryRunUpdate => "planned",
            };
            println!("  \u{2713} {client} {outcome}");
            if let Some(backup) = result.backup_path {
                println!("    backup: {}", backup.display());
            }
            if result.migrated_legacy_remote {
                println!("    replaced legacy static Wardwell connection with OAuth configuration");
            }
        }
        Err(error) => {
            println!("  \u{2717} {client} configuration unchanged: {error}");
            skipped.push(format!(
                "{client}: resolve the reported configuration conflict"
            ));
        }
    }
}

/// Update just the vault_path in an existing config.yml.
fn update_config_vault_path(
    config_path: &Path,
    vault_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(config_path)?;
    // Replace the vault_path line
    let mut new_lines = Vec::new();
    let mut replaced = false;
    for line in content.lines() {
        if line.starts_with("vault_path:") && !replaced {
            new_lines.push(format!("vault_path: {}", vault_path.display()));
            replaced = true;
        } else {
            new_lines.push(line.to_string());
        }
    }
    if !replaced {
        // vault_path line not found, prepend it
        new_lines.insert(0, format!("vault_path: {}", vault_path.display()));
    }
    std::fs::write(config_path, new_lines.join("\n") + "\n")?;
    Ok(())
}

/// Migrate domains from config to vault files.
fn migrate_config_domains(
    config: &crate::config::loader::WardwellConfig,
    vault_path: &std::path::Path,
) {
    let domains_dir = vault_path.join("domains");
    if let Err(e) = std::fs::create_dir_all(&domains_dir) {
        eprintln!("wardwell: failed to create domains dir: {e}");
        return;
    }

    let mut count = 0;
    for domain in config.registry.all() {
        let name = domain.name.as_str();
        let path = domains_dir.join(format!("{name}.md"));

        let mut content = format!(
            "---\ntype: domain\ndomain: {name}\nconfidence: confirmed\nstatus: active\n---\n\n## Paths\n"
        );
        for p in &domain.paths {
            content.push_str(&format!("- {}\n", p.as_str()));
        }

        if !domain.aliases.is_empty() {
            content.push_str("\n## Aliases\n");
            let mut sorted_aliases: Vec<_> = domain.aliases.iter().collect();
            sorted_aliases.sort_by_key(|(k, _)| (*k).clone());
            for (key, value) in sorted_aliases {
                content.push_str(&format!("- {key}: {value}\n"));
            }
        }

        if let Err(e) = std::fs::write(&path, content) {
            eprintln!(
                "wardwell: failed to write domain file {}: {e}",
                path.display()
            );
        } else {
            count += 1;
        }
    }

    if count > 0 {
        println!("  migrated {count} domains from config to vault");
    }
}

fn write_minimal_config(
    config_path: &std::path::Path,
    vault_path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let yaml = format!(
        "\
# Wardwell config

vault_path: {}

session_sources:
  - ~/.claude/projects/

exclude:
  - node_modules
  - .git
  - vendor
  - target
  - .obsidian
  - .trash
",
        vault_path.display()
    );

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(config_path, yaml)?;

    Ok(())
}

fn build_injection_content(_domains: &[String]) -> String {
    "\
## Wardwell — Personal Knowledge System

Your vault is indexed. Three tools:

**wardwell_search** — Find things.
  action: search | read | history | orchestrate | retrospective | patterns | context | resume
  - \"search\": FTS query across vault (default). Add mode:\"semantic\" for hybrid BM25+vector search — returns chunk-level results with full text. Use limit to control depth (3=surgical, 20=broad).
  - \"read\": full file by path
  - \"history\": query across history.jsonl files
  - \"orchestrate\": prioritized project queue
  - \"retrospective\": what happened in a time period (requires since date)
  - \"patterns\": recurring blockers, stale threads, hot topics (defaults to 90 days)
  - \"context\": session summary by ID (lightweight, cached)
  - \"resume\": full session handoff by ID — plan, progress, remaining work (always fresh, uses AI)

**wardwell_write** — Change things.
  action: sync | decide | append_history | lesson | append
  - \"sync\": FULL REPLACE of current_state.md + optionally append history.jsonl
  - \"decide\": append to decisions.md
  - \"append_history\": log to history.jsonl without state change
  - \"lesson\": append to lessons.jsonl (what went wrong, why, prevention)
  - \"append\": append to a named JSONL list (requires 'list' param, e.g. 'future-ideas'). Check existing lists first. ASK the user before creating a new list — never create lists speculatively.

**wardwell_clipboard** — Copy to clipboard (ALWAYS ask first).

**When to use:**
- User references a project → search first
- Session produced state changes → offer to sync
- Meaningful work completed (commits, multi-file edits, feature shipped) → proactively offer to sync
- Real tradeoff decision made → offer to record it
- Something broke → offer to record the lesson
- User asks \"what's next\" → orchestrate
- User asks \"how has X evolved\" → history query
- User asks \"what did I accomplish this week\" → retrospective
- User asks \"what keeps blocking me\" → patterns
- User asks \"catch me up on session X\" → context
- User asks \"pick up from session X\" or gives a session ID to continue → resume

**Source tagging:**
All writes accept an optional 'source' param. Always pass it:
- 'desktop' — from Claude Desktop or claude.ai
- 'code' — from Claude Code
- 'manual' — human-edited

**Quality bar:**
- Snapshots: one sentence focus, concrete next action
- History entries: what changed and why, not what was discussed
- Decisions: the tradeoff and rejected alternatives, not the implementation
- Lessons: root cause and prevention, not just what happened

**File roles:**
- INDEX.md — rich project notes, architecture, context. Human-edited. Never overwritten by wardwell.
- current_state.md — lightweight snapshot. FULLY REPLACED on every sync. Do NOT put rich content here.
- decisions.md — append-only. Human-readable markdown.
- history.jsonl — append-only machine log. JSONL with schema header.
- lessons.jsonl — append-only machine log. JSONL with schema header.

Other .md files in a project folder are user-managed — indexed and searchable, but never written or overwritten by wardwell.

Domains are folders under the vault root. Projects are subfolders."
        .to_string()
}

fn inject_claude_md_pointer() {
    // Load config to get domain names
    let config_path = config_dir().join("config.yml");
    let domain_names: Vec<String> = crate::config::loader::load(Some(&config_path))
        .map(|c| c.registry.names())
        .unwrap_or_default();

    let content = build_injection_content(&domain_names);

    // Inject into global CLAUDE.md
    if let Some(home) = dirs::home_dir() {
        let global = home.join(".claude/CLAUDE.md");
        let _ = crate::inject::inject(&global, &content);
    }

    // Inject into domain project CLAUDE.md files
    if let Ok(config) = crate::config::loader::load(Some(&config_path)) {
        let domain_paths: Vec<String> = config
            .registry
            .all()
            .iter()
            .flat_map(|d| d.paths.iter().map(|p| p.as_str().to_string()))
            .collect();
        let claude_md_files = crate::install::detect::find_claude_md_files(&domain_paths);
        for path in &claude_md_files {
            // Skip global — already handled above
            if let Some(home) = dirs::home_dir()
                && *path == home.join(".claude/CLAUDE.md")
            {
                continue;
            }
            let _ = crate::inject::inject(path, &content);
        }
    }
}

/// The installer's plan for this computer, with the bindings in config.yml.
fn hooks_plan(binary_path: &Path) -> Result<installer::Plan, String> {
    let home = dirs::home_dir().ok_or("Could not find the home directory")?;
    hooks_plan_at(&home, &config_dir(), binary_path, cfg!(target_os = "macos"))
}

fn hooks_plan_at(home: &Path, config_dir: &Path, binary_path: &Path, launchd: bool) -> Result<installer::Plan, String> {
    let trackers = crate::install::setup::tracker_bindings(config_dir)?;
    installer::plan(&installer::Inputs {
        home,
        config_dir,
        binary: binary_path,
        trackers: &trackers,
        claude_code: true,
        launchd,
    })
}

/// Apply the installer's plan; the lines to print, activation notes last.
fn install_hooks(binary_path: &Path) -> Result<Vec<String>, String> {
    let home = dirs::home_dir().ok_or("Could not find the home directory")?;
    install_hooks_at(&home, &config_dir(), binary_path, cfg!(target_os = "macos"), &SystemRunner, &current_uid)
}

fn install_hooks_at(
    home: &Path,
    config_dir: &Path,
    binary_path: &Path,
    launchd: bool,
    runner: &dyn crate::tracker::schedule::LaunchctlRunner,
    uid: &dyn Fn() -> Result<u32, String>,
) -> Result<Vec<String>, String> {
    let plan = hooks_plan_at(home, config_dir, binary_path, launchd)?;
    let mut lines = installer::apply(&plan, runner, uid).map_err(|failed| {
        let mut text = failed.lines.join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        text + &failed.message
    })?;
    lines.push("  \u{2713} Hooks installed".to_string());
    lines.extend(plan.activation().into_iter().map(|note| format!("    {note}")));
    Ok(lines)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn expand_path_with_tilde() {
        let result = expand_path("~/foo");
        let home = dirs::home_dir().unwrap();
        assert_eq!(result, home.join("foo"));
    }

    #[test]
    fn expand_path_absolute() {
        let result = expand_path("/abs/path");
        assert_eq!(result, PathBuf::from("/abs/path"));
    }

    #[test]
    fn expand_path_relative() {
        let result = expand_path("relative/path");
        assert_eq!(result, PathBuf::from("relative/path"));
    }

    #[test]
    fn count_files_recursive_with_nested() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::File::create(dir.path().join("a.md")).unwrap();
        std::fs::File::create(dir.path().join("b.txt")).unwrap();
        std::fs::File::create(sub.join("c.md")).unwrap();
        assert_eq!(count_files_recursive(dir.path()), 3);
    }

    #[test]
    fn count_files_recursive_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(count_files_recursive(dir.path()), 0);
    }

    #[test]
    fn init_installs_hooks_through_the_careful_installer() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cfg = home.join(".wardwell");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(home.join(".claude/settings.json"), r#"{"model": "keep", "hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "peon ping"}]}]}}"#).unwrap();
        std::fs::write(cfg.join("config.yml"), format!("vault_path: {}\nsession_sources: []\n", dir.path().display())).unwrap();
        let fake = crate::tracker::schedule::fake::Fake::new(&[]);
        let binary = Path::new("/nonexistent-init-test/wardwell");
        let lines = install_hooks_at(&home, &cfg, binary, true, &fake, &|| Ok(501)).unwrap();
        assert!(lines.iter().any(|l| l.contains("Hooks installed")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("Sessions already running do not change")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("backup:")), "{lines:?}");
        let text = std::fs::read_to_string(home.join(".claude/settings.json")).unwrap();
        assert!(text.contains("peon ping") && text.contains("\"model\": \"keep\""), "{text}");
        assert!(text.contains("'/nonexistent-init-test/wardwell' inject"), "{text}");
        assert!(fake.calls.borrow().is_empty());
        let again = hooks_plan_at(&home, &cfg, binary, true).unwrap();
        assert!(again.is_noop());
    }

    #[test]
    fn init_hooks_refuse_a_malformed_settings_file_and_write_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(home.join(".claude/settings.json"), "{").unwrap();
        let fake = crate::tracker::schedule::fake::Fake::new(&[]);
        let error = install_hooks_at(&home, &home.join(".wardwell"), Path::new("/w"), true, &fake, &|| Ok(501)).unwrap_err();
        assert!(error.contains("no files changed"), "{error}");
        assert_eq!(std::fs::read_to_string(home.join(".claude/settings.json")).unwrap(), "{");
    }

    #[test]
    fn build_injection_content_returns_expected() {
        let content = build_injection_content(&[]);
        assert!(
            content.contains("wardwell_search"),
            "missing wardwell_search"
        );
        assert!(content.contains("wardwell_write"), "missing wardwell_write");
        assert!(
            content.contains("wardwell_clipboard"),
            "missing wardwell_clipboard"
        );
    }
}
