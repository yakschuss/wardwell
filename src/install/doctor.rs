use crate::config::loader::{self, config_dir};
use crate::install::detect;
use crate::install::mcp_config::{self, McpConfigPaths, McpEntryStatus};

/// Run diagnostic checks.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    println!("wardwell doctor\n");

    let mut all_ok = true;

    // 1. Config
    let config_path = config_dir().join("config.yml");
    if config_path.exists() {
        match loader::load(Some(&config_path)) {
            Ok(config) => {
                println!("  Config                                 \u{2713} vault: {}", config.vault_path.display());

                // Vault directory + file count
                if config.vault_path.exists() {
                    let md_count = count_md_files(&config.vault_path, &config.exclude);
                    println!("  Vault                                  \u{2713} {} .md files", md_count);
                } else {
                    println!("  Vault                                  \u{2717}");
                    println!("    {} does not exist", config.vault_path.display());
                    all_ok = false;
                }

                // Domains — derived from vault subdirectories
                if config.vault_path.exists() {
                    let domains = list_vault_domains(&config.vault_path);
                    if domains.is_empty() {
                        println!("  Domains                                \u{2717} no subdirectories in vault");
                    } else {
                        println!("  Domains                                \u{2713} {}", domains.join(", "));
                    }
                }

                // Index
                let index_path = config_dir().join("index.db");
                if index_path.exists() {
                    if let Ok(index) = crate::index::store::IndexStore::open(&index_path)
                        && let Ok(conn) = index.lock()
                    {
                        let count: i64 = conn
                            .query_row("SELECT COUNT(*) FROM vault_meta", [], |row| row.get(0))
                            .unwrap_or(0);
                        let size = std::fs::metadata(&index_path)
                            .map(|m| format_size(m.len()))
                            .unwrap_or_default();
                        println!("  Index                                  \u{2713} {} entries ({})", count, size);
                    } else {
                        println!("  Index                                  \u{2717} could not open");
                        all_ok = false;
                    }
                } else {
                    println!("  Index                                  \u{2717} not built yet (run `wardwell serve`)");
                    all_ok = false;
                }

                // Excluded patterns
                if !config.exclude.is_empty() {
                    println!("  Excluded                               \u{2713} {}", config.exclude.join(", "));
                }

                // Sessions
                let sessions_db = config_dir().join("sessions.db");
                if sessions_db.exists()
                    && let Ok(store) = crate::daemon::indexer::SessionStore::open(&sessions_db)
                    && let Ok(count) = store.count()
                {
                    println!("  Sessions                               \u{2713} {} indexed", count);
                }

                // MCP configs
                let mcp_paths = McpConfigPaths::detect();
                let binary_path = detect::find_binary_path();
                let binary_str = binary_path.to_string_lossy().to_string();

                check_mcp("Claude Code MCP", &mcp_paths.claude_code, &binary_str, &mut all_ok);
                check_mcp("Claude Desktop MCP", &mcp_paths.claude_desktop, &binary_str, &mut all_ok);

                // CLAUDE.md pointers
                let domain_paths: Vec<String> = config.registry
                    .all()
                    .iter()
                    .flat_map(|d| d.paths.iter().map(|p| p.as_str().to_string()))
                    .collect();

                let claude_md_files = detect::find_claude_md_files(&domain_paths);
                let mut pointer_count = 0;
                for path in &claude_md_files {
                    if let Ok(content) = std::fs::read_to_string(path)
                        && content.contains("<!-- wardwell:start -->")
                    {
                        pointer_count += 1;
                    }
                }
                if pointer_count > 0 {
                    println!("  CLAUDE.md pointer                      \u{2713} markers found");
                } else {
                    println!("  CLAUDE.md pointer                      \u{2717} no wardwell markers");
                    all_ok = false;
                }

                // SessionStart hook
                let home = dirs::home_dir().unwrap_or_default();
                let settings_path = home.join(".claude/settings.json");
                if check_session_start_hook(&settings_path) {
                    println!("  SessionStart hook                      \u{2713} wardwell inject");
                } else {
                    println!("  SessionStart hook                      \u{2717} not registered");
                    all_ok = false;
                }

                // Claude CLI
                let claude_available = std::process::Command::new("claude")
                    .arg("--version")
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success());
                if claude_available {
                    println!("  Claude CLI                             \u{2713} {} available", config.ai.summarize_model);
                } else {
                    println!("  Claude CLI                             \u{2717} `claude` not found");
                    all_ok = false;
                }
            }
            Err(e) => {
                println!("  Config                                 \u{2717} parse error: {e}");
                all_ok = false;
            }
        }
    } else {
        println!("  Config                                 \u{2717} not found. Run `wardwell init`.");
        all_ok = false;
    }

    println!();
    if all_ok {
        println!("  All checks passed.");
    } else {
        println!("  Some checks failed. Run `wardwell init` to fix.");
    }

    Ok(())
}

fn check_session_start_hook(settings_path: &std::path::Path) -> bool {
    let content = match std::fs::read_to_string(settings_path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let config: serde_json::Value = match serde_json::from_str(&content) {
        Ok(c) => c,
        Err(_) => return false,
    };

    let entries = match config.get("hooks")
        .and_then(|h| h.get("SessionStart"))
        .and_then(|s| s.as_array())
    {
        Some(e) => e,
        None => return false,
    };

    entries.iter().any(|entry| {
        entry.get("command").and_then(|c| c.as_str()).is_some_and(|c| c.contains("wardwell"))
            || entry.get("hooks").and_then(|h| h.as_array()).is_some_and(|hooks| {
                hooks.iter().any(|h| {
                    h.get("command").and_then(|c| c.as_str()).is_some_and(|c| c.contains("wardwell"))
                })
            })
    })
}

fn check_mcp(name: &str, config_path: &std::path::Path, expected_binary: &str, all_ok: &mut bool) {
    match mcp_config::check_mcp_entry(config_path) {
        McpEntryStatus::Configured { binary_path } => {
            if binary_path == expected_binary {
                println!("  {name:<40} \u{2713} wardwell in mcpServers");
            } else {
                println!("  {name:<40} \u{2713} wardwell (binary path differs)");
            }
        }
        McpEntryStatus::NotConfigured => {
            println!("  {name:<40} \u{2717} not configured");
            *all_ok = false;
        }
        McpEntryStatus::ConfigMissing => {
            println!("  {name:<40} \u{2717} config file missing");
            *all_ok = false;
        }
    }
}

/// List vault subdirectory names (domains).
fn list_vault_domains(vault_dir: &std::path::Path) -> Vec<String> {
    let mut domains = Vec::new();
    if let Ok(entries) = std::fs::read_dir(vault_dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir()
                && let Some(name) = entry.file_name().to_str()
            {
                domains.push(name.to_string());
            }
        }
    }
    domains.sort();
    domains
}

/// Count .md files in a directory tree, respecting exclude patterns.
fn count_md_files(root: &std::path::Path, exclude: &[String]) -> usize {
    let results = crate::vault::reader::walk_vault_filtered(root, exclude);
    results.iter().filter(|r| r.is_ok()).count()
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1_000_000 {
        format!("{}MB", bytes / 1_000_000)
    } else if bytes >= 1_000 {
        format!("{}KB", bytes / 1_000)
    } else {
        format!("{bytes}B")
    }
}
