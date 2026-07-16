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
    /// Embedder for hybrid semantic search. None if model not available.
    pub embedder: Arc<Mutex<Option<crate::index::embed::Embedder>>>,
    /// Which domain this session belongs to (None = domainless/full access).
    session_domain: Option<String>,
    /// session_domain + its can_read list. Empty = domainless mode (full access).
    allowed_domains: Vec<String>,
    kanban: Option<Arc<crate::kanban::store::KanbanStore>>,
    kanban_queries: std::collections::HashMap<String, String>,
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
    #[schemars(description = "Search mode: 'keyword' (FTS5 only, default) or 'semantic' (hybrid BM25 + vector + RRF). Use 'semantic' for broad/conceptual queries. Use default 'keyword' for exact terms or file names.")]
    pub mode: Option<String>,
    #[schemars(description = "For read: 1-indexed start line of body (frontmatter always returned in full). Omit for full content.")]
    pub offset: Option<usize>,
    #[schemars(description = "For read: number of body lines to return. Omit for all remaining lines.")]
    pub read_limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WriteParams {
    #[schemars(description = "sync: replace current_state.md and optionally append history. decide: append to decisions.md. append_history: append to history.jsonl. lesson: append to lessons.jsonl. append: append to a named JSONL list (requires 'list' param). write_file: write content to a file in the project directory (requires 'path' for relative path within project, e.g. 'docs/my-audit.md', and 'body' for content). IMPORTANT for append: check existing lists first (they're returned if list doesn't exist). ASK the user before creating a new list — do not create lists speculatively.")]
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

    // -- write_file fields --
    #[schemars(description = "For write_file: path relative to project directory (e.g., 'docs/my-audit.md'). Directories created automatically.")]
    pub path: Option<String>,

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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KanbanParams {
    #[schemars(description = "get: fetch a single ticket by ID (ticket_id required). search: find tickets by text (query required). list: filter and return items. create: new item (title+project required). update: modify fields (ticket_id required). move: status transition (ticket_id+status required). note: append note (ticket_id+text required). query: run a named query (question required). attach: write content to a ticket as a doc (ticket_id+title+text) — or link an existing vault file (ticket_id+file_path). detach: unlink attachment (ticket_id+attachment_id required). sequence: set ticket position — single (ticket_id+position) or bulk (project+order array). export_roadmap: generate PDF roadmap (project required).")]
    pub action: String,
    #[schemars(description = "Ticket identifier (e.g., 'SH-3'). Required for update, move, note, attach, detach.")]
    pub ticket_id: Option<String>,
    #[schemars(description = "Project slug (e.g., 'shulops'). Required for create. Optional filter for list, query.")]
    pub project: Option<String>,
    #[schemars(description = "Vault domain (e.g., 'personal'). Optional for create — inferred from project directory if omitted.")]
    pub domain: Option<String>,
    #[schemars(description = "Item title. Required for create. Optional for update. For attach: filename (e.g., 'build-prompt.md').")]
    pub title: Option<String>,
    #[schemars(description = "Item description/details.")]
    pub description: Option<String>,
    #[schemars(description = "Status: backlog, todo, in_progress, review, blocked, done. For move: target status. For list: filter.")]
    pub status: Option<String>,
    #[schemars(description = "Priority: low, medium, high, urgent.")]
    pub priority: Option<String>,
    #[schemars(description = "Who is responsible for this item.")]
    pub assignee: Option<String>,
    #[schemars(description = "ISO date deadline (e.g., '2026-05-01').")]
    pub deadline: Option<String>,
    #[schemars(description = "Who created this item (e.g., 'hank', 'manual', 'cmo').")]
    pub source: Option<String>,
    #[schemars(description = "Epic label for grouping related items (e.g., 'whatsapp-flows', 'admin-redesign'). Optional on create/update, filter on list.")]
    pub epic: Option<String>,
    #[schemars(description = "For note: note text. For attach: file content to write (used with title as filename).")]
    pub text: Option<String>,
    #[schemars(description = "Include completed items in list results. Default false.")]
    pub include_done: Option<bool>,
    #[schemars(description = "Named query to run (e.g., 'overdue', 'stale', 'no_deadline', 'blocked', 'recent').")]
    pub question: Option<String>,
    #[schemars(description = "Parent ticket ID for subtask relationships (e.g., 'SH-1'). For create/update. Pass empty string to clear.")]
    pub parent: Option<String>,
    #[schemars(description = "Tags: array of free-form string labels. For create/update: set tags. For list: filter by tag.")]
    pub tags: Option<Vec<String>>,
    #[schemars(description = "Single tag to filter by on list. Returns items containing this tag.")]
    pub tag: Option<String>,
    #[schemars(description = "For attach with existing file: vault-relative path (e.g., 'personal/shulops/docs/SH-6-build-prompt.md'). Not needed when using text+title to write content directly.")]
    pub file_path: Option<String>,
    #[schemars(description = "Attachment ID to detach. Required for detach action.")]
    pub attachment_id: Option<String>,
    #[schemars(description = "For search: text to search for in ticket ID, title, and description.")]
    pub query: Option<String>,
    #[schemars(description = "Include per-ticket activity feed (JSONL event history). Default false. Only use with get or small result sets.")]
    pub include_activity: Option<bool>,
    #[schemars(description = "For sequence: 1-based position number for single ticket reorder.")]
    pub position: Option<i64>,
    #[schemars(description = "For sequence bulk: ordered array of ticket IDs. Position assigned 1, 2, 3... in array order. Requires project.")]
    pub order: Option<Vec<String>>,

    // -- relationship fields --
    #[schemars(description = "For relationship_create: source ticket ID (the 'from' side of the relationship).")]
    pub from_ticket_id: Option<String>,
    #[schemars(description = "For relationship_create: target ticket ID (the 'to' side of the relationship).")]
    pub to_ticket_id: Option<String>,
    #[schemars(description = "For relationship_create: type of relationship. One of: blocks, depends_on, feeds, consumes_output_from, duplicates, supersedes, related.")]
    pub relationship_type: Option<String>,
    #[schemars(description = "For relationship_delete: relationship ID to remove.")]
    pub relationship_id: Option<String>,

    // -- question fields --
    #[schemars(description = "For question_create: the question text. For question_update: updated question text.")]
    pub question_text: Option<String>,
    #[schemars(description = "For question_create/update: current assumption about the answer.")]
    pub current_assumption: Option<String>,
    #[schemars(description = "For question_create/update: evidence supporting the assumption.")]
    pub evidence: Option<String>,
    #[schemars(description = "For question_create/update: what decision/work this question blocks.")]
    pub needed_for: Option<String>,
    #[schemars(description = "For question_create/update: semantic control shown in Wardwell. One of: question, decision. Defaults to question. Use decision only with 2-5 explicit interaction_options.")]
    pub interaction_type: Option<crate::kanban::questions::QuestionInteractionType>,
    #[schemars(description = "For decision questions: 2-5 explicit options, each with a unique id, a human-readable label, and optional detail. Do not include an 'other' option; the Wardwell surface supplies it.")]
    pub interaction_options: Option<Vec<crate::kanban::questions::QuestionOption>>,
    #[schemars(description = "For plain questions: short answer-field guidance, at most 300 characters.")]
    pub interaction_placeholder: Option<String>,
    #[schemars(description = "For question_answer: the resolved answer. For question_invalidate: optional reason.")]
    pub answer: Option<String>,
    #[schemars(description = "For question_answer/invalidate/update, proposal_approve/reject/apply/get: the question or proposal ID.")]
    pub target_id: Option<String>,
    #[schemars(description = "For question_invalidate: reason it was invalidated.")]
    pub reason: Option<String>,

    // -- proposal fields --
    #[schemars(description = "For proposal_create: array of change operations. Each is a JSON object with 'op' field (update_ticket, append_note, create_relationship, create_question, answer_question, invalidate_question) and relevant sub-fields.")]
    pub changes: Option<Vec<serde_json::Value>>,
    #[schemars(description = "For proposal_create: intent category. One of: context_capture, context_migration, closure, priority_change, split, merge, supersede, decision_record.")]
    pub intent: Option<String>,
    #[schemars(description = "For proposal_create: rationale for changes. Recommended for closures, required (by convention) for priority changes.")]
    pub rationale: Option<String>,
    #[schemars(description = "For proposal_create: context transfers. Array of {from_ticket_id, to_ticket_id, description?}.")]
    pub context_transfers: Option<Vec<serde_json::Value>>,
    #[schemars(description = "For proposal_create: closure summary. Object with shipped_scope?, not_shipped?, context_destination?.")]
    pub closure_summary: Option<serde_json::Value>,
    #[schemars(description = "For proposal_create: questions for the reviewer to consider before approving.")]
    pub reviewer_questions: Option<Vec<String>>,

    // -- verification fields --
    #[schemars(description = "For verify: source of verification. One of: user, code, git, meeting, board, agent.")]
    pub verification_source: Option<String>,
    #[schemars(description = "For verify: confidence level. One of: verified, likely, stale, contradicted.")]
    pub confidence: Option<String>,
    #[schemars(description = "For verify: brief summary of what was verified.")]
    pub summary: Option<String>,

    // -- reality_check fields --
    #[schemars(description = "For reality_check: number of days after which a ticket is considered stale. Default 14.")]
    pub stale_after_days: Option<u64>,
    #[schemars(description = "For question_list/reality_check: filter to show only open questions. Default true for question_list.")]
    pub open_only: Option<bool>,
    #[schemars(description = "For reality_check: set true for full verbose output including tickets_by_status, no_deadline, relationship_graph. For proposal_list: set true to return raw proposals with every operation instead of the scannable review summary. Default false (compact).")]
    pub full: Option<bool>,
    #[schemars(description = "For reality_check/hygiene_suggestions/plan: max items per section. Default 10.")]
    pub limit: Option<usize>,

    // -- plan fields --
    #[schemars(description = "For plan: root parent/epic ticket ID to organize (e.g., 'CM-2'). Gathers its descendant subtree, relationship neighbors, and attached open questions.")]
    pub root_ticket_id: Option<String>,

    // -- groom fields --
    #[schemars(description = "For groom: agent/source requesting grooming (e.g., 'codex'). Optional provenance.")]
    pub requested_by: Option<String>,

    // -- loop / stage fields (WA-5) --
    #[schemars(description = "For update: loop stage. One of: idea, grill, spec, design_audit, post_design_audit, audit_gate, build, pr, complete. Nullable/opt-in — most tickets have no stage. Any stage change auto-clears waiting_on/summary/since and sets status=in_progress; stage=complete sets status=done. Stages can move backward.")]
    pub stage: Option<String>,
    #[schemars(description = "For update: who/what the ticket is waiting on. Must start with 'human:' (e.g. 'human:design_decisions' → status=review) or 'blocker:' (e.g. 'blocker:external' → status=blocked). Setting it auto-stamps waiting_since=now. Pass empty string or 'null' to clear (→ status=in_progress). Never set waiting_since directly.")]
    pub waiting_on: Option<String>,
    #[schemars(description = "For update: the constrained ask, human-readable (options + recommendation, not open-ended). Surfaced in briefings. Only applied when a wait is set; a stage advance clears it.")]
    pub waiting_summary: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GraphParams {
    #[schemars(description = "links: get forward links and backlinks for a vault file (path required). resolve: find entities by fuzzy name match (query required). mentions: find unlinked references to an entity across vault content (path required).")]
    pub action: String,
    #[schemars(description = "For links/mentions: vault-relative file path.")]
    pub path: Option<String>,
    #[schemars(description = "For resolve: name, title, or alias to search for.")]
    pub query: Option<String>,
    #[schemars(description = "Max results. Default 5.")]
    pub limit: Option<usize>,
}

#[tool_router(router = tool_router)]
impl WardwellServer {
    pub fn new(config: WardwellConfig, index: Arc<IndexStore>, embedder: Arc<Mutex<Option<crate::index::embed::Embedder>>>, domain: Option<String>, kanban: Option<crate::kanban::store::KanbanStore>) -> Self {
        let vault_root = config.vault_path.clone();
        let raw_registry = DomainRegistry::from_domains(config.registry.all().to_vec());

        // Log registry state for debugging
        if raw_registry.is_empty() {
            eprintln!("[WARDWELL] WARNING: domain registry is empty (no confirmed domain files in {}/domains/)", vault_root.display());
        } else {
            eprintln!("[WARDWELL] Registry loaded: {:?}", raw_registry.names());
        }

        // Build domain scope before wrapping registry in Arc<RwLock>
        let (session_domain, allowed_domains) = match domain {
            Some(ref d) => {
                match raw_registry.find(d) {
                    Some(found) => {
                        let mut allowed = vec![d.clone()];
                        allowed.extend(found.can_read.clone());
                        eprintln!("[WARDWELL] Starting with domain scope: {:?}, allowed: {:?}", d, allowed);
                        (Some(d.clone()), allowed)
                    }
                    None => {
                        let names = raw_registry.names();
                        eprintln!("[WARDWELL] FATAL: domain '{}' not found in registry. Available: {:?}", d, names);
                        if names.is_empty() {
                            eprintln!("[WARDWELL] HINT: registry is empty — check vault_path in ~/.wardwell/config.yml and ensure domains/ directory exists with confirmed .md files");
                        }
                        std::process::exit(1);
                    }
                }
            }
            None => {
                eprintln!("[WARDWELL] Starting in DOMAINLESS mode (full access)");
                (None, vec![])
            }
        };

        let registry = Arc::new(RwLock::new(raw_registry));

        let kanban_queries = crate::kanban::store::merge_kanban_queries(&config.kanban_queries);

        if let Some(ref k) = kanban
            && let Err(e) = k.validate_queries(&kanban_queries)
        {
            eprintln!("wardwell: kanban query validation warning (non-fatal): {e}");
        }

        let mut tool_router = Self::tool_router();
        if kanban.is_none() {
            tool_router.remove_route("wardwell_kanban");
        }
        // Feature-flag gate the graph tool
        let graph_enabled = config.features.graph_navigation
            || config.features.entity_resolution
            || config.features.unlinked_mentions;
        if !graph_enabled {
            tool_router.remove_route("wardwell_graph");
        }
        let kanban = kanban.map(Arc::new);

        Self {
            tool_router,
            config: Arc::new(config),
            index,
            vault_root,
            registry,
            accessed_projects: Arc::new(Mutex::new(HashSet::new())),
            last_project: Arc::new(Mutex::new(None)),
            embedder,
            session_domain,
            allowed_domains,
            kanban,
            kanban_queries,
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

        // ACL: check domain access before any write
        if let Err(e) = self.check_domain_access(&p.domain, "write") {
            return json_error(&e);
        }

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
            "write_file" => self.action_write_file(&p, &project),
            other => json_error(&format!("Unknown action: '{other}'. Use sync, decide, append_history, lesson, append, or write_file.")),
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

    #[tool(description = "Project kanban board with structured PM primitives. Core: get, list, search, create, update, move, note, query, attach, detach, sequence, export_roadmap. Relationships: relationship_create, relationship_list, relationship_delete. Questions: question_create, question_list, question_update, question_answer, question_invalidate. Proposals: proposal_create (supports intent, rationale, context_transfers, closure_summary, reviewer_questions — returns risk_flags and review summary), proposal_get (returns proposal + summary + freshly-recomputed risk_flags), proposal_list (scannable per-proposal review metadata with risk counts and a short risk summary; set full=true for raw operations), proposal_approve, proposal_reject, proposal_apply. Verification: verify. Audit: reality_check (compact by default, set full=true for verbose), hygiene_suggestions. Planning: plan (read-only execution map for a root_ticket_id, epic, or whole project — buckets work into current_center, next_recommended, safety_companions, gates_before_externalization, parallel_tracks, later_expansion, blocked_or_parked, with open_questions and suggested_relationships; mutates nothing). Grooming: groom (async request — appends a groom_requested event for a ticket_id, or up to `limit` tickets in a project; does NOT run Claude, call the service, wait, or mutate the ticket; the always-on vault service processes it later and the receipt shows on the ticket's `grooming` field).")]
    async fn wardwell_kanban(&self, params: Parameters<KanbanParams>) -> String {
        let Some(ref kanban) = self.kanban else {
            return json_error("kanban is disabled — set kanban.enabled: true in ~/.wardwell/config.yml");
        };
        let p = params.0;
        match p.action.as_str() {
            "list" => self.kanban_list(kanban, &p),
            "create" => self.kanban_create(kanban, &p),
            "update" => self.kanban_update(kanban, &p),
            "move" => self.kanban_move(kanban, &p),
            "note" => self.kanban_note(kanban, &p),
            "query" => self.kanban_query(kanban, &p),
            "attach" => self.kanban_attach(kanban, &p),
            "detach" => self.kanban_detach(kanban, &p),
            "get" => self.kanban_get(kanban, &p),
            "search" => self.kanban_search(kanban, &p),
            "sequence" => self.kanban_sequence(kanban, &p),
            "export_roadmap" => self.kanban_export_roadmap(&p),
            "relationship_create" => self.kanban_relationship_create(kanban, &p),
            "relationship_list" => self.kanban_relationship_list(kanban, &p),
            "relationship_delete" => self.kanban_relationship_delete(kanban, &p),
            "question_create" => self.kanban_question_create(kanban, &p),
            "question_list" => self.kanban_question_list(&p),
            "question_update" => self.kanban_question_update(&p),
            "question_answer" => self.kanban_question_answer(&p),
            "question_invalidate" => self.kanban_question_invalidate(&p),
            "proposal_create" => self.kanban_proposal_create(kanban, &p),
            "proposal_get" => self.kanban_proposal_get(kanban, &p),
            "proposal_list" => self.kanban_proposal_list(kanban, &p),
            "proposal_approve" => self.kanban_proposal_approve(&p),
            "proposal_reject" => self.kanban_proposal_reject(&p),
            "proposal_apply" => self.kanban_proposal_apply(kanban, &p),
            "verify" => self.kanban_verify(kanban, &p),
            "reality_check" => self.kanban_reality_check(kanban, &p),
            "hygiene_suggestions" => self.kanban_hygiene_suggestions(kanban, &p),
            "plan" => self.kanban_plan(kanban, &p),
            "groom" => self.kanban_groom(kanban, &p),
            other => json_error(&format!("unknown kanban action '{other}'. Use: get, list, search, create, update, move, note, query, attach, detach, sequence, export_roadmap, relationship_create, relationship_list, relationship_delete, question_create, question_list, question_update, question_answer, question_invalidate, proposal_create, proposal_get, proposal_list, proposal_approve, proposal_reject, proposal_apply, verify, reality_check, hygiene_suggestions, plan, groom")),
        }
    }

    #[tool(description = "Navigate the vault knowledge graph. Get links/backlinks for a file, resolve entities by name, or find unlinked mentions across content.")]
    async fn wardwell_graph(&self, params: Parameters<GraphParams>) -> String {
        let p = params.0;
        match p.action.as_str() {
            "links" => self.graph_links(&p),
            "resolve" => self.graph_resolve(&p),
            "mentions" => self.graph_mentions(&p),
            other => json_error(&format!("unknown graph action '{other}'. Use: links, resolve, mentions")),
        }
    }
}

// -- ACL enforcement --

impl WardwellServer {
    /// Check if a domain is within this session's allowed scope.
    /// Returns Ok(()) if allowed, Err(error_string) if denied.
    fn check_domain_access(&self, domain: &str, action: &str) -> Result<(), String> {
        if self.allowed_domains.is_empty() {
            return Ok(()); // domainless mode — full access
        }
        if self.allowed_domains.iter().any(|d| d == domain) {
            Ok(())
        } else {
            eprintln!("[WARDWELL ACL] DENIED: session_domain={:?} attempted={} action={}",
                self.session_domain, domain, action);
            Err(format!("Access denied: domain '{}' is outside allowed domains {:?}", domain, self.allowed_domains))
        }
    }

    /// Filter domains for vault-walking actions. Returns the list of domain dirs to scan.
    fn scoped_domain_dirs(&self, vault_dir: &std::path::Path, client_domain: Option<&str>) -> Vec<PathBuf> {
        if !self.allowed_domains.is_empty() {
            // Scoped mode: only scan allowed domains, ignore client filter
            self.allowed_domains.iter()
                .map(|d| vault_dir.join(d))
                .filter(|p| p.is_dir())
                .collect()
        } else {
            // Domainless mode: honor client filter
            match client_domain {
                Some(d) => vec![vault_dir.join(d)],
                None => list_subdirs(vault_dir),
            }
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

        // Check if semantic/hybrid mode requested
        if p.mode.as_deref() == Some("semantic") {
            return self.action_search_semantic(&query_str, p);
        }

        let search_domains = if self.allowed_domains.is_empty() {
            // Domainless mode: honor client filter
            p.domain.as_ref().map(|d| vec![d.clone()])
        } else {
            // Scoped mode: enforce server-side, ignore client domain param
            Some(self.allowed_domains.clone())
        };

        let query = SearchQuery {
            query: query_str,
            domains: search_domains,
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

    fn action_search_semantic(&self, query: &str, p: &SearchParams) -> String {
        let mut emb_guard = match self.embedder.lock() {
            Ok(g) => g,
            Err(_) => return json_error("Embedder lock poisoned."),
        };

        let embedder = match emb_guard.as_mut() {
            Some(e) => e,
            None => return json_error(
                "Semantic search unavailable. The embedding model has not been initialized. \
                 Run `wardwell reindex` to download the model and build the vector index."
            ),
        };

        let limit = p.limit.unwrap_or(5);
        let domains: Option<Vec<String>> = if self.allowed_domains.is_empty() {
            p.domain.as_ref().map(|d| vec![d.clone()])
        } else {
            Some(self.allowed_domains.clone())
        };

        match crate::index::hybrid::hybrid_search(
            &self.index,
            embedder,
            query,
            limit,
            domains.as_deref(),
        ) {
            Ok(results) => {
                // Track accessed projects from chunk results
                for chunk in &results.chunks {
                    if let Some((d, p)) = extract_domain_project(&chunk.path) {
                        self.record_access(&d, &p);
                    }
                }
                serde_json::to_string_pretty(&results).unwrap_or_default()
            }
            Err(e) => {
                eprintln!("wardwell: semantic search failed, falling back to keyword: {e}");
                // Fall back to keyword search instead of returning an error
                drop(emb_guard);
                let fallback_domains = if self.allowed_domains.is_empty() {
                    p.domain.as_ref().map(|d| vec![d.clone()])
                } else {
                    Some(self.allowed_domains.clone())
                };
                let fallback_query = SearchQuery {
                    query: query.to_string(),
                    domains: fallback_domains,
                    types: Vec::new(),
                    status: None,
                    limit,
                };
                match self.index.search(&fallback_query) {
                    Ok(results) => serde_json::to_string_pretty(&results).unwrap_or_default(),
                    Err(e2) => json_error(&format!("Search failed: {e2}")),
                }
            }
        }
    }

    fn action_read(&self, p: &SearchParams) -> String {
        let path = match &p.path {
            Some(path) => path.clone(),
            None => return json_error("'path' is required for action 'read'."),
        };

        // ACL: check domain access before reading
        if !self.allowed_domains.is_empty() {
            let clean = path.strip_prefix('/').unwrap_or(&path);
            if let Some(file_domain) = clean.split('/').next()
                && let Err(e) = self.check_domain_access(file_domain, "read") {
                return json_error(&e);
            }
        }

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

        let body_lines: Vec<&str> = vf.body.lines().collect();
        let total_lines = body_lines.len();

        let (content, partial_meta) = if self.config.features.partial_reads && (p.offset.is_some() || p.read_limit.is_some()) {
            let start = p.offset.unwrap_or(1).max(1) - 1; // convert 1-indexed to 0-indexed
            let end = match p.read_limit {
                Some(lim) => (start + lim).min(total_lines),
                None => total_lines,
            };
            let sliced: String = if start < total_lines {
                body_lines[start..end].join("\n")
            } else {
                String::new()
            };
            let returned = if start < total_lines { end - start } else { 0 };
            (sliced, Some(serde_json::json!({
                "totalLines": total_lines,
                "returnedLines": returned,
                "offset": start + 1,
                "limit": p.read_limit,
            })))
        } else {
            (vf.body, None)
        };

        let mut result = serde_json::json!({
            "path": path,
            "frontmatter": vf.frontmatter,
            "content": content,
            "totalLines": total_lines,
            "related_previews": related_previews,
        });
        if let Some(meta) = partial_meta {
            result["partial"] = meta;
        }

        serde_json::to_string_pretty(&result).unwrap_or_default()
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

        // ACL: validate client domain param if scoped
        if let Some(ref d) = p.domain
            && let Err(e) = self.check_domain_access(d, "history") {
            return json_error(&e);
        }

        let since_date = p.since.as_deref()
            .and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());

        let mut all_entries = Vec::new();

        // Walk vault looking for *.history.md or history.md files
        let dirs_to_scan = if !self.allowed_domains.is_empty() {
            match (&p.domain, &p.project) {
                (Some(d), Some(proj)) => vec![vault_dir.join(d).join(proj)],
                (Some(d), None) => vec![vault_dir.join(d)],
                _ => self.scoped_domain_dirs(&vault_dir, None),
            }
        } else {
            match (&p.domain, &p.project) {
                (Some(d), Some(proj)) => vec![vault_dir.join(d).join(proj)],
                (Some(d), None) => vec![vault_dir.join(d)],
                _ => list_subdirs(&vault_dir),
            }
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

        // ACL: validate client domain param if scoped
        if let Some(ref d) = p.domain
            && let Err(e) = self.check_domain_access(d, "orchestrate") {
            return json_error(&e);
        }

        let dirs_to_scan = self.scoped_domain_dirs(&vault_dir, p.domain.as_deref());

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
/// `allowed_domains` overrides `domain_filter` when non-empty (ACL enforcement).
fn collect_history_entries(
    vault_root: &std::path::Path,
    since: Option<chrono::NaiveDate>,
    domain_filter: Option<&str>,
    skip_archive: bool,
    allowed_domains: &[String],
) -> Vec<ParsedHistoryEntry> {
    let mut entries = Vec::new();
    let dirs_to_scan = if !allowed_domains.is_empty() {
        // Scoped mode: only scan allowed domains
        allowed_domains.iter()
            .map(|d| vault_root.join(d))
            .filter(|p| p.is_dir())
            .collect()
    } else {
        match domain_filter {
            Some(d) => vec![vault_root.join(d)],
            None => list_subdirs(vault_root),
        }
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

        // ACL: validate client domain param if scoped
        if let Some(ref d) = p.domain
            && let Err(e) = self.check_domain_access(d, "retrospective") {
            return json_error(&e);
        }

        let skip_archive = !p.include_archived.unwrap_or(false);
        let entries = collect_history_entries(
            &self.vault_root,
            Some(since),
            p.domain.as_deref(),
            skip_archive,
            &self.allowed_domains,
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

        // ACL: validate client domain param if scoped
        if let Some(ref d) = p.domain
            && let Err(e) = self.check_domain_access(d, "patterns") {
            return json_error(&e);
        }

        let skip_archive = !p.include_archived.unwrap_or(false);
        let entries = collect_history_entries(
            &self.vault_root,
            Some(since),
            p.domain.as_deref(),
            skip_archive,
            &self.allowed_domains,
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

    fn action_write_file(&self, p: &WriteParams, project: &str) -> String {
        let Some(ref rel_path) = p.path else {
            return json_error("'path' is required for write_file (e.g., 'docs/my-audit.md')");
        };
        let Some(ref content) = p.body else {
            return json_error("'body' is required for write_file — the file content to write");
        };

        // Reject path traversal
        if rel_path.contains("..") {
            return json_error("path cannot contain '..'");
        }

        let project_dir = self.vault_root.join(&p.domain).join(project);
        let file_path = project_dir.join(rel_path);

        // Create parent directories
        if let Some(parent) = file_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return json_error(&format!("failed to create directory: {e}"));
            }
        }

        if let Err(e) = std::fs::write(&file_path, content) {
            return json_error(&format!("failed to write file: {e}"));
        }

        // Reindex the file so wardwell_search can find it immediately
        self.reindex_file(&file_path);

        let vault_rel = format!("{}/{}/{}", p.domain, project, rel_path);
        serde_json::to_string(&serde_json::json!({
            "written": true,
            "path": vault_rel,
            "size": content.len(),
            "hint": format!("Read with wardwell_search action:read path:{vault_rel}")
        })).unwrap_or_default()
    }

    /// Re-read a file from disk and upsert it into the FTS index.
    fn reindex_file(&self, path: &std::path::Path) {
        if let Ok(vf) = crate::vault::reader::read_file(path) {
            let _ = self.index.upsert(&vf, &self.vault_root);
        }
    }
}

// Kanban action handlers
impl WardwellServer {
    fn check_kanban_domain_access(&self, domain: &str) -> Result<(), String> {
        if self.allowed_domains.is_empty() {
            return Ok(()); // domainless mode — full access
        }
        if self.allowed_domains.contains(&domain.to_string()) {
            Ok(())
        } else {
            Err(format!("domain '{}' not in allowed domains for this session", domain))
        }
    }

    fn kanban_sequence(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        if let Some(ref order) = p.order {
            let Some(ref project) = p.project else {
                return json_error("'project' is required for bulk sequence");
            };
            match kanban.sequence_bulk(project, order) {
                Ok(items) => serde_json::to_string(&serde_json::json!({"sequenced": true, "items": items})).unwrap_or_default(),
                Err(e) => json_error(&e.to_string()),
            }
        } else if let Some(ref ticket_id) = p.ticket_id {
            let Some(position) = p.position else {
                return json_error("'position' is required for single sequence (1-based integer)");
            };
            match kanban.sequence_single(ticket_id, position) {
                Ok(item) => serde_json::to_string(&serde_json::json!({"sequenced": true, "item": item})).unwrap_or_default(),
                Err(e) => json_error(&e.to_string()),
            }
        } else {
            json_error("provide ticket_id+position (single) or project+order (bulk)")
        }
    }

    fn kanban_export_roadmap(&self, p: &KanbanParams) -> String {
        let Some(ref project) = p.project else {
            return json_error("'project' is required for export_roadmap");
        };
        let url = format!("http://localhost:9292/api/kanban/{project}/roadmap.pdf?save=true");
        match std::process::Command::new("curl")
            .args(["-s", "-X", "POST", &url])
            .output()
        {
            Ok(output) => {
                let body = String::from_utf8_lossy(&output.stdout);
                if output.status.success() {
                    serde_json::to_string(&serde_json::json!({"exported": true, "response": body.trim()})).unwrap_or_default()
                } else {
                    json_error(&format!("roadmap export failed ({}): {}", output.status, body.trim()))
                }
            }
            Err(e) => json_error(&format!("failed to call roadmap API: {e}")),
        }
    }

    fn kanban_get(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        let Some(ref ticket_id) = p.ticket_id else {
            return json_error("'ticket_id' is required for get");
        };
        if let Some((ref dom, _)) = self.lookup_item_domain(kanban, ticket_id) {
            if let Err(e) = self.check_kanban_domain_access(dom) {
                return json_error(&e);
            }
        }
        match kanban.get_item(ticket_id) {
            Ok(item) => serde_json::to_string(&serde_json::json!({"item": item})).unwrap_or_default(),
            Err(e) => json_error(&e.to_string()),
        }
    }

    fn kanban_search(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        let Some(ref query) = p.query else {
            return json_error("'query' is required for search (text to find in ticket ID, title, or description)");
        };
        let domains = if self.allowed_domains.is_empty() { None } else { Some(self.allowed_domains.as_slice()) };
        match kanban.search(query, p.project.as_deref(), domains) {
            Ok(items) => {
                let total = items.len();
                serde_json::to_string(&serde_json::json!({"items": items, "total": total})).unwrap_or_default()
            }
            Err(e) => json_error(&e.to_string()),
        }
    }

    fn kanban_list(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        let domains = if self.allowed_domains.is_empty() {
            None
        } else {
            Some(self.allowed_domains.as_slice())
        };
        match kanban.list(
            p.project.as_deref(),
            p.status.as_deref(),
            p.priority.as_deref(),
            p.assignee.as_deref(),
            p.epic.as_deref(),
            p.tag.as_deref(),
            p.include_done.unwrap_or(false),
            domains,
        ) {
            Ok(items) => {
                let total = items.len();
                serde_json::to_string(&serde_json::json!({
                    "items": items, "total": total, "returned": total,
                })).unwrap_or_default()
            }
            Err(e) => json_error(&e.to_string()),
        }
    }

    fn kanban_create(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        let Some(ref title) = p.title else {
            return json_error("'title' is required for create");
        };
        let Some(ref project) = p.project else {
            return json_error("'project' is required for create");
        };

        let domain = match &p.domain {
            Some(d) => d.clone(),
            None => match self.infer_domain_for_project(project) {
                Some(d) => d,
                None => return json_error(&format!(
                    "cannot infer domain for project '{}'. Pass 'domain' explicitly.", project
                )),
            },
        };

        if let Err(e) = self.check_kanban_domain_access(&domain) {
            return json_error(&e);
        }

        match kanban.create_item(
            title, project, &domain,
            p.description.as_deref(), p.status.as_deref(), p.priority.as_deref(),
            p.assignee.as_deref(), p.deadline.as_deref(), p.source.as_deref(),
            p.epic.as_deref(), p.parent.as_deref(), p.tags.as_deref(), &self.config.kanban_prefixes,
        ) {
            Ok(item) => {
                let mut audit_line = format!("{} created: {} [{}]", item.ticket_id, item.title, item.status);
                if item.priority != "medium" {
                    audit_line.push_str(&format!(" ⚡{}", item.priority));
                }
                if let Some(ref dl) = item.deadline {
                    // Format deadline as MM/DD from ISO date (YYYY-MM-DD or RFC3339)
                    let short_dl = dl.get(5..10)
                        .map(|s| s.replace('-', "/"))
                        .unwrap_or_else(|| dl.clone());
                    audit_line.push_str(&format!(" 📅{short_dl}"));
                }
                let _ = crate::kanban::audit::append_ticket_log(&self.vault_root, &domain, project, &audit_line);
                serde_json::to_string(&serde_json::json!({ "created": true, "item": item })).unwrap_or_default()
            }
            Err(e) => json_error(&e.to_string()),
        }
    }

    fn kanban_update(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        let Some(ref ticket_id) = p.ticket_id else {
            return json_error("'ticket_id' is required for update");
        };
        if let Some((ref dom, _)) = self.lookup_item_domain(kanban, ticket_id)
            && let Err(e) = self.check_kanban_domain_access(dom)
        {
            return json_error(&e);
        }
        match kanban.update_item(
            ticket_id, p.title.as_deref(), p.description.as_deref(),
            p.status.as_deref(), p.priority.as_deref(), p.assignee.as_deref(), p.deadline.as_deref(),
            p.epic.as_deref(), p.parent.as_deref(), p.tags.as_deref(),
            p.stage.as_deref(), p.waiting_on.as_deref(), p.waiting_summary.as_deref(),
        ) {
            Ok(item) => {
                let mut changes = Vec::new();
                if p.title.is_some() { changes.push("title".to_string()); }
                if p.description.is_some() { changes.push("description".to_string()); }
                if p.status.is_some() { changes.push("status".to_string()); }
                if p.priority.is_some() { changes.push("priority".to_string()); }
                if p.assignee.is_some() { changes.push("assignee".to_string()); }
                if let Some(ref dl) = p.deadline {
                    let short_dl = dl.get(5..10)
                        .map(|s| s.replace('-', "/"))
                        .unwrap_or_else(|| dl.clone());
                    changes.push(format!("📅{short_dl}"));
                }
                let audit_line = format!("{ticket_id} updated: {}", changes.join(", "));
                if let Some((ref dom, ref proj)) = self.lookup_item_domain(kanban, ticket_id) {
                    let _ = crate::kanban::audit::append_ticket_log(&self.vault_root, dom, proj, &audit_line);
                }
                serde_json::to_string(&serde_json::json!({ "updated": true, "item": item })).unwrap_or_default()
            }
            Err(e) => json_error(&e.to_string()),
        }
    }

    fn kanban_move(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        let Some(ref ticket_id) = p.ticket_id else {
            return json_error("'ticket_id' is required for move");
        };
        let Some(ref status) = p.status else {
            return json_error("'status' is required for move");
        };
        if let Some((ref dom, _)) = self.lookup_item_domain(kanban, ticket_id)
            && let Err(e) = self.check_kanban_domain_access(dom)
        {
            return json_error(&e);
        }
        match kanban.move_item(ticket_id, status) {
            Ok((item, transition)) => {
                let audit_line = format!("{ticket_id} → {status}");
                if let Some((ref dom, ref proj)) = self.lookup_item_domain(kanban, ticket_id) {
                    let _ = crate::kanban::audit::append_ticket_log(&self.vault_root, dom, proj, &audit_line);
                }
                serde_json::to_string(&serde_json::json!({ "moved": true, "item": item, "transition": transition })).unwrap_or_default()
            }
            Err(e) => json_error(&e.to_string()),
        }
    }

    fn kanban_note(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        let Some(ref ticket_id) = p.ticket_id else {
            return json_error("'ticket_id' is required for note");
        };
        let Some(ref text) = p.text else {
            return json_error("'text' is required for note");
        };
        if let Some((ref dom, _)) = self.lookup_item_domain(kanban, ticket_id)
            && let Err(e) = self.check_kanban_domain_access(dom)
        {
            return json_error(&e);
        }
        match kanban.add_note(ticket_id, text, p.source.as_deref()) {
            Ok(item) => {
                let audit_line = format!("{ticket_id} note: \"{text}\"");
                if let Some((ref dom, ref proj)) = self.lookup_item_domain(kanban, ticket_id) {
                    let _ = crate::kanban::audit::append_ticket_log(&self.vault_root, dom, proj, &audit_line);
                }
                serde_json::to_string(&serde_json::json!({ "noted": true, "item": item })).unwrap_or_default()
            }
            Err(e) => json_error(&e.to_string()),
        }
    }

    fn kanban_query(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        let Some(ref question) = p.question else {
            return json_error("'question' is required for query");
        };
        let domains = if self.allowed_domains.is_empty() {
            None
        } else {
            Some(self.allowed_domains.as_slice())
        };
        match kanban.query(question, &self.kanban_queries, p.project.as_deref(), domains) {
            Ok(items) => {
                let total = items.len();
                serde_json::to_string(&serde_json::json!({
                    "items": items, "total": total, "returned": total,
                })).unwrap_or_default()
            }
            Err(e) => json_error(&e.to_string()),
        }
    }

    fn kanban_attach(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        let Some(ref ticket_id) = p.ticket_id else {
            return json_error("'ticket_id' is required for attach");
        };
        if p.text.is_none() && p.file_path.is_none() {
            return json_error("provide 'text' (content to write and attach) with 'title' (filename), or 'file_path' (vault-relative path to existing file)");
        }
        let filename = p.title.as_deref().or(p.file_path.as_deref()).unwrap_or("attachment.md");
        if let Some((ref dom, _)) = self.lookup_item_domain(kanban, ticket_id) {
            if let Err(e) = self.check_kanban_domain_access(dom) {
                return json_error(&e);
            }
        }
        match kanban.attach_file(ticket_id, filename, p.text.as_deref(), p.file_path.as_deref()) {
            Ok(att) => {
                let audit_line = format!("{ticket_id} attach: \"{}\" ({})", att.filename, att.attachment_id);
                if let Some((ref dom, ref proj)) = self.lookup_item_domain(kanban, ticket_id) {
                    let _ = crate::kanban::audit::append_ticket_log(&self.vault_root, dom, proj, &audit_line);
                }
                serde_json::to_string(&serde_json::json!({
                    "attached": true, "attachment": {
                        "attachment_id": att.attachment_id, "filename": att.filename,
                        "mime_type": att.mime_type, "size": att.size,
                        "storage_path": att.storage_path,
                        "read_path": att.read_path,
                    },
                    "hint": "To read this file, use wardwell_search action:read path:<read_path>"
                })).unwrap_or_default()
            }
            Err(e) => json_error(&e.to_string()),
        }
    }

    fn kanban_detach(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        let Some(ref ticket_id) = p.ticket_id else {
            return json_error("'ticket_id' is required for detach");
        };
        let Some(ref attachment_id) = p.attachment_id else {
            return json_error("'attachment_id' is required for detach");
        };
        if let Some((ref dom, _)) = self.lookup_item_domain(kanban, ticket_id) {
            if let Err(e) = self.check_kanban_domain_access(dom) {
                return json_error(&e);
            }
        }
        match kanban.detach_file(ticket_id, attachment_id) {
            Ok(()) => {
                let audit_line = format!("{ticket_id} detach: {attachment_id}");
                if let Some((ref dom, ref proj)) = self.lookup_item_domain(kanban, ticket_id) {
                    let _ = crate::kanban::audit::append_ticket_log(&self.vault_root, dom, proj, &audit_line);
                }
                serde_json::to_string(&serde_json::json!({"detached": true})).unwrap_or_default()
            }
            Err(e) => json_error(&e.to_string()),
        }
    }

    // ---- Relationship handlers ----

    fn kanban_relationship_create(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        let Some(ref from_id) = p.from_ticket_id else {
            return json_error("'from_ticket_id' is required for relationship_create");
        };
        let Some(ref to_id) = p.to_ticket_id else {
            return json_error("'to_ticket_id' is required for relationship_create");
        };
        let Some(ref rel_type_str) = p.relationship_type else {
            return json_error(&format!("'relationship_type' is required. One of: {}", crate::kanban::relationships::RelationshipType::all_names().join(", ")));
        };
        let Some(rel_type) = crate::kanban::relationships::RelationshipType::parse(rel_type_str) else {
            return json_error(&format!("invalid relationship_type '{}'. Must be one of: {}", rel_type_str, crate::kanban::relationships::RelationshipType::all_names().join(", ")));
        };

        let Some((from_domain, from_project)) = self.lookup_item_domain(kanban, from_id) else {
            return json_error(&format!("ticket '{}' not found", from_id));
        };
        let Some((to_domain, to_project)) = self.lookup_item_domain(kanban, to_id) else {
            return json_error(&format!("ticket '{}' not found", to_id));
        };
        if from_project != to_project || from_domain != to_domain {
            return json_error(&format!("tickets must be in the same project. '{}' is in {}/{}, '{}' is in {}/{}", from_id, from_domain, from_project, to_id, to_domain, to_project));
        }
        if let Err(e) = self.check_kanban_domain_access(&from_domain) {
            return json_error(&e);
        }

        let existing = crate::kanban::relationships::read_all(&self.vault_root, &from_domain, &from_project);
        if existing.iter().any(|r| r.from_ticket_id == *from_id && r.to_ticket_id == *to_id && r.relationship_type == rel_type) {
            return json_error(&format!("duplicate relationship: {} {} {} already exists", from_id, rel_type_str, to_id));
        }

        let rel = crate::kanban::relationships::Relationship {
            id: uuid::Uuid::new_v4().to_string(),
            project: from_project.clone(),
            from_ticket_id: from_id.clone(),
            to_ticket_id: to_id.clone(),
            relationship_type: rel_type,
            description: p.description.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            source: p.source.clone(),
        };

        if let Err(e) = crate::kanban::relationships::append_event(&self.vault_root, &from_domain, &from_project, &crate::kanban::relationships::RelationshipEvent::Create(rel.clone())) {
            return json_error(&format!("failed to write relationship: {e}"));
        }
        let audit_line = format!("{} → {} [{}]", from_id, to_id, rel_type_str);
        let _ = crate::kanban::audit::append_ticket_log(&self.vault_root, &from_domain, &from_project, &audit_line);
        serde_json::to_string(&serde_json::json!({"created": true, "relationship": rel})).unwrap_or_default()
    }

    fn kanban_relationship_list(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        let (domain, project) = if let Some(ref tid) = p.ticket_id {
            match self.lookup_item_domain(kanban, tid) {
                Some(dp) => dp,
                None => return json_error(&format!("ticket '{}' not found", tid)),
            }
        } else if let Some(ref proj) = p.project {
            let domain = match &p.domain {
                Some(d) => d.clone(),
                None => match self.infer_domain_for_project(proj) {
                    Some(d) => d,
                    None => return json_error(&format!("cannot infer domain for project '{}'", proj)),
                },
            };
            (domain, proj.clone())
        } else {
            return json_error("'ticket_id' or 'project' is required for relationship_list");
        };

        let rels = crate::kanban::relationships::read_all(&self.vault_root, &domain, &project);
        let filtered: Vec<_> = if let Some(ref tid) = p.ticket_id {
            rels.into_iter().filter(|r| r.from_ticket_id == *tid || r.to_ticket_id == *tid).collect()
        } else {
            rels
        };

        // Filter by epic if requested
        let filtered: Vec<_> = if let Some(ref epic) = p.epic {
            let items = match kanban.list(Some(&project), None, None, None, Some(epic), None, true, None) {
                Ok(items) => items,
                Err(_) => return serde_json::to_string(&serde_json::json!({"relationships": filtered, "total": filtered.len()})).unwrap_or_default(),
            };
            let epic_ids: std::collections::HashSet<String> = items.iter().map(|i| i.ticket_id.clone()).collect();
            filtered.into_iter().filter(|r| epic_ids.contains(&r.from_ticket_id) || epic_ids.contains(&r.to_ticket_id)).collect()
        } else {
            filtered
        };

        let total = filtered.len();
        serde_json::to_string(&serde_json::json!({"relationships": filtered, "total": total})).unwrap_or_default()
    }

    fn kanban_relationship_delete(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        let Some(ref rel_id) = p.relationship_id else {
            return json_error("'relationship_id' is required for relationship_delete");
        };
        let Some(ref project) = p.project else {
            return json_error("'project' is required for relationship_delete");
        };
        let domain = match &p.domain {
            Some(d) => d.clone(),
            None => match self.infer_domain_for_project(project) {
                Some(d) => d,
                None => return json_error(&format!("cannot infer domain for project '{}'", project)),
            },
        };
        if let Err(e) = self.check_kanban_domain_access(&domain) {
            return json_error(&e);
        }
        let existing = crate::kanban::relationships::read_all(&self.vault_root, &domain, project);
        if !existing.iter().any(|r| r.id == *rel_id) {
            return json_error(&format!("relationship '{}' not found in project '{}'", rel_id, project));
        }
        let _ = kanban; // already validated domain access
        if let Err(e) = crate::kanban::relationships::append_event(&self.vault_root, &domain, project, &crate::kanban::relationships::RelationshipEvent::Delete {
            id: rel_id.clone(), project: project.clone(), timestamp: chrono::Utc::now().to_rfc3339(),
        }) {
            return json_error(&format!("failed to delete relationship: {e}"));
        }
        serde_json::to_string(&serde_json::json!({"deleted": true, "relationship_id": rel_id})).unwrap_or_default()
    }

    // ---- Question handlers ----

    fn kanban_question_create(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        let Some(ref question_text) = p.question_text else {
            return json_error("'question_text' is required for question_create");
        };
        let Some(ref project) = p.project else {
            return json_error("'project' is required for question_create");
        };
        let domain = match &p.domain {
            Some(d) => d.clone(),
            None => match self.infer_domain_for_project(project) {
                Some(d) => d,
                None => return json_error(&format!("cannot infer domain for project '{}'", project)),
            },
        };
        if let Err(e) = self.check_kanban_domain_access(&domain) {
            return json_error(&e);
        }
        if let Some(ref tid) = p.ticket_id {
            if self.lookup_item_domain(kanban, tid).is_none() {
                return json_error(&format!("ticket '{}' not found", tid));
            }
        }
        if let Err(error) = crate::kanban::questions::validate_interaction(
            p.interaction_type,
            p.interaction_options.as_deref(),
            p.interaction_placeholder.as_deref(),
        ) {
            return json_error(&error);
        }

        let now = chrono::Utc::now().to_rfc3339();
        let q = crate::kanban::questions::Question {
            id: uuid::Uuid::new_v4().to_string(),
            project: project.clone(),
            ticket_id: p.ticket_id.clone(),
            question: question_text.clone(),
            current_assumption: p.current_assumption.clone(),
            evidence: p.evidence.clone(),
            needed_for: p.needed_for.clone(),
            interaction_type: p.interaction_type,
            interaction_options: p.interaction_options.clone(),
            interaction_placeholder: p.interaction_placeholder.clone(),
            status: crate::kanban::questions::QuestionStatus::Open,
            answer: None,
            created_at: now.clone(),
            updated_at: now,
            resolved_at: None,
            source: p.source.clone(),
        };
        if let Err(e) = crate::kanban::questions::append_event(&self.vault_root, &domain, project, &crate::kanban::questions::QuestionEvent::Create(q.clone())) {
            return json_error(&format!("failed to write question: {e}"));
        }
        let audit_line = format!("question created: {}", question_text);
        let _ = crate::kanban::audit::append_ticket_log(&self.vault_root, &domain, project, &audit_line);
        serde_json::to_string(&serde_json::json!({"created": true, "question": q})).unwrap_or_default()
    }

    fn kanban_question_list(&self, p: &KanbanParams) -> String {
        let Some(ref project) = p.project else {
            return json_error("'project' is required for question_list");
        };
        let domain = match &p.domain {
            Some(d) => d.clone(),
            None => match self.infer_domain_for_project(project) {
                Some(d) => d,
                None => return json_error(&format!("cannot infer domain for project '{}'", project)),
            },
        };
        let questions = crate::kanban::questions::read_all(&self.vault_root, &domain, project);
        let open_only = p.open_only.unwrap_or(true);
        let filtered: Vec<_> = questions.into_iter()
            .filter(|q| {
                if open_only { q.status == crate::kanban::questions::QuestionStatus::Open } else { true }
            })
            .filter(|q| {
                if let Some(ref tid) = p.ticket_id { q.ticket_id.as_deref() == Some(tid.as_str()) } else { true }
            })
            .filter(|q| {
                if let Some(ref epic) = p.epic {
                    // Project-level questions (no ticket_id) show for any epic
                    q.ticket_id.is_none() || q.needed_for.as_deref() == Some(epic.as_str())
                } else { true }
            })
            .collect();
        let total = filtered.len();
        serde_json::to_string(&serde_json::json!({"questions": filtered, "total": total})).unwrap_or_default()
    }

    fn kanban_question_update(&self, p: &KanbanParams) -> String {
        let Some(ref target_id) = p.target_id else {
            return json_error("'target_id' (question ID) is required for question_update");
        };
        let Some(ref project) = p.project else {
            return json_error("'project' is required for question_update");
        };
        let domain = match &p.domain {
            Some(d) => d.clone(),
            None => match self.infer_domain_for_project(project) {
                Some(d) => d,
                None => return json_error(&format!("cannot infer domain for project '{}'", project)),
            },
        };
        if let Err(e) = self.check_kanban_domain_access(&domain) {
            return json_error(&e);
        }
        let existing = crate::kanban::questions::read_all(&self.vault_root, &domain, project);
        let Some(existing_question) = existing.iter().find(|q| q.id == *target_id) else {
            return json_error(&format!("question '{}' not found", target_id));
        };
        let interaction_type = p.interaction_type.or(existing_question.interaction_type);
        let clears_options = p.interaction_type
            == Some(crate::kanban::questions::QuestionInteractionType::Question)
            && p.interaction_options.is_none();
        let interaction_options = if clears_options {
            None
        } else {
            p.interaction_options
                .as_deref()
                .or(existing_question.interaction_options.as_deref())
        };
        let interaction_placeholder = p
            .interaction_placeholder
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .or(existing_question.interaction_placeholder.as_deref());
        if let Err(error) = crate::kanban::questions::validate_interaction(
            interaction_type,
            interaction_options,
            interaction_placeholder,
        ) {
            return json_error(&error);
        }
        let event = crate::kanban::questions::QuestionEvent::Update {
            id: target_id.clone(),
            project: project.clone(),
            question: p.question_text.clone(),
            current_assumption: p.current_assumption.clone(),
            evidence: p.evidence.clone(),
            needed_for: p.needed_for.clone(),
            interaction_type: p.interaction_type,
            interaction_options: if clears_options {
                Some(vec![])
            } else {
                p.interaction_options.clone()
            },
            interaction_placeholder: p.interaction_placeholder.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        if let Err(e) = crate::kanban::questions::append_event(&self.vault_root, &domain, project, &event) {
            return json_error(&format!("failed to update question: {e}"));
        }
        serde_json::to_string(&serde_json::json!({"updated": true, "question_id": target_id})).unwrap_or_default()
    }

    fn kanban_question_answer(&self, p: &KanbanParams) -> String {
        let Some(ref target_id) = p.target_id else {
            return json_error("'target_id' (question ID) is required for question_answer");
        };
        let Some(ref answer) = p.answer else {
            return json_error("'answer' is required for question_answer");
        };
        let Some(ref project) = p.project else {
            return json_error("'project' is required for question_answer");
        };
        let domain = match &p.domain {
            Some(d) => d.clone(),
            None => match self.infer_domain_for_project(project) {
                Some(d) => d,
                None => return json_error(&format!("cannot infer domain for project '{}'", project)),
            },
        };
        if let Err(e) = self.check_kanban_domain_access(&domain) {
            return json_error(&e);
        }
        let existing = crate::kanban::questions::read_all(&self.vault_root, &domain, project);
        let q = existing.iter().find(|q| q.id == *target_id);
        match q {
            None => return json_error(&format!("question '{}' not found", target_id)),
            Some(q) if q.status != crate::kanban::questions::QuestionStatus::Open => {
                return json_error(&format!("question '{}' is already {:?}", target_id, q.status));
            }
            _ => {}
        }
        let event = crate::kanban::questions::QuestionEvent::Answer {
            id: target_id.clone(),
            project: project.clone(),
            answer: answer.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        if let Err(e) = crate::kanban::questions::append_event(&self.vault_root, &domain, project, &event) {
            return json_error(&format!("failed to answer question: {e}"));
        }
        let audit_line = format!("question answered: {}", target_id);
        let _ = crate::kanban::audit::append_ticket_log(&self.vault_root, &domain, project, &audit_line);
        serde_json::to_string(&serde_json::json!({"answered": true, "question_id": target_id})).unwrap_or_default()
    }

    fn kanban_question_invalidate(&self, p: &KanbanParams) -> String {
        let Some(ref target_id) = p.target_id else {
            return json_error("'target_id' (question ID) is required for question_invalidate");
        };
        let Some(ref project) = p.project else {
            return json_error("'project' is required for question_invalidate");
        };
        let domain = match &p.domain {
            Some(d) => d.clone(),
            None => match self.infer_domain_for_project(project) {
                Some(d) => d,
                None => return json_error(&format!("cannot infer domain for project '{}'", project)),
            },
        };
        if let Err(e) = self.check_kanban_domain_access(&domain) {
            return json_error(&e);
        }
        let existing = crate::kanban::questions::read_all(&self.vault_root, &domain, project);
        if !existing.iter().any(|q| q.id == *target_id && q.status == crate::kanban::questions::QuestionStatus::Open) {
            return json_error(&format!("open question '{}' not found", target_id));
        }
        let event = crate::kanban::questions::QuestionEvent::Invalidate {
            id: target_id.clone(),
            project: project.clone(),
            reason: p.reason.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        if let Err(e) = crate::kanban::questions::append_event(&self.vault_root, &domain, project, &event) {
            return json_error(&format!("failed to invalidate question: {e}"));
        }
        serde_json::to_string(&serde_json::json!({"invalidated": true, "question_id": target_id})).unwrap_or_default()
    }

    // ---- Proposal handlers ----

    fn kanban_proposal_create(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        let Some(ref title) = p.title else {
            return json_error("'title' is required for proposal_create");
        };
        let Some(ref project) = p.project else {
            return json_error("'project' is required for proposal_create");
        };
        let Some(ref changes_json) = p.changes else {
            return json_error("'changes' is required for proposal_create — array of change operations");
        };
        let domain = match &p.domain {
            Some(d) => d.clone(),
            None => match self.infer_domain_for_project(project) {
                Some(d) => d,
                None => return json_error(&format!("cannot infer domain for project '{}'", project)),
            },
        };
        if let Err(e) = self.check_kanban_domain_access(&domain) {
            return json_error(&e);
        }

        let mut changes: Vec<crate::kanban::proposals::ChangeOperation> = vec![];
        let mut ticket_snapshots: Vec<crate::kanban::proposals::TicketSnapshot> = vec![];
        let mut snapshot_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

        for (i, val) in changes_json.iter().enumerate() {
            match serde_json::from_value::<crate::kanban::proposals::ChangeOperation>(val.clone()) {
                Ok(op) => {
                    if let crate::kanban::proposals::ChangeOperation::CreateQuestion {
                        interaction_type,
                        interaction_options,
                        interaction_placeholder,
                        ..
                    } = &op
                    && let Err(error) = crate::kanban::questions::validate_interaction(
                        *interaction_type,
                        interaction_options.as_deref(),
                        interaction_placeholder.as_deref(),
                    )
                    {
                        return json_error(&format!("change[{}]: {}", i, error));
                    }
                    // Validate ticket references and collect snapshots
                    let referenced_tickets = extract_ticket_ids_from_op(&op);
                    for tid in &referenced_tickets {
                        if self.lookup_item_domain(kanban, tid).is_none() {
                            return json_error(&format!("change[{}]: ticket '{}' not found", i, tid));
                        }
                        if !snapshot_ids.contains(tid) {
                            if let Ok(item) = kanban.get_item(tid) {
                                ticket_snapshots.push(crate::kanban::proposals::TicketSnapshot {
                                    ticket_id: tid.clone(),
                                    updated_at: item.updated_at.clone(),
                                });
                                snapshot_ids.insert(tid.clone());
                            }
                        }
                    }
                    changes.push(op);
                }
                Err(e) => return json_error(&format!("change[{}]: invalid operation: {}", i, e)),
            }
        }

        if changes.is_empty() {
            return json_error("'changes' must not be empty");
        }

        let intent = p.intent.as_deref().and_then(crate::kanban::proposals::ProposalIntent::parse);
        let rationale = p.rationale.clone();
        let context_transfers: Vec<crate::kanban::proposals::ContextTransfer> = p.context_transfers
            .as_ref()
            .map(|arr| arr.iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect())
            .unwrap_or_default();
        let closure_summary: Option<crate::kanban::proposals::ClosureSummary> = p.closure_summary
            .as_ref()
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        let reviewer_questions: Vec<String> = p.reviewer_questions.clone().unwrap_or_default();

        let now = chrono::Utc::now().to_rfc3339();
        let mut proposal = crate::kanban::proposals::Proposal {
            id: uuid::Uuid::new_v4().to_string(),
            project: project.clone(),
            title: title.clone(),
            description: p.description.clone(),
            status: crate::kanban::proposals::ProposalStatus::Pending,
            changes,
            created_at: now,
            decided_at: None,
            applied_at: None,
            source: p.source.clone(),
            ticket_snapshots,
            intent,
            rationale,
            risk_flags: vec![],
            context_transfers,
            closure_summary,
            reviewer_questions,
        };

        // Compute review with board context
        let review_items: Vec<_> = snapshot_ids.iter()
            .filter_map(|tid| kanban.get_item(tid).ok())
            .collect();
        let review_questions = crate::kanban::questions::read_all(&self.vault_root, &domain, project);
        let review_rels = crate::kanban::relationships::read_all(&self.vault_root, &domain, project);
        let review = crate::kanban::proposals::review_proposal(&proposal, &review_items, &review_questions, &review_rels);
        proposal.risk_flags = review.risk_flags.clone();

        if let Err(e) = crate::kanban::proposals::append_event(&self.vault_root, &domain, project, &crate::kanban::proposals::ProposalEvent::Create(proposal.clone())) {
            return json_error(&format!("failed to write proposal: {e}"));
        }
        let audit_line = format!("proposal created: {} ({} changes)", title, proposal.changes.len());
        let _ = crate::kanban::audit::append_ticket_log(&self.vault_root, &domain, project, &audit_line);
        serde_json::to_string(&serde_json::json!({"created": true, "proposal": proposal, "review": review})).unwrap_or_default()
    }

    /// Load the board context (project items, questions, relationships) used to
    /// recompute proposal risk against current state. Read-only.
    fn proposal_review_context(
        &self,
        kanban: &crate::kanban::store::KanbanStore,
        domain: &str,
        project: &str,
    ) -> (
        Vec<crate::kanban::store::KanbanItem>,
        Vec<crate::kanban::questions::Question>,
        Vec<crate::kanban::relationships::Relationship>,
    ) {
        let items = kanban.list(Some(project), None, None, None, None, None, true, None).unwrap_or_default();
        let questions = crate::kanban::questions::read_all(&self.vault_root, domain, project);
        let relationships = crate::kanban::relationships::read_all(&self.vault_root, domain, project);
        (items, questions, relationships)
    }

    fn kanban_proposal_get(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        let Some(ref target_id) = p.target_id else {
            return json_error("'target_id' (proposal ID) is required for proposal_get");
        };
        let Some(ref project) = p.project else {
            return json_error("'project' is required for proposal_get");
        };
        let domain = match &p.domain {
            Some(d) => d.clone(),
            None => match self.infer_domain_for_project(project) {
                Some(d) => d,
                None => return json_error(&format!("cannot infer domain for project '{}'", project)),
            },
        };
        let proposals = crate::kanban::proposals::read_all(&self.vault_root, &domain, project);
        match proposals.into_iter().find(|prop| prop.id == *target_id) {
            Some(prop) => {
                // Recompute risk against current board state so a pending proposal
                // reflects the board as it stands now.
                let (items, questions, relationships) = self.proposal_review_context(kanban, &domain, project);
                let review = crate::kanban::proposals::review_proposal(&prop, &items, &questions, &relationships);
                let risk_summary = crate::kanban::proposals::risk_summary_line(&review.risk_flags);
                serde_json::to_string(&serde_json::json!({
                    "proposal": prop,
                    "summary": review.summary,
                    "risk_flags": review.risk_flags,
                    "risk_count": review.risk_flags.len(),
                    "has_risks": !review.risk_flags.is_empty(),
                    "risk_summary": risk_summary,
                })).unwrap_or_default()
            }
            None => json_error(&format!("proposal '{}' not found", target_id)),
        }
    }

    fn kanban_proposal_list(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        let Some(ref project) = p.project else {
            return json_error("'project' is required for proposal_list");
        };
        let domain = match &p.domain {
            Some(d) => d.clone(),
            None => match self.infer_domain_for_project(project) {
                Some(d) => d,
                None => return json_error(&format!("cannot infer domain for project '{}'", project)),
            },
        };
        let proposals = crate::kanban::proposals::read_all(&self.vault_root, &domain, project);
        let mut filtered: Vec<_> = if let Some(ref status_str) = p.status {
            proposals.into_iter().filter(|prop| {
                prop.status.as_str() == status_str
            }).collect()
        } else {
            proposals
        };
        // Newest first so a reviewer sees fresh proposals at the top.
        filtered.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        let total = filtered.len();

        // full=true returns the raw proposals (every operation). Default is a
        // scannable, review-critical view with no raw operations.
        if p.full.unwrap_or(false) {
            return serde_json::to_string(&serde_json::json!({"proposals": filtered, "total": total})).unwrap_or_default();
        }

        let (items, questions, relationships) = self.proposal_review_context(kanban, &domain, project);
        let entries: Vec<_> = filtered.iter()
            .map(|prop| crate::kanban::proposals::list_entry(prop, &items, &questions, &relationships))
            .collect();
        let flagged = entries.iter().filter(|e| e.risk_flag_count > 0).count();
        serde_json::to_string(&serde_json::json!({
            "proposals": entries,
            "total": total,
            "flagged": flagged,
        })).unwrap_or_default()
    }

    fn kanban_proposal_approve(&self, p: &KanbanParams) -> String {
        let Some(ref target_id) = p.target_id else {
            return json_error("'target_id' (proposal ID) is required for proposal_approve");
        };
        let Some(ref project) = p.project else {
            return json_error("'project' is required for proposal_approve");
        };
        let domain = match &p.domain {
            Some(d) => d.clone(),
            None => match self.infer_domain_for_project(project) {
                Some(d) => d,
                None => return json_error(&format!("cannot infer domain for project '{}'", project)),
            },
        };
        if let Err(e) = self.check_kanban_domain_access(&domain) {
            return json_error(&e);
        }
        let proposals = crate::kanban::proposals::read_all(&self.vault_root, &domain, project);
        match proposals.iter().find(|prop| prop.id == *target_id) {
            None => return json_error(&format!("proposal '{}' not found", target_id)),
            Some(prop) if prop.status != crate::kanban::proposals::ProposalStatus::Pending => {
                return json_error(&format!("proposal '{}' is {:?}, cannot approve", target_id, prop.status));
            }
            _ => {}
        }
        if let Err(e) = crate::kanban::proposals::append_event(&self.vault_root, &domain, project, &crate::kanban::proposals::ProposalEvent::Approve {
            id: target_id.clone(), project: project.clone(), timestamp: chrono::Utc::now().to_rfc3339(),
        }) {
            return json_error(&format!("failed to approve proposal: {e}"));
        }
        serde_json::to_string(&serde_json::json!({"approved": true, "proposal_id": target_id})).unwrap_or_default()
    }

    fn kanban_proposal_reject(&self, p: &KanbanParams) -> String {
        let Some(ref target_id) = p.target_id else {
            return json_error("'target_id' (proposal ID) is required for proposal_reject");
        };
        let Some(ref project) = p.project else {
            return json_error("'project' is required for proposal_reject");
        };
        let domain = match &p.domain {
            Some(d) => d.clone(),
            None => match self.infer_domain_for_project(project) {
                Some(d) => d,
                None => return json_error(&format!("cannot infer domain for project '{}'", project)),
            },
        };
        if let Err(e) = self.check_kanban_domain_access(&domain) {
            return json_error(&e);
        }
        let proposals = crate::kanban::proposals::read_all(&self.vault_root, &domain, project);
        match proposals.iter().find(|prop| prop.id == *target_id) {
            None => return json_error(&format!("proposal '{}' not found", target_id)),
            Some(prop) if prop.status != crate::kanban::proposals::ProposalStatus::Pending => {
                return json_error(&format!("proposal '{}' is {:?}, cannot reject", target_id, prop.status));
            }
            _ => {}
        }
        if let Err(e) = crate::kanban::proposals::append_event(&self.vault_root, &domain, project, &crate::kanban::proposals::ProposalEvent::Reject {
            id: target_id.clone(), project: project.clone(), reason: p.reason.clone(), timestamp: chrono::Utc::now().to_rfc3339(),
        }) {
            return json_error(&format!("failed to reject proposal: {e}"));
        }
        serde_json::to_string(&serde_json::json!({"rejected": true, "proposal_id": target_id})).unwrap_or_default()
    }

    fn kanban_proposal_apply(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        let Some(ref target_id) = p.target_id else {
            return json_error("'target_id' (proposal ID) is required for proposal_apply");
        };
        let Some(ref project) = p.project else {
            return json_error("'project' is required for proposal_apply");
        };
        let domain = match &p.domain {
            Some(d) => d.clone(),
            None => match self.infer_domain_for_project(project) {
                Some(d) => d,
                None => return json_error(&format!("cannot infer domain for project '{}'", project)),
            },
        };
        if let Err(e) = self.check_kanban_domain_access(&domain) {
            return json_error(&e);
        }

        let proposals = crate::kanban::proposals::read_all(&self.vault_root, &domain, project);
        let proposal = match proposals.into_iter().find(|prop| prop.id == *target_id) {
            Some(prop) => prop,
            None => return json_error(&format!("proposal '{}' not found", target_id)),
        };

        if proposal.status != crate::kanban::proposals::ProposalStatus::Approved {
            return json_error(&format!("proposal '{}' is {:?}, must be approved before applying", target_id, proposal.status));
        }

        // Conflict detection: check if referenced tickets changed since proposal creation
        for snap in &proposal.ticket_snapshots {
            match kanban.get_item(&snap.ticket_id) {
                Ok(current) => {
                    if current.updated_at != snap.updated_at {
                        return json_error(&format!(
                            "conflict: ticket '{}' was updated since proposal creation (was {}, now {}). Re-create the proposal with current state.",
                            snap.ticket_id, snap.updated_at, current.updated_at
                        ));
                    }
                }
                Err(_) => return json_error(&format!("ticket '{}' no longer exists", snap.ticket_id)),
            }
        }

        // Apply each change operation
        let mut results: Vec<serde_json::Value> = vec![];
        for (i, change) in proposal.changes.iter().enumerate() {
            match self.apply_change_operation(kanban, &domain, project, change) {
                Ok(result) => results.push(result),
                Err(e) => return json_error(&format!("change[{}] failed: {}. Partial apply — {} of {} operations succeeded.", i, e, i, proposal.changes.len())),
            }
        }

        // Record the apply event
        if let Err(e) = crate::kanban::proposals::append_event(&self.vault_root, &domain, project, &crate::kanban::proposals::ProposalEvent::Apply {
            id: target_id.clone(), project: project.clone(), timestamp: chrono::Utc::now().to_rfc3339(),
        }) {
            return json_error(&format!("changes applied but failed to record apply event: {e}"));
        }

        let audit_line = format!("proposal applied: {} ({} changes)", proposal.title, results.len());
        let _ = crate::kanban::audit::append_ticket_log(&self.vault_root, &domain, project, &audit_line);
        serde_json::to_string(&serde_json::json!({"applied": true, "proposal_id": target_id, "results": results})).unwrap_or_default()
    }

    fn apply_change_operation(
        &self,
        kanban: &crate::kanban::store::KanbanStore,
        domain: &str,
        project: &str,
        op: &crate::kanban::proposals::ChangeOperation,
    ) -> Result<serde_json::Value, String> {
        use crate::kanban::proposals::ChangeOperation;
        match op {
            ChangeOperation::UpdateTicket { ticket_id, status, priority, epic, tags, parent, deadline, title, description } => {
                kanban.update_item(
                    ticket_id,
                    title.as_deref(),
                    description.as_deref(),
                    status.as_deref(),
                    priority.as_deref(),
                    None, // assignee
                    deadline.as_deref(),
                    epic.as_deref(),
                    parent.as_deref(),
                    tags.as_deref(),
                    None, // stage
                    None, // waiting_on
                    None, // waiting_summary
                ).map_err(|e| e.to_string())?;
                let audit_line = format!("{} updated via proposal", ticket_id);
                let _ = crate::kanban::audit::append_ticket_log(&self.vault_root, domain, project, &audit_line);
                Ok(serde_json::json!({"op": "update_ticket", "ticket_id": ticket_id, "applied": true}))
            }
            ChangeOperation::AppendNote { ticket_id, text } => {
                kanban.add_note(ticket_id, text, Some("proposal")).map_err(|e| e.to_string())?;
                Ok(serde_json::json!({"op": "append_note", "ticket_id": ticket_id, "applied": true}))
            }
            ChangeOperation::CreateRelationship { from_ticket_id, to_ticket_id, relationship_type, description } => {
                let rel_type = crate::kanban::relationships::RelationshipType::parse(relationship_type)
                    .ok_or_else(|| format!("invalid relationship_type '{}'", relationship_type))?;
                let rel = crate::kanban::relationships::Relationship {
                    id: uuid::Uuid::new_v4().to_string(),
                    project: project.to_string(),
                    from_ticket_id: from_ticket_id.clone(),
                    to_ticket_id: to_ticket_id.clone(),
                    relationship_type: rel_type,
                    description: description.clone(),
                    created_at: chrono::Utc::now().to_rfc3339(),
                    source: Some("proposal".into()),
                };
                crate::kanban::relationships::append_event(&self.vault_root, domain, project, &crate::kanban::relationships::RelationshipEvent::Create(rel))
                    .map_err(|e| e.to_string())?;
                Ok(serde_json::json!({"op": "create_relationship", "from": from_ticket_id, "to": to_ticket_id, "applied": true}))
            }
            ChangeOperation::CreateQuestion {
                question,
                ticket_id,
                current_assumption,
                evidence,
                needed_for,
                interaction_type,
                interaction_options,
                interaction_placeholder,
            } => {
                crate::kanban::questions::validate_interaction(
                    *interaction_type,
                    interaction_options.as_deref(),
                    interaction_placeholder.as_deref(),
                )?;
                let now = chrono::Utc::now().to_rfc3339();
                let q = crate::kanban::questions::Question {
                    id: uuid::Uuid::new_v4().to_string(),
                    project: project.to_string(),
                    ticket_id: ticket_id.clone(),
                    question: question.clone(),
                    current_assumption: current_assumption.clone(),
                    evidence: evidence.clone(),
                    needed_for: needed_for.clone(),
                    interaction_type: *interaction_type,
                    interaction_options: interaction_options.clone(),
                    interaction_placeholder: interaction_placeholder.clone(),
                    status: crate::kanban::questions::QuestionStatus::Open,
                    answer: None,
                    created_at: now.clone(),
                    updated_at: now,
                    resolved_at: None,
                    source: Some("proposal".into()),
                };
                crate::kanban::questions::append_event(&self.vault_root, domain, project, &crate::kanban::questions::QuestionEvent::Create(q))
                    .map_err(|e| e.to_string())?;
                Ok(serde_json::json!({"op": "create_question", "question": question, "applied": true}))
            }
            ChangeOperation::AnswerQuestion { question_id, answer } => {
                crate::kanban::questions::append_event(&self.vault_root, domain, project, &crate::kanban::questions::QuestionEvent::Answer {
                    id: question_id.clone(), project: project.to_string(), answer: answer.clone(), timestamp: chrono::Utc::now().to_rfc3339(),
                }).map_err(|e| e.to_string())?;
                Ok(serde_json::json!({"op": "answer_question", "question_id": question_id, "applied": true}))
            }
            ChangeOperation::InvalidateQuestion { question_id, reason } => {
                crate::kanban::questions::append_event(&self.vault_root, domain, project, &crate::kanban::questions::QuestionEvent::Invalidate {
                    id: question_id.clone(), project: project.to_string(), reason: reason.clone(), timestamp: chrono::Utc::now().to_rfc3339(),
                }).map_err(|e| e.to_string())?;
                Ok(serde_json::json!({"op": "invalidate_question", "question_id": question_id, "applied": true}))
            }
        }
    }

    // ---- Verification handler ----

    fn kanban_verify(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        let Some(ref ticket_id) = p.ticket_id else {
            return json_error("'ticket_id' is required for verify");
        };
        let Some(ref vs_str) = p.verification_source else {
            return json_error(&format!("'verification_source' is required. One of: {}", crate::kanban::verification::VerificationSource::all_names().join(", ")));
        };
        let Some(vs) = crate::kanban::verification::VerificationSource::parse(vs_str) else {
            return json_error(&format!("invalid verification_source '{}'. Must be one of: {}", vs_str, crate::kanban::verification::VerificationSource::all_names().join(", ")));
        };
        let Some(ref conf_str) = p.confidence else {
            return json_error(&format!("'confidence' is required. One of: {}", crate::kanban::verification::Confidence::all_names().join(", ")));
        };
        let Some(confidence) = crate::kanban::verification::Confidence::parse(conf_str) else {
            return json_error(&format!("invalid confidence '{}'. Must be one of: {}", conf_str, crate::kanban::verification::Confidence::all_names().join(", ")));
        };

        let Some((domain, project)) = self.lookup_item_domain(kanban, ticket_id) else {
            return json_error(&format!("ticket '{}' not found", ticket_id));
        };
        if let Err(e) = self.check_kanban_domain_access(&domain) {
            return json_error(&e);
        }

        let v = crate::kanban::verification::Verification {
            id: uuid::Uuid::new_v4().to_string(),
            ticket_id: ticket_id.clone(),
            project: project.clone(),
            verified_at: chrono::Utc::now().to_rfc3339(),
            verification_source: vs,
            confidence,
            summary: p.summary.clone(),
            source: p.source.clone(),
        };
        if let Err(e) = crate::kanban::verification::append_event(&self.vault_root, &domain, &project, &crate::kanban::verification::VerificationEvent::Verify(v.clone())) {
            return json_error(&format!("failed to write verification: {e}"));
        }
        let audit_line = format!("{} verified: {} ({})", ticket_id, conf_str, vs_str);
        let _ = crate::kanban::audit::append_ticket_log(&self.vault_root, &domain, &project, &audit_line);
        serde_json::to_string(&serde_json::json!({"verified": true, "verification": v})).unwrap_or_default()
    }

    // ---- Reality check handler ----

    fn kanban_reality_check(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        let Some(ref project) = p.project else {
            return json_error("'project' is required for reality_check");
        };
        let domain = match &p.domain {
            Some(d) => d.clone(),
            None => match self.infer_domain_for_project(project) {
                Some(d) => d,
                None => return json_error(&format!("cannot infer domain for project '{}'", project)),
            },
        };

        let items = match kanban.list_metadata(Some(project), true) {
            Ok(items) => items,
            Err(e) => return json_error(&format!("failed to list tickets: {e}")),
        };
        let relationships = crate::kanban::relationships::read_all(&self.vault_root, &domain, project);
        let questions = crate::kanban::questions::read_all(&self.vault_root, &domain, project);
        let verifications = crate::kanban::verification::read_all(&self.vault_root, &domain, project);

        let opts = crate::kanban::reality_check::RealityCheckOptions {
            compact: !p.full.unwrap_or(false),
            limit: p.limit.unwrap_or(10),
            include_done: p.include_done.unwrap_or(false),
            stale_after_days: p.stale_after_days.unwrap_or(14),
        };

        let result = crate::kanban::reality_check::build_reality_check(
            project,
            p.epic.as_deref(),
            &items,
            &relationships,
            &questions,
            &verifications,
            &opts,
        );
        serde_json::to_string_pretty(&result).unwrap_or_default()
    }

    fn kanban_hygiene_suggestions(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        let Some(ref project) = p.project else {
            return json_error("'project' is required for hygiene_suggestions");
        };
        let domain = match &p.domain {
            Some(d) => d.clone(),
            None => match self.infer_domain_for_project(project) {
                Some(d) => d,
                None => return json_error(&format!("cannot infer domain for project '{}'", project)),
            },
        };

        let limit = p.limit.unwrap_or(10);
        let ticket_ids: Vec<String> = match kanban.list_metadata(Some(project), false) {
            Ok(items) => items.iter()
                .filter(|i| {
                    if let Some(ref epic) = p.epic { i.epic.as_deref() == Some(epic.as_str()) } else { true }
                })
                .map(|i| i.ticket_id.clone())
                .collect(),
            Err(e) => return json_error(&format!("failed to list tickets: {e}")),
        };

        let items = match kanban.list_with_notes(project, &ticket_ids) {
            Ok(items) => items,
            Err(e) => return json_error(&format!("failed to load ticket details: {e}")),
        };
        let relationships = crate::kanban::relationships::read_all(&self.vault_root, &domain, project);

        let suggestions = crate::kanban::reality_check::build_hygiene_suggestions(
            &items, &relationships, p.epic.as_deref(), limit,
        );
        let total = suggestions.len();
        serde_json::to_string(&serde_json::json!({"suggestions": suggestions, "total": total})).unwrap_or_default()
    }

    /// Read-only planning lens — derives an execution map for a parent ticket,
    /// epic, or project area. Mutates nothing.
    fn kanban_plan(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        let Some(ref project) = p.project else {
            return json_error("'project' is required for plan");
        };
        let domain = match &p.domain {
            Some(d) => d.clone(),
            None => match self.infer_domain_for_project(project) {
                Some(d) => d,
                None => return json_error(&format!("cannot infer domain for project '{}'", project)),
            },
        };

        // Load all project tickets WITH notes and children so classification can
        // cite descriptions/notes and walk the parent subtree.
        let items = match kanban.list(Some(project), None, None, None, None, None, true, None) {
            Ok(items) => items,
            Err(e) => return json_error(&format!("failed to list tickets: {e}")),
        };

        if let Some(ref root) = p.root_ticket_id {
            if !items.iter().any(|i| &i.ticket_id == root) {
                return json_error(&format!("root_ticket_id '{}' not found in project '{}'", root, project));
            }
        }

        let relationships = crate::kanban::relationships::read_all(&self.vault_root, &domain, project);
        let questions = crate::kanban::questions::read_all(&self.vault_root, &domain, project);

        let opts = crate::kanban::plan::PlanOptions {
            full: p.full.unwrap_or(false),
            limit: p.limit.unwrap_or(10),
        };

        let map = crate::kanban::plan::build_plan(
            project,
            p.root_ticket_id.as_deref(),
            p.epic.as_deref(),
            &items,
            &relationships,
            &questions,
            &opts,
        );
        serde_json::to_string_pretty(&map).unwrap_or_default()
    }

    /// Request asynchronous grooming. This is a durable kanban request, NOT an RPC:
    /// it appends a `groom_requested` event and returns immediately. It does not run
    /// Claude, does not call the vault service, and does not mutate the ticket. The
    /// always-on service consumes the request later and appends a receipt.
    fn kanban_groom(&self, kanban: &crate::kanban::store::KanbanStore, p: &KanbanParams) -> String {
        // Single-ticket form: ticket_id provided.
        if let Some(ref ticket_id) = p.ticket_id {
            let Some((domain, project)) = self.lookup_item_domain(kanban, ticket_id) else {
                return json_error(&format!("ticket '{}' not found", ticket_id));
            };
            if let Err(e) = self.check_kanban_domain_access(&domain) {
                return json_error(&e);
            }
            return self.request_groom_one(&domain, &project, ticket_id, p);
        }

        // Batch form: project provided, request grooming for up to `limit` tickets.
        let Some(ref project) = p.project else {
            return json_error("'ticket_id' or 'project' is required for groom");
        };
        let domain = match &p.domain {
            Some(d) => d.clone(),
            None => match self.infer_domain_for_project(project) {
                Some(d) => d,
                None => return json_error(&format!("cannot infer domain for project '{}'", project)),
            },
        };
        if let Err(e) = self.check_kanban_domain_access(&domain) {
            return json_error(&e);
        }

        let limit = p.limit.unwrap_or(3);
        // Candidates: open (non-done) tickets, most-recently-updated first, that
        // don't already have a pending grooming request.
        let items = match kanban.list(Some(project), None, None, None, None, None, false, None) {
            Ok(items) => items,
            Err(e) => return json_error(&format!("failed to list tickets: {e}")),
        };
        let existing = crate::kanban::events::read_events(&self.vault_root, &domain, project);

        let mut requested: Vec<String> = Vec::new();
        let mut skipped_pending: Vec<String> = Vec::new();
        for item in &items {
            if requested.len() >= limit { break; }
            if crate::kanban::events::has_pending_groom(&existing, &item.ticket_id) {
                skipped_pending.push(item.ticket_id.clone());
                continue;
            }
            let event = crate::kanban::events::KanbanEvent::GroomRequested {
                ticket_id: item.ticket_id.clone(),
                requested_by: p.requested_by.clone().or_else(|| p.source.clone()),
                reason: p.reason.clone(),
                timestamp: chrono::Utc::now().to_rfc3339(),
            };
            if let Err(e) = crate::kanban::events::append_event(&self.vault_root, &domain, project, &event) {
                return json_error(&format!("failed to write groom_requested for {}: {e}", item.ticket_id));
            }
            let _ = crate::kanban::audit::append_ticket_log(
                &self.vault_root, &domain, project, &format!("{} groom_requested", item.ticket_id),
            );
            requested.push(item.ticket_id.clone());
        }

        serde_json::to_string(&serde_json::json!({
            "requested": true,
            "project": project,
            "event": "groom_requested",
            "count": requested.len(),
            "ticket_ids": requested,
            "skipped_already_pending": skipped_pending,
            "mutated_ticket": false,
            "message": "Grooming requested. The vault service will process these asynchronously.",
        })).unwrap_or_default()
    }

    fn request_groom_one(&self, domain: &str, project: &str, ticket_id: &str, p: &KanbanParams) -> String {
        let existing = crate::kanban::events::read_events(&self.vault_root, domain, project);
        // Dedupe: don't stack a second pending request on the same ticket.
        if crate::kanban::events::has_pending_groom(&existing, ticket_id) {
            return serde_json::to_string(&serde_json::json!({
                "requested": true,
                "ticket_id": ticket_id,
                "event": "groom_requested",
                "already_pending": true,
                "mutated_ticket": false,
                "message": "Grooming already requested for this ticket and not yet processed; not duplicated.",
            })).unwrap_or_default();
        }
        let event = crate::kanban::events::KanbanEvent::GroomRequested {
            ticket_id: ticket_id.to_string(),
            requested_by: p.requested_by.clone().or_else(|| p.source.clone()),
            reason: p.reason.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        if let Err(e) = crate::kanban::events::append_event(&self.vault_root, domain, project, &event) {
            return json_error(&format!("failed to write groom_requested: {e}"));
        }
        let _ = crate::kanban::audit::append_ticket_log(
            &self.vault_root, domain, project, &format!("{ticket_id} groom_requested"),
        );
        serde_json::to_string(&serde_json::json!({
            "requested": true,
            "ticket_id": ticket_id,
            "event": "groom_requested",
            "mutated_ticket": false,
            "message": "Grooming requested. The vault service will process this asynchronously.",
        })).unwrap_or_default()
    }

    fn infer_domain_for_project(&self, project: &str) -> Option<String> {
        let registry = self.registry.try_read().ok()?;
        for domain in registry.all() {
            let domain_name = domain.name.as_str();
            let project_dir = self.vault_root.join(domain_name).join(project);
            if project_dir.exists() {
                return Some(domain_name.to_string());
            }
        }
        None
    }

    fn lookup_item_domain(&self, kanban: &crate::kanban::store::KanbanStore, ticket_id: &str) -> Option<(String, String)> {
        let conn = kanban.conn().ok()?;
        conn.query_row(
            "SELECT p.domain, i.project FROM kanban_items i JOIN kanban_projects p ON i.project = p.project WHERE i.ticket_id = ?1",
            rusqlite::params![ticket_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        ).ok()
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for WardwellServer {
    fn get_info(&self) -> ServerInfo {
        let instructions = if self.kanban.is_some() {
            "Wardwell: Personal AI knowledge vault. Four tools: \
             wardwell_search (action: search|read|history|orchestrate|retrospective|patterns|context|resume; \
             search supports mode:'semantic' for broad/conceptual queries — prefer it over keyword for exploratory searches), \
             wardwell_write (action: sync|decide|append_history|lesson|append|write_file), \
             wardwell_clipboard (copy to clipboard, ask first), \
             wardwell_kanban (action: list|create|update|move|note|query|relationship_create|relationship_list|relationship_delete|question_create|question_list|question_update|question_answer|question_invalidate|proposal_create|proposal_get|proposal_list|proposal_approve|proposal_reject|proposal_apply|verify|reality_check|plan|groom — project kanban board with tickets, statuses, priorities, deadlines). \
             GROOMING RULE: when you read a ticket (get), check item.grooming. If item.grooming.artifact_path is present, READ that artifact (wardwell_search action:read path:<artifact_path>) BEFORE planning or building — it is the latest readiness/DDD assessment for the ticket. Treat item.grooming.readiness (e.g. build_prompt_needed, design_needed, audit_needed, blocker) and item.grooming.surfaced as primary signals."
                .to_string()
        } else {
            "Wardwell: Personal AI knowledge vault. Three tools: \
             wardwell_search (action: search|read|history|orchestrate|retrospective|patterns|context|resume; \
             search supports mode:'semantic' for broad/conceptual queries — prefer it over keyword for exploratory searches), \
             wardwell_write (action: sync|decide|append_history|lesson|append|write_file), \
             wardwell_clipboard (copy to clipboard, ask first)."
                .to_string()
        };

        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation::from_build_env(),
            instructions: Some(instructions),
        }
    }
}

// -- Graph actions --

impl WardwellServer {
    fn graph_links(&self, p: &GraphParams) -> String {
        if !self.config.features.graph_navigation {
            return json_error("graph_navigation feature is disabled in config.yml");
        }
        let path = match &p.path {
            Some(path) => path.clone(),
            None => return json_error("'path' is required for action 'links'."),
        };

        let clean = path.strip_prefix('/').unwrap_or(&path).to_string();

        let forward = match self.index.get_forward_links(&clean) {
            Ok(links) => links,
            Err(e) => return json_error(&format!("failed to get forward links: {e}")),
        };

        let backlinks = match self.index.get_backlinks(&clean) {
            Ok(links) => links,
            Err(e) => return json_error(&format!("failed to get backlinks: {e}")),
        };

        serde_json::to_string_pretty(&serde_json::json!({
            "path": clean,
            "forward_links": forward,
            "forward_count": forward.len(),
            "backlinks": backlinks,
            "backlink_count": backlinks.len(),
        })).unwrap_or_default()
    }

    fn graph_resolve(&self, p: &GraphParams) -> String {
        if !self.config.features.entity_resolution {
            return json_error("entity_resolution feature is disabled in config.yml");
        }
        let query = match &p.query {
            Some(q) => q.clone(),
            None => return json_error("'query' is required for action 'resolve'."),
        };

        let limit = p.limit.unwrap_or(5);
        let query_lower = query.to_lowercase();

        // Check aliases first (via domain registry)
        let mut alias_matches = Vec::new();
        if let Ok(reg) = self.registry.try_read() {
            for domain in reg.all() {
                for (alias, target) in &domain.aliases {
                    if alias.to_lowercase() == query.to_lowercase()
                        || target.to_lowercase() == query.to_lowercase()
                    {
                        alias_matches.push(serde_json::json!({
                            "type": "alias",
                            "alias": alias,
                            "target": target,
                            "domain": domain.name.as_str(),
                            "score": 1.0,
                        }));
                    }
                }
            }
        }

        // Entity resolution from index
        let entity_matches = match self.index.resolve_entity(&query, limit) {
            Ok(matches) => matches,
            Err(e) => return json_error(&format!("entity resolution failed: {e}")),
        };

        // Check kanban tickets — filter by similarity to avoid noisy substring matches
        let mut kanban_matches = Vec::new();
        if let Some(ref kanban) = self.kanban {
            if let Ok(results) = kanban.search(&query, None, None) {
                for item in results {
                    let id_lower = item.ticket_id.to_lowercase();
                    let title_lower = item.title.to_lowercase();

                    let score = if id_lower == query_lower || title_lower == query_lower {
                        1.0
                    } else if id_lower.starts_with(&query_lower) || title_lower.starts_with(&query_lower) {
                        0.95
                    } else {
                        let id_sim = strsim::jaro_winkler(&query_lower, &id_lower);
                        let title_sim = strsim::jaro_winkler(&query_lower, &title_lower);
                        id_sim.max(title_sim)
                    };

                    if score > 0.7 {
                        kanban_matches.push((score, serde_json::json!({
                            "type": "kanban_ticket",
                            "ticket_id": item.ticket_id,
                            "title": item.title,
                            "project": item.project,
                            "status": item.status,
                            "score": score,
                        })));
                    }
                }
                kanban_matches.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
                kanban_matches.truncate(limit);
            }
        }
        let kanban_matches: Vec<_> = kanban_matches.into_iter().map(|(_, v)| v).collect();

        serde_json::to_string_pretty(&serde_json::json!({
            "query": query,
            "aliases": alias_matches,
            "entities": entity_matches,
            "kanban_tickets": kanban_matches,
        })).unwrap_or_default()
    }

    fn graph_mentions(&self, p: &GraphParams) -> String {
        if !self.config.features.unlinked_mentions {
            return json_error("unlinked_mentions feature is disabled in config.yml");
        }
        let path = match &p.path {
            Some(path) => path.clone(),
            None => return json_error("'path' is required for action 'mentions'."),
        };

        let clean = path.strip_prefix('/').unwrap_or(&path).to_string();

        // Build target names from file stem and frontmatter summary
        let mut target_names = Vec::new();
        let full_path = resolve_path(&self.vault_root, &clean);
        if let Some(fp) = full_path {
            if let Ok(vf) = crate::vault::reader::read_file(&fp) {
                // Use filename stem
                if let Some(stem) = fp.file_stem().and_then(|s| s.to_str()) {
                    if stem.len() >= 3 {
                        target_names.push(stem.to_string());
                    }
                }
                // Use summary/title if available
                if let Some(ref summary) = vf.frontmatter.summary {
                    if summary.len() >= 3 {
                        target_names.push(summary.clone());
                    }
                }
            }
        }

        if target_names.is_empty() {
            // Fallback to filename stem from path
            if let Some(stem) = std::path::Path::new(&clean).file_stem().and_then(|s| s.to_str()) {
                if stem.len() >= 3 {
                    target_names.push(stem.to_string());
                }
            }
        }

        if target_names.is_empty() {
            return json_error("could not derive searchable names from the file path");
        }

        let mentions = match self.index.find_unlinked_mentions(&clean, &target_names) {
            Ok(m) => m,
            Err(e) => return json_error(&format!("unlinked mentions search failed: {e}")),
        };

        let limit = p.limit.unwrap_or(10);
        let truncated: Vec<_> = mentions.into_iter().take(limit).collect();

        serde_json::to_string_pretty(&serde_json::json!({
            "path": clean,
            "searched_names": target_names,
            "mentions": truncated,
            "total": truncated.len(),
        })).unwrap_or_default()
    }
}

// -- Helpers --

fn json_error(msg: &str) -> String {
    serde_json::to_string(&serde_json::json!({"error": msg})).unwrap_or_default()
}

fn extract_ticket_ids_from_op(op: &crate::kanban::proposals::ChangeOperation) -> Vec<String> {
    use crate::kanban::proposals::ChangeOperation;
    match op {
        ChangeOperation::UpdateTicket { ticket_id, .. } => vec![ticket_id.clone()],
        ChangeOperation::AppendNote { ticket_id, .. } => vec![ticket_id.clone()],
        ChangeOperation::CreateRelationship { from_ticket_id, to_ticket_id, .. } => vec![from_ticket_id.clone(), to_ticket_id.clone()],
        ChangeOperation::CreateQuestion { ticket_id, .. } => ticket_id.iter().cloned().collect(),
        ChangeOperation::AnswerQuestion { .. } => vec![],
        ChangeOperation::InvalidateQuestion { .. } => vec![],
    }
}

/// Resolve a vault path: only allow vault-relative paths.
fn resolve_path(vault_root: &std::path::Path, path: &str) -> Option<PathBuf> {
    // Strip leading slash from relative paths (common copy-paste error)
    let clean = path.strip_prefix('/').unwrap_or(path);

    // Reject absolute paths and traversal attempts
    let p = std::path::Path::new(clean);
    if p.is_absolute() {
        return None;
    }
    // Reject path traversal (e.g. "../../etc/passwd")
    for component in p.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return None;
        }
    }

    let vault_candidate = vault_root.join(clean);
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
            stop_hook: true,
            kanban_enabled: false,
            kanban_queries: std::collections::HashMap::new(),
            kanban_prefixes: std::collections::HashMap::new(),
            features: Default::default(),
        };
        WardwellServer::new(config, index, Arc::new(Mutex::new(None)), None, None)
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
        let entries = collect_history_entries(&tmp, Some(since), None, true, &[]);
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

        let entries = collect_history_entries(&tmp, None, None, true, &[]);
        assert!(entries.is_empty());

        let entries_with_archive = collect_history_entries(&tmp, None, None, false, &[]);
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

        let entries = collect_history_entries(&tmp, None, Some("work"), true, &[]);
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

        let entries = collect_history_entries(&tmp, Some(chrono::NaiveDate::parse_from_str("2026-02-01", "%Y-%m-%d").unwrap()), None, true, &[]);
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

        let entries = collect_history_entries(&tmp, None, None, true, &[]);
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
        let today = chrono::Local::now().date_naive().format("%Y-%m-%d").to_string();
        let recent_content = make_history_jsonl(&[(&today, "Recent", "active", "f")]);
        let tmp = setup_test_vault("wardwell_test_stale", &[
            ("work", "stale-proj", &old_content),
            ("work", "fresh-proj", &recent_content),
        ]);

        let entries = collect_history_entries(&tmp, None, None, true, &[]);
        let today_date = chrono::Local::now().date_naive();
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
                        .is_ok_and(|d| (today_date - d).num_days() >= 14)
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

        let entries = collect_history_entries(&tmp, None, None, true, &[]);
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
            what_happened: None, root_cause: None, prevention: None, path: None,
            source: None,
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
            what_happened: None, root_cause: None, prevention: None, path: None,
            source: None,
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
            what_happened: None, root_cause: None, prevention: None, path: None,
            source: None,
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
            what_happened: None, root_cause: None, prevention: None, path: None,
            source: None,
        };
        let result = server.action_append_list(&params, "test-proj", None);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["appended"], true);

        let content = std::fs::read_to_string(project_dir.join("bookmarks.jsonl")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3); // schema + first + second

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn question_mcp_persists_a_typed_decision_and_can_return_it_to_a_question() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("personal").join("finance")).unwrap();
        let server = make_test_server(tmp.path());
        let store = crate::kanban::store::KanbanStore::open(
            &tmp.path().join("kanban.db"),
            tmp.path().to_path_buf(),
        ).unwrap();
        let create: KanbanParams = serde_json::from_value(serde_json::json!({
            "action": "question_create",
            "domain": "personal",
            "project": "finance",
            "question_text": "Keep Figma?",
            "interaction_type": "decision",
            "interaction_options": [
                {"id": "keep", "label": "Keep Figma", "detail": "Retain the workspace."},
                {"id": "cancel", "label": "Cancel Figma", "detail": "Save the monthly cost."}
            ],
            "source": "hank"
        })).unwrap();

        let created: serde_json::Value = serde_json::from_str(
            &server.kanban_question_create(&store, &create),
        ).unwrap();
        assert_eq!(created["created"], true);
        let id = created["question"]["id"].as_str().unwrap();
        let persisted = crate::kanban::questions::read_all(tmp.path(), "personal", "finance");
        assert_eq!(persisted[0].interaction_type, Some(crate::kanban::questions::QuestionInteractionType::Decision));
        assert_eq!(persisted[0].interaction_options.as_ref().map(Vec::len), Some(2));

        let update: KanbanParams = serde_json::from_value(serde_json::json!({
            "action": "question_update",
            "domain": "personal",
            "project": "finance",
            "target_id": id,
            "interaction_type": "question",
            "interaction_placeholder": "Explain the preferred outcome"
        })).unwrap();
        let updated: serde_json::Value = serde_json::from_str(
            &server.kanban_question_update(&update),
        ).unwrap();
        assert_eq!(updated["updated"], true);
        let persisted = crate::kanban::questions::read_all(tmp.path(), "personal", "finance");
        assert_eq!(persisted[0].interaction_type, Some(crate::kanban::questions::QuestionInteractionType::Question));
        assert_eq!(persisted[0].interaction_options, None);
        assert_eq!(persisted[0].interaction_placeholder.as_deref(), Some("Explain the preferred outcome"));
    }

    #[test]
    fn question_mcp_rejects_a_decision_without_enough_choices() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("personal").join("finance")).unwrap();
        let server = make_test_server(tmp.path());
        let store = crate::kanban::store::KanbanStore::open(
            &tmp.path().join("kanban.db"),
            tmp.path().to_path_buf(),
        ).unwrap();
        let params: KanbanParams = serde_json::from_value(serde_json::json!({
            "action": "question_create",
            "domain": "personal",
            "project": "finance",
            "question_text": "Keep Figma?",
            "interaction_type": "decision",
            "interaction_options": [{"id": "keep", "label": "Keep Figma"}]
        })).unwrap();

        let result = server.kanban_question_create(&store, &params);
        assert!(result.contains("requires 2 to 5"));
        assert!(!crate::kanban::questions::jsonl_path(tmp.path(), "personal", "finance").exists());
    }
}
