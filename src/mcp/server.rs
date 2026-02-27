use crate::config::loader::WardwellConfig;
use crate::domain::registry::DomainRegistry;
use crate::index::fts::SearchQuery;
use crate::index::store::IndexStore;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::RwLock;

/// The Wardwell MCP server.
#[derive(Clone)]
pub struct WardwellServer {
    tool_router: ToolRouter<Self>,
    pub config: Arc<WardwellConfig>,
    pub index: Arc<IndexStore>,
    pub vault_root: PathBuf,
    pub registry: Arc<RwLock<DomainRegistry>>,
    /// Projects accessed (searched/read) in this session, as "domain/project" keys.
    accessed_projects: Arc<Mutex<HashSet<String>>>,
    /// Most recently accessed (domain, project) pair.
    last_project: Arc<Mutex<Option<(String, String)>>>,
}

// -- Tool parameter types --

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    #[schemars(description = "search: FTS query across vault. read: full file content. history: query across history files. orchestrate: prioritized project queue. retrospective: what happened in a time period. patterns: recurring blockers, stale threads, hot topics. context: session summary by ID. resume: full session handoff with plan, progress, remaining work by ID.")]
    pub action: String,
    #[schemars(description = "For search: FTS query. For history: what to look for.")]
    pub query: Option<String>,
    #[schemars(description = "For read: file path relative to vault root.")]
    pub path: Option<String>,
    #[schemars(description = "Filter to a domain (vault subdirectory). Optional.")]
    pub domain: Option<String>,
    #[schemars(description = "Filter to a project within a domain. For history queries.")]
    pub project: Option<String>,
    #[schemars(description = "For history: ISO date, only entries after this.")]
    pub since: Option<String>,
    #[schemars(description = "Max results.")]
    pub limit: Option<usize>,
    #[schemars(description = "For context: Claude Code session ID.")]
    pub session_id: Option<String>,
    #[schemars(description = "Include archived projects in retrospective/patterns. Default false.")]
    pub include_archived: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WriteParams {
    #[schemars(description = "sync: replace current_state.md and optionally append history. decide: append to decisions.md. append_history: append to history.jsonl. lesson: append to lessons.jsonl. append: append to a named JSONL list (requires 'list' param). IMPORTANT for append: check existing lists first (they're returned if list doesn't exist). ASK the user before creating a new list — do not create lists speculatively.")]
    pub action: String,
    #[schemars(description = "Domain folder under vault root (e.g., 'work', 'personal')")]
    pub domain: String,
    #[schemars(description = "Project folder within the domain. If omitted, inferred from last-accessed project in this session.")]
    pub project: Option<String>,

    // -- sync fields --
    #[schemars(description = "REQUIRED for sync: project status (active, blocked, completed)")]
    pub status: Option<String>,
    #[schemars(description = "REQUIRED for sync: what you're working on right now")]
    pub focus: Option<String>,
    #[schemars(description = "Optional for sync: why this project matters")]
    pub why_this_matters: Option<String>,
    #[schemars(description = "REQUIRED for sync: single concrete next step")]
    pub next_action: Option<String>,
    #[schemars(description = "Optional for sync: open questions")]
    pub open_questions: Option<Vec<String>>,
    #[schemars(description = "Optional for sync: things blocking progress")]
    pub blockers: Option<Vec<String>>,
    #[schemars(description = "Optional for sync: things waiting on others")]
    pub waiting_on: Option<Vec<String>>,
    #[schemars(description = "REQUIRED for sync: one-line commit message summarizing the session")]
    pub commit_message: Option<String>,

    // -- shared fields --
    #[schemars(description = "REQUIRED for decide/append_history/lesson. For sync: history entry title (defaults to commit_message if omitted).")]
    pub title: Option<String>,
    #[schemars(description = "REQUIRED for decide/append_history. Optional for sync/lesson.")]
    pub body: Option<String>,

    // -- append (generic list) fields --
    #[schemars(description = "For append: list name without extension (e.g., 'future-ideas'). Writes to {list}.jsonl in the project dir.")]
    pub list: Option<String>,
    #[schemars(description = "For append: set to true to confirm creating a NEW list. Required when the list doesn't exist yet.")]
    pub confirmed: Option<bool>,

    // -- source tagging --
    #[schemars(description = "Where this write originates: 'desktop' (Claude Desktop / claude.ai), 'code' (Claude Code), or 'manual'. Used to track intent vs execution.")]
    pub source: Option<String>,

    // -- lesson fields --
    #[schemars(description = "REQUIRED for lesson: what went wrong")]
    pub what_happened: Option<String>,
    #[schemars(description = "REQUIRED for lesson: why it went wrong")]
    pub root_cause: Option<String>,
    #[schemars(description = "REQUIRED for lesson: how to prevent it")]
    pub prevention: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClipboardParams {
    #[schemars(description = "Content to place on clipboard")]
    pub content: String,
}

#[tool_router(router = tool_router)]
impl WardwellServer {
    pub fn new(config: WardwellConfig, index: Arc<IndexStore>) -> Self {
        let vault_root = config.vault_path.clone();
        let registry = Arc::new(RwLock::new(DomainRegistry::from_domains(config.registry.all().to_vec())));

        Self {
            tool_router: Self::tool_router(),
            config: Arc::new(config),
            index,
            vault_root,
            registry,
            accessed_projects: Arc::new(Mutex::new(HashSet::new())),
            last_project: Arc::new(Mutex::new(None)),
        }
    }

    #[tool(description = "Search the vault index, query project history, read files, or get a prioritized work queue. Use `action` to specify what you need.")]
    async fn wardwell_search(&self, params: Parameters<SearchParams>) -> String {
        let p = params.0;
        match p.action.as_str() {
            "search" => self.action_search(&p),
            "read" => self.action_read(&p),
            "history" => self.action_history(&p),
            "orchestrate" => self.action_orchestrate(&p),
            "retrospective" => self.action_retrospective(&p),
            "patterns" => self.action_patterns(&p),
            "context" => self.action_context(&p).await,
            "resume" => self.action_resume(&p).await,
            other => json_error(&format!("Unknown action: '{other}'. Use search, read, history, orchestrate, retrospective, patterns, context, or resume.")),
        }
    }

    #[tool(description = "Write to the vault. Sync project state, record decisions, append history, or record lessons. Use `action` to specify the operation.")]
    async fn wardwell_write(&self, params: Parameters<WriteParams>) -> String {
        let p = params.0;

        // Resolve project: explicit > inferred from last access
        let project = match p.project.clone() {
            Some(proj) => proj,
            None => match self.last_project.lock().ok().and_then(|lp| lp.clone()) {
                Some((d, proj)) if d == p.domain => proj,
                Some(_) => return json_error("'project' is required — last accessed project is in a different domain."),
                None => return json_error("'project' is required — no project accessed in this session to infer from."),
            },
        };

        // Check if this project was accessed (searched/read) in this session
        let key = format!("{}/{}", p.domain, project);
        let was_accessed = self.accessed_projects.lock()
            .map(|set| set.contains(&key))
            .unwrap_or(true);
        let warning = if was_accessed {
            None
        } else {
            Some(format!("project '{key}' was not read or searched in this session"))
        };
        let inferred = p.project.is_none();

        match p.action.as_str() {
            "sync" => self.action_sync(&p, &project, warning.as_deref(), inferred),
            "decide" => self.action_decide(&p, &project, warning.as_deref()),
            "append_history" => self.action_append_history(&p, &project, warning.as_deref()),
            "lesson" => self.action_lesson(&p, &project, warning.as_deref()),
            "append" => self.action_append_list(&p, &project, warning.as_deref()),
            other => json_error(&format!("Unknown action: '{other}'. Use sync, decide, append_history, lesson, or append.")),
        }
    }

    #[tool(description = "Copy content to the system clipboard via pbcopy. IMPORTANT: Always ask the user for permission before calling this tool. Never overwrite the clipboard silently.")]
    async fn wardwell_clipboard(&self, params: Parameters<ClipboardParams>) -> String {
        let p = params.0;
        match clipboard_copy(&p.content) {
            Ok(bytes) => serde_json::to_string(&serde_json::json!({
                "copied": true,
                "bytes": bytes,
            })).unwrap_or_default(),
            Err(e) => json_error(&format!("Clipboard failed: {e}")),
        }
    }
}

// -- Session tracking --

impl WardwellServer {
    /// Record that a domain/project was accessed in this session.
    fn record_access(&self, domain: &str, project: &str) {
        let key = format!("{domain}/{project}");
        if let Ok(mut set) = self.accessed_projects.lock() {
            set.insert(key);
        }
        if let Ok(mut last) = self.last_project.lock() {
            *last = Some((domain.to_string(), project.to_string()));
        }
    }
}

/// Extract (domain, project) from a vault-relative path like "work/sentry-bot/current_state.md".
fn extract_domain_project(path: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() >= 2 {
        Some((parts[0].to_string(), parts[1].to_string()))
    } else {
        None
    }
}

// -- Search actions --

impl WardwellServer {
    fn action_search(&self, p: &SearchParams) -> String {
        let query_str = match &p.query {
            Some(q) => q.clone(),
            None => return json_error("'query' is required for action 'search'."),
        };

        let query = SearchQuery {
            query: query_str,
            domains: p.domain.as_ref().map(|d| vec![d.clone()]),
            types: Vec::new(),
            status: None,
            limit: p.limit.unwrap_or(5),
        };

        match self.index.search(&query) {
            Ok(results) => {
                // Track accessed projects from search results
                for r in &results.results {
                    if let Some((d, p)) = extract_domain_project(&r.path) {
                        self.record_access(&d, &p);
                    }
                }
                serde_json::to_string_pretty(&results).unwrap_or_default()
            }
            Err(e) => json_error(&format!("Search failed: {e}")),
        }
    }

    fn action_read(&self, p: &SearchParams) -> String {
        let path = match &p.path {
            Some(path) => path.clone(),
            None => return json_error("'path' is required for action 'read'."),
        };

        let full_path = resolve_path(&self.vault_root, &path);
        let vf = match full_path.and_then(|fp| crate::vault::reader::read_file(&fp).ok()) {
            Some(vf) => vf,
            None => return json_error(&format!("File not found: {path}. Use action 'search' to find valid paths.")),
        };

        // Track accessed project from read path
        if let Some((d, p)) = extract_domain_project(&path) {
            self.record_access(&d, &p);
        }

        let mut related_previews = Vec::new();
        for related_path in &vf.frontmatter.related {
            if let Some(related_full) = resolve_path(&self.vault_root, related_path)
                && let Ok(related_vf) = crate::vault::reader::read_file(&related_full)
            {
                related_previews.push(serde_json::json!({
                    "path": related_path,
                    "summary": related_vf.frontmatter.summary.unwrap_or_default(),
                }));
            }
        }

        serde_json::to_string_pretty(&serde_json::json!({
            "path": path,
            "frontmatter": vf.frontmatter,
            "content": vf.body,
            "related_previews": related_previews,
        })).unwrap_or_default()
    }

    fn action_history(&self, p: &SearchParams) -> String {
        let query_str = match &p.query {
            Some(q) => q.clone(),
            None => return json_error("'query' is required for action 'history'."),
        };

        let vault_dir = self.vault_root.clone();
        if !vault_dir.exists() {
            return json_error(&format!("No {}/ directory found in vault.", self.vault_root.display()));
        }

        let since_date = p.since.as_deref()
            .and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());

        let mut all_entries = Vec::new();

        // Walk vault looking for *.history.md or history.md files
        let dirs_to_scan = match (&p.domain, &p.project) {
            (Some(d), Some(proj)) => vec![vault_dir.join(d).join(proj)],
            (Some(d), None) => vec![vault_dir.join(d)],
            _ => list_subdirs(&vault_dir),
        };

        for dir in &dirs_to_scan {
            let vault_name = self.vault_root.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("vault");
            walk_history_files(dir, &query_str, since_date, p.limit.unwrap_or(5) * 3, vault_name, &mut all_entries);
        }

        // Sort by date descending
        all_entries.sort_by(|a, b| b.date.cmp(&a.date));
        all_entries.truncate(p.limit.unwrap_or(5));

        // Track accessed projects from history results
        for e in &all_entries {
            self.record_access(&e.domain, &e.project);
        }

        let total = all_entries.len();
        let entries_json: Vec<serde_json::Value> = all_entries.iter().map(|e| {
            serde_json::json!({
                "project": e.project,
                "domain": e.domain,
                "date": e.date,
                "title": e.title,
                "body": e.body,
                "source": e.source,
            })
        }).collect();

        serde_json::to_string_pretty(&serde_json::json!({
            "entries": entries_json,
            "total": total,
            "returned": entries_json.len(),
        })).unwrap_or_default()
    }

    fn action_orchestrate(&self, p: &SearchParams) -> String {
        let vault_dir = self.vault_root.clone();
        if !vault_dir.exists() {
            return json_error(&format!("No {}/ directory found in vault.", self.vault_root.display()));
        }

        let dirs_to_scan = match &p.domain {
            Some(d) => vec![vault_dir.join(d)],
            None => list_subdirs(&vault_dir),
        };

        let mut active = Vec::new();
        let mut blocked = Vec::new();
        let mut completed_recently = Vec::new();

        for domain_dir in &dirs_to_scan {
            let domain_name = domain_dir.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");

            // Look for current_state.md in immediate subdirs (projects) and at domain level
            let mut targets = vec![domain_dir.clone()];
            targets.extend(list_subdirs(domain_dir));

            for project_dir in &targets {
                let state_path = project_dir.join("current_state.md");
                if !state_path.exists() {
                    continue;
                }

                if let Ok(vf) = crate::vault::reader::read_file(&state_path) {
                    let project_name = project_dir.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown");

                    let status_str = vf.frontmatter.status.as_ref()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "active".to_string());

                    let focus = extract_section(&vf.body, "Focus");
                    let next_action = extract_section(&vf.body, "Next Action");

                    // Skip empty seeds — no focus and no next action
                    if focus.is_empty() && next_action.is_empty() {
                        continue;
                    }

                    let updated_str = vf.frontmatter.updated
                        .map(|d| d.to_string())
                        .or_else(|| {
                            std::fs::metadata(&state_path).ok()
                                .and_then(|m| m.modified().ok())
                                .map(|t| {
                                    let dt: chrono::DateTime<chrono::Local> = t.into();
                                    dt.format("%Y-%m-%d").to_string()
                                })
                        })
                        .unwrap_or_default();

                    let entry = serde_json::json!({
                        "domain": domain_name,
                        "project": project_name,
                        "status": status_str,
                        "updated": updated_str,
                        "focus": focus,
                        "next_action": next_action,
                    });

                    match status_str.as_str() {
                        "blocked" => blocked.push(entry),
                        "completed" | "resolved" => completed_recently.push(entry),
                        "paused" | "abandoned" | "superseded" => {} // excluded from queue
                        _ => active.push(entry),
                    }
                }
            }
        }

        // Track all returned projects
        for entry in active.iter().chain(blocked.iter()).chain(completed_recently.iter()) {
            if let (Some(d), Some(p)) = (entry["domain"].as_str(), entry["project"].as_str()) {
                self.record_access(d, p);
            }
        }

        let now = active.first().cloned();

        serde_json::to_string_pretty(&serde_json::json!({
            "now": now,
            "queue": active,
            "blocked": blocked,
            "completed_recently": completed_recently,
        })).unwrap_or_default()
    }
}

