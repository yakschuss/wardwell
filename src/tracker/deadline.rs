//! The hard deadline on a `tracker pull` run. A watchdog thread waits for
//! the deadline; if the run has not finished, it records a pull_failed
//! marker with the code `timeout` for each binding whose pull this process
//! started and did not end, and the caller then stops the process.
//!
//! Does NOT pull, take the project lock, or exit the process itself.

use crate::config::loader::TrackerBinding;
use crate::tracker::events::FailureCode;
use crate::tracker::view::{Attempt, MirrorView};
use crate::tracker::{log, pull, state};
use chrono::{DateTime, Utc};
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

/// A pull run stops after this. Longer than the GitHub read budget
/// (`github::PULL_BUDGET`) and the per-request timeouts, so it only stops
/// a pull that is stuck.
pub const PULL_DEADLINE: Duration = Duration::from_secs(15 * 60);

/// Names a deadline in seconds that replaces `PULL_DEADLINE`, so a test
/// of the built binary can see the watchdog fire.
pub const DEADLINE_VARIABLE: &str = "WARDWELL_PULL_DEADLINE_SECONDS";

/// `PULL_DEADLINE`, or the whole seconds in `DEADLINE_VARIABLE` when they
/// are 1 through 3600. Any other value uses the default.
pub fn pull_deadline() -> Duration {
    deadline_from(std::env::var(DEADLINE_VARIABLE).ok().as_deref())
}

fn deadline_from(value: Option<&str>) -> Duration {
    value.and_then(|v| v.trim().parse::<u64>().ok()).filter(|s| (1..=3600).contains(s)).map_or(PULL_DEADLINE, Duration::from_secs)
}

/// A deadline in words: `15 minutes`, `2 seconds`, `1 second`.
pub fn describe(deadline: Duration) -> String {
    let plural = |n: u64, unit: &str| if n == 1 { format!("1 {unit}") } else { format!("{n} {unit}s") };
    match (deadline.as_secs() >= 60, deadline.as_secs() % 60) {
        (true, 0) => plural(deadline.as_secs() / 60, "minute"),
        _ => plural(deadline.as_secs(), "second"),
    }
}

/// How long the watchdog waits for its timeout markers to be written
/// before the process stops anyway. The write itself may hang on a vault
/// folder that does not answer.
pub const MARKER_WAIT: Duration = Duration::from_secs(5);

/// Armed while it lives. Dropping it disarms the watchdog.
pub struct Watchdog {
    _cancel: Sender<()>,
}

/// Run `on_expiry` on a watchdog thread after `after`, unless the returned
/// guard is dropped first.
pub fn arm(after: Duration, on_expiry: impl FnOnce() + Send + 'static) -> Watchdog {
    let (cancel, cancelled) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        if let Err(RecvTimeoutError::Timeout) = cancelled.recv_timeout(after) {
            on_expiry();
        }
    });
    Watchdog { _cancel: cancel }
}

/// Record `timeout` for each of `bindings` whose open start, in the local
/// refresh state under `config_dir` or in the log, is this process (`pid`).
/// The state is written first, for every such binding, since it lives
/// outside the vault; then a pull_failed marker goes to each log that can
/// be written. Returns the keys of the bindings recorded.
pub fn record_timeouts(vault_root: &Path, config_dir: &Path, bindings: &[TrackerBinding], pid: u32, now: DateTime<Utc>) -> Vec<String> {
    let in_state = record_state_timeouts(config_dir, bindings, pid, now);
    let in_log = record_log_timeouts(vault_root, bindings, pid, now);
    bindings.iter().map(key).filter(|k| in_state.contains(k) || in_log.contains(k)).collect()
}

/// The local-state half of `record_timeouts`: never touches the vault.
pub fn record_state_timeouts(config_dir: &Path, bindings: &[TrackerBinding], pid: u32, now: DateTime<Utc>) -> Vec<String> {
    bindings
        .iter()
        .filter(|binding| {
            let path = state::path(config_dir, &binding.domain, &binding.project);
            let ours = state::provider(&path, &binding.provider).and_then(|s| s.open_start()).is_some_and(|(_, started)| started == Some(pid));
            ours && state::record(&path, &binding.provider, state::Record::Failed(FailureCode::Timeout), now).is_ok()
        })
        .map(key)
        .collect()
}

/// The log half of `record_timeouts`: a marker in each log whose newest
/// attempt is this process's start. It may block on a vault that does not answer.
pub fn record_log_timeouts(vault_root: &Path, bindings: &[TrackerBinding], pid: u32, now: DateTime<Utc>) -> Vec<String> {
    bindings
        .iter()
        .filter(|binding| {
            ours_in_log(vault_root, binding, pid) && {
                let path = log::path_for(vault_root, &binding.domain, &binding.project);
                let marker = pull::pull_failed(binding, now, FailureCode::Timeout, false);
                log::read_for(&path, &binding.provider).and_then(|mut summary| log::append_new(&path, &[marker], &mut summary)).is_ok()
            }
        })
        .map(key)
        .collect()
}

fn key(binding: &TrackerBinding) -> String {
    format!("{} {}", binding.key(), binding.provider)
}

