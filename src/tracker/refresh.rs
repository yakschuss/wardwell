//! The labeled on-miss refresh behind kanban `get`: which binding a missing
//! key belongs to, the per-binding cooldown read from the log, and the
//! closed reason the result carries.
//!
//! Does NOT pull (the caller runs `pull_binding` in `IncrementalOnly` mode)
//! and never applies to list, query, or search.

use crate::config::loader::TrackerBinding;
use crate::tracker::events::FailureCode;
use crate::tracker::view::MirrorView;
use chrono::{DateTime, Utc};
use std::time::Duration;

/// At most one on-miss pull per binding in this window.
pub const COOLDOWN: Duration = Duration::from_secs(60);

/// Why an on-miss lookup did or did not pull, as the `get` result names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    FoundAfterPull,
    StillMissing,
    Cooldown,
    PullFailed(FailureCode),
    NoBinding,
    /// The log has no pull marker, so an incremental pull would read the
    /// whole team. `wardwell tracker pull` seeds it.
    NeverPulled,
}

impl Reason {
    /// The closed label: `found_after_pull`, `still_missing`, `cooldown`,
    /// `pull_failed:<code>`, `no_binding` or `never_pulled`.
    pub fn label(self) -> String {
        match self {
            Self::FoundAfterPull => "found_after_pull".to_string(),
            Self::StillMissing => "still_missing".to_string(),
            Self::Cooldown => "cooldown".to_string(),
            Self::PullFailed(code) => format!("pull_failed:{}", code.as_str()),
            Self::NoBinding => "no_binding".to_string(),
            Self::NeverPulled => "never_pulled".to_string(),
        }
    }

    /// True when a pull completed before the second look.
    pub fn refreshed(self) -> bool {
        matches!(self, Self::FoundAfterPull | Self::StillMissing)
    }
}

/// How long an on-miss pull waits for the project lock before it fails
/// with `lock_busy`. Shorter than a scheduled pull's wait, since a person
/// is waiting on the lookup.
pub const LOCK_WAIT: Duration = Duration::from_secs(2);

/// The binding a missing `key` would come from: the one whose team key
/// equals the key's prefix, ignoring case. The caller narrows `bindings`
/// by project and domain first.
pub fn target<'a>(bindings: &[&'a TrackerBinding], key: &str) -> Option<&'a TrackerBinding> {
    let prefix = key.rsplit_once('-').map(|(team, _)| team)?;
    bindings.iter().copied().find(|b| b.team.eq_ignore_ascii_case(prefix))
}

/// Whether a mirror may be refreshed at `now`: it needs a pull marker,
/// and its newest pull_completed, full_resync or pull_failed marker must be
/// older than `COOLDOWN`.
pub fn check(view: &MirrorView, now: DateTime<Utc>) -> Result<(), Reason> {
    let Some(pulled) = view.last_pull_at else {
        return Err(Reason::NeverPulled);
    };
    let newest = view.last_failure.map_or(pulled, |(failed, _)| failed.max(pulled));
    match (now - newest).to_std().is_ok_and(|age| age >= COOLDOWN) {
        true => Ok(()),
        false => Err(Reason::Cooldown),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn binding(project: &str, team: &str) -> TrackerBinding {
        TrackerBinding {
            domain: "work".into(),
            project: project.into(),
            provider: "linear".into(),
            team: team.into(),
            credential: "c".into(),
            readonly: true,
            gate: false,
            repository: None,
        }
    }

    #[test]
    fn labels_are_the_closed_set() {
        assert_eq!(Reason::FoundAfterPull.label(), "found_after_pull");
        assert_eq!(Reason::StillMissing.label(), "still_missing");
        assert_eq!(Reason::Cooldown.label(), "cooldown");
        assert_eq!(Reason::PullFailed(FailureCode::Auth).label(), "pull_failed:auth");
        assert_eq!(Reason::NoBinding.label(), "no_binding");
        assert_eq!(Reason::NeverPulled.label(), "never_pulled");
        assert!(!Reason::NeverPulled.refreshed());
        assert!(Reason::FoundAfterPull.refreshed() && Reason::StillMissing.refreshed());
        assert!(!Reason::Cooldown.refreshed() && !Reason::NoBinding.refreshed());
        assert!(!Reason::PullFailed(FailureCode::Provider).refreshed());
    }

    #[test]
    fn target_is_the_binding_whose_team_prefixes_the_key() {
        let claims = binding("claims", "COR");
        let ops = binding("ops", "OPS");
        let bindings = [&claims, &ops];
        assert_eq!(target(&bindings, "cor-99"), Some(&claims));
        assert_eq!(target(&bindings, "OPS-1"), Some(&ops));
        assert_eq!(target(&[&ops], "COR-99"), None, "a named project's binding must own the prefix");
        assert_eq!(target(&bindings, "XYZ-1"), None);
        assert_eq!(target(&bindings, "nohyphen"), None);
    }

    #[test]
    fn cooldown_reads_the_newest_pull_marker() {
        use chrono::TimeZone;
        let at = Utc.with_ymd_and_hms(2026, 9, 30, 12, 0, 0).unwrap();
        let secs = chrono::TimeDelta::seconds;
        assert_eq!(check(&MirrorView::default(), at), Err(Reason::NeverPulled));
        let pulled = MirrorView { last_pull_at: Some(at), ..Default::default() };
        assert_eq!(check(&pulled, at + secs(59)), Err(Reason::Cooldown));
        assert_eq!(check(&pulled, at + secs(60)), Ok(()));
        let failed = MirrorView { last_failure: Some((at + secs(30), FailureCode::Provider)), ..pulled };
        assert_eq!(check(&failed, at + secs(80)), Err(Reason::Cooldown), "a failure marker cools down too");
        assert_eq!(check(&failed, at + secs(90)), Ok(()));
        let only_failed = MirrorView { last_failure: Some((at, FailureCode::Auth)), ..Default::default() };
        assert_eq!(check(&only_failed, at + secs(600)), Err(Reason::NeverPulled));
    }
}