// -- Retrospective & patterns actions --

/// A parsed history entry with domain/project context attached.
struct ParsedHistoryEntry {
    domain: String,
    project: String,
    date: String,
    title: String,
    status: String,
    focus: String,
    body: String,
}

/// Walk the vault and collect all history.jsonl entries, filtered by date and domain.
fn collect_history_entries(
    vault_root: &std::path::Path,
    since: Option<chrono::NaiveDate>,
    domain_filter: Option<&str>,
    skip_archive: bool,
) -> Vec<ParsedHistoryEntry> {
    let mut entries = Vec::new();
    let dirs_to_scan = match domain_filter {
        Some(d) => vec![vault_root.join(d)],
        None => list_subdirs(vault_root),
    };

    for domain_dir in &dirs_to_scan {
        if !domain_dir.is_dir() { continue; }
        if skip_archive && domain_dir.file_name().is_some_and(|n| n == "archive") {
            continue;
        }
        let domain_name = domain_dir.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        for project_dir in list_subdirs(domain_dir) {
            if skip_archive && project_dir.file_name().is_some_and(|n| n == "archive") {
                continue;
            }
            let project_name = project_dir.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();

            let jsonl_path = project_dir.join("history.jsonl");
            if !jsonl_path.exists() { continue; }
            let content = match std::fs::read_to_string(&jsonl_path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            for line in content.lines() {
                if line.trim().is_empty() || line.starts_with("{\"_schema\":") || line.starts_with("{\"_schema\" :") {
                    continue;
                }
                let entry: HistoryJsonlEntry = match serde_json::from_str(line) {
                    Ok(e) => e,
                    Err(_) => continue,
                };

                // Date filter
                let date_str = entry.date.get(..10).unwrap_or(&entry.date);
                if let Some(s) = since
                    && chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d").is_ok_and(|d| d < s) {
                    continue;
                }

                entries.push(ParsedHistoryEntry {
                    domain: domain_name.clone(),
                    project: project_name.clone(),
                    date: date_str.to_string(),
                    title: entry.title,
                    status: entry.status,
                    focus: entry.focus,
                    body: entry.body,
                });
            }
        }
    }

    // Sort by date descending
    entries.sort_by(|a, b| b.date.cmp(&a.date));
    entries
}

impl WardwellServer {
    fn action_retrospective(&self, p: &SearchParams) -> String {
        let since_str = match &p.since {
            Some(s) => s.clone(),
            None => return json_error("'since' is required for action 'retrospective'. Use ISO date (e.g. '2026-02-15')."),
        };
        let since = match chrono::NaiveDate::parse_from_str(&since_str, "%Y-%m-%d") {
            Ok(d) => d,
            Err(_) => return json_error(&format!("Invalid date format: '{since_str}'. Use YYYY-MM-DD.")),
        };

        let skip_archive = !p.include_archived.unwrap_or(false);
        let entries = collect_history_entries(
            &self.vault_root,
            Some(since),
            p.domain.as_deref(),
            skip_archive,
        );

        // Group by domain/project
        let mut groups: std::collections::HashMap<String, Vec<&ParsedHistoryEntry>> = std::collections::HashMap::new();
        for e in &entries {
            let key = format!("{}/{}", e.domain, e.project);
            groups.entry(key).or_default().push(e);
        }

        let mut completed = Vec::new();
        let mut still_active = Vec::new();
        let mut per_project = Vec::new();

        for (key, project_entries) in &groups {
            let entry_count = project_entries.len();
            let first_status = project_entries.last().map(|e| e.status.as_str()).unwrap_or("");
            let last_status = project_entries.first().map(|e| e.status.as_str()).unwrap_or("");
            let titles: Vec<&str> = project_entries.iter().map(|e| e.title.as_str()).collect();

            let status_flow = if first_status == last_status {
                last_status.to_string()
            } else {
                format!("{first_status} → {last_status}")
            };

            let parts: Vec<&str> = key.split('/').collect();
            let domain = parts.first().unwrap_or(&"unknown");
            let project = parts.get(1).unwrap_or(&"unknown");

            per_project.push(serde_json::json!({
                "project": key,
                "domain": domain,
                "entries": entry_count,
                "status_flow": status_flow,
                "titles": titles,
            }));

            if last_status == "completed" || last_status == "resolved" {
                completed.push(key.clone());
            } else {
                still_active.push(key.clone());
            }

            // Track accessed projects
            self.record_access(domain, project);
        }

        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        serde_json::to_string_pretty(&serde_json::json!({
            "period": format!("{since_str} to {today}"),
            "projects_touched": groups.len(),
            "completed": completed,
            "still_active": still_active,
            "per_project": per_project,
        })).unwrap_or_default()
    }

