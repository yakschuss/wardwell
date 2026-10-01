//! Runs one tracker pull for a bound project: resolve the credential,
//! derive the cursor from the log, record a pull_started marker, call the
//! adapter, append new events, and once every page arrived record a cursor
//! marker (pull_completed, or on a full resync the removals and a
//! full_resync marker).
//!
//! Does NOT decide when a pull runs (trigger.rs does) or write to the provider.

use crate::config::loader::TrackerBinding;
use crate::tracker::adapter::Adapter;
use crate::tracker::credential::{self, Credential};
use crate::tracker::events::{Common, Event, FailureCode};
use crate::tracker::github::{GhRunner, GitHub, HttpRest, Rest, SystemGh};
use crate::tracker::linear::{HttpTransport, Linear};
use crate::tracker::{GITHUB, lock, log, provider_label, state};
use chrono::{DateTime, TimeDelta, Utc};
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

/// How far before the last completed pull's `through` an incremental pull starts.
/// Covers clock skew and updates that land while a pull is paging.
pub const CURSOR_OVERLAP: TimeDelta = TimeDelta::hours(1);

/// A pull runs full when the newest full resync is older than this, or
/// absent. Linear does not timestamp every change (a new relation, an
/// archive of an old issue), so an incremental pull alone can miss them.
pub const FULL_RESYNC_MAX_AGE: TimeDelta = TimeDelta::hours(24);

/// After an automatic full pull fails, no automatic full is tried again for
/// this long; incremental pulls carry on meanwhile. An explicit `--full` is
/// never held back.
pub const AUTOMATIC_FULL_RETRY_AFTER: TimeDelta = TimeDelta::hours(6);

/// What a pull was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// From the cursor; runs full when a full resync is due.
    Incremental,
    /// From the cursor, never escalating to a full pull. The kanban on-miss
    /// refresh uses it so a lookup never waits on a full resync.
    IncrementalOnly,
    /// Every issue; records removals. Fails on an empty result while the
    /// mirror holds open issues.
    Full,
    /// `Full` that accepts an empty result and removes every open issue.
    /// Only a person asks for this; an automatic full pull never does.
    FullAllowEmpty,
    /// `Full` started by `pull_binding` because a full resync was due. Its
    /// failure marker holds back the next automatic full.
    AutomaticFull,
}

impl Mode {
    fn is_full(self) -> bool {
        !matches!(self, Mode::Incremental | Mode::IncrementalOnly)
    }
}

/// Why a pull that was not asked to be full ran full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResyncDue {
    /// The log has no full_resync marker.
    NeverRan,
    /// The newest full_resync marker is older than `FULL_RESYNC_MAX_AGE`.
    Stale,
}

impl ResyncDue {
    /// The reason as the pull output line prints it.
    pub fn describe(self) -> String {
        match self {
            Self::NeverRan => "no full resync on record".to_string(),
            Self::Stale => format!("last full resync over {} hours ago", FULL_RESYNC_MAX_AGE.num_hours()),
        }
    }

    /// Whether a pull at `now` must run full, given the log: a full resync
    /// is missing or stale, and no automatic full failed within
    /// `AUTOMATIC_FULL_RETRY_AFTER`.
    pub fn check(summary: &log::LogSummary, now: DateTime<Utc>) -> Option<Self> {
        if summary.last_automatic_full_failure.is_some_and(|at| now - at < AUTOMATIC_FULL_RETRY_AFTER) {
            return None;
        }
        match summary.last_full_resync_at {
            None => Some(Self::NeverRan),
            Some(at) if now - at > FULL_RESYNC_MAX_AGE => Some(Self::Stale),
            Some(_) => None,
        }
    }
}

/// Result of pulling one project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullOutcome {
    pub appended: usize,
    pub removed: usize,
    pub full: bool,
    /// Set when a full resync was due, so a full pull was attempted.
    pub resync_due: Option<ResyncDue>,
    /// Why the due full pull failed; the outcome is then the incremental
    /// pull that ran after it.
    pub failed_full: Option<PullError>,
    /// True when a pull that completed after this one began waiting made it
    /// unneeded, so it read nothing from the provider.
    pub skipped: bool,
}

/// Builds the adapter for a binding from its credential, None when the
/// binding's provider can read without one and none is stored. Injected so
/// tests never reach a network or start a process.
pub type Connect<'a> = dyn Fn(&TrackerBinding, Option<&Credential>) -> Result<Box<dyn Adapter>, String> + 'a;

/// The production adapter for a binding's provider.
pub fn connect_provider(binding: &TrackerBinding, credential: Option<&Credential>) -> Result<Box<dyn Adapter>, String> {
    match (binding.provider.as_str(), credential) {
        ("linear", Some(credential)) => Ok(Box::new(Linear::new(
            HttpTransport::new(credential.token().to_string()),
            &binding.team,
        ))),
        ("linear", None) => Err("a linear binding needs its credential".to_string()),
        (GITHUB, credential) => Ok(Box::new(github_for(binding, credential, Box::new(SystemGh::located())))),
        (other, _) => Err(format!("unsupported tracker provider '{other}'")),
    }
}

/// The GitHub adapter for a binding: it reads through `gh`, and through the REST API
/// when a token is stored.
pub fn github_for(binding: &TrackerBinding, credential: Option<&Credential>, gh: Box<dyn GhRunner>) -> GitHub {
    let rest = credential.map(|c| Box::new(HttpRest::new(c.token().to_string())) as Box<dyn Rest>);
    GitHub::new(binding.scope(), &binding.credential, gh, rest)
}

/// The binding's credential. An issue tracker cannot pull without one; a
/// provider that can read through another route gets None when none is
/// stored. A stored credential that fails its checks fails either way.
pub fn load_credential(config_dir: &Path, binding: &TrackerBinding) -> Result<Option<Credential>, PullError> {
    let failed = |message| PullError::new(FailureCode::Credential, message);
    let path = credential::path_in(config_dir, &binding.credential).map_err(failed)?;
    let optional = !crate::tracker::mirrors_issues(&binding.provider);
    match (optional, std::fs::symlink_metadata(&path).is_err()) {
        (true, true) => Ok(None),
        _ => credential::load(&path).map(Some).map_err(failed),
    }
}

/// Why a pull stopped: a closed code for the log and the status line, and a
/// message for the person running it. Neither carries a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullError {
    pub code: FailureCode,
    pub message: String,
}

impl PullError {
    fn new(code: FailureCode, message: String) -> Self {
        Self { code, message }
    }
}

impl std::fmt::Display for PullError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code.as_str())
    }
}

/// Load the binding's credential, then pull. A missing or unreadable
/// credential fails before the log is read or written. The local refresh
/// state in `config_dir` records the start, the completion, and any failure
/// but a held lock.
pub fn pull_binding(
    vault_root: &Path,
    config_dir: &Path,
    binding: &TrackerBinding,
    mode: Mode,
    now: DateTime<Utc>,
    connect: &Connect<'_>,
) -> Result<PullOutcome, PullError> {
    let state = state::path(config_dir, &binding.domain, &binding.project);
    let recording = Recording::starting(&state, now);
    let result = pull_binding_recorded(vault_root, config_dir, binding, mode, now, connect, &recording);
    if let Err(error) = &result
        && error.code != FailureCode::LockBusy
    {
        let _ = state::record(&state, &binding.provider, state::Record::Failed(error.code), recording.clock());
    }
    result
}

fn pull_binding_recorded(
    vault_root: &Path,
    config_dir: &Path,
    binding: &TrackerBinding,
    mode: Mode,
    now: DateTime<Utc>,
    connect: &Connect<'_>,
    recording: &Recording<'_>,
) -> Result<PullOutcome, PullError> {
    let wait = lock::DEFAULT_WAIT;
    let credential = load_credential(config_dir, binding)?;
    let adapter = connect(binding, credential.as_ref()).map_err(|message| PullError::new(FailureCode::UnsupportedProvider, message))?;
    // Merged changes are never removed, so their mirror needs no resync.
    let resync_due = match (mode, crate::tracker::mirrors_issues(&binding.provider)) {
        (Mode::Incremental, true) => resync_due(vault_root, binding, now)?,
        _ => None,
    };
    let Some(due) = resync_due else {
        return pull_recorded(vault_root, binding, adapter.as_ref(), mode, now, wait, Some(recording));
    };
    let pull = |mode| pull_recorded(vault_root, binding, adapter.as_ref(), mode, now, wait, Some(recording));
    match pull(Mode::AutomaticFull) {
        Ok(outcome) => Ok(PullOutcome { resync_due: Some(due), ..outcome }),
        // A failed automatic full must not stop the mirror moving.
        Err(failed) => match pull(Mode::Incremental) {
            Ok(outcome) => Ok(PullOutcome { resync_due: Some(due), failed_full: Some(failed), ..outcome }),
            Err(error) => Err(PullError::new(error.code, format!("automatic full pull failed: {failed}; incremental pull: {}", error.message))),
        },
    }
}

/// Whether the binding's log is due a full resync at `now`.
fn resync_due(vault_root: &Path, binding: &TrackerBinding, now: DateTime<Utc>) -> Result<Option<ResyncDue>, PullError> {
    let path = log::path_for(vault_root, &binding.domain, &binding.project);
    let summary = log::read_for(&path, &binding.provider).map_err(|message| PullError::new(FailureCode::LogRead, message))?;
    Ok(ResyncDue::check(&summary, now))
}

/// Pull one project through `adapter` and append what is new.
pub fn pull_project(
    vault_root: &Path,
    binding: &TrackerBinding,
    adapter: &dyn Adapter,
    full: bool,
    now: DateTime<Utc>,
) -> Result<PullOutcome, PullError> {
    let mode = match full {
        true => Mode::Full,
        false => Mode::Incremental,
    };
    pull_project_waiting(vault_root, binding, adapter, mode, now, lock::DEFAULT_WAIT)
}

