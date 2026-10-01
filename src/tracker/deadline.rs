//! The hard deadline on a `tracker pull` run. A watchdog thread waits for
//! the deadline; if the run has not finished, it records a pull_failed
//! marker with the code `timeout` for each binding whose pull this process
//! started and did not end, and the caller then stops the process.
//!
//! Does NOT pull, take the project lock, or exit the process itself.

use crate::config::loader::TrackerBinding;
use crate::tracker::events::FailureCode;
use crate::tracker::view::{Attempt, MirrorView};
use crate::tracker::{log, pull};
use chrono::{DateTime, Utc};
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

/// A pull run stops after this. Longer than the GitHub read budget
/// (`github::PULL_BUDGET`) and the per-request timeouts, so it only stops
/// a pull that is stuck.
pub const PULL_DEADLINE: Duration = Duration::from_secs(15 * 60);

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

/// Append a `timeout` pull_failed marker for each of `bindings` whose newest
/// attempt is a pull this process (`pid`) started. Returns the keys of the
/// bindings marked. A binding whose log cannot be read or written is skipped.
pub fn record_timeouts(vault_root: &Path, bindings: &[TrackerBinding], pid: u32, now: DateTime<Utc>) -> Vec<String> {
    bindings
        .iter()
        .filter(|binding| ours(vault_root, binding, pid))
        .filter(|binding| {
            let path = log::path_for(vault_root, &binding.domain, &binding.project);
            let marker = pull::pull_failed(binding, now, FailureCode::Timeout, false);
            log::read_for(&path, &binding.provider).and_then(|mut summary| log::append_new(&path, &[marker], &mut summary)).is_ok()
        })
        .map(|binding| format!("{} {}", binding.key(), binding.provider))
        .collect()
}

fn ours(vault_root: &Path, binding: &TrackerBinding, pid: u32) -> bool {
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
        let marked = record_timeouts(vault.path(), &bindings, 4242, now());
        assert_eq!(marked, vec!["work/mine linear"]);
        let view = MirrorView::read_for(&log::path_for(vault.path(), "work", "mine"), "linear").unwrap();
        assert_eq!(view.last_attempt, Some(Attempt::Failed { at: now(), code: FailureCode::Timeout }));
        assert!(!log::path_for(vault.path(), "work", "absent").exists(), "no log is created");
        assert!(record_timeouts(vault.path(), &bindings, 4242, now()).is_empty(), "once marked, done");
    }

    #[test]
    fn the_deadline_outlasts_the_github_read_budget() {
        assert!(PULL_DEADLINE > crate::tracker::github::PULL_BUDGET);
    }
}
