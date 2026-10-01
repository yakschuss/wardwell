use crate::config::loader::{self, config_dir};
use crate::install::detect;
use crate::install::mcp_config::{self, ClientStatus, EntryStatus, McpConfigPaths};
use std::path::Path;

/// Run diagnostic checks.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    println!("wardwell doctor\n");

    let mut all_ok = true;

    // 1. Config
    let config_path = config_dir().join("config.yml");
    if config_path.exists() {
        match loader::load(Some(&config_path)) {
            Ok(config) => {
                println!(
                    "  Config                                 \u{2713} vault: {}",
                    config.vault_path.display()
                );

                // Vault directory + file count
                if config.vault_path.exists() {
                    let md_count = count_md_files(&config.vault_path, &config.exclude);
                    println!(
                        "  Vault                                  \u{2713} {} .md files",
                        md_count
                    );
                } else {
                    println!("  Vault                                  \u{2717}");
                    println!("    {} does not exist", config.vault_path.display());
                    all_ok = false;
                }

                // Domains — derived from vault subdirectories
                if config.vault_path.exists() {
                    let domains = list_vault_domains(&config.vault_path);
                    if domains.is_empty() {
                        println!(
                            "  Domains                                \u{2717} no subdirectories in vault"
                        );
                    } else {
                        println!(
                            "  Domains                                \u{2713} {}",
                            domains.join(", ")
                        );
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
                        println!(
                            "  Index                                  \u{2713} {} entries ({})",
                            count, size
                        );
                    } else {
                        println!(
                            "  Index                                  \u{2717} could not open"
                        );
                        all_ok = false;
                    }
                } else {
                    println!(
                        "  Index                                  \u{2717} not built yet (run `wardwell serve`)"
                    );
                    all_ok = false;
                }

                // Excluded patterns
                if !config.exclude.is_empty() {
                    println!(
                        "  Excluded                               \u{2713} {}",
                        config.exclude.join(", ")
                    );
                }

                // Sessions
                let sessions_db = config_dir().join("sessions.db");
                if sessions_db.exists()
                    && let Ok(store) = crate::daemon::indexer::SessionStore::open(&sessions_db)
                    && let Ok(count) = store.count()
                {
                    println!(
                        "  Sessions                               \u{2713} {} indexed",
                        count
                    );
                }

                // Mapped projects: paths, ages, last Stop-check block
                let (rows, projects_ok) = project_rows(&config, &config_dir(), chrono::Utc::now(), crate::inject::git::dirs);
                for row in rows {
                    println!("{row}");
                }
                all_ok &= projects_ok;

                // Tracker bindings: offline checks only
                let (rows, trackers_ok) = tracker_rows(&config, &config_dir());
                for row in rows {
                    println!("{row}");
                }
                all_ok &= trackers_ok;
                for row in mirror_rows(&config, chrono::Utc::now()) {
                    println!("{row}");
                }

                // MCP configs
                let mcp_paths = McpConfigPaths::detect();
                let binary_path = detect::find_binary_path();

                // Tracker policy and pull service: offline reads only
                let home = dirs::home_dir().unwrap_or_default();
                let (rows, policy_ok) = policy_rows(&config, &home, &config_dir(), &binary_path, cfg!(target_os = "macos"));
                for row in rows {
                    println!("{row}");
                }
                all_ok &= policy_ok;

                if detect::command_available("claude") || mcp_paths.claude_code.exists() {
                    print_client_status(
                        "Claude Code",
                        mcp_config::inspect_claude_code(&mcp_paths.claude_code, &binary_path),
                        true,
                        &mut all_ok,
                    );
                }
                if Path::new("/Applications/Claude.app").exists()
                    || mcp_paths.claude_desktop.exists()
                {
                    print_client_status(
                        "Claude Desktop",
                        mcp_config::inspect_claude_desktop(&mcp_paths.claude_desktop, &binary_path),
                        false,
                        &mut all_ok,
                    );
                }
                if detect::command_available("codex") || mcp_paths.codex.exists() {
                    print_client_status(
                        "Codex",
                        mcp_config::inspect_codex(&mcp_paths.codex, &binary_path),
                        true,
                        &mut all_ok,
                    );
                }

                // CLAUDE.md pointers
                let domain_paths: Vec<String> = config
                    .registry
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
                    println!(
                        "  CLAUDE.md pointer                      \u{2717} no wardwell markers"
                    );
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
                    println!(
                        "  Claude CLI                             \u{2713} {} available",
                        config.ai.summarize_model
                    );
                } else {
                    println!(
                        "  Claude CLI                             \u{2717} `claude` not found"
                    );
                    all_ok = false;
                }
            }
            Err(e) => {
                println!("  Config                                 \u{2717} parse error: {e}");
                all_ok = false;
            }
        }
    } else {
        println!(
            "  Config                                 \u{2717} not found. Run `wardwell init`."
        );
        all_ok = false;
    }

    println!();
    if all_ok {
        println!("  Configuration checks passed.");
        println!("  OAuth and a publish/refresh smoke are still required connection proof.");
    } else {
        println!("  Some checks failed. Run `wardwell init` to fix.");
    }

    Ok(())
}

