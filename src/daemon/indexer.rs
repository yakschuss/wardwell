use rusqlite::Connection;
use serde::Deserialize;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// Errors from session indexing.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("lock poisoned")]
    LockPoisoned,
}

/// Metadata extracted from a single session JSONL file.
#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub session_id: String,
    pub project_dir: String,
    pub project_path: String,
    pub domain: Option<String>,
    pub message_count: i64,
    pub user_message_count: i64,
    pub assistant_message_count: i64,
    pub first_message_at: Option<String>,
    pub last_message_at: Option<String>,
    pub file_size: i64,
    pub file_hash: String,
}

/// A single message entry from the JSONL transcript (only fields we need).
#[derive(Deserialize)]
struct RawMessage {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    message: Option<MessageContent>,
    #[serde(default)]
    content: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct MessageContent {
    #[serde(default)]
    content: Option<serde_json::Value>,
}

/// Session index store backed by SQLite.
pub struct SessionStore {
    conn: Mutex<Connection>,
}

impl SessionStore {
    pub fn open(path: &Path) -> Result<Self, SessionError> {
        let conn = Connection::open(path)?;
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                session_id TEXT PRIMARY KEY,
                project_dir TEXT NOT NULL,
                project_path TEXT NOT NULL,
                domain TEXT,
                message_count INTEGER NOT NULL DEFAULT 0,
                user_message_count INTEGER NOT NULL DEFAULT 0,
                assistant_message_count INTEGER NOT NULL DEFAULT 0,
                first_message_at TEXT,
                last_message_at TEXT,
                file_size INTEGER NOT NULL DEFAULT 0,
                file_hash TEXT NOT NULL,
                summarized INTEGER NOT NULL DEFAULT 0,
                indexed_at TEXT NOT NULL
            );"
        )?;

        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn open_in_memory() -> Result<Self, SessionError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE sessions (
                session_id TEXT PRIMARY KEY,
                project_dir TEXT NOT NULL,
                project_path TEXT NOT NULL,
                domain TEXT,
                message_count INTEGER NOT NULL DEFAULT 0,
                user_message_count INTEGER NOT NULL DEFAULT 0,
                assistant_message_count INTEGER NOT NULL DEFAULT 0,
                first_message_at TEXT,
                last_message_at TEXT,
                file_size INTEGER NOT NULL DEFAULT 0,
                file_hash TEXT NOT NULL,
                summarized INTEGER NOT NULL DEFAULT 0,
                indexed_at TEXT NOT NULL
            );"
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, SessionError> {
        self.conn.lock().map_err(|_| SessionError::LockPoisoned)
    }

    /// Upsert a session. Returns true if it was actually inserted/updated.
    pub fn upsert(&self, meta: &SessionMeta) -> Result<bool, SessionError> {
        let conn = self.lock()?;

        // Check if hash is unchanged
        let existing_hash: Option<String> = conn.query_row(
            "SELECT file_hash FROM sessions WHERE session_id = ?1",
            rusqlite::params![meta.session_id],
            |row| row.get(0),
        ).ok();

        if existing_hash.as_deref() == Some(meta.file_hash.as_str()) {
            return Ok(false);
        }

        let indexed_at = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT OR REPLACE INTO sessions
                (session_id, project_dir, project_path, domain,
                 message_count, user_message_count, assistant_message_count,
                 first_message_at, last_message_at, file_size, file_hash,
                 summarized, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0, ?12)",
            rusqlite::params![
                meta.session_id, meta.project_dir, meta.project_path, meta.domain,
                meta.message_count, meta.user_message_count, meta.assistant_message_count,
                meta.first_message_at, meta.last_message_at, meta.file_size, meta.file_hash,
                indexed_at
            ],
        )?;

        Ok(true)
    }

    /// Get all sessions that haven't been summarized yet.
    pub fn unsummarized(&self) -> Result<Vec<UnsummarizedSession>, SessionError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT session_id, project_dir, project_path, domain, user_message_count, file_size
             FROM sessions WHERE summarized = 0
             ORDER BY last_message_at DESC"
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(UnsummarizedSession {
                session_id: row.get(0)?,
                project_dir: row.get(1)?,
                project_path: row.get(2)?,
                domain: row.get(3)?,
                user_message_count: row.get(4)?,
                file_size: row.get(5)?,
            })
        })?;

        let mut results = Vec::new();
        for r in rows.flatten() {
            results.push(r);
        }
        Ok(results)
    }

    /// Mark a session as summarized.
    pub fn mark_summarized(&self, session_id: &str) -> Result<(), SessionError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE sessions SET summarized = 1 WHERE session_id = ?1",
            rusqlite::params![session_id],
        )?;
        Ok(())
    }

    /// Reset all sessions to unsummarized state.
    pub fn reset_summarized(&self) -> Result<usize, SessionError> {
        let conn = self.lock()?;
        let count = conn.execute("UPDATE sessions SET summarized = 0 WHERE summarized = 1", [])?;
        Ok(count)
    }

    /// Get total session count.
    pub fn count(&self) -> Result<i64, SessionError> {
        let conn = self.lock()?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
        Ok(count)
    }
}