fn ours_in_log(vault_root: &Path, binding: &TrackerBinding, pid: u32) -> bool {
    let path = log::path_for(vault_root, &binding.domain, &binding.project);
    let view = MirrorView::read_for(&path, &binding.provider).unwrap_or_default();
    matches!(view.last_attempt, Some(Attempt::Started { pid: started, .. }) if started == pid)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn binding(project: &str) -> TrackerBinding {
        TrackerBinding {
            domain: "work".into(),
            project: project.into(),
            provider: "linear".into(),
            team: "COR".into(),
            credential: "c".into(),
            readonly: true,
            gate: false,
            repository: None,
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 1, 3, 0, 0).unwrap()
    }

    fn write(vault: &Path, project: &str, rows: &[String]) {
        let path = log::path_for(vault, "work", project);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: String = rows.iter().map(|row| format!("{row}\n")).collect();
        std::fs::write(path, format!("{}\n{body}", crate::tracker::events::SCHEMA_HEADER)).unwrap();
    }

    fn started(pid: u32) -> String {
        format!(r#"{{"kind":"pull_started","id":"s{pid}","provider":"linear","external_key":"COR","external_id":"COR","occurred_at":"2026-10-01T02:33:00Z","title":"s","pid":{pid}}}"#)
    }

    const COMPLETED: &str = r#"{"kind":"pull_completed","id":"p","provider":"linear","external_key":"COR","external_id":"COR","occurred_at":"2026-10-01T02:34:00Z","title":"p"}"#;

    #[test]
    fn the_watchdog_fires_after_the_deadline() {
        let fired = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&fired);
        let _armed = arm(Duration::from_millis(30), move || flag.store(true, Ordering::SeqCst));
        std::thread::sleep(Duration::from_millis(300));
        assert!(fired.load(Ordering::SeqCst));
    }

    #[test]
    fn a_dropped_watchdog_never_fires() {
        let fired = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&fired);
        drop(arm(Duration::from_millis(100), move || flag.store(true, Ordering::SeqCst)));
        std::thread::sleep(Duration::from_millis(300));
        assert!(!fired.load(Ordering::SeqCst));
    }

    #[test]
    fn only_an_unfinished_start_of_this_process_gets_a_timeout_marker() {
        let vault = tempfile::tempdir().unwrap();
        write(vault.path(), "mine", &[started(4242)]);
        write(vault.path(), "other", &[started(7)]);
        write(vault.path(), "done", &[started(4242), COMPLETED.to_string()]);
        let bindings = [binding("mine"), binding("other"), binding("done"), binding("absent")];
        let marked = record_timeouts(vault.path(), &vault.path().join("cfg"), &bindings, 4242, now());
        assert_eq!(marked, vec!["work/mine linear"]);
        let view = MirrorView::read_for(&log::path_for(vault.path(), "work", "mine"), "linear").unwrap();
        assert_eq!(view.last_attempt, Some(Attempt::Failed { at: now(), code: FailureCode::Timeout }));
        assert!(!log::path_for(vault.path(), "work", "absent").exists(), "no log is created");
        assert!(record_timeouts(vault.path(), &vault.path().join("cfg"), &bindings, 4242, now()).is_empty(), "once marked, done");
    }

    #[test]
    fn the_local_state_records_the_timeout_even_when_the_vault_cannot_be_written() {
        let root = tempfile::tempdir().unwrap();
        let config_dir = root.path().join("cfg");
        let vault = root.path().join("vault");
        let state_path = state::path(&config_dir, "work", "mine");
        state::record(&state_path, "linear", state::Record::Started(4242), now() - chrono::TimeDelta::minutes(15)).unwrap();
        std::fs::create_dir_all(&vault).unwrap();
        std::fs::write(vault.join("work"), "a file where the domain folder should be").unwrap();
        let marked = record_timeouts(&vault, &config_dir, &[binding("mine")], 4242, now());
        assert_eq!(marked, vec!["work/mine linear"]);
        assert_eq!(state::provider(&state_path, "linear").unwrap().open_failure(), Some((now(), FailureCode::Timeout)));
    }

    #[test]
    fn the_deadline_reads_its_override_and_says_itself_in_words() {
        assert_eq!(deadline_from(None), PULL_DEADLINE);
        assert_eq!(deadline_from(Some("2")), Duration::from_secs(2));
        assert_eq!(deadline_from(Some("0")), PULL_DEADLINE);
        assert_eq!(deadline_from(Some("soon")), PULL_DEADLINE);
        assert_eq!(deadline_from(Some("1")), Duration::from_secs(1));
        assert_eq!(deadline_from(Some("3600")), Duration::from_secs(3600));
        assert_eq!(deadline_from(Some("3601")), PULL_DEADLINE);
        assert_eq!(deadline_from(Some("18446744073709551615")), PULL_DEADLINE);
        assert_eq!(deadline_from(Some("-5")), PULL_DEADLINE);
        assert_eq!(describe(PULL_DEADLINE), "15 minutes");
        assert_eq!(describe(Duration::from_secs(2)), "2 seconds");
    }

    #[test]
    fn the_deadline_outlasts_the_github_read_budget() {
        assert!(PULL_DEADLINE > crate::tracker::github::PULL_BUDGET);
    }
}
