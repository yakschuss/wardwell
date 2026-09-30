//! Runs one tracker pull for a bound project: resolve the credential,
//! derive the cursor from the log, call the adapter, append new events,
//! and on a full resync record removals and a resync marker.
//!
//! Does NOT schedule pulls (launchd does) or write to the provider.

use crate::config::loader::TrackerBinding;
use crate::tracker::adapter::Adapter;
use crate::tracker::credential::{self, Credential};
use crate::tracker::events::{Common, Event};
use crate::tracker::linear::{HttpTransport, Linear};
use crate::tracker::{log, provider_label};
use chrono::{DateTime, TimeDelta, Utc};
use std::collections::HashSet;
use std::path::Path;

/// How far before the newest mirrored event an incremental pull starts.
/// Covers clock skew and updates that land while a pull is paging.
pub const CURSOR_OVERLAP: TimeDelta = TimeDelta::hours(1);

/// Result of pulling one project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullOutcome {
    pub appended: usize,
    pub removed: usize,
    pub full: bool,
}

/// Builds the adapter for a binding. Injected so tests never reach a network.
pub type Connect<'a> = dyn Fn(&TrackerBinding, &Credential) -> Result<Box<dyn Adapter>, String> + 'a;

/// The production adapter for a binding's provider.
pub fn connect_provider(binding: &TrackerBinding, credential: &Credential) -> Result<Box<dyn Adapter>, String> {
    match binding.provider.as_str() {
        "linear" => Ok(Box::new(Linear::new(
            HttpTransport::new(credential.token().to_string()),
            &binding.team,
        ))),
        other => Err(format!("unsupported tracker provider '{other}'")),
    }
}

/// Load the binding's credential, then pull. A missing or unreadable
/// credential fails before the log is read or written.
pub fn pull_binding(
    vault_root: &Path,
    config_dir: &Path,
    binding: &TrackerBinding,
    full: bool,
    now: DateTime<Utc>,
    connect: &Connect<'_>,
) -> Result<PullOutcome, String> {
    let credential = credential::load(&credential::path_in(config_dir, &binding.credential)?)?;
    let adapter = connect(binding, &credential)?;
    pull_project(vault_root, binding, adapter.as_ref(), full, now)
}

/// Pull one project through `adapter` and append what is new.
pub fn pull_project(
    vault_root: &Path,
    binding: &TrackerBinding,
    adapter: &dyn Adapter,
    full: bool,
    now: DateTime<Utc>,
) -> Result<PullOutcome, String> {
    let path = log::path_for(vault_root, &binding.domain, &binding.project);
    let mut summary = log::read(&path)?;
    let since = match full {
        true => None,
        false => summary.newest_provider_event_at.map(|t| t - CURSOR_OVERLAP),
    };
    let events = adapter.pull(since, full)?;
    let mut appended = log::append_new(&path, &events, &mut summary)?;
    let mut removed = 0;
    if full {
        let markers = resync_markers(binding, &events, &summary, now);
        removed = markers.len().saturating_sub(1);
        appended += log::append_new(&path, &markers, &mut summary)?;
    }
    mark_pulled(&path, now);
    Ok(PullOutcome { appended, removed, full })
}

/// `issue_removed` for each open key the full result no longer contains,
/// followed by one `full_resync` marker with the counts.
fn resync_markers(binding: &TrackerBinding, events: &[Event], summary: &log::LogSummary, now: DateTime<Utc>) -> Vec<Event> {
    let returned: HashSet<&str> = events
        .iter()
        .filter(|e| matches!(e, Event::IssueUpserted { .. }))
        .map(|e| e.common().external_key.as_str())
        .collect();
    let label = provider_label(&binding.provider);
    let stamp = now.to_rfc3339();
    let mut markers: Vec<Event> = summary
        .open_issues
        .iter()
        .filter(|(key, _)| !returned.contains(key.as_str()))
        .map(|(key, open)| Event::IssueRemoved {
            common: local_common(
                binding,
                format!("wardwell:issue_removed:{}:{stamp}", open.external_id),
                key,
                &open.external_id,
                now,
                format!("{key} {}: removed from {label}", open.issue_title),
            ),
        })
        .collect();
    let removed = markers.len();
    markers.push(Event::FullResync {
        common: local_common(
            binding,
            format!("wardwell:full_resync:{}:{stamp}", binding.team),
            &binding.team,
            &binding.team,
            now,
            format!("{} full resync from {label}: {} issues, {removed} removed", binding.team, returned.len()),
        ),
        issues: returned.len(),
        removed,
    });
    markers
}

fn local_common(binding: &TrackerBinding, id: String, key: &str, external_id: &str, now: DateTime<Utc>, title: String) -> Common {
    Common {
        id,
        provider: binding.provider.clone(),
        external_key: key.to_string(),
        external_id: external_id.to_string(),
        actor: Some("wardwell".to_string()),
        occurred_at: now,
        title,
        raw: serde_json::Value::Null,
    }
}

