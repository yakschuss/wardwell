//! Reads and appends the per-project `tracker.jsonl` event log.
//!
//! The log is its own cursor: the reader derives the cursor from the latest
//! completed-pull or full-resync marker, the known event ids, and the issues
//! still open in the mirror. Appends go
//! only through `kanban::jsonl::append_line`, after ending a torn last line
//! with a newline. Does NOT talk to any provider.

use crate::kanban::jsonl::append_line;
use crate::tracker::events::{Event, RAW_FILE_NAME, RAW_SCHEMA_HEADER, SCHEMA_HEADER};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// What the mirror currently knows about an issue it has not seen removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenIssue {
    pub external_id: String,
    /// The issue's own title from its latest snapshot.
    pub issue_title: String,
}

/// Derived state of a tracker log.
#[derive(Debug, Default, Clone)]
pub struct LogSummary {
    /// `through` of the latest pull_completed or full_resync marker in file
    /// order. Provider events never move it, so a pull that failed part way
    /// leaves it where the last completed pull put it.
    pub cursor: Option<DateTime<Utc>>,
    /// When the latest pull_completed or full_resync marker was written.
    pub last_pull_at: Option<DateTime<Utc>>,
    pub last_full_resync_at: Option<DateTime<Utc>>,
    pub event_ids: HashSet<String>,
    pub open_issues: BTreeMap<String, OpenIssue>,
    /// Keys whose latest issue event is `issue_removed`, with its time.
    pub removed_at: BTreeMap<String, DateTime<Utc>>,
    pub event_count: usize,
    pub unreadable_lines: usize,
    /// When the latest pull_failed marker was written, and its code.
    pub last_failure: Option<(DateTime<Utc>, crate::tracker::events::FailureCode)>,
}

impl LogSummary {
    fn observe(&mut self, event: &Event) {
        let common = event.common();
        self.event_ids.insert(common.id.clone());
        self.event_count += 1;
        match event {
            Event::IssueRemoved { .. } => {
                self.open_issues.remove(&common.external_key);
                self.removed_at.insert(common.external_key.clone(), common.occurred_at);
            }
            Event::FullResync { through, .. } => {
                self.last_full_resync_at = later(self.last_full_resync_at, common.occurred_at);
                self.mark_pull(common.occurred_at, *through);
            }
            Event::PullCompleted { through, .. } => self.mark_pull(common.occurred_at, *through),
            Event::PullFailed { code, .. } => self.last_failure = Some((common.occurred_at, *code)),
            Event::IssueUpserted { issue, .. } => {
                self.removed_at.remove(&common.external_key);
                self.open_issues.insert(common.external_key.clone(), OpenIssue {
                    external_id: common.external_id.clone(),
                    issue_title: issue.issue_title.clone(),
                });
            }
            _ => {}
        }
    }

    fn mark_pull(&mut self, at: DateTime<Utc>, through: Option<DateTime<Utc>>) {
        self.last_pull_at = Some(at);
        self.cursor = through;
    }
}

/// The later of an optional time and a candidate.
pub fn later(current: Option<DateTime<Utc>>, candidate: DateTime<Utc>) -> Option<DateTime<Utc>> {
    Some(current.map_or(candidate, |c| c.max(candidate)))
}

/// `<vault>/<domain>/<project>/tracker.jsonl`.
pub fn path_for(vault_root: &Path, domain: &str, project: &str) -> PathBuf {
    vault_root.join(domain).join(project).join(crate::tracker::events::FILE_NAME)
}

/// Summarize the log at `path`. A missing file is an empty log; unreadable
/// lines are counted and skipped.
pub fn read(path: &Path) -> Result<LogSummary, String> {
    let mut summary = LogSummary::default();
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(summary),
        Err(_) => return Err(format!("could not read {}", path.display())),
    };
    for line in content.lines().map(str::trim) {
        if line.is_empty() || line.starts_with("{\"_schema\"") {
            continue;
        }
        match serde_json::from_str::<Event>(line) {
            Ok(event) => summary.observe(&event),
            Err(_) => summary.unreadable_lines += 1,
        }
    }
    Ok(summary)
}

