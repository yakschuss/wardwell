use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Default, Deserialize, Serialize)]
struct Journal {
    source_key: String,
    plan_id: String,
    current_revision: u64,
    response_cursor: Option<String>,
    #[serde(default)]
    pending_observations: Vec<Value>,
    #[serde(default)]
    acknowledgements: Vec<Value>,
}

pub fn path(source_key: &str) -> PathBuf {
    let digest = Sha256::digest(source_key.as_bytes());
    let name = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    crate::config::loader::config_dir()
        .join("companions")
        .join("wardwell-context")
        .join(format!("{name}.json"))
}

pub fn pending(source_key: &str, plan_id: &str) -> Result<Option<Value>, String> {
    let Some(journal) = load(&path(source_key))? else {
        return Ok(None);
    };
    require_identity(&journal, source_key, plan_id)?;
    if journal.pending_observations.is_empty() {
        Ok(None)
    } else {
        Ok(Some(view(&journal, "pending")))
    }
}

pub fn cursor(source_key: &str, plan_id: &str) -> Result<Option<String>, String> {
    let Some(journal) = load(&path(source_key))? else {
        return Ok(None);
    };
    require_identity(&journal, source_key, plan_id)?;
    Ok(journal.response_cursor)
}

pub fn source_cursor(source_key: &str) -> Result<Option<String>, String> {
    let Some(journal) = load(&path(source_key))? else {
        return Ok(None);
    };
    if journal.source_key != source_key {
        return Err("Local Companion journal identity does not match this source".into());
    }
    Ok(journal.response_cursor)
}

pub fn stage(source_key: &str, page: &Value) -> Result<Value, String> {
    stage_at(&path(source_key), source_key, page)
}

fn stage_at(journal_path: &Path, source_key: &str, page: &Value) -> Result<Value, String> {
    let plan_id = text(page, "plan_id")?;
    if text(page, "source_key")? != source_key {
        return Err("Response page belongs to a different Companion source".into());
    }
    let revision = page
        .get("current_revision")
        .and_then(Value::as_u64)
        .ok_or("Response page has no current revision")?;
    let cursor = text(page, "cursor")?.to_owned();
    let observations = page
        .get("observations")
        .and_then(Value::as_array)
        .ok_or("Response page has no observations")?;
    for observation in observations {
        validate_observation(observation)?;
    }

    let mut journal = load(journal_path)?.unwrap_or_else(|| Journal {
        source_key: source_key.to_owned(),
        plan_id: plan_id.to_owned(),
        ..Journal::default()
    });
    require_identity(&journal, source_key, plan_id)?;
    if !journal.pending_observations.is_empty() {
        return Err(
            "Acknowledge the staged Companion responses before fetching another page".into(),
        );
    }
    let acknowledged_ids = journal
        .acknowledgements
        .iter()
        .filter_map(|receipt| receipt.get("observation_ids").and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
        .collect::<HashSet<_>>();
    let unacknowledged = observations
        .iter()
        .filter(|observation| {
            observation
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| !acknowledged_ids.contains(id))
        })
        .cloned()
        .collect::<Vec<_>>();
    journal.current_revision = revision;
    journal.response_cursor = Some(cursor);
    journal.pending_observations = unacknowledged;
    save(journal_path, &journal)?;
    Ok(view(
        &journal,
        if journal.pending_observations.is_empty() {
            "caught_up"
        } else {
            "staged"
        },
    ))
}

pub fn acknowledge(source_key: &str, plan_id: &str, ids: &[String]) -> Result<Value, String> {
    acknowledge_at(&path(source_key), source_key, plan_id, ids)
}

fn acknowledge_at(
    journal_path: &Path,
    source_key: &str,
    plan_id: &str,
    ids: &[String],
) -> Result<Value, String> {
    let mut journal = load(journal_path)?.ok_or("No Companion response journal exists")?;
    require_identity(&journal, source_key, plan_id)?;
    let pending_ids = journal
        .pending_observations
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str))
        .collect::<Vec<_>>();
    if ids.is_empty() || ids.iter().any(|id| !pending_ids.contains(&id.as_str())) {
        return Err("Acknowledgement must name only currently staged observation ids".into());
    }
    journal.acknowledgements.push(json!({
        "observation_ids": ids,
        "acknowledged_at": Utc::now().to_rfc3339(),
        "meaning": "persisted_to_local_journal"
    }));
    journal.pending_observations.retain(|item| {
        item.get("id")
            .and_then(Value::as_str)
            .is_none_or(|id| !ids.iter().any(|candidate| candidate == id))
    });
    save(journal_path, &journal)?;
    Ok(view(&journal, "acknowledged"))
}

fn validate_observation(observation: &Value) -> Result<(), String> {
    for field in ["id", "node_id", "kind"] {
        text(observation, field)?;
    }
    if observation
        .get("expected_revision")
        .and_then(Value::as_u64)
        .is_none()
    {
        return Err("Response observation lacks its revision-bound context".into());
    }
    if matches!(
        observation.get("kind").and_then(Value::as_str),
        Some("decision_response" | "step_completed" | "step_reopened")
    ) {
        text(observation, "context_fingerprint")?;
        if observation
            .get("source_revision")
            .and_then(Value::as_u64)
            .is_none()
            || !observation
                .get("context_snapshot")
                .is_some_and(Value::is_object)
        {
            return Err("Companion response lacks its immutable context snapshot".into());
        }
    }
    Ok(())
}

