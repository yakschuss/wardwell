use crate::config::loader::config_dir;
use crate::install::detect;
use crate::install::mcp_config::{self, McpConfigPaths};
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
        if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.')) {
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
    println!("    INJECT  MCP → {}", mcp_paths.claude_code.display());
    println!("    INJECT  MCP → {}", mcp_paths.claude_desktop.display());

    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    println!("    INJECT  SessionStart hook → {}", home.join(".claude/settings.json").display());
    println!("    INJECT  CLAUDE.md markers → {}", home.join(".claude/CLAUDE.md").display());
    println!("    INDEX   {} → ~/.wardwell/index.db", vault_path.display());
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
    for dir in &[
        config_dir().to_path_buf(),
        config_dir().join("summaries"),
    ] {
        std::fs::create_dir_all(dir)?;
    }

    if config_path.exists() {
        println!("  \u{2713} Existing config found. Updating vault_path.");
        update_config_vault_path(&config_path, &vault_path)?;
    } else {
        write_minimal_config(&config_path, &vault_path)?;
        println!("  \u{2713} Config written: {}", config_path.display());
    }

    // 5. MCP — Claude Code
    let mcp_paths = McpConfigPaths::detect();
    if prompt_pause("Inject MCP server into Claude Code config?") {
        match mcp_config::inject_mcp_entry(&mcp_paths.claude_code, &binary_path) {
            Ok(_) => println!("  \u{2713} MCP injected into {}", mcp_paths.claude_code.display()),
            Err(e) => {
                println!("  \u{2717} MCP inject failed: {e}");
                skipped.push(format!("MCP Claude Code: manually add wardwell to {}", mcp_paths.claude_code.display()));
            }
        }
    } else {
        skipped.push(format!("MCP Claude Code: manually add wardwell to {}", mcp_paths.claude_code.display()));
    }

    // 6. MCP — Claude Desktop
    if prompt_pause("Inject MCP server into Claude Desktop config?") {
        match mcp_config::inject_mcp_entry(&mcp_paths.claude_desktop, &binary_path) {
            Ok(_) => println!("  \u{2713} MCP injected into {}", mcp_paths.claude_desktop.display()),
            Err(e) => {
                println!("  \u{2717} MCP inject failed: {e}");
                skipped.push(format!("MCP Claude Desktop: manually add wardwell to {}", mcp_paths.claude_desktop.display()));
            }
        }
    } else {
        skipped.push(format!("MCP Claude Desktop: manually add wardwell to {}", mcp_paths.claude_desktop.display()));
    }

    // 7. SessionStart hook
    if prompt_pause("Install SessionStart hook?") {
        match install_hook() {
            Ok(()) => println!("  \u{2713} SessionStart hook installed"),
            Err(e) => {
                println!("  \u{2717} Hook install failed: {e}");
                skipped.push("SessionStart hook: manually register wardwell inject in ~/.claude/settings.json".to_string());
            }
        }
    } else {
        skipped.push("SessionStart hook: manually register wardwell inject in ~/.claude/settings.json".to_string());
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
            match crate::index::builder::IndexBuilder::build_filtered(&index, &vault_path, &exclude) {
                Ok(stats) => println!("  \u{2713} Indexed {} files ({} skipped, {} errors)", stats.indexed, stats.skipped, stats.errors),
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
                .map(|e| e.flatten().any(|f| f.path().extension().and_then(|e| e.to_str()) == Some("md")))
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
    println!("\n  Restart Claude Code to activate wardwell.");

    Ok(())
}

/// Update just the vault_path in an existing config.yml.
fn update_config_vault_path(config_path: &Path, vault_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
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
fn migrate_config_domains(config: &crate::config::loader::WardwellConfig, vault_path: &std::path::Path) {
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
            eprintln!("wardwell: failed to write domain file {}: {e}", path.display());
        } else {
            count += 1;
        }
    }

    if count > 0 {
        println!("  migrated {count} domains from config to vault");
    }
}

