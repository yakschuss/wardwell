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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
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

/// Kanban store: JSONL is source of truth, SQLite is materialized cache.
#[derive(Debug)]
pub struct KanbanStore {
    conn: Mutex<Connection>,
    vault_root: PathBuf,
    /// Reverse lookup: project slug → group name (from config kanban.groups).
    project_to_group: HashMap<String, String>,
}

impl KanbanStore {
    const SCHEMA_VERSION: i64 = 6;

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
                tags         TEXT DEFAULT '[]',
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
            CREATE INDEX IF NOT EXISTS idx_kanban_attachments_ticket ON kanban_attachments(ticket_id);"
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
        let status = validate_status(status.unwrap_or("backlog"))?;
        let priority = validate_priority(priority.unwrap_or("medium"))?;
        let now = chrono::Utc::now().to_rfc3339();
        let tags_vec = tags.map(|t| t.to_vec()).unwrap_or_default();

        let (prefix, next_id) = self.resolve_ticket_id(project, domain, config_prefixes)?;
        let ticket_id = format!("{prefix}-{next_id}");

        let group = self.project_to_group.get(project).cloned();

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

        events::append_event(&self.vault_root, domain, project, &event)?;
        events::append_meta(&self.vault_root, domain, project, &prefix, next_id + 1)?;

        // Update SQLite cache
        let conn = self.conn()?;
        self.upsert_project(&conn, project, &prefix, domain, next_id + 1)?;
        let completed_at: Option<String> = if status == "done" { Some(now.clone()) } else { None };
        let tags_json = serde_json::to_string(&tags_vec).unwrap_or_else(|_| "[]".into());
        conn.execute(
            "INSERT OR REPLACE INTO kanban_items (ticket_id, project, title, description, status, priority, assignee, deadline, source, epic, parent, tags, created_at, updated_at, completed_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            rusqlite::params![ticket_id, project, title, description, status, priority, assignee, deadline, source, epic, parent, tags_json, now, now, completed_at],
        )?;