/// `pull_project`, waiting up to `wait` for a compaction holding the
/// project lock. A failure after the lock is taken appends a pull_failed
/// marker (best effort) before it returns.
pub fn pull_project_waiting(
    vault_root: &Path,
    binding: &TrackerBinding,
    adapter: &dyn Adapter,
    mode: Mode,
    now: DateTime<Utc>,
    wait: Duration,
) -> Result<PullOutcome, PullError> {
    pull_recorded(vault_root, binding, adapter, mode, now, wait, None)
}

/// Where a pull records itself, and its clock: the pull's `now` when it
/// began, before it waited for the lock, plus the time since. In use `now`
/// is the wall clock, so the state holds wall-clock times.
pub struct Recording<'a> {
    /// The project's refresh state file.
    pub state: &'a Path,
    /// When this pull began.
    pub began: DateTime<Utc>,
    start: std::time::Instant,
}

impl<'a> Recording<'a> {
    /// A recording into `state` for a pull that begins at `now`.
    pub fn starting(state: &'a Path, now: DateTime<Utc>) -> Self {
        Self { state, began: now, start: std::time::Instant::now() }
    }

    /// The pull's clock: `began` plus the time since it began.
    pub fn clock(&self) -> DateTime<Utc> {
        self.began + TimeDelta::from_std(self.start.elapsed()).unwrap_or_default()
    }
}

/// `pull_project_waiting`, recording into the refresh state when given.
fn pull_recorded(
    vault_root: &Path,
    binding: &TrackerBinding,
    adapter: &dyn Adapter,
    mode: Mode,
    now: DateTime<Utc>,
    wait: Duration,
    recording: Option<&Recording<'_>>,
) -> Result<PullOutcome, PullError> {
    let path = log::path_for(vault_root, &binding.domain, &binding.project);
    let lock = acquire_lock(&path, wait)?;
    pull_held(&path, binding, adapter, mode, now, &lock, recording)
}

/// Take the project lock beside `log_path`, failing with `lock_busy` after `wait`.
pub fn acquire_lock(log_path: &Path, wait: Duration) -> Result<lock::ProjectLock, PullError> {
    lock::acquire(log_path, wait).map_err(|message| {
        let code = match message.contains(lock::LOCK_BUSY) {
            true => FailureCode::LockBusy,
            false => FailureCode::LogWrite,
        };
        PullError::new(code, message)
    })
}

/// An `IncrementalOnly` pull for a caller that already holds the project
/// lock, so it can check the log under the lock before it pulls. Loads the
/// credential first, like `pull_binding`.
pub fn pull_binding_held(
    vault_root: &Path,
    config_dir: &Path,
    binding: &TrackerBinding,
    now: DateTime<Utc>,
    connect: &Connect<'_>,
    lock: &lock::ProjectLock,
) -> Result<PullOutcome, PullError> {
    let credential = load_credential(config_dir, binding)?;
    let adapter = connect(binding, credential.as_ref()).map_err(|message| PullError::new(FailureCode::UnsupportedProvider, message))?;
    let path = log::path_for(vault_root, &binding.domain, &binding.project);
    let state = state::path(config_dir, &binding.domain, &binding.project);
    pull_held(&path, binding, adapter.as_ref(), Mode::IncrementalOnly, now, lock, Some(&Recording::starting(&state, now)))
}

/// The pull itself, under `_lock`: the start in the refresh state `state`
/// and a pull_started marker with this process's id before the first
/// provider call, then the pull. A failure appends a pull_failed marker
/// (best effort) and records it in the state before it returns.
fn pull_held(
    path: &Path,
    binding: &TrackerBinding,
    adapter: &dyn Adapter,
    mode: Mode,
    now: DateTime<Utc>,
    _lock: &lock::ProjectLock,
    recording: Option<&Recording<'_>>,
) -> Result<PullOutcome, PullError> {
    let record = |record| {
        if let Some(recording) = recording {
            let _ = state::record(recording.state, &binding.provider, record, recording.clock());
        }
    };
    let skipped = Ok(PullOutcome { appended: 0, removed: 0, full: false, resync_due: None, failed_full: None, skipped: true });
    // A skipped pull writes nothing: the state already holds the real
    // completion, and a failure hold is left as it is.
    if recording.is_some_and(|r| overtaken(r, &binding.provider, mode)) {
        return skipped;
    }
    // The start goes to the local state before the log is read, so a read
    // that never returns still leaves a start the deadline can close.
    record(state::Record::Started(std::process::id()));
    let result = log::read_for(path, &binding.provider)
        .map_err(|message| PullError::new(FailureCode::LogRead, message))
        .and_then(|mut summary| {
            let started = pull_started(binding, now, std::process::id(), mode == Mode::AutomaticFull);
            let result = log::append_new(path, &[started], &mut summary)
                .map_err(|message| PullError::new(FailureCode::LogWrite, message))
                .and_then(|_| pull_locked(path, binding, adapter, mode, now, &mut summary));
            if let Err(error) = &result {
                let _ = log::append_new(path, &[pull_failed(binding, now, error.code, mode == Mode::AutomaticFull)], &mut summary);
            }
            result
        });
    match &result {
        Ok(_) => record(state::Record::Completed),
        Err(error) => record(state::Record::Failed(error.code)),
    }
    result
}

fn pull_locked(
    path: &Path,
    binding: &TrackerBinding,
    adapter: &dyn Adapter,
    mode: Mode,
    now: DateTime<Utc>,
    summary: &mut log::LogSummary,
) -> Result<PullOutcome, PullError> {
    let full = mode.is_full();
    let since = match full {
        true => None,
        false => summary.cursor.map(|t| t - CURSOR_OVERLAP),
    };
    let mut through = summary.cursor;
    let mut appended = 0;
    let mut returned: HashSet<String> = HashSet::new();
    let mut write_failed = false;
    // Each page is appended as it arrives (event ids make re-pulls
    // idempotent), so a failure keeps the pages already read. The cursor
    // moves only through the marker below, so a failure leaves it alone
    // whatever order the provider delivers pages in.
    let pulled = adapter.pull(since, full, &mut |page| {
        returned.extend(upserted_keys(&page));
        through = page.iter().fold(through, |t, e| log::later(t, e.common().occurred_at));
        appended += log::append_new(path, &page, summary).inspect_err(|_| write_failed = true)?;
        Ok(())
    });
    pulled.map_err(|message| {
        let code = match (write_failed, message.contains(crate::tracker::adapter::AUTH_REFUSED), message.contains(crate::tracker::adapter::UNREACHABLE)) {
            (true, _, _) => FailureCode::LogWrite,
            (false, true, _) => FailureCode::Auth,
            (false, false, true) => FailureCode::Credential,
            (false, false, false) => FailureCode::Provider,
        };
        PullError::new(code, message)
    })?;
    // A full result with no issues while the mirror holds open ones is far
    // more likely a lost team or token scope than an emptied tracker.
    if full && returned.is_empty() && !summary.open_issues.is_empty() && mode != Mode::FullAllowEmpty {
        return Err(PullError::new(
            FailureCode::EmptyFullResult,
            format!(
                "full pull returned no issues while the mirror holds {}; nothing removed. Check the team key and the token's access, or pass --allow-empty",
                summary.open_issues.len()
            ),
        ));
    }
    // Removals are only knowable after every page arrived.
    let markers = match full {
        true => resync_markers(binding, &returned, summary, now, through),
        false => vec![pull_completed(binding, now, through)],
    };
    let removed = markers.len().saturating_sub(1);
    log::append_new(path, &markers, summary).map_err(|message| PullError::new(FailureCode::LogWrite, message))?;
    appended += removed;
    Ok(PullOutcome { appended, removed, full, resync_due: None, failed_full: None, skipped: false })
}

/// True when the local refresh state shows a completion of `provider` later
/// than this pull's start and not later than its clock now. A log row never
/// decides it: a row's time is the writing pull's start, and another clock's
/// row can stand ahead of this one. A full pull a person asked for is never
/// overtaken.
fn overtaken(recording: &Recording<'_>, provider: &str, mode: Mode) -> bool {
    if matches!(mode, Mode::Full | Mode::FullAllowEmpty) {
        return false;
    }
    let clock = recording.clock();
    // Only a completion between this pull's start and its clock now counts.
    // One stamped later than the clock is from a skewed or future clock and
    // never stops a pull.
    let after_start = |at: DateTime<Utc>| at > recording.began && at <= clock;
    state::provider(recording.state, provider).and_then(|s| s.completed_at).is_some_and(after_start)
}

/// Marker for a pull about to call the provider, in process `pid`. An
/// automatic full pull and the incremental pull after it get distinct ids.
fn pull_started(binding: &TrackerBinding, now: DateTime<Utc>, pid: u32, automatic_full: bool) -> Event {
    let label = provider_label(&binding.provider);
    let kind = match automatic_full {
        true => ":automatic_full",
        false => "",
    };
    Event::PullStarted {
        common: local_common(
            binding,
            format!("wardwell:pull_started:{}:{}:{pid}{kind}", binding.scope(), now.to_rfc3339()),
            binding.scope(),
            binding.scope(),
            now,
            format!("{} pull from {label} started in process {pid}", binding.scope()),
        ),
        pid,
    }
}