#[derive(Debug)]
pub struct UnsummarizedSession {
    pub session_id: String,
    pub project_dir: String,
    pub project_path: String,
    pub domain: Option<String>,
    pub user_message_count: i64,
    pub file_size: i64,
}

/// Stats from an indexing run.
#[derive(Debug, Default)]
pub struct IndexStats {
    pub scanned: usize,
    pub indexed: usize,
    pub skipped: usize,
    pub errors: usize,
}

/// Walk all session sources and index session metadata.
pub fn index_sessions(
    session_sources: &[PathBuf],
    store: &SessionStore,
    domains: &[crate::domain::model::Domain],
) -> Result<IndexStats, SessionError> {
    let mut stats = IndexStats::default();

    for source in session_sources {
        if !source.exists() {
            continue;
        }

        let entries = match std::fs::read_dir(source) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let project_dir_path = entry.path();
            if !project_dir_path.is_dir() {
                continue;
            }

            let project_dir_name = entry.file_name().to_string_lossy().to_string();
            let project_path = decode_project_dir(&project_dir_name);

            // Resolve domain from project path
            let domain = resolve_domain(&project_path, domains);

            // Find all .jsonl files in this project dir
            let jsonl_entries = match std::fs::read_dir(&project_dir_path) {
                Ok(e) => e,
                Err(_) => continue,
            };

            for jsonl_entry in jsonl_entries.flatten() {
                let path = jsonl_entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }

                let session_id = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();

                stats.scanned += 1;

                match extract_session_meta(&path, &session_id, &project_dir_name, &project_path, &domain) {
                    Ok(meta) => {
                        match store.upsert(&meta) {
                            Ok(true) => stats.indexed += 1,
                            Ok(false) => stats.skipped += 1,
                            Err(_) => stats.errors += 1,
                        }
                    }
                    Err(_) => stats.errors += 1,
                }
            }
        }
    }

    Ok(stats)
}

/// Decode a claude project directory name back to a path.
/// `-Users-jack-Code-wardwell` → `/Users/jack/Code/wardwell`
pub fn decode_project_dir(dir_name: &str) -> String {
    if dir_name.starts_with('-') {
        dir_name.replace('-', "/")
    } else {
        dir_name.to_string()
    }
}

/// Resolve which domain a project path belongs to.
fn resolve_domain(project_path: &str, domains: &[crate::domain::model::Domain]) -> Option<String> {
    let path = Path::new(project_path);
    for domain in domains {
        for glob_pat in &domain.paths {
            let expanded = glob_pat.expand();
            let base = expanded.to_string_lossy();
            let base_dir = base.split('*').next().unwrap_or(&base);
            let base_dir = base_dir.trim_end_matches('/');
            if path.starts_with(base_dir) {
                return Some(domain.name.as_str().to_string());
            }
        }
    }
    None
}

/// Extract metadata from a session JSONL file.
fn extract_session_meta(
    path: &Path,
    session_id: &str,
    project_dir: &str,
    project_path: &str,
    domain: &Option<String>,
) -> Result<SessionMeta, SessionError> {
    let file_size = std::fs::metadata(path)?.len() as i64;

    // Quick hash from file size + modification time for change detection
    let modified = std::fs::metadata(path)?
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default();
    let file_hash = format!("{file_size}:{modified}");

    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);

    let mut message_count: i64 = 0;
    let mut user_count: i64 = 0;
    let mut assistant_count: i64 = 0;
    let mut first_ts: Option<String> = None;
    let mut last_ts: Option<String> = None;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        if line.trim().is_empty() {
            continue;
        }

        let msg: RawMessage = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(_) => continue,
        };

        message_count += 1;

        match msg.r#type.as_str() {
            "user" => user_count += 1,
            "assistant" => assistant_count += 1,
            _ => {}
        }

        if let Some(ref ts) = msg.timestamp {
            if first_ts.is_none() {
                first_ts = Some(ts.clone());
            }
            last_ts = Some(ts.clone());
        }
    }

    Ok(SessionMeta {
        session_id: session_id.to_string(),
        project_dir: project_dir.to_string(),
        project_path: project_path.to_string(),
        domain: domain.clone(),
        message_count,
        user_message_count: user_count,
        assistant_message_count: assistant_count,
        first_message_at: first_ts,
        last_message_at: last_ts,
        file_size,
        file_hash,
    })
}

