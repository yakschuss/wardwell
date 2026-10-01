//! Per-project lock shared by `tracker pull` and `tracker compact`, so a
//! compaction never renames the log under a running pull. An OS advisory
//! lock on `tracker.lock` beside the log: a crashed holder releases it.
//!
//! Does NOT read or write the log itself.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Lock file beside each project's tracker log. Not `.md` or `.jsonl`, so
/// the indexer never reads it.
pub const LOCK_FILE_NAME: &str = "tracker.lock";

/// Closed code in the error when the lock stays held past the wait.
pub const LOCK_BUSY: &str = "lock_busy";

/// How long a pull or compact waits for the other to finish.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(30);

/// The wait a test uses where it expects the lock to be free. Not zero: the
/// lock is an advisory lock on an open file, and when any test thread spawns
/// a process (git, for one), the child holds a copy of that open file until
/// it execs. A test that drops the lock and takes it again with no wait can
/// lose that window and fail with lock_busy. Zero stays only where a test
/// asserts that it holds the lock itself.
#[cfg(test)]
pub const TEST_FREE_WAIT: Duration = Duration::from_secs(2);

const POLL: Duration = Duration::from_millis(50);

/// Held for as long as it lives; dropping it releases the lock.
#[derive(Debug)]
pub struct ProjectLock {
    _file: File,
}

/// `tracker.lock` in the same folder as `log_path`.
pub fn path_for(log_path: &Path) -> PathBuf {
    log_path.with_file_name(LOCK_FILE_NAME)
}

/// Take the project's lock, waiting up to `wait` for another holder. Fails
/// with an error naming `lock_busy` when the wait runs out.
pub fn acquire(log_path: &Path, wait: Duration) -> Result<ProjectLock, String> {
    let path = path_for(log_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|_| format!("could not create {}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|_| format!("could not open {}", path.display()))?;
    let deadline = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(ProjectLock { _file: file }),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => std::thread::sleep(POLL),
            Err(TryLockError::WouldBlock) => {
                return Err(format!(
                    "{} is held by another tracker pull or compact ({LOCK_BUSY})",
                    path.display()
                ));
            }
            Err(TryLockError::Error(_)) => return Err(format!("could not lock {}", path.display())),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn second_holder_waits_then_fails_with_the_closed_code() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("work/claims/tracker.jsonl");
        let held = acquire(&log, Duration::ZERO).unwrap();
        let started = Instant::now();
        let error = acquire(&log, Duration::from_millis(200)).unwrap_err();
        assert!(started.elapsed() >= Duration::from_millis(200), "bounded wait honoured");
        assert!(error.contains(LOCK_BUSY), "{error}");
        drop(held);
        assert!(acquire(&log, Duration::ZERO).is_ok(), "released on drop");
        assert!(!log.exists(), "the lock never creates the log");
    }
}
