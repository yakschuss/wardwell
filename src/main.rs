use clap::{Parser, Subcommand};
use std::path::Path;

#[derive(Parser)]
#[command(name = "wardwell", version, about = "Personal AI knowledge vault — MCP server")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the MCP server (stdio transport) with background daemon tasks
    Serve,
    /// First-run setup — generates config, injects MCP entries, installs hooks
    Init,
    /// Check that everything is wired correctly
    Doctor,
    /// Clean removal — removes MCP entries, hooks, and markers (preserves vault data)
    Uninstall,
    /// Output project context for the given directory (used by hooks)
    Inject {
        /// Project directory (defaults to current directory)
        #[arg(default_value = ".")]
        path: String,
    },
    /// Stop hook — check if session should sync before exit (reads JSON from stdin)
    Resolve,
    /// Rebuild the vault search index from scratch
    Reindex,
    /// Create a domain or project folder under the vault (additive only)
    Seed {
        /// Domain or domain/project path (e.g., "work", "work/my-project")
        target: String,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result: Result<(), Box<dyn std::error::Error>> = match cli.command {
        Commands::Serve => run_serve().await,
        Commands::Init => wardwell::install::init::run(),
        Commands::Doctor => wardwell::install::doctor::run(),
        Commands::Uninstall => wardwell::install::uninstall::run(),
        Commands::Inject { ref path } => run_inject(path),
        Commands::Resolve => run_resolve(),
        Commands::Reindex => run_reindex(),
        Commands::Seed { ref target } => run_seed(target),
    };
    if let Err(e) = result {
        eprintln!("wardwell: {e}");
        std::process::exit(1);
    }
}

async fn run_serve() -> Result<(), Box<dyn std::error::Error>> {
    use rmcp::ServiceExt;
    use std::sync::Arc;
    use wardwell::config::loader;
    use wardwell::index::builder::IndexBuilder;
    use wardwell::index::store::IndexStore;
    use wardwell::mcp::server::WardwellServer;

    let config = loader::load(None)?;

    let config_dir = loader::config_dir();
    let index_path = config_dir.join("index.db");
    let index = IndexStore::open(&index_path)?;

    // Index vault path on startup
    let mut all_index_roots: Vec<std::path::PathBuf> = Vec::new();
    if config.vault_path.exists() {
        all_index_roots.push(config.vault_path.clone());
    }

    let index = Arc::new(index);

    // Index in background so the MCP server starts immediately
    let bg_index = Arc::clone(&index);
    let bg_roots = all_index_roots.clone();
    let bg_exclude = config.exclude.clone();
    tokio::spawn(async move {
        for root in &bg_roots {
            match IndexBuilder::build_filtered(&bg_index, root, &bg_exclude) {
                Ok(stats) => {
                    if stats.indexed > 0 || stats.removed > 0 {
                        eprintln!("wardwell: indexed {} files from {} ({} skipped, {} removed, {} errors)",
                            stats.indexed, root.display(), stats.skipped, stats.removed, stats.errors);
                    }
                }
                Err(e) => eprintln!("wardwell: index error for {}: {e}", root.display()),
            }
        }
    });

    let server = WardwellServer::new(config, Arc::clone(&index));
    let shared_registry = server.registry.clone();

    // Spawn vault file watcher for vault + sources
    // The vault root watcher gets the shared registry for live domain reload
    let vault_root_for_watcher = server.vault_root.clone();
    for root in all_index_roots {
        let watcher_index = Arc::clone(&index);
        let registry_for_watcher = if root == vault_root_for_watcher {
            Some(shared_registry.clone())
        } else {
            None
        };
        tokio::spawn(async move {
            if let Err(e) = wardwell::daemon::watcher::watch_vault(root.clone(), watcher_index, registry_for_watcher).await {
                eprintln!("wardwell: watcher error for {}: {e}", root.display());
            }
        });
    }

    // Spawn session indexer + summarizer (runs once then periodically)
    let session_sources = server.config.session_sources.clone();
    let domains = server.config.registry.all().to_vec();
    let ai_config = server.config.ai.clone();
    let summaries_dir = config_dir.join("summaries");
    let sessions_db = config_dir.join("sessions.db");
    tokio::spawn(async move {
        run_daemon_loop(sessions_db, session_sources, domains, summaries_dir, ai_config).await;
    });
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;

    Ok(())
}

async fn run_daemon_loop(
    sessions_db: std::path::PathBuf,
    session_sources: Vec<std::path::PathBuf>,
    domains: Vec<wardwell::domain::model::Domain>,
    summaries_dir: std::path::PathBuf,
    ai_config: wardwell::config::loader::AiConfig,
) {
    use wardwell::daemon::indexer;
    use wardwell::daemon::summarizer;

    let session_store = match indexer::SessionStore::open(&sessions_db) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wardwell: failed to open sessions.db: {e}");
            return;
        }
    };

    loop {
        // 1. Index sessions
        match indexer::index_sessions(&session_sources, &session_store, &domains) {
            Ok(stats) => {
                if stats.indexed > 0 {
                    eprintln!("wardwell: indexed {} sessions ({} skipped, {} errors)",
                        stats.indexed, stats.skipped, stats.errors);
                }
            }
            Err(e) => eprintln!("wardwell: session indexing error: {e}"),
        }

        // 2. Summarize via claude CLI
        match summarizer::summarize_pending(&session_store, &session_sources, &summaries_dir, &ai_config.summarize_model, false).await {
            Ok(stats) => {
                if stats.summarized > 0 {
                    eprintln!("wardwell: summarized {} sessions ({} skipped, {} errors)",
                        stats.summarized, stats.skipped, stats.errors);
                }
            }
            Err(e) => eprintln!("wardwell: summarization error: {e}"),
        }

        // Wait 5 minutes before next run
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;
    }
}