/// Rows for the Linear gate, its ruleset, the deny list, and a launchd pull
/// agent when one exists, read offline from Claude settings and the plist.
/// An agent fails its row when the vault is under a folder macOS protects.
/// Whether launchd has the agent loaded is not checked. The bool is false
/// when a row failed.
pub(crate) fn policy_rows(config: &crate::config::loader::WardwellConfig, home: &Path, config_dir: &Path, binary: &Path, launchd: bool) -> (Vec<String>, bool) {
    use crate::gate::ruleset::LINEAR_UPDATES;
    use crate::install::client_hooks::{self, GATE};
    const FIX: &str = "run `wardwell setup`";
    let policy = crate::install::installer::policy_enabled(&config.trackers);
    let settings_path = crate::install::installer::settings_path(home);
    let settings: Option<serde_json::Value> = std::fs::read(&settings_path).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let settings = settings.unwrap_or_default();
    let gate = client_hooks::placements(&settings, &GATE);
    let covers = |matcher: Option<&str>| {
        [crate::gate::linear::COMMENT_TOOL, crate::gate::linear::ISSUE_TOOL].iter().all(|tool| client_hooks::matcher_covers(matcher, tool))
    };
    let mut rows = vec![format!("  {:<38} \u{2713} {} v{}", "Gate ruleset", LINEAR_UPDATES.name, LINEAR_UPDATES.version)];
    let mut ok = true;
    // `None` is a row with no mark: a fact, not a check.
    let mut row = |label: &str, pass: Option<bool>, text: String| {
        ok &= pass != Some(false);
        match pass {
            Some(pass) => rows.push(format!("  {label:<38} {} {text}", if pass { '\u{2713}' } else { '\u{2717}' })),
            None => rows.push(format!("  {label:<38} {text}")),
        }
    };
    let first = gate
        .iter()
        .find(|(matcher, _)| covers(matcher.as_deref()))
        .or(gate.first())
        .map(|(matcher, command)| (matcher.as_deref(), client_hooks::executable(command)));
    let uncovered = first.filter(|(matcher, _)| policy && !covers(*matcher)).and_then(|(matcher, _)| matcher);
    match (policy, first.and_then(|(_, exe)| exe)) {
        (true, Some(_)) if uncovered.is_some() => row("Linear gate", Some(false), format!(
            "installed under matcher {}, which does not match Linear writes; edit that matcher or remove the handler, then run `wardwell setup`",
            uncovered.unwrap_or_default()
        )),
        (true, Some(exe)) if same_file(Path::new(exe), binary) => row("Linear gate", Some(true), format!("installed; runs {exe}")),
        (true, Some(exe)) => row("Linear gate", Some(false), format!("runs {exe}, not this binary {}; {FIX}", binary.display())),
        (true, None) => row("Linear gate", Some(false), format!("not installed; {FIX}")),
        (false, Some(_)) => row("Linear gate", Some(false), format!("installed, but no linear binding has gate: true; {FIX} to remove it")),
        (false, None) => row("Linear gate", None, "off; no linear binding has gate: true".to_string()),
    }
    if policy {
        let denied = client_hooks::denied(&settings);
        let missing: Vec<&str> = LINEAR_UPDATES.denied_tools.iter().copied().filter(|t| !denied.iter().any(|d| d == t)).collect();
        let total = LINEAR_UPDATES.denied_tools.len();
        match missing.is_empty() {
            true => row("Linear deny list", Some(true), format!("{total} of {total} destructive tools denied")),
            false => row("Linear deny list", Some(false), format!("missing {}; {FIX}", missing.join(", "))),
        }
    }
    if launchd {
        use crate::tracker::schedule::{self, Agent};
        const LABEL: &str = "Tracker pull service";
        let protected = schedule::is_protected(&config.vault_path, home);
        match (schedule::agent(home, config_dir), protected) {
            (Agent::Absent, _) => {}
            (_, true) => row(LABEL, Some(false), format!(
                "a launchd agent runs the pull, and the vault is under a folder macOS protects. {} Run `wardwell tracker unschedule` to remove it.",
                schedule::PROTECTED_SENTENCE
            )),
            (Agent::Owned { program, .. }, false) if !program.exists() => row(LABEL, Some(false), format!("plist runs {}, which does not exist; run `wardwell tracker schedule` again or `wardwell tracker unschedule`", program.display())),
            (Agent::Owned { interval, .. }, false) => row(LABEL, Some(true), format!(
                "plist present, every {interval}s, as `wardwell tracker schedule` wrote it; whether launchd loaded it is not checked offline"
            )),
            (Agent::Foreign, false) => row(LABEL, None, "a plist at com.wardwell.tracker-pull that Wardwell did not write; left alone".to_string()),
        }
    }
    (rows, ok)
}