        Ok(KanbanItem {
            ticket_id, project: project.into(), group, epic: epic.map(str::to_string), title: title.into(),
            description: description.map(str::to_string), status: status.into(), priority: priority.into(),
            assignee: assignee.map(str::to_string), deadline: deadline.map(str::to_string),
            source: source.map(str::to_string), parent: parent.map(str::to_string), children: vec![],
            tags: tags_vec, created_at: now.clone(), updated_at: now,
            completed_at, notes: vec![], attachments: vec![], activity: vec![], children_activity: vec![],
        })
    }

    pub fn move_item(&self, ticket_id: &str, new_status: &str) -> Result<(KanbanItem, String), KanbanError> {
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
    ) -> Result<KanbanItem, KanbanError> {
        if let Some(s) = status { validate_status(s)?; }
        if let Some(p) = priority { validate_priority(p)?; }

        let conn = self.conn()?;
        let now = chrono::Utc::now().to_rfc3339();

        let (current_status, project, domain) = self.get_item_context(&conn, ticket_id)?;

        let mut fields = HashMap::new();
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

        if !fields.is_empty() {
            let event = KanbanEvent::Update {
                ticket_id: ticket_id.into(),
                fields,
                timestamp: now.clone(),
            };
            events::append_event(&self.vault_root, &domain, &project, &event)?;
        }

        // Update SQLite cache
        let mut sets = vec!["updated_at = ?1".to_string()];
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(now)];
        let mut idx = 2;

        if let Some(v) = title { sets.push(format!("title=?{idx}")); params.push(Box::new(v.to_string())); idx += 1; }
        if let Some(v) = description { sets.push(format!("description=?{idx}")); params.push(Box::new(v.to_string())); idx += 1; }
        if let Some(v) = status {
            sets.push(format!("status=?{idx}")); params.push(Box::new(v.to_string())); idx += 1;
            if v == "done" && current_status != "done" {
                sets.push(format!("completed_at=?{idx}")); params.push(Box::new(chrono::Utc::now().to_rfc3339())); idx += 1;
            } else if v != "done" && current_status == "done" {
                sets.push(format!("completed_at=?{idx}")); params.push(Box::new(Option::<String>::None)); idx += 1;
            }
        }
        if let Some(v) = priority { sets.push(format!("priority=?{idx}")); params.push(Box::new(v.to_string())); idx += 1; }
        if let Some(v) = assignee { sets.push(format!("assignee=?{idx}")); params.push(Box::new(v.to_string())); idx += 1; }
        if let Some(v) = deadline { sets.push(format!("deadline=?{idx}")); params.push(Box::new(v.to_string())); idx += 1; }
        if let Some(v) = epic { sets.push(format!("epic=?{idx}")); params.push(Box::new(v.to_string())); idx += 1; }
        if let Some(v) = parent { sets.push(format!("parent=?{idx}")); params.push(Box::new(if v.is_empty() { None } else { Some(v.to_string()) })); idx += 1; }
        if let Some(t) = tags { let tj = serde_json::to_string(t).unwrap_or_else(|_| "[]".into()); sets.push(format!("tags=?{idx}")); params.push(Box::new(tj)); let _ = idx; }

        params.push(Box::new(ticket_id.to_string()));
        let sql = format!("UPDATE kanban_items SET {} WHERE ticket_id=?{}", sets.join(", "), params.len());
        let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        conn.execute(&sql, refs.as_slice())?;

        self.get_item_with_conn(&conn, ticket_id)
    }

    pub fn add_note(&self, ticket_id: &str, text: &str, author: Option<&str>) -> Result<KanbanItem, KanbanError> {
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

    // ---- Read path: SQLite only ----

    pub fn get_item(&self, ticket_id: &str) -> Result<KanbanItem, KanbanError> {
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
            "SELECT kanban_items.ticket_id, kanban_items.project, kanban_items.epic, kanban_items.parent, kanban_items.tags, kanban_items.title, kanban_items.description, kanban_items.status, kanban_items.priority, kanban_items.assignee, kanban_items.deadline, kanban_items.source, kanban_items.created_at, kanban_items.updated_at, kanban_items.completed_at {from} {wh} ORDER BY kanban_items.updated_at DESC LIMIT 20"
        );

        let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let items: Vec<KanbanItem> = stmt.query_map(refs.as_slice(), |row| {
            Ok(KanbanItem {
                ticket_id: row.get(0)?, project: row.get(1)?, group: None, epic: row.get(2)?,
                parent: row.get(3)?, children: vec![],
                tags: { let t: String = row.get::<_, String>(4).unwrap_or_else(|_| "[]".into()); serde_json::from_str(&t).unwrap_or_default() },
                title: row.get(5)?, description: row.get(6)?, status: row.get(7)?, priority: row.get(8)?,
                assignee: row.get(9)?, deadline: row.get(10)?, source: row.get(11)?,
                created_at: row.get(12)?, updated_at: row.get(13)?, completed_at: row.get(14)?,
                notes: vec![], attachments: vec![], activity: vec![], children_activity: vec![],
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
            "SELECT kanban_items.ticket_id, kanban_items.project, kanban_items.epic, kanban_items.parent, kanban_items.tags, kanban_items.title, kanban_items.description, kanban_items.status, kanban_items.priority, kanban_items.assignee, kanban_items.deadline, kanban_items.source, kanban_items.created_at, kanban_items.updated_at, kanban_items.completed_at {from} {wh} ORDER BY CASE kanban_items.priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'medium' THEN 2 WHEN 'low' THEN 3 ELSE 4 END, kanban_items.updated_at DESC"
        );

        let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let items: Vec<KanbanItem> = stmt.query_map(refs.as_slice(), |row| {
            Ok(KanbanItem {
                ticket_id: row.get(0)?, project: row.get(1)?, group: None, epic: row.get(2)?,
                parent: row.get(3)?, children: vec![],
                tags: { let t: String = row.get::<_, String>(4).unwrap_or_else(|_| "[]".into()); serde_json::from_str(&t).unwrap_or_default() },
                title: row.get(5)?, description: row.get(6)?, status: row.get(7)?, priority: row.get(8)?,
                assignee: row.get(9)?, deadline: row.get(10)?, source: row.get(11)?,
                created_at: row.get(12)?, updated_at: row.get(13)?, completed_at: row.get(14)?,
                notes: vec![], attachments: vec![], activity: vec![], children_activity: vec![],
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

    pub fn query(
        &self, question: &str, queries: &HashMap<String, String>,
        project: Option<&str>, domains: Option<&[String]>,
    ) -> Result<Vec<KanbanItem>, KanbanError> {
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
            "SELECT kanban_items.ticket_id, kanban_items.project, kanban_items.epic, kanban_items.parent, kanban_items.tags, kanban_items.title, kanban_items.description, kanban_items.status, kanban_items.priority, kanban_items.assignee, kanban_items.deadline, kanban_items.source, kanban_items.created_at, kanban_items.updated_at, kanban_items.completed_at {from} {wh} ORDER BY kanban_items.updated_at DESC"
        );

        let conn = self.conn()?;
        let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let items: Vec<KanbanItem> = stmt.query_map(refs.as_slice(), |row| {
            Ok(KanbanItem {
                ticket_id: row.get(0)?, project: row.get(1)?, group: None, epic: row.get(2)?,
                parent: row.get(3)?, children: vec![],
                tags: { let t: String = row.get::<_, String>(4).unwrap_or_else(|_| "[]".into()); serde_json::from_str(&t).unwrap_or_default() },
                title: row.get(5)?, description: row.get(6)?, status: row.get(7)?, priority: row.get(8)?,
                assignee: row.get(9)?, deadline: row.get(10)?, source: row.get(11)?,
                created_at: row.get(12)?, updated_at: row.get(13)?, completed_at: row.get(14)?,
                notes: vec![], attachments: vec![], activity: vec![], children_activity: vec![],
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
        let all = events::scan_all_jsonl(&self.vault_root);
        let conn = self.conn()?;

        conn.execute_batch(
            "DROP TABLE IF EXISTS kanban_attachments;
             DROP TABLE IF EXISTS kanban_notes;
             DROP TABLE IF EXISTS kanban_items;
             DROP TABLE IF EXISTS kanban_projects;"
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
                source TEXT, epic TEXT, parent TEXT, tags TEXT DEFAULT '[]', created_at TEXT NOT NULL,
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

        for (domain, _project, evts) in &all {
            let items = events::materialize(domain, evts);
            for item in &items {
                // Derive prefix from ticket_id
                if let Some(dash) = item.ticket_id.find('-') {
                    let prefix = &item.ticket_id[..dash];
                    let num: i64 = item.ticket_id[dash + 1..].parse().unwrap_or(1);
                    self.upsert_project(&conn, &item.project, prefix, &item.domain, num + 1)?;
                }

                let tags_json = serde_json::to_string(&item.tags).unwrap_or_else(|_| "[]".into());
                conn.execute(
                    "INSERT OR REPLACE INTO kanban_items (ticket_id, project, title, description, status, priority, assignee, deadline, source, epic, parent, tags, created_at, updated_at, completed_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
                    rusqlite::params![item.ticket_id, item.project, item.title, item.description, item.status, item.priority, item.assignee, item.deadline, item.source, item.epic, item.parent, tags_json, item.created_at, item.updated_at, item.completed_at],
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
            }
        }

        Ok(())
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
        let mut stmt = conn.prepare("SELECT id, text, author, created_at FROM kanban_notes WHERE ticket_id=?1 ORDER BY created_at DESC")?;
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
            "SELECT ticket_id, project, epic, parent, tags, title, description, status, priority, assignee, deadline, source, created_at, updated_at, completed_at FROM kanban_items WHERE ticket_id=?1",
            rusqlite::params![ticket_id],
            |row| Ok(KanbanItem {
                ticket_id: row.get(0)?, project: row.get(1)?, group: None, epic: row.get(2)?,
                parent: row.get(3)?, children: vec![],
                tags: { let t: String = row.get::<_, String>(4).unwrap_or_else(|_| "[]".into()); serde_json::from_str(&t).unwrap_or_default() },
                title: row.get(5)?, description: row.get(6)?, status: row.get(7)?, priority: row.get(8)?,
                assignee: row.get(9)?, deadline: row.get(10)?, source: row.get(11)?,
                created_at: row.get(12)?, updated_at: row.get(13)?, completed_at: row.get(14)?,
                notes: vec![], attachments: vec![], activity: vec![], children_activity: vec![],
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

    fn resolve_ticket_id(&self, project: &str, domain: &str, config_prefixes: &HashMap<String, String>) -> Result<(String, i64), KanbanError> {
        let conn = self.conn()?;

        // Check if project already registered
        let existing: Option<(String, i64)> = conn.query_row(
            "SELECT prefix, next_id FROM kanban_projects WHERE project=?1",
            rusqlite::params![project], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;

        if let Some((prefix, _)) = existing {
            // Get authoritative next_id from JSONL meta
            let next = events::next_ticket_number(&self.vault_root, domain, project, &prefix);
            return Ok((prefix, next));
        }

        // New project — derive prefix
        let mut stmt = conn.prepare("SELECT prefix FROM kanban_projects")?;
        let existing_prefixes: Vec<String> = stmt.query_map([], |row| row.get(0))?.collect::<Result<_, _>>()?;

        let prefix = crate::kanban::prefix::resolve_prefix(project, config_prefixes, &existing_prefixes)
            .ok_or_else(|| KanbanError::InvalidInput(format!(
                "could not derive a unique prefix for project '{project}'; set an explicit prefix in config"
            )))?;

        let next = events::next_ticket_number(&self.vault_root, domain, project, &prefix);
        Ok((prefix, next))
    }

    /// If `name` is a group name, return all member projects. Otherwise empty vec.
    fn resolve_group_members(&self, name: &str) -> Vec<String> {
        self.project_to_group.iter()
            .filter(|(_, g)| g.as_str() == name)
            .map(|(p, _)| p.clone())
            .collect()
    }

    fn upsert_project(&self, conn: &Connection, project: &str, prefix: &str, domain: &str, next_id: i64) -> Result<(), KanbanError> {
        conn.execute(
            "INSERT INTO kanban_projects (project, prefix, domain, next_id) VALUES (?1,?2,?3,?4) ON CONFLICT(project) DO UPDATE SET next_id=MAX(next_id, excluded.next_id)",
            rusqlite::params![project, prefix, domain, next_id],
        )?;
        Ok(())
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
    };
    ActivityEntry {
        ticket_id: ticket_override.map(String::from),
        event: event_type.into(),
        timestamp: event.timestamp().into(),
        summary,
    }
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
        "backlog" | "todo" | "in_progress" | "review" | "done" => Ok(s),
        other => Err(KanbanError::InvalidInput(format!("invalid status '{other}'; must be one of: backlog, todo, in_progress, review, done"))),
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
        let item = store.update_item("SH-1", Some("New"), None, None, None, None, None, None, None, None).unwrap();
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

        // Wipe SQLite cache
        let conn = store.conn().unwrap();
        conn.execute_batch("DELETE FROM kanban_notes; DELETE FROM kanban_items; DELETE FROM kanban_projects;").unwrap();
        drop(conn);

        // Verify empty
        let items = store.list(None, None, None, None, None, None, true, None).unwrap();
        assert_eq!(items.len(), 0);

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
}
