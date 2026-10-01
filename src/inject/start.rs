//! What `wardwell inject` prints for a working directory: the resolved
//! project's summary, rot line and tracker section, or the domain output as
//! before, or nothing when the directory resolves to neither. For a
//! resolved project it asks `refresh` to start a background pull, and
//! adds one line when one started.
//!
//! Does NOT resolve directories itself (resolve.rs does), decide whether a
//! refresh is due (tracker::trigger does), or pull trackers.

use crate::config::loader::WardwellConfig;
use crate::inject::git::GitDirs;
use crate::inject::resolve::{Resolution, resolve};
use chrono::{DateTime, NaiveDate, Utc};
use std::path::Path;

/// Asked once per session start for the resolved project; true when it
/// started a background refresh.
pub type Refresh<'a> = dyn Fn(&str, &str) -> bool + 'a;

/// The session-start output for `cwd` at `now` and local date `today`.
/// `refresh` runs only for a resolved project with a vault folder.
pub fn output(cwd: &Path, config: &WardwellConfig, config_dir: &Path, git: impl Fn(&Path) -> Option<GitDirs>, now: DateTime<Utc>, today: NaiveDate, refresh: &Refresh<'_>) -> String {
    match resolve(cwd, config, git) {
        Some(Resolution::Project { domain, project }) => {
            let mut out = crate::inject::domain::project_context(config, config_dir, &domain, &project, now, today);
            if !out.is_empty() && refresh(&domain, &project) {
                out.push_str(&format!("  {}\n", crate::tracker::trigger::STARTED_LINE));
            }
            out
        }
        Some(Resolution::Domain(dir)) => crate::inject::domain::domain_context(config, config_dir, &dir, now, today),
        None => String::new(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::inject::git::testing::{git, repo};
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 30, 12, 0, 0).unwrap()
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 30).unwrap()
    }

    /// A vault with `personal/corr-platform` holding state, history and a
    /// decision, a `work` domain with one project, and a config mapping
    /// `mapped` to corr-platform, bound to a tracker when `bound`.
    fn setup(root: &Path, mapped: &Path, bound: bool) -> WardwellConfig {
        let vault = root.join("vault");
        let corr = vault.join("personal/corr-platform");
        std::fs::create_dir_all(&corr).unwrap();
        std::fs::write(corr.join("current_state.md"), "---\nstatus: active\n---\n# corr\n\n## Focus\nShip C1.\n\n## Next Action\nOpen the PR.\n").unwrap();
        std::fs::write(corr.join("history.jsonl"), "{\"date\":\"2026-09-28T10:00:00Z\",\"title\":\"x\"}\n").unwrap();
        std::fs::write(corr.join("decisions.md"), "# Decisions\n\n## 2026-09-20 — Pick\n").unwrap();
        let alpha = vault.join("work/alpha");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::write(alpha.join("current_state.md"), "---\nstatus: active\n---\n# a\n\n## Focus\nAlpha focus.\n").unwrap();
        let mut yaml = format!("vault_path: {}\nsession_sources: []\nprojects:\n  personal/corr-platform:\n    paths:\n      - {}\n", vault.display(), mapped.display());
        if bound {
            yaml.push_str("trackers:\n  personal/corr-platform:\n    provider: linear\n    team: COR\n    credential: c\n");
            let credential = crate::tracker::credential::path_in(root, "c").unwrap();
            crate::tracker::credential::save(&credential, "t").unwrap();
            std::fs::write(
                corr.join("tracker.jsonl"),
                "{\"_schema\":\"tracker\",\"_version\":\"1.0\"}\n{\"kind\":\"pull_completed\",\"id\":\"p\",\"provider\":\"linear\",\"external_key\":\"COR\",\"external_id\":\"COR\",\"occurred_at\":\"2026-09-30T11:00:00Z\",\"title\":\"p\"}\n",
            )
            .unwrap();
        }
        std::fs::write(root.join("config.yml"), yaml).unwrap();
        crate::config::loader::load(Some(&root.join("config.yml"))).unwrap()
    }

    fn none(_: &str, _: &str) -> bool {
        false
    }

    const PROJECT: &str = "**personal/corr-platform** (active): Ship C1.\n  Next: Open the PR.\n  Last history entry 2 days ago. Last decision 10 days ago.\n";
    const TRACKER: &str = "  Tracker mirror. Last pulled 1 hour ago. Not authoritative.\n";

    #[test]
    fn a_mapped_repo_prints_its_summary_rot_line_and_tracker_section() {
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("code/corrtex");
        repo(&code);
        let config = setup(tmp.path(), &code, true);
        let out = output(&code, &config, tmp.path(), crate::inject::git::dirs, now(), today(), &none);
        assert_eq!(out, format!("{PROJECT}{TRACKER}"));
    }

    #[test]
    fn an_unbound_mapped_project_still_prints_its_rot_line() {
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("code/corrtex");
        repo(&code);
        let config = setup(tmp.path(), &code, false);
        assert_eq!(output(&code, &config, tmp.path(), crate::inject::git::dirs, now(), today(), &none), PROJECT);
    }

    #[test]
    fn a_linked_worktree_of_a_mapped_repo_prints_the_same() {
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("code/corrtex");
        repo(&code);
        let linked = tmp.path().join("worktrees/cm-9");
        git(&code, &["worktree", "add", "-q", "-b", "cm-9", linked.to_str().unwrap()]);
        let config = setup(tmp.path(), &code, true);
        let out = output(&linked, &config, tmp.path(), crate::inject::git::dirs, now(), today(), &none);
        assert_eq!(out, format!("{PROJECT}{TRACKER}"));
    }

    #[test]
    fn a_started_refresh_adds_one_line_under_the_project() {
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("code/corrtex");
        repo(&code);
        let config = setup(tmp.path(), &code, true);
        let asked = std::cell::RefCell::new(Vec::new());
        let started = |domain: &str, project: &str| {
            asked.borrow_mut().push(format!("{domain}/{project}"));
            true
        };
        let out = output(&code, &config, tmp.path(), crate::inject::git::dirs, now(), today(), &started);
        assert_eq!(out, format!("{PROJECT}{TRACKER}  Refresh started in the background.\n"));
        assert_eq!(*asked.borrow(), vec!["personal/corr-platform"]);
    }

    #[test]
    fn the_refresh_line_prints_once_for_repeated_session_starts() {
        struct WritesStart(std::path::PathBuf, DateTime<Utc>);
        impl crate::tracker::trigger::Spawner for WritesStart {
            fn spawn(&self, _: &str, _: &[&str]) -> Result<(), String> {
                crate::tracker::state::record(&self.0, "linear", crate::tracker::state::Record::Started(std::process::id()), self.1)
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("code/corrtex");
        repo(&code);
        let config = setup(tmp.path(), &code, true);
        let later = now() + chrono::TimeDelta::hours(1) + chrono::TimeDelta::minutes(1);
        let spawner = WritesStart(crate::tracker::state::path(tmp.path(), "personal", "corr-platform"), later);
        let places = crate::tracker::trigger::Places { config: &config, config_dir: tmp.path() };
        let probes = crate::tracker::trigger::Probes { alive: &crate::tracker::freshness::process_alive, can_pull: &|_| true };
        let refresh = |domain: &str, project: &str| {
            crate::tracker::trigger::refresh(&places, domain, project, later, &spawner, &probes) == crate::tracker::trigger::Outcome::Started
        };
        let first = output(&code, &config, tmp.path(), crate::inject::git::dirs, later, today(), &refresh);
        let second = output(&code, &config, tmp.path(), crate::inject::git::dirs, later, today(), &refresh);
        assert_eq!(first.matches("Refresh started in the background.").count(), 1, "{first}");
        assert!(!second.contains("Refresh started"), "{second}");
    }

    #[test]
    fn a_domain_or_an_unmapped_directory_never_asks_for_a_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("code/corrtex");
        let other = tmp.path().join("code/scratch");
        repo(&other);
        let work = tmp.path().join("code/work");
        std::fs::create_dir_all(&work).unwrap();
        let config = setup(tmp.path(), &code, true);
        let never = |_: &str, _: &str| -> bool { panic!("no refresh outside a resolved project") };
        assert_eq!(output(&other, &config, tmp.path(), crate::inject::git::dirs, now(), today(), &never), "");
        assert!(!output(&work, &config, tmp.path(), crate::inject::git::dirs, now(), today(), &never).is_empty());
    }

    #[test]
    fn an_unmapped_directory_prints_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("code/corrtex");
        let other = tmp.path().join("code/scratch");
        repo(&other);
        let config = setup(tmp.path(), &code, true);
        assert_eq!(output(&other, &config, tmp.path(), crate::inject::git::dirs, now(), today(), &none), "");
    }

    #[test]
    fn a_domain_named_directory_prints_the_domain_output_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("code/corrtex");
        let work = tmp.path().join("code/work");
        std::fs::create_dir_all(&work).unwrap();
        let config = setup(tmp.path(), &code, false);
        let out = output(&work, &config, tmp.path(), crate::inject::git::dirs, now(), today(), &none);
        let domain = crate::inject::domain::domain_context(&config, tmp.path(), &tmp.path().join("vault/work"), now(), today());
        assert_eq!(out, domain);
        assert_eq!(out, "**work/alpha** (active): Alpha focus.\n");
    }

    #[test]
    fn a_mapping_to_a_project_missing_from_the_vault_prints_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("code/corrtex");
        repo(&code);
        let config = setup(tmp.path(), &code, false);
        std::fs::remove_dir_all(tmp.path().join("vault/personal/corr-platform")).unwrap();
        assert_eq!(output(&code, &config, tmp.path(), crate::inject::git::dirs, now(), today(), &none), "");
    }

    #[test]
    fn a_worktree_of_an_unmapped_repo_inside_a_mapped_repo_prints_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("code/corrtex");
        repo(&code);
        let unmapped = tmp.path().join("code/unmapped");
        repo(&unmapped);
        let feature = code.join(".worktrees/feature");
        git(&unmapped, &["worktree", "add", "-q", "-b", "f", feature.to_str().unwrap()]);
        let config = setup(tmp.path(), &code, true);
        assert_eq!(output(&feature, &config, tmp.path(), crate::inject::git::dirs, now(), today(), &none), "");
    }
}
