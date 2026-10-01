//! How fresh one binding's mirror is: the age of the last completed pull,
//! and fresh, stale with one of three reasons, or running since a time.
//! `status`, `tracker doctor`, `doctor` and the session-start section all
//! read it from here, so they say the same words.
//!
//! Does NOT read files or start pulls; callers pass the view.

use crate::tracker::events::FailureCode;
use crate::tracker::view::{Attempt, MirrorView};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};

/// A mirror whose last completed pull is younger than this is fresh.
pub const FRESH_FOR: TimeDelta = TimeDelta::hours(2);

/// A pull_started marker older than this never counts as running, even
/// when a process with its id is alive: the deadline stops every pull
/// before it, so that process is another one with a reused id.
pub fn running_at_most() -> TimeDelta {
    TimeDelta::from_std(crate::tracker::deadline::PULL_DEADLINE).unwrap_or(TimeDelta::minutes(15)) + TimeDelta::minutes(5)
}

/// Why a mirror is stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The last pull failed with this code.
    Failed(FailureCode),
    /// A pull started at this time and did not finish: its process is gone,
    /// or it is older than the deadline allows.
    Unfinished(DateTime<Utc>),
    /// No pull was tried since the last completed one.
    NotTried,
}

impl Reason {
    /// The reason as every surface prints it, without a final period.
    pub fn sentence(self) -> String {
        match self {
            Self::Failed(code) => format!("The last pull failed: {}", code.as_str()),
            Self::Unfinished(at) => format!("A pull started at {} and did not finish", stamp(at)),
            Self::NotTried => "No pull was tried".to_string(),
        }
    }
}

/// Fresh, stale with a reason, or running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Fresh,
    Stale(Reason),
    /// A pull whose process is alive started at this time.
    Running(DateTime<Utc>),
}

/// One binding's freshness at a moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Freshness {
    /// Time since the last completed pull; None when it never completed.
    pub age: Option<TimeDelta>,
    pub state: State,
    /// Set when a pull started after the last completed one and did not
    /// finish, whatever the age, so `status` never calls it clean.
    pub unfinished: Option<DateTime<Utc>>,
}

impl Freshness {
    /// The words every surface uses:
    /// `Last pulled 5h ago. Stale. Reason: No pull was tried.`,
    /// `Last pulled 45m ago.`, or `Last pulled 5h ago; pull running since <time>.`
    pub fn sentence(&self) -> String {
        let pulled = self.age.map_or("Never pulled".to_string(), |age| format!("Last pulled {} ago", short_age(age)));
        match self.state {
            State::Fresh => format!("{pulled}."),
            State::Stale(reason) => format!("{pulled}. Stale. Reason: {}.", reason.sentence()),
            State::Running(since) => format!("{pulled}; pull running since {}.", stamp(since)),
        }
    }

    /// True when the mirror is fresh and no pull is unfinished.
    pub fn is_clean(&self) -> bool {
        self.state == State::Fresh && self.unfinished.is_none()
    }
}

/// The freshness of `view` at `now`. `alive` says whether a process id is
/// running; the real one is `process_alive`.
pub fn assess(view: &MirrorView, now: DateTime<Utc>, alive: &dyn Fn(u32) -> bool) -> Freshness {
    let age = view.last_pull_at.map(|at| now - at);
    let reason = match view.last_attempt {
        Some(Attempt::Started { at, pid }) if is_recent(now - at, running_at_most()) && alive(pid) => {
            return Freshness { age, state: State::Running(at), unfinished: None };
        }
        Some(Attempt::Started { at, .. }) => Reason::Unfinished(at),
        Some(Attempt::Failed { code, .. }) => Reason::Failed(code),
        None => Reason::NotTried,
    };
    let unfinished = match reason {
        Reason::Unfinished(at) => Some(at),
        _ => None,
    };
    let state = match age.is_some_and(|age| is_recent(age, FRESH_FOR)) {
        true => State::Fresh,
        false => State::Stale(reason),
    };
    Freshness { age, state, unfinished }
}

/// An age at least zero and under `limit`. A negative age, from a time
/// stamped in the future, counts as old.
fn is_recent(age: TimeDelta, limit: TimeDelta) -> bool {
    age >= TimeDelta::zero() && age < limit
}

/// Whether a process with id `pid` exists. A signal-0 probe: it sends
/// nothing and only checks that the id is in use.
pub fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill with signal 0 performs only the existence and permission
    // check; it sends no signal and touches no memory.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// An age as the stale line prints it: `45m`, `5h`, `3d`.
pub fn short_age(age: TimeDelta) -> String {
    let minutes = age.num_minutes().max(0);
    match minutes {
        m if m < 60 => format!("{m}m"),
        m if m < 48 * 60 => format!("{}h", m / 60),
        m => format!("{}d", m / (24 * 60)),
    }
}