fn run_inject(cwd: &str) -> Result<(), Box<dyn std::error::Error>> {
    use wardwell::config::loader;

    let config = loader::load(None)?;
    let vault_path = &config.vault_path;

    if !vault_path.exists() {
        return Ok(());
    }

    // Try to match cwd to a vault domain by checking if cwd directory name
    // matches a subdirectory of the vault
    let cwd_path = std::path::Path::new(cwd);
    let cwd_name = cwd_path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    let matched_domain = std::fs::read_dir(vault_path).ok()
        .and_then(|entries| {
            entries.flatten()
                .find(|e| {
                    e.path().is_dir()
                        && e.file_name().to_string_lossy() == cwd_name
                })
                .map(|e| e.path())
        });

    if let Some(domain_dir) = matched_domain {
        // Found a matching domain — output its project summaries
        inject_domain_context(&domain_dir);
    }
    // No match = no output. Don't pollute non-project sessions.

    Ok(())
}

/// Output context for a specific domain's projects.
fn inject_domain_context(domain_dir: &Path) {
    let domain = domain_dir.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    // Check domain-level current_state.md
    let state = domain_dir.join("current_state.md");
    if state.exists()
        && let Ok(content) = std::fs::read_to_string(&state)
    {
        print!("{content}");
        return;
    }

    // Check subdirectory projects
    if let Ok(entries) = std::fs::read_dir(domain_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                let state = p.join("current_state.md");
                if state.exists()
                    && let Ok(vf) = wardwell::vault::reader::read_file(&state)
                {
                    let project = p.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown");
                    let status = vf.frontmatter.status.as_ref()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "active".to_string());
                    let focus = extract_section_simple(&vf.body, "Focus");
                    let next = extract_section_simple(&vf.body, "Next Action");
                    println!("**{domain}/{project}** ({status}): {focus}");
                    if !next.is_empty() {
                        println!("  Next: {next}");
                    }
                }
            }
        }
    }
}

/// Simple section extractor for inject (no dependency on server module).
fn extract_section_simple(body: &str, heading: &str) -> String {
    let marker = format!("## {heading}");
    let start = match body.find(&marker) {
        Some(pos) => pos + marker.len(),
        None => return String::new(),
    };
    let rest = body[start..].trim_start();
    let end = rest.find("\n## ").unwrap_or(rest.len());
    rest[..end].trim().to_string()
}


fn run_resolve() -> Result<(), Box<dyn std::error::Error>> {
    use wardwell::config::loader;
    use wardwell::domain::registry::DomainRegistry;

    // Read Stop hook JSON from stdin
    let mut input = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
    let hook_data: serde_json::Value = serde_json::from_str(&input)
        .unwrap_or_else(|_| serde_json::json!({}));

    let cwd = hook_data["cwd"].as_str().unwrap_or(".");

    let config = loader::load(None)?;
    let registry = DomainRegistry::from_domains(config.registry.all().to_vec());
    let cwd_path = std::path::Path::new(cwd);

    // Resolve domain from cwd
    let domain = match registry.resolve(cwd_path) {
        Some(d) => d,
        None => return Ok(()), // no domain → nothing to resolve
    };

    // Find project: walk vault domain dir, find project whose path matches cwd
    let domain_dir = config.vault_path.join(domain.name.as_str());
    if !domain_dir.exists() {
        return Ok(());
    }

    let project_name = find_project_for_cwd(&domain_dir, cwd_path);
    let project = match &project_name {
        Some(p) => p.as_str(),
        None => return Ok(()), // no project → nothing to resolve
    };

    // Read history.jsonl, find last source:desktop entry
    let history_path = domain_dir.join(project).join("history.jsonl");
    if !history_path.exists() {
        return Ok(());
    }

    let content = std::fs::read_to_string(&history_path)?;
    let last_desktop = content.lines()
        .rev()
        .filter(|line| !line.starts_with("{\"_schema\""))
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|entry| entry["source"].as_str() == Some("desktop"));

    let intent = match last_desktop {
        Some(entry) => entry,
        None => return Ok(()), // no desktop sync → nothing to resolve
    };

    // Check if a code sync already resolved this intent
    // (last entry with source:code is newer than the desktop entry)
    let desktop_date = intent["date"].as_str().unwrap_or("");
    let already_resolved = content.lines()
        .rev()
        .filter(|line| !line.starts_with("{\"_schema\""))
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .any(|entry| {
            entry["source"].as_str() == Some("code")
                && entry["date"].as_str().unwrap_or("") > desktop_date
        });

    if already_resolved {
        return Ok(()); // already synced from code since last desktop intent
    }

    // Build the block reason
    let focus = intent["focus"].as_str().unwrap_or("(no focus)");
    let next_action = intent["next_action"].as_str().unwrap_or("");

    let mut reason = format!(
        "Before ending: sync this session against the last Desktop intent.\n\n\
         **Intent (from Desktop):**\n- Focus: {focus}\n"
    );
    if !next_action.is_empty() {
        reason.push_str(&format!("- Next action: {next_action}\n"));
    }
    reason.push_str(&format!(
        "\nCall `wardwell_write` with action:sync, source:code for project {}/{project}. \
         Summarize what you accomplished against this intent. \
         If nothing meaningful happened, set the same focus and next_action to preserve the Desktop intent.",
        domain.name
    ));

    // Exit code 2 = block stop, continue conversation with reason
    let response = serde_json::json!({
        "decision": "block",
        "reason": reason,
    });
    println!("{}", serde_json::to_string(&response)?);
    std::process::exit(2);
}