/// Append events whose id is not already in `summary`, in order, and fold
/// them into `summary`. A snapshot of a removed issue is appended even when
/// its id is known, under a restored id, so the issue reappears. Each
/// event's raw payload goes to the sidecar first, then its light row to the
/// log. Returns how many were written.
pub fn append_new(path: &Path, events: &[Event], summary: &mut LogSummary) -> Result<usize, String> {
    let raw_path = raw_path_for(path);
    let mut written = 0;
    for event in events {
        let event = match (summary.event_ids.contains(&event.common().id), restored(event, summary)) {
            (false, _) => event.clone(),
            (true, Some(restored)) if !summary.event_ids.contains(&restored.common().id) => restored,
            (true, _) => continue,
        };
        let (light, raw) = split_raw(event);
        if !raw.is_null() {
            append_raw(&raw_path, &light.common().id, raw)?;
        }
        let line = serde_json::to_string(&light).map_err(|_| "could not encode tracker event".to_string())?;
        append_after_newline(path, SCHEMA_HEADER, &line)?;
        summary.observe(&light);
        written += 1;
    }
    Ok(written)
}

/// A snapshot of an issue whose latest event is `issue_removed`, with its
/// id suffixed `:restored:<removal time>` so it is unique. None otherwise.
fn restored(event: &Event, summary: &LogSummary) -> Option<Event> {
    let Event::IssueUpserted { common, .. } = event else {
        return None;
    };
    let removed = summary.removed_at.get(&common.external_key)?;
    let mut restored = event.clone();
    let id = format!("{}:restored:{}", common.id, removed.to_rfc3339());
    common_mut(&mut restored).id = id;
    Some(restored)
}

/// The event without its raw payload, and the payload.
pub fn split_raw(mut event: Event) -> (Event, Value) {
    let raw = std::mem::take(&mut common_mut(&mut event).raw);
    (event, raw)
}

fn common_mut(event: &mut Event) -> &mut crate::tracker::events::Common {
    match event {
        Event::IssueUpserted { common, .. }
        | Event::CommentUpserted { common, .. }
        | Event::StateChanged { common, .. }
        | Event::LinkAdded { common, .. }
        | Event::IssueRemoved { common }
        | Event::FullResync { common, .. }
        | Event::PullCompleted { common, .. }
        | Event::PullFailed { common, .. } => common,
    }
}

/// One line of the raw sidecar.
#[derive(Debug, Serialize, Deserialize)]
pub struct RawRecord {
    pub id: String,
    pub raw: Value,
}

/// The sidecar beside a log: `tracker.raw.jsonl` in the same folder.
pub fn raw_path_for(log_path: &Path) -> PathBuf {
    log_path.with_file_name(RAW_FILE_NAME)
}

/// Append one raw payload to the sidecar at `raw_path`.
pub fn append_raw(raw_path: &Path, id: &str, raw: Value) -> Result<(), String> {
    let record = RawRecord { id: id.to_string(), raw };
    let line = serde_json::to_string(&record).map_err(|_| "could not encode a raw tracker payload".to_string())?;
    append_after_newline(raw_path, RAW_SCHEMA_HEADER, &line)
}

/// Append `line` through `append_line`, first ending a torn last line (a
/// crash mid-append) with a newline so it cannot swallow `line`.
fn append_after_newline(path: &Path, header: &str, line: &str) -> Result<(), String> {
    let failed = |error: std::io::Error| format!("could not append to {}: {error}", path.display());
    if ends_without_newline(path).map_err(failed)? {
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .and_then(|mut file| std::io::Write::write_all(&mut file, b"\n"))
            .map_err(failed)?;
    }
    append_line(path, Some(header), line).map_err(failed)
}

/// True when `path` exists, is not empty, and its last byte is not `\n`.
fn ends_without_newline(path: &Path) -> std::io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] != b'\n')
}

