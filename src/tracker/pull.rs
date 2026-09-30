//! Runs one tracker pull for a bound project: resolve the credential,
//! derive the cursor from the log, call the adapter, append new events,
//! and once every page arrived record a cursor marker (pull_completed, or
//! on a full resync the removals and a full_resync marker).
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

/// How far before the last completed pull's `through` an incremental pull starts.
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
        false => summary.cursor.map(|t| t - CURSOR_OVERLAP),
    };
    let mut through = summary.cursor;
    let mut appended = 0;
    let mut returned: HashSet<String> = HashSet::new();
    // Each page is appended as it arrives (event ids make re-pulls
    // idempotent), so a failure keeps the pages already read. The cursor
    // moves only through the marker below, so a failure leaves it alone
    // whatever order the provider delivers pages in.
    adapter.pull(since, full, &mut |page| {
        returned.extend(upserted_keys(&page));
        through = page.iter().fold(through, |t, e| log::later(t, e.common().occurred_at));
        appended += log::append_new(&path, &page, &mut summary)?;
        Ok(())
    })?;
    // Removals are only knowable after every page arrived.
    let markers = match full {
        true => resync_markers(binding, &returned, &summary, now, through),
        false => vec![pull_completed(binding, now, through)],
    };
    let removed = markers.len().saturating_sub(1);
    log::append_new(&path, &markers, &mut summary)?;
    appended += removed;
    Ok(PullOutcome { appended, removed, full })
}

/// Cursor marker for an incremental pull that delivered every page.
fn pull_completed(binding: &TrackerBinding, now: DateTime<Utc>, through: Option<DateTime<Utc>>) -> Event {
    let label = provider_label(&binding.provider);
    let upto = through.map_or("the beginning".to_string(), |t| t.to_rfc3339());
    Event::PullCompleted {
        common: local_common(
            binding,
            format!("wardwell:pull_completed:{}:{}", binding.team, now.to_rfc3339()),
            &binding.team,
            &binding.team,
            now,
            format!("{} pull from {label} completed through {upto}", binding.team),
        ),
        through,
    }
}