/// Marker for a pull that stopped early. The title names only the code.
pub(crate) fn pull_failed(binding: &TrackerBinding, now: DateTime<Utc>, code: FailureCode, automatic_full: bool) -> Event {
    let label = provider_label(&binding.provider);
    Event::PullFailed {
        common: local_common(
            binding,
            match automatic_full {
                true => format!("wardwell:pull_failed:{}:{}:automatic_full", binding.scope(), now.to_rfc3339()),
                false => format!("wardwell:pull_failed:{}:{}", binding.scope(), now.to_rfc3339()),
            },
            binding.scope(),
            binding.scope(),
            now,
            format!("{} pull from {label} failed: {}", binding.scope(), code.as_str()),
        ),
        code,
        automatic_full,
    }
}

/// Cursor marker for an incremental pull that delivered every page.
fn pull_completed(binding: &TrackerBinding, now: DateTime<Utc>, through: Option<DateTime<Utc>>) -> Event {
    let label = provider_label(&binding.provider);
    let upto = through.map_or("the beginning".to_string(), |t| t.to_rfc3339());
    Event::PullCompleted {
        common: local_common(
            binding,
            format!("wardwell:pull_completed:{}:{}", binding.scope(), now.to_rfc3339()),
            binding.scope(),
            binding.scope(),
            now,
            format!("{} pull from {label} completed through {upto}", binding.scope()),
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
            format!("wardwell:full_resync:{}:{stamp}", binding.scope()),
            binding.scope(),
            binding.scope(),
            now,
            format!("{} full resync from {label}: {} {}, {removed} removed", binding.scope(), returned.len(), match crate::tracker::mirrors_issues(&binding.provider) {
                true => "issues",
                false => "changes",
            }),
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

/// Keys of the issues and changes a page returned. Only issue keys can be
/// open, so only they can be removed.
fn upserted_keys(events: &[Event]) -> impl Iterator<Item = String> + '_ {
    events
        .iter()
        .filter(|e| matches!(e, Event::IssueUpserted { .. } | Event::ChangeMerged { .. }))
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
            issue: Box::new(IssueSnapshot {
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
                ..Default::default()
            }),
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
            gate: false,
            repository: None,
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
        assert_eq!(error.message, "Linear request failed");
        assert_eq!(error.code, FailureCode::Provider);

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
    fn provider_failure_appends_a_pull_failed_marker_with_a_closed_code() {
        let vault = tempfile::tempdir().unwrap();
        pull_project(vault.path(), &binding(), &fake(vec![snapshot("COR-1", 9)]), false, at(12)).unwrap();
        let broken = FakeAdapter {
            pages: vec![],
            fail_after: Some("Linear returned an error: token lin_api_secret rejected"),
            calls: RefCell::new(vec![]),
        };
        let error = pull_project(vault.path(), &binding(), &broken, false, at(13)).unwrap_err();
        assert_eq!(error.code, FailureCode::Provider);

        let path = log::path_for(vault.path(), "work", "claims");
        let content = std::fs::read_to_string(&path).unwrap();
        let last: Event = serde_json::from_str(content.lines().last().unwrap()).unwrap();
        let Event::PullFailed { common, code, .. } = last else { panic!("{content}") };
        assert_eq!(code, FailureCode::Provider);
        assert_eq!(common.title, "COR pull from Linear failed: provider");
        assert!(!content.contains("lin_api_secret"), "no provider text in the log");
        let summary = log::read(&path).unwrap();
        assert_eq!(summary.last_failure, Some((at(13), FailureCode::Provider)));
        assert_eq!(summary.last_pull_at, Some(at(12)), "a failure is not a pull");
        assert_eq!(summary.cursor, Some(at(9)));
    }

    #[test]
    fn a_refused_token_is_recorded_as_auth() {
        let vault = tempfile::tempdir().unwrap();
        let message = format!("Linear returned HTTP 401: {}", crate::tracker::adapter::AUTH_REFUSED);
        let revoked = FakeAdapter { pages: vec![], fail_after: Some(message.leak()), calls: RefCell::new(vec![]) };
        let error = pull_project(vault.path(), &binding(), &revoked, false, at(13)).unwrap_err();
        assert_eq!(error.code, FailureCode::Auth, "{error}");
        let summary = log::read(&log::path_for(vault.path(), "work", "claims")).unwrap();
        assert_eq!(summary.last_failure, Some((at(13), FailureCode::Auth)));
    }

    #[test]
    fn a_failing_sink_is_a_log_write_failure() {
        let vault = tempfile::tempdir().unwrap();
        let path = log::path_for(vault.path(), "work", "claims");
        std::fs::create_dir_all(log::raw_path_for(&path)).unwrap();
        let mut event = snapshot("COR-1", 9);
        if let Event::IssueUpserted { common, .. } = &mut event {
            common.raw = serde_json::json!({"id": "x"});
        }
        let error = pull_project(vault.path(), &binding(), &fake(vec![event]), false, at(12)).unwrap_err();
        assert_eq!(error.code, FailureCode::LogWrite, "{error}");
    }

    /// Records, at the moment the provider is called, the log's last attempt.
    struct SeesTheLog {
        path: std::path::PathBuf,
        seen: RefCell<Option<crate::tracker::view::Attempt>>,
    }

    impl Adapter for SeesTheLog {
        fn pull(&self, _: Option<DateTime<Utc>>, _: bool, _: &mut Sink<'_>) -> Result<(), String> {
            *self.seen.borrow_mut() = crate::tracker::view::MirrorView::read_for(&self.path, "linear").unwrap().last_attempt;
            Ok(())
        }
    }

    #[test]
    fn pull_started_with_this_process_id_is_on_disk_before_the_provider_call() {
        let vault = tempfile::tempdir().unwrap();
        let path = log::path_for(vault.path(), "work", "claims");
        let adapter = SeesTheLog { path: path.clone(), seen: RefCell::new(None) };
        pull_project(vault.path(), &binding(), &adapter, false, at(12)).unwrap();
        let pid = std::process::id();
        assert_eq!(*adapter.seen.borrow(), Some(crate::tracker::view::Attempt::Started { at: at(12), pid }));
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains(&format!("\"kind\":\"pull_started\",\"id\":\"wardwell:pull_started:COR:{}:{pid}\"", at(12).to_rfc3339())), "{content}");
        assert_eq!(crate::tracker::view::MirrorView::read_for(&path, "linear").unwrap().last_attempt, None, "the completed pull ends it");
    }

    /// Fails a full pull, delivers nothing on an incremental one.
    struct FullThenFine;

    impl Adapter for FullThenFine {
        fn pull(&self, _: Option<DateTime<Utc>>, full: bool, _: &mut Sink<'_>) -> Result<(), String> {
            match full {
                true => Err("Linear request failed".to_string()),
                false => Ok(()),
            }
        }
    }

    #[test]
    fn an_automatic_full_and_the_incremental_after_it_each_record_a_start() {
        let vault = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        crate::tracker::credential::save(&crate::tracker::credential::path_in(config.path(), "corr-linear").unwrap(), "t").unwrap();
        let connect = |_: &TrackerBinding, _: Option<&Credential>| -> Result<Box<dyn Adapter>, String> { Ok(Box::new(FullThenFine)) };
        pull_binding(vault.path(), config.path(), &binding(), Mode::Incremental, at(12), &connect).unwrap();
        let path = log::path_for(vault.path(), "work", "claims");
        let kinds: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .skip(1)
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["kind"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(kinds, vec!["pull_started", "pull_failed", "pull_started", "pull_completed"]);
        assert_eq!(crate::tracker::view::MirrorView::read_for(&path, "linear").unwrap().last_attempt, None);
    }

    /// Counts provider reads across threads.
    struct CountsReads(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    impl Adapter for CountsReads {
        fn pull(&self, _: Option<DateTime<Utc>>, _: bool, _: &mut Sink<'_>) -> Result<(), String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    /// Counts provider reads across threads, waits `hold`, then fails.
    struct SlowFails(std::sync::Arc<std::sync::atomic::AtomicUsize>, Duration);

    impl Adapter for SlowFails {
        fn pull(&self, _: Option<DateTime<Utc>>, _: bool, _: &mut Sink<'_>) -> Result<(), String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::thread::sleep(self.1);
            Err("Linear request failed".to_string())
        }
    }

    #[test]
    fn a_foreign_log_completion_never_makes_a_waiting_pull_skip_or_clear_a_hold() {
        let vault = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        crate::tracker::credential::save(&crate::tracker::credential::path_in(config.path(), "corr-linear").unwrap(), "t").unwrap();
        let path = log::path_for(vault.path(), "work", "claims");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let ahead = (Utc::now() + TimeDelta::seconds(2)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let resync = format!(r#"{{"kind":"full_resync","id":"r","provider":"linear","external_key":"COR","external_id":"COR","occurred_at":"{ahead}","title":"r","issues":0,"removed":0}}"#);
        let foreign = format!(r#"{{"kind":"pull_completed","id":"foreign","provider":"linear","external_key":"COR","external_id":"COR","occurred_at":"{ahead}","title":"foreign"}}"#);
        std::fs::write(&path, format!("{}\n{resync}\n{foreign}\n", crate::tracker::events::SCHEMA_HEADER)).unwrap();
        let state_path = state::path(config.path(), "work", "claims");
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pull_in_thread = |hold: Duration| {
            let (vault_dir, config_dir, counter) = (vault.path().to_path_buf(), config.path().to_path_buf(), std::sync::Arc::clone(&reads));
            std::thread::spawn(move || {
                let connect = move |_: &TrackerBinding, _: Option<&Credential>| -> Result<Box<dyn Adapter>, String> { Ok(Box::new(SlowFails(std::sync::Arc::clone(&counter), hold))) };
                pull_binding(&vault_dir, &config_dir, &binding(), Mode::IncrementalOnly, Utc::now(), &connect)
            })
        };
        let a = pull_in_thread(Duration::from_millis(2500));
        std::thread::sleep(Duration::from_millis(200));
        let b = pull_in_thread(Duration::ZERO);
        let a = a.join().unwrap().unwrap_err();
        assert_eq!(a.code, FailureCode::Provider);
        let b = b.join().unwrap();
        assert!(!matches!(&b, Ok(outcome) if outcome.skipped), "B must not skip: {b:?}");
        assert_eq!(b.unwrap_err().code, FailureCode::Provider, "B failed on its own terms");
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 2, "B read the provider");
        let local = state::provider(&state_path, "linear").unwrap();
        assert_eq!(local.completed_at, None, "no pull recorded a completion");
        assert!(local.open_failure().is_some(), "the failure hold stays: {local:?}");
    }

    #[test]
    fn a_pull_overtaken_while_it_waited_for_the_lock_reads_nothing() {
        let vault = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        crate::tracker::credential::save(&crate::tracker::credential::path_in(config.path(), "corr-linear").unwrap(), "t").unwrap();
        let path = log::path_for(vault.path(), "work", "claims");
        let state_path = state::path(config.path(), "work", "claims");
        let held = lock::acquire(&path, Duration::ZERO).unwrap();
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (vault_dir, config_dir, counter) = (vault.path().to_path_buf(), config.path().to_path_buf(), std::sync::Arc::clone(&reads));
        let waiting = std::thread::spawn(move || {
            let connect = move |_: &TrackerBinding, _: Option<&Credential>| -> Result<Box<dyn Adapter>, String> { Ok(Box::new(CountsReads(std::sync::Arc::clone(&counter)))) };
            pull_binding(&vault_dir, &config_dir, &binding(), Mode::IncrementalOnly, Utc::now(), &connect)
        });
        std::thread::sleep(Duration::from_millis(300));
        state::record(&state_path, "linear", state::Record::Completed, Utc::now()).unwrap();
        drop(held);
        let outcome = waiting.join().unwrap().unwrap();
        assert!(outcome.skipped, "{outcome:?}");
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 0, "no provider read");
        assert!(!path.exists() || !std::fs::read_to_string(&path).unwrap().contains("pull_started"), "no start recorded");

        let reads_before = reads.load(std::sync::atomic::Ordering::SeqCst);
        let counter = std::sync::Arc::clone(&reads);
        let connect = move |_: &TrackerBinding, _: Option<&Credential>| -> Result<Box<dyn Adapter>, String> { Ok(Box::new(CountsReads(std::sync::Arc::clone(&counter)))) };
        let fresh = pull_binding(vault.path(), config.path(), &binding(), Mode::IncrementalOnly, Utc::now() + TimeDelta::seconds(1), &connect).unwrap();
        assert!(!fresh.skipped, "a completion before this pull began does not stop it");
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), reads_before + 1);
    }

    #[test]
    fn first_pull_starts_from_the_beginning_and_appends() {
        let vault = tempfile::tempdir().unwrap();
        let adapter = fake(vec![snapshot("COR-1", 9), snapshot("COR-2", 10)]);
        let outcome = pull_project(vault.path(), &binding(), &adapter, false, at(12)).unwrap();
        assert_eq!(outcome.appended, 2);
        assert_eq!(adapter.calls.borrow()[0], (None, false));
        let path = log::path_for(vault.path(), "work", "claims");
        assert_eq!(log::read(&path).unwrap().event_count, 4, "pull_started, two snapshots and the pull_completed marker");
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
        let Some(Event::IssueRemoved { common }) = events.iter().find(|e| matches!(e, Event::IssueRemoved { .. })) else { panic!("{content}") };
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

    fn github() -> TrackerBinding {
        TrackerBinding {
            domain: "work".into(),
            project: "claims".into(),
            provider: "github".into(),
            team: String::new(),
            credential: "github".into(),
            readonly: false,
            gate: false,
            repository: Some("acme/app".into()),
        }
    }

    /// A change the GitHub mirror saw merged at `hour`.
    fn merged(number: u64, hour: u32) -> Event {
        Event::ChangeMerged {
            common: Common {
                id: format!("github:acme/app#{number}"),
                provider: "github".into(),
                external_key: format!("acme/app#{number}"),
                external_id: format!("PR_{number}"),
                actor: Some("jdoe".into()),
                occurred_at: at(hour),
                title: format!("acme/app#{number} merged into main: COR-{number} change"),
                raw: serde_json::Value::Null,
            },
            change: Box::new(crate::tracker::events::MergedChange { number, merged_at: at(hour), ..Default::default() }),
        }
    }

    #[test]
    fn linear_and_github_events_interleave_and_each_cursor_advances_on_its_own() {
        let vault = tempfile::tempdir().unwrap();
        let path = log::path_for(vault.path(), "work", "claims");
        let linear = |events| pull_project(vault.path(), &binding(), &fake(events), false, at(12)).unwrap();
        linear(vec![snapshot("COR-1", 9)]);
        // GitHub writes newer events and its own marker after Linear's.
        pull_project(vault.path(), &github(), &fake(vec![merged(7, 11)]), false, at(13)).unwrap();
        assert_eq!(log::read_for(&path, "linear").unwrap().cursor, Some(at(9)), "a github marker does not move the linear cursor");
        assert_eq!(log::read_for(&path, "github").unwrap().cursor, Some(at(11)));

        let next_linear = fake(vec![snapshot("COR-2", 14)]);
        pull_project(vault.path(), &binding(), &next_linear, false, at(15)).unwrap();
        assert_eq!(next_linear.calls.borrow()[0], (Some(at(9) - CURSOR_OVERLAP), false), "linear resumes from its own marker");
        assert_eq!(log::read_for(&path, "linear").unwrap().cursor, Some(at(14)));
        assert_eq!(log::read_for(&path, "github").unwrap().cursor, Some(at(11)), "a linear marker does not move the github cursor");

        let next_github = fake(vec![merged(7, 11), merged(8, 16)]);
        let outcome = pull_project(vault.path(), &github(), &next_github, false, at(17)).unwrap();
        assert_eq!(next_github.calls.borrow()[0], (Some(at(11) - CURSOR_OVERLAP), false), "github resumes from its own marker");
        assert_eq!(outcome.appended, 1, "the overlap re-pull of #7 is deduplicated");
        assert_eq!(log::read_for(&path, "github").unwrap().cursor, Some(at(16)));
        assert_eq!(log::read_for(&path, "linear").unwrap().cursor, Some(at(14)));
    }

    fn github_adapter(outcome: crate::tracker::github::GhOutcome) -> GitHub {
        let (gh, _) = crate::tracker::github::tests::gh_with(outcome);
        github_for(&github(), None, gh)
    }

    fn gh_output(nodes: Vec<serde_json::Value>) -> crate::tracker::github::GhOutcome {
        crate::tracker::github::GhOutcome::Output(serde_json::to_vec(&nodes).unwrap())
    }

    #[test]
    fn a_re_pull_of_an_unchanged_pull_request_appends_nothing_and_raw_goes_to_the_sidecar() {
        use crate::tracker::github::tests::gh_node;
        let vault = tempfile::tempdir().unwrap();
        let nodes = || gh_output(vec![gh_node(42, "COR-12 Fix the inbox", 10), gh_node(41, "Bump deps", 9)]);
        let first = pull_project(vault.path(), &github(), &github_adapter(nodes()), false, at(12)).unwrap();
        assert_eq!(first.appended, 2);
        let again = pull_project(vault.path(), &github(), &github_adapter(nodes()), false, at(13)).unwrap();
        assert_eq!(again.appended, 0, "same pull requests, same ids");
        let again = pull_project(vault.path(), &github(), &github_adapter(nodes()), true, at(14)).unwrap();
        assert_eq!((again.appended, again.removed), (0, 0), "a full re-pull appends only its marker");

        let path = log::path_for(vault.path(), "work", "claims");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("\"raw\""), "{content}");
        assert_eq!(content.matches("\"kind\":\"change_merged\"").count(), 2, "{content}");
        assert!(content.contains("acme/app full resync from GitHub: 2 changes, 0 removed"), "{content}");
        let raws = log::read_raw(&log::raw_path_for(&path)).unwrap();
        assert_eq!(raws["github:acme/app#42"]["title"], "COR-12 Fix the inbox");
        assert_eq!(log::read_for(&path, "github").unwrap().cursor, Some(at(10)));
    }

    /// Minute `m` after 2026-09-01T00:00:00Z.
    fn minute(m: i64) -> DateTime<Utc> {
        at(0) + TimeDelta::minutes(m)
    }

    fn dataset_adapter(gh: crate::tracker::github::tests::DatasetGh) -> GitHub {
        github_for(&github(), None, Box::new(gh))
    }

    /// Chunk headings the real index returns for `query` over the vault.
    fn search(vault: &Path, query: &str) -> Vec<String> {
        let store = crate::index::store::IndexStore::in_memory().unwrap();
        crate::index::builder::IndexBuilder::full_build(&store, vault, None).unwrap();
        store.chunk_fts_search(query, 20, None).unwrap().into_iter().filter_map(|(id, _)| store.get_chunk(&id).ok().and_then(|c| c.2)).collect()
    }

    #[test]
    fn a_pull_request_retitled_after_merge_gains_its_new_key_and_search_finds_it() {
        use crate::tracker::github::tests::{DatasetGh, pr};
        let vault = tempfile::tempdir().unwrap();
        let path = log::path_for(vault.path(), "work", "claims");
        let (gh, nodes, replies) = DatasetGh::new(vec![pr(42, "Fix the claims inbox", "Body", minute(10), minute(10))]);
        pull_project(vault.path(), &github(), &dataset_adapter(gh), false, minute(20)).unwrap();
        assert_eq!(log::read_for(&path, "github").unwrap().cursor, Some(minute(10)));

        nodes.borrow_mut()[0] = pr(42, "COR-77 Fix the claims inbox", "Body", minute(10), minute(90));
        let gh = DatasetGh { nodes: nodes.clone(), replies: replies.clone(), ..Default::default() };
        let outcome = pull_project(vault.path(), &github(), &dataset_adapter(gh), false, minute(100)).unwrap();
        assert_eq!(outcome.appended, 1, "the retitle is a new revision");
        assert!(replies.borrow().last().unwrap().0.join(" ").contains("--search is:merged updated:>=2026-08-31T23:10:00Z sort:updated-desc"), "{:?}", replies.borrow());
        let summary = log::read_for(&path, "github").unwrap();
        assert_eq!(summary.cursor, Some(minute(90)), "the cursor is the newest update time");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"id\":\"github:acme/app#42:rev:"), "{content}");
        assert!(content.contains("\"keys\":[\"COR-77\"]"), "{content}");
        let found = search(vault.path(), "COR-77");
        assert_eq!(found, vec!["acme/app#42 merged into main: COR-77 Fix the claims inbox; revision updated 2026-09-01T01:30:00Z"]);
    }

    #[test]
    fn search_returns_every_revision_and_each_heading_shows_its_update_time() {
        use crate::tracker::github::tests::{DatasetGh, pr};
        let vault = tempfile::tempdir().unwrap();
        let titles = [("COR-12 Fix the claims inbox", 10), ("COR-77 Fix the claims inbox", 90), ("COR-12 Fix the claims inbox", 200)];
        for (title, updated) in titles {
            let (gh, _, _) = DatasetGh::new(vec![pr(42, title, "Body", minute(10), minute(updated))]);
            pull_project(vault.path(), &github(), &dataset_adapter(gh), false, minute(updated + 5)).unwrap();
        }
        assert_eq!(
            search(vault.path(), "COR-77"),
            vec!["acme/app#42 merged into main: COR-77 Fix the claims inbox; revision updated 2026-09-01T01:30:00Z"],
            "an earlier revision, older than the current one at 03:20"
        );
        let mut current = search(vault.path(), "COR-12");
        current.sort();
        assert_eq!(current, vec![
            "acme/app#42 merged into main: COR-12 Fix the claims inbox",
            "acme/app#42 merged into main: COR-12 Fix the claims inbox; revision updated 2026-09-01T03:20:00Z",
        ]);
        let path = log::path_for(vault.path(), "work", "claims");
        assert_eq!(log::read_for(&path, "github").unwrap().changes["acme/app#42"].keys, vec!["COR-12"]);
    }

    #[test]
    fn a_comment_only_update_appends_nothing_and_full_repairs_keys() {
        use crate::tracker::github::tests::{DatasetGh, pr};
        let vault = tempfile::tempdir().unwrap();
        let path = log::path_for(vault.path(), "work", "claims");
        let (gh, nodes, _) = DatasetGh::new(vec![pr(42, "COR-12 Fix", "Body", minute(10), minute(10))]);
        pull_project(vault.path(), &github(), &dataset_adapter(gh), false, minute(20)).unwrap();
        nodes.borrow_mut()[0] = pr(42, "COR-12 Fix", "Body", minute(10), minute(50));
        let gh = DatasetGh { nodes: nodes.clone(), replies: Default::default(), ..Default::default() };
        let outcome = pull_project(vault.path(), &github(), &dataset_adapter(gh), false, minute(60)).unwrap();
        assert_eq!(outcome.appended, 0, "a comment moves the update time, not the content");
        assert_eq!(log::read_for(&path, "github").unwrap().cursor, Some(minute(50)));

        // Keys behind the cursor, as an earlier build parsed them, are out
        // of reach of an incremental pull; --full repairs them.
        nodes.borrow_mut().push(pr(43, "Later", "Body", minute(300), minute(300)));
        let gh = DatasetGh { nodes: nodes.clone(), replies: Default::default(), ..Default::default() };
        pull_project(vault.path(), &github(), &dataset_adapter(gh), false, minute(310)).unwrap();
        nodes.borrow_mut()[0] = pr(42, "COR-12 COR-13 Fix", "Body", minute(10), minute(50));
        let gh = DatasetGh { nodes: nodes.clone(), replies: Default::default(), ..Default::default() };
        let missed = pull_project(vault.path(), &github(), &dataset_adapter(gh), false, minute(320)).unwrap();
        assert_eq!(missed.appended, 0);
        assert_eq!(log::read_for(&path, "github").unwrap().changes["acme/app#42"].keys, vec!["COR-12"]);
        let gh = DatasetGh { nodes: nodes.clone(), replies: Default::default(), ..Default::default() };
        let full = pull_project(vault.path(), &github(), &dataset_adapter(gh), true, minute(330)).unwrap();
        assert_eq!(full.appended, 1);
        assert_eq!(log::read_for(&path, "github").unwrap().changes["acme/app#42"].keys, vec!["COR-12", "COR-13"]);
        let gh = DatasetGh { nodes: nodes.clone(), replies: Default::default(), ..Default::default() };
        assert_eq!(pull_project(vault.path(), &github(), &dataset_adapter(gh), true, minute(340)).unwrap().appended, 0);
    }

    #[test]
    fn the_first_pull_of_200_then_an_incremental_pull_leaves_nothing_after_its_oldest_row_unmirrored() {
        use crate::tracker::github::tests::{DatasetGh, pr};
        let vault = tempfile::tempdir().unwrap();
        let path = log::path_for(vault.path(), "work", "claims");
        let rows: Vec<serde_json::Value> = (1..=300).map(|n| pr(n, &format!("Change {n}"), "Body", minute(n as i64), minute(n as i64))).collect();
        let (gh, nodes, _) = DatasetGh::new(rows);
        pull_project(vault.path(), &github(), &dataset_adapter(gh), false, minute(400)).unwrap();
        let oldest = minute(101);
        assert_eq!(log::read_for(&path, "github").unwrap().changes.len(), 200);

        // After the first pull: an old one is edited, a mirrored one is edited, a new one merges.
        nodes.borrow_mut()[49] = pr(50, "COR-50 Change 50", "Body", minute(50), minute(500));
        nodes.borrow_mut()[199] = pr(200, "Change 200", "New body", minute(200), minute(501));
        nodes.borrow_mut().push(pr(301, "Change 301", "Body", minute(502), minute(502)));
        let gh = DatasetGh { nodes: nodes.clone(), replies: Default::default(), ..Default::default() };
        pull_project(vault.path(), &github(), &dataset_adapter(gh), false, minute(600)).unwrap();

        let mirrored = log::read_for(&path, "github").unwrap().changes;
        for node in nodes.borrow().iter() {
            let updated = DateTime::parse_from_rfc3339(node["updatedAt"].as_str().unwrap()).unwrap().with_timezone(&Utc);
            let key = format!("acme/app#{}", node["number"]);
            if updated >= oldest {
                let latest = &mirrored.get(&key).unwrap_or_else(|| panic!("{key} is not mirrored"));
                assert_eq!(latest.title, node["title"].as_str().unwrap(), "{key}");
                assert_eq!(latest.body.as_deref(), node["body"].as_str(), "{key}");
            }
        }
    }

    /// A log with #1 pulled at minute 10, and 400 more merged pull requests
    /// updated one a minute from minute 20.
    fn cursor_then_400(vault: &Path) -> std::rc::Rc<RefCell<Vec<serde_json::Value>>> {
        use crate::tracker::github::tests::{DatasetGh, pr};
        let (gh, nodes, _) = DatasetGh::new(vec![pr(1, "One", "Body", minute(10), minute(10))]);
        pull_project(vault, &github(), &dataset_adapter(gh), false, minute(15)).unwrap();
        nodes.borrow_mut().extend((2..=401).map(|n| pr(n, "Change", "Body", minute(n as i64 + 18), minute(n as i64 + 18))));
        nodes
    }

    #[test]
    fn a_failure_on_the_fifth_window_keeps_four_windows_of_rows_and_the_next_pull_appends_the_rest() {
        use crate::tracker::github::tests::DatasetGh;
        let vault = tempfile::tempdir().unwrap();
        let path = log::path_for(vault.path(), "work", "claims");
        let nodes = cursor_then_400(vault.path());
        let replies = std::rc::Rc::new(RefCell::new(Vec::new()));
        let failing = DatasetGh { nodes: nodes.clone(), replies: replies.clone(), fail_window: Some(5), ..Default::default() };
        let error = pull_project(vault.path(), &github(), &dataset_adapter(failing), false, minute(500)).unwrap_err();
        assert_eq!((error.message.as_str(), error.code), ("gh exited with status 1", FailureCode::Provider));
        let windows: Vec<usize> = replies.borrow().iter().map(|(_, rows, _)| *rows).filter(|rows| (1..100).contains(rows)).collect();
        assert_eq!(windows.len(), 4, "{:?}", replies.borrow().iter().map(|r| r.1).collect::<Vec<_>>());
        let kept: usize = windows.iter().sum();
        let summary = log::read_for(&path, "github").unwrap();
        assert_eq!(summary.changes.len(), 1 + kept, "the first four windows' rows are in the log");
        assert_eq!((summary.cursor, summary.last_pull_at), (Some(minute(10)), Some(minute(15))), "no marker");

        let gh = DatasetGh { nodes: nodes.clone(), ..Default::default() };
        let outcome = pull_project(vault.path(), &github(), &dataset_adapter(gh), false, minute(510)).unwrap();
        assert_eq!(outcome.appended, 400 - kept, "only the rest");
        let summary = log::read_for(&path, "github").unwrap();
        assert_eq!(summary.changes.len(), 401);
        assert_eq!(summary.last_pull_at, Some(minute(510)));
    }

    #[test]
    fn the_budget_stops_a_slow_read_keeps_its_rows_and_releases_the_lock() {
        use crate::tracker::github::tests::DatasetGh;
        let vault = tempfile::tempdir().unwrap();
        let path = log::path_for(vault.path(), "work", "claims");
        let nodes = cursor_then_400(vault.path());
        let slow = DatasetGh { nodes: nodes.clone(), delay: Duration::from_millis(250), ..Default::default() };
        let adapter = dataset_adapter(slow).with_budget(Duration::from_secs(2));
        let started = std::time::Instant::now();
        let error = pull_project(vault.path(), &github(), &adapter, false, minute(500)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
        assert_eq!(
            (error.message.as_str(), error.code),
            ("the read did not finish in 2 seconds; rows read so far are kept; run the pull again", FailureCode::Provider)
        );
        let summary = log::read_for(&path, "github").unwrap();
        assert_eq!(summary.last_pull_at, Some(minute(15)), "no marker");
        assert_eq!(summary.last_failure.map(|(_, c)| c), Some(FailureCode::Provider));
        lock::acquire(&path, lock::TEST_FREE_WAIT).unwrap();
    }

    #[test]
    fn a_gh_reply_at_the_limit_in_one_second_writes_no_pull_completed() {
        use crate::tracker::github::tests::{DatasetGh, pr};
        let vault = tempfile::tempdir().unwrap();
        let path = log::path_for(vault.path(), "work", "claims");
        let (gh, nodes, _) = DatasetGh::new(vec![pr(1, "One", "Body", minute(10), minute(10))]);
        pull_project(vault.path(), &github(), &dataset_adapter(gh), false, minute(20)).unwrap();
        nodes.borrow_mut().extend((2..=101).map(|n| pr(n, "Same second", "Body", minute(30), minute(40))));
        let gh = DatasetGh { nodes: nodes.clone(), replies: Default::default(), ..Default::default() };
        let error = pull_project(vault.path(), &github(), &dataset_adapter(gh), false, minute(50)).unwrap_err();
        assert_eq!(error.code, FailureCode::Provider, "{error}");
        let summary = log::read_for(&path, "github").unwrap();
        assert_eq!((summary.cursor, summary.last_pull_at), (Some(minute(10)), Some(minute(20))), "no pull_completed");
    }

    #[test]
    fn a_full_github_pull_never_removes_linear_issues_in_the_same_log() {
        let vault = tempfile::tempdir().unwrap();
        pull_project(vault.path(), &binding(), &fake(vec![snapshot("COR-1", 9), snapshot("COR-2", 9)]), true, at(10)).unwrap();
        let outcome = pull_project(vault.path(), &github(), &github_adapter(gh_output(vec![])), true, at(11)).unwrap();
        assert_eq!(outcome.removed, 0);
        let path = log::path_for(vault.path(), "work", "claims");
        assert_eq!(log::read_for(&path, "linear").unwrap().open_issues.len(), 2);
        assert!(!std::fs::read_to_string(&path).unwrap().contains("issue_removed"));
    }

    #[test]
    fn a_github_binding_pulls_without_a_stored_token_and_never_runs_an_automatic_full() {
        let vault = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let seen = std::rc::Rc::new(RefCell::new(Vec::new()));
        let adapter = std::rc::Rc::new(fake(vec![merged(7, 9)]));
        struct Shared(std::rc::Rc<FakeAdapter>);
        impl Adapter for Shared {
            fn pull(&self, since: Option<DateTime<Utc>>, full: bool, sink: &mut Sink<'_>) -> Result<(), String> {
                self.0.pull(since, full, sink)
            }
        }
        let (record, shared) = (seen.clone(), adapter.clone());
        let connect = move |_: &TrackerBinding, credential: Option<&Credential>| -> Result<Box<dyn Adapter>, String> {
            record.borrow_mut().push(credential.is_some());
            Ok(Box::new(Shared(shared.clone())))
        };
        let outcome = pull_binding(vault.path(), config.path(), &github(), Mode::Incremental, at(12), &connect).unwrap();
        assert_eq!((outcome.full, outcome.resync_due), (false, None));
        let later = at(12) + FULL_RESYNC_MAX_AGE + TimeDelta::hours(1);
        pull_binding(vault.path(), config.path(), &github(), Mode::Incremental, later, &connect).unwrap();
        assert_eq!(*adapter.calls.borrow(), vec![(None, false), (Some(at(9) - CURSOR_OVERLAP), false)], "the first pull is the bounded one, then the cursor");
        assert_eq!(*seen.borrow(), vec![false, false]);

        let path = crate::tracker::credential::path_in(config.path(), "github").unwrap();
        crate::tracker::credential::save(&path, "ghp_test").unwrap();
        pull_binding(vault.path(), config.path(), &github(), Mode::Incremental, later, &connect).unwrap();
        assert_eq!(seen.borrow().last(), Some(&true), "a stored token reaches the adapter");
    }

    #[test]
    fn a_token_saved_by_connect_github_reaches_a_github_binding_without_a_credential_field() {
        let config = tempfile::tempdir().unwrap();
        assert!(load_credential(config.path(), &github()).unwrap().is_none());
        let message = crate::tracker::cli::connect(config.path(), "github", "ghp_secret\n").unwrap();
        assert!(!message.contains("ghp_secret"), "{message}");
        let stored = load_credential(config.path(), &github()).unwrap().unwrap();
        assert_eq!(stored.token(), "ghp_secret");
        assert!(!format!("{stored:?}").contains("ghp_secret"));
    }

    #[cfg(unix)]
    #[test]
    fn a_github_token_with_loose_permissions_fails_before_the_log() {
        use std::os::unix::fs::PermissionsExt;
        let vault = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let path = crate::tracker::credential::path_in(config.path(), "github").unwrap();
        crate::tracker::credential::save(&path, "ghp_test").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let connect = |_: &TrackerBinding, _: Option<&Credential>| -> Result<Box<dyn Adapter>, String> { Ok(Box::new(fake(vec![]))) };
        let error = pull_binding(vault.path(), config.path(), &github(), Mode::Incremental, at(12), &connect).unwrap_err();
        assert_eq!(error.code, FailureCode::Credential, "{error}");
        assert!(!log::path_for(vault.path(), "work", "claims").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_gh_timeout_records_its_failure_and_releases_the_project_lock() {
        use crate::tracker::github::{SystemGh, tests::stub_gh};
        let dir = tempfile::tempdir().unwrap();
        let stub = stub_gh(dir.path(), "sleep 30 &\nsleep 30");
        let runner = SystemGh::at(Some(stub)).with_limits(Duration::from_secs(1), 1024);
        let vault = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();
        let error = pull_project(vault.path(), &github(), &github_for(&github(), None, Box::new(runner)), false, at(12)).unwrap_err();
        assert!(started.elapsed() < Duration::from_millis(2500), "{:?}", started.elapsed());
        assert_eq!((error.message.as_str(), error.code), ("gh did not finish within 1 seconds", FailureCode::Provider));
        let path = log::path_for(vault.path(), "work", "claims");
        assert_eq!(log::read_for(&path, "github").unwrap().last_failure.map(|(_, c)| c), Some(FailureCode::Provider));
        lock::acquire(&path, lock::TEST_FREE_WAIT).unwrap();
    }

    #[test]
    fn a_failing_gh_without_a_token_is_a_provider_failure_with_its_sentence() {
        use crate::tracker::github::GhOutcome;
        let cases = [
            (GhOutcome::Exited(Some(1)), "gh exited with status 1", FailureCode::Provider),
            (GhOutcome::TimedOut(Duration::from_secs(120)), "gh did not finish within 120 seconds", FailureCode::Provider),
            (GhOutcome::Oversize(16), "gh output exceeded 16 bytes", FailureCode::Provider),
            (GhOutcome::Output(b"{}".to_vec()), "gh returned output that is not a list of pull requests", FailureCode::Provider),
            (GhOutcome::SignedOut, "gh is not signed in; github: unreachable, run `wardwell tracker connect github`", FailureCode::Credential),
        ];
        for (outcome, sentence, code) in cases {
            let vault = tempfile::tempdir().unwrap();
            let error = pull_project(vault.path(), &github(), &github_adapter(outcome), false, at(12)).unwrap_err();
            assert_eq!((error.message.as_str(), error.code), (sentence, code));
            let summary = log::read_for(&log::path_for(vault.path(), "work", "claims"), "github").unwrap();
            assert_eq!(summary.last_failure, Some((at(12), code)));
            assert_eq!(summary.last_pull_at, None, "no pull_completed");
        }
    }

    #[test]
    fn neither_gh_nor_a_token_appends_a_credential_marker_without_secrets() {
        let vault = tempfile::tempdir().unwrap();
        let error = pull_project(vault.path(), &github(), &github_adapter(crate::tracker::github::GhOutcome::Missing), false, at(12)).unwrap_err();
        assert_eq!(error.code, FailureCode::Credential);
        assert_eq!(error.to_string(), "github: unreachable, run `wardwell tracker connect github` (credential)");
        let path = log::path_for(vault.path(), "work", "claims");
        let summary = log::read_for(&path, "github").unwrap();
        assert_eq!(summary.last_failure, Some((at(12), FailureCode::Credential)));
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("acme/app pull from GitHub failed: credential"), "{content}");
    }

    #[cfg(unix)]
    #[test]
    fn with_a_bare_path_the_pull_and_doctor_use_gh_at_a_candidate_path() {
        use crate::tracker::github::{SystemGh, locate_gh, tests::stub_gh};
        let dir = tempfile::tempdir().unwrap();
        let node = crate::tracker::github::tests::gh_node(42, "COR-12 Fix the inbox", 10);
        let stub = stub_gh(&dir.path().join("homebrew"), &format!(
            "if [ \"$1\" = repo ]; then echo '{{\"nameWithOwner\":\"acme/app\"}}'; exit 0; fi\necho '[{node}]'"
        ));
        // PATH is an empty folder made here, never a system folder.
        let empty = dir.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let found = locate_gh(Some(empty.as_os_str()), std::slice::from_ref(&stub));
        assert_eq!(found.as_deref(), Some(stub.as_path()));

        let vault = tempfile::tempdir().unwrap();
        let adapter = github_for(&github(), None, Box::new(SystemGh::at(found.clone())));
        let outcome = pull_project(vault.path(), &github(), &adapter, false, at(12)).unwrap();
        assert_eq!(outcome.appended, 1);
        let path = log::path_for(vault.path(), "work", "claims");
        assert_eq!(log::read_for(&path, "github").unwrap().last_failure, None);

        let config = crate::config::loader::parse(&format!(
            "vault_path: {}\nsession_sources: []\ntrackers:\n  work/claims:\n    provider: github\n    repository: acme/app\n",
            vault.path().display()
        ))
        .unwrap();
        let probe = |b: &TrackerBinding, c: Option<&Credential>| github_for(b, c, Box::new(SystemGh::at(found.clone())));
        let linear = |_: &TrackerBinding, _: &Credential| -> Result<Box<dyn crate::tracker::linear::Transport>, String> { Err("unused".into()) };
        let (lines, healthy) = crate::tracker::doctor::run_with(&config, dir.path(), &linear, &probe, &Default::default());
        assert!(healthy, "{lines:?}");
        assert_eq!(lines[1], "work/claims: github repository acme/app ok through gh");
        assert_eq!(crate::tracker::doctor::check_offline_with(dir.path(), &config.trackers[0], found.is_some()), Ok(()));
    }

    #[test]
    fn a_failure_of_one_provider_is_not_the_other_providers_failure() {
        let vault = tempfile::tempdir().unwrap();
        let path = log::path_for(vault.path(), "work", "claims");
        pull_project(vault.path(), &binding(), &fake(vec![snapshot("COR-1", 9)]), false, at(12)).unwrap();
        let broken = FakeAdapter { pages: vec![], fail_after: Some("GitHub request failed"), calls: RefCell::new(vec![]) };
        pull_project(vault.path(), &github(), &broken, false, at(13)).unwrap_err();
        let linear = log::read_for(&path, "linear").unwrap();
        assert_eq!(linear.last_failure, None);
        assert_eq!(linear.last_pull_at, Some(at(12)));
        let github = log::read_for(&path, "github").unwrap();
        assert_eq!(github.last_failure, Some((at(13), FailureCode::Provider)));
        assert_eq!(github.last_pull_at, None);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"id\":\"wardwell:pull_failed:acme/app:"), "the github marker names its repository: {content}");
    }

    #[test]
    fn a_marker_without_a_provider_counts_as_linear() {
        let vault = tempfile::tempdir().unwrap();
        let path = log::path_for(vault.path(), "work", "claims");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let old = r#"{"kind":"pull_completed","id":"p1","external_key":"COR","external_id":"COR","occurred_at":"2026-09-01T10:00:00Z","title":"COR pull","through":"2026-09-01T09:00:00Z"}"#;
        std::fs::write(&path, format!("{}\n{old}\n", crate::tracker::events::SCHEMA_HEADER)).unwrap();
        assert_eq!(log::read_for(&path, "linear").unwrap().cursor, Some(at(9)));
        assert_eq!(log::read_for(&path, "github").unwrap().cursor, None);
        let next = fake(vec![]);
        pull_project(vault.path(), &binding(), &next, false, at(12)).unwrap();
        assert_eq!(next.calls.borrow()[0], (Some(at(9) - CURSOR_OVERLAP), false));
    }

    #[test]
    fn pull_during_a_held_lock_fails_with_the_closed_code_after_the_wait() {
        let vault = tempfile::tempdir().unwrap();
        let path = log::path_for(vault.path(), "work", "claims");
        let held = lock::acquire(&path, lock::TEST_FREE_WAIT).unwrap();
        let adapter = fake(vec![snapshot("COR-1", 9)]);
        let started = std::time::Instant::now();
        let error = pull_project_waiting(vault.path(), &binding(), &adapter, Mode::Incremental, at(12), Duration::from_millis(150)).unwrap_err();
        assert!(started.elapsed() >= Duration::from_millis(150));
        assert_eq!(error.code, FailureCode::LockBusy, "{error}");
        assert!(error.to_string().ends_with("(lock_busy)"), "{error}");
        assert!(adapter.calls.borrow().is_empty(), "no provider call while locked");
        assert!(!path.exists());
        drop(held);
        pull_project_waiting(vault.path(), &binding(), &adapter, Mode::Incremental, at(12), lock::TEST_FREE_WAIT).unwrap();
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
        assert!(error.message.contains("could not append"), "{error}");
        assert_eq!(error.code, FailureCode::LogWrite);
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
        let connect = |_: &TrackerBinding, _: Option<&Credential>| -> Result<Box<dyn Adapter>, String> {
            *called.borrow_mut() = true;
            Ok(Box::new(fake(vec![snapshot("COR-1", 9)])))
        };
        let error = pull_binding(vault.path(), config_dir.path(), &binding(), Mode::Incremental, at(12), &connect).unwrap_err();
        assert!(error.message.contains("not configured"), "{error}");
        assert_eq!(error.code, FailureCode::Credential);
        assert!(!*called.borrow(), "no adapter without a credential");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    /// A config dir holding the binding's credential.
    fn credential_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = crate::tracker::credential::path_in(dir.path(), "corr-linear").unwrap();
        crate::tracker::credential::save(&path, "t").unwrap();
        dir
    }

    /// Pull an empty fake through `pull_binding` at `now`.
    fn scheduled_pull(vault: &Path, config: &Path, full: bool, now: DateTime<Utc>) -> PullOutcome {
        let connect = |_: &TrackerBinding, _: Option<&Credential>| -> Result<Box<dyn Adapter>, String> { Ok(Box::new(fake(vec![]))) };
        pull_binding(vault, config, &binding(), if full { Mode::Full } else { Mode::Incremental }, now, &connect).unwrap()
    }

    #[test]
    fn pull_without_a_full_resync_on_record_runs_full() {
        let vault = tempfile::tempdir().unwrap();
        let config = credential_dir();
        let outcome = scheduled_pull(vault.path(), config.path(), false, at(12));
        assert!(outcome.full);
        assert_eq!(outcome.resync_due, Some(ResyncDue::NeverRan));
        let summary = log::read(&log::path_for(vault.path(), "work", "claims")).unwrap();
        assert_eq!(summary.last_full_resync_at, Some(at(12)));
    }

    #[test]
    fn pull_runs_full_once_the_last_full_resync_is_older_than_the_limit() {
        let vault = tempfile::tempdir().unwrap();
        let config = credential_dir();
        scheduled_pull(vault.path(), config.path(), true, at(0));
        let fresh = at(0) + FULL_RESYNC_MAX_AGE - TimeDelta::minutes(1);
        let outcome = scheduled_pull(vault.path(), config.path(), false, fresh);
        assert!(!outcome.full, "within the limit the pull stays incremental");
        assert_eq!(outcome.resync_due, None);

        let stale = at(0) + FULL_RESYNC_MAX_AGE + TimeDelta::minutes(1);
        let outcome = scheduled_pull(vault.path(), config.path(), false, stale);
        assert!(outcome.full);
        assert_eq!(outcome.resync_due, Some(ResyncDue::Stale));
        let summary = log::read(&log::path_for(vault.path(), "work", "claims")).unwrap();
        assert_eq!(summary.last_full_resync_at, Some(stale));
    }

    #[test]
    fn an_incremental_only_pull_never_runs_full_even_when_a_resync_is_due() {
        let vault = tempfile::tempdir().unwrap();
        let config = credential_dir();
        let connect = |_: &TrackerBinding, _: Option<&Credential>| -> Result<Box<dyn Adapter>, String> { Ok(Box::new(fake(vec![snapshot("COR-1", 9)]))) };
        let outcome = pull_binding(vault.path(), config.path(), &binding(), Mode::IncrementalOnly, at(12), &connect).unwrap();
        assert!(!outcome.full);
        assert_eq!(outcome.resync_due, None);
        let summary = log::read(&log::path_for(vault.path(), "work", "claims")).unwrap();
        assert_eq!(summary.last_full_resync_at, None, "no full resync ran");
        assert!(summary.open_issues.contains_key("COR-1"));
        assert_eq!(summary.last_pull_at, Some(at(12)));
    }

    #[test]
    fn a_requested_full_pull_has_no_resync_reason() {
        let vault = tempfile::tempdir().unwrap();
        let config = credential_dir();
        let outcome = scheduled_pull(vault.path(), config.path(), true, at(12));
        assert!(outcome.full);
        assert_eq!(outcome.resync_due, None);
    }

    #[test]
    fn a_stale_resync_reaches_the_adapter_as_a_full_pull() {
        let vault = tempfile::tempdir().unwrap();
        let config = credential_dir();
        let adapter = std::rc::Rc::new(fake(vec![]));
        let shared = adapter.clone();
        struct Shared(std::rc::Rc<FakeAdapter>);
        impl Adapter for Shared {
            fn pull(&self, since: Option<DateTime<Utc>>, full: bool, sink: &mut Sink<'_>) -> Result<(), String> {
                self.0.pull(since, full, sink)
            }
        }
        let connect = move |_: &TrackerBinding, _: Option<&Credential>| -> Result<Box<dyn Adapter>, String> { Ok(Box::new(Shared(shared.clone()))) };
        pull_binding(vault.path(), config.path(), &binding(), Mode::Incremental, at(1), &connect).unwrap();
        assert_eq!(adapter.calls.borrow()[0], (None, true));
    }

    fn kinds(vault: &Path) -> String {
        std::fs::read_to_string(log::path_for(vault, "work", "claims")).unwrap()
    }

    #[test]
    fn an_empty_full_result_with_open_issues_fails_instead_of_removing() {
        let vault = tempfile::tempdir().unwrap();
        let three = fake(vec![snapshot("COR-1", 9), snapshot("COR-2", 9), snapshot("COR-3", 9)]);
        pull_project(vault.path(), &binding(), &three, true, at(10)).unwrap();

        let error = pull_project(vault.path(), &binding(), &fake(vec![]), true, at(11)).unwrap_err();
        assert_eq!(error.code, FailureCode::EmptyFullResult, "{error}");
        let content = kinds(vault.path());
        assert!(!content.contains("\"issue_removed\""), "{content}");
        assert_eq!(content.matches("\"full_resync\"").count(), 1, "only the first full pull: {content}");
        let summary = log::read(&log::path_for(vault.path(), "work", "claims")).unwrap();
        assert_eq!(summary.last_failure, Some((at(11), FailureCode::EmptyFullResult)));
        assert_eq!(summary.open_issues.len(), 3);

        let allowed = pull_project_waiting(vault.path(), &binding(), &fake(vec![]), Mode::FullAllowEmpty, at(12), lock::TEST_FREE_WAIT).unwrap();
        assert_eq!(allowed.removed, 3, "--allow-empty removes them");
        assert!(log::read(&log::path_for(vault.path(), "work", "claims")).unwrap().open_issues.is_empty());
    }

    #[test]
    fn an_empty_full_result_on_an_empty_mirror_is_fine() {
        let vault = tempfile::tempdir().unwrap();
        let outcome = pull_project(vault.path(), &binding(), &fake(vec![]), true, at(10)).unwrap();
        assert!(outcome.full);
    }

    #[test]
    fn an_automatic_full_pull_never_allows_an_empty_result() {
        let vault = tempfile::tempdir().unwrap();
        let config = credential_dir();
        let three = || fake(vec![snapshot("COR-1", 9), snapshot("COR-2", 9), snapshot("COR-3", 9)]);
        let connect = move |_: &TrackerBinding, _: Option<&Credential>| -> Result<Box<dyn Adapter>, String> { Ok(Box::new(three())) };
        pull_binding(vault.path(), config.path(), &binding(), Mode::Incremental, at(10), &connect).unwrap();

        let empty = |_: &TrackerBinding, _: Option<&Credential>| -> Result<Box<dyn Adapter>, String> { Ok(Box::new(fake(vec![]))) };
        let later = at(10) + FULL_RESYNC_MAX_AGE + TimeDelta::hours(1);
        let outcome = pull_binding(vault.path(), config.path(), &binding(), Mode::Incremental, later, &empty).unwrap();
        assert_eq!(outcome.failed_full.map(|e| e.code), Some(FailureCode::EmptyFullResult));
        let content = kinds(vault.path());
        assert!(!content.contains("\"issue_removed\""), "{content}");
        assert!(content.contains("\"code\":\"empty_full_result\""), "{content}");
        assert_eq!(log::read(&log::path_for(vault.path(), "work", "claims")).unwrap().open_issues.len(), 3);
    }

    /// Three issues pulled, then removed by an allowed empty full pull.
    fn removed_three(vault: &Path) {
        let three = fake(vec![snapshot("COR-1", 9), snapshot("COR-2", 9), snapshot("COR-3", 9)]);
        pull_project(vault, &binding(), &three, true, at(10)).unwrap();
        let removed = pull_project_waiting(vault, &binding(), &fake(vec![]), Mode::FullAllowEmpty, at(11), lock::TEST_FREE_WAIT).unwrap();
        assert_eq!(removed.removed, 3);
        assert!(log::read(&log::path_for(vault, "work", "claims")).unwrap().open_issues.is_empty());
    }

    fn restored_ids(vault: &Path) -> Vec<String> {
        let content = kinds(vault);
        content
            .lines()
            .filter_map(|line| serde_json::from_str::<Event>(line).ok())
            .map(|event| event.common().id.clone())
            .filter(|id| id.contains(":restored:"))
            .collect()
    }

    #[test]
    fn a_removed_issue_that_returns_reappears_once() {
        for mode in [Mode::Incremental, Mode::Full] {
            let vault = tempfile::tempdir().unwrap();
            removed_three(vault.path());
            let back = || fake(vec![snapshot("COR-1", 9), snapshot("COR-2", 9), snapshot("COR-3", 9)]);

            let outcome = pull_project_waiting(vault.path(), &binding(), &back(), mode, at(12), lock::TEST_FREE_WAIT).unwrap();
            assert_eq!((outcome.appended, outcome.removed), (3, 0), "{mode:?}");
            let summary = log::read(&log::path_for(vault.path(), "work", "claims")).unwrap();
            assert_eq!(summary.open_issues.keys().collect::<Vec<_>>(), vec!["COR-1", "COR-2", "COR-3"], "{mode:?}");
            let ids = restored_ids(vault.path());
            assert_eq!(ids.len(), 3, "{mode:?}");
            assert!(ids.contains(&format!("linear:issue:COR-1:9:restored:{}", at(11).to_rfc3339())), "{ids:?}");

            for again in [Mode::Incremental, Mode::Full] {
                let outcome = pull_project_waiting(vault.path(), &binding(), &back(), again, at(13), lock::TEST_FREE_WAIT).unwrap();
                assert_eq!((outcome.appended, outcome.removed), (0, 0), "{mode:?} then {again:?}");
            }
            assert_eq!(restored_ids(vault.path()).len(), 3);
        }
    }

    /// Fails every full pull; incremental pulls succeed. Records `full` per call.
    struct FullFails(std::rc::Rc<RefCell<Vec<bool>>>);
    impl Adapter for FullFails {
        fn pull(&self, _: Option<DateTime<Utc>>, full: bool, sink: &mut Sink<'_>) -> Result<(), String> {
            self.0.borrow_mut().push(full);
            sink(vec![snapshot("COR-1", 1)])?;
            match full {
                true => Err("Linear request failed".to_string()),
                false => Ok(()),
            }
        }
    }

    #[test]
    fn a_failed_automatic_full_runs_the_incremental_and_waits_before_trying_again() {
        let vault = tempfile::tempdir().unwrap();
        let config = credential_dir();
        let hour = |h: i64| at(0) + TimeDelta::hours(h);
        let first = fake(vec![snapshot("COR-1", 0)]);
        pull_project(vault.path(), &binding(), &first, true, hour(0)).unwrap();

        let calls = std::rc::Rc::new(RefCell::new(vec![]));
        let shared = calls.clone();
        let connect = move |_: &TrackerBinding, _: Option<&Credential>| -> Result<Box<dyn Adapter>, String> { Ok(Box::new(FullFails(shared.clone()))) };
        let failed = pull_binding(vault.path(), config.path(), &binding(), Mode::Incremental, hour(25), &connect).unwrap();
        assert_eq!(failed.resync_due, Some(ResyncDue::Stale));
        assert!(!failed.full, "the outcome is the incremental pull's");
        assert_eq!(failed.failed_full.as_ref().map(|e| e.code), Some(FailureCode::Provider));
        for h in 26..=29 {
            let outcome = pull_binding(vault.path(), config.path(), &binding(), Mode::Incremental, hour(h), &connect).unwrap();
            assert_eq!((outcome.full, outcome.resync_due, outcome.failed_full), (false, None, None), "hour {h}");
        }
        assert_eq!(*calls.borrow(), vec![true, false, false, false, false, false], "one full attempt, five incrementals");

        let summary = log::read(&log::path_for(vault.path(), "work", "claims")).unwrap();
        assert_eq!(summary.last_pull_at, Some(hour(29)), "incrementals kept the mirror moving");

        // An explicit full is never held back.
        calls.borrow_mut().clear();
        pull_binding(vault.path(), config.path(), &binding(), Mode::Full, hour(30), &connect).unwrap_err();
        assert_eq!(*calls.borrow(), vec![true]);

        // Once the hold since the last automatic failure passes, it tries again.
        calls.borrow_mut().clear();
        let held_until = hour(25) + AUTOMATIC_FULL_RETRY_AFTER;
        pull_binding(vault.path(), config.path(), &binding(), Mode::Incremental, held_until - TimeDelta::minutes(1), &connect).unwrap();
        pull_binding(vault.path(), config.path(), &binding(), Mode::Incremental, held_until + TimeDelta::minutes(1), &connect).unwrap();
        assert_eq!(*calls.borrow(), vec![false, true, false]);
    }

    #[test]
    fn unsupported_provider_is_rejected() {
        let mut b = binding();
        b.provider = "jira".into();
        let credential_dir = tempfile::tempdir().unwrap();
        let path = crate::tracker::credential::path_in(credential_dir.path(), "x").unwrap();
        crate::tracker::credential::save(&path, "t").unwrap();
        let credential = crate::tracker::credential::load(&path).unwrap();
        let error = connect_provider(&b, Some(&credential)).err().unwrap();
        assert!(error.contains("jira"), "{error}");
    }
}
