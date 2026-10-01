//! Migrates a project's `tracker.jsonl` to light rows: inline raw payloads
//! move to the `tracker.raw.jsonl` sidecar and exact duplicate events go.
//!
//! The only code that rewrites a tracker log, and only `tracker.jsonl`: the
//! log is a re-pullable mirror, not a system of record. Under the project
//! lock it writes the sidecar first, verifies, writes the new log beside the
//! old one, keeps the old one as `tracker.jsonl.bak`, then renames.
//! Does NOT pull, and does NOT touch any other vault file.

use crate::tracker::events::Event;
use crate::tracker::{lock, log};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What a compaction did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactOutcome {
    /// False when the log was already compact and nothing was written.
    pub changed: bool,
    pub events: usize,
    pub moved_raw: usize,
    pub duplicates_removed: usize,
    pub backup: Option<PathBuf>,
}

/// The backup the last successful compaction left beside `log_path`.
pub fn backup_path_for(log_path: &Path) -> PathBuf {
    with_suffix(log_path, ".bak")
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

/// Compact the log at `log_path`. A compact log is left alone. Refuses when
/// a backup from an earlier compaction exists, unless `force`.
pub fn compact(log_path: &Path, force: bool, wait: Duration) -> Result<CompactOutcome, String> {
    let _lock = lock::acquire(log_path, wait)?;
    let original = match std::fs::read_to_string(log_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(unchanged(0)),
        Err(_) => return Err(format!("could not read {}", log_path.display())),
    };
    let raw_path = log::raw_path_for(log_path);
    let plan = Plan::build(&original, &log::read_raw(&raw_path)?)?;
    if plan.content == original {
        return Ok(unchanged(plan.events));
    }
    let backup = backup_path_for(log_path);
    if backup.exists() && !force {
        return Err(format!(
            "{} exists from an earlier compact; check it, then delete it or pass --force",
            backup.display()
        ));
    }
    for (id, raw) in &plan.to_move {
        log::append_raw(&raw_path, id, raw.clone())?;
    }
    plan.verify(&log::read_raw(&raw_path)?)?;
    replace_log(log_path, &plan.content, &backup)?;
    Ok(CompactOutcome {
        changed: true,
        events: plan.events,
        moved_raw: plan.inline_raw.len(),
        duplicates_removed: plan.duplicates,
        backup: Some(backup),
    })
}

fn unchanged(events: usize) -> CompactOutcome {
    CompactOutcome { changed: false, events, moved_raw: 0, duplicates_removed: 0, backup: None }
}

/// The compacted file, computed in memory before anything is written.
struct Plan {
    content: String,
    events: usize,
    unique_ids: usize,
    duplicates: usize,
    /// Every inline payload, by event id, that must be readable afterwards.
    inline_raw: HashMap<String, Value>,
    /// Inline payloads the sidecar does not hold yet, in file order.
    to_move: Vec<(String, Value)>,
}

impl Plan {
    fn build(original: &str, sidecar: &HashMap<String, Value>) -> Result<Self, String> {
        let mut plan = Plan {
            content: String::new(),
            events: 0,
            unique_ids: 0,
            duplicates: 0,
            inline_raw: HashMap::new(),
            to_move: Vec::new(),
        };
        let mut kept: HashMap<String, Vec<(Event, Value)>> = HashMap::new();
        let mut ids: HashSet<String> = HashSet::new();
        for line in original.lines() {
            let trimmed = line.trim();
            let parsed = match trimmed.starts_with("{\"_schema\"") {
                true => None,
                false => serde_json::from_str::<Event>(trimmed).ok(),
            };
            let Some(event) = parsed else {
                // Headers, blank and unreadable lines are kept as they are.
                plan.push_line(line);
                continue;
            };
            let (light, raw) = log::split_raw(event);
            let id = light.common().id.clone();
            ids.insert(id.clone());
            let same_id = kept.entry(id.clone()).or_default();
            if same_id.iter().any(|(e, r)| *e == light && *r == raw) {
                plan.duplicates += 1;
                continue;
            }
            same_id.push((light.clone(), raw.clone()));
            plan.take_raw(&id, raw, sidecar);
            let encoded = serde_json::to_string(&light).map_err(|_| "could not encode tracker event".to_string())?;
            plan.push_line(&encoded);
            plan.events += 1;
        }
        plan.unique_ids = ids.len();
        Ok(plan)
    }

    fn push_line(&mut self, line: &str) {
        self.content.push_str(line);
        self.content.push('\n');
    }

    fn take_raw(&mut self, id: &str, raw: Value, sidecar: &HashMap<String, Value>) {
        if raw.is_null() || self.inline_raw.contains_key(id) {
            return;
        }
        if !sidecar.contains_key(id) {
            self.to_move.push((id.to_string(), raw.clone()));
        }
        self.inline_raw.insert(id.to_string(), raw);
    }

    /// One event per id after dedup, and every inline payload readable from
    /// the sidecar by its event id with the same content.
    fn verify(&self, sidecar: &HashMap<String, Value>) -> Result<(), String> {
        if self.events != self.unique_ids {
            return Err(format!(
                "compact stopped: {} events remain for {} ids; rows that share an id differ, so the log is left as it was",
                self.events, self.unique_ids
            ));
        }
        let unreadable = self.inline_raw.iter().filter(|(id, raw)| sidecar.get(*id) != Some(*raw)).count();
        match unreadable {
            0 => Ok(()),
            n => Err(format!("compact stopped: {n} raw payloads are not readable from the sidecar; the log is left as it was")),
        }
    }
}

/// Write `content` beside the log, keep the old log as `backup`, then
/// rename the new file into place.
fn replace_log(log_path: &Path, content: &str, backup: &Path) -> Result<(), String> {
    let temp = with_suffix(log_path, ".compact");
    let written = std::fs::File::create(&temp)
        .and_then(|mut file| file.write_all(content.as_bytes()).and_then(|_| file.sync_all()));
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
        return Err(format!("could not write {}", temp.display()));
    }
    let _ = std::fs::remove_file(backup);
    let backed_up = std::fs::hard_link(log_path, backup).or_else(|_| std::fs::copy(log_path, backup).map(|_| ()));
    if backed_up.is_err() {
        let _ = std::fs::remove_file(&temp);
        return Err(format!("could not keep a backup at {}", backup.display()));
    }
    std::fs::rename(&temp, log_path).map_err(|_| format!("could not replace {}", log_path.display()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::tracker::events::{Common, IssueSnapshot, SCHEMA_HEADER};
    use chrono::TimeZone;
    use serde_json::json;

    fn row(id: &str, key: &str, raw: Value) -> String {
        let event = Event::IssueUpserted {
            common: Common {
                id: id.into(),
                provider: "linear".into(),
                external_key: key.into(),
                external_id: format!("{key}-uuid"),
                actor: None,
                occurred_at: chrono::Utc.with_ymd_and_hms(2026, 9, 1, 9, 0, 0).unwrap(),
                title: format!("{key} Title: Todo"),
                raw: Value::Null,
            },
            issue: Box::new(IssueSnapshot { issue_title: "Title".into(), state: "Todo".into(), ..Default::default() }),
        };
        let mut value = serde_json::to_value(event).unwrap();
        value["raw"] = raw;
        value.to_string()
    }

    /// A log in the old format: inline raw on every row, `raw: null` on
    /// markers, and one row pulled twice.
    fn old_log(dir: &Path) -> PathBuf {
        let path = log::path_for(dir, "work", "claims");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let lines = [
            SCHEMA_HEADER.to_string(),
            row("e1", "COR-1", json!({"identifier": "COR-1", "body": "first"})),
            row("e2", "COR-2", json!({"identifier": "COR-2", "body": "second"})),
            row("e1", "COR-1", json!({"identifier": "COR-1", "body": "first"})),
            row("e3", "COR-3", Value::Null),
            "not json".to_string(),
        ];
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    #[test]
    fn compacts_inline_raw_and_duplicates_into_light_rows_a_sidecar_and_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = old_log(dir.path());
        let before = std::fs::read_to_string(&path).unwrap();
        let summary_before = log::read(&path).unwrap();

        let outcome = compact(&path, false, Duration::ZERO).unwrap();
        assert!(outcome.changed);
        assert_eq!((outcome.events, outcome.moved_raw, outcome.duplicates_removed), (3, 2, 1));

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.starts_with(&format!("{SCHEMA_HEADER}\n")), "{after}");
        assert!(!after.contains("\"raw\""), "{after}");
        assert_eq!(after.matches("\"id\":\"e1\"").count(), 1, "{after}");
        assert!(after.contains("not json"), "unreadable lines are kept");
        let raws = log::read_raw(&log::raw_path_for(&path)).unwrap();
        assert_eq!(raws.len(), 2);
        assert_eq!(raws["e2"], json!({"identifier": "COR-2", "body": "second"}));
        assert_eq!(std::fs::read_to_string(backup_path_for(&path)).unwrap(), before);

        let summary_after = log::read(&path).unwrap();
        assert_eq!(summary_after.event_ids, summary_before.event_ids);
        assert_eq!(summary_after.event_count, 3);
        assert!(!dir.path().join("work/claims/tracker.jsonl.compact").exists());
    }

    #[test]
    fn second_run_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = old_log(dir.path());
        compact(&path, false, Duration::ZERO).unwrap();
        let log_after = std::fs::read(&path).unwrap();
        let sidecar_after = std::fs::read(log::raw_path_for(&path)).unwrap();
        let backup_after = std::fs::read(backup_path_for(&path)).unwrap();

        let again = compact(&path, false, Duration::ZERO).unwrap();
        assert!(!again.changed);
        assert_eq!(again.events, 3);
        assert_eq!(std::fs::read(&path).unwrap(), log_after);
        assert_eq!(std::fs::read(log::raw_path_for(&path)).unwrap(), sidecar_after);
        assert_eq!(std::fs::read(backup_path_for(&path)).unwrap(), backup_after, "backup kept");
    }

    #[test]
    fn refuses_when_a_backup_exists_unless_forced() {
        let dir = tempfile::tempdir().unwrap();
        let path = old_log(dir.path());
        std::fs::write(backup_path_for(&path), "older backup").unwrap();
        let before = std::fs::read(&path).unwrap();
        let error = compact(&path, false, Duration::ZERO).unwrap_err();
        assert!(error.contains("--force"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!log::raw_path_for(&path).exists(), "nothing written before the refusal");

        let outcome = compact(&path, true, Duration::ZERO).unwrap();
        assert!(outcome.changed);
        assert_eq!(std::fs::read(backup_path_for(&path)).unwrap(), before, "the backup is the log just replaced");
    }

    #[test]
    fn verification_failure_leaves_the_original_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = log::path_for(dir.path(), "work", "claims");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = [
            SCHEMA_HEADER.to_string(),
            row("e1", "COR-1", json!({"body": "first"})),
            row("e1", "COR-9", json!({"body": "same id, other content"})),
        ]
        .join("\n")
            + "\n";
        std::fs::write(&path, &original).unwrap();

        let error = compact(&path, false, Duration::ZERO).unwrap_err();
        assert!(error.contains("left as it was"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(!backup_path_for(&path).exists());
        assert!(!dir.path().join("work/claims/tracker.jsonl.compact").exists());
    }

    #[test]
    fn sidecar_payload_that_disagrees_with_the_inline_one_stops_the_compact() {
        let dir = tempfile::tempdir().unwrap();
        let path = log::path_for(dir.path(), "work", "claims");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = format!("{SCHEMA_HEADER}\n{}\n", row("e1", "COR-1", json!({"body": "inline"})));
        std::fs::write(&path, &original).unwrap();
        log::append_raw(&log::raw_path_for(&path), "e1", json!({"body": "other"})).unwrap();

        let error = compact(&path, false, Duration::ZERO).unwrap_err();
        assert!(error.contains("not readable from the sidecar"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn compact_waits_for_the_lock_then_fails_with_the_closed_code() {
        let dir = tempfile::tempdir().unwrap();
        let path = old_log(dir.path());
        let before = std::fs::read(&path).unwrap();
        let _held = lock::acquire(&path, Duration::ZERO).unwrap();
        let error = compact(&path, false, Duration::from_millis(100)).unwrap_err();
        assert!(error.contains(lock::LOCK_BUSY), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn missing_log_is_nothing_to_do() {
        let dir = tempfile::tempdir().unwrap();
        let path = log::path_for(dir.path(), "work", "claims");
        assert!(!compact(&path, false, Duration::ZERO).unwrap().changed);
        assert!(!path.exists());
    }
}
