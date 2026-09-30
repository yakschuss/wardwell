//! The `wardwell tracker` commands as plain functions returning printable
//! lines. Does NOT read stdin or print; main.rs owns the terminal. Never
//! returns or formats a token.

use crate::config::loader::{TrackerBinding, WardwellConfig};
use crate::tracker::pull::{Connect, pull_binding};
use crate::tracker::schedule::{self, LaunchctlRunner};
use crate::tracker::{credential, log};
use chrono::{DateTime, SecondsFormat, Utc};
use std::path::Path;

/// Save a token for credential `name`. Trailing newlines from stdin are dropped.
pub fn connect(config_dir: &Path, name: &str, token: &str) -> Result<String, String> {
    let path = credential::path_in(config_dir, name)?;
    credential::save(&path, token.trim_end_matches(['\r', '\n']))?;
    Ok(format!("Saved tracker credential '{name}' to {}", path.display()))
}

/// Pull every bound project, or only `only`. Returns one line per project;
/// fails with every project's error if any project failed.
pub fn pull(
    config: &WardwellConfig,
    config_dir: &Path,
    only: Option<&str>,
    full: bool,
    now: DateTime<Utc>,
    connect: &Connect<'_>,
) -> Result<Vec<String>, String> {
    let bindings = selected(config, only)?;
    let mut lines = Vec::new();
    let mut failures = Vec::new();
    for (key, binding) in bindings {
        match pull_binding(&config.vault_path, config_dir, binding, full, now, connect) {
            Ok(outcome) => lines.push(format!(
                "{key}: {} pull appended {} events, {} removed",
                if outcome.full { "full" } else { "incremental" },
                outcome.appended,
                outcome.removed,
            )),
            Err(error) => failures.push(format!("{key}: {error}")),
        }
    }
    match failures.is_empty() {
        true => Ok(lines),
        false => Err(lines.into_iter().chain(failures).collect::<Vec<_>>().join("\n")),
    }
}

fn selected<'a>(config: &'a WardwellConfig, only: Option<&str>) -> Result<Vec<(&'a String, &'a TrackerBinding)>, String> {
    match only {
        None => Ok(config.trackers.iter().collect()),
        Some(key) => config
            .trackers
            .get_key_value(key)
            .map(|pair| vec![pair])
            .ok_or_else(|| format!("no tracker is bound to '{key}' in config.yml")),
    }
}

/// One line per bound project: provider, last pull and its age, last full
/// resync, event count, readonly flag; then one line on the pull schedule
/// (`scheduled` is the interval from the installed plist, if any).
pub fn status(config: &WardwellConfig, now: DateTime<Utc>, scheduled: Option<u32>) -> Vec<String> {
    let mut lines = match config.trackers.is_empty() {
        true => vec!["No trackers bound. Add a trackers section to config.yml.".to_string()],
        false => config
            .trackers
            .iter()
            .map(|(key, binding)| status_line(&config.vault_path, key, binding, now))
            .collect(),
    };
    lines.push(schedule_line(scheduled));
    lines
}

fn schedule_line(scheduled: Option<u32>) -> String {
    match scheduled {
        Some(seconds) => format!("hourly pull: scheduled every {seconds} s"),
        None => "hourly pull: not scheduled".to_string(),
    }
}

/// Install the launchd agent that runs `tracker pull` every `interval_seconds`.
pub fn schedule(
    home: &Path,
    config_dir: &Path,
    interval_seconds: u32,
    runner: &dyn LaunchctlRunner,
    current_exe: &Path,
    uid: u32,
) -> Result<String, String> {
    schedule::schedule(home, config_dir, interval_seconds, runner, current_exe, uid)
}

/// Remove the launchd agent.
pub fn unschedule(home: &Path, runner: &dyn LaunchctlRunner, uid: u32) -> Result<String, String> {
    schedule::unschedule(home, runner, uid)
}

fn status_line(vault_root: &Path, key: &str, binding: &TrackerBinding, now: DateTime<Utc>) -> String {
    let mode = if binding.readonly { "readonly" } else { "writable" };
    let head = format!("{key}: {} {} ({mode})", binding.provider, binding.team);
    let path = log::path_for(vault_root, &binding.domain, &binding.project);
    let summary = match log::read(&path) {
        Ok(summary) => summary,
        Err(error) => return format!("{head}, {error}"),
    };
    let Some(pulled) = summary.last_pull_at else {
        return format!("{head}, never pulled");
    };
    let resync = summary.last_full_resync_at.map_or("never".to_string(), stamp);
    format!(
        "{head}, last pull {} ({} ago), last full resync {resync}, {} events",
        stamp(pulled),
        age(now - pulled),
        summary.event_count
    )
}