/// `issue_removed` for each open key the full result no longer contains,
/// followed by one `full_resync` marker with the counts.
fn resync_markers(
    binding: &TrackerBinding,
    returned: &HashSet<String>,
    summary: &log::LogSummary,
    now: DateTime<Utc>,
    through: Option<DateTime<Utc>>,
) -> Vec<Event> {
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
        through,
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

fn upserted_keys(events: &[Event]) -> impl Iterator<Item = String> + '_ {
    events
        .iter()
        .filter(|e| matches!(e, Event::IssueUpserted { .. }))
        .map(|e| e.common().external_key.clone())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::tracker::adapter::Sink;
    use crate::tracker::events::{Common, IssueSnapshot, Priority, StateCategory};
    use chrono::TimeZone;
    use std::cell::RefCell;

    /// Delivers `pages` in order, then fails if `fail_after` is set.
    struct FakeAdapter {
        pages: Vec<Vec<Event>>,
        fail_after: Option<&'static str>,
        calls: RefCell<Vec<(Option<DateTime<Utc>>, bool)>>,
    }

    impl Adapter for FakeAdapter {
        fn pull(&self, since: Option<DateTime<Utc>>, full: bool, sink: &mut Sink<'_>) -> Result<(), String> {
            self.calls.borrow_mut().push((since, full));
            for page in &self.pages {
                sink(page.clone())?;
            }
            self.fail_after.map_or(Ok(()), |error| Err(error.to_string()))
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
        FakeAdapter { pages: vec![events], fail_after: None, calls: RefCell::new(vec![]) }
    }

    #[test]
    fn failed_full_pull_keeps_earlier_pages_and_removes_nothing() {
        let vault = tempfile::tempdir().unwrap();
        let first = fake(vec![snapshot("COR-1", 9), snapshot("COR-2", 10)]);
        pull_project(vault.path(), &binding(), &first, false, at(12)).unwrap();

        let broken = FakeAdapter {
            pages: vec![vec![snapshot("COR-2", 11)]],
            fail_after: Some("Linear request failed"),
            calls: RefCell::new(vec![]),
        };
        let error = pull_project(vault.path(), &binding(), &broken, true, at(14)).unwrap_err();
        assert_eq!(error, "Linear request failed");

        let path = log::path_for(vault.path(), "work", "claims");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("linear:issue:COR-2:11"), "page 1 is on disk: {content}");
        assert!(!content.contains("issue_removed"), "{content}");
        assert!(!content.contains("full_resync"), "{content}");
        let summary = log::read(&path).unwrap();
        assert!(summary.open_issues.contains_key("COR-1"));
        assert_eq!(summary.last_full_resync_at, None);
    }

    #[test]
    fn first_pull_starts_from_the_beginning_and_appends() {
        let vault = tempfile::tempdir().unwrap();
        let adapter = fake(vec![snapshot("COR-1", 9), snapshot("COR-2", 10)]);
        let outcome = pull_project(vault.path(), &binding(), &adapter, false, at(12)).unwrap();
        assert_eq!(outcome.appended, 2);
        assert_eq!(adapter.calls.borrow()[0], (None, false));
        let path = log::path_for(vault.path(), "work", "claims");
        assert_eq!(log::read(&path).unwrap().event_count, 3, "two snapshots and the pull_completed marker");
    }

    #[test]
    fn next_pull_uses_the_completed_pull_marker_minus_overlap_and_dedups() {
        let vault = tempfile::tempdir().unwrap();
        let adapter = fake(vec![snapshot("COR-1", 9), snapshot("COR-2", 10)]);
        pull_project(vault.path(), &binding(), &adapter, false, at(12)).unwrap();
        let outcome = pull_project(vault.path(), &binding(), &adapter, false, at(13)).unwrap();
        assert_eq!(outcome.appended, 0, "overlap re-pull is deduplicated");
        assert_eq!(adapter.calls.borrow()[1], (Some(at(10) - CURSOR_OVERLAP), false));
    }

    #[test]
    fn completed_pull_records_a_marker_through_the_newest_event_it_saw() {
        let vault = tempfile::tempdir().unwrap();
        // Newest first, the way a provider may order by update time.
        let adapter = FakeAdapter {
            pages: vec![vec![snapshot("COR-2", 10)], vec![snapshot("COR-1", 9)]],
            fail_after: None,
            calls: RefCell::new(vec![]),
        };
        pull_project(vault.path(), &binding(), &adapter, false, at(12)).unwrap();
        let path = log::path_for(vault.path(), "work", "claims");
        let content = std::fs::read_to_string(&path).unwrap();
        let last: Event = serde_json::from_str(content.lines().last().unwrap()).unwrap();
        let Event::PullCompleted { common, through } = last else { panic!("{content}") };
        assert_eq!(through, Some(at(10)));
        assert_eq!(common.occurred_at, at(12));
        assert_eq!(common.actor.as_deref(), Some("wardwell"));
        let summary = log::read(&path).unwrap();
        assert_eq!(summary.cursor, Some(at(10)));
        assert_eq!(summary.last_pull_at, Some(at(12)));
    }

    #[test]
    fn pull_that_sees_nothing_keeps_the_previous_cursor() {
        let vault = tempfile::tempdir().unwrap();
        pull_project(vault.path(), &binding(), &fake(vec![snapshot("COR-1", 9)]), false, at(12)).unwrap();
        let empty = fake(vec![]);
        pull_project(vault.path(), &binding(), &empty, false, at(13)).unwrap();
        let summary = log::read(&log::path_for(vault.path(), "work", "claims")).unwrap();
        assert_eq!(summary.cursor, Some(at(9)));
        assert_eq!(summary.last_pull_at, Some(at(13)));
    }

    #[test]
    fn failed_pull_keeps_its_pages_but_does_not_move_the_cursor() {
        let vault = tempfile::tempdir().unwrap();
        pull_project(vault.path(), &binding(), &fake(vec![snapshot("COR-1", 9)]), false, at(12)).unwrap();

        // Page 1 (newest) arrives, page 2 fails: the older updates were never read.
        let broken = FakeAdapter {
            pages: vec![vec![snapshot("COR-3", 20)]],
            fail_after: Some("Linear request failed"),
            calls: RefCell::new(vec![]),
        };
        pull_project(vault.path(), &binding(), &broken, false, at(21)).unwrap_err();
        let path = log::path_for(vault.path(), "work", "claims");
        let summary = log::read(&path).unwrap();
        assert!(summary.event_ids.contains("linear:issue:COR-3:20"), "page 1 is kept");
        assert_eq!(summary.cursor, Some(at(9)), "a failed pull does not move the cursor");
        assert_eq!(summary.last_pull_at, Some(at(12)));

        let retry = fake(vec![snapshot("COR-3", 20), snapshot("COR-2", 15)]);
        let outcome = pull_project(vault.path(), &binding(), &retry, false, at(22)).unwrap();
        assert_eq!(retry.calls.borrow()[0], (Some(at(9) - CURSOR_OVERLAP), false));
        assert_eq!(outcome.appended, 1, "the kept page is deduplicated");
        assert_eq!(log::read(&path).unwrap().cursor, Some(at(20)));
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
        let Some(Event::IssueRemoved { common }) = events.get(3) else { panic!("{content}") };
        assert_eq!(common.external_key, "COR-1");
        assert_eq!(common.title, "COR-1 COR-1 work: removed from Linear");
        let Some(Event::FullResync { common, issues, removed, through }) = events.last() else { panic!("{content}") };
        assert_eq!((*issues, *removed, *through), (1, 1, Some(at(10))));
        assert_eq!(common.occurred_at, at(14));
        assert!(common.title.contains("full resync"), "{}", common.title);

        let summary = log::read(&path).unwrap();
        assert_eq!(summary.last_full_resync_at, Some(at(14)));
        assert_eq!(summary.last_pull_at, Some(at(14)));
        assert_eq!(summary.cursor, Some(at(10)), "full_resync sets the cursor to the newest event it saw");

        let next = fake(vec![]);
        pull_project(vault.path(), &binding(), &next, false, at(15)).unwrap();
        assert_eq!(next.calls.borrow()[0], (Some(at(10) - CURSOR_OVERLAP), false));
    }

    #[test]
    fn empty_first_pull_creates_the_log_with_a_marker_and_no_cursor() {
        let vault = tempfile::tempdir().unwrap();
        let outcome = pull_project(vault.path(), &binding(), &fake(vec![]), false, at(12)).unwrap();
        assert_eq!(outcome.appended, 0);
        let path = log::path_for(vault.path(), "work", "claims");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.starts_with(&format!("{}\n", crate::tracker::events::SCHEMA_HEADER)), "{content}");
        let summary = log::read(&path).unwrap();
        assert_eq!(summary.last_pull_at, Some(at(12)));
        assert_eq!(summary.cursor, None, "nothing seen yet: the next pull starts from the beginning");
        let next = fake(vec![]);
        pull_project(vault.path(), &binding(), &next, false, at(13)).unwrap();
        assert_eq!(next.calls.borrow()[0], (None, false));
    }

    #[cfg(unix)]
    #[test]
    fn unwritable_marker_fails_the_pull() {
        use std::os::unix::fs::PermissionsExt;
        let vault = tempfile::tempdir().unwrap();
        pull_project(vault.path(), &binding(), &fake(vec![]), false, at(12)).unwrap();
        let path = log::path_for(vault.path(), "work", "claims");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        let result = pull_project(vault.path(), &binding(), &fake(vec![]), false, at(13));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = result.unwrap_err();
        assert!(error.contains("could not append"), "{error}");
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