fn same_file(a: &Path, b: &Path) -> bool {
    let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    real(a) == real(b)
}

/// One row per tracker binding from the offline doctor checks (credential
/// file and provider; for github, a token or a `gh` that `locate_gh` finds; no network, and
/// `gh` never runs). Each row names `wardwell tracker doctor` for the live
/// check. The bool is false when any binding failed.
fn tracker_rows(config: &crate::config::loader::WardwellConfig, config_dir: &Path) -> (Vec<String>, bool) {
    let native = crate::tracker::doctor::native_prefixes(config, &config_dir.join("kanban.db"));
    tracker_rows_with(config, config_dir, &native, crate::tracker::github::gh_available_for(&config.trackers))
}

/// `tracker_rows` with each binding's native kanban prefix given by key and
/// whether a `gh` was found. A team key equal to the prefix fails the row
/// with the collision sentence.
fn tracker_rows_with(
    config: &crate::config::loader::WardwellConfig,
    config_dir: &Path,
    native: &std::collections::BTreeMap<String, String>,
    gh_on_path: bool,
) -> (Vec<String>, bool) {
    const LIVE: &str = "live check: `wardwell tracker doctor`";
    let mut ok = true;
    let rows = config
        .trackers
        .iter()
        .map(|binding| {
            let key = binding.key();
            let label = format!("Tracker {key} {}", binding.provider);
            let collision = crate::tracker::doctor::prefix_failure(binding, native.get(&key))
                .filter(|_| crate::tracker::mirrors_issues(&binding.provider))
                .map(|sentence| (crate::tracker::events::FailureCode::PrefixCollision, Some(sentence)));
            let passed = match crate::tracker::mirrors_issues(&binding.provider) {
                true => "credential ok",
                false => match gh_on_path {
                    true => "gh found or a token stored",
                    false => "token stored",
                },
            };
            match crate::tracker::doctor::check_offline_with(config_dir, binding, gh_on_path).and(collision.map_or(Ok(()), Err)) {
                Ok(()) => format!("  {label:<38} \u{2713} {passed}; {LIVE}"),
                Err((code, detail)) => {
                    ok = false;
                    let detail = detail.map(|d| format!(": {d}")).unwrap_or_default();
                    format!("  {label:<38} \u{2717} failed ({}){detail}; {LIVE}", code.as_str())
                }
            }
        })
        .collect();
    (rows, ok)
}

/// One fact row per binding with the mirror's freshness words: fresh,
/// stale and why, or the pull running since a time. Never fails doctor.
fn mirror_rows(config: &crate::config::loader::WardwellConfig, now: chrono::DateTime<chrono::Utc>) -> Vec<String> {
    config
        .trackers
        .iter()
        .map(|binding| {
            let label = format!("Mirror {} {}", binding.key(), binding.provider);
            format!("  {label:<38} {}", freshness_words(config, binding, now))
        })
        .collect()
}

/// The freshness words for one binding, read from its log.
fn freshness_words(config: &crate::config::loader::WardwellConfig, binding: &crate::config::loader::TrackerBinding, now: chrono::DateTime<chrono::Utc>) -> String {
    let path = crate::tracker::log::path_for(&config.vault_path, &binding.domain, &binding.project);
    let view = crate::tracker::view::MirrorView::read_for(&path, &binding.provider).unwrap_or_default();
    crate::tracker::freshness::assess(&view, now, &crate::tracker::freshness::process_alive).sentence()
}

/// One row per mapped project: whether each path exists, the age of the
/// last history entry and decision, the last pull when bound, and the last
/// Stop-check block. A missing path, a path inside a linked worktree, or a
/// missing vault folder fails the row.
fn project_rows(config: &crate::config::loader::WardwellConfig, config_dir: &Path, now: chrono::DateTime<chrono::Utc>, git: impl Fn(&Path) -> Option<crate::inject::git::GitDirs>) -> (Vec<String>, bool) {
    let mut ok = true;
    let rows = config
        .projects
        .iter()
        .map(|(key, mapping)| {
            let label = format!("Project {key}");
            let folder = config.vault_path.join(&mapping.domain).join(&mapping.project);
            let checked: Vec<(String, bool)> = mapping.paths.iter().map(|p| path_status(p, &git)).collect();
            let paths: Vec<String> = checked.iter().map(|(text, _)| text.clone()).collect();
            let row_ok = folder.is_dir() && checked.iter().all(|(_, fine)| *fine);
            ok &= row_ok;
            let mark = if row_ok { '\u{2713}' } else { '\u{2717}' };
            if !folder.is_dir() {
                return format!("  {label:<38} {mark} {}. No vault folder; run `wardwell seed {key}`.", paths.join(", "));
            }
            let today = now.with_timezone(&chrono::Local).date_naive();
            let rot = crate::inject::session::project_rot_line(&folder, today);
            format!("  {label:<38} {mark} {}. {rot}{} {}", paths.join(", "), last_pull(config, mapping, now), last_block(config_dir, key, now))
        })
        .collect();
    (rows, ok)
}