fn stamp(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn age(delta: chrono::TimeDelta) -> String {
    let minutes = delta.num_minutes().max(0);
    match (minutes / 1440, minutes / 60 % 24, minutes % 60) {
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::tracker::adapter::{Adapter, Sink};
    use chrono::TimeZone;

    struct Empty;
    impl Adapter for Empty {
        fn pull(&self, _: Option<DateTime<Utc>>, _: bool, _: &mut Sink<'_>) -> Result<(), String> {
            Ok(())
        }
    }

    fn fake_connect(_: &TrackerBinding, _: &crate::tracker::credential::Credential) -> Result<Box<dyn Adapter>, String> {
        Ok(Box::new(Empty))
    }

    fn setup(readonly: bool) -> (tempfile::TempDir, WardwellConfig) {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        let yaml = format!(
            "vault_path: {}\nsession_sources: []\ntrackers:\n  work/claims:\n    provider: linear\n    team: COR\n    credential: corr-linear\n    readonly: {readonly}\n",
            vault.display()
        );
        let config_path = dir.path().join("config.yml");
        std::fs::write(&config_path, yaml).unwrap();
        let config = crate::config::loader::load(Some(&config_path)).unwrap();
        (dir, config)
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap()
    }

    #[test]
    fn connect_saves_and_reports_without_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let message = connect(dir.path(), "corr-linear", "lin_api_secret\n").unwrap();
        assert!(message.contains("corr-linear"));
        assert!(!message.contains("lin_api_secret"));
        let path = crate::tracker::credential::path_in(dir.path(), "corr-linear").unwrap();
        assert_eq!(crate::tracker::credential::load(&path).unwrap().token(), "lin_api_secret");
    }

    #[test]
    fn pull_rejects_an_unbound_project() {
        let (dir, config) = setup(false);
        let error = pull(&config, dir.path(), Some("work/nope"), false, now(), &fake_connect).unwrap_err();
        assert!(error.contains("work/nope"), "{error}");
    }

    #[test]
    fn pull_reports_each_project_and_fails_when_any_fails() {
        let (dir, config) = setup(false);
        // No credential yet: the project fails and the run reports failure.
        let error = pull(&config, dir.path(), None, false, now(), &fake_connect).unwrap_err();
        assert!(error.contains("work/claims") && error.contains("not configured"), "{error}");

        connect(dir.path(), "corr-linear", "t").unwrap();
        let lines = pull(&config, dir.path(), Some("work/claims"), true, now(), &fake_connect).unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("work/claims") && lines[0].contains("full"), "{}", lines[0]);
    }

    #[test]
    fn status_reads_the_last_pull_from_a_pull_that_found_nothing() {
        let (dir, config) = setup(false);
        connect(dir.path(), "corr-linear", "t").unwrap();
        pull(&config, dir.path(), None, false, now(), &fake_connect).unwrap();
        let later = now() + chrono::TimeDelta::minutes(5);
        pull(&config, dir.path(), None, false, later, &fake_connect).unwrap();
        let line = &status(&config, later, None)[0];
        assert!(line.contains("last pull 2026-09-01T12:05:00Z (0m ago)"), "{line}");
        assert!(line.contains("last full resync never"), "{line}");
    }

    #[test]
    fn status_shows_pull_age_resync_count_and_readonly() {
        let (dir, config) = setup(true);
        let before = status(&config, now(), None);
        assert!(before[0].contains("never pulled"), "{}", before[0]);

        connect(dir.path(), "corr-linear", "lin_api_secret").unwrap();
        pull(&config, dir.path(), None, true, now(), &fake_connect).unwrap();
        let later = now() + chrono::TimeDelta::minutes(90);
        let lines = status(&config, later, None);
        let line = &lines[0];
        assert!(line.contains("work/claims"), "{line}");
        assert!(line.contains("readonly"), "{line}");
        assert!(line.contains("last pull 2026-09-01T12:00:00Z (1h 30m ago)"), "{line}");
        assert!(line.contains("last full resync 2026-09-01T12:00:00Z"), "{line}");
        assert!(line.contains("1 events"), "{line}");
        assert!(!line.contains("lin_api_secret"));
    }

    #[test]
    fn status_ends_with_the_schedule_line() {
        let (_dir, config) = setup(false);
        assert_eq!(status(&config, now(), None).last().unwrap(), "hourly pull: not scheduled");
        assert_eq!(status(&config, now(), Some(900)).last().unwrap(), "hourly pull: scheduled every 900 s");
    }
}
