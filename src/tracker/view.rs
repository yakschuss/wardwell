//! Read model over one project's tracker mirror: folds `tracker.jsonl` into
//! the latest snapshot per issue key, plus when the mirror last pulled,
//! last ran a full resync, and last failed.
//!
//! Does NOT write the log, pull from a provider, or decide what a surface
//! shows; callers filter and format.

use crate::tracker::events::{Event, FailureCode, IssueSnapshot, StateCategory};
use crate::tracker::log::later;
use chrono::{DateTime, TimeDelta, Utc};
use std::collections::BTreeMap;
use std::path::Path;

/// The mirror's latest knowledge of one issue.
#[derive(Debug, Clone, PartialEq)]
pub struct MirroredIssue {
    /// External issue key, such as COR-12.
    pub key: String,
    /// Provider id the issue came from.
    pub provider: String,
    /// The provider's own id for the issue.
    pub external_id: String,
    /// The latest provider-neutral snapshot.
    pub issue: IssueSnapshot,
    /// Provider time of the snapshot held.
    pub updated_at: DateTime<Utc>,
    /// Set when a full resync no longer found the issue and it has not come back.
    pub removed_at: Option<DateTime<Utc>>,
}

impl MirroredIssue {
    /// Neither removed by a full resync nor archived in the tracker.
    pub fn is_open(&self) -> bool {
        self.removed_at.is_none() && self.issue.archived_at.is_none()
    }

    /// Completed or canceled in the tracker.
    pub fn is_done(&self) -> bool {
        matches!(self.issue.state_category, StateCategory::Completed | StateCategory::Canceled)
    }
}

/// Current issues and pull times of one tracker log.
#[derive(Debug, Clone, Default)]
pub struct MirrorView {
    /// Every issue the log has a snapshot for, by key, removed and archived included.
    pub issues: BTreeMap<String, MirroredIssue>,
    /// When the latest pull_completed or full_resync marker was written.
    pub last_pull_at: Option<DateTime<Utc>>,
    /// When the latest full resync marker was written.
    pub last_full_resync_at: Option<DateTime<Utc>>,
    /// The latest pull_failed marker, its time and code.
    pub last_failure: Option<(DateTime<Utc>, FailureCode)>,
    /// The latest pull_started marker, its time and process id.
    pub last_started: Option<(DateTime<Utc>, u32)>,
    /// The newest start or failure after the latest completed pull, in file
    /// order; None when a completed pull is the newest marker or none exists.
    pub last_attempt: Option<Attempt>,
}

/// A pull marker that a completed pull has not yet followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attempt {
    /// A pull started at this time in this process.
    Started { at: DateTime<Utc>, pid: u32 },
    /// A pull failed at this time with this code.
    Failed { at: DateTime<Utc>, code: FailureCode },
}

impl MirrorView {
    /// Fold `provider`'s events in the log at `path`. Another provider's
    /// rows, merged changes and markers alike, never reach the view.
    pub fn read_for(path: &Path, provider: &str) -> Result<Self, String> {
        Self::read_filtered(path, Some(provider))
    }

    /// Fold the log at `path`. A missing file is an empty mirror; unreadable
    /// lines are skipped.
    pub fn read(path: &Path) -> Result<Self, String> {
        Self::read_filtered(path, None)
    }

