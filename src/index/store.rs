use rusqlite::Connection;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

/// Errors from index operations.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("vault error: {0}")]
    Vault(#[from] crate::vault::types::VaultError),

    #[error("index IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("lock poisoned")]
    LockPoisoned,
}

/// SQLite FTS5 index store. Thread-safe via Mutex.
#[derive(Debug)]
pub struct IndexStore {
    conn: Mutex<Connection>,
}

impl IndexStore {
    /// Open (or create) an index at the given path.
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        let conn = Connection::open(path)?;
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;

        let fts_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='vault_search'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|c| c > 0)
            .unwrap_or(false);

        if !fts_exists {
            conn.execute_batch(
                "CREATE VIRTUAL TABLE vault_search USING fts5(
                    path, type, domain, status, confidence, summary, tags, body,
                    tokenize='porter unicode61'
                );"
            )?;
        }

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS vault_meta (
                path TEXT PRIMARY KEY,
                type TEXT NOT NULL,
                domain TEXT,
                status TEXT,
                confidence TEXT,
                updated TEXT,
                summary TEXT,
                related TEXT,
                tags TEXT,
                body_hash TEXT,
                indexed_at TEXT
            );"
        )?;

        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Open an in-memory index (for testing).
    pub fn in_memory() -> Result<Self, IndexError> {
        let conn = Connection::open_in_memory()?;

        conn.execute_batch(
            "CREATE VIRTUAL TABLE vault_search USING fts5(
                path, type, domain, status, confidence, summary, tags, body,
                tokenize='porter unicode61'
            );

            CREATE TABLE vault_meta (
                path TEXT PRIMARY KEY,
                type TEXT NOT NULL,
                domain TEXT,
                status TEXT,
                confidence TEXT,
                updated TEXT,
                summary TEXT,
                related TEXT,
                tags TEXT,
                body_hash TEXT,
                indexed_at TEXT
            );"
        )?;

        Ok(Self { conn: Mutex::new(conn) })
    }

    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, Connection>, IndexError> {
        self.conn.lock().map_err(|_| IndexError::LockPoisoned)
    }

    /// Delete all rows from both tables. Safe to call while other processes hold the db.
    pub fn clear(&self) -> Result<(), IndexError> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM vault_search", [])?;
        conn.execute("DELETE FROM vault_meta", [])?;
        Ok(())
    }

    /// Upsert a vault file into the index. Skips if body hash is unchanged.
    /// Returns true if the file was actually updated.
    pub fn upsert(&self, vf: &crate::vault::types::VaultFile, vault_root: &Path) -> Result<bool, IndexError> {
        let abs_path = vf
            .path
            .strip_prefix(vault_root)
            .unwrap_or(&vf.path)
            .to_string_lossy()
            .to_string();

        let new_hash = crate::index::builder::compute_hash(&vf.body);
        let conn = self.lock()?;

        // Check if hash is unchanged
        let existing_hash: Option<String> = conn.query_row(
            "SELECT body_hash FROM vault_meta WHERE path = ?1",
            rusqlite::params![abs_path],
            |row| row.get(0),
        ).ok();

        if existing_hash.as_deref() == Some(new_hash.as_str()) {
            return Ok(false);
        }

        // Remove old entries
        conn.execute("DELETE FROM vault_search WHERE path = ?1", rusqlite::params![abs_path])?;
        conn.execute("DELETE FROM vault_meta WHERE path = ?1", rusqlite::params![abs_path])?;

        // Insert fresh
        let fm = &vf.frontmatter;
        let file_type = fm.file_type.to_string();
        let domain = fm.domain.as_deref().unwrap_or("");
        let status = fm.status.as_ref().map(|s| s.to_string()).unwrap_or_default();
        let confidence = fm.confidence.as_ref().map(|c| c.to_string()).unwrap_or_default();
        let summary = fm.summary.as_deref().unwrap_or("");
        let tags = fm.tags.join(", ");
        let updated = fm.updated.map(|d| d.to_string()).unwrap_or_default();
        let related = fm.related.join(", ");
        let indexed_at = chrono::Utc::now().to_rfc3339();

        conn.execute(
            "INSERT INTO vault_search (path, type, domain, status, confidence, summary, tags, body)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![abs_path, file_type, domain, status, confidence, summary, tags, vf.body],
        )?;

        conn.execute(
            "INSERT OR REPLACE INTO vault_meta (path, type, domain, status, confidence, updated, summary, related, tags, body_hash, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![abs_path, file_type, domain, status, confidence, updated, summary, related, tags, new_hash, indexed_at],
        )?;

        Ok(true)
    }

    /// Remove a file from the index by its path.
    pub fn remove(&self, path: &str) -> Result<(), IndexError> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM vault_search WHERE path = ?1", rusqlite::params![path])?;
        conn.execute("DELETE FROM vault_meta WHERE path = ?1", rusqlite::params![path])?;
        Ok(())
    }

    /// Remove all indexed paths that are NOT in the given set.
    /// Returns the number of entries removed.
    pub fn remove_stale(&self, live_paths: &std::collections::HashSet<String>) -> Result<usize, IndexError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT path FROM vault_meta")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut stale: Vec<String> = Vec::new();
        for row in rows {
            if let Ok(path) = row
                && !live_paths.contains(&path) {
                    stale.push(path);
                }
        }
        drop(stmt);
        for path in &stale {
            conn.execute("DELETE FROM vault_search WHERE path = ?1", rusqlite::params![path])?;
            conn.execute("DELETE FROM vault_meta WHERE path = ?1", rusqlite::params![path])?;
        }
        Ok(stale.len())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory() {
        let store = IndexStore::in_memory();
        assert!(store.is_ok(), "{store:?}");
    }

    #[test]
    fn open_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let store = IndexStore::open(&db_path);
        assert!(store.is_ok(), "{store:?}");
        assert!(db_path.exists());
    }

    #[test]
    fn upsert_skips_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.md");
        std::fs::write(&file_path, "---\ntype: project\n---\nbody content\n").ok();

        let store = IndexStore::in_memory().unwrap();
        let vf = crate::vault::reader::read_file(&file_path).unwrap();

        let updated = store.upsert(&vf, dir.path());
        assert!(updated.is_ok(), "{updated:?}");
        assert_eq!(updated.ok(), Some(true));

        let updated = store.upsert(&vf, dir.path());
        assert!(updated.is_ok(), "{updated:?}");
        assert_eq!(updated.ok(), Some(false));
    }

    #[test]
    fn upsert_updates_changed() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.md");
        std::fs::write(&file_path, "---\ntype: project\n---\noriginal body\n").ok();

        let store = IndexStore::in_memory().unwrap();
        let vf = crate::vault::reader::read_file(&file_path).unwrap();
        store.upsert(&vf, dir.path()).ok();

        std::fs::write(&file_path, "---\ntype: project\n---\nupdated body\n").ok();
        let vf2 = crate::vault::reader::read_file(&file_path).unwrap();
        let updated = store.upsert(&vf2, dir.path());
        assert!(updated.is_ok(), "{updated:?}");
        assert_eq!(updated.ok(), Some(true));
    }

    #[test]
    fn remove_deletes_from_index() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.md");
        std::fs::write(&file_path, "---\ntype: insight\nsummary: Test insight\n---\nbody\n").ok();

        let store = IndexStore::in_memory().unwrap();
        let vf = crate::vault::reader::read_file(&file_path).unwrap();
        store.upsert(&vf, dir.path()).ok();

        let conn = store.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM vault_meta WHERE path = 'test.md'", [], |row| row.get(0)
        ).unwrap_or(0);
        assert_eq!(count, 1);
        drop(conn);

        store.remove("test.md").ok();

        let conn = store.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM vault_meta WHERE path = 'test.md'", [], |row| row.get(0)
        ).unwrap_or(0);
        assert_eq!(count, 0);
    }
}
