//! Migrates a project's `tracker.jsonl` to light rows: inline raw payloads
//! move to the `tracker.raw.jsonl` sidecar and exact duplicate events go.
//!
//! The only code that rewrites a tracker log, and only `tracker.jsonl`: the
//! log is a re-pullable mirror, not a system of record. Under the project
//! lock it refuses rows that share an id but differ, writes the sidecar,
//! verifies, writes the new log beside the old one, links the old one to
//! `tracker.jsonl.bak.new`, renames the new log into place, then renames
//! `.bak.new` over `tracker.jsonl.bak`.
//! Does NOT pull, and does NOT touch any other vault file.

use crate::kanban::jsonl::retry_transient;
use crate::tracker::events::Event;
use crate::tracker::{lock, log};
use serde_json::Value;
use std::collections::HashMap;
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
    compact_with(log_path, force, wait, &|from, to| std::fs::rename(from, to))
}

/// Renames one file over another. Injected so tests can make a rename fail.
type Rename<'a> = dyn Fn(&Path, &Path) -> std::io::Result<()> + 'a;

fn compact_with(log_path: &Path, force: bool, wait: Duration, rename: &Rename<'_>) -> Result<CompactOutcome, String> {
    let _lock = lock::acquire(log_path, wait)?;
    let original = match std::fs::read_to_string(log_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(unchanged(0)),
        Err(_) => return Err(format!("could not read {}", log_path.display())),
    };
    let raw_path = log::raw_path_for(log_path);
    let plan = Plan::build(&original, &log::read_raw(&raw_path)?)?;
    plan.refuse_conflicts(log_path)?;
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
    replace_log(log_path, &plan.content, &backup, rename)?;
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
    /// The first id, in file order, carried by rows that differ.
    conflict: Option<String>,
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
            conflict: None,
            duplicates: 0,
            inline_raw: HashMap::new(),
            to_move: Vec::new(),
        };
        let mut kept: HashMap<String, Vec<(Value, Value)>> = HashMap::new();
        for line in original.lines() {
            let Some((id, light, inline)) = parse_row(line) else {
                // Headers, blank and unreadable lines are kept as they are.
                plan.push_line(line);
                continue;
            };
            let raw = inline.clone().unwrap_or(Value::Null);
            let same_id = kept.entry(id.clone()).or_default();
            if same_id.iter().any(|(l, r)| *l == light && *r == raw) {
                plan.duplicates += 1;
                continue;
            }
            if !same_id.is_empty() && plan.conflict.is_none() {
                plan.conflict = Some(id.clone());
            }
            same_id.push((light.clone(), raw));
            match inline {
                // Without an inline raw the row is kept byte for byte.
                None => plan.push_line(line),
                Some(raw) => {
                    plan.take_raw(&id, raw, sidecar);
                    let encoded = serde_json::to_string(&light).map_err(|_| "could not encode tracker event".to_string())?;
                    plan.push_line(&encoded);
                }
            }
            plan.events += 1;
        }
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

    /// Rows that share an id but differ cannot be deduplicated safely.
    /// Checked before anything is written; `--force` does not override it.
    fn refuse_conflicts(&self, log_path: &Path) -> Result<(), String> {
        match &self.conflict {
            None => Ok(()),
            Some(id) => Err(format!(
                "compact stopped: rows with id {id} differ; resolve them by hand in {}, --force does not override this; the log is left as it was",
                log_path.display()
            )),
        }
    }

    /// Every inline payload readable from the sidecar by its event id with
    /// the same content.
    fn verify(&self, sidecar: &HashMap<String, Value>) -> Result<(), String> {
        let unreadable = self.inline_raw.iter().filter(|(id, raw)| sidecar.get(*id) != Some(*raw)).count();
        match unreadable {
            0 => Ok(()),
            n => Err(format!("compact stopped: {n} raw payloads are not readable from the sidecar; the log is left as it was")),
        }
    }
}