    fn read_filtered(path: &Path, provider: Option<&str>) -> Result<Self, String> {
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(_) => return Err(format!("could not read {}", path.display())),
        };
        let mut view = Self::default();
        content
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with("{\"_schema\""))
            .filter_map(|line| serde_json::from_str::<Event>(line).ok())
            .filter(|event| provider.is_none_or(|p| event.common().provider == p))
            .for_each(|event| view.observe(event));
        Ok(view)
    }

    /// Issues neither removed nor archived, by key.
    pub fn open(&self) -> impl Iterator<Item = &MirroredIssue> {
        self.issues.values().filter(|issue| issue.is_open())
    }

    /// The issue under `key`, matched without regard to case.
    pub fn get(&self, key: &str) -> Option<&MirroredIssue> {
        self.issues.get(key).or_else(|| self.issues.values().find(|i| i.key.eq_ignore_ascii_case(key)))
    }

    /// The code of a pull_failed marker written after the latest completed pull.
    pub fn failed_since_last_pull(&self) -> Option<FailureCode> {
        let (failed_at, code) = self.last_failure?;
        match self.last_pull_at {
            Some(pulled) if pulled >= failed_at => None,
            _ => Some(code),
        }
    }

    fn observe(&mut self, event: Event) {
        match event {
            Event::IssueUpserted { common, issue } => {
                let current = self.issues.get(&common.external_key);
                let newer = current.is_none_or(|c| c.removed_at.is_some() || common.occurred_at >= c.updated_at);
                if newer {
                    self.issues.insert(common.external_key.clone(), MirroredIssue {
                        key: common.external_key,
                        provider: common.provider,
                        external_id: common.external_id,
                        issue: *issue,
                        updated_at: common.occurred_at,
                        removed_at: None,
                    });
                }
            }
            Event::IssueRemoved { common } => {
                if let Some(issue) = self.issues.get_mut(&common.external_key) {
                    issue.removed_at = Some(common.occurred_at);
                }
            }
            Event::FullResync { common, .. } => {
                self.last_full_resync_at = later(self.last_full_resync_at, common.occurred_at);
                self.last_pull_at = Some(common.occurred_at);
                self.last_attempt = None;
            }
            Event::PullCompleted { common, .. } => {
                self.last_pull_at = Some(common.occurred_at);
                self.last_attempt = None;
            }
            Event::PullStarted { common, pid } => {
                self.last_started = Some((common.occurred_at, pid));
                self.last_attempt = Some(Attempt::Started { at: common.occurred_at, pid });
            }
            Event::PullFailed { common, code, .. } => {
                self.last_failure = Some((common.occurred_at, code));
                self.last_attempt = Some(Attempt::Failed { at: common.occurred_at, code });
            }
            _ => {}
        }
    }
}

/// A log body that interleaves both providers on 2026-09-30: a started
/// Linear issue COR-1 and Linear's completed pull at 11:00, then a GitHub
/// merged change at 11:30 and a failed GitHub pull at 11:45.
#[cfg(test)]
pub(crate) fn two_provider_log() -> String {
    [
        crate::tracker::events::SCHEMA_HEADER,
        r#"{"kind":"issue_upserted","id":"linear:issue:i1:1","provider":"linear","external_key":"COR-1","external_id":"i1","occurred_at":"2026-09-30T10:00:00Z","title":"COR-1 Claims inbox: In Progress","issue":{"issue_title":"Claims inbox","state":"In Progress","state_category":"started","priority":"none"}}"#,
        r#"{"kind":"pull_completed","id":"wardwell:pull_completed:COR:1","provider":"linear","external_key":"COR","external_id":"COR","actor":"wardwell","occurred_at":"2026-09-30T11:00:00Z","title":"COR pull from Linear completed","through":"2026-09-30T10:00:00Z"}"#,
        r#"{"kind":"change_merged","id":"github:acme/app#42","provider":"github","external_key":"acme/app#42","external_id":"PR_42","actor":"jdoe","occurred_at":"2026-09-30T11:30:00Z","title":"acme/app#42 merged into main: COR-1 Fix the claims inbox","change":{"number":42,"title":"COR-1 Fix the claims inbox","merged_at":"2026-09-30T11:30:00Z","base_branch":"main","keys":["COR-1"]}}"#,
        r#"{"kind":"pull_failed","id":"wardwell:pull_failed:acme/app:1","provider":"github","external_key":"acme/app","external_id":"acme/app","actor":"wardwell","occurred_at":"2026-09-30T11:45:00Z","title":"acme/app pull from GitHub failed: provider","code":"provider"}"#,
    ]
    .map(|line| format!("{line}\n"))
    .concat()
}