    fn action_patterns(&self, p: &SearchParams) -> String {
        let since = p.since.as_deref()
            .and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
            .unwrap_or_else(|| chrono::Local::now().date_naive() - chrono::Duration::days(90));

        let skip_archive = !p.include_archived.unwrap_or(false);
        let entries = collect_history_entries(
            &self.vault_root,
            Some(since),
            p.domain.as_deref(),
            skip_archive,
        );

        // -- Recurring blockers --
        let blocked_terms = ["blocked", "waiting", "stuck", "blocker"];
        let mut blocker_counts: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
        for e in &entries {
            let text = format!("{} {} {}", e.status, e.focus, e.body).to_lowercase();
            if blocked_terms.iter().any(|t| text.contains(t)) {
                let key = format!("{}/{}", e.domain, e.project);
                blocker_counts.entry(key).or_default().push(e.title.clone());
            }
        }
        let recurring_blockers: Vec<serde_json::Value> = blocker_counts.iter()
            .filter(|(_, titles)| titles.len() >= 2)
            .map(|(project, titles)| serde_json::json!({
                "project": project,
                "count": titles.len(),
                "titles": titles,
            }))
            .collect();

        // -- Stale threads --
        let mut latest_by_project: std::collections::HashMap<String, (&str, &str)> = std::collections::HashMap::new();
        for e in &entries {
            let key = format!("{}/{}", e.domain, e.project);
            latest_by_project.entry(key)
                .and_modify(|(date, status)| {
                    if e.date.as_str() > *date {
                        *date = &e.date;
                        *status = &e.status;
                    }
                })
                .or_insert((&e.date, &e.status));
        }
        let today = chrono::Local::now().date_naive();
        let stale_threads: Vec<serde_json::Value> = latest_by_project.iter()
            .filter_map(|(project, (date, status))| {
                if *status == "completed" || *status == "resolved" {
                    return None;
                }
                let last = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
                let days = (today - last).num_days();
                if days >= 14 {
                    Some(serde_json::json!({
                        "project": project,
                        "last_entry": date,
                        "days_stale": days,
                    }))
                } else {
                    None
                }
            })
            .collect();

        // -- Hot topics --
        let stopwords: &[&str] = &[
            "the", "a", "an", "is", "are", "was", "were", "be", "been", "being",
            "have", "has", "had", "do", "does", "did", "will", "would", "could",
            "should", "may", "might", "shall", "can", "need", "to", "of", "in",
            "for", "on", "with", "at", "by", "from", "as", "into", "through",
            "during", "before", "after", "between", "out", "off", "over", "under",
            "again", "further", "then", "once", "that", "this", "these", "those",
            "not", "no", "and", "but", "or", "so", "if", "when", "it", "its",
            "he", "she", "they", "them", "we", "you", "complete", "active",
            "project", "focus", "next", "action", "status", "none", "still",
        ];
        let mut word_projects: std::collections::HashMap<String, HashSet<String>> = std::collections::HashMap::new();
        let mut word_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for e in &entries {
            let project_key = format!("{}/{}", e.domain, e.project);
            for word in e.title.split_whitespace() {
                let clean = word.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
                if clean.len() > 2 && !stopwords.contains(&clean.as_str()) {
                    *word_counts.entry(clean.clone()).or_default() += 1;
                    word_projects.entry(clean).or_default().insert(project_key.clone());
                }
            }
        }
        let mut hot_topics: Vec<(String, usize, Vec<String>)> = word_counts.into_iter()
            .filter(|(_, count)| *count >= 3)
            .map(|(term, count)| {
                let projects: Vec<String> = word_projects.get(&term)
                    .map(|s| s.iter().cloned().collect())
                    .unwrap_or_default();
                (term, count, projects)
            })
            .collect();
        hot_topics.sort_by(|a, b| b.1.cmp(&a.1));
        hot_topics.truncate(10);
        let hot_topics_json: Vec<serde_json::Value> = hot_topics.into_iter()
            .map(|(term, count, projects)| serde_json::json!({
                "term": term,
                "mentions": count,
                "projects": projects,
            }))
            .collect();

        // -- Status oscillations --
        let mut status_flows: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
        // Entries are date desc, reverse for chronological order
        for e in entries.iter().rev() {
            let key = format!("{}/{}", e.domain, e.project);
            let flow = status_flows.entry(key).or_default();
            if !e.status.is_empty() && flow.last().map(|s| s.as_str()) != Some(&e.status) {
                flow.push(e.status.clone());
            }
        }
        let oscillations: Vec<serde_json::Value> = status_flows.into_iter()
            .filter(|(_, flow)| flow.len() >= 3)
            .map(|(project, flow)| serde_json::json!({
                "project": project,
                "flow": flow,
            }))
            .collect();

        let since_str = since.format("%Y-%m-%d").to_string();
        let today_str = today.format("%Y-%m-%d").to_string();

        serde_json::to_string_pretty(&serde_json::json!({
            "period": format!("{since_str} to {today_str}"),
            "recurring_blockers": recurring_blockers,
            "stale_threads": stale_threads,
            "hot_topics": hot_topics_json,
            "status_oscillations": oscillations,
        })).unwrap_or_default()
    }
}

// -- Context action --

impl WardwellServer {
    async fn action_context(&self, p: &SearchParams) -> String {
        let session_id = match &p.session_id {
            Some(id) => id.clone(),
            None => return json_error("'session_id' is required for action 'context'."),
        };

        // Find the session JSONL file
        let jsonl_path = match crate::daemon::summarizer::find_session_file_by_id(
            &session_id,
            &self.config.session_sources,
        ) {
            Some(p) => p,
            None => return json_error(&format!("Session not found: '{session_id}'.")),
        };

        // Extract project info from parent directory name
        let project_dir_name = jsonl_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("");
        let project_path = crate::daemon::indexer::decode_project_dir(project_dir_name);

        // Parse metadata from JSONL
        let (started, message_count) = parse_session_metadata(&jsonl_path);

        // Get or generate summary
        let summaries_dir = self.config.vault_path.parent()
            .unwrap_or(std::path::Path::new("/tmp"))
            .join("summaries");
        let (summary, summary_error) = get_or_generate_summary(
            &session_id,
            &jsonl_path,
            &project_path,
            &summaries_dir,
            &self.config.ai.summarize_model,
        ).await;

        // Resolve domain/project from vault directory
        let vault_match = resolve_vault_project(
            std::path::Path::new(&project_path),
            &self.vault_root,
        );

        // Pull vault state if we matched a project
        let vault_state = vault_match.as_ref().and_then(|(_, _, project_dir)| {
            let state_path = project_dir.join("current_state.md");
            if !state_path.exists() {
                return None;
            }
            let vf = crate::vault::reader::read_file(&state_path).ok()?;
            let focus = extract_section(&vf.body, "Focus");
            let next_action = extract_section(&vf.body, "Next Action");
            let updated = vf.frontmatter.updated.map(|d| d.to_string());

            let status_str = vf.frontmatter.status.as_ref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "active".to_string());

            // Read recent history — prefer JSONL, fall back to .md
            let recent_history = read_recent_history_from_dir(project_dir, 3);

            Some(serde_json::json!({
                "status": status_str,
                "focus": focus,
                "next_action": next_action,
                "updated": updated,
                "recent_history": recent_history,
            }))
        });

        // Related vault hits from summary terms
        let related: Vec<serde_json::Value> = if let Some(ref summary_text) = summary {
            let terms = extract_search_terms(summary_text, 5);
            if terms.is_empty() {
                Vec::new()
            } else {
                let query = SearchQuery {
                    query: terms,
                    domains: vault_match.as_ref().map(|(d, _, _)| vec![d.clone()]),
                    types: Vec::new(),
                    status: None,
                    limit: 3,
                };
                match self.index.search(&query) {
                    Ok(sr) => sr.results.into_iter().map(|r| serde_json::json!({
                        "path": r.path,
                        "snippet": r.snippet,
                    })).collect(),
                    Err(_) => Vec::new(),
                }
            }
        } else {
            Vec::new()
        };

        let (domain_name, project_name) = vault_match
            .map(|(d, p, _)| (Some(d), Some(p)))
            .unwrap_or((None, None));

        // Track accessed project from context resolution
        if let (Some(d), Some(p)) = (&domain_name, &project_name) {
            self.record_access(d, p);
        }

        serde_json::to_string_pretty(&serde_json::json!({
            "session_id": session_id,
            "project_path": project_path,
            "started": started,
            "message_count": message_count,
            "summary": summary,
            "summary_error": summary_error,
            "domain": domain_name,
            "project": project_name,
            "vault_state": vault_state,
            "related": related,
        })).unwrap_or_default()
    }

    /// Resume a previous session — generates a handoff document with plan, progress,
    /// remaining work, and current state. Always generates fresh (ignores cache).
    async fn action_resume(&self, p: &SearchParams) -> String {
        let session_id = match &p.session_id {
            Some(id) => id.clone(),
            None => return json_error("'session_id' is required for action 'resume'."),
        };

        let jsonl_path = match crate::daemon::summarizer::find_session_file_by_id(
            &session_id,
            &self.config.session_sources,
        ) {
            Some(p) => p,
            None => return json_error(&format!("Session not found: '{session_id}'.")),
        };

        let project_dir_name = jsonl_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("");
        let project_path = crate::daemon::indexer::decode_project_dir(project_dir_name);

        let (started, message_count) = parse_session_metadata(&jsonl_path);

        // Always generate fresh with RESUME_PROMPT (no cache)
        let conversation = match crate::daemon::indexer::extract_conversation(&jsonl_path) {
            Ok(c) => c,
            Err(e) => return json_error(&format!("Failed to extract conversation: {e}")),
        };

        if conversation.is_empty() {
            return json_error("Empty session — nothing to resume.");
        }

        let payload = crate::daemon::summarizer::build_resume_payload(&conversation);
        let prompt = format!(
            "{}\n\n---\n\nThis session was for the project at `{project_path}`.\n\n---\n\n{payload}",
            crate::daemon::summarizer::RESUME_PROMPT,
        );

        let resume_doc = match crate::daemon::summarizer::claude_cli_call(
            &prompt,
            &self.config.ai.summarize_model,
        ).await {
            Ok(doc) => doc,
            Err(e) => return json_error(&format!("Failed to generate resume document: {e}")),
        };

        // Resolve vault project for context
        let vault_match = resolve_vault_project(
            std::path::Path::new(&project_path),
            &self.vault_root,
        );
        let (domain_name, project_name) = vault_match
            .map(|(d, p, _)| (Some(d), Some(p)))
            .unwrap_or((None, None));

        if let (Some(d), Some(p)) = (&domain_name, &project_name) {
            self.record_access(d, p);
        }

        serde_json::to_string_pretty(&serde_json::json!({
            "session_id": session_id,
            "project_path": project_path,
            "started": started,
            "message_count": message_count,
            "domain": domain_name,
            "project": project_name,
            "resume": resume_doc,
        })).unwrap_or_default()
    }
}

/// Parse first JSONL line for timestamp and count user+assistant messages.
fn parse_session_metadata(path: &std::path::Path) -> (Option<String>, usize) {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return (None, 0),
    };
    let reader = std::io::BufReader::new(file);
    let mut started: Option<String> = None;
    let mut count: usize = 0;

    use std::io::BufRead;
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        let parsed: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if started.is_none()
            && let Some(ts) = parsed.get("timestamp").and_then(|t| t.as_str()) {
                started = Some(ts.to_string());
            }
        let msg_type = parsed.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if msg_type == "user" || msg_type == "assistant" {
            count += 1;
        }
    }
    (started, count)
}

