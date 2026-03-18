use crate::index::chunk::Chunk;
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

    #[error("embedding error: {0}")]
    Embedding(String),

    #[error("lock poisoned")]
    LockPoisoned,
}

/// SQLite FTS5 index store. Thread-safe via Mutex.
#[derive(Debug)]
pub struct IndexStore {
    conn: Mutex<Connection>,
}

/// Register sqlite-vec extension globally. Must be called once before opening any connection.
/// Safe to call multiple times (idempotent).
pub fn register_vec_extension() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // Safety: sqlite3_vec_init is a well-maintained C extension from the sqlite-vec crate.
        // sqlite3_auto_extension registers it for all future connections.
        unsafe {
            #[allow(clippy::missing_transmute_annotations)]
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

impl IndexStore {
    /// Open (or create) an index at the given path.
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        register_vec_extension();
        let conn = Connection::open(path)?;
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;

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

        // Chunk tables for hybrid search
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS vault_chunks (
                chunk_id TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                chunk_index INTEGER NOT NULL,
                heading TEXT,
                body TEXT NOT NULL,
                body_hash TEXT NOT NULL,
                UNIQUE(path, chunk_index)
            );"
        )?;

        let chunk_fts_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='chunk_search'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|c| c > 0)
            .unwrap_or(false);

        if !chunk_fts_exists {
            conn.execute_batch(
                "CREATE VIRTUAL TABLE chunk_search USING fts5(
                    chunk_id, path, heading, body,
                    tokenize='porter unicode61'
                );"
            )?;
        }

        // sqlite-vec virtual table for embeddings
        let vec_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='chunk_vec'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|c| c > 0)
            .unwrap_or(false);

        if !vec_exists {
            conn.execute_batch(
                "CREATE VIRTUAL TABLE chunk_vec USING vec0(
                    chunk_id TEXT PRIMARY KEY,
                    embedding FLOAT[384]
                );"
            )?;
        }

        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Open an in-memory index (for testing).
    pub fn in_memory() -> Result<Self, IndexError> {
        register_vec_extension();
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
            );

            CREATE TABLE vault_chunks (
                chunk_id TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                chunk_index INTEGER NOT NULL,
                heading TEXT,
                body TEXT NOT NULL,
                body_hash TEXT NOT NULL,
                UNIQUE(path, chunk_index)
            );

            CREATE VIRTUAL TABLE chunk_search USING fts5(
                chunk_id, path, heading, body,
                tokenize='porter unicode61'
            );

            CREATE VIRTUAL TABLE chunk_vec USING vec0(
                chunk_id TEXT PRIMARY KEY,
                embedding FLOAT[384]
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
        conn.execute("DELETE FROM chunk_search", [])?;
        conn.execute("DELETE FROM vault_chunks", [])?;
        conn.execute("DELETE FROM chunk_vec", [])?;
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

    /// Insert/update chunks for a file. Returns IDs of chunks whose body changed (need re-embedding).
    pub fn upsert_chunks(&self, path: &str, chunks: &[Chunk]) -> Result<Vec<String>, IndexError> {
        let conn = self.lock()?;
        let mut changed_ids = Vec::new();

        // Remove old chunks beyond current count
        conn.execute(
            "DELETE FROM chunk_search WHERE path = ?1",
            rusqlite::params![path],
        )?;

        // Collect existing hashes for comparison
        let mut existing: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT chunk_id, body_hash FROM vault_chunks WHERE path = ?1"
            )?;
            let rows = stmt.query_map(rusqlite::params![path], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for (id, hash) in rows.flatten() {
                existing.insert(id, hash);
            }
        }

        // Remove stale chunks (indexes beyond current count)
        conn.execute(
            "DELETE FROM vault_chunks WHERE path = ?1 AND chunk_index >= ?2",
            rusqlite::params![path, chunks.len() as i64],
        )?;
        conn.execute(
            "DELETE FROM chunk_vec WHERE chunk_id IN (SELECT chunk_id FROM vault_chunks WHERE path = ?1 AND chunk_index >= ?2)",
            rusqlite::params![path, chunks.len() as i64],
        ).ok(); // chunk_vec may not have entries yet

        for chunk in chunks {
            let chunk_id = format!("{path}::{}", chunk.index);
            let new_hash = crate::index::builder::compute_hash(&chunk.body);

            // Check if body changed
            let body_changed = existing.get(&chunk_id) != Some(&new_hash);

            // Upsert into vault_chunks
            conn.execute(
                "INSERT OR REPLACE INTO vault_chunks (chunk_id, path, chunk_index, heading, body, body_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![chunk_id, path, chunk.index as i64, chunk.heading, chunk.body, new_hash],
            )?;

            // Always re-insert into chunk_search FTS (we deleted all above)
            conn.execute(
                "INSERT INTO chunk_search (chunk_id, path, heading, body)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![chunk_id, path, chunk.heading.as_deref().unwrap_or(""), chunk.body],
            )?;

            if body_changed {
                changed_ids.push(chunk_id);
            }
        }

        Ok(changed_ids)
    }

    /// Insert embeddings for chunks. `ids` and `vecs` must have the same length.
    pub fn upsert_embeddings(&self, ids: &[String], vecs: &[Vec<f32>]) -> Result<(), IndexError> {
        let conn = self.lock()?;
        for (id, vec) in ids.iter().zip(vecs.iter()) {
            // Remove old embedding if exists
            conn.execute(
                "DELETE FROM chunk_vec WHERE chunk_id = ?1",
                rusqlite::params![id],
            ).ok();

            // Insert new embedding as raw bytes
            let bytes: Vec<u8> = vec.iter().flat_map(|f| f.to_le_bytes()).collect();
            conn.execute(
                "INSERT INTO chunk_vec (chunk_id, embedding) VALUES (?1, ?2)",
                rusqlite::params![id, bytes],
            )?;
        }
        Ok(())
    }

    /// Remove all chunks and embeddings for a file path.
    pub fn remove_chunks(&self, path: &str) -> Result<(), IndexError> {
        let conn = self.lock()?;
        // Get chunk_ids first for vec cleanup
        let mut stmt = conn.prepare("SELECT chunk_id FROM vault_chunks WHERE path = ?1")?;
        let ids: Vec<String> = stmt
            .query_map(rusqlite::params![path], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);

        for id in &ids {
            conn.execute("DELETE FROM chunk_vec WHERE chunk_id = ?1", rusqlite::params![id]).ok();
        }
        conn.execute("DELETE FROM chunk_search WHERE path = ?1", rusqlite::params![path])?;
        conn.execute("DELETE FROM vault_chunks WHERE path = ?1", rusqlite::params![path])?;
        Ok(())
    }

    /// KNN vector search on chunk_vec. Returns (chunk_id, distance) pairs, lowest distance first.
    pub fn vector_search(
        &self,
        query_vec: &[f32],
        limit: usize,
        domains: Option<&[String]>,
    ) -> Result<Vec<(String, f32)>, IndexError> {
        let conn = self.lock()?;
        let query_bytes: Vec<u8> = query_vec.iter().flat_map(|f| f.to_le_bytes()).collect();

        let (sql, params): (String, Vec<Box<dyn rusqlite::types::ToSql>>) = if let Some(domains) = domains {
            if domains.is_empty() {
                return Ok(Vec::new());
            }
            let placeholders: Vec<String> = (0..domains.len()).map(|i| format!("?{}", i + 3)).collect();
            let sql = format!(
                "SELECT cv.chunk_id, cv.distance
                 FROM chunk_vec cv
                 JOIN vault_chunks vc ON cv.chunk_id = vc.chunk_id
                 JOIN vault_meta vm ON vc.path = vm.path
                 WHERE cv.embedding MATCH ?1
                 AND k = ?2
                 AND vm.domain IN ({})
                 ORDER BY cv.distance",
                placeholders.join(", ")
            );
            let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![
                Box::new(query_bytes),
                Box::new(limit as i64),
            ];
            for d in domains {
                params.push(Box::new(d.clone()));
            }
            (sql, params)
        } else {
            let sql = "SELECT chunk_id, distance
                       FROM chunk_vec
                       WHERE embedding MATCH ?1
                       AND k = ?2
                       ORDER BY distance".to_string();
            let params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![
                Box::new(query_bytes),
                Box::new(limit as i64),
            ];
            (sql, params)
        };

        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f32>(1)?))
        })?;

        let mut results = Vec::new();
        for r in rows.flatten() {
            results.push(r);
        }
        Ok(results)
    }

    /// FTS5 search on chunk_search. Returns (chunk_id, rank) pairs.
    pub fn chunk_fts_search(
        &self,
        query: &str,
        limit: usize,
        domains: Option<&[String]>,
    ) -> Result<Vec<(String, f64)>, IndexError> {
        let conn = self.lock()?;
        let quoted_query = format!("\"{}\"", query.replace('"', "\"\""));

        let (sql, params): (String, Vec<Box<dyn rusqlite::types::ToSql>>) = if let Some(domains) = domains {
            if domains.is_empty() {
                return Ok(Vec::new());
            }
            let placeholders: Vec<String> = (0..domains.len()).map(|i| format!("?{}", i + 2)).collect();
            let sql = format!(
                "SELECT cs.chunk_id, cs.rank
                 FROM chunk_search cs
                 JOIN vault_meta vm ON cs.path = vm.path
                 WHERE chunk_search MATCH ?1
                 AND vm.domain IN ({})
                 ORDER BY cs.rank
                 LIMIT {limit}",
                placeholders.join(", ")
            );
            let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(quoted_query)];
            for d in domains {
                params.push(Box::new(d.clone()));
            }
            (sql, params)
        } else {
            let sql = format!(
                "SELECT chunk_id, rank
                 FROM chunk_search
                 WHERE chunk_search MATCH ?1
                 ORDER BY rank
                 LIMIT {limit}"
            );
            let params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(quoted_query)];
            (sql, params)
        };

        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })?;

        let mut results = Vec::new();
        for r in rows.flatten() {
            results.push(r);
        }
        Ok(results)
    }

    /// Get a chunk by ID. Returns (path, chunk_index, heading, body).
    pub fn get_chunk(&self, chunk_id: &str) -> Result<(String, usize, Option<String>, String), IndexError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT path, chunk_index, heading, body FROM vault_chunks WHERE chunk_id = ?1",
            rusqlite::params![chunk_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, usize>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        ).map_err(IndexError::from)
    }

    /// Get frontmatter for a file path from vault_meta.
    pub fn get_frontmatter(&self, path: &str) -> Result<crate::vault::types::Frontmatter, IndexError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT type, domain, status, confidence, updated, summary, related, tags
             FROM vault_meta WHERE path = ?1",
            rusqlite::params![path],
            |row| {
                let file_type: String = row.get(0)?;
                let domain: Option<String> = row.get(1)?;
                let status: Option<String> = row.get(2)?;
                let confidence: Option<String> = row.get(3)?;
                let updated: Option<String> = row.get(4)?;
                let summary: Option<String> = row.get(5)?;
                let related: Option<String> = row.get(6)?;
                let tags: Option<String> = row.get(7)?;

                Ok(crate::vault::types::Frontmatter {
                    file_type: crate::index::fts::parse_vault_type(&file_type),
                    domain,
                    status: status.as_deref().and_then(crate::index::fts::parse_status),
                    confidence: confidence.as_deref().and_then(crate::index::fts::parse_confidence),
                    updated: updated.and_then(|s| chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok()),
                    summary,
                    related: related.map(|s| s.split(", ").filter(|s| !s.is_empty()).map(String::from).collect()).unwrap_or_default(),
                    tags: tags.map(|s| s.split(", ").filter(|s| !s.is_empty()).map(String::from).collect()).unwrap_or_default(),
                    can_read: Vec::new(),
                })
            },
        ).map_err(IndexError::from)
    }

    /// Remove a file from the index by its path.
    pub fn remove(&self, path: &str) -> Result<(), IndexError> {
        // Remove chunks first (drops MutexGuard between calls)
        self.remove_chunks(path)?;
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
        // Clean up chunk tables for stale paths
        for path in &stale {
            // Get chunk IDs for vec cleanup
            let mut cstmt = conn.prepare("SELECT chunk_id FROM vault_chunks WHERE path = ?1")?;
            let ids: Vec<String> = cstmt
                .query_map(rusqlite::params![path], |row| row.get::<_, String>(0))?
                .filter_map(|r| r.ok())
                .collect();
            drop(cstmt);
            for id in &ids {
                conn.execute("DELETE FROM chunk_vec WHERE chunk_id = ?1", rusqlite::params![id]).ok();
            }
            conn.execute("DELETE FROM chunk_search WHERE path = ?1", rusqlite::params![path])?;
            conn.execute("DELETE FROM vault_chunks WHERE path = ?1", rusqlite::params![path])?;
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
