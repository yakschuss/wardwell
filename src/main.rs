use clap::{Parser, Subcommand};
use std::path::Path;

#[derive(Parser)]
#[command(
    name = "wardwell",
    version,
    about = "Personal AI knowledge vault — MCP server"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the MCP server (stdio transport) with background daemon tasks
    Serve {
        /// Scope this server to a specific vault domain (also reads WARDWELL_DOMAIN env var)
        #[arg(long)]
        domain: Option<String>,
    },
    /// Inspect or configure the optional hosted Companion connection
    Companion {
        #[command(subcommand)]
        command: CompanionCommand,
    },
    /// First-run setup — generates config, injects MCP entries, installs hooks
    Init,
    /// Set up or repair Wardwell on this computer
    Setup {
        /// Show intended client-config changes without writing them
        #[arg(long)]
        dry_run: bool,
        /// Apply client-config changes without an interactive confirmation
        #[arg(long)]
        yes: bool,
    },
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
    /// Migrate kanban attachments from ~/.wardwell/attachments/ to vault docs/
    MigrateAttachments,
}

#[derive(Subcommand)]
enum CompanionCommand {
    /// Verify the installation can read hosted work; never prints credentials
    Status,
    /// Bootstrap an already-issued installation credential from standard input
    Connect {
        /// Read a credential from stdin, never a command-line argument
        #[arg(long, required = true)]
        token_stdin: bool,
    },
    /// Execute one bounded Companion request from JSON on standard input
    Request,
    /// Install or preview native lifecycle hooks
    Install {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        skill_file: Option<std::path::PathBuf>,
    },
    /// Record the current prompt's explicit handoff outcome
    Checkpoint {
        #[arg(long)]
        token: String,
        #[arg(long)]
        outcome: String,
        #[arg(long)]
        receipt_id: Option<String>,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Native hook entry points; reads the hook event JSON from standard input
    Lifecycle {
        #[command(subcommand)]
        command: LifecycleCommand,
    },
    /// Report locally observed lifecycle execution and checkpoint outcomes
    Coverage {
        #[arg(long)]
        client: Option<String>,
    },
}

#[derive(Subcommand)]
enum LifecycleCommand {
    Begin {
        #[arg(long)]
        client: String,
    },
    Resume {
        #[arg(long)]
        client: String,
    },
    Stop {
        #[arg(long)]
        client: String,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result: Result<(), Box<dyn std::error::Error>> = match cli.command {
        Commands::Serve { domain } => {
            let domain = domain.or_else(|| std::env::var("WARDWELL_DOMAIN").ok());
            run_serve(domain).await
        }
        Commands::Companion { command } => run_companion(command).await,
        Commands::Init => wardwell::install::init::run(),
        Commands::Setup { dry_run, yes } => wardwell::install::setup::run(dry_run, yes),
        Commands::Doctor => wardwell::install::doctor::run(),
        Commands::Uninstall => wardwell::install::uninstall::run(),
        Commands::Inject { ref path } => run_inject(path),
        Commands::Resolve => run_resolve(),
        Commands::Reindex => run_reindex(),
        Commands::Seed { ref target } => run_seed(target),
        Commands::MigrateAttachments => run_migrate_attachments(),
    };
    if let Err(e) = result {
        eprintln!("wardwell: {e}");
        std::process::exit(1);
    }
}

async fn run_companion(command: CompanionCommand) -> Result<(), Box<dyn std::error::Error>> {
    use wardwell::companion::{self, CompanionParams};
    let result = match command {
        CompanionCommand::Status => {
            companion::execute(CompanionParams {
                action: "status".into(),
                source_key: None,
                arguments: None,
                arguments_file: None,
            })
            .await
        }
        CompanionCommand::Connect { token_stdin: _ } => {
            use std::io::Read;
            let mut bytes = Vec::new();
            std::io::stdin().take(8193).read_to_end(&mut bytes)?;
            if bytes.len() > 8192 {
                return Err("Installation credential exceeds size limit".into());
            }
            let token =
                String::from_utf8(bytes).map_err(|_| "Installation credential must be UTF-8")?;
            companion::connect(token.trim_end_matches(['\r', '\n'])).await
        }
        CompanionCommand::Request => {
            let params = read_stdin_json::<CompanionParams>(200_000)?;
            companion::execute(params).await
        }
        CompanionCommand::Install {
            dry_run,
            skill_file,
        } => wardwell::companion::install::run(dry_run, skill_file.as_deref()),
        CompanionCommand::Checkpoint {
            token,
            outcome,
            receipt_id,
            reason,
        } => {
            let outcome = outcome.parse::<wardwell::companion::lifecycle::Outcome>()?;
            wardwell::companion::lifecycle::checkpoint(
                &token,
                outcome,
                receipt_id.as_deref(),
                reason.as_deref(),
            )
        }
        CompanionCommand::Lifecycle { command } => {
            let input = read_stdin_json::<serde_json::Value>(200_000)?;
            match command {
                LifecycleCommand::Begin { client } => {
                    let client = client.parse()?;
                    let mut output = wardwell::companion::lifecycle::begin(client, &input)?;
                    let source = wardwell::companion::lifecycle::begin_source(client, &input)?;
                    if let Some(plan_id) =
                        wardwell::companion::lifecycle::known_plan_id_for_source(&source)?
                    {
                        match refresh_companion_responses(client, &input, source, plan_id).await {
                            Ok(Some(context)) => {
                                let additional = output["hookSpecificOutput"]["additionalContext"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .to_owned();
                                output["hookSpecificOutput"]["additionalContext"] =
                                    serde_json::Value::String(format!("{additional}\n\n{context}"));
                            }
                            Ok(None) => {}
                            Err(_) => {
                                output["hookSpecificOutput"]["additionalContext"] =
                                    serde_json::Value::String(format!(
                                        "{}\n\nCompanion reply check unavailable; pending replies are retained. Use consume to retry before relying on an absent answer.",
                                        output["hookSpecificOutput"]["additionalContext"]
                                            .as_str()
                                            .unwrap_or_default()
                                    ));
                            }
                        }
                    }
                    Ok(output)
                }
                LifecycleCommand::Resume { client } => {
                    refresh_companion_resume(client.parse()?, &input).await
                }
                LifecycleCommand::Stop { client } => {
                    wardwell::companion::lifecycle::stop(client.parse()?, &input)
                }
            }
        }
        CompanionCommand::Coverage { client } => {
            let client = client.map(|value| value.parse()).transpose()?;
            wardwell::companion::lifecycle::coverage(client)
        }
    };
    match result {
        Ok(value) => {
            println!("{value}");
            Ok(())
        }
        Err(message) => Err(message.into()),
    }
}

async fn refresh_companion_resume(
    client: wardwell::companion::lifecycle::Client,
    input: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    use wardwell::companion::{self, CompanionParams};
    let Some(source) = wardwell::companion::lifecycle::resume_source(client, input)? else {
        return wardwell::companion::lifecycle::resume(client, input);
    };
    let refresh = async {
        let plan_id = match wardwell::companion::lifecycle::known_plan_id(client, input, &source)? {
            Some(id) => Some(id),
            None => {
                let list = companion::execute(CompanionParams {
                    action: "list".into(),
                    source_key: Some(source.clone()),
                    arguments: Some(serde_json::json!({})),
                    arguments_file: None,
                })
                .await?;
                let plans = list["work_plans"]
                    .as_array()
                    .ok_or("Hank returned an invalid source plan list")?;
                if plans.len() == 1 {
                    plans[0]["id"].as_str().map(str::to_owned)
                } else {
                    None
                }
            }
        };
        if let Some(id) = plan_id {
            companion::execute(CompanionParams {
                action: "consume".into(),
                source_key: Some(source.clone()),
                arguments: Some(
                    serde_json::json!({"id":id, "compact":true, "session_id":input["session_id"]}),
                ),
                arguments_file: None,
            })
            .await?;
        }
        Ok::<(), String>(())
    };
    let warning = match tokio::time::timeout(std::time::Duration::from_secs(5), refresh).await {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(format!("Companion response refresh was deferred: {error}")),
        Err(_) => Some(
            "Companion response refresh was deferred after the five-second startup limit."
                .to_string(),
        ),
    };
    let mut output = wardwell::companion::lifecycle::resume(client, input)?;
    if let Some(message) = warning {
        output["systemMessage"] = serde_json::Value::String(message);
    }
    Ok(output)
}

async fn refresh_companion_responses(
    _client: wardwell::companion::lifecycle::Client,
    input: &serde_json::Value,
    source: String,
    plan_id: String,
) -> Result<Option<String>, String> {
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), wardwell::companion::execute(wardwell::companion::CompanionParams {
        action: "consume".into(),
        source_key: Some(source),
        arguments: Some(serde_json::json!({"id": plan_id, "compact": true, "session_id": input["session_id"]})),
        arguments_file: None,
    })).await.map_err(|_| "Companion response refresh exceeded the five-second startup limit".to_string())??;
    Ok(wardwell::companion::lifecycle::response_context(&result))
}

fn read_stdin_json<T: serde::de::DeserializeOwned>(
    limit: u64,
) -> Result<T, Box<dyn std::error::Error>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::io::stdin().take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err("standard input exceeds the 200000-byte limit".into());
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| "standard input must contain valid request JSON".into())
}