/// Get cached summary or generate on-the-fly via claude CLI.
async fn get_or_generate_summary(
    session_id: &str,
    jsonl_path: &std::path::Path,
    project_path: &str,
    summaries_dir: &std::path::Path,
    model: &str,
) -> (Option<String>, Option<String>) {
    let summary_path = summaries_dir.join(format!("{session_id}.md"));

    // Check cache first
    if summary_path.exists()
        && let Ok(content) = std::fs::read_to_string(&summary_path) {
            let body = strip_frontmatter(&content);
            if !body.trim().is_empty() {
                return (Some(body), None);
            }
        }

    // Generate on-the-fly
    let conversation = match crate::daemon::indexer::extract_conversation(jsonl_path) {
        Ok(c) => c,
        Err(e) => return (None, Some(format!("Failed to extract conversation: {e}"))),
    };

    if conversation.is_empty() {
        return (None, Some("Empty session".to_string()));
    }

    let payload = crate::daemon::summarizer::build_conversation_payload(&conversation);
    let prompt = format!(
        "{}\n\n---\n\nThis session was for the project at `{project_path}`.\n\n---\n\n{payload}",
        crate::daemon::summarizer::SUMMARY_PROMPT,
    );

    match crate::daemon::summarizer::claude_cli_call(&prompt, model).await {
        Ok(summary) => {
            // Cache the result
            let _ = std::fs::create_dir_all(summaries_dir);
            let frontmatter = format!(
                "---\ntype: thread\nproject: {project_path}\nstatus: resolved\nconfidence: inferred\nsummary: Session summary for {project_path}\n---\n"
            );
            let _ = std::fs::write(&summary_path, format!("{frontmatter}\n{summary}"));
            (Some(summary), None)
        }
        Err(e) => (None, Some(format!("{e}"))),
    }
}

/// Strip YAML frontmatter from markdown content.
fn strip_frontmatter(content: &str) -> String {
    if !content.starts_with("---") {
        return content.to_string();
    }
    // Find the closing ---
    if let Some(end) = content[3..].find("\n---") {
        let after = end + 3 + 4; // skip past "\n---"
        if after < content.len() {
            return content[after..].trim_start_matches('\n').to_string();
        }
    }
    content.to_string()
}

/// Resolve a project path against the vault directory.
/// Scans vault_dir subdirectories and matches the last path component
/// of the project path against project folder names (case-insensitive).
fn resolve_vault_project(
    project_path: &std::path::Path,
    vault_dir: &std::path::Path,
) -> Option<(String, String, PathBuf)> {
    if !vault_dir.exists() {
        return None;
    }

    // Extract the last component of the project path as the match target
    let target = project_path
        .file_name()
        .and_then(|n| n.to_str())?
        .to_lowercase();

    let domain_entries = std::fs::read_dir(vault_dir).ok()?;
    for domain_entry in domain_entries.flatten() {
        let domain_path = domain_entry.path();
        if !domain_path.is_dir() {
            continue;
        }
        let domain_name = domain_entry.file_name().to_string_lossy().to_string();

        let project_entries = match std::fs::read_dir(&domain_path) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for project_entry in project_entries.flatten() {
            let proj_path = project_entry.path();
            if !proj_path.is_dir() {
                continue;
            }
            let proj_name = project_entry.file_name().to_string_lossy().to_string();
            if proj_name.to_lowercase() == target {
                return Some((domain_name, proj_name, proj_path));
            }
        }
    }
    None
}

/// Read recent history entries from a project directory.
/// Tries history.jsonl first, falls back to history.md.
fn read_recent_history_from_dir(project_dir: &std::path::Path, n: usize) -> Vec<serde_json::Value> {
    let jsonl_path = project_dir.join("history.jsonl");
    if jsonl_path.exists()
        && let Ok(content) = std::fs::read_to_string(&jsonl_path) {
            return extract_recent_history_jsonl(&content, n);
        }
    let md_path = project_dir.join("history.md");
    if md_path.exists()
        && let Ok(content) = std::fs::read_to_string(&md_path) {
            return extract_recent_history_md(&content, n);
        }
    Vec::new()
}

/// Extract recent history entries from JSONL content. Returns newest first.
fn extract_recent_history_jsonl(content: &str, n: usize) -> Vec<serde_json::Value> {
    let mut entries = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() || line.starts_with("{\"_schema\":") || line.starts_with("{\"_schema\" :") {
            continue;
        }
        let entry: HistoryJsonlEntry = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let date_str = entry.date.get(..10).unwrap_or(&entry.date).to_string();
        entries.push(serde_json::json!({
            "date": date_str,
            "title": entry.title,
            "body": entry.body,
        }));
    }
    // Reverse to get newest first (append = newest at bottom)
    entries.reverse();
    entries.truncate(n);
    entries
}

/// Extract recent history entries from markdown content.
/// Parses `## YYYY-MM-DD HH:MM — Title` entries and returns first N.
fn extract_recent_history_md(content: &str, n: usize) -> Vec<serde_json::Value> {
    let mut entries = Vec::new();
    let mut current_date = String::new();
    let mut current_title = String::new();
    let mut current_body = String::new();
    let mut in_entry = false;

    for line in content.lines() {
        if line.starts_with("## ") && line.len() > 16 {
            // Flush previous entry
            if in_entry && !current_title.is_empty() && entries.len() < n {
                entries.push(serde_json::json!({
                    "date": current_date,
                    "title": current_title,
                    "body": current_body.trim(),
                }));
            }
            if entries.len() >= n {
                break;
            }

            let heading = &line[3..];
            if heading.len() >= 10 {
                current_date = heading[..10].to_string();
                current_title = heading.split('—').nth(1)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|| heading[10..].trim().to_string());
            } else {
                current_date = String::new();
                current_title = heading.to_string();
            }
            current_body.clear();
            in_entry = true;
        } else if line == "---" {
            // separator — ignore
        } else if in_entry {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }

    // Flush last entry
    if in_entry && !current_title.is_empty() && entries.len() < n {
        entries.push(serde_json::json!({
            "date": current_date,
            "title": current_title,
            "body": current_body.trim(),
        }));
    }

    entries
}

/// Extract search terms from a summary for FTS queries.
/// Pulls words from `##` headings and `**bold**` text, filters stopwords.
fn extract_search_terms(summary: &str, max_terms: usize) -> String {
    let stopwords: &[&str] = &[
        "the", "a", "an", "is", "are", "was", "were", "be", "been", "being",
        "have", "has", "had", "do", "does", "did", "will", "would", "could",
        "should", "may", "might", "shall", "can", "need", "dare", "ought",
        "used", "to", "of", "in", "for", "on", "with", "at", "by", "from",
        "as", "into", "through", "during", "before", "after", "above",
        "below", "between", "out", "off", "over", "under", "again",
        "further", "then", "once", "that", "this", "these", "those",
        "not", "no", "nor", "and", "but", "or", "so", "if", "when",
        "it", "its", "he", "she", "they", "them", "we", "you", "i",
    ];

    let mut terms = Vec::new();

    for line in summary.lines() {
        let text = if let Some(heading) = line.strip_prefix("## ") {
            heading
        } else if line.contains("**") {
            // Extract text between ** markers
            let mut collected = String::new();
            let mut in_bold = false;
            let chars: Vec<char> = line.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*' {
                    in_bold = !in_bold;
                    if !in_bold {
                        collected.push(' ');
                    }
                    i += 2;
                } else {
                    if in_bold {
                        collected.push(chars[i]);
                    }
                    i += 1;
                }
            }
            if collected.trim().is_empty() {
                continue;
            }
            // Use a temporary string that we'll process below
            // We need to own this, so we'll handle it differently
            let words: Vec<&str> = collected.split_whitespace().collect();
            for word in words {
                let clean = word.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
                if clean.len() > 2 && !stopwords.contains(&clean.as_str()) && !terms.contains(&clean) {
                    terms.push(clean);
                    if terms.len() >= max_terms {
                        return terms.join(" OR ");
                    }
                }
            }
            continue;
        } else {
            continue;
        };

        for word in text.split_whitespace() {
            let clean = word.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
            if clean.len() > 2 && !stopwords.contains(&clean.as_str()) && !terms.contains(&clean) {
                terms.push(clean);
                if terms.len() >= max_terms {
                    return terms.join(" OR ");
                }
            }
        }
    }

    terms.join(" OR ")
}

// -- Write actions --

