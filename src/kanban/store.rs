use rusqlite::{Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

#[derive(Debug, Clone, serde::Serialize)]
pub struct KanbanItem {
    pub ticket_id: String,
    pub project: String,
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
    pub notes: Vec<KanbanNote>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct KanbanNote {
    pub id: i64,
    pub text: String,
    pub author: Option<String>,
    pub created_at: String,
}

/// Errors from kanban store operations.
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

/// SQLite-backed kanban store. Thread-safe via Mutex.
#[derive(Debug)]
pub struct KanbanStore {
    conn: Mutex<Connection>,
}

impl KanbanStore {
    /// Open (or create) a kanban store at the given path.
    pub fn open(path: &Path) -> Result<Self, KanbanError> {
        let conn = Connection::open(path)?;
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        conn.busy_timeout(Duration::from_secs(5))?;

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

            CREATE INDEX IF NOT EXISTS idx_kanban_items_project
                ON kanban_items (project);

            CREATE INDEX IF NOT EXISTS idx_kanban_items_status
                ON kanban_items (status);

            CREATE INDEX IF NOT EXISTS idx_kanban_notes_ticket
                ON kanban_notes (ticket_id);",
        )?;

        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Acquire the connection lock.
    pub fn conn(&self) -> Result<MutexGuard<'_, Connection>, KanbanError> {
        self.conn.lock().map_err(|_| KanbanError::LockPoisoned)
    }

    /// Create a new kanban item, registering the project if needed.
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
        config_prefixes: &HashMap<String, String>,
    ) -> Result<KanbanItem, KanbanError> {
        let conn = self.conn()?;
        let now = chrono::Utc::now().to_rfc3339();

        let status = match status.unwrap_or("backlog") {
            s @ ("backlog" | "todo" | "in_progress" | "review" | "done") => s.to_string(),
            other => {
                return Err(KanbanError::InvalidInput(format!(
                    "invalid status '{other}'; must be one of: backlog, todo, in_progress, review, done"
                )))
            }
        };

        let priority = match priority.unwrap_or("medium") {
            p @ ("low" | "medium" | "high" | "urgent") => p.to_string(),
            other => {
                return Err(KanbanError::InvalidInput(format!(
                    "invalid priority '{other}'; must be one of: low, medium, high, urgent"
                )))
            }
        };

        let (prefix, next_id) = self.ensure_project(&conn, project, domain, config_prefixes)?;
        let ticket_id = format!("{prefix}-{next_id}");

        conn.execute(
            "UPDATE kanban_projects SET next_id = next_id + 1 WHERE project = ?1",
            rusqlite::params![project],
        )?;

        let completed_at: Option<String> = if status == "done" { Some(now.clone()) } else { None };

        conn.execute(
            "INSERT INTO kanban_items
                (ticket_id, project, title, description, status, priority,
                 assignee, deadline, source, created_at, updated_at, completed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                ticket_id,
                project,
                title,
                description,
                status,
                priority,
                assignee,
                deadline,
                source,
                now,
                now,
                completed_at,
            ],
        )?;

        Ok(KanbanItem {
            ticket_id,
            project: project.to_string(),
            title: title.to_string(),
            description: description.map(str::to_string),
            status,
            priority,
            assignee: assignee.map(str::to_string),
            deadline: deadline.map(str::to_string),
            source: source.map(str::to_string),
            created_at: now.clone(),
            updated_at: now,
            completed_at,
            notes: vec![],
        })
    }

    /// List kanban items with optional filters.
    pub fn list(
        &self,
        project: Option<&str>,
        status: Option<&str>,
        priority: Option<&str>,
        assignee: Option<&str>,
        include_done: bool,
    ) -> Result<Vec<KanbanItem>, KanbanError> {
        let conn = self.conn()?;

        let mut conditions: Vec<String> = vec![];
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![];
        let mut param_idx = 1usize;

        if !include_done {
            conditions.push(format!("status != ?{param_idx}"));
            params.push(Box::new("done".to_string()));
            param_idx += 1;
        }
        if let Some(p) = project {
            conditions.push(format!("project = ?{param_idx}"));
            params.push(Box::new(p.to_string()));
            param_idx += 1;
        }
        if let Some(s) = status {
            conditions.push(format!("status = ?{param_idx}"));
            params.push(Box::new(s.to_string()));
            param_idx += 1;
        }
        if let Some(p) = priority {
            conditions.push(format!("priority = ?{param_idx}"));
            params.push(Box::new(p.to_string()));
            param_idx += 1;
        }
        if let Some(a) = assignee {
            conditions.push(format!("assignee = ?{param_idx}"));
            params.push(Box::new(a.to_string()));
            // param_idx would increment here but it's the last use
            let _ = param_idx;
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        let sql = format!(
            "SELECT ticket_id, project, title, description, status, priority,
                    assignee, deadline, source, created_at, updated_at, completed_at
             FROM kanban_items
             {where_clause}
             ORDER BY
                CASE priority
                    WHEN 'urgent' THEN 0
                    WHEN 'high'   THEN 1
                    WHEN 'medium' THEN 2
                    WHEN 'low'    THEN 3
                    ELSE 4
                END,
                updated_at DESC"
        );

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();

        let mut stmt = conn.prepare(&sql)?;
        let items: Vec<KanbanItem> = stmt
            .query_map(param_refs.as_slice(), |row| {
                Ok(KanbanItem {
                    ticket_id: row.get(0)?,
                    project: row.get(1)?,
                    title: row.get(2)?,
                    description: row.get(3)?,
                    status: row.get(4)?,
                    priority: row.get(5)?,
                    assignee: row.get(6)?,
                    deadline: row.get(7)?,
                    source: row.get(8)?,
                    created_at: row.get(9)?,
                    updated_at: row.get(10)?,
                    completed_at: row.get(11)?,
                    notes: vec![],
                })
            })?
            .collect::<Result<_, _>>()?;

        items
            .into_iter()
            .map(|mut item| {
                item.notes = self.load_notes_with_conn(&conn, &item.ticket_id)?;
                Ok(item)
            })
            .collect()
    }

    /// Load notes for a ticket (uses an already-locked connection).
    fn load_notes_with_conn(
        &self,
        conn: &Connection,
        ticket_id: &str,
    ) -> Result<Vec<KanbanNote>, KanbanError> {
        let mut stmt = conn.prepare(
            "SELECT id, text, author, created_at
             FROM kanban_notes
             WHERE ticket_id = ?1
             ORDER BY created_at DESC",
        )?;
        let notes = stmt
            .query_map(rusqlite::params![ticket_id], |row| {
                Ok(KanbanNote {
                    id: row.get(0)?,
                    text: row.get(1)?,
                    author: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(notes)
    }

    /// Fetch a single item by ticket_id (uses an already-locked connection).
    fn get_item_with_conn(
        &self,
        conn: &Connection,
        ticket_id: &str,
    ) -> Result<KanbanItem, KanbanError> {
        let item: Option<KanbanItem> = conn
            .query_row(
                "SELECT ticket_id, project, title, description, status, priority,
                        assignee, deadline, source, created_at, updated_at, completed_at
                 FROM kanban_items
                 WHERE ticket_id = ?1",
                rusqlite::params![ticket_id],
                |row| {
                    Ok(KanbanItem {
                        ticket_id: row.get(0)?,
                        project: row.get(1)?,
                        title: row.get(2)?,
                        description: row.get(3)?,
                        status: row.get(4)?,
                        priority: row.get(5)?,
                        assignee: row.get(6)?,
                        deadline: row.get(7)?,
                        source: row.get(8)?,
                        created_at: row.get(9)?,
                        updated_at: row.get(10)?,
                        completed_at: row.get(11)?,
                        notes: vec![],
                    })
                },
            )
            .optional()?;

        let mut item = item.ok_or_else(|| {
            KanbanError::NotFound(format!("ticket '{ticket_id}' not found"))
        })?;
        item.notes = self.load_notes_with_conn(conn, ticket_id)?;
        Ok(item)
    }

    /// Update fields on an existing item. Only provided (Some) fields are changed.
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
    ) -> Result<KanbanItem, KanbanError> {
        let conn = self.conn()?;

        // Verify exists and get current status.
        let current_status: Option<String> = conn
            .query_row(
                "SELECT status FROM kanban_items WHERE ticket_id = ?1",
                rusqlite::params![ticket_id],
                |row| row.get(0),
            )
            .optional()?;
        let current_status =
            current_status.ok_or_else(|| KanbanError::NotFound(format!("ticket '{ticket_id}' not found")))?;

        if let Some(s) = status {
            match s {
                "backlog" | "todo" | "in_progress" | "review" | "done" => {}
                other => {
                    return Err(KanbanError::InvalidInput(format!(
                        "invalid status '{other}'; must be one of: backlog, todo, in_progress, review, done"
                    )))
                }
            }
        }
        if let Some(p) = priority {
            match p {
                "low" | "medium" | "high" | "urgent" => {}
                other => {
                    return Err(KanbanError::InvalidInput(format!(
                        "invalid priority '{other}'; must be one of: low, medium, high, urgent"
                    )))
                }
            }
        }

        let now = chrono::Utc::now().to_rfc3339();

        let mut sets: Vec<String> = vec![];
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![];
        let mut param_idx = 1usize;

        if let Some(v) = title {
            sets.push(format!("title = ?{param_idx}"));
            params.push(Box::new(v.to_string()));
            param_idx += 1;
        }
        if let Some(v) = description {
            sets.push(format!("description = ?{param_idx}"));
            params.push(Box::new(v.to_string()));
            param_idx += 1;
        }
        if let Some(v) = status {
            sets.push(format!("status = ?{param_idx}"));
            params.push(Box::new(v.to_string()));
            param_idx += 1;

            // completed_at logic
            if v == "done" && current_status != "done" {
                sets.push(format!("completed_at = ?{param_idx}"));
                params.push(Box::new(now.clone()));
                param_idx += 1;
            } else if v != "done" && current_status == "done" {
                sets.push(format!("completed_at = ?{param_idx}"));
                params.push(Box::new(Option::<String>::None));
                param_idx += 1;
            }
        }
        if let Some(v) = priority {
            sets.push(format!("priority = ?{param_idx}"));
            params.push(Box::new(v.to_string()));
            param_idx += 1;
        }
        if let Some(v) = assignee {
            sets.push(format!("assignee = ?{param_idx}"));
            params.push(Box::new(v.to_string()));
            param_idx += 1;
        }
        if let Some(v) = deadline {
            sets.push(format!("deadline = ?{param_idx}"));
            params.push(Box::new(v.to_string()));
            param_idx += 1;
        }

        // Always update updated_at.
        sets.push(format!("updated_at = ?{param_idx}"));
        params.push(Box::new(now));
        param_idx += 1;

        // ticket_id param at the end for WHERE clause.
        params.push(Box::new(ticket_id.to_string()));

        let sql = format!(
            "UPDATE kanban_items SET {} WHERE ticket_id = ?{param_idx}",
            sets.join(", ")
        );

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        conn.execute(&sql, param_refs.as_slice())?;

        self.get_item_with_conn(&conn, ticket_id)
    }

    /// Move an item to a new status, auto-logging the transition as a note.
    pub fn move_item(
        &self,
        ticket_id: &str,
        new_status: &str,
    ) -> Result<(KanbanItem, String), KanbanError> {
        match new_status {
            "backlog" | "todo" | "in_progress" | "review" | "done" => {}
            other => {
                return Err(KanbanError::InvalidInput(format!(
                    "invalid status '{other}'; must be one of: backlog, todo, in_progress, review, done"
                )))
            }
        }

        let conn = self.conn()?;
        let now = chrono::Utc::now().to_rfc3339();

        let old_status: Option<String> = conn
            .query_row(
                "SELECT status FROM kanban_items WHERE ticket_id = ?1",
                rusqlite::params![ticket_id],
                |row| row.get(0),
            )
            .optional()?;
        let old_status =
            old_status.ok_or_else(|| KanbanError::NotFound(format!("ticket '{ticket_id}' not found")))?;

        let completed_at: Option<String> = if new_status == "done" {
            Some(now.clone())
        } else {
            None
        };

        conn.execute(
            "UPDATE kanban_items SET status = ?1, updated_at = ?2, completed_at = ?3 WHERE ticket_id = ?4",
            rusqlite::params![new_status, now, completed_at, ticket_id],
        )?;

        let transition = format!("Status: {old_status} → {new_status}");
        conn.execute(
            "INSERT INTO kanban_notes (ticket_id, text, author, created_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![ticket_id, transition, Option::<String>::None, now],
        )?;

        let item = self.get_item_with_conn(&conn, ticket_id)?;
        Ok((item, transition))
    }

    /// Append a note to an item and return the updated item.
    pub fn add_note(
        &self,
        ticket_id: &str,
        text: &str,
        author: Option<&str>,
    ) -> Result<KanbanItem, KanbanError> {
        let conn = self.conn()?;
        let now = chrono::Utc::now().to_rfc3339();

        // Verify exists.
        let exists: Option<String> = conn
            .query_row(
                "SELECT ticket_id FROM kanban_items WHERE ticket_id = ?1",
                rusqlite::params![ticket_id],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(KanbanError::NotFound(format!("ticket '{ticket_id}' not found")));
        }

        conn.execute(
            "INSERT INTO kanban_notes (ticket_id, text, author, created_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![ticket_id, text, author, now],
        )?;
        conn.execute(
            "UPDATE kanban_items SET updated_at = ?1 WHERE ticket_id = ?2",
            rusqlite::params![now, ticket_id],
        )?;

        self.get_item_with_conn(&conn, ticket_id)
    }

    /// Run a named dynamic query against kanban_items.
    pub fn query(
        &self,
        question: &str,
        queries: &HashMap<String, String>,
    ) -> Result<Vec<KanbanItem>, KanbanError> {
        let where_clause = queries.get(question).ok_or_else(|| {
            let mut names: Vec<&str> = queries.keys().map(String::as_str).collect();
            names.sort();
            KanbanError::InvalidInput(format!(
                "unknown query '{question}'; available: {}",
                names.join(", ")
            ))
        })?;

        let sql = format!(
            "SELECT ticket_id, project, title, description, status, priority,
                    assignee, deadline, source, created_at, updated_at, completed_at
             FROM kanban_items
             WHERE {where_clause}
             ORDER BY updated_at DESC"
        );

        let conn = self.conn()?;
        let mut stmt = conn.prepare(&sql)?;
        let items: Vec<KanbanItem> = stmt
            .query_map([], |row| {
                Ok(KanbanItem {
                    ticket_id: row.get(0)?,
                    project: row.get(1)?,
                    title: row.get(2)?,
                    description: row.get(3)?,
                    status: row.get(4)?,
                    priority: row.get(5)?,
                    assignee: row.get(6)?,
                    deadline: row.get(7)?,
                    source: row.get(8)?,
                    created_at: row.get(9)?,
                    updated_at: row.get(10)?,
                    completed_at: row.get(11)?,
                    notes: vec![],
                })
            })?
            .collect::<Result<_, _>>()?;

        items
            .into_iter()
            .map(|mut item| {
                item.notes = self.load_notes_with_conn(&conn, &item.ticket_id)?;
                Ok(item)
            })
            .collect()
    }

    /// Ensure a project exists in kanban_projects, creating it if absent.
    /// Returns (prefix, next_id).
    fn ensure_project(
        &self,
        conn: &Connection,
        project: &str,
        domain: &str,
        config_prefixes: &HashMap<String, String>,
    ) -> Result<(String, i64), KanbanError> {
        let existing: Option<(String, i64)> = conn
            .query_row(
                "SELECT prefix, next_id FROM kanban_projects WHERE project = ?1",
                rusqlite::params![project],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        if let Some((prefix, next_id)) = existing {
            return Ok((prefix, next_id));
        }

        // Collect all existing prefixes to avoid collisions.
        let mut stmt = conn.prepare("SELECT prefix FROM kanban_projects")?;
        let existing_prefixes: Vec<String> = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;

        let prefix =
            crate::kanban::prefix::resolve_prefix(project, config_prefixes, &existing_prefixes)
                .ok_or_else(|| {
                    KanbanError::InvalidInput(format!(
                        "could not derive a unique prefix for project '{project}'; \
                         set an explicit prefix in your wardwell config"
                    ))
                })?;

        conn.execute(
            "INSERT INTO kanban_projects (project, prefix, domain, next_id) VALUES (?1, ?2, ?3, 1)",
            rusqlite::params![project, prefix, domain],
        )?;

        Ok((prefix, 1))
    }
}

/// Default kanban query definitions.
pub fn default_kanban_queries() -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("overdue".into(), "status != 'done' AND deadline < date('now')".into());
    m.insert(
        "stale".into(),
        "status != 'done' AND updated_at < datetime('now', '-7 days')".into(),
    );
    m.insert("no_deadline".into(), "status != 'done' AND deadline IS NULL".into());
    m.insert("blocked".into(), "status = 'backlog'".into());
    m.insert("recent".into(), "updated_at > datetime('now', '-2 days')".into());
    m
}

/// Merge config queries over defaults. Config entries override matching defaults;
/// unmentioned defaults survive; new config entries are added.
pub fn merge_kanban_queries(config_queries: &HashMap<String, String>) -> HashMap<String, String> {
    let mut merged = default_kanban_queries();
    for (k, v) in config_queries {
        merged.insert(k.clone(), v.clone());
    }
    merged
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn open_creates_tables() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("kanban.db");
        let store = KanbanStore::open(&db_path).unwrap();

        let conn = store.conn().unwrap();
        let tables: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'kanban_%'
                     ORDER BY name",
                )
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };

        assert_eq!(
            tables,
            vec!["kanban_items", "kanban_notes", "kanban_projects"]
        );
    }

    #[test]
    fn open_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("kanban.db");

        let first = KanbanStore::open(&db_path);
        assert!(first.is_ok(), "{first:?}");
        drop(first);

        let second = KanbanStore::open(&db_path);
        assert!(second.is_ok(), "{second:?}");
    }

    #[test]
    fn create_item_basic() {
        let dir = tempfile::tempdir().unwrap();
        let store = KanbanStore::open(&dir.path().join("kanban.db")).unwrap();
        let prefixes = HashMap::new();

        let item = store
            .create_item(
                "Do the thing",
                "shulops",
                "work",
                None,
                None,
                None,
                None,
                None,
                None,
                &prefixes,
            )
            .unwrap();

        assert_eq!(item.ticket_id, "SH-1");
        assert_eq!(item.status, "backlog");
        assert_eq!(item.priority, "medium");
        assert_eq!(item.project, "shulops");
        assert_eq!(item.title, "Do the thing");
        assert!(item.completed_at.is_none());
        assert!(item.notes.is_empty());
    }

    #[test]
    fn create_item_increments_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = KanbanStore::open(&dir.path().join("kanban.db")).unwrap();
        let prefixes = HashMap::new();

        let first = store
            .create_item("First", "shulops", "work", None, None, None, None, None, None, &prefixes)
            .unwrap();
        let second = store
            .create_item("Second", "shulops", "work", None, None, None, None, None, None, &prefixes)
            .unwrap();

        assert_eq!(first.ticket_id, "SH-1");
        assert_eq!(second.ticket_id, "SH-2");
    }

    #[test]
    fn create_item_with_all_fields() {
        let dir = tempfile::tempdir().unwrap();
        let store = KanbanStore::open(&dir.path().join("kanban.db")).unwrap();
        let mut prefixes = HashMap::new();
        prefixes.insert("myproject".to_string(), "MP".to_string());

        let item = store
            .create_item(
                "Full item",
                "myproject",
                "personal",
                Some("A detailed description"),
                Some("done"),
                Some("high"),
                Some("alice"),
                Some("2026-05-01"),
                Some("github"),
                &prefixes,
            )
            .unwrap();

        assert_eq!(item.ticket_id, "MP-1");
        assert_eq!(item.description.as_deref(), Some("A detailed description"));
        assert_eq!(item.status, "done");
        assert_eq!(item.priority, "high");
        assert_eq!(item.assignee.as_deref(), Some("alice"));
        assert_eq!(item.deadline.as_deref(), Some("2026-05-01"));
        assert_eq!(item.source.as_deref(), Some("github"));
        assert!(item.completed_at.is_some());
    }

    // ---- list tests ----

    fn make_store() -> KanbanStore {
        let dir = tempfile::tempdir().unwrap();
        KanbanStore::open(&dir.path().join("kanban.db")).unwrap()
    }

    #[test]
    fn list_all_items() {
        let store = make_store();
        let p = HashMap::new();
        store.create_item("A", "shulops", "work", None, None, None, None, None, None, &p).unwrap();
        store.create_item("B", "personal", "life", None, None, None, None, None, None, &p).unwrap();
        let items = store.list(None, None, None, None, true).unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn list_filters_by_project() {
        let store = make_store();
        let p = HashMap::new();
        store.create_item("A", "shulops", "work", None, None, None, None, None, None, &p).unwrap();
        store.create_item("B", "personal", "life", None, None, None, None, None, None, &p).unwrap();
        let items = store.list(Some("shulops"), None, None, None, true).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].project, "shulops");
    }

    #[test]
    fn list_excludes_done_by_default() {
        let store = make_store();
        let p = HashMap::new();
        store.create_item("Active", "shulops", "work", None, Some("backlog"), None, None, None, None, &p).unwrap();
        store.create_item("Done", "shulops", "work", None, Some("done"), None, None, None, None, &p).unwrap();
        let items = store.list(None, None, None, None, false).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Active");
    }

    #[test]
    fn list_filters_by_status() {
        let store = make_store();
        let p = HashMap::new();
        store.create_item("Backlog", "shulops", "work", None, Some("backlog"), None, None, None, None, &p).unwrap();
        store.create_item("In Progress", "shulops", "work", None, Some("in_progress"), None, None, None, None, &p).unwrap();
        let items = store.list(None, Some("in_progress"), None, None, true).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].status, "in_progress");
    }

    #[test]
    fn list_filters_by_assignee() {
        let store = make_store();
        let p = HashMap::new();
        store.create_item("Assigned", "shulops", "work", None, None, None, Some("alice"), None, None, &p).unwrap();
        store.create_item("Unassigned", "shulops", "work", None, None, None, None, None, None, &p).unwrap();
        let items = store.list(None, None, None, Some("alice"), true).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].assignee.as_deref(), Some("alice"));
    }

    // ---- update tests ----

    #[test]
    fn update_item_title() {
        let store = make_store();
        let p = HashMap::new();
        let item = store.create_item("Old", "shulops", "work", None, None, None, None, None, None, &p).unwrap();
        let updated = store.update_item(&item.ticket_id, Some("New"), None, None, None, None, None).unwrap();
        assert_eq!(updated.title, "New");
        assert_eq!(updated.ticket_id, item.ticket_id);
    }

    #[test]
    fn update_item_not_found() {
        let store = make_store();
        let result = store.update_item("SH-999", Some("title"), None, None, None, None, None);
        assert!(matches!(result, Err(KanbanError::NotFound(_))));
    }

    // ---- move tests ----

    #[test]
    fn move_item_changes_status() {
        let store = make_store();
        let p = HashMap::new();
        let item = store.create_item("Task", "shulops", "work", None, None, None, None, None, None, &p).unwrap();
        let (moved, transition) = store.move_item(&item.ticket_id, "in_progress").unwrap();
        assert_eq!(moved.status, "in_progress");
        assert!(transition.contains("in_progress"));
    }

    #[test]
    fn move_to_done_sets_completed_at() {
        let store = make_store();
        let p = HashMap::new();
        let item = store.create_item("Task", "shulops", "work", None, None, None, None, None, None, &p).unwrap();
        let (moved, _) = store.move_item(&item.ticket_id, "done").unwrap();
        assert!(moved.completed_at.is_some());
    }

    #[test]
    fn move_from_done_clears_completed_at() {
        let store = make_store();
        let p = HashMap::new();
        let item = store.create_item("Task", "shulops", "work", None, Some("done"), None, None, None, None, &p).unwrap();
        assert!(item.completed_at.is_some());
        let (moved, _) = store.move_item(&item.ticket_id, "in_progress").unwrap();
        assert!(moved.completed_at.is_none());
    }

    // ---- note test ----

    #[test]
    fn add_note_to_item() {
        let store = make_store();
        let p = HashMap::new();
        let item = store.create_item("Task", "shulops", "work", None, None, None, None, None, None, &p).unwrap();
        let with_note = store.add_note(&item.ticket_id, "looks good", Some("bob")).unwrap();
        assert_eq!(with_note.notes.len(), 1);
        assert_eq!(with_note.notes[0].text, "looks good");
        assert_eq!(with_note.notes[0].author.as_deref(), Some("bob"));
    }

    // ---- query tests ----

    #[test]
    fn query_overdue() {
        let store = make_store();
        let p = HashMap::new();
        // Past deadline, non-done → should match overdue
        store
            .create_item("Past", "shulops", "work", None, Some("todo"), None, None, Some("2020-01-01"), None, &p)
            .unwrap();
        // Future deadline → should not match
        store
            .create_item("Future", "shulops", "work", None, Some("todo"), None, None, Some("2099-12-31"), None, &p)
            .unwrap();
        // No deadline → should not match
        store
            .create_item("No deadline", "shulops", "work", None, Some("todo"), None, None, None, None, &p)
            .unwrap();

        let results = store.query("overdue", &default_kanban_queries()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Past");
    }

    #[test]
    fn query_no_deadline() {
        let store = make_store();
        let p = HashMap::new();
        store
            .create_item("Has deadline", "shulops", "work", None, Some("todo"), None, None, Some("2026-12-01"), None, &p)
            .unwrap();
        store
            .create_item("No deadline", "shulops", "work", None, Some("todo"), None, None, None, None, &p)
            .unwrap();

        let results = store.query("no_deadline", &default_kanban_queries()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "No deadline");
    }

    #[test]
    fn query_unknown_returns_error() {
        let store = make_store();
        let result = store.query("nonexistent", &default_kanban_queries());
        assert!(matches!(result, Err(KanbanError::InvalidInput(_))));
    }
}
