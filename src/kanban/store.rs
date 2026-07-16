use crate::kanban::events::{self, KanbanEvent};
use rusqlite::{Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

#[derive(Debug, Clone, serde::Serialize)]
pub struct KanbanItem {
    pub ticket_id: String,
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epic: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub status: String,
    pub priority: String,
    pub assignee: Option<String>,
    pub deadline: Option<String>,
    pub source: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
    /// Loop stage (WA-5): idea|grill|spec|design_audit|post_design_audit|audit_gate|build|pr|complete.
    /// `None` for tickets not using the loop system.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    /// Loop wait target (WA-5): `human:*` or `blocker:*`. `None` when not waiting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_on: Option<String>,
    /// Constrained ask surfaced in briefings. `None` unless waiting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_summary: Option<String>,
    /// When the current wait started. Stamped by the tool, never set directly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_since: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<KanbanChild>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub notes: Vec<KanbanNote>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<KanbanAttachment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activity: Vec<ActivityEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children_activity: Vec<ActivityEntry>,
    /// Grooming readiness/status, surfaced inline at read time. Sourced from
    /// receipt events when present, and otherwise resolved from the latest
    /// on-disk artifact (`docs/grooming/<ticket>-grooming-*.md`) so the ticket
    /// always carries a readable pointer (`artifact_path`) plus readiness.
    /// `None` for tickets with no grooming at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grooming: Option<crate::kanban::events::Grooming>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ActivityEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<String>,
    pub event: String,
    pub timestamp: String,
    pub summary: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct KanbanChild {
    pub ticket_id: String,
    pub title: String,
    pub status: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct KanbanAttachment {
    pub attachment_id: String,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
    pub storage_path: String,
    pub read_path: String,
    pub created_at: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct KanbanNote {
    pub id: i64,
    pub text: String,
    pub author: Option<String>,
    pub created_at: String,
}

#[derive(Debug, thiserror::Error)]
pub enum KanbanError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("kanban IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("lock poisoned")]
    LockPoisoned,
}

/// Result of `recover_collisions`: what identity faults were found and which
/// boards were quarantined.
#[derive(Debug, Default, serde::Serialize)]
pub struct CollisionReport {
    /// Cross-domain slug shadows found (human-readable).
    pub slug_collisions: Vec<String>,
    /// Duplicate ticket_id create events found within a board.
    pub duplicate_ids: Vec<String>,
    /// Paths of kanban.jsonl files renamed to quarantine sidecars.
    pub quarantined: Vec<String>,
}

/// Kanban store: JSONL is source of truth, SQLite is materialized cache.
#[derive(Debug)]
pub struct KanbanStore {
    conn: Mutex<Connection>,
    vault_root: PathBuf,
    /// Reverse lookup: project slug → group name (from config kanban.groups).
    project_to_group: HashMap<String, String>,
}

impl KanbanStore {
    const SCHEMA_VERSION: i64 = 8;

    pub fn open(db_path: &Path, vault_root: PathBuf) -> Result<Self, KanbanError> {
        let groups = load_kanban_yml(&vault_root);

        // Check schema version — wipe if stale (SQLite is just a cache)
        if db_path.exists() {
            if let Ok(c) = Connection::open(db_path) {
                let version: i64 = c.query_row(
                    "SELECT COALESCE((SELECT version FROM kanban_schema_version), 0)", [], |r| r.get(0),
                ).unwrap_or(0);
                if version != Self::SCHEMA_VERSION {
                    drop(c);
                    let _ = std::fs::remove_file(db_path);
                    let shm = db_path.with_extension("db-shm");
                    let wal = db_path.with_extension("db-wal");
                    let _ = std::fs::remove_file(shm);
                    let _ = std::fs::remove_file(wal);
                    eprintln!("wardwell: kanban schema v{version} → v{}, rebuilding from JSONL", Self::SCHEMA_VERSION);
                }
            }
        }

        let conn = Connection::open(db_path)?;
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        conn.busy_timeout(Duration::from_secs(5))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS kanban_schema_version (version INTEGER NOT NULL);"
        )?;
        let current: i64 = conn.query_row(
            "SELECT COALESCE((SELECT version FROM kanban_schema_version), 0)", [], |r| r.get(0),
        ).unwrap_or(0);
        if current != Self::SCHEMA_VERSION {
            conn.execute_batch("DELETE FROM kanban_schema_version;")?;
            conn.execute("INSERT INTO kanban_schema_version (version) VALUES (?1)", rusqlite::params![Self::SCHEMA_VERSION])?;
        }

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS kanban_projects (
                project     TEXT PRIMARY KEY,
                prefix      TEXT UNIQUE NOT NULL,
                domain      TEXT NOT NULL,
                next_id     INTEGER NOT NULL DEFAULT 1
            );
            CREATE TABLE IF NOT EXISTS kanban_items (
                ticket_id    TEXT PRIMARY KEY,
                project      TEXT NOT NULL,
                title        TEXT NOT NULL,
                description  TEXT,
                status       TEXT NOT NULL DEFAULT 'backlog',
                priority     TEXT NOT NULL DEFAULT 'medium',
                assignee     TEXT,
                deadline     TEXT,
                source       TEXT,
                epic         TEXT,
                parent       TEXT,
                position     INTEGER,
                tags         TEXT DEFAULT '[]',
                stage           TEXT,
                waiting_on      TEXT,
                waiting_summary TEXT,
                waiting_since   TEXT,
                created_at   TEXT NOT NULL,
                updated_at   TEXT NOT NULL,
                completed_at TEXT
            );
            CREATE TABLE IF NOT EXISTS kanban_notes (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                ticket_id  TEXT NOT NULL,
                text       TEXT NOT NULL,
                author     TEXT,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS kanban_attachments (
                attachment_id TEXT PRIMARY KEY,
                ticket_id    TEXT NOT NULL,
                filename     TEXT NOT NULL,
                mime_type    TEXT NOT NULL,
                size         INTEGER NOT NULL,
                storage_path TEXT NOT NULL,
                created_at   TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_kanban_items_project ON kanban_items(project);
            CREATE INDEX IF NOT EXISTS idx_kanban_items_status ON kanban_items(status);
            CREATE INDEX IF NOT EXISTS idx_kanban_notes_ticket ON kanban_notes(ticket_id);
            CREATE INDEX IF NOT EXISTS idx_kanban_attachments_ticket ON kanban_attachments(ticket_id);
            CREATE TABLE IF NOT EXISTS kanban_fold_state (
                path      TEXT PRIMARY KEY,
                byte_len  INTEGER NOT NULL,
                mtime_ns  INTEGER NOT NULL,
                folded_at TEXT NOT NULL
            );"
        )?;

        let mut project_to_group = HashMap::new();
        for (group_name, projects) in &groups {
            for proj in projects {
                project_to_group.insert(proj.clone(), group_name.clone());
            }
        }
        let store = Self { conn: Mutex::new(conn), vault_root, project_to_group };
        if let Err(e) = store.rebuild_from_jsonl() {
            eprintln!("wardwell: kanban rebuild warning (non-fatal): {e}");
        }
        Ok(store)
    }

    pub fn conn(&self) -> Result<MutexGuard<'_, Connection>, KanbanError> {
        self.conn.lock().map_err(|_| KanbanError::LockPoisoned)
    }

    // ---- Write path: JSONL append + SQLite cache update ----

    #[allow(clippy::too_many_arguments)]
    pub fn create_item(
        &self,
        title: &str,
        project: &str,
        domain: &str,
        description: Option<&str>,
        status: Option<&str>,
        priority: Option<&str>,
        assignee: Option<&str>,
        deadline: Option<&str>,
        source: Option<&str>,
        epic: Option<&str>,
        parent: Option<&str>,
        tags: Option<&[String]>,
        config_prefixes: &HashMap<String, String>,
    ) -> Result<KanbanItem, KanbanError> {
        self.fold_foreign_events_logged();
        let status = validate_status(status.unwrap_or("backlog"))?;
        let priority = validate_priority(priority.unwrap_or("medium"))?;
        let now = chrono::Utc::now().to_rfc3339();
        let tags_vec = tags.map(|t| t.to_vec()).unwrap_or_default();

        let group = self.project_to_group.get(project).cloned();

        // Hold the connection mutex for the ENTIRE allocate→append→cache
        // critical section. The old code resolved the id under a brief lock,
        // dropped it, then appended — so two concurrent creates could read the
        // same next_id and mint the same ticket_id (the SW-57…67 collision).
        // Now the single process-wide mutex serializes creates, and the id
        // itself is reserved by an atomic counter bump inside a DB transaction.
        // Cross-process lock on the board file coordinates this create with the
        // Ruby dual writer. Held until the appends below complete.
        let _board_lock = events::BoardLock::acquire(&self.vault_root, domain, project)?;

        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        Self::ensure_project_domain(&tx, project, domain)?;
        let (prefix, next_id) =
            Self::reserve_ticket_id(&tx, &self.vault_root, project, domain, config_prefixes, &self.project_to_group_prefixes())?;
        let ticket_id = format!("{prefix}-{next_id}");

        let event = KanbanEvent::Create {
            ticket_id: ticket_id.clone(),
            title: title.to_string(),
            project: project.to_string(),
            group: group.clone(),
            epic: epic.map(str::to_string),
            parent: parent.map(str::to_string),
            tags: tags_vec.clone(),
            status: status.to_string(),
            priority: priority.to_string(),
            description: description.map(str::to_string),
            deadline: deadline.map(str::to_string),
            assignee: assignee.map(str::to_string),
            source: source.map(str::to_string),
            timestamp: now.clone(),
        };

        // Append to canonical JSONL while still holding the mutex. If either
        // append fails, the transaction is dropped (rolled back) and the
        // reserved id is released — no gap, no orphaned counter bump.
        events::append_event(&self.vault_root, domain, project, &event)?;
        events::append_meta(&self.vault_root, domain, project, &prefix, next_id + 1)?;

        let completed_at: Option<String> = if status == "done" { Some(now.clone()) } else { None };
        let tags_json = serde_json::to_string(&tags_vec).unwrap_or_else(|_| "[]".into());
        tx.execute(
            "INSERT OR REPLACE INTO kanban_items (ticket_id, project, title, description, status, priority, assignee, deadline, source, epic, parent, tags, created_at, updated_at, completed_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            rusqlite::params![ticket_id, project, title, description, status, priority, assignee, deadline, source, epic, parent, tags_json, now, now, completed_at],
        )?;
        tx.commit()?;

        Ok(KanbanItem {
            ticket_id, project: project.into(), group, epic: epic.map(str::to_string), title: title.into(),
            description: description.map(str::to_string), status: status.into(), priority: priority.into(),
            assignee: assignee.map(str::to_string), deadline: deadline.map(str::to_string),
            source: source.map(str::to_string), parent: parent.map(str::to_string), position: None, children: vec![],
            tags: tags_vec, created_at: now.clone(), updated_at: now,
            completed_at, stage: None, waiting_on: None, waiting_summary: None, waiting_since: None,
            notes: vec![], attachments: vec![], activity: vec![], children_activity: vec![], grooming: None,
        })
    }

    pub fn move_item(&self, ticket_id: &str, new_status: &str) -> Result<(KanbanItem, String), KanbanError> {
        self.fold_foreign_events_logged();
        let new_status = validate_status(new_status)?;
        let conn = self.conn()?;
        let now = chrono::Utc::now().to_rfc3339();

        let (old_status, project, domain) = self.get_item_context(&conn, ticket_id)?;

        let event = KanbanEvent::Move {
            ticket_id: ticket_id.into(),
            from: Some(old_status.clone()),
            to: new_status.to_string(),
            timestamp: now.clone(),
        };
        events::append_event(&self.vault_root, &domain, &project, &event)?;

        let completed_at: Option<String> = if new_status == "done" { Some(now.clone()) } else { None };
        conn.execute(
            "UPDATE kanban_items SET status=?1, updated_at=?2, completed_at=?3 WHERE ticket_id=?4",
            rusqlite::params![new_status, now, completed_at, ticket_id],
        )?;

        let transition = format!("{old_status} → {new_status}");
        let note_text = format!("Status: {transition}");
        conn.execute(
            "INSERT INTO kanban_notes (ticket_id, text, author, created_at) VALUES (?1,?2,?3,?4)",
            rusqlite::params![ticket_id, note_text, Option::<String>::None, now],
        )?;

        let item = self.get_item_with_conn(&conn, ticket_id)?;
        Ok((item, transition))
    }

    /// Sentinel for `waiting_on` meaning "clear it". The tool accepts an empty
    /// string or the literal "null" from callers; both map to this.
    fn is_waiting_clear(v: &str) -> bool {
        v.is_empty() || v.eq_ignore_ascii_case("null")
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_item(
        &self,
        ticket_id: &str,
        title: Option<&str>,
        description: Option<&str>,
        status: Option<&str>,
        priority: Option<&str>,
        assignee: Option<&str>,
        deadline: Option<&str>,
        epic: Option<&str>,
        parent: Option<&str>,
        tags: Option<&[String]>,
        stage: Option<&str>,
        waiting_on: Option<&str>,
        waiting_summary: Option<&str>,
    ) -> Result<KanbanItem, KanbanError> {
        self.fold_foreign_events_logged();
        if let Some(p) = priority { validate_priority(p)?; }
        if let Some(s) = stage { validate_stage(s)?; }
        if let Some(w) = waiting_on
            && !Self::is_waiting_clear(w)
        {
            validate_waiting_on(w)?;
        }
        // Explicit status validation is deferred until after loop invariants may
        // override it — but a caller-supplied status must still be legal.
        if let Some(s) = status { validate_status(s)?; }

        let conn = self.conn()?;
        let now = chrono::Utc::now().to_rfc3339();

        let (current_status, project, domain) = self.get_item_context(&conn, ticket_id)?;

        // Build the resolved field set. This is the single choke point for the
        // WA-5 loop invariants (loop-system-spec.md "Tool-enforced invariants").
        // Every downstream write — the JSONL update event AND the SQLite cache —
        // is derived from `fields`, so they can never disagree.
        let mut fields: HashMap<String, serde_json::Value> = HashMap::new();
        if let Some(v) = title { fields.insert("title".into(), serde_json::Value::String(v.into())); }
        if let Some(v) = description { fields.insert("description".into(), serde_json::Value::String(v.into())); }
        if let Some(v) = status { fields.insert("status".into(), serde_json::Value::String(v.into())); }
        if let Some(v) = priority { fields.insert("priority".into(), serde_json::Value::String(v.into())); }
        if let Some(v) = assignee { fields.insert("assignee".into(), serde_json::Value::String(v.into())); }
        if let Some(v) = deadline { fields.insert("deadline".into(), serde_json::Value::String(v.into())); }
        if let Some(v) = epic { fields.insert("epic".into(), serde_json::Value::String(v.into())); }
        if let Some(v) = parent { fields.insert("parent".into(), if v.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(v.into()) }); }
        if let Some(t) = tags {
            fields.insert("tags".into(), serde_json::Value::Array(t.iter().map(|s| serde_json::Value::String(s.clone())).collect()));
        }

        // ---- WA-5 loop invariants ----
        // Note: stage and waiting_on are mutually resolved. A stage change wins
        // over an explicit waiting_on in the same call (advancing means the prior
        // ask is moot), so stage is applied first and clears the wait, then a
        // waiting_on set (if still present after clearing) re-establishes it.
        if let Some(s) = stage {
            // (6) enum-validated above. Set the stage.
            fields.insert("stage".into(), serde_json::Value::String(s.into()));
            // (2) ANY stage change auto-clears the wait (forward or backward).
            fields.insert("waiting_on".into(), serde_json::Value::Null);
            fields.insert("waiting_summary".into(), serde_json::Value::Null);
            fields.insert("waiting_since".into(), serde_json::Value::Null);
            // (4) clearing waiting_on returns activity to in_progress …
            fields.insert("status".into(), serde_json::Value::String("in_progress".into()));
            // (5) … unless we're completing, which supersedes.
            if s == "complete" {
                fields.insert("status".into(), serde_json::Value::String("done".into()));
            }
        }
        if let Some(w) = waiting_on {
            if Self::is_waiting_clear(w) {
                // (4) explicit clear → in_progress + drop summary/since.
                fields.insert("waiting_on".into(), serde_json::Value::Null);
                fields.insert("waiting_summary".into(), serde_json::Value::Null);
                fields.insert("waiting_since".into(), serde_json::Value::Null);
                fields.insert("status".into(), serde_json::Value::String("in_progress".into()));
            } else {
                // (7) prefix-validated above. Set it and …
                fields.insert("waiting_on".into(), serde_json::Value::String(w.into()));
                // (1) auto-stamp waiting_since = now.
                fields.insert("waiting_since".into(), serde_json::Value::String(now.clone()));
                // (3) human:* → review, blocker:* → blocked.
                let derived_status = if w.starts_with("human:") { "review" } else { "blocked" };
                fields.insert("status".into(), serde_json::Value::String(derived_status.into()));
            }
        }
        // waiting_summary is caller-provided free text; only honor it when a wait
        // is (or remains) set — a stage advance/clear already nulled it above.
        if let Some(v) = waiting_summary {
            let waiting_set = matches!(fields.get("waiting_on"), Some(serde_json::Value::String(_)));
            if waiting_set {
                fields.insert("waiting_summary".into(), serde_json::Value::String(v.into()));
            }
        }

        if !fields.is_empty() {
            let event = KanbanEvent::Update {
                ticket_id: ticket_id.into(),
                fields: fields.clone(),
                timestamp: now.clone(),
            };
            events::append_event(&self.vault_root, &domain, &project, &event)?;
        }

        // Update SQLite cache from the SAME resolved `fields` map.
        self.apply_update_to_cache(&conn, ticket_id, &fields, &current_status, &now)?;

        self.get_item_with_conn(&conn, ticket_id)
    }

    /// Apply a resolved update-field map to the SQLite cache. Column values are
    /// taken verbatim from `fields`; a JSON Null clears the column. Mirrors the
    /// JSONL update event so the cache stays byte-for-byte consistent with replay.
    fn apply_update_to_cache(
        &self,
        conn: &Connection,
        ticket_id: &str,
        fields: &HashMap<String, serde_json::Value>,
        current_status: &str,
        now: &str,
    ) -> Result<(), KanbanError> {
        let mut sets = vec!["updated_at = ?1".to_string()];
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(now.to_string())];
        let mut idx = 2;

        // Columns that map 1:1 to a field key and take a nullable TEXT value.
        // `tags` is stored as a JSON string; `status` drives completed_at.
        let text_cols = [
            "title", "description", "priority", "assignee", "deadline", "epic",
            "parent", "stage", "waiting_on", "waiting_summary", "waiting_since",
        ];
        for col in text_cols {
            if let Some(v) = fields.get(col) {
                sets.push(format!("{col}=?{idx}"));
                let val: Option<String> = match v {
                    serde_json::Value::String(s) => Some(s.clone()),
                    serde_json::Value::Null => None,
                    other => Some(other.to_string()),
                };
                params.push(Box::new(val));
                idx += 1;
            }
        }
        if let Some(serde_json::Value::String(v)) = fields.get("status") {
            sets.push(format!("status=?{idx}")); params.push(Box::new(v.clone())); idx += 1;
            if v == "done" && current_status != "done" {
                sets.push(format!("completed_at=?{idx}")); params.push(Box::new(chrono::Utc::now().to_rfc3339())); idx += 1;
            } else if v != "done" && current_status == "done" {
                sets.push(format!("completed_at=?{idx}")); params.push(Box::new(Option::<String>::None)); idx += 1;
            }
        }
        if let Some(serde_json::Value::Array(arr)) = fields.get("tags") {
            let strs: Vec<String> = arr.iter().filter_map(|v| v.as_str().map(String::from)).collect();
            let tj = serde_json::to_string(&strs).unwrap_or_else(|_| "[]".into());
            sets.push(format!("tags=?{idx}")); params.push(Box::new(tj)); idx += 1;
        }
        let _ = idx;

        params.push(Box::new(ticket_id.to_string()));
        let sql = format!("UPDATE kanban_items SET {} WHERE ticket_id=?{}", sets.join(", "), params.len());
        let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        conn.execute(&sql, refs.as_slice())?;
        Ok(())
    }

    pub fn add_note(&self, ticket_id: &str, text: &str, author: Option<&str>) -> Result<KanbanItem, KanbanError> {
        self.fold_foreign_events_logged();
        let conn = self.conn()?;
        let now = chrono::Utc::now().to_rfc3339();

        let (_status, project, domain) = self.get_item_context(&conn, ticket_id)?;

        let event = KanbanEvent::Note {
            ticket_id: ticket_id.into(),
            text: text.into(),
            author: author.map(str::to_string),
            timestamp: now.clone(),
        };
        events::append_event(&self.vault_root, &domain, &project, &event)?;

        conn.execute(
            "INSERT INTO kanban_notes (ticket_id, text, author, created_at) VALUES (?1,?2,?3,?4)",
            rusqlite::params![ticket_id, text, author, now],
        )?;
        conn.execute(
            "UPDATE kanban_items SET updated_at=?1 WHERE ticket_id=?2",
            rusqlite::params![now, ticket_id],
        )?;

        self.get_item_with_conn(&conn, ticket_id)
    }

    pub fn sequence_single(&self, ticket_id: &str, position: i64) -> Result<KanbanItem, KanbanError> {
        self.fold_foreign_events_logged();
        let conn = self.conn()?;
        let now = chrono::Utc::now().to_rfc3339();
        let (_status, project, domain) = self.get_item_context(&conn, ticket_id)?;

        let event = KanbanEvent::Reorder {
            ticket_id: ticket_id.into(),
            data: events::ReorderData { position },
            timestamp: now.clone(),
        };
        events::append_event(&self.vault_root, &domain, &project, &event)?;

        conn.execute(
            "UPDATE kanban_items SET position=?1, updated_at=?2 WHERE ticket_id=?3",
            rusqlite::params![position, now, ticket_id],
        )?;

        self.get_item_with_conn(&conn, ticket_id)
    }

    pub fn sequence_bulk(&self, project: &str, order: &[String]) -> Result<Vec<KanbanItem>, KanbanError> {
        self.fold_foreign_events_logged();
        let conn = self.conn()?;
        let now = chrono::Utc::now().to_rfc3339();

        // Get domain from first ticket
        let domain: String = conn.query_row(
            "SELECT p.domain FROM kanban_projects p WHERE p.project=?1",
            rusqlite::params![project], |row| row.get(0),
        ).map_err(|_| KanbanError::NotFound(format!("project '{project}' not found")))?;

        for (i, tid) in order.iter().enumerate() {
            let position = (i + 1) as i64;
            let event = KanbanEvent::Reorder {
                ticket_id: tid.clone(),
                data: events::ReorderData { position },
                timestamp: now.clone(),
            };
            events::append_event(&self.vault_root, &domain, project, &event)?;
            conn.execute(
                "UPDATE kanban_items SET position=?1, updated_at=?2 WHERE ticket_id=?3",
                rusqlite::params![position, now, tid],
            )?;
        }

        let mut items: Vec<KanbanItem> = order.iter()
            .filter_map(|tid| self.get_item_with_conn(&conn, tid).ok())
            .collect();
        self.populate_children(&conn, &mut items)?;
        Ok(items)
    }

    // ---- Read path: SQLite only ----

    pub fn get_item(&self, ticket_id: &str) -> Result<KanbanItem, KanbanError> {
        self.fold_foreign_events_logged();
        let conn = self.conn()?;
        let mut item = self.get_item_with_conn(&conn, ticket_id)?;
        let mut items = vec![item];
        self.populate_children(&conn, &mut items)?;
        item = items.into_iter().next().ok_or_else(|| KanbanError::NotFound(ticket_id.into()))?;

        // Build activity feed from JSONL
        let domain_project = conn.query_row(
            "SELECT p.domain, i.project FROM kanban_items i JOIN kanban_projects p ON i.project = p.project WHERE i.ticket_id=?1",
            rusqlite::params![ticket_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        ).ok();

        if let Some((domain, project)) = domain_project {
            let all_events = events::read_events(&self.vault_root, &domain, &project);
            item.activity = all_events.iter()
                .filter(|e| e.ticket_id() == ticket_id)
                .map(|e| event_to_activity(e, None))
                .collect();

            // Fold grooming provenance from groom_* events (metadata, not notes).
            item.grooming = events::grooming_for_ticket(&all_events, ticket_id);
            // Surface a readable pointer inline: prefer the receipt's fields, but
            // fall back to the latest on-disk artifact (path by convention, plus
            // readiness/surfaced parsed from its header) so a manually-groomed
            // ticket — or one whose receipt hasn't landed — still shows readiness
            // and a path on the ticket.
            if let Some(path) = latest_grooming_artifact(&self.vault_root, &domain, &project, ticket_id) {
                let (readiness, surfaced) = parse_grooming_header(&self.vault_root, &path);
                match item.grooming.as_mut() {
                    Some(g) => {
                        if g.artifact_path.is_none() { g.artifact_path = Some(path); }
                        if g.readiness.is_none() { g.readiness = readiness; }
                        if g.surfaced.is_none() { g.surfaced = surfaced; }
                    }
                    None => {
                        // Artifact on disk with no groom events at all (manual run):
                        // grooming clearly happened and produced output.
                        item.grooming = Some(crate::kanban::events::Grooming {
                            status: "completed".into(),
                            requested_at: None,
                            requested_by: None,
                            reason: None,
                            completed_at: None,
                            failed_at: None,
                            readiness,
                            artifact_path: Some(path),
                            surfaced,
                            work_item_id: None,
                            cost_usd: None,
                            error: None,
                        });
                    }
                }
            }

            // Children activity: last 2 events per child
            if !item.children.is_empty() {
                let child_ids: Vec<&str> = item.children.iter().map(|c| c.ticket_id.as_str()).collect();
                let mut children_activity: Vec<ActivityEntry> = Vec::new();
                for child_id in &child_ids {
                    let child_events: Vec<ActivityEntry> = all_events.iter()
                        .filter(|e| e.ticket_id() == *child_id)
                        .map(|e| event_to_activity(e, Some(child_id)))
                        .collect();
                    let start = child_events.len().saturating_sub(2);
                    children_activity.extend_from_slice(&child_events[start..]);
                }
                children_activity.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
                item.children_activity = children_activity;
            }
        }

        Ok(item)
    }

    pub fn search(&self, query: &str, project: Option<&str>, domains: Option<&[String]>) -> Result<Vec<KanbanItem>, KanbanError> {
        self.fold_foreign_events_logged();
        let conn = self.conn()?;
        let use_domain = domains.map(|d| !d.is_empty()).unwrap_or(false);
        let from = if use_domain {
            "FROM kanban_items INNER JOIN kanban_projects p ON kanban_items.project = p.project"
        } else { "FROM kanban_items" };

        let mut conditions = vec!["(kanban_items.title LIKE ?1 OR kanban_items.description LIKE ?1 OR kanban_items.ticket_id LIKE ?1)".to_string()];
        let search_pat = format!("%{query}%");
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(search_pat)];
        let mut idx = 2;

        if use_domain {
            if let Some(dl) = domains {
                let ph: Vec<String> = dl.iter().map(|_| { let s = format!("?{idx}"); idx += 1; s }).collect();
                conditions.push(format!("p.domain IN ({})", ph.join(",")));
                for d in dl { params.push(Box::new(d.clone())); }
            }
        }
        if let Some(proj) = project {
            let group_members = self.resolve_group_members(proj);
            if group_members.is_empty() {
                conditions.push(format!("kanban_items.project=?{idx}"));
                params.push(Box::new(proj.to_string()));
            } else {
                let ph: Vec<String> = group_members.iter().map(|_| { let s = format!("?{idx}"); idx += 1; s }).collect();
                conditions.push(format!("kanban_items.project IN ({})", ph.join(",")));
                for m in &group_members { params.push(Box::new(m.clone())); }
            }
            let _ = idx;
        }

        let wh = format!("WHERE {}", conditions.join(" AND "));
        let sql = format!(
            "SELECT kanban_items.ticket_id, kanban_items.project, kanban_items.epic, kanban_items.parent, kanban_items.position, kanban_items.tags, kanban_items.title, kanban_items.description, kanban_items.status, kanban_items.priority, kanban_items.assignee, kanban_items.deadline, kanban_items.source, kanban_items.created_at, kanban_items.updated_at, kanban_items.completed_at, kanban_items.stage, kanban_items.waiting_on, kanban_items.waiting_summary, kanban_items.waiting_since {from} {wh} ORDER BY kanban_items.updated_at DESC LIMIT 20"
        );

        let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let items: Vec<KanbanItem> = stmt.query_map(refs.as_slice(), |row| {
            Ok(KanbanItem {
                ticket_id: row.get(0)?, project: row.get(1)?, group: None, epic: row.get(2)?,
                parent: row.get(3)?, position: row.get(4)?, children: vec![],
                tags: { let t: String = row.get::<_, String>(5).unwrap_or_else(|_| "[]".into()); serde_json::from_str(&t).unwrap_or_default() },
                title: row.get(6)?, description: row.get(7)?, status: row.get(8)?, priority: row.get(9)?,
                assignee: row.get(10)?, deadline: row.get(11)?, source: row.get(12)?,
                created_at: row.get(13)?, updated_at: row.get(14)?, completed_at: row.get(15)?,
                stage: row.get(16)?, waiting_on: row.get(17)?, waiting_summary: row.get(18)?, waiting_since: row.get(19)?,
                notes: vec![], attachments: vec![], activity: vec![], children_activity: vec![], grooming: None,
            })
        })?.collect::<Result<_, _>>()?;

        let mut items: Vec<KanbanItem> = items.into_iter().map(|mut item| {
            item.group = self.project_to_group.get(&item.project).cloned();
            Ok(item)
        }).collect::<Result<_, KanbanError>>()?;
        self.populate_children(&conn, &mut items)?;
        Ok(items)
    }

    pub fn list(
        &self, project: Option<&str>, status: Option<&str>, priority: Option<&str>,
        assignee: Option<&str>, epic: Option<&str>, tag: Option<&str>, include_done: bool, domains: Option<&[String]>,
    ) -> Result<Vec<KanbanItem>, KanbanError> {
        self.fold_foreign_events_logged();
        let conn = self.conn()?;
        let mut conditions: Vec<String> = vec![];
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![];
        let mut idx = 1usize;

        let use_domain = domains.map(|d| !d.is_empty()).unwrap_or(false);
        let from = if use_domain {
            "FROM kanban_items INNER JOIN kanban_projects p ON kanban_items.project = p.project"
        } else { "FROM kanban_items" };

        if use_domain {
            if let Some(dl) = domains {
                let ph: Vec<String> = dl.iter().map(|_| { let s = format!("?{idx}"); idx += 1; s }).collect();
                conditions.push(format!("p.domain IN ({})", ph.join(",")));
                for d in dl { params.push(Box::new(d.clone())); }
            }
        }
        if !include_done { conditions.push(format!("kanban_items.status != ?{idx}")); params.push(Box::new("done".to_string())); idx += 1; }
        if let Some(v) = project {
            let group_members = self.resolve_group_members(v);
            if group_members.is_empty() {
                conditions.push(format!("kanban_items.project=?{idx}"));
                params.push(Box::new(v.to_string()));
                idx += 1;
            } else {
                let ph: Vec<String> = group_members.iter().map(|_| { let s = format!("?{idx}"); idx += 1; s }).collect();
                conditions.push(format!("kanban_items.project IN ({})", ph.join(",")));
                for m in &group_members { params.push(Box::new(m.clone())); }
            }
        }
        if let Some(v) = status { conditions.push(format!("kanban_items.status=?{idx}")); params.push(Box::new(v.to_string())); idx += 1; }
        if let Some(v) = priority { conditions.push(format!("kanban_items.priority=?{idx}")); params.push(Box::new(v.to_string())); idx += 1; }
        if let Some(v) = assignee { conditions.push(format!("kanban_items.assignee=?{idx}")); params.push(Box::new(v.to_string())); idx += 1; }
        if let Some(v) = epic { conditions.push(format!("kanban_items.epic=?{idx}")); params.push(Box::new(v.to_string())); idx += 1; }
        if let Some(v) = tag { conditions.push(format!("kanban_items.tags LIKE ?{idx}")); params.push(Box::new(format!("%\"{v}\"%"))); let _ = idx; }

        let wh = if conditions.is_empty() { String::new() } else { format!("WHERE {}", conditions.join(" AND ")) };
        let sql = format!(
            "SELECT kanban_items.ticket_id, kanban_items.project, kanban_items.epic, kanban_items.parent, kanban_items.position, kanban_items.tags, kanban_items.title, kanban_items.description, kanban_items.status, kanban_items.priority, kanban_items.assignee, kanban_items.deadline, kanban_items.source, kanban_items.created_at, kanban_items.updated_at, kanban_items.completed_at, kanban_items.stage, kanban_items.waiting_on, kanban_items.waiting_summary, kanban_items.waiting_since {from} {wh} ORDER BY CASE kanban_items.priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'medium' THEN 2 WHEN 'low' THEN 3 ELSE 4 END, kanban_items.updated_at DESC"
        );

        let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let items: Vec<KanbanItem> = stmt.query_map(refs.as_slice(), |row| {
            Ok(KanbanItem {
                ticket_id: row.get(0)?, project: row.get(1)?, group: None, epic: row.get(2)?,
                parent: row.get(3)?, position: row.get(4)?, children: vec![],
                tags: { let t: String = row.get::<_, String>(5).unwrap_or_else(|_| "[]".into()); serde_json::from_str(&t).unwrap_or_default() },
                title: row.get(6)?, description: row.get(7)?, status: row.get(8)?, priority: row.get(9)?,
                assignee: row.get(10)?, deadline: row.get(11)?, source: row.get(12)?,
                created_at: row.get(13)?, updated_at: row.get(14)?, completed_at: row.get(15)?,
                stage: row.get(16)?, waiting_on: row.get(17)?, waiting_summary: row.get(18)?, waiting_since: row.get(19)?,
                notes: vec![], attachments: vec![], activity: vec![], children_activity: vec![], grooming: None,
            })
        })?.collect::<Result<_, _>>()?;

        let mut items: Vec<KanbanItem> = items.into_iter().map(|mut item| {
            item.group = self.project_to_group.get(&item.project).cloned();
            item.notes = self.load_notes(&conn, &item.ticket_id)?;
            item.attachments = self.load_attachments(&conn, &item.ticket_id)?;
            Ok(item)
        }).collect::<Result<_, KanbanError>>()?;
        self.populate_children(&conn, &mut items)?;
        Ok(items)
    }

    pub fn list_metadata(
        &self, project: Option<&str>, include_done: bool,
    ) -> Result<Vec<KanbanItem>, KanbanError> {
        self.fold_foreign_events_logged();
        let conn = self.conn()?;
        let mut conditions: Vec<String> = vec![];
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![];
        let mut idx = 1usize;

        if !include_done { conditions.push(format!("status != ?{idx}")); params.push(Box::new("done".to_string())); idx += 1; }
        if let Some(v) = project {
            let group_members = self.resolve_group_members(v);
            if group_members.is_empty() {
                conditions.push(format!("project=?{idx}"));
                params.push(Box::new(v.to_string()));
                idx += 1;
            } else {
                let ph: Vec<String> = group_members.iter().map(|_| { let s = format!("?{idx}"); idx += 1; s }).collect();
                conditions.push(format!("project IN ({})", ph.join(",")));
                for m in &group_members { params.push(Box::new(m.clone())); }
            }
        }
        let _ = idx;

        let wh = if conditions.is_empty() { String::new() } else { format!("WHERE {}", conditions.join(" AND ")) };
        let sql = format!(
            "SELECT ticket_id, project, epic, parent, position, tags, title, NULL, status, priority, assignee, deadline, source, created_at, updated_at, completed_at, stage, waiting_on, waiting_summary, waiting_since FROM kanban_items {wh} ORDER BY CASE priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'medium' THEN 2 WHEN 'low' THEN 3 ELSE 4 END, updated_at DESC"
        );

        let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let items: Vec<KanbanItem> = stmt.query_map(refs.as_slice(), |row| {
            Ok(KanbanItem {
                ticket_id: row.get(0)?, project: row.get(1)?, group: None, epic: row.get(2)?,
                parent: row.get(3)?, position: row.get(4)?, children: vec![],
                tags: { let t: String = row.get::<_, String>(5).unwrap_or_else(|_| "[]".into()); serde_json::from_str(&t).unwrap_or_default() },
                title: row.get(6)?, description: row.get(7)?, status: row.get(8)?, priority: row.get(9)?,
                assignee: row.get(10)?, deadline: row.get(11)?, source: row.get(12)?,
                created_at: row.get(13)?, updated_at: row.get(14)?, completed_at: row.get(15)?,
                stage: row.get(16)?, waiting_on: row.get(17)?, waiting_summary: row.get(18)?, waiting_since: row.get(19)?,
                notes: vec![], attachments: vec![], activity: vec![], children_activity: vec![], grooming: None,
            })
        })?.collect::<Result<_, _>>()?;

        let mut items: Vec<KanbanItem> = items.into_iter().map(|mut item| {
            item.group = self.project_to_group.get(&item.project).cloned();
            Ok(item)
        }).collect::<Result<_, KanbanError>>()?;
        self.populate_children(&conn, &mut items)?;
        Ok(items)
    }

    pub fn list_with_notes(
        &self, project: &str, ticket_ids: &[String],
    ) -> Result<Vec<KanbanItem>, KanbanError> {
        self.fold_foreign_events_logged();
        if ticket_ids.is_empty() { return Ok(vec![]); }
        let conn = self.conn()?;
        let ph: Vec<String> = (1..=ticket_ids.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT ticket_id, project, epic, parent, position, tags, title, description, status, priority, assignee, deadline, source, created_at, updated_at, completed_at, stage, waiting_on, waiting_summary, waiting_since FROM kanban_items WHERE ticket_id IN ({}) ORDER BY updated_at DESC",
            ph.join(",")
        );
        let params: Vec<&dyn rusqlite::types::ToSql> = ticket_ids.iter().map(|s| s as &dyn rusqlite::types::ToSql).collect();
        let mut stmt = conn.prepare(&sql)?;
        let items: Vec<KanbanItem> = stmt.query_map(params.as_slice(), |row| {
            Ok(KanbanItem {
                ticket_id: row.get(0)?, project: row.get(1)?, group: None, epic: row.get(2)?,
                parent: row.get(3)?, position: row.get(4)?, children: vec![],
                tags: { let t: String = row.get::<_, String>(5).unwrap_or_else(|_| "[]".into()); serde_json::from_str(&t).unwrap_or_default() },
                title: row.get(6)?, description: row.get(7)?, status: row.get(8)?, priority: row.get(9)?,
                assignee: row.get(10)?, deadline: row.get(11)?, source: row.get(12)?,
                created_at: row.get(13)?, updated_at: row.get(14)?, completed_at: row.get(15)?,
                stage: row.get(16)?, waiting_on: row.get(17)?, waiting_summary: row.get(18)?, waiting_since: row.get(19)?,
                notes: vec![], attachments: vec![], activity: vec![], children_activity: vec![], grooming: None,
            })
        })?.collect::<Result<_, _>>()?;

        let items: Vec<KanbanItem> = items.into_iter().map(|mut item| {
            item.group = self.project_to_group.get(&item.project).cloned();
            item.notes = self.load_notes(&conn, &item.ticket_id)?;
            Ok(item)
        }).collect::<Result<_, KanbanError>>()?;
        let _ = project;
        Ok(items)
    }

    pub fn query(
        &self, question: &str, queries: &HashMap<String, String>,
        project: Option<&str>, domains: Option<&[String]>,
    ) -> Result<Vec<KanbanItem>, KanbanError> {
        self.fold_foreign_events_logged();
        let named_where = queries.get(question).ok_or_else(|| {
            let mut names: Vec<&str> = queries.keys().map(String::as_str).collect();
            names.sort();
            KanbanError::InvalidInput(format!("unknown query '{question}'; available: {}", names.join(", ")))
        })?;

        let use_domain = domains.map(|d| !d.is_empty()).unwrap_or(false);
        let from = if use_domain {
            "FROM kanban_items INNER JOIN kanban_projects p ON kanban_items.project = p.project"
        } else { "FROM kanban_items" };

        let mut extra: Vec<String> = vec![];
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![];
        let mut idx = 1;

        if use_domain { if let Some(dl) = domains {
            let ph: Vec<String> = dl.iter().map(|_| { let s = format!("?{idx}"); idx += 1; s }).collect();
            extra.push(format!("p.domain IN ({})", ph.join(",")));
            for d in dl { params.push(Box::new(d.clone())); }
        }}
        if let Some(p) = project { extra.push(format!("kanban_items.project=?{idx}")); params.push(Box::new(p.to_string())); let _ = idx; }

        let wh = if extra.is_empty() { format!("WHERE {named_where}") } else { format!("WHERE ({named_where}) AND {}", extra.join(" AND ")) };
        let sql = format!(
            "SELECT kanban_items.ticket_id, kanban_items.project, kanban_items.epic, kanban_items.parent, kanban_items.position, kanban_items.tags, kanban_items.title, kanban_items.description, kanban_items.status, kanban_items.priority, kanban_items.assignee, kanban_items.deadline, kanban_items.source, kanban_items.created_at, kanban_items.updated_at, kanban_items.completed_at, kanban_items.stage, kanban_items.waiting_on, kanban_items.waiting_summary, kanban_items.waiting_since {from} {wh} ORDER BY kanban_items.updated_at DESC"
        );

        let conn = self.conn()?;
        let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let items: Vec<KanbanItem> = stmt.query_map(refs.as_slice(), |row| {
            Ok(KanbanItem {
                ticket_id: row.get(0)?, project: row.get(1)?, group: None, epic: row.get(2)?,
                parent: row.get(3)?, position: row.get(4)?, children: vec![],
                tags: { let t: String = row.get::<_, String>(5).unwrap_or_else(|_| "[]".into()); serde_json::from_str(&t).unwrap_or_default() },
                title: row.get(6)?, description: row.get(7)?, status: row.get(8)?, priority: row.get(9)?,
                assignee: row.get(10)?, deadline: row.get(11)?, source: row.get(12)?,
                created_at: row.get(13)?, updated_at: row.get(14)?, completed_at: row.get(15)?,
                stage: row.get(16)?, waiting_on: row.get(17)?, waiting_summary: row.get(18)?, waiting_since: row.get(19)?,
                notes: vec![], attachments: vec![], activity: vec![], children_activity: vec![], grooming: None,
            })
        })?.collect::<Result<_, _>>()?;

        let mut items: Vec<KanbanItem> = items.into_iter().map(|mut item| {
            item.group = self.project_to_group.get(&item.project).cloned();
            item.notes = self.load_notes(&conn, &item.ticket_id)?;
            item.attachments = self.load_attachments(&conn, &item.ticket_id)?;
            Ok(item)
        }).collect::<Result<_, KanbanError>>()?;
        self.populate_children(&conn, &mut items)?;
        Ok(items)
    }

    pub fn validate_queries(&self, queries: &HashMap<String, String>) -> Result<(), KanbanError> {
        let conn = self.conn()?;
        for (name, wh) in queries {
            conn.prepare(&format!("SELECT * FROM kanban_items WHERE {wh}")).map_err(|e| {
                KanbanError::InvalidInput(format!("invalid query '{name}': {e} (WHERE clause: {wh})"))
            })?;
        }
        Ok(())
    }

    // ---- Rebuild SQLite from JSONL ----

    pub fn rebuild_from_jsonl(&self) -> Result<(), KanbanError> {
        let files = events::scan_jsonl_paths(&self.vault_root);
        let conn = self.conn()?;

        conn.execute_batch(
            "DROP TABLE IF EXISTS kanban_attachments;
             DROP TABLE IF EXISTS kanban_notes;
             DROP TABLE IF EXISTS kanban_items;
             DROP TABLE IF EXISTS kanban_projects;
             DELETE FROM kanban_fold_state;"
        )?;
        // Recreate with current schema
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS kanban_projects (
                project TEXT PRIMARY KEY, prefix TEXT UNIQUE NOT NULL,
                domain TEXT NOT NULL, next_id INTEGER NOT NULL DEFAULT 1
            );
            CREATE TABLE IF NOT EXISTS kanban_items (
                ticket_id TEXT PRIMARY KEY, project TEXT NOT NULL, title TEXT NOT NULL,
                description TEXT, status TEXT NOT NULL DEFAULT 'backlog',
                priority TEXT NOT NULL DEFAULT 'medium', assignee TEXT, deadline TEXT,
                source TEXT, epic TEXT, parent TEXT, position INTEGER, tags TEXT DEFAULT '[]',
                stage TEXT, waiting_on TEXT, waiting_summary TEXT, waiting_since TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL, completed_at TEXT
            );
            CREATE TABLE IF NOT EXISTS kanban_notes (
                id INTEGER PRIMARY KEY AUTOINCREMENT, ticket_id TEXT NOT NULL,
                text TEXT NOT NULL, author TEXT, created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS kanban_attachments (
                attachment_id TEXT PRIMARY KEY, ticket_id TEXT NOT NULL,
                filename TEXT NOT NULL, mime_type TEXT NOT NULL, size INTEGER NOT NULL,
                storage_path TEXT NOT NULL, created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_kanban_items_project ON kanban_items(project);
            CREATE INDEX IF NOT EXISTS idx_kanban_items_status ON kanban_items(status);
            CREATE INDEX IF NOT EXISTS idx_kanban_notes_ticket ON kanban_notes(ticket_id);
            CREATE INDEX IF NOT EXISTS idx_kanban_attachments_ticket ON kanban_attachments(ticket_id);"
        )?;

        for (domain, project, path) in &files {
            let evts = events::read_events_from_path_warn(path);
            let items = events::materialize(domain, &evts);
            for item in &items {
                self.insert_materialized_item(&conn, item)?;
            }
            Self::record_fold_watermark(&conn, domain, project, path)?;
        }

        Ok(())
    }

    /// Insert one replay-materialized item (plus its notes/attachments and
    /// project registration) into the SQLite cache. Shared by the full rebuild
    /// and the incremental foreign-event fold so both produce identical rows.
    fn insert_materialized_item(&self, conn: &Connection, item: &events::MaterializedItem) -> Result<(), KanbanError> {
        // Derive prefix from ticket_id
        if let Some(dash) = item.ticket_id.find('-') {
            let prefix = &item.ticket_id[..dash];
            let num: i64 = item.ticket_id[dash + 1..].parse().unwrap_or(1);
            self.upsert_project(conn, &item.project, prefix, &item.domain, num + 1)?;
        }

        let tags_json = serde_json::to_string(&item.tags).unwrap_or_else(|_| "[]".into());
        conn.execute(
            "INSERT OR REPLACE INTO kanban_items (ticket_id, project, title, description, status, priority, assignee, deadline, source, epic, parent, position, tags, stage, waiting_on, waiting_summary, waiting_since, created_at, updated_at, completed_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
            rusqlite::params![item.ticket_id, item.project, item.title, item.description, item.status, item.priority, item.assignee, item.deadline, item.source, item.epic, item.parent, item.position, tags_json, item.stage, item.waiting_on, item.waiting_summary, item.waiting_since, item.created_at, item.updated_at, item.completed_at],
        )?;

        for note in &item.notes {
            conn.execute(
                "INSERT INTO kanban_notes (ticket_id, text, author, created_at) VALUES (?1,?2,?3,?4)",
                rusqlite::params![item.ticket_id, note.text, note.author, note.created_at],
            )?;
        }
        for att in &item.attachments {
            conn.execute(
                "INSERT OR REPLACE INTO kanban_attachments (attachment_id, ticket_id, filename, mime_type, size, storage_path, created_at) VALUES (?1,?2,?3,?4,?5,?6,?7)",
                rusqlite::params![att.attachment_id, item.ticket_id, att.filename, att.mime_type, att.size, att.storage_path, att.created_at],
            )?;
        }
        Ok(())
    }

    /// (size, mtime_ns) identity of a jsonl file, used as the fold watermark.
    /// Appends grow the size; vault-sync file replacement bumps the mtime.
    fn file_watermark(path: &Path) -> Option<(i64, i64)> {
        let meta = std::fs::metadata(path).ok()?;
        let mtime_ns = meta.modified().ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        Some((meta.len() as i64, mtime_ns))
    }

    fn record_fold_watermark(conn: &Connection, domain: &str, project: &str, path: &Path) -> Result<(), KanbanError> {
        if let Some((byte_len, mtime_ns)) = Self::file_watermark(path) {
            conn.execute(
                "INSERT OR REPLACE INTO kanban_fold_state (path, byte_len, mtime_ns, folded_at) VALUES (?1,?2,?3,?4)",
                rusqlite::params![format!("{domain}/{project}"), byte_len, mtime_ns, chrono::Utc::now().to_rfc3339()],
            )?;
        }
        Ok(())
    }

    // ---- Fold foreign events (multi-machine sync) ----

    /// Fold events appended to kanban.jsonl files by OTHER writers — other
    /// machines via vault sync, or other processes on this one — into the
    /// SQLite cache. The jsonl is truth; the db is a derived index. Without
    /// this, a long-running reader (the MCP server) only ever sees its own
    /// writes: `open()` rebuilds once, and every later foreign event is
    /// visible in the activity feed (read from jsonl) but missing from
    /// kanban_notes/kanban_items.
    ///
    /// Cheap when quiescent: one stat per project file, compared against a
    /// per-project (byte_len, mtime_ns) watermark in kanban_fold_state. When a
    /// watermark differs, that project's rows are atomically re-materialized
    /// from a full replay of its jsonl. Idempotency and dedupe are structural:
    /// every row — locally written or foreign — is regenerated from the same
    /// event stream, so repeated folds can never double-insert.
    ///
    /// Returns the number of projects re-folded.
    pub fn fold_foreign_events(&self) -> Result<usize, KanbanError> {
        let files = events::scan_jsonl_paths(&self.vault_root);

        // Prune boards that vanished from disk (deleted, or quarantined by the
        // recovery command which renames kanban.jsonl → kanban.jsonl.quarantine-*).
        // A board still cached but no longer on disk must have its materialized
        // rows dropped immediately, or the cache keeps serving a board the
        // source of truth no longer has.
        let live: std::collections::HashSet<String> = files
            .iter()
            .map(|(domain, project, _)| format!("{domain}/{project}"))
            .collect();
        self.prune_vanished_boards(&live)?;

        let mut folded = 0usize;
        for (domain, project, path) in files {
            let Some(current) = Self::file_watermark(&path) else { continue };
            {
                let conn = self.conn()?;
                let stored: Option<(i64, i64)> = conn.query_row(
                    "SELECT byte_len, mtime_ns FROM kanban_fold_state WHERE path=?1",
                    rusqlite::params![format!("{domain}/{project}")],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                ).optional()?;
                if stored == Some(current) { continue; }
            }

            // Replay this project's full event stream (bad lines skipped with
            // a warning, never aborting) and atomically swap its rows.
            let evts = events::read_events_from_path_warn(&path);
            let items = events::materialize(&domain, &evts);

            {
                let conn = self.conn()?;
                Self::ensure_project_domain(&conn, &project, &domain)?;
            }

            let mut conn = self.conn()?;
            let tx = conn.transaction()?;
            tx.execute(
                "DELETE FROM kanban_notes WHERE ticket_id IN (SELECT ticket_id FROM kanban_items WHERE project=?1)",
                rusqlite::params![project],
            )?;
            tx.execute(
                "DELETE FROM kanban_attachments WHERE ticket_id IN (SELECT ticket_id FROM kanban_items WHERE project=?1)",
                rusqlite::params![project],
            )?;
            tx.execute("DELETE FROM kanban_items WHERE project=?1", rusqlite::params![project])?;
            for item in &items {
                self.insert_materialized_item(&tx, item)?;
            }
            Self::record_fold_watermark(&tx, &domain, &project, &path)?;
            tx.commit()?;
            folded += 1;
        }
        Ok(folded)
    }

    /// Drop cached rows for any board recorded in kanban_fold_state whose
    /// `<domain>/<project>/kanban.jsonl` is no longer on disk. `live` is the set
    /// of `domain/project` keys that DO currently exist. Runs at the top of
    /// every fold, so a delete/quarantine is reflected on the very next read.
    fn prune_vanished_boards(&self, live: &std::collections::HashSet<String>) -> Result<(), KanbanError> {
        let mut conn = self.conn()?;
        let stale: Vec<(String, String)> = {
            let mut stmt = conn.prepare("SELECT path FROM kanban_fold_state")?;
            let keys: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<_, _>>()?;
            keys.into_iter()
                .filter(|key| !live.contains(key))
                .filter_map(|key| key.split_once('/').map(|(_d, p)| (key.clone(), p.to_string())))
                .collect()
        };
        if stale.is_empty() {
            return Ok(());
        }
        let tx = conn.transaction()?;
        for (key, project) in &stale {
            tx.execute(
                "DELETE FROM kanban_notes WHERE ticket_id IN (SELECT ticket_id FROM kanban_items WHERE project=?1)",
                rusqlite::params![project],
            )?;
            tx.execute(
                "DELETE FROM kanban_attachments WHERE ticket_id IN (SELECT ticket_id FROM kanban_items WHERE project=?1)",
                rusqlite::params![project],
            )?;
            tx.execute("DELETE FROM kanban_items WHERE project=?1", rusqlite::params![project])?;
            tx.execute("DELETE FROM kanban_projects WHERE project=?1", rusqlite::params![project])?;
            tx.execute("DELETE FROM kanban_fold_state WHERE path=?1", rusqlite::params![key])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Recovery command (SW-68 acceptance): scan every board on disk for
    /// identity collisions and quarantine the offenders. Two collision classes:
    ///   1. cross-domain slug shadow — the same `project` slug in two domains
    ///      (the work/switchboard vs personal/switchboard fault);
    ///   2. duplicate ticket_ids within a single board's create events.
    /// For (1), the newer board (by first create timestamp) is quarantined,
    /// preserving the original. Quarantine renames kanban.jsonl →
    /// kanban.jsonl.quarantine-<UTC date>, so the source of truth is retained
    /// but no longer folded. Returns a report of what was found and moved.
    /// `dry_run` reports without moving anything.
    pub fn recover_collisions(&self, dry_run: bool) -> Result<CollisionReport, KanbanError> {
        let files = events::scan_jsonl_paths(&self.vault_root);
        let mut report = CollisionReport::default();

        // Class 1: cross-domain slug shadowing. Group paths by project slug.
        let mut by_slug: HashMap<String, Vec<(String, PathBuf)>> = HashMap::new();
        for (domain, project, path) in &files {
            by_slug.entry(project.clone()).or_default().push((domain.clone(), path.clone()));
        }
        for (slug, mut boards) in by_slug {
            if boards.len() < 2 {
                continue;
            }
            // Keep the oldest board (earliest first-create timestamp); quarantine
            // the rest. Sort ascending so index 0 is the keeper.
            boards.sort_by_key(|(_d, path)| Self::first_create_ts(path));
            let keeper_domain = boards[0].0.clone();
            for (domain, path) in boards.into_iter().skip(1) {
                report.slug_collisions.push(format!(
                    "project '{slug}' exists in domains '{keeper_domain}' (kept) and '{domain}' (quarantined)"
                ));
                if !dry_run {
                    Self::quarantine_board(&path)?;
                    report.quarantined.push(path.display().to_string());
                }
            }
        }

        // Class 2: duplicate ticket_ids inside a single board.
        for (_domain, project, path) in &files {
            let evts = events::read_events_from_path_warn(path);
            let mut seen = std::collections::HashSet::new();
            for evt in &evts {
                if let KanbanEvent::Create { ticket_id, .. } = evt
                    && !seen.insert(ticket_id.clone())
                {
                    report.duplicate_ids.push(format!("{project}: duplicate create for {ticket_id}"));
                }
            }
        }

        // Reflect the quarantines in the cache immediately.
        if !dry_run && !report.quarantined.is_empty() {
            self.fold_foreign_events()?;
        }
        Ok(report)
    }

    /// Earliest create-event timestamp on a board, used to pick the keeper in a
    /// cross-domain collision. Missing/empty → a max sentinel so it sorts last.
    fn first_create_ts(path: &Path) -> String {
        events::read_events_from_path_warn(path)
            .into_iter()
            .filter_map(|e| match e {
                KanbanEvent::Create { timestamp, .. } => Some(timestamp),
                _ => None,
            })
            .min()
            .unwrap_or_else(|| "9999".to_string())
    }

    /// Rename a board's kanban.jsonl to a dated quarantine sidecar. The source
    /// is preserved (never deleted), but scan_jsonl_paths no longer sees it, so
    /// the next fold prunes its rows from the cache.
    fn quarantine_board(path: &Path) -> Result<(), KanbanError> {
        let date = chrono::Utc::now().format("%Y-%m-%d");
        let target = path.with_file_name(format!("kanban.jsonl.quarantine-{date}"));
        std::fs::rename(path, &target)?;
        Ok(())
    }

    /// Read/write-path entry point: fold, but never let a fold failure take
    /// down the operation — the db just stays one sync behind.
    fn fold_foreign_events_logged(&self) {
        if let Err(e) = self.fold_foreign_events() {
            eprintln!("wardwell: kanban fold warning (non-fatal): {e}");
        }
    }

    // ---- Internal helpers ----

    fn populate_children(&self, conn: &Connection, items: &mut [KanbanItem]) -> Result<(), KanbanError> {
        let ids: Vec<String> = items.iter().map(|i| i.ticket_id.clone()).collect();
        if ids.is_empty() { return Ok(()); }
        let ph: Vec<String> = (1..=ids.len()).map(|i| format!("?{i}")).collect();
        let sql = format!("SELECT ticket_id, title, status, parent FROM kanban_items WHERE parent IN ({}) ORDER BY created_at", ph.join(","));
        let params: Vec<&dyn rusqlite::types::ToSql> = ids.iter().map(|s| s as &dyn rusqlite::types::ToSql).collect();
        let mut stmt = conn.prepare(&sql)?;
        let children: Vec<(String, String, String, String)> = stmt.query_map(params.as_slice(), |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get::<_, String>(3)?))
        })?.filter_map(|r| r.ok()).collect();
        for (child_id, title, status, parent_id) in children {
            if let Some(parent) = items.iter_mut().find(|i| i.ticket_id == parent_id) {
                parent.children.push(KanbanChild { ticket_id: child_id, title, status });
            }
        }
        Ok(())
    }

    fn load_notes(&self, conn: &Connection, ticket_id: &str) -> Result<Vec<KanbanNote>, KanbanError> {
        // Newest first; id tiebreak keeps same-timestamp notes stable. No LIMIT:
        // full note bodies are the point — agents must be able to read what the
        // activity feed only headlines.
        let mut stmt = conn.prepare("SELECT id, text, author, created_at FROM kanban_notes WHERE ticket_id=?1 ORDER BY created_at DESC, id DESC")?;
        let notes = stmt.query_map(rusqlite::params![ticket_id], |row| {
            Ok(KanbanNote { id: row.get(0)?, text: row.get(1)?, author: row.get(2)?, created_at: row.get(3)? })
        })?.collect::<Result<_, _>>()?;
        Ok(notes)
    }

    fn load_attachments(&self, conn: &Connection, ticket_id: &str) -> Result<Vec<KanbanAttachment>, KanbanError> {
        let mut stmt = conn.prepare("SELECT attachment_id, filename, mime_type, size, storage_path, created_at FROM kanban_attachments WHERE ticket_id=?1 ORDER BY created_at")?;
        let atts = stmt.query_map(rusqlite::params![ticket_id], |row| {
            let sp: String = row.get(4)?;
            Ok(KanbanAttachment {
                attachment_id: row.get(0)?, filename: row.get(1)?, mime_type: row.get(2)?,
                size: row.get(3)?, read_path: sp.clone(), storage_path: sp, created_at: row.get(5)?,
            })
        })?.collect::<Result<_, _>>()?;
        Ok(atts)
    }

    /// Attach content or an existing file to a ticket.
    /// If content is provided: writes to {domain}/{project}/docs/{filename}, then records pointer.
    /// If content is None: registers an existing vault-relative file path as a pointer.
    pub fn attach_file(&self, ticket_id: &str, filename: &str, content: Option<&str>, vault_path: Option<&str>) -> Result<KanbanAttachment, KanbanError> {
        self.fold_foreign_events_logged();
        let conn = self.conn()?;
        let now = chrono::Utc::now().to_rfc3339();

        let (_status, project, domain) = self.get_item_context(&conn, ticket_id)?;

        let (resolved_filename, resolved_path, file_size) = if let Some(text) = content {
            if text.is_empty() {
                return Err(KanbanError::InvalidInput("content is empty".into()));
            }
            let fname = if filename.starts_with(ticket_id) { filename.to_string() } else { format!("{ticket_id}-{filename}") };
            let docs_dir = self.vault_root.join(&domain).join(&project).join("docs");
            std::fs::create_dir_all(&docs_dir)?;
            let dest = docs_dir.join(&fname);
            std::fs::write(&dest, text)?;
            let vault_rel = format!("{domain}/{project}/docs/{fname}");
            (fname, vault_rel, text.len() as u64)
        } else if let Some(vp) = vault_path {
            let expected_prefix = format!("{domain}/{project}/docs/");
            if !vp.starts_with(&expected_prefix) {
                return Err(KanbanError::InvalidInput(format!(
                    "file must be in {expected_prefix}. Pass content instead, or write the file there first."
                )));
            }
            let full_path = self.vault_root.join(vp);
            if !full_path.exists() {
                return Err(KanbanError::InvalidInput(format!(
                    "file not found at {vp}. Use content mode instead (pass text+title), or write the file to the vault first."
                )));
            }
            let size = std::fs::metadata(&full_path).map(|m| m.len()).unwrap_or(0);
            if size == 0 {
                return Err(KanbanError::InvalidInput(format!(
                    "file at {vp} is 0 bytes (likely an iCloud placeholder). Use content mode instead: pass text+title to write and attach in one call."
                )));
            }
            let fname = Path::new(vp).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_else(|| "unnamed".into());
            (fname, vp.to_string(), size)
        } else {
            return Err(KanbanError::InvalidInput("provide either 'content' (to write and attach) or 'file_path' (to attach an existing vault file)".into()));
        };

        let mime_type = mime_from_ext(&resolved_filename);
        let attachment_id = uuid::Uuid::new_v4().to_string();

        let event = KanbanEvent::Attach {
            ticket_id: ticket_id.into(), attachment_id: attachment_id.clone(),
            filename: resolved_filename.clone(), mime_type: mime_type.clone(),
            size: file_size, storage_path: resolved_path.clone(), timestamp: now.clone(),
        };
        events::append_event(&self.vault_root, &domain, &project, &event)?;

        conn.execute(
            "INSERT INTO kanban_attachments (attachment_id, ticket_id, filename, mime_type, size, storage_path, created_at) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![attachment_id, ticket_id, resolved_filename, mime_type, file_size as i64, resolved_path, now],
        )?;
        conn.execute("UPDATE kanban_items SET updated_at=?1 WHERE ticket_id=?2", rusqlite::params![now, ticket_id])?;

        Ok(KanbanAttachment {
            attachment_id, filename: resolved_filename, mime_type, size: file_size,
            read_path: resolved_path.clone(), storage_path: resolved_path, created_at: now,
        })
    }

    /// Remove an attachment pointer from a ticket. Does not delete the file.
    pub fn detach_file(&self, ticket_id: &str, attachment_id: &str) -> Result<(), KanbanError> {
        self.fold_foreign_events_logged();
        let conn = self.conn()?;
        let now = chrono::Utc::now().to_rfc3339();

        let (_status, project, domain) = self.get_item_context(&conn, ticket_id)?;

        let _storage_path: String = conn.query_row(
            "SELECT storage_path FROM kanban_attachments WHERE attachment_id=?1 AND ticket_id=?2",
            rusqlite::params![attachment_id, ticket_id], |row| row.get(0),
        ).optional()?.ok_or_else(|| KanbanError::NotFound(format!("attachment '{attachment_id}' not found on ticket '{ticket_id}'")))?;

        let event = KanbanEvent::Detach {
            ticket_id: ticket_id.into(), attachment_id: attachment_id.into(), timestamp: now.clone(),
        };
        events::append_event(&self.vault_root, &domain, &project, &event)?;

        conn.execute("DELETE FROM kanban_attachments WHERE attachment_id=?1", rusqlite::params![attachment_id])?;
        conn.execute("UPDATE kanban_items SET updated_at=?1 WHERE ticket_id=?2", rusqlite::params![now, ticket_id])?;

        Ok(())
    }

    fn get_item_with_conn(&self, conn: &Connection, ticket_id: &str) -> Result<KanbanItem, KanbanError> {
        let item: Option<KanbanItem> = conn.query_row(
            "SELECT ticket_id, project, epic, parent, position, tags, title, description, status, priority, assignee, deadline, source, created_at, updated_at, completed_at, stage, waiting_on, waiting_summary, waiting_since FROM kanban_items WHERE ticket_id=?1",
            rusqlite::params![ticket_id],
            |row| Ok(KanbanItem {
                ticket_id: row.get(0)?, project: row.get(1)?, group: None, epic: row.get(2)?,
                parent: row.get(3)?, position: row.get(4)?, children: vec![],
                tags: { let t: String = row.get::<_, String>(5).unwrap_or_else(|_| "[]".into()); serde_json::from_str(&t).unwrap_or_default() },
                title: row.get(6)?, description: row.get(7)?, status: row.get(8)?, priority: row.get(9)?,
                assignee: row.get(10)?, deadline: row.get(11)?, source: row.get(12)?,
                created_at: row.get(13)?, updated_at: row.get(14)?, completed_at: row.get(15)?,
                stage: row.get(16)?, waiting_on: row.get(17)?, waiting_summary: row.get(18)?, waiting_since: row.get(19)?,
                notes: vec![], attachments: vec![], activity: vec![], children_activity: vec![], grooming: None,
            }),
        ).optional()?;
        let mut item = item.ok_or_else(|| KanbanError::NotFound(format!("ticket '{ticket_id}' not found")))?;
        item.group = self.project_to_group.get(&item.project).cloned();
        item.notes = self.load_notes(conn, ticket_id)?;
        item.attachments = self.load_attachments(conn, ticket_id)?;
        Ok(item)
    }

    fn get_item_context(&self, conn: &Connection, ticket_id: &str) -> Result<(String, String, String), KanbanError> {
        conn.query_row(
            "SELECT i.status, i.project, p.domain FROM kanban_items i JOIN kanban_projects p ON i.project = p.project WHERE i.ticket_id=?1",
            rusqlite::params![ticket_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?.ok_or_else(|| KanbanError::NotFound(format!("ticket '{ticket_id}' not found")))
    }

    /// Snapshot of config-derived prefixes, so the allocator (which runs inside
    /// a held transaction and can't borrow `self.config`) sees the same mapping.
    /// Currently empty — prefix config is threaded through `create_item`'s
    /// `config_prefixes` arg — but kept as a seam for group-scoped prefixes.
    fn project_to_group_prefixes(&self) -> HashMap<String, String> {
        HashMap::new()
    }

    /// Atomically reserve the next ticket number for `project`, inside an open
    /// transaction. This is the collision fix: the id is minted by a single
    /// monotonic counter in `kanban_projects.next_id`, bumped with a
    /// serialized `UPDATE`, so no two callers can ever mint the same id.
    ///
    /// The DB counter is the allocator; the JSONL `_meta`/create events are the
    /// durable seed. On each reserve we reconcile: the effective counter is the
    /// MAX of (a) the DB row's next_id and (b) the JSONL-derived next number.
    /// That keeps the counter correct across cold starts, rebuilds, and events
    /// folded in from other machines — always monotonic, never reused.
    fn reserve_ticket_id(
        conn: &Connection,
        vault_root: &Path,
        project: &str,
        domain: &str,
        config_prefixes: &HashMap<String, String>,
        _group_prefixes: &HashMap<String, String>,
    ) -> Result<(String, i64), KanbanError> {
        // Existing registration?
        let existing: Option<(String, i64)> = conn.query_row(
            "SELECT prefix, next_id FROM kanban_projects WHERE project=?1",
            rusqlite::params![project], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;

        let prefix = match &existing {
            Some((prefix, _)) => prefix.clone(),
            None => {
                let mut stmt = conn.prepare("SELECT prefix FROM kanban_projects")?;
                let existing_prefixes: Vec<String> =
                    stmt.query_map([], |row| row.get(0))?.collect::<Result<_, _>>()?;
                crate::kanban::prefix::resolve_prefix(project, config_prefixes, &existing_prefixes)
                    .ok_or_else(|| KanbanError::InvalidInput(format!(
                        "could not derive a unique prefix for project '{project}'; set an explicit prefix in config"
                    )))?
            }
        };

        // Reconcile DB counter with the JSONL seed and take the higher. The
        // JSONL seed guards against a stale/empty cache handing out a number
        // that already exists on disk (rebuild races, foreign folds).
        let db_next = existing.map(|(_, n)| n).unwrap_or(1);
        let jsonl_next = events::next_ticket_number(vault_root, domain, project, &prefix);
        let reserved = db_next.max(jsonl_next).max(1);

        // Persist the bump atomically within the transaction. Any concurrent
        // create is blocked on the same mutex/tx, so it will read reserved+1.
        conn.execute(
            "INSERT INTO kanban_projects (project, prefix, domain, next_id) VALUES (?1,?2,?3,?4)
             ON CONFLICT(project) DO UPDATE SET next_id=?4",
            rusqlite::params![project, prefix, domain, reserved + 1],
        )?;

        Ok((prefix, reserved))
    }

    /// If `name` is a group name, return all member projects. Otherwise empty vec.
    fn resolve_group_members(&self, name: &str) -> Vec<String> {
        self.project_to_group.iter()
            .filter(|(_, g)| g.as_str() == name)
            .map(|(p, _)| p.clone())
            .collect()
    }

    fn upsert_project(&self, conn: &Connection, project: &str, prefix: &str, domain: &str, next_id: i64) -> Result<(), KanbanError> {
        Self::ensure_project_domain(conn, project, domain)?;
        conn.execute(
            "INSERT INTO kanban_projects (project, prefix, domain, next_id) VALUES (?1,?2,?3,?4) ON CONFLICT(project) DO UPDATE SET next_id=MAX(next_id, excluded.next_id)",
            rusqlite::params![project, prefix, domain, next_id],
        )?;
        Ok(())
    }

    /// The current cache/API address projects and tickets by bare slug/id.
    /// Until those public identities become `(domain, project, ticket_id)`,
    /// accepting the same project slug in two domains would silently replace
    /// one domain's materialized rows with the other. Fail closed before any
    /// source write or cache swap; JSONL remains canonical and untouched.
    fn ensure_project_domain(conn: &Connection, project: &str, domain: &str) -> Result<(), KanbanError> {
        let existing: Option<String> = conn.query_row(
            "SELECT domain FROM kanban_projects WHERE project=?1",
            rusqlite::params![project],
            |row| row.get(0),
        ).optional()?;

        match existing {
            Some(existing) if existing != domain => Err(KanbanError::InvalidInput(format!(
                "project slug collision: '{project}' is already registered in domain '{existing}', cannot also use domain '{domain}'"
            ))),
            _ => Ok(()),
        }
    }
}

/// Load groups from {vault}/kanban.yml. Returns empty map if file missing or malformed.
fn load_kanban_yml(vault_root: &Path) -> HashMap<String, Vec<String>> {
    #[derive(serde::Deserialize)]
    struct KanbanYml {
        #[serde(default)]
        groups: HashMap<String, Vec<String>>,
    }

    let path = vault_root.join("kanban.yml");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };
    match serde_yaml::from_str::<KanbanYml>(&content) {
        Ok(yml) => yml.groups,
        Err(_) => HashMap::new(),
    }
}

fn event_to_activity(event: &events::KanbanEvent, ticket_override: Option<&str>) -> ActivityEntry {
    let (event_type, summary) = match event {
        events::KanbanEvent::Create { title, .. } => ("create", title.clone()),
        events::KanbanEvent::Move { from, to, .. } => {
            let from_str = from.as_deref().unwrap_or("?");
            ("move", format!("{from_str} → {to}"))
        }
        events::KanbanEvent::Update { fields, .. } => {
            let keys: Vec<&str> = fields.keys().map(String::as_str).collect();
            ("update", keys.join(", "))
        }
        events::KanbanEvent::Note { text, .. } => {
            let truncated = if text.len() > 80 {
                let boundary = text.floor_char_boundary(80);
                format!("{}…", &text[..boundary])
            } else { text.clone() };
            ("note", truncated)
        }
        events::KanbanEvent::Archive { .. } => ("archive", "archived".into()),
        events::KanbanEvent::Attach { filename, .. } => ("attach", filename.clone()),
        events::KanbanEvent::Detach { attachment_id, .. } => ("detach", attachment_id.clone()),
        events::KanbanEvent::Reorder { data, .. } => ("reorder", format!("position {}", data.position)),
        events::KanbanEvent::GroomRequested { reason, .. } => {
            ("groom_requested", reason.clone().unwrap_or_else(|| "grooming requested".into()))
        }
        events::KanbanEvent::GroomCompleted { readiness, .. } => {
            ("groom_completed", readiness.clone().unwrap_or_else(|| "grooming completed".into()))
        }
        events::KanbanEvent::GroomFailed { error, .. } => {
            ("groom_failed", error.clone().unwrap_or_else(|| "grooming failed".into()))
        }
    };
    ActivityEntry {
        ticket_id: ticket_override.map(String::from),
        event: event_type.into(),
        timestamp: event.timestamp().into(),
        summary,
    }
}

/// Resolve the newest grooming artifact for a ticket by the
/// `<domain>/<project>/docs/grooming/<ticket>-grooming-<ts>.md` convention.
/// Returns a vault-relative path (readable via `wardwell_search action:read`),
/// or `None` if no artifact exists. Read-only.
fn latest_grooming_artifact(vault_root: &Path, domain: &str, project: &str, ticket_id: &str) -> Option<String> {
    let dir = vault_root.join(domain).join(project).join("docs").join("grooming");
    let prefix = format!("{ticket_id}-grooming-");
    let mut newest: Option<String> = None;
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // The dash after the full ticket id prevents CM-14 matching CM-140.
        if name.starts_with(&prefix) && name.ends_with(".md") {
            // Filenames embed a sortable timestamp, so lexicographic max = newest.
            if newest.as_deref().is_none_or(|cur| name.as_str() > cur) {
                newest = Some(name);
            }
        }
    }
    newest.map(|name| format!("{domain}/{project}/docs/grooming/{name}"))
}

/// Parse `readiness` and `surface_to_jack` from a grooming artifact's header.
/// The groomer writes a stable preamble:
///   - readiness: build_prompt_needed
///   - surface_to_jack: true
/// Returns `(readiness, surfaced)`; either may be `None` if absent/unreadable.
fn parse_grooming_header(vault_root: &Path, rel_path: &str) -> (Option<String>, Option<bool>) {
    let full = vault_root.join(rel_path);
    let content = match std::fs::read_to_string(&full) {
        Ok(c) => c,
        Err(_) => return (None, None),
    };
    let mut readiness = None;
    let mut surfaced = None;
    // Only scan the header region; stop once findings/prose begin.
    for line in content.lines().take(40) {
        let l = line.trim_start_matches(['-', ' ', '*']).trim();
        if let Some(rest) = l.strip_prefix("readiness:") {
            if readiness.is_none() { readiness = Some(rest.trim().to_string()); }
        } else if let Some(rest) = l.strip_prefix("surface_to_jack:") {
            if surfaced.is_none() {
                surfaced = match rest.trim() {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => None,
                };
            }
        }
    }
    (readiness, surfaced)
}

fn mime_from_ext(filename: &str) -> String {
    match filename.rsplit('.').next().map(|e| e.to_lowercase()).as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("json") => "application/json",
        Some("txt") => "text/plain",
        Some("md") => "text/markdown",
        Some("csv") => "text/csv",
        _ => "application/octet-stream",
    }.into()
}

fn validate_status(s: &str) -> Result<&str, KanbanError> {
    match s {
        "backlog" | "todo" | "in_progress" | "review" | "blocked" | "done" => Ok(s),
        other => Err(KanbanError::InvalidInput(format!("invalid status '{other}'; must be one of: backlog, todo, in_progress, review, blocked, done"))),
    }
}

/// WA-5: loop stages. Enum-validated; free text is rejected.
fn validate_stage(s: &str) -> Result<&str, KanbanError> {
    match s {
        "idea" | "grill" | "spec" | "design_audit" | "post_design_audit"
        | "audit_gate" | "build" | "pr" | "complete" => Ok(s),
        other => Err(KanbanError::InvalidInput(format!(
            "invalid stage '{other}'; must be one of: idea, grill, spec, design_audit, post_design_audit, audit_gate, build, pr, complete"
        ))),
    }
}

/// WA-5: `waiting_on` is prefix-validated. Must be null (handled by the caller)
/// or start with `human:` or `blocker:`. Free text like "waiting on Jack" is
/// rejected so briefings can rely on the structured form.
fn validate_waiting_on(w: &str) -> Result<&str, KanbanError> {
    if w.starts_with("human:") || w.starts_with("blocker:") {
        Ok(w)
    } else {
        Err(KanbanError::InvalidInput(format!(
            "invalid waiting_on '{w}'; must be null (empty or \"null\" to clear), or start with 'human:' or 'blocker:'"
        )))
    }
}

fn validate_priority(p: &str) -> Result<&str, KanbanError> {
    match p {
        "low" | "medium" | "high" | "urgent" => Ok(p),
        other => Err(KanbanError::InvalidInput(format!("invalid priority '{other}'; must be one of: low, medium, high, urgent"))),
    }
}

pub fn default_kanban_queries() -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("overdue".into(), "status != 'done' AND deadline < date('now')".into());
    m.insert("stale".into(), "status != 'done' AND updated_at < datetime('now', '-7 days')".into());
    m.insert("no_deadline".into(), "status != 'done' AND deadline IS NULL".into());
    m.insert("blocked".into(), "status = 'backlog'".into());
    m.insert("recent".into(), "updated_at > datetime('now', '-2 days')".into());
    m.insert("by_epic".into(), "epic IS NOT NULL AND status != 'done'".into());
    m
}

pub fn merge_kanban_queries(config_queries: &HashMap<String, String>) -> HashMap<String, String> {
    let mut merged = default_kanban_queries();
    for (k, v) in config_queries { merged.insert(k.clone(), v.clone()); }
    merged
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn make_store() -> (tempfile::TempDir, KanbanStore) {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        let db = dir.path().join("kanban.db");
        let store = KanbanStore::open(&db, vault).unwrap();
        (dir, store)
    }

    #[test]
    fn create_item_basic() {
        let (_dir, store) = make_store();
        let item = store.create_item("Do the thing", "shulops", "work", None, None, None, None, None, None, None, None, None, &HashMap::new()).unwrap();
        assert_eq!(item.ticket_id, "SH-1");
        assert_eq!(item.status, "backlog");
        assert_eq!(item.priority, "medium");
    }

    #[test]
    fn create_item_increments_id() {
        let (_dir, store) = make_store();
        let p = HashMap::new();
        let a = store.create_item("A", "shulops", "work", None, None, None, None, None, None, None, None, None, &p).unwrap();
        let b = store.create_item("B", "shulops", "work", None, None, None, None, None, None, None, None, None, &p).unwrap();
        assert_eq!(a.ticket_id, "SH-1");
        assert_eq!(b.ticket_id, "SH-2");
    }

    #[test]
    fn create_writes_jsonl() {
        let (dir, store) = make_store();
        store.create_item("Test", "shulops", "work", None, None, None, None, None, None, None, None, None, &HashMap::new()).unwrap();
        let jsonl = dir.path().join("vault/work/shulops/kanban.jsonl");
        assert!(jsonl.exists());
        let content = std::fs::read_to_string(&jsonl).unwrap();
        assert!(content.contains("\"_schema\":\"kanban\""));
        assert!(content.contains("SH-1"));
        assert!(content.contains("\"_meta\":true"));
    }

    #[test]
    fn list_all_items() {
        let (_dir, store) = make_store();
        let p = HashMap::new();
        store.create_item("A", "shulops", "work", None, None, None, None, None, None, None, None, None, &p).unwrap();
        store.create_item("B", "other", "work", None, None, None, None, None, None, None, None, None, &p).unwrap();
        let items = store.list(None, None, None, None, None, None, true, None).unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn list_excludes_done() {
        let (_dir, store) = make_store();
        let p = HashMap::new();
        store.create_item("Active", "proj", "work", None, None, None, None, None, None, None, None, None, &p).unwrap();
        store.create_item("Done", "proj", "work", None, Some("done"), None, None, None, None, None, None, None, &p).unwrap();
        let items = store.list(None, None, None, None, None, None, false, None).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Active");
    }

    #[test]
    fn move_item_writes_jsonl() {
        let (dir, store) = make_store();
        let p = HashMap::new();
        store.create_item("Task", "shulops", "work", None, None, None, None, None, None, None, None, None, &p).unwrap();
        let (item, transition) = store.move_item("SH-1", "in_progress").unwrap();
        assert_eq!(item.status, "in_progress");
        assert_eq!(transition, "backlog → in_progress");

        let content = std::fs::read_to_string(dir.path().join("vault/work/shulops/kanban.jsonl")).unwrap();
        assert!(content.contains("\"event\":\"move\""));
    }

    #[test]
    fn update_item_writes_jsonl() {
        let (dir, store) = make_store();
        let p = HashMap::new();
        store.create_item("Old", "shulops", "work", None, None, None, None, None, None, None, None, None, &p).unwrap();
        let item = store.update_item("SH-1", Some("New"), None, None, None, None, None, None, None, None, None, None, None).unwrap();
        assert_eq!(item.title, "New");

        let content = std::fs::read_to_string(dir.path().join("vault/work/shulops/kanban.jsonl")).unwrap();
        assert!(content.contains("\"event\":\"update\""));
    }

    #[test]
    fn add_note_writes_jsonl() {
        let (dir, store) = make_store();
        let p = HashMap::new();
        store.create_item("Task", "shulops", "work", None, None, None, None, None, None, None, None, None, &p).unwrap();
        let item = store.add_note("SH-1", "Hello", Some("jack")).unwrap();
        assert_eq!(item.notes.len(), 1);

        let content = std::fs::read_to_string(dir.path().join("vault/work/shulops/kanban.jsonl")).unwrap();
        assert!(content.contains("\"event\":\"note\""));
    }

    #[test]
    fn rebuild_from_jsonl_restores_state() {
        let (dir, store) = make_store();
        let p = HashMap::new();
        store.create_item("Task", "shulops", "work", None, None, None, None, None, None, None, None, None, &p).unwrap();
        store.move_item("SH-1", "todo").unwrap();
        store.add_note("SH-1", "Note", None).unwrap();

        // Wipe SQLite cache (fold watermarks too, so this stays wiped until
        // an explicit rebuild — reads would otherwise self-heal via the fold)
        let conn = store.conn().unwrap();
        conn.execute_batch("DELETE FROM kanban_notes; DELETE FROM kanban_items; DELETE FROM kanban_projects;").unwrap();
        drop(conn);

        // Rebuild
        store.rebuild_from_jsonl().unwrap();

        let items = store.list(None, None, None, None, None, None, true, None).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].status, "todo");
        assert_eq!(items[0].ticket_id, "SH-1");
    }

    #[test]
    fn query_overdue() {
        let (_dir, store) = make_store();
        let p = HashMap::new();
        store.create_item("Past", "proj", "work", None, Some("todo"), None, None, Some("2020-01-01"), None, None, None, None, &p).unwrap();
        store.create_item("Future", "proj", "work", None, Some("todo"), None, None, Some("2099-12-31"), None, None, None, None, &p).unwrap();
        let results = store.query("overdue", &default_kanban_queries(), None, None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Past");
    }

    fn write_kanban_yml(vault: &Path, content: &str) {
        std::fs::write(vault.join("kanban.yml"), content).unwrap();
    }

    #[test]
    fn group_filtering() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        write_kanban_yml(&vault, "groups:\n  agent-system:\n    - vault-sync\n    - ai-arch\n");
        let store = KanbanStore::open(&dir.path().join("k.db"), vault).unwrap();
        let p = HashMap::new();

        store.create_item("Sync fix", "vault-sync", "work", None, None, None, None, None, None, None, None, None, &p).unwrap();
        store.create_item("Arch doc", "ai-arch", "work", None, None, None, None, None, None, None, None, None, &p).unwrap();
        store.create_item("Unrelated", "shulops", "work", None, None, None, None, None, None, None, None, None, &p).unwrap();

        // Filter by group name → returns both member projects
        let items = store.list(Some("agent-system"), None, None, None, None, None, false, None).unwrap();
        assert_eq!(items.len(), 2);

        // Filter by specific project still works
        let items = store.list(Some("vault-sync"), None, None, None, None, None, false, None).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].project, "vault-sync");
        assert_eq!(items[0].group.as_deref(), Some("agent-system"));

        // Ungrouped item has no group
        let items = store.list(Some("shulops"), None, None, None, None, None, false, None).unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].group.is_none());

        // All items
        let items = store.list(None, None, None, None, None, None, false, None).unwrap();
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn create_item_includes_group() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        write_kanban_yml(&vault, "groups:\n  mygroup:\n    - myproj\n");
        let store = KanbanStore::open(&dir.path().join("k.db"), vault).unwrap();

        let item = store.create_item("Test", "myproj", "work", None, None, None, None, None, None, None, None, None, &HashMap::new()).unwrap();
        assert_eq!(item.group.as_deref(), Some("mygroup"));

        // Check JSONL has group
        let content = std::fs::read_to_string(dir.path().join("vault/work/myproj/kanban.jsonl")).unwrap();
        assert!(content.contains("\"group\":\"mygroup\""));
    }

    #[test]
    fn no_kanban_yml_works_fine() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        // No kanban.yml — should not error
        let store = KanbanStore::open(&dir.path().join("k.db"), vault).unwrap();
        let item = store.create_item("Test", "proj", "work", None, None, None, None, None, None, None, None, None, &HashMap::new()).unwrap();
        assert!(item.group.is_none());
    }

    // ---- fold_foreign_events: multi-machine sync fidelity ----

    /// Simulate another machine's write landing via vault sync: append a raw
    /// event line to the jsonl without touching this store's SQLite cache.
    fn append_foreign_line(jsonl: &Path, line: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(jsonl).unwrap();
        writeln!(f, "{line}").unwrap();
    }

    #[test]
    fn fold_ingests_foreign_notes_with_full_text_and_dedupes_local() {
        let (dir, store) = make_store();
        let p = HashMap::new();
        store.create_item("Task", "shulops", "work", None, None, None, None, None, None, None, None, None, &p).unwrap();
        store.add_note("SH-1", "local note", Some("laptop")).unwrap();

        let jsonl = dir.path().join("vault/work/shulops/kanban.jsonl");
        let body = "foreign note body — the full 860 chars, not the 80-char headline";
        append_foreign_line(&jsonl, &format!(
            r#"{{"event":"note","ticket_id":"SH-1","text":"{body}","author":"mini","timestamp":"2099-01-01T00:00:00+00:00"}}"#
        ));

        // The read path folds before serving.
        let item = store.get_item("SH-1").unwrap();
        assert_eq!(item.notes.len(), 2, "foreign note ingested, local note not duplicated");
        assert_eq!(item.notes[0].text, body, "full text preserved, newest first");
        assert_eq!(item.notes[0].author.as_deref(), Some("mini"));
        assert_eq!(item.notes[0].created_at, "2099-01-01T00:00:00+00:00");

        // Idempotency: repeated reads / explicit refolds never double-insert.
        let refolded = store.fold_foreign_events().unwrap();
        assert_eq!(refolded, 0, "quiescent fold is a no-op");
        let item = store.get_item("SH-1").unwrap();
        assert_eq!(item.notes.len(), 2);
    }

    #[test]
    fn fold_ingests_foreign_create_and_move_latest_event_wins() {
        let (dir, store) = make_store();
        let p = HashMap::new();
        store.create_item("Local", "shulops", "work", None, None, None, None, None, None, None, None, None, &p).unwrap();

        let jsonl = dir.path().join("vault/work/shulops/kanban.jsonl");
        append_foreign_line(&jsonl, r#"{"event":"create","ticket_id":"SH-2","title":"Foreign ticket","project":"shulops","status":"backlog","priority":"high","timestamp":"2099-01-01T00:00:00+00:00"}"#);
        append_foreign_line(&jsonl, r#"{"event":"move","ticket_id":"SH-2","from":"backlog","to":"in_progress","timestamp":"2099-01-02T00:00:00+00:00"}"#);
        append_foreign_line(&jsonl, r#"{"event":"update","ticket_id":"SH-2","fields":{"assignee":"jack"},"timestamp":"2099-01-03T00:00:00+00:00"}"#);

        let item = store.get_item("SH-2").unwrap();
        assert_eq!(item.title, "Foreign ticket");
        assert_eq!(item.status, "in_progress", "latest event wins");
        assert_eq!(item.assignee.as_deref(), Some("jack"));
        assert_eq!(item.created_at, "2099-01-01T00:00:00+00:00");

        // Both tickets visible in list; local one untouched.
        let items = store.list(Some("shulops"), None, None, None, None, None, true, None).unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn fold_skips_unparseable_lines_without_aborting() {
        let (dir, store) = make_store();
        let p = HashMap::new();
        store.create_item("Task", "shulops", "work", None, None, None, None, None, None, None, None, None, &p).unwrap();

        let jsonl = dir.path().join("vault/work/shulops/kanban.jsonl");
        append_foreign_line(&jsonl, "{this is not json");
        append_foreign_line(&jsonl, r#"{"event":"note","ticket_id":"SH-1","text":"after garbage","timestamp":"2099-01-01T00:00:00+00:00"}"#);

        let item = store.get_item("SH-1").unwrap();
        assert_eq!(item.notes.len(), 1);
        assert_eq!(item.notes[0].text, "after garbage");
    }

    #[test]
    fn fold_is_idempotent_across_repeated_explicit_calls() {
        let (dir, store) = make_store();
        let p = HashMap::new();
        store.create_item("Task", "shulops", "work", None, None, None, None, None, None, None, None, None, &p).unwrap();

        let jsonl = dir.path().join("vault/work/shulops/kanban.jsonl");
        append_foreign_line(&jsonl, r#"{"event":"note","ticket_id":"SH-1","text":"once","timestamp":"2099-01-01T00:00:00+00:00"}"#);

        assert_eq!(store.fold_foreign_events().unwrap(), 1);
        assert_eq!(store.fold_foreign_events().unwrap(), 0);
        assert_eq!(store.fold_foreign_events().unwrap(), 0);
        let item = store.get_item("SH-1").unwrap();
        assert_eq!(item.notes.iter().filter(|n| n.text == "once").count(), 1);
    }

    #[test]
    fn duplicate_project_slug_across_domains_fails_closed_without_replacing_rows()
        -> Result<(), Box<dyn std::error::Error>>
    {
        let (dir, store) = make_store();
        let prefixes = HashMap::new();
        let original = store.create_item(
            "Personal board item",
            "switchboard",
            "personal",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            &prefixes,
        )?;

        let foreign_dir = dir.path().join("vault/work/switchboard");
        std::fs::create_dir_all(&foreign_dir)?;
        std::fs::write(
            foreign_dir.join("kanban.jsonl"),
            r#"{"event":"create","ticket_id":"SW-1","title":"Warden replacement","project":"switchboard","status":"backlog","priority":"medium","timestamp":"2099-01-01T00:00:00+00:00"}
"#,
        )?;

        let error = match store.fold_foreign_events() {
            Err(error) => error,
            Ok(_) => return Err("duplicate domain fold unexpectedly succeeded".into()),
        };

        assert!(error.to_string().contains("project slug collision"));
        let still_standing = store.get_item(&original.ticket_id)?;
        assert_eq!(still_standing.title, "Personal board item");
        Ok(())
    }

    // ---- SW-68: collision-free, domain-scoped identity ----

    #[test]
    fn concurrent_creates_yield_unique_monotonic_ids() {
        use std::sync::Arc;
        let (_dir, store) = make_store();
        let store = Arc::new(store);
        let n = 25;

        let handles: Vec<_> = (0..n)
            .map(|i| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    store
                        .create_item(
                            &format!("item {i}"), "shulops", "work",
                            None, None, None, None, None, None, None, None, None,
                            &HashMap::new(),
                        )
                        .unwrap()
                        .ticket_id
                })
            })
            .collect();

        let mut ids: Vec<i64> = handles
            .into_iter()
            .map(|h| {
                let id = h.join().unwrap();
                id.strip_prefix("SH-").unwrap().parse::<i64>().unwrap()
            })
            .collect();
        ids.sort_unstable();

        // 25 unique, contiguous, monotonic ids 1..=25 — no dupes, no gaps.
        assert_eq!(ids.len(), n as usize);
        let unique: std::collections::HashSet<_> = ids.iter().copied().collect();
        assert_eq!(unique.len(), n as usize, "duplicate ids minted: {ids:?}");
        assert_eq!(ids, (1..=n).collect::<Vec<_>>());
    }

    #[test]
    fn quarantining_a_board_removes_its_rows_from_cache_on_next_fold()
        -> Result<(), Box<dyn std::error::Error>> {
        let (dir, store) = make_store();
        store.create_item("Doomed", "acme", "work", None, None, None, None, None, None, None, None, None, &HashMap::new())?;
        assert!(store.get_item("AC-1").is_ok());

        // Simulate the recovery command / a deletion: rename the board away.
        let board = dir.path().join("vault/work/acme/kanban.jsonl");
        std::fs::rename(&board, board.with_file_name("kanban.jsonl.quarantine-2026-07-16"))?;

        // Next fold must prune the vanished board's rows immediately.
        store.fold_foreign_events()?;
        assert!(store.get_item("AC-1").is_err(), "row survived quarantine");
        Ok(())
    }

    #[test]
    fn recover_quarantines_cross_domain_slug_shadow_keeping_the_original()
        -> Result<(), Box<dyn std::error::Error>> {
        let (dir, _store) = make_store();
        // Original personal board (older create).
        let personal = dir.path().join("vault/personal/switchboard");
        std::fs::create_dir_all(&personal)?;
        std::fs::write(personal.join("kanban.jsonl"),
            "{\"event\":\"create\",\"ticket_id\":\"SW-1\",\"title\":\"original\",\"project\":\"switchboard\",\"status\":\"backlog\",\"priority\":\"medium\",\"timestamp\":\"2026-01-01T00:00:00+00:00\"}\n")?;
        // Shadowing work board (newer create).
        let work = dir.path().join("vault/work/switchboard");
        std::fs::create_dir_all(&work)?;
        std::fs::write(work.join("kanban.jsonl"),
            "{\"event\":\"create\",\"ticket_id\":\"SW-1\",\"title\":\"shadow\",\"project\":\"switchboard\",\"status\":\"backlog\",\"priority\":\"medium\",\"timestamp\":\"2026-07-14T00:00:00+00:00\"}\n")?;

        // Fresh store so open() doesn't fail-closed on the pre-existing collision.
        let db = dir.path().join("kanban2.db");
        let store = KanbanStore::open(&db, dir.path().join("vault"))?;
        let report = store.recover_collisions(false)?;

        assert_eq!(report.slug_collisions.len(), 1, "{report:?}");
        assert_eq!(report.quarantined.len(), 1);
        // Original personal board preserved; work board quarantined away.
        assert!(personal.join("kanban.jsonl").exists());
        assert!(!work.join("kanban.jsonl").exists());
        Ok(())
    }

    #[test]
    fn write_file_rejects_kanban_owned_paths_is_covered_by_server_tests() {
        // The prohibition lives in the MCP write_file handler (server.rs);
        // this marker documents that store-level creates remain the only
        // sanctioned path to mutate kanban.jsonl.
        let (_dir, store) = make_store();
        assert!(store.create_item("x", "shulops", "work", None, None, None, None, None, None, None, None, None, &HashMap::new()).is_ok());
    }

    #[test]
    fn notes_ordering_is_created_at_desc_with_no_cap() {
        let (dir, store) = make_store();
        let p = HashMap::new();
        store.create_item("Task", "shulops", "work", None, None, None, None, None, None, None, None, None, &p).unwrap();

        let jsonl = dir.path().join("vault/work/shulops/kanban.jsonl");
        // 60 foreign notes with ascending timestamps — more than any small cap.
        for i in 0..60 {
            append_foreign_line(&jsonl, &format!(
                r#"{{"event":"note","ticket_id":"SH-1","text":"note {i}","timestamp":"2099-01-01T00:{i:02}:00+00:00"}}"#
            ));
        }

        let item = store.get_item("SH-1").unwrap();
        assert_eq!(item.notes.len(), 60, "no hidden LIMIT on notes");
        assert_eq!(item.notes[0].text, "note 59", "newest first");
        assert_eq!(item.notes[59].text, "note 0");
    }
}