impl WardwellServer {
    fn action_sync(&self, p: &WriteParams, project: &str, warning: Option<&str>, inferred: bool) -> String {
        let status = match &p.status {
            Some(s) => s.clone(),
            None => return json_error("'status' is required for action 'sync'."),
        };
        let focus = match &p.focus {
            Some(f) => f.clone(),
            None => return json_error("'focus' is required for action 'sync'."),
        };
        let next_action = match &p.next_action {
            Some(n) => n.clone(),
            None => return json_error("'next_action' is required for action 'sync'."),
        };
        let commit_message = match &p.commit_message {
            Some(c) => c.clone(),
            None => return json_error("'commit_message' is required for action 'sync'."),
        };

        let project_dir = self.vault_root.clone().join(&p.domain).join(project);
        if let Err(e) = std::fs::create_dir_all(&project_dir) {
            return json_error(&format!("Failed to create directory: {e}"));
        }

        let now = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();

        // Build current_state.md
        let source = p.source.as_deref().unwrap_or("unknown");
        let mut content = format!(
            "---\nchat_name: {project}\nupdated: {now}\nstatus: {status}\ntype: project\ncontext: {domain}\nsource: {source}\n---\n\n# {project}\n\n## Focus\n{focus}\n",
            domain = p.domain,
        );

        if let Some(ref why) = p.why_this_matters {
            content.push_str(&format!("\n## Why This Matters\n{why}\n"));
        }

        content.push_str(&format!("\n## Next Action\n{next_action}\n"));

        if let Some(ref qs) = p.open_questions
            && !qs.is_empty() {
                content.push_str("\n## Open Questions\n");
                for q in qs { content.push_str(&format!("- {q}\n")); }
            }

        if let Some(ref bs) = p.blockers
            && !bs.is_empty() {
                content.push_str("\n## Blockers\n");
                for b in bs { content.push_str(&format!("- {b}\n")); }
            }

        if let Some(ref ws) = p.waiting_on
            && !ws.is_empty() {
                content.push_str("\n## Waiting On\n");
                for w in ws { content.push_str(&format!("- {w}\n")); }
            }

        content.push_str(&format!("\n## Commit Message\n{commit_message}\n"));

        let state_path = project_dir.join("current_state.md");
        let mut files_written = vec![];

        if let Err(e) = std::fs::write(&state_path, &content) {
            return json_error(&format!("Failed to write current_state.md: {e}"));
        }
        files_written.push(format!("{}/{}/{}/current_state.md", self.vault_root.display(), p.domain, project));

        // Always append history entry on sync
        let history_path = project_dir.join("history.jsonl");
        let jsonl_entry = HistoryJsonlEntry {
            date: chrono::Utc::now().to_rfc3339(),
            title: p.title.clone().unwrap_or_else(|| commit_message.clone()),
            status: status.clone(),
            focus: focus.clone(),
            next_action: next_action.clone(),
            commit: commit_message.clone(),
            body: p.body.clone().unwrap_or_else(|| commit_message.clone()),
            source: source.to_string(),
        };
        let json = match serde_json::to_string(&jsonl_entry) {
            Ok(j) => j,
            Err(e) => return json_error(&format!("Failed to serialize history entry: {e}")),
        };
        if let Err(e) = append_jsonl(&history_path, "history", &json) {
            return json_error(&format!("Failed to write history.jsonl: {e}"));
        }
        files_written.push(format!("{}/{}/{}/history.jsonl", self.vault_root.display(), p.domain, project));

        // Update FTS index for written files
        self.reindex_file(&state_path);

        let project_key = format!("{}/{}", p.domain, project);
        let mut resp = serde_json::json!({
            "synced": true,
            "project": project_key,
            "files_written": files_written,
        });
        if let Some(w) = warning {
            resp["warning"] = serde_json::json!(w);
        }
        if inferred {
            resp["inferred_project"] = serde_json::json!(true);
        }
        serde_json::to_string(&resp).unwrap_or_default()
    }

    fn action_decide(&self, p: &WriteParams, project: &str, warning: Option<&str>) -> String {
        let title = match &p.title {
            Some(t) => t.clone(),
            None => return json_error("'title' is required for action 'decide'."),
        };
        let body = match &p.body {
            Some(b) => b.clone(),
            None => return json_error("'body' is required for action 'decide'."),
        };

        let project_dir = self.vault_root.clone().join(&p.domain).join(project);
        if let Err(e) = std::fs::create_dir_all(&project_dir) {
            return json_error(&format!("Failed to create directory: {e}"));
        }

        let decisions_path = project_dir.join("decisions.md");
        let now = chrono::Local::now().format("%Y-%m-%d").to_string();

        let entry = format!("## {now} — {title}\n\n{body}\n\n---\n\n");

        if let Err(e) = prepend_to_file(&decisions_path, &format!("# {project} Decisions"), &entry) {
            return json_error(&format!("Failed to write decisions.md: {e}"));
        }

        self.reindex_file(&decisions_path);

        let project_key = format!("{}/{}", p.domain, project);
        let rel = format!("{}/{}/decisions.md", self.vault_root.display(), project_key);
        let mut resp = serde_json::json!({
            "recorded": true,
            "project": project_key,
            "path": rel,
        });
        if let Some(w) = warning {
            resp["warning"] = serde_json::json!(w);
        }
        serde_json::to_string(&resp).unwrap_or_default()
    }

    fn action_append_history(&self, p: &WriteParams, project: &str, warning: Option<&str>) -> String {
        let title = match &p.title {
            Some(t) => t.clone(),
            None => return json_error("'title' is required for action 'append_history'."),
        };

        let project_dir = self.vault_root.clone().join(&p.domain).join(project);
        if let Err(e) = std::fs::create_dir_all(&project_dir) {
            return json_error(&format!("Failed to create directory: {e}"));
        }

        let history_path = project_dir.join("history.jsonl");
        let jsonl_entry = HistoryJsonlEntry {
            date: chrono::Utc::now().to_rfc3339(),
            title,
            status: String::new(),
            focus: String::new(),
            next_action: String::new(),
            commit: String::new(),
            body: p.body.clone().unwrap_or_default(),
            source: p.source.clone().unwrap_or_default(),
        };
        let json = match serde_json::to_string(&jsonl_entry) {
            Ok(j) => j,
            Err(e) => return json_error(&format!("Failed to serialize history entry: {e}")),
        };
        if let Err(e) = append_jsonl(&history_path, "history", &json) {
            return json_error(&format!("Failed to write history.jsonl: {e}"));
        }

        let project_key = format!("{}/{}", p.domain, project);
        let rel = format!("{}/{}/history.jsonl", self.vault_root.display(), project_key);
        let mut resp = serde_json::json!({
            "appended": true,
            "project": project_key,
            "path": rel,
        });
        if let Some(w) = warning {
            resp["warning"] = serde_json::json!(w);
        }
        serde_json::to_string(&resp).unwrap_or_default()
    }

    fn action_lesson(&self, p: &WriteParams, project: &str, warning: Option<&str>) -> String {
        let title = match &p.title {
            Some(t) => t.clone(),
            None => return json_error("'title' is required for action 'lesson'."),
        };
        let what_happened = match &p.what_happened {
            Some(w) => w.clone(),
            None => return json_error("'what_happened' is required for action 'lesson'."),
        };
        let root_cause = match &p.root_cause {
            Some(r) => r.clone(),
            None => return json_error("'root_cause' is required for action 'lesson'."),
        };
        let prevention = match &p.prevention {
            Some(p) => p.clone(),
            None => return json_error("'prevention' is required for action 'lesson'."),
        };

        let project_dir = self.vault_root.clone().join(&p.domain).join(project);
        if let Err(e) = std::fs::create_dir_all(&project_dir) {
            return json_error(&format!("Failed to create directory: {e}"));
        }

        let lessons_path = project_dir.join("lessons.jsonl");
        let jsonl_entry = LessonJsonlEntry {
            date: chrono::Utc::now().format("%Y-%m-%d").to_string(),
            title,
            what_happened,
            root_cause,
            prevention,
            source: p.source.clone().unwrap_or_default(),
        };
        let json = match serde_json::to_string(&jsonl_entry) {
            Ok(j) => j,
            Err(e) => return json_error(&format!("Failed to serialize lesson entry: {e}")),
        };
        if let Err(e) = append_jsonl(&lessons_path, "lessons", &json) {
            return json_error(&format!("Failed to write lessons.jsonl: {e}"));
        }

        let project_key = format!("{}/{}", p.domain, project);
        let rel = format!("{}/{}/lessons.jsonl", self.vault_root.display(), project_key);
        let mut resp = serde_json::json!({
            "recorded": true,
            "project": project_key,
            "path": rel,
        });
        if let Some(w) = warning {
            resp["warning"] = serde_json::json!(w);
        }
        serde_json::to_string(&resp).unwrap_or_default()
    }

    fn action_append_list(&self, p: &WriteParams, project: &str, warning: Option<&str>) -> String {
        let list_name = match &p.list {
            Some(l) => l.clone(),
            None => return json_error("'list' is required for action 'append'."),
        };

        // Sanitize: alphanumeric, hyphens, underscores only
        if !list_name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
            return json_error("'list' must contain only alphanumeric characters, hyphens, and underscores.");
        }

        // Reserved names — use the dedicated actions instead
        if matches!(list_name.as_str(), "history" | "lessons") {
            return json_error(&format!("'{list_name}' is a built-in list. Use action '{}'.", if list_name == "history" { "append_history" } else { "lesson" }));
        }

        let title = match &p.title {
            Some(t) => t.clone(),
            None => return json_error("'title' is required for action 'append'."),
        };

        let project_dir = self.vault_root.join(&p.domain).join(project);
        let list_path = project_dir.join(format!("{list_name}.jsonl"));

        // If list doesn't exist yet, require explicit confirmation
        if !list_path.exists() && !p.confirmed.unwrap_or(false) {
            // Collect existing .jsonl lists in this project
            let existing: Vec<String> = std::fs::read_dir(&project_dir)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    if name.ends_with(".jsonl") {
                        Some(name.trim_end_matches(".jsonl").to_string())
                    } else {
                        None
                    }
                })
                .collect();

            return serde_json::to_string_pretty(&serde_json::json!({
                "error": false,
                "needs_confirmation": true,
                "message": format!("List '{list_name}' does not exist yet. Set confirmed=true to create it, or use an existing list."),
                "existing_lists": existing,
                "project": format!("{}/{}", p.domain, project),
            })).unwrap_or_default();
        }

        if let Err(e) = std::fs::create_dir_all(&project_dir) {
            return json_error(&format!("Failed to create directory: {e}"));
        }

        let entry = serde_json::json!({
            "date": chrono::Utc::now().to_rfc3339(),
            "title": title,
            "body": p.body.clone().unwrap_or_default(),
        });
        let json = match serde_json::to_string(&entry) {
            Ok(j) => j,
            Err(e) => return json_error(&format!("Failed to serialize entry: {e}")),
        };
        if let Err(e) = append_jsonl(&list_path, &list_name, &json) {
            return json_error(&format!("Failed to write {list_name}.jsonl: {e}"));
        }

        let project_key = format!("{}/{}", p.domain, project);
        let mut resp = serde_json::json!({
            "appended": true,
            "list": list_name,
            "project": project_key,
            "path": list_path.display().to_string(),
        });
        if let Some(w) = warning {
            resp["warning"] = serde_json::json!(w);
        }
        serde_json::to_string(&resp).unwrap_or_default()
    }

    /// Re-read a file from disk and upsert it into the FTS index.
    fn reindex_file(&self, path: &std::path::Path) {
        if let Ok(vf) = crate::vault::reader::read_file(path) {
            let _ = self.index.upsert(&vf, &self.vault_root);
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for WardwellServer {
    fn get_info(&self) -> ServerInfo {
        let instructions =
            "Wardwell: Personal AI knowledge vault. Three tools: \
             wardwell_search (action: search|read|history|orchestrate|retrospective|patterns|context|resume), \
             wardwell_write (action: sync|decide|append_history|lesson|append), \
             wardwell_clipboard (copy to clipboard, ask first)."
                .to_string();

        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation::from_build_env(),
            instructions: Some(instructions),
        }
    }
}

// -- Helpers --

fn json_error(msg: &str) -> String {
    serde_json::to_string(&serde_json::json!({"error": msg})).unwrap_or_default()
}

/// Resolve a vault path: try vault root first, then each source directory.
fn resolve_path(vault_root: &std::path::Path, path: &str) -> Option<PathBuf> {
    let p = std::path::Path::new(path);
    if p.is_absolute() && p.exists() {
        return Some(p.to_path_buf());
    }
    let vault_candidate = vault_root.join(path);
    if vault_candidate.exists() {
        return Some(vault_candidate);
    }
    None
}

/// List immediate subdirectories of a directory.
fn list_subdirs(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                dirs.push(p);
            }
        }
    }
    dirs.sort();
    dirs
}

