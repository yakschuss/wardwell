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
}