/// A tracker event row as its id, the row without its `raw` key, and the
/// inline raw when the row has the key. Every other field, known to this
/// build or not, stays in the row. None for headers and unreadable lines.
fn parse_row(line: &str) -> Option<(String, Value, Option<Value>)> {
    let trimmed = line.trim();
    if trimmed.starts_with("{\"_schema\"") {
        return None;
    }
    let event = serde_json::from_str::<Event>(trimmed).ok()?;
    let mut light = serde_json::from_str::<Value>(trimmed).ok()?;
    let inline = light.as_object_mut()?.remove("raw");
    Some((event.common().id.clone(), light, inline))
}

/// Write `content` beside the log, link the old log to `.bak.new`, rename
/// the new file over the log, then `.bak.new` over `backup`. On any failure
/// the temp files go and the log and the old backup stay as they were.
fn replace_log(log_path: &Path, content: &str, backup: &Path, rename: &Rename<'_>) -> Result<(), String> {
    let temp = with_suffix(log_path, ".compact");
    let next_backup = with_suffix(log_path, ".bak.new");
    let cleanup = || {
        let _ = std::fs::remove_file(&temp);
        let _ = std::fs::remove_file(&next_backup);
    };
    let written = std::fs::File::create(&temp)
        .and_then(|mut file| file.write_all(content.as_bytes()).and_then(|_| file.sync_all()));
    if written.is_err() {
        cleanup();
        return Err(format!("could not write {}", temp.display()));
    }
    let _ = std::fs::remove_file(&next_backup);
    let linked = std::fs::hard_link(log_path, &next_backup).or_else(|_| std::fs::copy(log_path, &next_backup).map(|_| ()));
    if linked.is_err() {
        cleanup();
        return Err(format!("could not keep a backup at {}", next_backup.display()));
    }
    if retry_transient(log_path, || rename(&temp, log_path)).is_err() {
        cleanup();
        return Err(format!("could not replace {}; the log and the old backup are as they were", log_path.display()));
    }
    if retry_transient(backup, || rename(&next_backup, backup)).is_err() {
        // Put the old log back so the log and its backup stay a pair.
        let restored = retry_transient(log_path, || rename(&next_backup, log_path));
        let _ = std::fs::remove_file(&temp);
        return Err(match restored {
            Ok(()) => format!("could not keep a backup at {}; the log and the old backup are as they were", backup.display()),
            Err(_) => format!(
                "could not keep a backup at {}; the log is compacted, the old backup is unchanged, and the log before this compact is at {}",
                backup.display(),
                next_backup.display()
            ),
        });
    }
    Ok(())
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

        let outcome = compact(&path, false, lock::TEST_FREE_WAIT).unwrap();
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
    fn two_providers_in_one_log_compact_and_verify_together() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work/claims/tracker.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut lines: Vec<String> = crate::tracker::view::two_provider_log().lines().map(str::to_string).collect();
        for (index, raw) in [(1, json!({"identifier": "COR-1"})), (3, json!({"number": 42, "title": "COR-1 Fix the claims inbox"}))] {
            let mut row: Value = serde_json::from_str(&lines[index]).unwrap();
            row["raw"] = raw;
            lines[index] = row.to_string();
        }
        lines.push(lines[3].clone());
        lines.push(lines[2].clone());
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let linear_before = log::read_for(&path, "linear").unwrap();
        let github_before = log::read_for(&path, "github").unwrap();

        let outcome = compact(&path, false, lock::TEST_FREE_WAIT).unwrap();
        assert_eq!((outcome.events, outcome.moved_raw, outcome.duplicates_removed), (4, 2, 2));
        let raws = log::read_raw(&log::raw_path_for(&path)).unwrap();
        assert_eq!(raws["github:acme/app#42"]["number"], 42);
        assert_eq!(raws["linear:issue:i1:1"]["identifier"], "COR-1");
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains("\"raw\""), "{after}");
        assert_eq!(after.matches("\"kind\":\"change_merged\"").count(), 1, "{after}");
        for (provider, before) in [("linear", linear_before), ("github", github_before)] {
            let now = log::read_for(&path, provider).unwrap();
            assert_eq!((now.cursor, now.last_pull_at, now.last_failure), (before.cursor, before.last_pull_at, before.last_failure), "{provider}");
        }
        assert!(!compact(&path, false, lock::TEST_FREE_WAIT).unwrap().changed);
    }

    #[test]
    fn second_run_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = old_log(dir.path());
        compact(&path, false, lock::TEST_FREE_WAIT).unwrap();
        let log_after = std::fs::read(&path).unwrap();
        let sidecar_after = std::fs::read(log::raw_path_for(&path)).unwrap();
        let backup_after = std::fs::read(backup_path_for(&path)).unwrap();

        let again = compact(&path, false, lock::TEST_FREE_WAIT).unwrap();
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
        let error = compact(&path, false, lock::TEST_FREE_WAIT).unwrap_err();
        assert!(error.contains("--force"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!log::raw_path_for(&path).exists(), "nothing written before the refusal");

        let outcome = compact(&path, true, lock::TEST_FREE_WAIT).unwrap();
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

        let error = compact(&path, false, lock::TEST_FREE_WAIT).unwrap_err();
        assert!(error.contains("left as it was"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(!backup_path_for(&path).exists());
        assert!(!dir.path().join("work/claims/tracker.jsonl.compact").exists());
    }

    #[test]
    fn rows_that_share_an_id_but_differ_stop_before_any_write_even_forced() {
        let dir = tempfile::tempdir().unwrap();
        let path = log::path_for(dir.path(), "work", "claims");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = [
            SCHEMA_HEADER.to_string(),
            row("e2", "COR-2", json!({"v": 3})),
            row("e1", "COR-1", json!({"v": 1})),
            row("e1", "COR-1", json!({"v": 2})),
        ]
        .join("\n")
            + "\n";
        std::fs::write(&path, &original).unwrap();
        for force in [false, true] {
            let error = compact(&path, force, lock::TEST_FREE_WAIT).unwrap_err();
            assert!(error.contains("e1"), "names the id: {error}");
            assert!(!error.contains("e2"), "{error}");
            assert!(error.contains("by hand"), "{error}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
            assert!(!log::raw_path_for(&path).exists(), "nothing written to the sidecar");
            assert!(!backup_path_for(&path).exists());
            assert!(leftovers(&path).is_empty());
        }
    }

    #[test]
    fn sidecar_payload_that_disagrees_with_the_inline_one_stops_the_compact() {
        let dir = tempfile::tempdir().unwrap();
        let path = log::path_for(dir.path(), "work", "claims");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = format!("{SCHEMA_HEADER}\n{}\n", row("e1", "COR-1", json!({"body": "inline"})));
        std::fs::write(&path, &original).unwrap();
        log::append_raw(&log::raw_path_for(&path), "e1", json!({"body": "other"})).unwrap();

        let error = compact(&path, false, lock::TEST_FREE_WAIT).unwrap_err();
        assert!(error.contains("not readable from the sidecar"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn compact_waits_for_the_lock_then_fails_with_the_closed_code() {
        let dir = tempfile::tempdir().unwrap();
        let path = old_log(dir.path());
        let before = std::fs::read(&path).unwrap();
        let _held = lock::acquire(&path, lock::TEST_FREE_WAIT).unwrap();
        let error = compact(&path, false, Duration::from_millis(100)).unwrap_err();
        assert!(error.contains(lock::LOCK_BUSY), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    /// A log with inline raw, an older backup beside it, and the content of both.
    fn forced_setup(dir: &Path) -> (PathBuf, Vec<u8>, String) {
        let path = old_log(dir);
        std::fs::write(backup_path_for(&path), "older backup").unwrap();
        (path.clone(), std::fs::read(&path).unwrap(), "older backup".to_string())
    }

    fn leftovers(path: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".compact") || n.ends_with(".bak.new"))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn failed_rename_over_the_log_keeps_the_old_backup_and_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let (path, log_before, backup_before) = forced_setup(dir.path());
        let failing = |from: &Path, to: &Path| -> std::io::Result<()> {
            match to.file_name().and_then(|n| n.to_str()) == Some("tracker.jsonl") && from.to_string_lossy().ends_with(".compact") {
                true => Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
                false => std::fs::rename(from, to),
            }
        };
        let error = compact_with(&path, true, lock::TEST_FREE_WAIT, &failing).unwrap_err();
        assert!(error.contains("could not replace"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), log_before);
        assert_eq!(std::fs::read_to_string(backup_path_for(&path)).unwrap(), backup_before);
        assert!(leftovers(&path).is_empty(), "{:?}", leftovers(&path));
    }

    #[test]
    fn failed_rename_of_the_new_backup_restores_the_log_and_keeps_the_old_backup() {
        let dir = tempfile::tempdir().unwrap();
        let (path, log_before, backup_before) = forced_setup(dir.path());
        let failing = |from: &Path, to: &Path| -> std::io::Result<()> {
            match to.to_string_lossy().ends_with(".bak") {
                true => Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
                false => std::fs::rename(from, to),
            }
        };
        let error = compact_with(&path, true, lock::TEST_FREE_WAIT, &failing).unwrap_err();
        assert!(error.contains("could not keep a backup"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), log_before);
        assert_eq!(std::fs::read_to_string(backup_path_for(&path)).unwrap(), backup_before);
        assert!(leftovers(&path).is_empty(), "{:?}", leftovers(&path));
    }

    #[test]
    fn a_transient_eperm_on_rename_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        let (path, log_before, _) = forced_setup(dir.path());
        let failures = std::cell::Cell::new(0);
        let flaky = |from: &Path, to: &Path| -> std::io::Result<()> {
            match failures.get() {
                0 => {
                    failures.set(1);
                    Err(std::io::Error::from_raw_os_error(1))
                }
                _ => std::fs::rename(from, to),
            }
        };
        let outcome = compact_with(&path, true, lock::TEST_FREE_WAIT, &flaky).unwrap();
        assert!(outcome.changed);
        assert_eq!(std::fs::read(backup_path_for(&path)).unwrap(), log_before, "the backup is the log just replaced");
        assert!(leftovers(&path).is_empty(), "{:?}", leftovers(&path));
    }

    /// A row a newer writer produced: an unknown top-level field and an
    /// unknown field inside `issue`, keys in an order serde would not write.
    fn newer_row(id: &str, key: &str, raw: Option<Value>) -> String {
        let mut value: Value = serde_json::from_str(&row(id, key, Value::Null)).unwrap();
        value.as_object_mut().unwrap().remove("raw");
        value["grooming_note"] = json!("keep me");
        value["issue"]["estimate"] = json!(5);
        let mut text = format!("{{\"zz_first\":true,{}", &value.to_string()[1..]);
        if let Some(raw) = raw {
            text = format!("{},\"raw\":{raw}}}", &text[..text.len() - 1]);
        }
        text
    }

    #[test]
    fn compact_keeps_fields_this_build_does_not_know() {
        let dir = tempfile::tempdir().unwrap();
        let path = log::path_for(dir.path(), "work", "claims");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let light = newer_row("e1", "COR-1", None);
        let heavy = newer_row("e2", "COR-2", Some(json!({"body": "payload"})));
        std::fs::write(&path, format!("{SCHEMA_HEADER}\n{light}\n{heavy}\n")).unwrap();

        let outcome = compact(&path, false, lock::TEST_FREE_WAIT).unwrap();
        assert_eq!(outcome.moved_raw, 1);
        let after = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = after.lines().collect();
        assert_eq!(lines[1], light, "a row without inline raw is kept byte for byte");
        let moved: Value = serde_json::from_str(lines[2]).unwrap();
        let mut expected: Value = serde_json::from_str(&heavy).unwrap();
        expected.as_object_mut().unwrap().remove("raw");
        assert_eq!(moved, expected, "only the raw key goes");
        assert_eq!(moved["grooming_note"], "keep me");
        assert_eq!(moved["zz_first"], true);
        assert_eq!(moved["issue"]["estimate"], 5);
        assert_eq!(log::read_raw(&log::raw_path_for(&path)).unwrap()["e2"], json!({"body": "payload"}));
        assert!(!compact(&path, false, lock::TEST_FREE_WAIT).unwrap().changed, "second run is a no-op");
    }

    #[test]
    fn missing_log_is_nothing_to_do() {
        let dir = tempfile::tempdir().unwrap();
        let path = log::path_for(dir.path(), "work", "claims");
        assert!(!compact(&path, false, lock::TEST_FREE_WAIT).unwrap().changed);
        assert!(!path.exists());
    }
}
