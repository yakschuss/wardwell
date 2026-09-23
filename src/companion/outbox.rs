use chrono::Utc;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_REQUEST_BYTES: usize = 256 * 1024;
const MAX_OPERATIONS_PER_SOURCE: u64 = 1_000;

#[derive(Debug)]
pub struct Pending {
    pub request_id: String,
    pub action: String,
    pub arguments: Value,
    pub principal_fingerprint: Option<String>,
    pub state: String,
}

pub fn path() -> PathBuf {
    crate::config::loader::config_dir()
        .join("companions")
        .join("wardwell-context")
        .join("outbox.sqlite3")
}

pub fn principal_fingerprint(endpoint: &str, token: &str) -> String {
    hex_digest(format!("{endpoint}\0{token}").as_bytes())
}

pub fn stage(
    source_key: &str,
    action: &str,
    arguments: &Value,
    principal: Option<&str>,
    allow_bind: bool,
) -> Result<String, String> {
    stage_at(
        &path(),
        source_key,
        action,
        arguments,
        principal,
        allow_bind,
    )
}

fn stage_at(
    database_path: &Path,
    source_key: &str,
    action: &str,
    arguments: &Value,
    principal: Option<&str>,
    allow_bind: bool,
) -> Result<String, String> {
    let request = serde_json::to_vec(arguments)
        .map_err(|_| "Could not encode the Companion outbox request")?;
    if request.len() > MAX_REQUEST_BYTES {
        return Err("Companion outbox request exceeds 256 KiB".into());
    }
    let request_id = request_id(source_key, action, &request);
    let connection = open(database_path)?;
    let transaction = connection
        .unchecked_transaction()
        .map_err(|_| "Could not lock the Companion outbox")?;
    let existing: Option<(String, Option<String>)> = transaction
        .query_row(
            "SELECT request_json, principal_fingerprint FROM operations WHERE request_id = ?1",
            [&request_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|_| "Could not read the Companion outbox")?;
    if let Some((stored, stored_principal)) = existing {
        if stored.as_bytes() != request {
            return Err("Companion outbox request identity conflict".into());
        }
        if let (Some(stored), Some(current)) = (stored_principal.as_deref(), principal)
            && stored != current
        {
            return Err("Companion outbox is bound to a different installation account".into());
        }
        if stored_principal.is_none()
            && allow_bind
            && let Some(current) = principal
        {
            transaction
                .execute(
                    "UPDATE operations SET principal_fingerprint = ?1 WHERE request_id = ?2",
                    params![current, request_id],
                )
                .map_err(|_| "Could not bind the Companion outbox request")?;
        }
    } else {
        let count: u64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM operations WHERE source_key = ?1",
                [source_key],
                |row| row.get(0),
            )
            .map_err(|_| "Could not inspect the Companion outbox")?;
        if count >= MAX_OPERATIONS_PER_SOURCE {
            return Err("Companion outbox has reached its per-source limit".into());
        }
        transaction
            .execute(
                "INSERT INTO operations (request_id, source_key, action, request_json, principal_fingerprint, state, staged_at) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)",
                params![request_id, source_key, action, String::from_utf8_lossy(&request), principal, Utc::now().to_rfc3339()],
            )
            .map_err(|_| "Could not stage the Companion outbox request")?;
    }
    transaction
        .commit()
        .map_err(|_| "Could not durably stage the Companion outbox request")?;
    Ok(request_id)
}

pub fn pending(source_key: &str) -> Result<Vec<Pending>, String> {
    pending_at(&path(), source_key)
}