fn write_minimal_config(config_path: &std::path::Path, vault_path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
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
  - \"search\": FTS query across vault
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
- Decisions: the tradeoff, not the implementation
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
        let domain_paths: Vec<String> = config.registry.all().iter()
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

fn install_hook() -> Result<(), Box<dyn std::error::Error>> {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let settings_path = home.join(".claude/settings.json");

    let mut config: serde_json::Value = if settings_path.exists() {
        let content = std::fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let hooks = config
        .as_object_mut()
        .ok_or_else(|| std::io::Error::other("settings.json is not a JSON object"))?
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));

    let hooks_obj = hooks
        .as_object_mut()
        .ok_or_else(|| std::io::Error::other("hooks is not a JSON object"))?;

    let binary_path = detect::find_binary_path();

    // SessionStart: fast inject (no index rebuild)
    let inject_command = format!("{} inject \"$(pwd)\"", binary_path.display());
    let start_hook = serde_json::json!({
        "hooks": [{
            "type": "command",
            "command": inject_command
        }]
    });

    // Install SessionStart hook
    install_hook_entry(hooks_obj, "SessionStart", &start_hook)?;

    // Stop: resolve session against last Desktop intent
    let resolve_command = format!("{} resolve", binary_path.display());
    let stop_hook = serde_json::json!({
        "hooks": [{
            "type": "command",
            "command": resolve_command
        }]
    });
    install_hook_entry(hooks_obj, "Stop", &stop_hook)?;

    // Remove SessionEnd hook if present
    hooks_obj.remove("SessionEnd");

    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(&config)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    std::fs::write(&settings_path, json)?;

    Ok(())
}

/// Install or update a wardwell hook entry in a given hook event array.
fn install_hook_entry(
    hooks_obj: &mut serde_json::Map<String, serde_json::Value>,
    event: &str,
    hook: &serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let entries = hooks_obj
        .entry(event)
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .ok_or_else(|| std::io::Error::other(format!("{event} is not an array")))?;

    let already_registered = entries.iter().any(is_wardwell_hook);

    if already_registered {
        for entry in entries.iter_mut() {
            if is_wardwell_hook(entry) {
                *entry = hook.clone();
            }
        }
    } else {
        entries.push(hook.clone());
    }

    Ok(())
}

/// Check if a hook entry is a wardwell hook (old or new format).
fn is_wardwell_hook(entry: &serde_json::Value) -> bool {
    // Old flat format: {type: "command", command: "...wardwell..."}
    entry.get("command").and_then(|c| c.as_str()).is_some_and(|c| c.contains("wardwell"))
        || entry.get("hooks").and_then(|h| h.as_array()).is_some_and(|hooks| {
            hooks.iter().any(|h| {
                h.get("command").and_then(|c| c.as_str()).is_some_and(|c| c.contains("wardwell"))
            })
        })
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
    fn is_wardwell_hook_old_flat_format() {
        let entry = serde_json::json!({"type": "command", "command": "wardwell inject $(pwd)"});
        assert!(is_wardwell_hook(&entry));
    }

    #[test]
    fn is_wardwell_hook_new_nested_format() {
        let entry = serde_json::json!({"hooks": [{"type": "command", "command": "/usr/bin/wardwell inject $(pwd)"}]});
        assert!(is_wardwell_hook(&entry));
    }

    #[test]
    fn is_wardwell_hook_non_wardwell() {
        let entry = serde_json::json!({"type": "command", "command": "echo hello"});
        assert!(!is_wardwell_hook(&entry));
    }

    #[test]
    fn build_injection_content_returns_expected() {
        let content = build_injection_content(&[]);
        assert!(content.contains("wardwell_search"), "missing wardwell_search");
        assert!(content.contains("wardwell_write"), "missing wardwell_write");
        assert!(content.contains("wardwell_clipboard"), "missing wardwell_clipboard");
    }
}