/// Extract user and assistant message text from a session JSONL file.
/// Used by the summarizer to build the conversation for the LLM.
pub fn extract_conversation(path: &Path) -> Result<Vec<ConversationMessage>, SessionError> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut messages = Vec::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        if line.trim().is_empty() {
            continue;
        }

        let msg: RawMessage = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(_) => continue,
        };

        let (role, text) = match msg.r#type.as_str() {
            "user" => {
                let text = extract_text_content(&msg);
                if text.is_empty() { continue; }
                ("user".to_string(), text)
            }
            "assistant" => {
                let text = extract_text_content(&msg);
                if text.is_empty() { continue; }
                ("assistant".to_string(), text)
            }
            _ => continue,
        };

        messages.push(ConversationMessage { role, text });
    }

    Ok(messages)
}

#[derive(Debug, Clone)]
pub struct ConversationMessage {
    pub role: String,
    pub text: String,
}

/// Extract text content from a message, handling both string and array content formats.
fn extract_text_content(msg: &RawMessage) -> String {
    // Try message.content first
    if let Some(ref m) = msg.message
        && let Some(ref content) = m.content
    {
        return content_value_to_text(content);
    }

    // Fall back to top-level content
    if let Some(ref content) = msg.content {
        return content_value_to_text(content);
    }

    String::new()
}

fn content_value_to_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => {
            let mut parts = Vec::new();
            for item in arr {
                if let Some(obj) = item.as_object()
                    && obj.get("type").and_then(|t| t.as_str()) == Some("text")
                    && let Some(text) = obj.get("text").and_then(|t| t.as_str())
                {
                    parts.push(text.to_string());
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn decode_project_dir_standard() {
        assert_eq!(
            decode_project_dir("-Users-jack-Code-wardwell"),
            "/Users/jack/Code/wardwell"
        );
    }

    #[test]
    fn session_store_open_in_memory() {
        let store = SessionStore::open_in_memory();
        assert!(store.is_ok(), "{:?}", store.err());
    }

    #[test]
    fn session_store_upsert_and_count() {
        let store = SessionStore::open_in_memory().unwrap();
        let meta = SessionMeta {
            session_id: "test-123".to_string(),
            project_dir: "-Users-test".to_string(),
            project_path: "/Users/test".to_string(),
            domain: Some("personal".to_string()),
            message_count: 10,
            user_message_count: 5,
            assistant_message_count: 5,
            first_message_at: Some("2026-01-01T00:00:00Z".to_string()),
            last_message_at: Some("2026-01-01T01:00:00Z".to_string()),
            file_size: 1024,
            file_hash: "1024:12345".to_string(),
        };

        let result = store.upsert(&meta);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(result.ok(), Some(true));

        let count = store.count();
        assert_eq!(count.ok(), Some(1));
    }

    #[test]
    fn session_store_upsert_skips_unchanged() {
        let store = SessionStore::open_in_memory().unwrap();
        let meta = SessionMeta {
            session_id: "test-456".to_string(),
            project_dir: "-Users-test".to_string(),
            project_path: "/Users/test".to_string(),
            domain: None,
            message_count: 5,
            user_message_count: 2,
            assistant_message_count: 3,
            first_message_at: None,
            last_message_at: None,
            file_size: 512,
            file_hash: "512:99999".to_string(),
        };

        store.upsert(&meta).ok();
        let result = store.upsert(&meta);
        assert_eq!(result.ok(), Some(false));
    }

    #[test]
    fn session_store_unsummarized() {
        let store = SessionStore::open_in_memory().unwrap();
        let meta = SessionMeta {
            session_id: "unsumm-1".to_string(),
            project_dir: "-Users-test".to_string(),
            project_path: "/Users/test".to_string(),
            domain: Some("work".to_string()),
            message_count: 20,
            user_message_count: 10,
            assistant_message_count: 10,
            first_message_at: Some("2026-02-01T00:00:00Z".to_string()),
            last_message_at: Some("2026-02-01T02:00:00Z".to_string()),
            file_size: 2048,
            file_hash: "2048:11111".to_string(),
        };

        store.upsert(&meta).ok();
        let unsumm = store.unsummarized().unwrap();
        assert_eq!(unsumm.len(), 1);
        assert_eq!(unsumm[0].session_id, "unsumm-1");

        store.mark_summarized("unsumm-1").ok();
        let unsumm = store.unsummarized().unwrap();
        assert_eq!(unsumm.len(), 0);
    }

    #[test]
    fn content_value_to_text_string() {
        let val = serde_json::json!("hello world");
        assert_eq!(content_value_to_text(&val), "hello world");
    }

    #[test]
    fn content_value_to_text_array() {
        let val = serde_json::json!([
            {"type": "text", "text": "part one"},
            {"type": "tool_use", "name": "read"},
            {"type": "text", "text": "part two"}
        ]);
        assert_eq!(content_value_to_text(&val), "part one\npart two");
    }
}