fn pending_at(database_path: &Path, source_key: &str) -> Result<Vec<Pending>, String> {
    let connection = open(database_path)?;
    let mut statement = connection
        .prepare("SELECT request_id, action, request_json, principal_fingerprint, state FROM operations WHERE source_key = ?1 AND state IN ('pending', 'blocked') ORDER BY staged_at, request_id LIMIT 100")
        .map_err(|_| "Could not read the Companion outbox")?;
    let rows = statement
        .query_map([source_key], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(|_| "Could not read the Companion outbox")?;
    let mut result = Vec::new();
    for row in rows {
        let (request_id, action, request_json, principal_fingerprint, state) =
            row.map_err(|_| "Could not read the Companion outbox")?;
        let arguments = serde_json::from_str(&request_json)
            .map_err(|_| "Companion outbox contains a malformed request")?;
        result.push(Pending {
            request_id,
            action,
            arguments,
            principal_fingerprint,
            state,
        });
    }
    Ok(result)
}

pub fn mark_pending(request_id: &str, code: &str) -> Result<(), String> {
    update_state(request_id, "pending", code)
}

pub fn mark_blocked(request_id: &str, code: &str) -> Result<(), String> {
    update_state(request_id, "blocked", code)
}

fn update_state(request_id: &str, state: &str, code: &str) -> Result<(), String> {
    let connection = open(&path())?;
    connection
        .execute(
            "UPDATE operations SET state = ?1, failure_code = ?2, attempts = attempts + 1 WHERE request_id = ?3 AND state != 'completed'",
            params![state, code, request_id],
        )
        .map_err(|_| "Could not update the Companion outbox")?;
    Ok(())
}

pub fn complete(
    request_id: &str,
    source_key: &str,
    action: &str,
    remote_id: &str,
    revision: Option<u64>,
) -> Result<Value, String> {
    complete_at(&path(), request_id, source_key, action, remote_id, revision)
}

fn complete_at(
    database_path: &Path,
    request_id: &str,
    source_key: &str,
    action: &str,
    remote_id: &str,
    revision: Option<u64>,
) -> Result<Value, String> {
    let receipt = json!({
        "receipt_id": request_id,
        "request_id": request_id,
        "source_key": source_key,
        "action": action,
        "remote_id": remote_id,
        "revision": revision,
        "status": "verified",
        "verified_at": Utc::now().to_rfc3339()
    });
    let encoded = serde_json::to_string(&receipt)
        .map_err(|_| "Could not encode the Companion outbox receipt")?;
    let connection = open(database_path)?;
    let changed = connection
        .execute(
            "UPDATE operations SET state = 'completed', receipt_json = ?1, failure_code = NULL, attempts = attempts + 1 WHERE request_id = ?2 AND source_key = ?3",
            params![encoded, request_id, source_key],
        )
        .map_err(|_| "Could not store the Companion outbox receipt")?;
    if changed != 1 {
        return Err("Companion outbox request was not found for this source".into());
    }
    Ok(receipt)
}

pub fn verified_receipt(source_key: &str, receipt_id: &str) -> Result<Option<Value>, String> {
    let connection = open(&path())?;
    let encoded: Option<String> = connection
        .query_row(
            "SELECT receipt_json FROM operations WHERE source_key = ?1 AND request_id = ?2 AND action = 'publish' AND state = 'completed'",
            params![source_key, receipt_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| "Could not read the Companion outbox receipt")?;
    encoded
        .map(|value| {
            serde_json::from_str(&value).map_err(|_| "Companion outbox receipt is malformed".into())
        })
        .transpose()
}

pub fn completed_receipt(source_key: &str, request_id: &str) -> Result<Option<Value>, String> {
    let connection = open(&path())?;
    let encoded: Option<String> = connection
        .query_row(
            "SELECT receipt_json FROM operations WHERE source_key = ?1 AND request_id = ?2 AND state = 'completed'",
            params![source_key, request_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| "Could not read the Companion outbox receipt")?;
    encoded
        .map(|value| {
            serde_json::from_str(&value).map_err(|_| "Companion outbox receipt is malformed".into())
        })
        .transpose()
}

pub fn verified_publish_receipt_after(
    source_key: &str,
    receipt_id: &str,
    opened_at: &str,
) -> Result<bool, String> {
    verified_publish_receipt_after_at(&path(), source_key, receipt_id, opened_at)
}

fn verified_publish_receipt_after_at(
    database_path: &Path,
    source_key: &str,
    receipt_id: &str,
    opened_at: &str,
) -> Result<bool, String> {
    let connection = open(database_path)?;
    let row: Option<(String, String)> = connection
        .query_row(
            "SELECT receipt_json, staged_at FROM operations WHERE source_key = ?1 AND request_id = ?2 AND action = 'publish' AND state = 'completed'",
            params![source_key, receipt_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|_| "Could not read the Companion outbox receipt")?;
    let Some((encoded, staged_at)) = row else {
        return Ok(false);
    };
    let receipt: Value =
        serde_json::from_str(&encoded).map_err(|_| "Companion outbox receipt is malformed")?;
    let verified_at = receipt
        .get("verified_at")
        .and_then(Value::as_str)
        .ok_or("Companion outbox receipt has no verification time")?;
    let verified = chrono::DateTime::parse_from_rfc3339(verified_at)
        .map_err(|_| "Companion outbox receipt has an invalid verification time")?;
    let staged = chrono::DateTime::parse_from_rfc3339(&staged_at)
        .map_err(|_| "Companion outbox request has an invalid staging time")?;
    let opened = chrono::DateTime::parse_from_rfc3339(opened_at)
        .map_err(|_| "Companion generation has an invalid open time")?;
    if staged < opened || verified < opened {
        return Ok(false);
    }
    let newer: u64 = connection
        .query_row(
            "SELECT COUNT(*) FROM operations WHERE source_key = ?1 AND action = 'publish' AND state IN ('pending', 'blocked') AND staged_at > ?2",
            params![source_key, staged_at],
            |row| row.get(0),
        )
        .map_err(|_| "Could not inspect newer Companion outbox work")?;
    Ok(newer == 0)
}

pub fn unchanged_eligible(source_key: &str) -> Result<bool, String> {
    unchanged_eligible_at(&path(), source_key)
}

fn unchanged_eligible_at(database_path: &Path, source_key: &str) -> Result<bool, String> {
    let connection = open(database_path)?;
    let outstanding: u64 = connection.query_row(
        "SELECT COUNT(*) FROM operations WHERE source_key = ?1 AND action = 'publish' AND state IN ('pending', 'blocked')",
        [source_key], |row| row.get(0),
    ).map_err(|_| "Could not inspect outstanding Companion publications")?;
    if outstanding > 0 {
        return Ok(false);
    }
    let mut statement = connection.prepare(
        "SELECT receipt_json FROM operations WHERE source_key = ?1 AND action = 'publish' AND state = 'completed'"
    ).map_err(|_| "Could not inspect completed Companion publications")?;
    let receipts = statement
        .query_map([source_key], |row| row.get::<_, String>(0))
        .map_err(|_| "Could not read Companion publication receipts")?;
    for receipt in receipts {
        let encoded = receipt.map_err(|_| "Could not read Companion publication receipt")?;
        let value: Value = serde_json::from_str(&encoded)
            .map_err(|_| "Companion publication receipt is malformed")?;
        if value["status"] == "verified"
            && value["source_key"] == source_key
            && value["action"] == "publish"
            && value["revision"].as_u64().is_some()
            && value["remote_id"].as_str().is_some_and(|id| !id.is_empty())
            && value["verified_at"]
                .as_str()
                .is_some_and(|at| chrono::DateTime::parse_from_rfc3339(at).is_ok())
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn status(source_key: &str) -> Result<Value, String> {
    let connection = open(&path())?;
    let (pending, blocked, completed): (u64, u64, u64) = connection
        .query_row(
            "SELECT COALESCE(SUM(CASE WHEN state = 'pending' THEN 1 ELSE 0 END), 0), COALESCE(SUM(CASE WHEN state = 'blocked' THEN 1 ELSE 0 END), 0), COALESCE(SUM(CASE WHEN state = 'completed' THEN 1 ELSE 0 END), 0) FROM operations WHERE source_key = ?1",
            [source_key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|_| "Could not read the Companion outbox status")?;
    let mut statement = connection
        .prepare("SELECT request_id, action, state, failure_code, staged_at FROM operations WHERE source_key = ?1 AND state != 'completed' ORDER BY staged_at, request_id LIMIT 100")
        .map_err(|_| "Could not read the Companion outbox status")?;
    let rows = statement
        .query_map([source_key], |row| {
            Ok(json!({
                "request_id": row.get::<_, String>(0)?,
                "action": row.get::<_, String>(1)?,
                "state": row.get::<_, String>(2)?,
                "failure_code": row.get::<_, Option<String>>(3)?,
                "staged_at": row.get::<_, String>(4)?
            }))
        })
        .map_err(|_| "Could not read the Companion outbox status")?;
    let mut operations = Vec::new();
    for row in rows {
        operations.push(row.map_err(|_| "Could not read the Companion outbox status")?);
    }
    Ok(json!({
        "source_key":source_key,
        "pending":pending,
        "blocked":blocked,
        "completed":completed,
        "operations":operations,
        "operations_truncated": pending.saturating_add(blocked) > 100
    }))
}

fn request_id(source_key: &str, action: &str, request: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(source_key.as_bytes());
    digest.update([0]);
    digest.update(action.as_bytes());
    digest.update([0]);
    digest.update(request);
    hex_digest(&digest.finalize())
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn open(path: &Path) -> Result<Connection, String> {
    let parent = path.parent().ok_or("Companion outbox has no parent")?;
    let companion_dir = parent
        .parent()
        .ok_or("Companion outbox has no owner directory")?;
    secure_directory(companion_dir)?;
    secure_directory(parent)?;
    let existed = fs::symlink_metadata(path).is_ok();
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err("Companion outbox must be a private regular file".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err("Companion outbox permissions must be private".into());
            }
        }
    }
    if !existed {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        match options.open(path) {
            Ok(file) => file
                .sync_all()
                .map_err(|_| "Could not durably create the Companion outbox")?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err("Could not create the Companion outbox".into()),
        }
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "Could not open the Companion outbox")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| "Could not secure the Companion outbox")?;
    }
    connection
        .busy_timeout(Duration::from_secs(2))
        .map_err(|_| "Could not configure the Companion outbox")?;
    connection
        .query_row("PRAGMA journal_mode=DELETE", [], |_| Ok(()))
        .and_then(|_| connection.pragma_update(None, "synchronous", "FULL"))
        .map_err(|_| "Could not configure durable Companion outbox writes")?;
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS operations (
               request_id TEXT PRIMARY KEY,
               source_key TEXT NOT NULL,
               action TEXT NOT NULL CHECK (action IN ('capture', 'publish')),
               request_json TEXT NOT NULL,
               principal_fingerprint TEXT,
               state TEXT NOT NULL CHECK (state IN ('pending', 'blocked', 'completed')),
               staged_at TEXT NOT NULL,
               attempts INTEGER NOT NULL DEFAULT 0,
               failure_code TEXT,
               receipt_json TEXT
             );
             CREATE INDEX IF NOT EXISTS operations_source_state ON operations(source_key, state);",
        )
        .map_err(|_| "Could not initialize the Companion outbox")?;
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|_| "Could not durably initialize the Companion outbox")?;
    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| "Could not durably install the Companion outbox")?;
    Ok(connection)
}

fn secure_directory(path: &Path) -> Result<(), String> {
    let created = match fs::create_dir(path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(_) => return Err("Could not create the Companion outbox directory".into()),
    };
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "Could not inspect the Companion outbox directory")?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err("Companion outbox directory must not be a symlink".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if created {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .map_err(|_| "Could not secure the Companion outbox directory")?;
        } else if metadata.permissions().mode() & 0o077 != 0 {
            return Err("Companion outbox directory permissions must be private".into());
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_requires_verified_publication_without_outstanding_work_for_exact_source() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("companions/wardwell-context/outbox.sqlite3");
        assert!(!unchanged_eligible_at(&db, "a").unwrap());
        let first = stage_at(
            &db,
            "a",
            "publish",
            &json!({"expected_revision":0}),
            Some("p"),
            true,
        )
        .unwrap();
        assert!(!unchanged_eligible_at(&db, "a").unwrap());
        complete_at(&db, &first, "a", "publish", "plan", Some(1)).unwrap();
        assert!(unchanged_eligible_at(&db, "a").unwrap());
        assert!(!unchanged_eligible_at(&db, "b").unwrap());
        stage_at(
            &db,
            "b",
            "publish",
            &json!({"expected_revision":0}),
            Some("p"),
            true,
        )
        .unwrap();
        assert!(unchanged_eligible_at(&db, "a").unwrap());
        let next = stage_at(
            &db,
            "a",
            "publish",
            &json!({"expected_revision":1}),
            Some("p"),
            true,
        )
        .unwrap();
        assert!(!unchanged_eligible_at(&db, "a").unwrap());
        open(&db)
            .unwrap()
            .execute(
                "UPDATE operations SET state = 'blocked' WHERE request_id = ?1",
                [&next],
            )
            .unwrap();
        assert!(!unchanged_eligible_at(&db, "a").unwrap());
    }

    #[test]
    fn duplicate_survives_restart_and_sources_are_isolated() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory
            .path()
            .join("companions/wardwell-context/outbox.sqlite3");
        let args = json!({"conversation_key":"source-a","external_id":"event-1"});
        let first = stage_at(&database, "source-a", "capture", &args, Some("p1"), true).unwrap();
        let duplicate =
            stage_at(&database, "source-a", "capture", &args, Some("p1"), true).unwrap();
        assert_eq!(first, duplicate);
        assert_eq!(pending_at(&database, "source-a").unwrap().len(), 1);
        assert!(pending_at(&database, "source-b").unwrap().is_empty());
    }

    #[test]
    fn request_cannot_replay_under_another_principal() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory
            .path()
            .join("companions/wardwell-context/outbox.sqlite3");
        let args = json!({"source_key":"source-a","expected_revision":0});
        stage_at(&database, "source-a", "publish", &args, Some("p1"), true).unwrap();
        let error =
            stage_at(&database, "source-a", "publish", &args, Some("p2"), true).unwrap_err();
        assert!(error.contains("different installation account"));
    }

    #[test]
    fn unbound_request_requires_explicit_repeat_to_bind() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory
            .path()
            .join("companions/wardwell-context/outbox.sqlite3");
        let args = json!({"source_key":"source-a","expected_revision":0});
        stage_at(&database, "source-a", "publish", &args, None, true).unwrap();
        stage_at(&database, "source-a", "publish", &args, Some("p1"), false).unwrap();
        assert_eq!(
            pending_at(&database, "source-a").unwrap()[0].principal_fingerprint,
            None
        );
        stage_at(&database, "source-a", "publish", &args, Some("p1"), true).unwrap();
        assert_eq!(
            pending_at(&database, "source-a").unwrap()[0]
                .principal_fingerprint
                .as_deref(),
            Some("p1")
        );
    }

    #[test]
    fn old_receipt_or_newer_pending_publish_cannot_prove_current_generation() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory
            .path()
            .join("companions/wardwell-context/outbox.sqlite3");
        let first_args = json!({"source_key":"source-a","expected_revision":0,"title":"one"});
        let first = stage_at(
            &database,
            "source-a",
            "publish",
            &first_args,
            Some("p1"),
            true,
        )
        .unwrap();
        complete_at(
            &database,
            &first,
            "source-a",
            "publish",
            "2e2ea0dd-8b32-42e0-b102-fd18589a6214",
            Some(1),
        )
        .unwrap();
        assert!(
            !verified_publish_receipt_after_at(
                &database,
                "source-a",
                &first,
                "2999-01-01T00:00:00Z"
            )
            .unwrap()
        );
        assert!(
            verified_publish_receipt_after_at(
                &database,
                "source-a",
                &first,
                "2000-01-01T00:00:00Z"
            )
            .unwrap()
        );

        let second_args = json!({"source_key":"source-a","expected_revision":1,"title":"two"});
        let second = stage_at(
            &database,
            "source-a",
            "publish",
            &second_args,
            Some("p1"),
            true,
        )
        .unwrap();
        open(&database)
            .unwrap()
            .execute(
                "UPDATE operations SET staged_at = '2999-01-01T00:00:00Z' WHERE request_id = ?1",
                [second],
            )
            .unwrap();
        assert!(
            !verified_publish_receipt_after_at(
                &database,
                "source-a",
                &first,
                "2000-01-01T00:00:00Z"
            )
            .unwrap()
        );
    }

    #[test]
    fn request_staged_before_generation_stays_ineligible_when_flushed_during_it() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory
            .path()
            .join("companions/wardwell-context/outbox.sqlite3");
        let old_args = json!({"source_key":"source-a","expected_revision":0,"title":"old"});
        let old = stage_at(
            &database,
            "source-a",
            "publish",
            &old_args,
            Some("p1"),
            true,
        )
        .unwrap();
        open(&database)
            .unwrap()
            .execute(
                "UPDATE operations SET staged_at = '2000-01-01T00:00:00Z' WHERE request_id = ?1",
                [&old],
            )
            .unwrap();
        complete_at(
            &database,
            &old,
            "source-a",
            "publish",
            "2e2ea0dd-8b32-42e0-b102-fd18589a6214",
            Some(1),
        )
        .unwrap();
        assert!(
            !verified_publish_receipt_after_at(&database, "source-a", &old, "2020-01-01T00:00:00Z")
                .unwrap()
        );

        let new_args = json!({"source_key":"source-a","expected_revision":1,"title":"new"});
        let new = stage_at(
            &database,
            "source-a",
            "publish",
            &new_args,
            Some("p1"),
            true,
        )
        .unwrap();
        complete_at(
            &database,
            &new,
            "source-a",
            "publish",
            "2e2ea0dd-8b32-42e0-b102-fd18589a6214",
            Some(2),
        )
        .unwrap();
        assert!(
            verified_publish_receipt_after_at(&database, "source-a", &new, "2020-01-01T00:00:00Z")
                .unwrap()
        );
    }
}
