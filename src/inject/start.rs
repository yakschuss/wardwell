//! What `wardwell inject` prints for a working directory: the resolved
//! project's summary, rot line and tracker section, or the domain output as
//! before, or nothing when the directory resolves to neither.
//!
//! Does NOT resolve directories itself (resolve.rs does) or pull trackers.

use crate::config::loader::WardwellConfig;
use crate::inject::git::GitDirs;
use crate::inject::resolve::{Resolution, resolve};
use chrono::{DateTime, NaiveDate, Utc};
use std::path::Path;

/// The session-start output for `cwd` at `now` and local date `today`.
pub fn output(cwd: &Path, config: &WardwellConfig, config_dir: &Path, git: impl Fn(&Path) -> Option<GitDirs>, now: DateTime<Utc>, today: NaiveDate) -> String {
    match resolve(cwd, config, git) {
        Some(Resolution::Project { domain, project }) => {
            crate::inject::domain::project_context(config, config_dir, &domain, &project, now, today)
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

    const PROJECT: &str = "**personal/corr-platform** (active): Ship C1.\n  Next: Open the PR.\n  Last history entry 2 days ago. Last decision 10 days ago.\n";
    const TRACKER: &str = "  Tracker mirror. Last pulled 1 hour ago. Not authoritative.\n";

    #[test]
    fn a_mapped_repo_prints_its_summary_rot_line_and_tracker_section() {
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("code/corrtex");
        repo(&code);
        let config = setup(tmp.path(), &code, true);
        let out = output(&code, &config, tmp.path(), crate::inject::git::dirs, now(), today());
        assert_eq!(out, format!("{PROJECT}{TRACKER}"));
    }

    #[test]
    fn an_unbound_mapped_project_still_prints_its_rot_line() {
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("code/corrtex");
        repo(&code);
        let config = setup(tmp.path(), &code, false);
        assert_eq!(output(&code, &config, tmp.path(), crate::inject::git::dirs, now(), today()), PROJECT);
    }

    #[test]
    fn a_linked_worktree_of_a_mapped_repo_prints_the_same() {
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("code/corrtex");
        repo(&code);
        let linked = tmp.path().join("worktrees/cm-9");
        git(&code, &["worktree", "add", "-q", "-b", "cm-9", linked.to_str().unwrap()]);
        let config = setup(tmp.path(), &code, true);
        let out = output(&linked, &config, tmp.path(), crate::inject::git::dirs, now(), today());
        assert_eq!(out, format!("{PROJECT}{TRACKER}"));
    }

    #[test]
    fn an_unmapped_directory_prints_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("code/corrtex");
        let other = tmp.path().join("code/scratch");
        repo(&other);
        let config = setup(tmp.path(), &code, true);
        assert_eq!(output(&other, &config, tmp.path(), crate::inject::git::dirs, now(), today()), "");
    }

    #[test]
    fn a_domain_named_directory_prints_the_domain_output_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("code/corrtex");
        let work = tmp.path().join("code/work");
        std::fs::create_dir_all(&work).unwrap();
        let config = setup(tmp.path(), &code, false);
        let out = output(&work, &config, tmp.path(), crate::inject::git::dirs, now(), today());
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
        assert_eq!(output(&code, &config, tmp.path(), crate::inject::git::dirs, now(), today()), "");
    }
}