/// An age in plain words, such as `5 minutes` or `12 days`.
pub fn age_words(delta: TimeDelta) -> String {
    let minutes = delta.num_minutes().max(0);
    let (count, unit) = match minutes {
        0 => return "less than a minute".to_string(),
        m if m < 60 => (m, "minute"),
        m if m < 60 * 24 => (m / 60, "hour"),
        m => (m / (60 * 24), "day"),
    };
    match count {
        1 => format!("1 {unit}"),
        n => format!("{n} {unit}s"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::tracker::events::{Common, Priority, Relation, RelationKind, SCHEMA_HEADER};
    use chrono::TimeZone;

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 1, hour, 0, 0).unwrap()
    }

    fn common(id: &str, key: &str, hour: u32) -> Common {
        Common {
            id: id.into(),
            provider: "linear".into(),
            external_key: key.into(),
            external_id: format!("{key}-uuid"),
            actor: None,
            occurred_at: at(hour),
            title: format!("{key} title"),
            raw: serde_json::Value::Null,
        }
    }

    fn snapshot(id: &str, key: &str, hour: u32, state: &str, category: StateCategory) -> Event {
        Event::IssueUpserted {
            common: common(id, key, hour),
            issue: Box::new(IssueSnapshot {
                issue_title: format!("{key} work"),
                state: state.into(),
                state_category: category,
                ..Default::default()
            }),
        }
    }

    fn write_log(events: &[Event]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tracker.jsonl");
        let mut body = format!("{SCHEMA_HEADER}\n");
        for event in events {
            body.push_str(&serde_json::to_string(event).unwrap());
            body.push('\n');
        }
        body.push_str("not json\n");
        std::fs::write(&path, body).unwrap();
        (dir, path)
    }

    #[test]
    fn latest_snapshot_per_key_carries_every_field() {
        let structured = Event::IssueUpserted {
            common: common("s2", "COR-12", 10),
            issue: Box::new(IssueSnapshot {
                issue_title: "Claims inbox".into(),
                state: "In Progress".into(),
                state_category: StateCategory::Started,
                priority: Priority::High,
                assignee: Some("Jane Doe".into()),
                labels: vec!["billing".into()],
                url: Some("https://example.com/COR-12".into()),
                parent_key: Some("COR-5".into()),
                relations: vec![Relation { kind: RelationKind::BlockedBy, key: "COR-9".into() }],
                ..Default::default()
            }),
        };
        let (_dir, path) = write_log(&[
            snapshot("s1", "COR-12", 9, "Todo", StateCategory::Unstarted),
            structured,
            snapshot("old", "COR-12", 8, "Backlog", StateCategory::Backlog),
        ]);
        let view = MirrorView::read(&path).unwrap();
        let issue = view.get("cor-12").unwrap();
        assert_eq!(issue.issue.state, "In Progress", "an older snapshot later in the file does not win");
        assert_eq!(issue.issue.state_category, StateCategory::Started);
        assert_eq!(issue.issue.parent_key.as_deref(), Some("COR-5"));
        assert_eq!(issue.issue.relations[0].key, "COR-9");
        assert_eq!(issue.issue.url.as_deref(), Some("https://example.com/COR-12"));
        assert_eq!(issue.issue.labels, vec!["billing"]);
        assert_eq!(issue.issue.assignee.as_deref(), Some("Jane Doe"));
        assert_eq!(issue.issue.priority, Priority::High);
        assert_eq!(issue.updated_at, at(10));
        assert_eq!(issue.provider, "linear");
    }

    #[test]
    fn removed_and_archived_issues_leave_the_open_set() {
        let mut archived = snapshot("a1", "COR-3", 9, "Done", StateCategory::Completed);
        if let Event::IssueUpserted { issue, .. } = &mut archived {
            issue.archived_at = Some(at(9));
        }
        let (_dir, path) = write_log(&[
            snapshot("s1", "COR-1", 9, "Todo", StateCategory::Unstarted),
            snapshot("s2", "COR-2", 9, "Todo", StateCategory::Unstarted),
            archived,
            Event::IssueRemoved { common: common("r1", "COR-2", 12) },
        ]);
        let view = MirrorView::read(&path).unwrap();
        let open: Vec<&str> = view.open().map(|i| i.key.as_str()).collect();
        assert_eq!(open, vec!["COR-1"]);
        assert_eq!(view.get("COR-2").unwrap().removed_at, Some(at(12)), "get still finds a removed issue");
        assert!(view.get("COR-3").unwrap().issue.archived_at.is_some());
    }

    #[test]
    fn a_restored_issue_is_open_again_even_with_an_older_provider_time() {
        let mut restored = snapshot("s1:restored:x", "COR-1", 9, "In Progress", StateCategory::Started);
        if let Event::IssueUpserted { common, .. } = &mut restored {
            common.id = "s1:restored:2026-09-01T12:00:00+00:00".into();
        }
        let (_dir, path) = write_log(&[
            snapshot("s1", "COR-1", 9, "Todo", StateCategory::Unstarted),
            Event::IssueRemoved { common: common("r1", "COR-1", 12) },
            restored,
        ]);
        let view = MirrorView::read(&path).unwrap();
        let issue = view.get("COR-1").unwrap();
        assert!(issue.is_open());
        assert_eq!(issue.issue.state, "In Progress");
    }

    #[test]
    fn pull_times_and_a_pull_failed_tail() {
        let (_dir, path) = write_log(&[
            Event::FullResync { common: common("f1", "COR", 8), issues: 1, removed: 0, through: Some(at(7)) },
            snapshot("s1", "COR-1", 9, "Todo", StateCategory::Unstarted),
            Event::PullCompleted { common: common("p1", "COR", 10), through: Some(at(9)) },
            Event::PullFailed { common: common("x1", "COR", 11), code: FailureCode::Auth, automatic_full: false },
        ]);
        let view = MirrorView::read(&path).unwrap();
        assert_eq!(view.last_pull_at, Some(at(10)));
        assert_eq!(view.last_full_resync_at, Some(at(8)));
        assert_eq!(view.last_failure, Some((at(11), FailureCode::Auth)));
        assert_eq!(view.failed_since_last_pull(), Some(FailureCode::Auth));
    }

    #[test]
    fn the_last_attempt_is_the_newest_start_or_failure_after_the_last_completed_pull() {
        let started = |id: &str, hour, pid| Event::PullStarted { common: common(id, "COR", hour), pid };
        let (_dir, path) = write_log(&[started("s1", 9, 11), Event::PullCompleted { common: common("p1", "COR", 9), through: None }]);
        let view = MirrorView::read(&path).unwrap();
        assert_eq!(view.last_attempt, None);
        assert_eq!(view.last_started, Some((at(9), 11)));

        let (_dir, path) = write_log(&[
            Event::PullCompleted { common: common("p1", "COR", 9), through: None },
            started("s2", 10, 22),
        ]);
        assert_eq!(MirrorView::read(&path).unwrap().last_attempt, Some(Attempt::Started { at: at(10), pid: 22 }));

        let (_dir, path) = write_log(&[
            started("s3", 10, 33),
            Event::PullFailed { common: common("x1", "COR", 10), code: FailureCode::Timeout, automatic_full: false },
        ]);
        assert_eq!(MirrorView::read(&path).unwrap().last_attempt, Some(Attempt::Failed { at: at(10), code: FailureCode::Timeout }));
    }

    #[test]
    fn a_completed_pull_after_a_failure_clears_it() {
        let (_dir, path) = write_log(&[
            Event::PullFailed { common: common("x1", "COR", 9), code: FailureCode::Provider, automatic_full: false },
            Event::PullCompleted { common: common("p1", "COR", 10), through: None },
        ]);
        assert_eq!(MirrorView::read(&path).unwrap().failed_since_last_pull(), None);
    }

    #[test]
    fn a_provider_view_ignores_the_other_providers_changes_and_markers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tracker.jsonl");
        std::fs::write(&path, two_provider_log()).unwrap();
        let linear = MirrorView::read_for(&path, "linear").unwrap();
        assert_eq!(linear.issues.keys().collect::<Vec<_>>(), vec!["COR-1"]);
        assert_eq!(linear.last_pull_at, Some(Utc.with_ymd_and_hms(2026, 9, 30, 11, 0, 0).unwrap()));
        assert_eq!(linear.failed_since_last_pull(), None, "the github failure is not linear's");
        let github = MirrorView::read_for(&path, "github").unwrap();
        assert!(github.issues.is_empty(), "a merged change is not an issue");
        assert_eq!(github.failed_since_last_pull(), Some(FailureCode::Provider));
    }

    #[test]
    fn a_missing_log_is_an_empty_mirror() {
        let dir = tempfile::tempdir().unwrap();
        let view = MirrorView::read(&dir.path().join("tracker.jsonl")).unwrap();
        assert!(view.issues.is_empty());
        assert!(view.last_pull_at.is_none());
        assert_eq!(view.failed_since_last_pull(), None);
    }

    #[test]
    fn ages_read_as_plain_words() {
        assert_eq!(age_words(TimeDelta::seconds(20)), "less than a minute");
        assert_eq!(age_words(TimeDelta::minutes(1)), "1 minute");
        assert_eq!(age_words(TimeDelta::minutes(59)), "59 minutes");
        assert_eq!(age_words(TimeDelta::hours(3)), "3 hours");
        assert_eq!(age_words(TimeDelta::hours(25)), "1 day");
        assert_eq!(age_words(TimeDelta::days(12)), "12 days");
        assert_eq!(age_words(TimeDelta::minutes(-5)), "less than a minute");
    }
}