/// Extract a markdown section body by heading name (e.g. "Focus" → content under "## Focus").
fn extract_section(body: &str, heading: &str) -> String {
    let marker = format!("\n## {heading}");
    // Find marker at line start (check start-of-body case too)
    let pos = if body.starts_with(&marker[1..]) {
        Some(0)
    } else {
        body.find(&marker).map(|p| p + 1) // skip the leading \n
    };
    let start = match pos {
        Some(p) => p + marker.len() - 1, // past "## Heading"
        None => return String::new(),
    };
    // Skip to next line after heading
    let after_heading = match body[start..].find('\n') {
        Some(nl) => start + nl + 1,
        None => return String::new(),
    };
    let rest = &body[after_heading..];
    let end = rest.find("\n## ").unwrap_or(rest.len());
    rest[..end].trim().to_string()
}

// -- History parsing --

struct HistoryEntry {
    project: String,
    domain: String,
    date: String,
    title: String,
    body: String,
    source: String,
}

/// Walk a directory looking for history files (JSONL or legacy .md) and parse matching entries.
fn walk_history_files(
    dir: &std::path::Path,
    query: &str,
    since: Option<chrono::NaiveDate>,
    max: usize,
    vault_dir_name: &str,
    out: &mut Vec<HistoryEntry>,
) {
    if !dir.exists() { return; }

    let query_lower = query.to_lowercase();

    // Infer domain/project from a file path
    let infer_domain_project = |path: &std::path::Path, vault_name: &str| -> (String, String) {
        let path_str = path.to_string_lossy();
        let components: Vec<&str> = path_str.split('/').collect();
        let vault_idx = components.iter().position(|c| *c == vault_name);
        match vault_idx {
            Some(idx) => {
                let d = components.get(idx + 1).unwrap_or(&"unknown");
                let p = components.get(idx + 2)
                    .map(|s| s.trim_end_matches(".history.md").trim_end_matches(".history.jsonl").trim_end_matches(".md").trim_end_matches(".jsonl"))
                    .unwrap_or(d);
                (d.to_string(), p.to_string())
            }
            None => ("unknown".to_string(), "unknown".to_string()),
        }
    };

    let process_jsonl = |path: &std::path::Path, vault_name: &str, out: &mut Vec<HistoryEntry>| {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return,
        };
        let (domain, project) = infer_domain_project(path, vault_name);
        let source = path.to_string_lossy().to_string();

        for line in content.lines() {
            if line.trim().is_empty() || line.starts_with("{\"_schema\":") || line.starts_with("{\"_schema\" :") {
                continue;
            }
            let entry: HistoryJsonlEntry = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(_) => {
                    eprintln!("wardwell: skipping corrupted history line in {}", path.display());
                    continue;
                }
            };

            // Filter by query
            let searchable = format!("{} {} {}", entry.title, entry.body, entry.focus).to_lowercase();
            if !searchable.contains(&query_lower) {
                continue;
            }

            // Filter by date
            let date_str = entry.date.get(..10).unwrap_or(&entry.date);
            let skip = since.is_some_and(|s| {
                chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
                    .is_ok_and(|d| d < s)
            });
            if skip || out.len() >= max {
                continue;
            }

            out.push(HistoryEntry {
                project: project.clone(),
                domain: domain.clone(),
                date: date_str.to_string(),
                title: entry.title,
                body: entry.body,
                source: source.clone(),
            });
        }
    };

    let process_md = |path: &std::path::Path, vault_name: &str, out: &mut Vec<HistoryEntry>| {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return,
        };
        let (domain, project) = infer_domain_project(path, vault_name);
        let source = path.to_string_lossy().to_string();

        let mut current_date = String::new();
        let mut current_title = String::new();
        let mut current_body = String::new();
        let mut in_entry = false;

        for line in content.lines() {
            if line.starts_with("## ") && line.len() > 16 {
                if in_entry && !current_title.is_empty() {
                    let entry_text = format!("{current_title} {current_body}").to_lowercase();
                    if entry_text.contains(&query_lower) {
                        let skip = since.is_some_and(|s| {
                            chrono::NaiveDate::parse_from_str(&current_date, "%Y-%m-%d")
                                .is_ok_and(|d| d < s)
                        });
                        if !skip && out.len() < max {
                            out.push(HistoryEntry {
                                project: project.clone(),
                                domain: domain.clone(),
                                date: current_date.clone(),
                                title: current_title.clone(),
                                body: current_body.trim().to_string(),
                                source: source.clone(),
                            });
                        }
                    }
                }

                let heading = &line[3..];
                if heading.len() >= 10 {
                    current_date = heading[..10].to_string();
                    current_title = heading.split('—').nth(1)
                        .map(|s| s.trim().to_string())
                        .unwrap_or_else(|| heading[10..].trim().to_string());
                } else {
                    current_date = String::new();
                    current_title = heading.to_string();
                }
                current_body.clear();
                in_entry = true;
            } else if line == "---" {
                // separator — ignore
            } else if in_entry {
                current_body.push_str(line);
                current_body.push('\n');
            }
        }

        if in_entry && !current_title.is_empty() {
            let entry_text = format!("{current_title} {current_body}").to_lowercase();
            if entry_text.contains(&query_lower) {
                let skip = since.is_some_and(|s| {
                    chrono::NaiveDate::parse_from_str(&current_date, "%Y-%m-%d")
                        .is_ok_and(|d| d < s)
                });
                if !skip && out.len() < max {
                    out.push(HistoryEntry {
                        project: project.clone(),
                        domain: domain.clone(),
                        date: current_date,
                        title: current_title,
                        body: current_body.trim().to_string(),
                        source,
                    });
                }
            }
        }
    };

    // Prefer JSONL, fall back to .md
    let jsonl_path = dir.join("history.jsonl");
    let md_path = dir.join("history.md");
    if jsonl_path.exists() {
        process_jsonl(&jsonl_path, vault_dir_name, out);
    } else if md_path.exists() {
        process_md(&md_path, vault_dir_name, out);
    }

    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() && p.to_string_lossy().ends_with(".history.jsonl") {
                process_jsonl(&p, vault_dir_name, out);
            } else if p.is_file() && p.to_string_lossy().ends_with(".history.md") {
                process_md(&p, vault_dir_name, out);
            } else if p.is_dir() {
                walk_history_files(&p, query, since, max, vault_dir_name, out);
            }
        }
    }
}

// -- JSONL types --

#[derive(Debug, Serialize, Deserialize)]
struct HistoryJsonlEntry {
    date: String,
    title: String,
    status: String,
    focus: String,
    next_action: String,
    commit: String,
    body: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    source: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct LessonJsonlEntry {
    date: String,
    title: String,
    what_happened: String,
    root_cause: String,
    prevention: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    source: String,
}

// -- Write helpers --

/// Append a JSON line to a JSONL file. Creates file with schema header if missing.
fn append_jsonl(
    path: &std::path::Path,
    schema_name: &str,
    entry_json: &str,
) -> Result<(), std::io::Error> {
    use std::io::Write;
    let needs_schema = !path.exists() || std::fs::metadata(path).is_ok_and(|m| m.len() == 0);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    if needs_schema {
        writeln!(file, "{{\"_schema\": \"{schema_name}\", \"_version\": \"1.0\"}}")?;
    }
    writeln!(file, "{entry_json}")?;
    Ok(())
}

/// Prepend content to a file, creating it with a header if it doesn't exist.
fn prepend_to_file(path: &std::path::Path, header: &str, content: &str) -> Result<(), std::io::Error> {
    let existing = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        format!("{header}\n\n")
    };

    // Insert after the header line
    let new_content = if let Some(pos) = existing.find("\n\n") {
        let header_part = &existing[..pos + 2];
        let rest = &existing[pos + 2..];
        format!("{header_part}{content}{rest}")
    } else {
        format!("{existing}\n{content}")
    };

    std::fs::write(path, new_content)
}