/// Find which project directory under a domain matches the given cwd.
fn find_project_for_cwd(domain_dir: &Path, cwd: &Path) -> Option<String> {
    // Check if cwd basename matches a project dir
    let cwd_name = cwd.file_name()?.to_str()?;
    let candidate = domain_dir.join(cwd_name);
    if candidate.is_dir() && candidate.join("current_state.md").exists() {
        return Some(cwd_name.to_string());
    }

    // Check if any project's path patterns match cwd
    // (look at current_state.md frontmatter for context matching)
    for entry in std::fs::read_dir(domain_dir).ok()?.flatten() {
        if entry.path().is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == cwd_name {
                return Some(name);
            }
        }
    }
    None
}

fn run_reindex() -> Result<(), Box<dyn std::error::Error>> {
    use wardwell::config::loader;
    use wardwell::index::builder::IndexBuilder;
    use wardwell::index::store::IndexStore;

    let config = loader::load(None)?;
    let config_dir = loader::config_dir();
    let index_path = config_dir.join("index.db");

    let index = IndexStore::open(&index_path)?;

    // Clear existing data in-place (safe even if other processes hold the db open)
    index.clear()?;

    if !config.vault_path.exists() {
        println!("Vault directory does not exist: {}", config.vault_path.display());
        return Ok(());
    }

    let stats = IndexBuilder::build_filtered(&index, &config.vault_path, &config.exclude)?;
    println!("Reindexed {} file(s) ({} skipped, {} error(s)).", stats.indexed, stats.skipped, stats.errors);
    for detail in &stats.error_details {
        eprintln!("  error: {detail}");
    }
    Ok(())
}

fn run_seed(target: &str) -> Result<(), Box<dyn std::error::Error>> {
    use wardwell::config::loader;

    let config = loader::load(None)?;
    let vault_path = &config.vault_path;

    let parts: Vec<&str> = target.splitn(2, '/').collect();
    let domain = parts[0];

    if parts.len() == 1 {
        // Bare domain — just create the directory
        let domain_dir = vault_path.join(domain);
        std::fs::create_dir_all(&domain_dir)?;
        println!("{domain}/: domain directory ready");
        if let Ok(entries) = std::fs::read_dir(&domain_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let has_state = entry.path().join("current_state.md").exists();
                    let marker = if has_state { "\u{2713}" } else { " " };
                    println!("  [{marker}] {name}");
                }
            }
        }
        return Ok(());
    }

    let project = parts[1];
    let project_dir = vault_path.join(domain).join(project);

    if project_dir.exists() {
        eprintln!("Project already exists at {domain}/{project}/");
        return Ok(());
    }

    let title = slug_to_title(project);
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
    let rel = format!("{domain}/{project}");

    std::fs::create_dir_all(&project_dir)?;
    println!("  Creating  {rel}/                {:>width$}", "\u{2713}", width = 40_usize.saturating_sub(rel.len() + 12));

    // INDEX.md
    let index_path = project_dir.join("INDEX.md");
    std::fs::write(&index_path, format!("\
# {title}

## What
(one sentence — what is this)

## Why
(one sentence — why does this matter)

## Links
(related vault files, external URLs)
"))?;
    println!("  Writing   {rel}/INDEX.md         \u{2713}");

    // current_state.md
    let state_path = project_dir.join("current_state.md");
    std::fs::write(&state_path, format!("\
---
chat_name: {project}
updated: {now}
status: active
type: project
context: {domain}
---

# {title}

## Focus
(what are you working on right now)

## Next Action
(single concrete next step)

## Commit Message
Seeded by wardwell
"))?;
    println!("  Writing   {rel}/current_state.md \u{2713}");

    println!("\n  Done. Fill in the placeholders in INDEX.md and current_state.md.");
    Ok(())
}

fn slug_to_title(slug: &str) -> String {
    slug.split('-')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(c) => {
                    let mut s = c.to_uppercase().to_string();
                    s.extend(chars);
                    s
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

