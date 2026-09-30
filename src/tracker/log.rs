//! Reads and appends the per-project `tracker.jsonl` event log.
//!
//! The log is its own cursor: the reader derives the newest provider time,
//! the known event ids, and the issues still open in the mirror. Appends go
//! only through `kanban::jsonl::append_line`. Does NOT talk to any provider.

use crate::kanban::jsonl::append_line;
use crate::tracker::events::{Event, SCHEMA_HEADER};
use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, HashSet};
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
    /// Newest `occurred_at` among provider-originated events. Local markers
    /// (issue_removed, full_resync) are excluded so they never move the cursor.
    pub newest_provider_event_at: Option<DateTime<Utc>>,
    pub last_full_resync_at: Option<DateTime<Utc>>,
    pub event_ids: HashSet<String>,
    pub open_issues: BTreeMap<String, OpenIssue>,
    pub event_count: usize,
    pub unreadable_lines: usize,
}

impl LogSummary {
    fn observe(&mut self, event: &Event) {
        let common = event.common();
        self.event_ids.insert(common.id.clone());
        self.event_count += 1;
        match event {
            Event::IssueRemoved { .. } => {
                self.open_issues.remove(&common.external_key);
            }
            Event::FullResync { .. } => {
                self.last_full_resync_at = later(self.last_full_resync_at, common.occurred_at);
            }
            Event::IssueUpserted { issue, .. } => {
                self.open_issues.insert(common.external_key.clone(), OpenIssue {
                    external_id: common.external_id.clone(),
                    issue_title: issue.issue_title.clone(),
                });
                self.newest_provider_event_at = later(self.newest_provider_event_at, common.occurred_at);
            }
            _ => {
                self.newest_provider_event_at = later(self.newest_provider_event_at, common.occurred_at);
            }
        }
    }
}

fn later(current: Option<DateTime<Utc>>, candidate: DateTime<Utc>) -> Option<DateTime<Utc>> {
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
/// them into `summary`. Returns how many were written.
pub fn append_new(path: &Path, events: &[Event], summary: &mut LogSummary) -> Result<usize, String> {
    let mut written = 0;
    for event in events {
        if summary.event_ids.contains(&event.common().id) {
            continue;
        }
        let line = serde_json::to_string(event).map_err(|_| "could not encode tracker event".to_string())?;
        append_line(path, Some(SCHEMA_HEADER), &line)
            .map_err(|error| format!("could not append to {}: {error}", path.display()))?;
        summary.observe(event);
        written += 1;
    }
    Ok(written)
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
            issue: IssueSnapshot {
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
            },
        }
    }

    #[test]
    fn missing_file_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let summary = read(&dir.path().join("tracker.jsonl")).unwrap();
        assert!(summary.newest_provider_event_at.is_none());
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
            Event::FullResync { common: common("f1", "COR", 23), issues: 1, removed: 1 },
        ];
        append_new(&path, &events, &mut summary).unwrap();
        std::fs::OpenOptions::new().append(true).open(&path).and_then(|mut f| {
            use std::io::Write;
            writeln!(f, "not json")
        }).unwrap();

        let summary = read(&path).unwrap();
        assert_eq!(
            summary.newest_provider_event_at,
            Some(Utc.with_ymd_and_hms(2026, 9, 1, 11, 0, 0).unwrap()),
            "removals and resync markers are local, not provider time"
        );
        assert_eq!(summary.event_count, 4);
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