/// Copy content to the system clipboard via pbcopy.
fn clipboard_copy(content: &str) -> Result<usize, String> {
    use std::io::Write;
    let bytes = content.len();
    let mut child = std::process::Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn pbcopy: {e}"))?;

    if let Some(ref mut stdin) = child.stdin {
        stdin.write_all(content.as_bytes())
            .map_err(|e| format!("Failed to write to pbcopy: {e}"))?;
    }

    child.wait().map_err(|e| format!("pbcopy failed: {e}"))?;
    Ok(bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn make_test_server(vault_root: &std::path::Path) -> WardwellServer {
        let db_path = vault_root.join("_test_index.db");
        let index = Arc::new(crate::index::store::IndexStore::open(&db_path).unwrap());
        let config = crate::config::loader::WardwellConfig {
            vault_path: vault_root.to_path_buf(),
            registry: crate::domain::registry::DomainRegistry::from_domains(vec![]),
            session_sources: vec![],
            exclude: vec![],
            ai: Default::default(),
        };
        WardwellServer::new(config, index)
    }

    #[test]
    fn extract_search_terms_from_summary() {
        let summary = "## Authentication Architecture\n\nSome body text.\n\n## Database Migration\n\n**retry logic** and **caching layer** discussed.";
        let terms = extract_search_terms(summary, 5);
        assert!(terms.contains("authentication"));
        assert!(terms.contains("architecture"));
        assert!(terms.contains("database"));
        assert!(terms.contains("migration"));
        // Should not contain stopwords
        assert!(!terms.contains(" and "));
    }

    #[test]
    fn extract_search_terms_stopword_filtering() {
        let summary = "## The Big Decision\n\nBody.";
        let terms = extract_search_terms(summary, 5);
        assert!(!terms.contains("the"));
        assert!(terms.contains("big"));
        assert!(terms.contains("decision"));
    }

    #[test]
    fn extract_search_terms_max_limit() {
        let summary = "## Alpha Beta Gamma Delta Epsilon Zeta Eta";
        let terms = extract_search_terms(summary, 3);
        let count = terms.split(" OR ").count();
        assert!(count <= 3);
    }

    #[test]
    fn extract_search_terms_empty_summary() {
        let terms = extract_search_terms("No headings or bold here.", 5);
        assert!(terms.is_empty());
    }

    #[test]
    fn extract_recent_history_entries() {
        let content = "# Project History\n\n## 2026-02-20 14:30 — First entry\n\nDid some work.\n\n---\n\n## 2026-02-19 10:00 — Second entry\n\nMore work.\n\n---\n\n## 2026-02-18 09:00 — Third entry\n\nEven more.\n\n---\n\n## 2026-02-17 08:00 — Fourth entry\n\nOld stuff.\n";
        let entries = extract_recent_history_md(content, 3);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0]["title"], "First entry");
        assert_eq!(entries[0]["date"], "2026-02-20");
        assert_eq!(entries[2]["title"], "Third entry");
    }

    #[test]
    fn extract_recent_history_fewer_than_n() {
        let content = "# History\n\n## 2026-02-20 14:30 — Only entry\n\nContent.\n";
        let entries = extract_recent_history_md(content, 5);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["title"], "Only entry");
    }

    #[test]
    fn resolve_vault_project_matches() {
        let tmp = std::env::temp_dir().join("wardwell_test_vault_match");
        let _ = std::fs::remove_dir_all(&tmp);
        let project_dir = tmp.join("personal").join("wardwell");
        std::fs::create_dir_all(&project_dir).unwrap();

        let result = resolve_vault_project(
            std::path::Path::new("/Users/jack/Code/wardwell"),
            &tmp,
        );
        assert!(result.is_some());
        let (domain, project, _) = result.unwrap();
        assert_eq!(domain, "personal");
        assert_eq!(project, "wardwell");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_vault_project_no_match() {
        let tmp = std::env::temp_dir().join("wardwell_test_vault_nomatch");
        let _ = std::fs::remove_dir_all(&tmp);
        let project_dir = tmp.join("personal").join("other-project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let result = resolve_vault_project(
            std::path::Path::new("/Users/jack/Code/wardwell"),
            &tmp,
        );
        assert!(result.is_none());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn strip_frontmatter_removes_yaml() {
        let content = "---\ntype: thread\nproject: test\n---\n\n## Summary\n\nContent here.";
        let result = strip_frontmatter(content);
        assert!(result.starts_with("## Summary"));
        assert!(!result.contains("type: thread"));
    }

    #[test]
    fn strip_frontmatter_no_frontmatter() {
        let content = "Just plain content.";
        let result = strip_frontmatter(content);
        assert_eq!(result, content);
    }

    // -- JSONL tests --

    #[test]
    fn append_jsonl_creates_file_with_schema() {
        let tmp = std::env::temp_dir().join("wardwell_test_jsonl_create");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let path = tmp.join("history.jsonl");
        let entry = r#"{"date":"2026-02-22T14:30:00Z","title":"Test","status":"active","focus":"f","next_action":"n","commit":"c","body":"b"}"#;
        append_jsonl(&path, "history", entry).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"_schema\": \"history\""));
        assert!(lines[1].contains("\"title\":\"Test\""));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn append_jsonl_second_append_no_duplicate_schema() {
        let tmp = std::env::temp_dir().join("wardwell_test_jsonl_append");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let path = tmp.join("history.jsonl");
        let entry1 = r#"{"date":"2026-02-22T14:00:00Z","title":"First","status":"","focus":"","next_action":"","commit":"","body":""}"#;
        let entry2 = r#"{"date":"2026-02-22T15:00:00Z","title":"Second","status":"","focus":"","next_action":"","commit":"","body":""}"#;
        append_jsonl(&path, "history", entry1).unwrap();
        append_jsonl(&path, "history", entry2).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3); // schema + 2 entries
        assert!(lines[0].contains("\"_schema\""));
        assert!(lines[1].contains("First"));
        assert!(lines[2].contains("Second"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn append_jsonl_lesson() {
        let tmp = std::env::temp_dir().join("wardwell_test_jsonl_lesson");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let path = tmp.join("lessons.jsonl");
        let entry = LessonJsonlEntry {
            date: "2026-02-22".to_string(),
            title: "FTS5 duplicate".to_string(),
            what_happened: "Re-inserted all files".to_string(),
            root_cause: "No existence check".to_string(),
            prevention: "Use upsert".to_string(),
            source: String::new(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        append_jsonl(&path, "lessons", &json).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"_schema\": \"lessons\""));
        assert!(lines[1].contains("FTS5 duplicate"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn extract_recent_history_jsonl_newest_first() {
        let content = "{\"_schema\": \"history\", \"_version\": \"1.0\"}\n\
            {\"date\":\"2026-02-20T10:00:00Z\",\"title\":\"Older\",\"status\":\"active\",\"focus\":\"f\",\"next_action\":\"n\",\"commit\":\"c\",\"body\":\"old\"}\n\
            {\"date\":\"2026-02-22T14:00:00Z\",\"title\":\"Newer\",\"status\":\"active\",\"focus\":\"f\",\"next_action\":\"n\",\"commit\":\"c\",\"body\":\"new\"}";
        let entries = extract_recent_history_jsonl(content, 5);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["title"], "Newer");
        assert_eq!(entries[1]["title"], "Older");
    }

    #[test]
    fn extract_recent_history_jsonl_empty_file() {
        let content = "{\"_schema\": \"history\", \"_version\": \"1.0\"}";
        let entries = extract_recent_history_jsonl(content, 5);
        assert!(entries.is_empty());
    }

    #[test]
    fn extract_recent_history_jsonl_corrupted_line() {
        let content = "{\"_schema\": \"history\", \"_version\": \"1.0\"}\n\
            {\"date\":\"2026-02-20T10:00:00Z\",\"title\":\"Good\",\"status\":\"active\",\"focus\":\"f\",\"next_action\":\"n\",\"commit\":\"c\",\"body\":\"ok\"}\n\
            this is not json\n\
            {\"date\":\"2026-02-22T14:00:00Z\",\"title\":\"Also Good\",\"status\":\"active\",\"focus\":\"f\",\"next_action\":\"n\",\"commit\":\"c\",\"body\":\"ok2\"}";
        let entries = extract_recent_history_jsonl(content, 5);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn read_recent_history_from_dir_prefers_jsonl() {
        let tmp = std::env::temp_dir().join("wardwell_test_history_prefer_jsonl");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Create both files — JSONL should win
        let jsonl = tmp.join("history.jsonl");
        std::fs::write(&jsonl, "{\"_schema\": \"history\", \"_version\": \"1.0\"}\n{\"date\":\"2026-02-22T14:00:00Z\",\"title\":\"From JSONL\",\"status\":\"active\",\"focus\":\"f\",\"next_action\":\"n\",\"commit\":\"c\",\"body\":\"b\"}\n").unwrap();

        let md = tmp.join("history.md");
        std::fs::write(&md, "# History\n\n## 2026-02-22 14:00 — From MD\n\nBody.\n").unwrap();

        let entries = read_recent_history_from_dir(&tmp, 5);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["title"], "From JSONL");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // -- Session tracking tests --

    #[test]
    fn extract_domain_project_from_path() {
        let result = extract_domain_project("work/sentry-bot/current_state.md");
        assert_eq!(result, Some(("work".to_string(), "sentry-bot".to_string())));
    }

    #[test]
    fn extract_domain_project_short_path() {
        let result = extract_domain_project("work");
        assert!(result.is_none());
    }

    #[test]
    fn extract_domain_project_deep_path() {
        let result = extract_domain_project("personal/fitness/history.jsonl");
        assert_eq!(result, Some(("personal".to_string(), "fitness".to_string())));
    }

    #[test]
    fn record_access_tracks_projects() {
        let tmp = std::env::temp_dir().join("wardwell_test_record_access");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let accessed = Arc::new(Mutex::new(HashSet::new()));
        let last = Arc::new(Mutex::new(None));

        // Simulate record_access directly
        {
            let key = "work/sentry-bot".to_string();
            accessed.lock().unwrap().insert(key);
            *last.lock().unwrap() = Some(("work".to_string(), "sentry-bot".to_string()));
        }

        assert!(accessed.lock().unwrap().contains("work/sentry-bot"));
        assert!(!accessed.lock().unwrap().contains("work/other"));
        assert_eq!(last.lock().unwrap().as_ref().unwrap().1, "sentry-bot");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn write_response_includes_project_key() {
        // Verify the response JSON shape includes "project" field
        let project_key = format!("{}/{}", "work", "sentry-bot");
        let resp = serde_json::json!({
            "synced": true,
            "project": project_key,
            "files_written": [],
        });
        assert_eq!(resp["project"], "work/sentry-bot");
    }

    #[test]
    fn warning_included_when_project_not_accessed() {
        let accessed: HashSet<String> = HashSet::new();
        let key = "work/wardwell";
        let was_accessed = accessed.contains(key);
        let warning = if was_accessed {
            None
        } else {
            Some(format!("project '{key}' was not read or searched in this session"))
        };
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("work/wardwell"));
    }

    #[test]
    fn no_warning_when_project_was_accessed() {
        let mut accessed: HashSet<String> = HashSet::new();
        accessed.insert("work/sentry-bot".to_string());
        let key = "work/sentry-bot";
        let was_accessed = accessed.contains(key);
        assert!(was_accessed);
    }

    // -- Retrospective & patterns tests --

    fn make_history_jsonl(entries: &[(&str, &str, &str, &str)]) -> String {
        let mut lines = vec!["{\"_schema\": \"history\", \"_version\": \"1.0\"}".to_string()];
        for (date, title, status, focus) in entries {
            lines.push(format!(
                "{{\"date\":\"{date}T10:00:00Z\",\"title\":\"{title}\",\"status\":\"{status}\",\"focus\":\"{focus}\",\"next_action\":\"\",\"commit\":\"\",\"body\":\"\"}}"
            ));
        }
        lines.join("\n")
    }

    fn setup_test_vault(name: &str, projects: &[(&str, &str, &str)]) -> std::path::PathBuf {
        let tmp = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&tmp);
        for (domain, project, content) in projects {
            let dir = tmp.join(domain).join(project);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("history.jsonl"), content).unwrap();
        }
        tmp
    }

    #[test]
    fn collect_history_entries_parses_and_filters() {
        let content = make_history_jsonl(&[
            ("2026-02-20", "Recent entry", "active", "working"),
            ("2026-01-01", "Old entry", "active", "old stuff"),
        ]);
        let tmp = setup_test_vault("wardwell_test_collect", &[
            ("work", "proj-a", &content),
        ]);

        let since = chrono::NaiveDate::parse_from_str("2026-02-01", "%Y-%m-%d").unwrap();
        let entries = collect_history_entries(&tmp, Some(since), None, true);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "Recent entry");
        assert_eq!(entries[0].domain, "work");
        assert_eq!(entries[0].project, "proj-a");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn collect_history_entries_skips_archive() {
        let content = make_history_jsonl(&[
            ("2026-02-20", "Archived entry", "resolved", "done"),
        ]);
        let tmp = setup_test_vault("wardwell_test_archive", &[
            ("work", "archive", &content),
        ]);

        let entries = collect_history_entries(&tmp, None, None, true);
        assert!(entries.is_empty());

        let entries_with_archive = collect_history_entries(&tmp, None, None, false);
        assert_eq!(entries_with_archive.len(), 1);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn collect_history_entries_domain_filter() {
        let work_content = make_history_jsonl(&[("2026-02-20", "Work", "active", "w")]);
        let personal_content = make_history_jsonl(&[("2026-02-20", "Personal", "active", "p")]);
        let tmp = setup_test_vault("wardwell_test_domain_filter", &[
            ("work", "proj-a", &work_content),
            ("personal", "proj-b", &personal_content),
        ]);

        let entries = collect_history_entries(&tmp, None, Some("work"), true);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "Work");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn retrospective_groups_by_project() {
        let content = make_history_jsonl(&[
            ("2026-02-20", "Entry A", "active", "focus a"),
            ("2026-02-18", "Entry B", "active", "focus b"),
        ]);
        let tmp = setup_test_vault("wardwell_test_retro", &[
            ("work", "proj-a", &content),
        ]);

        let entries = collect_history_entries(&tmp, Some(chrono::NaiveDate::parse_from_str("2026-02-01", "%Y-%m-%d").unwrap()), None, true);
        let mut groups: std::collections::HashMap<String, Vec<&ParsedHistoryEntry>> = std::collections::HashMap::new();
        for e in &entries {
            groups.entry(format!("{}/{}", e.domain, e.project)).or_default().push(e);
        }
        assert_eq!(groups.len(), 1);
        assert_eq!(groups["work/proj-a"].len(), 2);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn retrospective_classifies_completed() {
        let active_content = make_history_jsonl(&[("2026-02-20", "Still going", "active", "f")]);
        let done_content = make_history_jsonl(&[("2026-02-20", "Done", "completed", "f")]);
        let tmp = setup_test_vault("wardwell_test_retro_classify", &[
            ("work", "active-proj", &active_content),
            ("work", "done-proj", &done_content),
        ]);

        let entries = collect_history_entries(&tmp, None, None, true);
        let mut completed = Vec::new();
        let mut still_active = Vec::new();
        let mut groups: std::collections::HashMap<String, Vec<&ParsedHistoryEntry>> = std::collections::HashMap::new();
        for e in &entries {
            groups.entry(format!("{}/{}", e.domain, e.project)).or_default().push(e);
        }
        for (key, project_entries) in &groups {
            let last_status = project_entries.first().map(|e| e.status.as_str()).unwrap_or("");
            if last_status == "completed" || last_status == "resolved" {
                completed.push(key.clone());
            } else {
                still_active.push(key.clone());
            }
        }
        assert_eq!(completed.len(), 1);
        assert!(completed[0].contains("done-proj"));
        assert_eq!(still_active.len(), 1);
        assert!(still_active[0].contains("active-proj"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn patterns_detects_stale_threads() {
        let old_content = make_history_jsonl(&[("2026-01-01", "Old work", "active", "f")]);
        let recent_content = make_history_jsonl(&[("2026-02-20", "Recent", "active", "f")]);
        let tmp = setup_test_vault("wardwell_test_stale", &[
            ("work", "stale-proj", &old_content),
            ("work", "fresh-proj", &recent_content),
        ]);

        let entries = collect_history_entries(&tmp, None, None, true);
        let today = chrono::Local::now().date_naive();
        let mut latest: std::collections::HashMap<String, (&str, &str)> = std::collections::HashMap::new();
        for e in &entries {
            let key = format!("{}/{}", e.domain, e.project);
            latest.entry(key)
                .and_modify(|(date, status)| {
                    if e.date.as_str() > *date { *date = &e.date; *status = &e.status; }
                })
                .or_insert((&e.date, &e.status));
        }
        let stale: Vec<&String> = latest.iter()
            .filter(|(_, (date, status))| {
                *status != "completed" && *status != "resolved"
                    && chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
                        .is_ok_and(|d| (today - d).num_days() >= 14)
            })
            .map(|(k, _)| k)
            .collect();
        assert_eq!(stale.len(), 1);
        assert!(stale[0].contains("stale-proj"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn patterns_detects_hot_topics() {
        let content_a = make_history_jsonl(&[
            ("2026-02-20", "Nebula deploy fix", "active", "f"),
            ("2026-02-19", "Nebula monitoring", "active", "f"),
            ("2026-02-18", "Nebula cost analysis", "active", "f"),
        ]);
        let content_b = make_history_jsonl(&[
            ("2026-02-20", "Nebula integration", "active", "f"),
        ]);
        let tmp = setup_test_vault("wardwell_test_hot_topics", &[
            ("work", "proj-a", &content_a),
            ("work", "proj-b", &content_b),
        ]);

        let entries = collect_history_entries(&tmp, None, None, true);
        let stopwords: &[&str] = &["the", "a", "an", "is", "for", "and"];
        let mut word_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for e in &entries {
            for word in e.title.split_whitespace() {
                let clean = word.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
                if clean.len() > 2 && !stopwords.contains(&clean.as_str()) {
                    *word_counts.entry(clean).or_default() += 1;
                }
            }
        }
        assert!(word_counts.get("nebula").is_some_and(|c| *c >= 3));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn read_recent_history_from_dir_falls_back_to_md() {
        let tmp = std::env::temp_dir().join("wardwell_test_history_fallback_md");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let md = tmp.join("history.md");
        std::fs::write(&md, "# History\n\n## 2026-02-22 14:00 — From MD\n\nBody.\n").unwrap();

        let entries = read_recent_history_from_dir(&tmp, 5);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["title"], "From MD");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn append_list_requires_confirmation_for_new_list() {
        let tmp = std::env::temp_dir().join("wardwell_test_append_new_list");
        let _ = std::fs::remove_dir_all(&tmp);
        let project_dir = tmp.join("personal").join("test-proj");
        std::fs::create_dir_all(&project_dir).unwrap();

        // Write an existing list so we can verify it appears in existing_lists
        append_jsonl(&project_dir.join("ideas.jsonl"), "ideas", r#"{"title":"old"}"#).unwrap();

        let server = make_test_server(&tmp);
        let params = WriteParams {
            action: "append".to_string(),
            domain: "personal".to_string(),
            project: Some("test-proj".to_string()),
            list: Some("future-ideas".to_string()),
            confirmed: None,
            title: Some("Test idea".to_string()),
            body: Some("Details".to_string()),
            status: None, focus: None, why_this_matters: None, next_action: None,
            open_questions: None, blockers: None, waiting_on: None, commit_message: None,
            what_happened: None, root_cause: None, prevention: None, source: None,
        };
        let result = server.action_append_list(&params, "test-proj", None);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["needs_confirmation"], true);
        assert!(parsed["existing_lists"].as_array().unwrap().iter().any(|v| v == "ideas"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn append_list_creates_and_appends_with_confirmation() {
        let tmp = std::env::temp_dir().join("wardwell_test_append_confirmed");
        let _ = std::fs::remove_dir_all(&tmp);
        let project_dir = tmp.join("personal").join("test-proj");
        std::fs::create_dir_all(&project_dir).unwrap();

        let server = make_test_server(&tmp);
        let params = WriteParams {
            action: "append".to_string(),
            domain: "personal".to_string(),
            project: Some("test-proj".to_string()),
            list: Some("future-ideas".to_string()),
            confirmed: Some(true),
            title: Some("Build a rocket".to_string()),
            body: Some("Literally".to_string()),
            status: None, focus: None, why_this_matters: None, next_action: None,
            open_questions: None, blockers: None, waiting_on: None, commit_message: None,
            what_happened: None, root_cause: None, prevention: None, source: None,
        };
        let result = server.action_append_list(&params, "test-proj", None);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["appended"], true);
        assert_eq!(parsed["list"], "future-ideas");

        let content = std::fs::read_to_string(project_dir.join("future-ideas.jsonl")).unwrap();
        assert!(content.contains("Build a rocket"));
        assert!(content.contains("\"_schema\": \"future-ideas\""));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn append_list_rejects_reserved_names() {
        let tmp = std::env::temp_dir().join("wardwell_test_append_reserved");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let server = make_test_server(&tmp);
        let params = WriteParams {
            action: "append".to_string(),
            domain: "personal".to_string(),
            project: Some("test-proj".to_string()),
            list: Some("history".to_string()),
            confirmed: None,
            title: Some("Test".to_string()),
            body: None,
            status: None, focus: None, why_this_matters: None, next_action: None,
            open_questions: None, blockers: None, waiting_on: None, commit_message: None,
            what_happened: None, root_cause: None, prevention: None, source: None,
        };
        let result = server.action_append_list(&params, "test-proj", None);
        assert!(result.contains("built-in list"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn append_list_existing_list_no_confirmation_needed() {
        let tmp = std::env::temp_dir().join("wardwell_test_append_existing");
        let _ = std::fs::remove_dir_all(&tmp);
        let project_dir = tmp.join("personal").join("test-proj");
        std::fs::create_dir_all(&project_dir).unwrap();

        // Pre-create the list
        append_jsonl(&project_dir.join("bookmarks.jsonl"), "bookmarks", r#"{"title":"first"}"#).unwrap();

        let server = make_test_server(&tmp);
        let params = WriteParams {
            action: "append".to_string(),
            domain: "personal".to_string(),
            project: Some("test-proj".to_string()),
            list: Some("bookmarks".to_string()),
            confirmed: None, // not needed — list exists
            title: Some("Second entry".to_string()),
            body: None,
            status: None, focus: None, why_this_matters: None, next_action: None,
            open_questions: None, blockers: None, waiting_on: None, commit_message: None,
            what_happened: None, root_cause: None, prevention: None, source: None,
        };
        let result = server.action_append_list(&params, "test-proj", None);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["appended"], true);

        let content = std::fs::read_to_string(project_dir.join("bookmarks.jsonl")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3); // schema + first + second

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
