use rusqlite::Connection;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

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
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

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
}