/// A time as the freshness words print it: UTC to the second.
pub fn stamp(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 1, 7, 33, 0).unwrap()
    }

    fn view(pulled_hours_ago: Option<i64>, attempt: Option<Attempt>) -> MirrorView {
        MirrorView {
            last_pull_at: pulled_hours_ago.map(|h| now() - TimeDelta::hours(h)),
            last_attempt: attempt,
            ..Default::default()
        }
    }

    fn started(minutes_ago: i64) -> Option<Attempt> {
        Some(Attempt::Started { at: now() - TimeDelta::minutes(minutes_ago), pid: 4242 })
    }

    const ALIVE: &dyn Fn(u32) -> bool = &|_| true;
    const GONE: &dyn Fn(u32) -> bool = &|_| false;

    #[test]
    fn under_two_hours_is_fresh() {
        let fresh = assess(&view(Some(1), None), now(), GONE);
        assert_eq!(fresh.state, State::Fresh);
        assert!(fresh.is_clean());
        assert_eq!(fresh.sentence(), "Last pulled 1h ago.");
    }

    #[test]
    fn stale_with_no_pull_tried() {
        let stale = assess(&view(Some(5), None), now(), GONE);
        assert_eq!(stale.state, State::Stale(Reason::NotTried));
        assert_eq!(stale.sentence(), "Last pulled 5h ago. Stale. Reason: No pull was tried.");
        assert_eq!(assess(&view(None, None), now(), GONE).sentence(), "Never pulled. Stale. Reason: No pull was tried.");
    }

    #[test]
    fn stale_with_the_failure_code() {
        let failed = Some(Attempt::Failed { at: now() - TimeDelta::hours(1), code: FailureCode::Timeout });
        let stale = assess(&view(Some(5), failed), now(), GONE);
        assert_eq!(stale.sentence(), "Last pulled 5h ago. Stale. Reason: The last pull failed: timeout.");
        assert!(stale.unfinished.is_none());
    }

    #[test]
    fn a_start_whose_process_is_gone_did_not_finish() {
        let stale = assess(&view(Some(5), started(60)), now(), GONE);
        assert_eq!(stale.state, State::Stale(Reason::Unfinished(now() - TimeDelta::hours(1))));
        assert_eq!(stale.sentence(), "Last pulled 5h ago. Stale. Reason: A pull started at 2026-10-01T06:33:00Z and did not finish.");
        let fresh_but_unfinished = assess(&view(Some(1), started(30)), now(), GONE);
        assert_eq!(fresh_but_unfinished.state, State::Fresh);
        assert!(!fresh_but_unfinished.is_clean(), "status must not call it clean");
    }

    #[test]
    fn a_start_whose_process_is_alive_is_running() {
        let running = assess(&view(Some(5), started(3)), now(), ALIVE);
        assert_eq!(running.state, State::Running(now() - TimeDelta::minutes(3)));
        assert_eq!(running.sentence(), "Last pulled 5h ago; pull running since 2026-10-01T07:30:00Z.");
        assert!(!running.is_clean());
    }

    #[test]
    fn a_start_older_than_the_deadline_is_never_running() {
        let old = assess(&view(Some(5), started(21)), now(), ALIVE);
        assert_eq!(old.state, State::Stale(Reason::Unfinished(now() - TimeDelta::minutes(21))));
    }

    #[test]
    fn a_time_stamped_in_the_future_counts_as_old() {
        let ahead = MirrorView { last_pull_at: Some(now() + TimeDelta::hours(3)), ..Default::default() };
        assert_eq!(assess(&ahead, now(), GONE).state, State::Stale(Reason::NotTried), "a completed pull 3 hours ahead is not fresh");
        let failed_ahead = Some(Attempt::Failed { at: now() + TimeDelta::hours(3), code: FailureCode::Provider });
        assert_eq!(assess(&view(Some(5), failed_ahead), now(), GONE).state, State::Stale(Reason::Failed(FailureCode::Provider)));
        let started_ahead = Some(Attempt::Started { at: now() + TimeDelta::minutes(10), pid: 4242 });
        let fresh = assess(&view(Some(5), started_ahead), now(), ALIVE);
        assert_eq!(fresh.state, State::Stale(Reason::Unfinished(now() + TimeDelta::minutes(10))), "a start 10 minutes ahead is not running");
    }

    #[test]
    fn ages_print_short() {
        assert_eq!(short_age(TimeDelta::minutes(45)), "45m");
        assert_eq!(short_age(TimeDelta::hours(25)), "25h");
        assert_eq!(short_age(TimeDelta::hours(72)), "3d");
        assert_eq!(short_age(TimeDelta::minutes(-3)), "0m");
    }

    #[test]
    fn this_process_is_alive_and_an_unused_id_is_not() {
        assert!(process_alive(std::process::id()));
        assert!(!process_alive(0));
        assert!(!process_alive(u32::MAX));
    }
}