/// Every raw payload in the sidecar by event id; the first line for an id
/// wins. A missing sidecar is empty; unreadable lines are skipped.
pub fn read_raw(raw_path: &Path) -> Result<HashMap<String, Value>, String> {
    let content = match std::fs::read_to_string(raw_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(_) => return Err(format!("could not read {}", raw_path.display())),
    };
    let mut raws = HashMap::new();
    for line in content.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("{\"_schema\"")) {
        if let Ok(record) = serde_json::from_str::<RawRecord>(line) {
            raws.entry(record.id).or_insert(record.raw);
        }
    }
    Ok(raws)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::tracker::events::{Common, Event, IssueSnapshot, Priority, StateCategory};
    use chrono::{TimeZone, Utc};

    fn common(id: &str, key: &str, hour: u32) -> Common {
        Common {
            id: id.into(),
            provider: "linear".into(),
            external_key: key.into(),
            external_id: format!("{key}-uuid"),
            actor: None,
            occurred_at: Utc.with_ymd_and_hms(2026, 9, 1, hour, 0, 0).unwrap(),
            title: format!("{key} Title"),
            raw: serde_json::Value::Null,
        }
    }

    fn upsert(id: &str, key: &str, hour: u32) -> Event {
        Event::IssueUpserted {
            common: common(id, key, hour),
            issue: Box::new(IssueSnapshot {
                issue_title: "Title".into(),
                description: None,
                state: "Todo".into(),
                state_category: StateCategory::Unstarted,
                priority: Priority::None,
                team: None,
                project: None,
                assignee: None,
                creator: None,
                labels: vec![],
                url: None,
                created_at: None,
                archived_at: None,
                ..Default::default()
            }),
        }
    }

    fn with_raw(mut event: Event, raw: serde_json::Value) -> Event {
        match &mut event {
            Event::IssueUpserted { common, .. } => common.raw = raw,
            _ => unreachable!(),
        }
        event
    }

    #[test]
    fn append_writes_light_rows_and_raw_to_the_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d/p/tracker.jsonl");
        let mut summary = read(&path).unwrap();
        let events = vec![
            with_raw(upsert("e1", "COR-1", 9), serde_json::json!({"identifier": "COR-1", "big": "payload"})),
            upsert("e2", "COR-2", 10),
        ];
        append_new(&path, &events, &mut summary).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("\"raw\""), "{content}");
        assert!(!content.contains("payload"), "{content}");
        let raw_path = raw_path_for(&path);
        assert_eq!(raw_path, dir.path().join("d/p/tracker.raw.jsonl"));
        let sidecar = std::fs::read_to_string(&raw_path).unwrap();
        assert_eq!(sidecar.lines().next().unwrap(), crate::tracker::events::RAW_SCHEMA_HEADER);
        assert_eq!(sidecar.lines().count(), 2, "header and one payload; a null raw writes nothing: {sidecar}");
        let raws = read_raw(&raw_path).unwrap();
        assert_eq!(raws["e1"], serde_json::json!({"identifier": "COR-1", "big": "payload"}));
        assert!(!raws.contains_key("e2"));
    }

    #[test]
    fn sidecar_is_written_before_the_log_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tracker.jsonl");
        std::fs::create_dir_all(raw_path_for(&path)).unwrap();
        let mut summary = read(&path).unwrap();
        let event = with_raw(upsert("e1", "COR-1", 9), serde_json::json!({"id": "x"}));
        let error = append_new(&path, &[event], &mut summary).unwrap_err();
        assert!(error.contains("tracker.raw.jsonl"), "{error}");
        assert!(!path.exists(), "no log line without its raw payload");
        assert!(summary.event_ids.is_empty());
    }

    #[test]
    fn a_torn_tail_does_not_swallow_the_next_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tracker.jsonl");
        let raw_path = raw_path_for(&path);
        let torn = format!("{}\n{{\"id\":\"e0\",\"ra", crate::tracker::events::RAW_SCHEMA_HEADER);
        std::fs::write(&raw_path, &torn).unwrap();
        std::fs::write(&path, format!("{}\n{{\"kind\":\"issue_ups", crate::tracker::events::SCHEMA_HEADER)).unwrap();

        let mut summary = read(&path).unwrap();
        let event = with_raw(upsert("e1", "COR-1", 9), serde_json::json!({"body": "next"}));
        append_new(&path, &[event], &mut summary).unwrap();

        assert_eq!(read_raw(&raw_path).unwrap()["e1"], serde_json::json!({"body": "next"}));
        assert!(std::fs::read_to_string(&raw_path).unwrap().starts_with(&format!("{torn}\n")), "the torn line is kept on its own line");
        let summary = read(&path).unwrap();
        assert!(summary.event_ids.contains("e1"));
        assert_eq!(summary.unreadable_lines, 1);
    }

    #[test]
    fn old_rows_with_inline_raw_still_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tracker.jsonl");
        let mut old = serde_json::to_value(upsert("e1", "COR-1", 9)).unwrap();
        old["raw"] = serde_json::json!({"identifier": "COR-1"});
        std::fs::write(&path, format!("{}\n{old}\n", crate::tracker::events::SCHEMA_HEADER)).unwrap();
        let summary = read(&path).unwrap();
        assert_eq!(summary.event_count, 1);
        assert_eq!(summary.unreadable_lines, 0);
        assert!(summary.open_issues.contains_key("COR-1"));
    }

    #[test]
    fn missing_file_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let summary = read(&dir.path().join("tracker.jsonl")).unwrap();
        assert!(summary.cursor.is_none());
        assert!(summary.last_pull_at.is_none());
        assert!(summary.event_ids.is_empty());
        assert_eq!(summary.event_count, 0);
    }

    #[test]
    fn append_writes_header_once_and_dedups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d/p/tracker.jsonl");
        let mut summary = read(&path).unwrap();
        let first = vec![upsert("e1", "COR-1", 9), upsert("e1", "COR-1", 9), upsert("e2", "COR-2", 10)];
        assert_eq!(append_new(&path, &first, &mut summary).unwrap(), 2);
        let again = vec![upsert("e2", "COR-2", 10), upsert("e3", "COR-2", 11)];
        assert_eq!(append_new(&path, &again, &mut summary).unwrap(), 1);

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], crate::tracker::events::SCHEMA_HEADER);
        assert_eq!(content.matches("_schema").count(), 1);
    }

    #[test]
    fn read_reports_newest_provider_time_ids_and_open_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tracker.jsonl");
        let mut summary = read(&path).unwrap();
        let events = vec![
            upsert("e1", "COR-1", 9),
            upsert("e2", "COR-2", 11),
            Event::IssueRemoved { common: common("r1", "COR-1", 23) },
            Event::FullResync { common: common("f1", "COR", 23), issues: 1, removed: 1, through: Some(Utc.with_ymd_and_hms(2026, 9, 1, 11, 0, 0).unwrap()) },
            upsert("e3", "COR-2", 22),
        ];
        append_new(&path, &events, &mut summary).unwrap();
        std::fs::OpenOptions::new().append(true).open(&path).and_then(|mut f| {
            use std::io::Write;
            writeln!(f, "not json")
        }).unwrap();

        let summary = read(&path).unwrap();
        assert_eq!(
            summary.cursor,
            Some(Utc.with_ymd_and_hms(2026, 9, 1, 11, 0, 0).unwrap()),
            "the cursor is the latest marker's through; provider events alone do not move it"
        );
        assert_eq!(summary.last_pull_at, Some(Utc.with_ymd_and_hms(2026, 9, 1, 23, 0, 0).unwrap()));
        assert_eq!(summary.event_count, 5);
        assert_eq!(summary.unreadable_lines, 1);
        assert!(summary.event_ids.contains("e1") && summary.event_ids.contains("f1"));
        assert_eq!(summary.open_issues.keys().collect::<Vec<_>>(), vec!["COR-2"]);
        assert_eq!(summary.open_issues["COR-2"].issue_title, "Title");
        assert_eq!(
            summary.last_full_resync_at,
            Some(Utc.with_ymd_and_hms(2026, 9, 1, 23, 0, 0).unwrap())
        );
    }

    #[test]
    fn latest_marker_in_file_order_sets_the_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tracker.jsonl");
        let mut summary = read(&path).unwrap();
        let hour = |h| Utc.with_ymd_and_hms(2026, 9, 1, h, 0, 0).unwrap();
        let events = vec![
            Event::PullCompleted { common: common("p1", "COR", 12), through: Some(hour(10)) },
            Event::FullResync { common: common("f1", "COR", 14), issues: 2, removed: 0, through: Some(hour(11)) },
            Event::PullCompleted { common: common("p2", "COR", 15), through: Some(hour(13)) },
        ];
        append_new(&path, &events, &mut summary).unwrap();
        let summary = read(&path).unwrap();
        assert_eq!(summary.cursor, Some(hour(13)));
        assert_eq!(summary.last_pull_at, Some(hour(15)));
        assert_eq!(summary.last_full_resync_at, Some(hour(14)));
    }

    #[test]
    fn upsert_after_removal_reopens_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tracker.jsonl");
        let mut summary = read(&path).unwrap();
        let events = vec![
            upsert("e1", "COR-1", 9),
            Event::IssueRemoved { common: common("r1", "COR-1", 10) },
            upsert("e2", "COR-1", 12),
        ];
        append_new(&path, &events, &mut summary).unwrap();
        assert!(read(&path).unwrap().open_issues.contains_key("COR-1"));
        assert!(summary.open_issues.contains_key("COR-1"), "in-memory summary tracks appends");
    }
}
