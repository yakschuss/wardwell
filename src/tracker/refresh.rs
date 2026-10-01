//! The labeled on-miss refresh behind kanban `get`: which binding a missing
//! key belongs to, the per-binding cooldown, and the closed reason the
//! result carries.
//!
//! Does NOT pull (the caller runs `pull_binding` in `IncrementalOnly` mode)
//! and never applies to list, query, or search.

use crate::config::loader::TrackerBinding;
use crate::tracker::events::FailureCode;
use std::collections::HashMap;
use std::time::{Duration, Instant};

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

/// The binding a missing `key` would come from: the named project's when a
/// project is given, else the one whose team key prefixes `key`.
pub fn target<'a>(bindings: &[&'a TrackerBinding], key: &str, project: Option<&str>) -> Option<&'a TrackerBinding> {
    let prefix = key.rsplit_once('-').map(|(team, _)| team)?;
    bindings
        .iter()
        .copied()
        .find(|b| project.map_or_else(|| b.team.eq_ignore_ascii_case(prefix), |name| b.project == name))
}

/// When each binding last started an on-miss pull, held in server memory.
#[derive(Debug, Default)]
pub struct Cooldown {
    started: HashMap<String, Instant>,
}

impl Cooldown {
    /// Record a pull for `binding_key` at `now` and return true, or return
    /// false when one started within `COOLDOWN`.
    pub fn try_start(&mut self, binding_key: &str, now: Instant) -> bool {
        let cooling = self.started.get(binding_key).is_some_and(|at| now.saturating_duration_since(*at) < COOLDOWN);
        if !cooling {
            self.started.insert(binding_key.to_string(), now);
        }
        !cooling
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
    fn target_is_the_named_project_or_the_team_that_prefixes_the_key() {
        let claims = binding("claims", "COR");
        let ops = binding("ops", "OPS");
        let bindings = [&claims, &ops];
        assert_eq!(target(&bindings, "cor-99", None), Some(&claims));
        assert_eq!(target(&bindings, "OPS-1", None), Some(&ops));
        assert_eq!(target(&bindings, "COR-99", Some("ops")), Some(&ops));
        assert_eq!(target(&bindings, "XYZ-1", None), None);
        assert_eq!(target(&bindings, "COR-1", Some("billing")), None);
        assert_eq!(target(&bindings, "nohyphen", None), None);
    }

    #[test]
    fn cooldown_allows_one_pull_per_binding_per_minute() {
        let mut cooldown = Cooldown::default();
        let start = Instant::now();
        assert!(cooldown.try_start("work/claims", start));
        assert!(!cooldown.try_start("work/claims", start + Duration::from_secs(59)));
        assert!(cooldown.try_start("work/ops", start + Duration::from_secs(1)), "per binding");
        assert!(cooldown.try_start("work/claims", start + Duration::from_secs(60)));
        assert!(!cooldown.try_start("work/claims", start + Duration::from_secs(61)), "the window restarts");
    }
}