fn text<'a>(value: &'a Value, field: &str) -> Result<&'a str, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| format!("Response page has invalid {field}"))
}

fn require_identity(journal: &Journal, source_key: &str, plan_id: &str) -> Result<(), String> {
    if journal.source_key == source_key && journal.plan_id == plan_id {
        Ok(())
    } else {
        Err("Local Companion journal identity does not match this plan".into())
    }
}

fn view(journal: &Journal, status: &str) -> Value {
    json!({
        "status": status,
        "plan_id": journal.plan_id,
        "source_key": journal.source_key,
        "current_revision": journal.current_revision,
        "response_cursor": journal.response_cursor,
        "pending_observations": journal.pending_observations,
        "journal_path": path(&journal.source_key),
        "acknowledgement_meaning": "local persistence only; not execution or completion"
    })
}

fn load(path: &Path) -> Result<Option<Journal>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("Could not inspect the Companion response journal".into()),
    };
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.len() > MAX_BYTES
    {
        return Err("Companion response journal is not a safe regular file".into());
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err("Companion response journal permissions must be private".into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)
        .and_then(|file| file.take(MAX_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|_| "Could not read the Companion response journal".to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("Companion response journal is too large".into());
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| "Companion response journal is malformed".into())
}

fn save(path: &Path, journal: &Journal) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or("Companion response journal has no parent")?;
    fs::create_dir_all(parent).map_err(|_| "Could not create the Companion journal directory")?;
    #[cfg(unix)]
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
        .map_err(|_| "Could not secure the Companion journal directory")?;
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err("Companion response journal must not be a symlink".into());
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let temp = parent.join(format!(".journal.{}.{}", std::process::id(), nonce));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options
            .open(&temp)
            .map_err(|_| "Could not create the Companion response journal")?;
        #[cfg(unix)]
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| "Could not secure the Companion response journal")?;
        let bytes = serde_json::to_vec_pretty(journal)
            .map_err(|_| "Could not encode the Companion response journal")?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("Companion response journal is too large");
        }
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| "Could not write the Companion response journal")?;
        drop(file);
        fs::rename(&temp, path).map_err(|_| "Could not install the Companion response journal")?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(str::to_owned)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), &'static str> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| "Could not durably install the Companion response journal")
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), &'static str> {
    Ok(())
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn page(observation_id: &str) -> Value {
        json!({
            "plan_id": "2e2ea0dd-8b32-42e0-b102-fd18589a6214",
            "source_key": "session-a",
            "current_revision": 2,
            "cursor": "signed-cursor",
            "observations": [{
                "id": observation_id,
                "node_id": "choose",
                "kind": "decision_response",
                "expected_revision": 1,
                "source_revision": 1,
                "context_fingerprint": "fingerprint",
                "context_snapshot": {"question": "Ship?"}
            }]
        })
    }

    #[test]
    fn stages_once_then_requires_explicit_local_acknowledgement() {
        let directory = tempfile::tempdir().unwrap();
        let journal_path = directory.path().join("responses.json");
        let observation_id = "70b1c956-69cc-4bd3-94ac-780906817251";

        let staged = stage_at(&journal_path, "session-a", &page(observation_id)).unwrap();
        assert_eq!(staged["status"], "staged");
        assert_eq!(staged["pending_observations"][0]["source_revision"], 1);
        assert!(stage_at(&journal_path, "session-a", &page(observation_id)).is_err());

        let acknowledged = acknowledge_at(
            &journal_path,
            "session-a",
            "2e2ea0dd-8b32-42e0-b102-fd18589a6214",
            &[observation_id.to_owned()],
        )
        .unwrap();
        assert_eq!(acknowledged["status"], "acknowledged");
        assert_eq!(acknowledged["pending_observations"], json!([]));

        let replayed = stage_at(&journal_path, "session-a", &page(observation_id)).unwrap();
        assert_eq!(replayed["status"], "caught_up");
        assert_eq!(replayed["pending_observations"], json!([]));

        let generic = json!({
            "plan_id": "2e2ea0dd-8b32-42e0-b102-fd18589a6214",
            "source_key": "session-a",
            "current_revision": 2,
            "cursor": "next-signed-cursor",
            "observations": [{
                "id": "e548918f-a986-4b74-955d-a339c8549bee",
                "node_id": "choose",
                "sequence": 2,
                "kind": "set_priority",
                "priority": "high",
                "expected_revision": 1,
                "inserted_at": "2026-09-17T14:00:00Z"
            }]
        });
        let generic_staged = stage_at(&journal_path, "session-a", &generic).unwrap();
        assert_eq!(generic_staged["status"], "staged");
        assert_eq!(
            generic_staged["pending_observations"][0]["kind"],
            "set_priority"
        );

        #[cfg(unix)]
        assert_eq!(
            fs::metadata(journal_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