async fn run_serve(domain: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    use rmcp::ServiceExt;
    use std::sync::Arc;
    use wardwell::config::loader;
    use wardwell::index::builder::IndexBuilder;
    use wardwell::index::store::IndexStore;
    use wardwell::mcp::server::WardwellServer;

    eprintln!("wardwell: loading config");
    let config = loader::load(None)?;

    let config_dir = loader::config_dir();

    // Open kanban BEFORE index — IndexStore registers sqlite-vec globally
    // which causes disk I/O errors on connections opened after it.
    let kanban = if config.kanban_enabled {
        let kanban_path = config_dir.join("kanban.db");
        let vault_root = config.vault_path.clone();
        match wardwell::kanban::store::KanbanStore::open(&kanban_path, vault_root) {
            Ok(k) => {
                eprintln!("wardwell: kanban enabled");
                Some(k)
            }
            Err(e) => {
                eprintln!("wardwell: kanban db error (disabled): {e}");
                None
            }
        }
    } else {
        None
    };

    let index_path = config_dir.join("index.db");
    eprintln!("wardwell: opening index");
    let index = IndexStore::open(&index_path)?;
    eprintln!("wardwell: index ready");

    // Index vault path on startup
    let mut all_index_roots: Vec<std::path::PathBuf> = Vec::new();
    if config.vault_path.exists() {
        all_index_roots.push(config.vault_path.clone());
    }

    let index = Arc::new(index);

    // Embedder starts as None — loaded in background so MCP server starts immediately
    let embedder: Arc<std::sync::Mutex<Option<wardwell::index::embed::Embedder>>> =
        Arc::new(std::sync::Mutex::new(None));

    // Index + load embedder in background
    let bg_index = Arc::clone(&index);
    let bg_roots = all_index_roots.clone();
    let bg_exclude = config.exclude.clone();
    let bg_embedder = Arc::clone(&embedder);
    let models_dir = config_dir.join("models");
    tokio::spawn(async move {
        // 1. Index with FTS only (fast, no embedder needed)
        for root in &bg_roots {
            match IndexBuilder::build_filtered(&bg_index, root, &bg_exclude, None) {
                Ok(stats) => {
                    if stats.indexed > 0 || stats.removed > 0 {
                        eprintln!(
                            "wardwell: indexed {} files from {} ({} skipped, {} removed, {} errors)",
                            stats.indexed,
                            root.display(),
                            stats.skipped,
                            stats.removed,
                            stats.errors
                        );
                    }
                }
                Err(e) => eprintln!("wardwell: index error for {}: {e}", root.display()),
            }
        }

        // 2. Load embedder (may download model ~33MB on first run)
        match wardwell::index::embed::Embedder::new(&models_dir) {
            Ok(e) => {
                eprintln!("wardwell: embedding model loaded");
                let mut guard = bg_embedder.lock().unwrap_or_else(|e| e.into_inner());
                *guard = Some(e);
                drop(guard);

                // 3. Re-index with embeddings for any files that need chunk vectors
                for root in &bg_roots {
                    let mut emb_guard = bg_embedder.lock().unwrap_or_else(|e| e.into_inner());
                    let result = IndexBuilder::build_filtered(
                        &bg_index,
                        root,
                        &bg_exclude,
                        emb_guard.as_mut(),
                    );
                    drop(emb_guard);
                    match result {
                        Ok(stats) => {
                            if stats.chunks_embedded > 0 {
                                eprintln!(
                                    "wardwell: embedded {} chunks from {}",
                                    stats.chunks_embedded,
                                    root.display()
                                );
                            }
                        }
                        Err(e) => eprintln!(
                            "wardwell: embedding index error for {}: {e}",
                            root.display()
                        ),
                    }
                }
            }
            Err(e) => {
                eprintln!("wardwell: embedding model unavailable (semantic search disabled): {e}");
            }
        }
    });

    eprintln!("wardwell: starting MCP server");
    let server = WardwellServer::new(config, Arc::clone(&index), embedder, domain, kanban);
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
            if let Err(e) = wardwell::daemon::watcher::watch_vault(
                root.clone(),
                watcher_index,
                registry_for_watcher,
            )
            .await
            {
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
        run_daemon_loop(
            sessions_db,
            session_sources,
            domains,
            summaries_dir,
            ai_config,
        )
        .await;
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
                    eprintln!(
                        "wardwell: indexed {} sessions ({} skipped, {} errors)",
                        stats.indexed, stats.skipped, stats.errors
                    );
                }
            }
            Err(e) => eprintln!("wardwell: session indexing error: {e}"),
        }

        // 2. Summarize via claude CLI
        match summarizer::summarize_pending(
            &session_store,
            &session_sources,
            &summaries_dir,
            &ai_config.summarize_model,
            false,
        )
        .await
        {
            Ok(stats) => {
                if stats.summarized > 0 {
                    eprintln!(
                        "wardwell: summarized {} sessions ({} skipped, {} errors)",
                        stats.summarized, stats.skipped, stats.errors
                    );
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
    let cwd_name = cwd_path.file_name().and_then(|n| n.to_str()).unwrap_or("");

    let matched_domain = std::fs::read_dir(vault_path).ok().and_then(|entries| {
        entries
            .flatten()
            .find(|e| e.path().is_dir() && e.file_name().to_string_lossy() == cwd_name)
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
    let domain = domain_dir
        .file_name()
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
                    let project = p.file_name().and_then(|n| n.to_str()).unwrap_or("unknown");
                    let status = vf
                        .frontmatter
                        .status
                        .as_ref()
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
    // No-op. Session logging is handled by CLAUDE.md behavioral rules.
    // The hook entry is kept so wardwell can re-enable blocking if
    // Claude Code adds a silent block mechanism.
    Ok(())
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
        println!(
            "Vault directory does not exist: {}",
            config.vault_path.display()
        );
        return Ok(());
    }

    // Initialize embedder for vector index
    let models_dir = config_dir.join("models");
    let mut embedder = match wardwell::index::embed::Embedder::new(&models_dir) {
        Ok(e) => {
            println!("Embedding model loaded.");
            Some(e)
        }
        Err(e) => {
            eprintln!("Embedding model unavailable (skipping vector index): {e}");
            None
        }
    };

    let stats = IndexBuilder::build_filtered(
        &index,
        &config.vault_path,
        &config.exclude,
        embedder.as_mut(),
    )?;
    println!(
        "Reindexed {} file(s) ({} skipped, {} error(s)).",
        stats.indexed, stats.skipped, stats.errors
    );
    if stats.chunks_embedded > 0 {
        println!("Embedded {} chunks.", stats.chunks_embedded);
    }
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
    println!(
        "  Creating  {rel}/                {:>width$}",
        "\u{2713}",
        width = 40_usize.saturating_sub(rel.len() + 12)
    );

    // INDEX.md
    let index_path = project_dir.join("INDEX.md");
    std::fs::write(
        &index_path,
        format!(
            "\
# {title}

## What
(one sentence — what is this)

## Why
(one sentence — why does this matter)

## Links
(related vault files, external URLs)
"
        ),
    )?;
    println!("  Writing   {rel}/INDEX.md         \u{2713}");

    // current_state.md
    let state_path = project_dir.join("current_state.md");
    std::fs::write(
        &state_path,
        format!(
            "\
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
"
        ),
    )?;
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

fn run_migrate_attachments() -> Result<(), Box<dyn std::error::Error>> {
    use wardwell::config::loader;
    use wardwell::kanban::events;

    let config = loader::load(None)?;
    let vault_root = &config.vault_path;
    let config_dir = loader::config_dir();
    let old_dir = config_dir.join("attachments");

    if !old_dir.exists() {
        println!("No ~/.wardwell/attachments/ directory found — nothing to migrate.");
        return Ok(());
    }

    let all = events::scan_all_jsonl(vault_root);
    let mut migrated = 0u32;
    let mut skipped = 0u32;

    for (domain, project, evts) in &all {
        let items = events::materialize(domain, evts);
        for item in &items {
            for att in &item.attachments {
                // Check if storage_path points to old internal location
                let old_path = config_dir.join("attachments").join(&att.storage_path);
                if !old_path.exists() {
                    // Try the old format: {ticket_id}/{uuid}-{filename}
                    let alt_path = old_dir.join(&att.storage_path);
                    if !alt_path.exists() {
                        skipped += 1;
                        continue;
                    }
                }

                let source = if old_path.exists() {
                    &old_path
                } else {
                    skipped += 1;
                    continue;
                };

                // New destination in vault docs
                let docs_dir = vault_root.join(domain).join(project).join("docs");
                std::fs::create_dir_all(&docs_dir)?;
                let dest_filename = format!("{}-{}", item.ticket_id, att.filename);
                let dest = docs_dir.join(&dest_filename);

                if dest.exists() {
                    skipped += 1;
                    continue;
                }

                std::fs::copy(source, &dest)?;
                println!(
                    "  {} → {domain}/{project}/docs/{dest_filename}",
                    att.storage_path
                );
                migrated += 1;
            }
        }
    }

    println!("\nMigrated: {migrated}, Skipped: {skipped}");
    if migrated > 0 {
        println!(
            "Note: Old files left in ~/.wardwell/attachments/ — delete manually after verifying."
        );
    }
    Ok(())
}
