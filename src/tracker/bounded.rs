//! Runs work that may block on the vault on a helper thread and waits for it
//! a bounded time. Past the bound the caller goes on; the helper thread is
//! left behind and ends with the process.
//!
//! Does NOT cancel the work: a blocked read cannot be stopped from outside.

use std::time::Duration;

/// How long session start waits for the mirror log, and a refresh for its
/// spawn-failure marker.
pub const VAULT_BOUND: Duration = Duration::from_millis(500);

/// `work`'s result when it finishes within `bound`, else None.
pub fn run<T: Send + 'static>(bound: Duration, work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(work());
    });
    finished.recv_timeout(bound).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn work_within_the_bound_returns_and_past_it_does_not() {
        assert_eq!(run(Duration::from_secs(1), || 7), Some(7));
        let started = std::time::Instant::now();
        assert_eq!(run(Duration::from_millis(50), || std::thread::sleep(Duration::from_secs(5))), None);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