/// Stamp the log's modification time so `status` can report the last pull
/// even when it found nothing new. Content is untouched.
fn mark_pulled(path: &Path, now: DateTime<Utc>) {
    if let Ok(file) = std::fs::File::options().append(true).open(path) {
        let _ = file.set_modified(now.into());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::tracker::events::{Common, IssueSnapshot, Priority, StateCategory};
    use chrono::TimeZone;
    use std::cell::RefCell;

    struct FakeAdapter {
        events: Vec<Event>,
        calls: RefCell<Vec<(Option<DateTime<Utc>>, bool)>>,
    }

    impl Adapter for FakeAdapter {
        fn pull(&self, since: Option<DateTime<Utc>>, full: bool) -> Result<Vec<Event>, String> {
            self.calls.borrow_mut().push((since, full));
            Ok(self.events.clone())
        }
    }

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 1, hour, 0, 0).unwrap()
    }

    fn snapshot(key: &str, hour: u32) -> Event {
        Event::IssueUpserted {
            common: Common {
                id: format!("linear:issue:{key}:{hour}"),
                provider: "linear".into(),
                external_key: key.into(),
                external_id: format!("{key}-id"),
                actor: None,
                occurred_at: at(hour),
                title: format!("{key} Title: Todo"),
                raw: serde_json::Value::Null,
            },
            issue: IssueSnapshot {
                issue_title: format!("{key} work"),
                description: None,
                state: "Todo".into(),
                state_category: StateCategory::Unstarted,
                priority: Priority::None,
                team: Some("COR".into()),
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

    fn binding() -> TrackerBinding {
        TrackerBinding {
            domain: "work".into(),
            project: "claims".into(),
            provider: "linear".into(),
            team: "COR".into(),
            credential: "corr-linear".into(),
            readonly: true,
        }
    }

    fn fake(events: Vec<Event>) -> FakeAdapter {
        FakeAdapter { events, calls: RefCell::new(vec![]) }
    }

    #[test]
    fn first_pull_starts_from_the_beginning_and_appends() {
        let vault = tempfile::tempdir().unwrap();
        let adapter = fake(vec![snapshot("COR-1", 9), snapshot("COR-2", 10)]);
        let outcome = pull_project(vault.path(), &binding(), &adapter, false, at(12)).unwrap();
        assert_eq!(outcome.appended, 2);
        assert_eq!(adapter.calls.borrow()[0], (None, false));
        let path = log::path_for(vault.path(), "work", "claims");
        assert_eq!(log::read(&path).unwrap().event_count, 2);
    }

    #[test]
    fn next_pull_uses_newest_event_minus_overlap_and_dedups() {
        let vault = tempfile::tempdir().unwrap();
        let adapter = fake(vec![snapshot("COR-1", 9), snapshot("COR-2", 10)]);
        pull_project(vault.path(), &binding(), &adapter, false, at(12)).unwrap();
        let outcome = pull_project(vault.path(), &binding(), &adapter, false, at(13)).unwrap();
        assert_eq!(outcome.appended, 0, "overlap re-pull is deduplicated");
        assert_eq!(adapter.calls.borrow()[1], (Some(at(10) - CURSOR_OVERLAP), false));
    }

    #[test]
    fn full_pull_removes_missing_keys_and_records_resync() {
        let vault = tempfile::tempdir().unwrap();
        let first = fake(vec![snapshot("COR-1", 9), snapshot("COR-2", 10)]);
        pull_project(vault.path(), &binding(), &first, false, at(12)).unwrap();

        let full = fake(vec![snapshot("COR-2", 10)]);
        let outcome = pull_project(vault.path(), &binding(), &full, true, at(14)).unwrap();
        assert_eq!(full.calls.borrow()[0], (None, true));
        assert_eq!(outcome.removed, 1);

        let path = log::path_for(vault.path(), "work", "claims");
        let content = std::fs::read_to_string(&path).unwrap();
        let events: Vec<Event> = content.lines().skip(1).map(|l| serde_json::from_str(l).unwrap()).collect();
        let Some(Event::IssueRemoved { common }) = events.get(2) else { panic!("{content}") };
        assert_eq!(common.external_key, "COR-1");
        assert_eq!(common.title, "COR-1 COR-1 work: removed from Linear");
        let Some(Event::FullResync { common, issues, removed }) = events.last() else { panic!("{content}") };
        assert_eq!((*issues, *removed), (1, 1));
        assert_eq!(common.occurred_at, at(14));
        assert!(common.title.contains("full resync"), "{}", common.title);

        let summary = log::read(&path).unwrap();
        assert_eq!(summary.last_full_resync_at, Some(at(14)));
        assert_eq!(summary.newest_provider_event_at, Some(at(10)), "local markers do not move the cursor");
    }

    #[test]
    fn missing_credential_is_a_clean_error_and_leaves_the_file_untouched() {
        let vault = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let path = log::path_for(vault.path(), "work", "claims");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{\"_schema\":\"tracker\",\"_version\":\"1.0\"}\n").unwrap();
        let before = std::fs::read(&path).unwrap();

        let called = RefCell::new(false);
        let connect = |_: &TrackerBinding, _: &Credential| -> Result<Box<dyn Adapter>, String> {
            *called.borrow_mut() = true;
            Ok(Box::new(fake(vec![snapshot("COR-1", 9)])))
        };
        let error = pull_binding(vault.path(), config_dir.path(), &binding(), false, at(12), &connect).unwrap_err();
        assert!(error.contains("not configured"), "{error}");
        assert!(!*called.borrow(), "no adapter without a credential");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn unsupported_provider_is_rejected() {
        let mut b = binding();
        b.provider = "jira".into();
        let credential_dir = tempfile::tempdir().unwrap();
        let path = crate::tracker::credential::path_in(credential_dir.path(), "x").unwrap();
        crate::tracker::credential::save(&path, "t").unwrap();
        let credential = crate::tracker::credential::load(&path).unwrap();
        let error = connect_provider(&b, &credential).err().unwrap();
        assert!(error.contains("jira"), "{error}");
    }
}
