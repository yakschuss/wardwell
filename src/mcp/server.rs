use crate::config::loader::WardwellConfig;
use crate::domain::registry::DomainRegistry;
use crate::index::fts::SearchQuery;
use crate::index::store::IndexStore;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

/// The Wardwell MCP server.
#[derive(Clone)]
pub struct WardwellServer {
    tool_router: ToolRouter<Self>,
    pub config: Arc<WardwellConfig>,
    pub index: Arc<IndexStore>,
    pub vault_root: PathBuf,
    pub registry: Arc<RwLock<DomainRegistry>>,
}

// -- Tool parameter types --

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    #[schemars(description = "search: FTS query across vault. read: full file content. history: query across history files. orchestrate: prioritized project queue. context: full session context by ID.")]
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
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WriteParams {
    #[schemars(description = "sync: replace current_state.md and optionally append history. decide: append to decisions.md. append_history: append to history.jsonl. lesson: append to lessons.jsonl.")]
    pub action: String,
    #[schemars(description = "Domain folder under vault root (e.g., 'work', 'personal')")]
    pub domain: String,
    #[schemars(description = "Project folder within the domain (e.g., 'my-project')")]
    pub project: String,

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
            "context" => self.action_context(&p).await,
            other => json_error(&format!("Unknown action: '{other}'. Use search, read, history, orchestrate, or context.")),
        }
    }

    #[tool(description = "Write to the vault. Sync project state, record decisions, append history, or record lessons. Use `action` to specify the operation.")]
    async fn wardwell_write(&self, params: Parameters<WriteParams>) -> String {
        let p = params.0;
        match p.action.as_str() {
            "sync" => self.action_sync(&p),
            "decide" => self.action_decide(&p),
            "append_history" => self.action_append_history(&p),
            "lesson" => self.action_lesson(&p),
            other => json_error(&format!("Unknown action: '{other}'. Use sync, decide, append_history, or lesson.")),
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
            Ok(results) => serde_json::to_string_pretty(&results).unwrap_or_default(),
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

                    let entry = serde_json::json!({
                        "domain": domain_name,
                        "project": project_name,
                        "status": status_str,
                        "focus": focus,
                        "next_action": next_action,
                    });

                    match status_str.as_str() {
                        "blocked" => blocked.push(entry),
                        "completed" | "resolved" => completed_recently.push(entry),
                        _ => active.push(entry),
                    }
                }
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
    fn action_sync(&self, p: &WriteParams) -> String {
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

        let project_dir = self.vault_root.clone().join(&p.domain).join(&p.project);
        if let Err(e) = std::fs::create_dir_all(&project_dir) {
            return json_error(&format!("Failed to create directory: {e}"));
        }

        let now = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();

        // Build current_state.md
        let mut content = format!(
            "---\nchat_name: {project}\nupdated: {now}\nstatus: {status}\ntype: project\ncontext: {domain}\n---\n\n# {project}\n\n## Focus\n{focus}\n",
            project = p.project,
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
        files_written.push(format!("{}/{}/{}/current_state.md", self.vault_root.display(), p.domain, p.project));

        // Always append history entry on sync
        let history_path = project_dir.join("history.jsonl");
        let jsonl_entry = HistoryJsonlEntry {
            date: chrono::Utc::now().to_rfc3339(),
            title: p.title.clone().unwrap_or_else(|| commit_message.clone()),
            status: status.clone(),
            focus: focus.clone(),
            next_action: next_action.clone(),
            commit: commit_message.clone(),
            body: p.body.clone().unwrap_or_default(),
        };
        let json = match serde_json::to_string(&jsonl_entry) {
            Ok(j) => j,
            Err(e) => return json_error(&format!("Failed to serialize history entry: {e}")),
        };
        if let Err(e) = append_jsonl(&history_path, "history", &json) {
            return json_error(&format!("Failed to write history.jsonl: {e}"));
        }
        files_written.push(format!("{}/{}/{}/history.jsonl", self.vault_root.display(), p.domain, p.project));

        // Update FTS index for written files
        self.reindex_file(&state_path);

        serde_json::to_string(&serde_json::json!({
            "synced": true,
            "files_written": files_written,
        })).unwrap_or_default()
    }

    fn action_decide(&self, p: &WriteParams) -> String {
        let title = match &p.title {
            Some(t) => t.clone(),
            None => return json_error("'title' is required for action 'decide'."),
        };
        let body = match &p.body {
            Some(b) => b.clone(),
            None => return json_error("'body' is required for action 'decide'."),
        };

        let project_dir = self.vault_root.clone().join(&p.domain).join(&p.project);
        if let Err(e) = std::fs::create_dir_all(&project_dir) {
            return json_error(&format!("Failed to create directory: {e}"));
        }

        let decisions_path = project_dir.join("decisions.md");
        let now = chrono::Local::now().format("%Y-%m-%d").to_string();

        let entry = format!("## {now} — {title}\n\n{body}\n\n---\n\n");

        if let Err(e) = prepend_to_file(&decisions_path, &format!("# {} Decisions", p.project), &entry) {
            return json_error(&format!("Failed to write decisions.md: {e}"));
        }

        self.reindex_file(&decisions_path);

        let rel = format!("{}/{}/{}/decisions.md", self.vault_root.display(), p.domain, p.project);
        serde_json::to_string(&serde_json::json!({
            "recorded": true,
            "path": rel,
        })).unwrap_or_default()
    }

    fn action_append_history(&self, p: &WriteParams) -> String {
        let title = match &p.title {
            Some(t) => t.clone(),
            None => return json_error("'title' is required for action 'append_history'."),
        };

        let project_dir = self.vault_root.clone().join(&p.domain).join(&p.project);
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
        };
        let json = match serde_json::to_string(&jsonl_entry) {
            Ok(j) => j,
            Err(e) => return json_error(&format!("Failed to serialize history entry: {e}")),
        };
        if let Err(e) = append_jsonl(&history_path, "history", &json) {
            return json_error(&format!("Failed to write history.jsonl: {e}"));
        }

        let rel = format!("{}/{}/{}/history.jsonl", self.vault_root.display(), p.domain, p.project);
        serde_json::to_string(&serde_json::json!({
            "appended": true,
            "path": rel,
        })).unwrap_or_default()
    }

    fn action_lesson(&self, p: &WriteParams) -> String {
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

        let project_dir = self.vault_root.clone().join(&p.domain).join(&p.project);
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
        };
        let json = match serde_json::to_string(&jsonl_entry) {
            Ok(j) => j,
            Err(e) => return json_error(&format!("Failed to serialize lesson entry: {e}")),
        };
        if let Err(e) = append_jsonl(&lessons_path, "lessons", &json) {
            return json_error(&format!("Failed to write lessons.jsonl: {e}"));
        }

        let rel = format!("{}/{}/{}/lessons.jsonl", self.vault_root.display(), p.domain, p.project);
        serde_json::to_string(&serde_json::json!({
            "recorded": true,
            "path": rel,
        })).unwrap_or_default()
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
             wardwell_search (action: search|read|history|orchestrate|context), \
             wardwell_write (action: sync|decide|append_history|lesson), \
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
}

#[derive(Debug, Serialize, Deserialize)]
struct LessonJsonlEntry {
    date: String,
    title: String,
    what_happened: String,
    root_cause: String,
    prevention: String,
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
}