/// One mapped path as doctor reports it, and whether it is usable. A path
/// inside a linked worktree is never used: worktrees resolve through their
/// main checkout, so that is what to map.
fn path_status(path: &Path, git: &impl Fn(&Path) -> Option<crate::inject::git::GitDirs>) -> (String, bool) {
    if !path.is_dir() {
        return (format!("{} missing", path.display()), false);
    }
    let real = crate::inject::git::canonical(path);
    match git(&real).and_then(|dirs| dirs.in_main_worktree(&real)).filter(|main| *main != real) {
        Some(main) => (format!("{} is a linked worktree and is never used; map its main checkout {} instead", path.display(), main.display()), false),
        None => (format!("{} exists", path.display()), true),
    }
}

/// The freshness words for a bound project, such as ` Last pulled 45m ago.`,
/// with a leading space; empty when it is not bound.
fn last_pull(config: &crate::config::loader::WardwellConfig, mapping: &crate::config::loader::ProjectMapping, now: chrono::DateTime<chrono::Utc>) -> String {
    config.tracker_for(&mapping.domain, &mapping.project).map_or(String::new(), |binding| format!(" {}", freshness_words(config, binding, now)))
}

fn last_block(config_dir: &Path, key: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    let state = crate::stop_check::state_dir(config_dir);
    crate::stop_check::last_block(&state, key).map_or("No stop-check blocks.".to_string(), |(at, commits)| {
        let noun = if commits == 1 { "commit" } else { "commits" };
        format!("Last stop-check block {} ago, {commits} {noun}.", crate::tracker::view::age_words(now - at))
    })
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

    let entries = match config
        .get("hooks")
        .and_then(|h| h.get("SessionStart"))
        .and_then(|s| s.as_array())
    {
        Some(e) => e,
        None => return false,
    };

    entries.iter().any(|entry| {
        entry
            .get("command")
            .and_then(|c| c.as_str())
            .is_some_and(|c| c.contains("wardwell"))
            || entry
                .get("hooks")
                .and_then(|h| h.as_array())
                .is_some_and(|hooks| {
                    hooks.iter().any(|h| {
                        h.get("command")
                            .and_then(|c| c.as_str())
                            .is_some_and(|c| c.contains("wardwell"))
                    })
                })
    })
}

fn print_client_status(
    client: &str,
    status: ClientStatus,
    expects_remote: bool,
    all_ok: &mut bool,
) {
    print_entry_status(&format!("{client} · local context"), status.local, all_ok);
    if expects_remote {
        print_hosted_entry_status(&format!("{client} · hosted app"), status.remote, all_ok);
    } else {
        println!("  {client:<30} · hosted app    account connector");
    }
    if status.legacy_remote {
        println!("  {client:<30} · old connection\u{2717} static/proxy connection remains");
        *all_ok = false;
    }
}

fn print_hosted_entry_status(label: &str, status: EntryStatus, all_ok: &mut bool) {
    if status == EntryStatus::Configured {
        println!("  {label:<40} registered; OAuth unverified");
    } else {
        print_entry_status(label, status, all_ok);
    }
}

fn print_entry_status(label: &str, status: EntryStatus, all_ok: &mut bool) {
    match status {
        EntryStatus::Configured => println!("  {label:<40} \u{2713} configured"),
        EntryStatus::ConfigMissing => {
            println!("  {label:<40} \u{2717} client config missing");
            *all_ok = false;
        }
        EntryStatus::ParseError => {
            println!("  {label:<40} \u{2717} malformed; left untouched");
            *all_ok = false;
        }
        EntryStatus::Missing => {
            println!("  {label:<40} \u{2717} not configured");
            *all_ok = false;
        }
        EntryStatus::WrongTarget => {
            println!("  {label:<40} \u{2717} points somewhere unexpected");
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn format_size_bytes() {
        assert_eq!(format_size(500), "500B");
    }

    #[test]
    fn format_size_kilobytes() {
        assert_eq!(format_size(1_500), "1KB");
    }

    #[test]
    fn format_size_megabytes() {
        assert_eq!(format_size(2_500_000), "2MB");
    }

    #[test]
    fn format_size_zero() {
        assert_eq!(format_size(0), "0B");
    }

    #[test]
    fn list_vault_domains_with_subdirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("personal")).unwrap();
        std::fs::create_dir(dir.path().join("work")).unwrap();
        // File should not appear
        std::fs::File::create(dir.path().join("readme.md")).unwrap();
        let domains = list_vault_domains(dir.path());
        assert_eq!(domains, vec!["personal", "work"]);
    }

    #[test]
    fn list_vault_domains_empty() {
        let dir = tempfile::tempdir().unwrap();
        let domains = list_vault_domains(dir.path());
        assert!(domains.is_empty());
    }

    #[test]
    fn count_md_files_with_exclusion() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Create .md files
        std::fs::write(root.join("note.md"), "---\ntype: reference\n---\n# Note\n").unwrap();
        std::fs::write(
            root.join("other.md"),
            "---\ntype: reference\n---\n# Other\n",
        )
        .unwrap();
        // Create excluded subdir
        let excluded = root.join("node_modules");
        std::fs::create_dir(&excluded).unwrap();
        std::fs::write(
            excluded.join("pkg.md"),
            "---\ntype: reference\n---\n# Pkg\n",
        )
        .unwrap();
        let count = count_md_files(root, &["node_modules".to_string()]);
        assert_eq!(count, 2);
    }

    #[test]
    fn check_session_start_hook_detects_nested() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let json = serde_json::json!({
            "hooks": {
                "SessionStart": [{
                    "hooks": [{"type": "command", "command": "wardwell inject $(pwd)"}]
                }]
            }
        });
        std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();
        assert!(check_session_start_hook(&path));
    }

    fn tracker_config(dir: &Path, provider: &str) -> crate::config::loader::WardwellConfig {
        let vault = dir.join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        let yaml = format!(
            "vault_path: {}\nsession_sources: []\ntrackers:\n  work/claims:\n    provider: linear\n    team: COR\n    credential: corr-linear\n",
            vault.display()
        );
        std::fs::write(dir.join("config.yml"), yaml).unwrap();
        let mut config = loader::load(Some(&dir.join("config.yml"))).unwrap();
        config.trackers[0].provider = provider.into();
        config
    }

    #[test]
    fn tracker_rows_use_the_row_format_and_name_the_live_check() {
        let dir = tempfile::tempdir().unwrap();
        let config = tracker_config(dir.path(), "linear");
        let (rows, ok) = tracker_rows(&config, dir.path());
        assert!(!ok);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].starts_with("  Tracker work/claims linear             \u{2717} failed (credential): tracker credential not configured"), "{}", rows[0]);
        assert!(rows[0].ends_with("; live check: `wardwell tracker doctor`"), "{}", rows[0]);
        assert_eq!(rows[0].find('\u{2717}'), "  Config                                 \u{2713}".find('\u{2713}'), "marks line up with the other rows");

        let path = crate::tracker::credential::path_in(dir.path(), "corr-linear").unwrap();
        crate::tracker::credential::save(&path, "lin_api_secret").unwrap();
        let (rows, ok) = tracker_rows(&config, dir.path());
        assert!(ok);
        assert_eq!(rows, vec!["  Tracker work/claims linear             \u{2713} credential ok; live check: `wardwell tracker doctor`"]);
        assert!(!rows[0].contains("lin_api_secret"));

        let jira = tracker_config(dir.path(), "jira");
        let (rows, ok) = tracker_rows(&jira, dir.path());
        assert!(!ok);
        assert!(rows[0].contains("\u{2717} failed (unsupported_provider); live check"), "{}", rows[0]);
    }

    #[test]
    fn a_team_key_equal_to_a_native_prefix_fails_the_row() {
        let dir = tempfile::tempdir().unwrap();
        let config = tracker_config(dir.path(), "linear");
        let path = crate::tracker::credential::path_in(dir.path(), "corr-linear").unwrap();
        crate::tracker::credential::save(&path, "t").unwrap();
        let native = std::collections::BTreeMap::from([("work/claims".to_string(), "COR".to_string())]);
        let (rows, ok) = tracker_rows_with(&config, dir.path(), &native, false);
        assert!(!ok);
        assert_eq!(
            rows,
            vec!["  Tracker work/claims linear             \u{2717} failed (prefix_collision): Tracker team key COR of work/claims equals the native kanban prefix COR of project claims. Set a different native prefix for claims in kanban.prefixes.; live check: `wardwell tracker doctor`"]
        );
    }

    #[test]
    fn each_binding_of_a_project_gets_its_row_and_github_names_the_connect_command() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = format!(
            "vault_path: {}\nsession_sources: []\ntrackers:\n  work/claims:\n    - provider: linear\n      team: COR\n      credential: corr-linear\n    - provider: github\n      repository: acme/app\n",
            dir.path().join("vault").display()
        );
        let config = loader::parse(&yaml).unwrap();
        crate::tracker::credential::save(&crate::tracker::credential::path_in(dir.path(), "corr-linear").unwrap(), "t").unwrap();
        let native = std::collections::BTreeMap::from([("work/claims".to_string(), "COR".to_string())]);
        let (rows, ok) = tracker_rows_with(&config, dir.path(), &native, false);
        assert!(!ok);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows[0].starts_with("  Tracker work/claims linear             \u{2717} failed (prefix_collision)"), "{}", rows[0]);
        assert_eq!(rows[1], "  Tracker work/claims github             \u{2717} failed (credential): github: unreachable, run `wardwell tracker connect github`; live check: `wardwell tracker doctor`");

        let (rows, _) = tracker_rows_with(&config, dir.path(), &std::collections::BTreeMap::new(), true);
        assert_eq!(rows[1], "  Tracker work/claims github             \u{2713} gh found or a token stored; live check: `wardwell tracker doctor`");
        crate::tracker::credential::save(&crate::tracker::credential::path_in(dir.path(), "github").unwrap(), "ghp_secret").unwrap();
        let (rows, ok) = tracker_rows_with(&config, dir.path(), &std::collections::BTreeMap::new(), false);
        assert!(ok, "{rows:?}");
        assert_eq!(rows[1], "  Tracker work/claims github             \u{2713} token stored; live check: `wardwell tracker doctor`");
        assert!(!rows.join("\n").contains("ghp_secret"));
    }

    #[test]
    fn mirror_rows_carry_the_freshness_words_for_each_binding() {
        let dir = tempfile::tempdir().unwrap();
        let config = tracker_config(dir.path(), "linear");
        let now = chrono::Utc::now();
        assert_eq!(mirror_rows(&config, now), vec![format!("  {:<38} Never pulled. Stale. Reason: No pull was tried.", "Mirror work/claims linear")]);
        let path = crate::tracker::log::path_for(&config.vault_path, "work", "claims");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let at = |minutes: i64| (now - chrono::TimeDelta::minutes(minutes)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let completed = format!(r#"{{"kind":"pull_completed","id":"p","provider":"linear","external_key":"COR","external_id":"COR","occurred_at":"{}","title":"p"}}"#, at(300));
        let started = format!(r#"{{"kind":"pull_started","id":"s","provider":"linear","external_key":"COR","external_id":"COR","occurred_at":"{}","title":"s","pid":{}}}"#, at(1), std::process::id());
        std::fs::write(&path, format!("{}\n{completed}\n{started}\n", crate::tracker::events::SCHEMA_HEADER)).unwrap();
        assert!(mirror_rows(&config, now)[0].ends_with(&format!("Last pulled 5h ago; pull running since {}.", at(1))), "{:?}", mirror_rows(&config, now));
    }

    #[test]
    fn no_bindings_no_tracker_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = tracker_config(dir.path(), "linear");
        config.trackers.clear();
        assert_eq!(tracker_rows(&config, dir.path()), (vec![], true));
    }

    #[test]
    fn check_session_start_hook_missing_file() {
        assert!(!check_session_start_hook(std::path::Path::new(
            "/nonexistent"
        )));
    }

    fn project_config(dir: &Path, paths: &[&Path], bound: bool) -> crate::config::loader::WardwellConfig {
        let vault = dir.join("vault");
        std::fs::create_dir_all(vault.join("personal/corr")).unwrap();
        let list: Vec<String> = paths.iter().map(|p| format!("\"{}\"", p.display())).collect();
        let mut yaml = format!("vault_path: {}\nsession_sources: []\nprojects:\n  personal/corr:\n    paths: [{}]\n", vault.display(), list.join(", "));
        if bound {
            yaml.push_str("trackers:\n  personal/corr:\n    provider: linear\n    team: COR\n    credential: c\n");
        }
        loader::parse(&yaml).unwrap()
    }

    fn noon() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-09-30T12:00:00Z").unwrap().with_timezone(&chrono::Utc)
    }

    #[test]
    fn project_rows_show_paths_ages_and_the_last_block_in_the_row_format() {
        let dir = tempfile::tempdir().unwrap();
        let code = dir.path().join("code");
        std::fs::create_dir_all(&code).unwrap();
        let config = project_config(dir.path(), &[&code], false);
        let project = dir.path().join("vault/personal/corr");
        std::fs::write(project.join("history.jsonl"), "{\"date\":\"2026-09-28T10:00:00Z\",\"title\":\"x\"}\n").unwrap();
        let (rows, ok) = project_rows(&config, dir.path(), noon(), |_: &Path| None);
        assert!(ok);
        assert_eq!(rows, vec![format!("  {:<38} \u{2713} {} exists. Last history entry 2 days ago. No decisions. No stop-check blocks.", "Project personal/corr", code.display())]);
        assert_eq!(rows[0].find('\u{2713}'), "  Config                                 \u{2713}".find('\u{2713}'), "marks line up");

        let state = crate::stop_check::state_dir(dir.path());
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join(crate::stop_check::LOG), "{\"at\":\"2026-09-30T09:00:00Z\",\"project\":\"personal/corr\",\"commits\":2,\"session_id\":\"s\",\"since\":\"2026-09-30T08:00:00Z\"}\n").unwrap();
        let (rows, _) = project_rows(&config, dir.path(), noon(), |_: &Path| None);
        assert!(rows[0].ends_with("Last stop-check block 3 hours ago, 2 commits."), "{}", rows[0]);
    }

    #[test]
    fn a_missing_path_or_vault_folder_fails_the_row() {
        let dir = tempfile::tempdir().unwrap();
        let code = dir.path().join("code");
        let gone = dir.path().join("gone");
        std::fs::create_dir_all(&code).unwrap();
        let config = project_config(dir.path(), &[&code, &gone], false);
        let (rows, ok) = project_rows(&config, dir.path(), noon(), |_: &Path| None);
        assert!(!ok);
        assert!(rows[0].contains(&format!("\u{2717} {} exists, {} missing.", code.display(), gone.display())), "{}", rows[0]);

        std::fs::remove_dir_all(dir.path().join("vault/personal/corr")).unwrap();
        let config = project_config(dir.path(), &[&code], false);
        std::fs::remove_dir_all(dir.path().join("vault/personal/corr")).unwrap();
        let (rows, ok) = project_rows(&config, dir.path(), noon(), |_: &Path| None);
        assert!(!ok);
        assert!(rows[0].contains("\u{2717}") && rows[0].contains("No vault folder; run `wardwell seed personal/corr`."), "{}", rows[0]);
    }

    #[test]
    fn a_bound_project_row_adds_the_last_pull_and_no_mapping_no_rows() {
        let dir = tempfile::tempdir().unwrap();
        let code = dir.path().join("code");
        std::fs::create_dir_all(&code).unwrap();
        let config = project_config(dir.path(), &[&code], true);
        let (rows, _) = project_rows(&config, dir.path(), noon(), |_: &Path| None);
        assert!(rows[0].contains("No decisions. Never pulled. Stale. Reason: No pull was tried. No stop-check blocks."), "{}", rows[0]);
        let mut config = config;
        config.projects.clear();
        assert_eq!(project_rows(&config, dir.path(), noon(), |_: &Path| None), (vec![], true));
    }

    #[test]
    fn a_mapped_linked_worktree_fails_the_row_and_names_the_main_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let main = root.join("code");
        crate::inject::git::testing::repo(&main);
        let linked = root.join("code-wt");
        crate::inject::git::testing::git(&main, &["worktree", "add", "-q", "-b", "wt", linked.to_str().unwrap()]);
        let config = project_config(&root, &[&linked], false);
        let (rows, ok) = project_rows(&config, &root, noon(), crate::inject::git::dirs);
        assert!(!ok);
        assert!(rows[0].contains('\u{2717}'), "{}", rows[0]);
        assert!(rows[0].contains(&format!("{} is a linked worktree and is never used; map its main checkout {} instead.", linked.display(), main.display())), "{}", rows[0]);
        let config = project_config(&root, &[&main], false);
        let (rows, ok) = project_rows(&config, &root, noon(), crate::inject::git::dirs);
        assert!(ok, "{}", rows[0]);
    }

    fn policy_config(dir: &Path, gate: bool) -> crate::config::loader::WardwellConfig {
        let mut config = tracker_config(dir, "linear");
        config.trackers[0].gate = gate;
        config
    }

    fn put(home: &Path, relative: &str, text: &str) {
        let path = home.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// The agent plist exactly as Wardwell writes it for `program`, with the
    /// log in `home/cfg`, the config dir these tests pass.
    fn put_agent(home: &Path, program: &Path) {
        let log = crate::tracker::schedule::log_path(&home.join("cfg"));
        put(home, "Library/LaunchAgents/com.wardwell.tracker-pull.plist", &crate::tracker::schedule::launch_agent_plist(program, 3600, &log));
    }

    #[test]
    fn policy_rows_pass_when_the_gate_runs_this_binary_and_every_tool_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let binary = home.join("bin/wardwell");
        put(home, "bin/wardwell", "");
        let deny: Vec<&str> = crate::gate::ruleset::LINEAR_UPDATES.denied_tools.to_vec();
        let settings = serde_json::json!({"permissions": {"deny": deny}, "hooks": {"PreToolUse": [{"matcher": crate::gate::linear::MATCHER,
            "hooks": [{"type": "command", "command": format!("'{}' gate linear", binary.display())}]}]}});
        put(home, ".claude/settings.json", &settings.to_string());
        put_agent(home, &binary);
        let (rows, ok) = policy_rows(&policy_config(home, true), home, &home.join("cfg"), &binary, true);
        assert!(ok, "{rows:?}");
        assert_eq!(rows[0], format!("  {:<38} \u{2713} linear-updates v1", "Gate ruleset"));
        assert!(rows[1].contains("\u{2713} installed; runs"), "{}", rows[1]);
        assert!(rows[2].contains("7 of 7 destructive tools denied"), "{}", rows[2]);
        assert!(rows[3].contains("\u{2713} plist present, every 3600s, as `wardwell tracker schedule` wrote it; whether launchd loaded it is not checked offline"), "{}", rows[3]);
    }

    #[test]
    fn policy_rows_fail_on_a_stale_gate_path_and_missing_denies_and_no_agent_is_no_row() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        put(home, ".claude/settings.json", r#"{"hooks": {"PreToolUse": [{"matcher": "mcp__linear__save_comment|mcp__linear__save_issue", "hooks": [{"type": "command", "command": "/old/wardwell gate linear"}]}]}}"#);
        let (rows, ok) = policy_rows(&policy_config(home, true), home, &home.join("cfg"), Path::new("/new/wardwell"), true);
        assert!(!ok);
        assert!(rows[1].contains("\u{2717} runs /old/wardwell, not this binary /new/wardwell; run `wardwell setup`"), "{}", rows[1]);
        assert!(rows[2].contains("\u{2717} missing mcp__linear__delete_comment"), "{}", rows[2]);
        assert_eq!(rows.len(), 3, "no launchd agent, no pull row: {rows:?}");
    }

    #[test]
    fn the_pull_row_fails_when_the_plist_program_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        put_agent(home, Path::new("/gone/wardwell"));
        let (rows, ok) = policy_rows(&policy_config(home, false), home, &home.join("cfg"), Path::new("/w"), true);
        assert!(!ok);
        let row = rows.iter().find(|r| r.contains("Tracker pull service")).unwrap();
        assert!(row.contains("\u{2717} plist runs /gone/wardwell, which does not exist"), "{row}");
    }

    #[test]
    fn an_agent_fails_its_row_when_the_vault_is_under_a_protected_folder() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let binary = home.join("bin/wardwell");
        put(home, "bin/wardwell", "");
        put_agent(home, &binary);
        let mut config = policy_config(home, false);
        config.vault_path = home.join("Library/Mobile Documents/iCloud~md~obsidian/Documents/Notes");
        let (rows, ok) = policy_rows(&config, home, &home.join("cfg"), &binary, true);
        assert!(!ok);
        let row = rows.iter().find(|r| r.contains("Tracker pull service")).unwrap();
        assert!(row.contains("\u{2717} a launchd agent runs the pull, and the vault is under a folder macOS protects. macOS asks for consent after every upgrade, and the session refresh needs none. Run `wardwell tracker unschedule` to remove it."), "{row}");
        put(home, "Library/LaunchAgents/com.wardwell.tracker-pull.plist", "hand written");
        let (rows, ok) = policy_rows(&config, home, &home.join("cfg"), &binary, true);
        assert!(!ok, "a plist Wardwell did not write fails too: {rows:?}");
    }

    #[test]
    fn a_plist_wardwell_did_not_write_is_a_fact_row_outside_a_protected_vault() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        put(home, "Library/LaunchAgents/com.wardwell.tracker-pull.plist", "hand written");
        let (rows, ok) = policy_rows(&policy_config(home, false), home, &home.join("cfg"), Path::new("/w"), true);
        assert!(ok, "{rows:?}");
        assert_eq!(rows.last().unwrap(), &format!("  {:<38} a plist at com.wardwell.tracker-pull that Wardwell did not write; left alone", "Tracker pull service"));
    }

    #[test]
    fn the_gate_row_checks_the_matcher_as_well_as_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        put(home, ".claude/settings.json", r#"{"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "/w/wardwell gate linear"}]}]}}"#);
        let (rows, ok) = policy_rows(&policy_config(home, true), home, &home.join("cfg"), Path::new("/w/wardwell"), false);
        assert!(!ok);
        assert!(rows[1].contains("\u{2717} installed under matcher Bash, which does not match Linear writes"), "{}", rows[1]);
        put(home, ".claude/settings.json", r#"{"hooks": {"PreToolUse": [{"matcher": "mcp__linear__.*", "hooks": [{"type": "command", "command": "/w/wardwell gate linear"}]}]}}"#);
        let (rows, _) = policy_rows(&policy_config(home, true), home, &home.join("cfg"), Path::new("/w/wardwell"), false);
        assert!(rows[1].contains("\u{2713} installed; runs /w/wardwell"), "{}", rows[1]);
    }

    #[test]
    fn policy_rows_say_off_without_gate_true_and_have_no_pull_row_without_launchd() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        put_agent(home, Path::new("/w/wardwell"));
        let (rows, ok) = policy_rows(&policy_config(home, false), home, &home.join("cfg"), Path::new("/w"), false);
        assert!(ok);
        assert!(rows[1].ends_with("off; no linear binding has gate: true"), "{}", rows[1]);
        assert_eq!(rows.len(), 2, "{rows:?}");
        let mut config = policy_config(home, false);
        config.trackers.clear();
        std::fs::remove_file(home.join("Library/LaunchAgents/com.wardwell.tracker-pull.plist")).unwrap();
        assert_eq!(policy_rows(&config, home, &home.join("cfg"), Path::new("/w"), true).0.len(), 2);
    }
}
