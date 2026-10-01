//! The `wardwell tracker` commands as plain functions returning printable
//! lines. Does NOT read stdin or print; main.rs owns the terminal. Never
//! returns or formats a token.

use crate::config::loader::{TrackerBinding, WardwellConfig};
use crate::tracker::pull::{Connect, Mode, pull_binding};
use crate::tracker::schedule::{self, LaunchctlRunner};
use crate::tracker::events::FailureCode;
use crate::tracker::{compact, credential, freshness, lock, log};
use chrono::{DateTime, SecondsFormat, Utc};
use std::path::Path;

/// Save a token for credential `name`. Trailing newlines from stdin are dropped.
pub fn connect(config_dir: &Path, name: &str, token: &str) -> Result<String, String> {
    let path = credential::path_in(config_dir, name)?;
    credential::save(&path, token.trim_end_matches(['\r', '\n']))?;
    Ok(format!("Saved tracker credential '{name}' to {}", path.display()))
}

/// Pull every bound project, or only `only`. A project that fails does not
/// stop the others. Returns one line per project; fails with every
/// project's line, the errors ending in their closed code, if any failed.
pub fn pull(
    config: &WardwellConfig,
    config_dir: &Path,
    only: Option<&str>,
    mode: Mode,
    now: DateTime<Utc>,
    connect: &Connect<'_>,
) -> Result<Vec<String>, String> {
    pull_providers(config, config_dir, only, &[], mode, now, connect)
}

/// `pull`, limited to the bindings whose provider is in `providers` when it
/// is not empty. A background refresh passes the providers that are due, so
/// a provider held after a failure is not pulled with its healthy sibling.
pub fn pull_providers(
    config: &WardwellConfig,
    config_dir: &Path,
    only: Option<&str>,
    providers: &[String],
    mode: Mode,
    now: DateTime<Utc>,
    connect: &Connect<'_>,
) -> Result<Vec<String>, String> {
    let mut bindings = selected(config, only)?;
    bindings.retain(|(_, b)| providers.is_empty() || providers.contains(&b.provider));
    let mut lines = Vec::new();
    let mut failures = Vec::new();
    for (key, binding) in &bindings {
        let binding = *binding;
        match pull_binding(&config.vault_path, config_dir, binding, mode, now, connect) {
            Ok(outcome) => match (&outcome.failed_full, outcome.resync_due) {
                (Some(failed), Some(due)) => failures.push(format!(
                    "{key}: automatic full pull failed ({}): {failed}; {}",
                    due.describe(),
                    pull_summary(&outcome)
                )),
                _ => lines.push(format!("{key}: {}", pull_summary(&outcome))),
            },
            Err(error) => failures.push(format!("{key}: {error}")),
        }
    }
    release_claims(config_dir, &bindings);
    match failures.is_empty() {
        true => Ok(lines),
        false => Err(lines.into_iter().chain(failures).collect::<Vec<_>>().join("\n")),
    }
}

/// Remove the refresh claim of each project pulled, so the next due
/// refresh can start.
pub fn release_claims(config_dir: &Path, bindings: &[(String, &TrackerBinding)]) {
    for (_, binding) in bindings {
        crate::tracker::state::release(&crate::tracker::state::claim_path(config_dir, &binding.domain, &binding.project));
    }
}

fn pull_summary(outcome: &crate::tracker::pull::PullOutcome) -> String {
    if outcome.skipped {
        return "skipped, a pull that completed while this one waited already refreshed it".to_string();
    }
    let mode = match (outcome.full, outcome.resync_due) {
        (true, Some(due)) => format!("full pull ({}, so this pull ran full)", due.describe()),
        (true, None) => "full pull".to_string(),
        (false, _) => "incremental pull".to_string(),
    };
    format!("{mode} appended {} events, {} removed", outcome.appended, outcome.removed)
}

/// Compaction drops the rewritten log's vectors; the watcher re-adds its
/// text without them, and a server start does not re-embed an unchanged
/// file. Only a reindex restores them.
const SEARCH_BY_MEANING_RETURNS: &str = "Search by meaning returns for this log after you run `wardwell reindex`.";

/// Compact every bound project's log, or only `only`. One line per
/// project, however many bindings share its log; fails with every
/// project's error if any project failed.
pub fn compact(config: &WardwellConfig, only: Option<&str>, force: bool) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    let mut failures = Vec::new();
    let mut projects = selected(config, only)?;
    projects.dedup_by(|a, b| a.0 == b.0);
    for (key, binding) in projects {
        let path = log::path_for(&config.vault_path, &binding.domain, &binding.project);
        match compact::compact(&path, force, lock::DEFAULT_WAIT) {
            Ok(outcome) if !outcome.changed => lines.push(format!("{key}: already compact, {} events", outcome.events)),
            Ok(outcome) => lines.push(format!(
                "{key}: compacted to {} events, moved {} raw payloads to {}, removed {} duplicates, backup at {}. {SEARCH_BY_MEANING_RETURNS}",
                outcome.events,
                outcome.moved_raw,
                crate::tracker::events::RAW_FILE_NAME,
                outcome.duplicates_removed,
                outcome.backup.as_deref().map(|p| p.display().to_string()).unwrap_or_default(),
            )),
            Err(error) => failures.push(format!("{key}: {error}")),
        }
    }
    match failures.is_empty() {
        true => Ok(lines),
        false => Err(lines.into_iter().chain(failures).collect::<Vec<_>>().join("\n")),
    }
}

/// Credential, auth and team or repository checks for every binding, one
/// line each. Fails with every line when any check failed.
pub fn doctor(
    config: &WardwellConfig,
    config_dir: &Path,
    probe: &crate::tracker::doctor::Probe<'_>,
    github: &crate::tracker::doctor::GithubProbe<'_>,
) -> Result<Vec<String>, String> {
    match crate::tracker::doctor::run(config, config_dir, probe, github) {
        (lines, true) => Ok(lines),
        (lines, false) => Err(lines.join("\n")),
    }
}

/// Each selected binding with its `<domain>/<project>` key: every binding,
/// or every binding of the project `only`.
fn selected<'a>(config: &'a WardwellConfig, only: Option<&str>) -> Result<Vec<(String, &'a TrackerBinding)>, String> {
    let chosen: Vec<(String, &TrackerBinding)> =
        config.trackers.iter().map(|b| (b.key(), b)).filter(|(key, _)| only.is_none_or(|only| key == only)).collect();
    match (only, chosen.is_empty()) {
        (Some(key), true) => Err(format!("no tracker is bound to '{key}' in config.yml")),
        _ => Ok(chosen),
    }
}

/// One line per binding: provider, team or repository, last pull and its
/// age, last full resync, event count, readonly flag for an issue tracker,
/// and the closed code when the binding cannot pull or its last pull
/// failed, each read from that provider's own events; then one line on the
/// pull schedule (`scheduled` is the interval from the installed plist, if any).
pub fn status(config: &WardwellConfig, config_dir: &Path, now: DateTime<Utc>, scheduled: Option<u32>) -> Vec<String> {
    status_with(config, config_dir, now, scheduled, crate::tracker::github::gh_available_for(&config.trackers))
}

/// `status`, told whether a `gh` was found.
fn status_with(config: &WardwellConfig, config_dir: &Path, now: DateTime<Utc>, scheduled: Option<u32>, gh_on_path: bool) -> Vec<String> {
    let mut lines = match config.trackers.is_empty() {
        true => vec!["No trackers bound. Add a trackers section to config.yml.".to_string()],
        false => config
            .trackers
            .iter()
            .map(|binding| status_line(&config.vault_path, config_dir, &binding.key(), binding, now, gh_on_path))
            .collect(),
    };
    lines.push(schedule_line(scheduled));
    lines
}

fn schedule_line(scheduled: Option<u32>) -> String {
    match scheduled {
        Some(seconds) => format!("pull schedule: every {seconds} s (plist on disk)"),
        None => "pull schedule: no launchd agent; session start and the running server refresh a mirror over an hour old".to_string(),
    }
}

/// Install the launchd agent that runs `tracker pull` every `interval_seconds`.
/// For a vault under a folder macOS protects, the first line is the
/// sentence that says why the session refresh is the better choice.
pub fn schedule(
    home: &Path,
    config_dir: &Path,
    vault: Option<&Path>,
    interval_seconds: u32,
    runner: &dyn LaunchctlRunner,
    current_exe: &Path,
    uid: u32,
) -> Result<Vec<String>, String> {
    let installed = schedule::schedule(home, config_dir, interval_seconds, runner, current_exe, uid)?;
    let warning = vault.filter(|vault| schedule::is_protected(vault, home)).map(|_| schedule::PROTECTED_SENTENCE.to_string());
    Ok(warning.into_iter().chain([installed]).collect())
}

/// Remove the launchd agent.
pub fn unschedule(home: &Path, runner: &dyn LaunchctlRunner, uid: u32) -> Result<String, String> {
    schedule::unschedule(home, runner, uid)
}

/// Read-only check that a pull could start: a known provider and a
/// readable credential, or for github a `gh` that `locate_gh` finds. Never opens the network
/// and never runs `gh`.
fn cannot_pull(config_dir: &Path, binding: &TrackerBinding, gh_on_path: bool) -> Option<FailureCode> {
    if !crate::tracker::SUPPORTED_PROVIDERS.contains(&binding.provider.as_str()) {
        return Some(FailureCode::UnsupportedProvider);
    }
    crate::tracker::doctor::check_offline_with(config_dir, binding, gh_on_path).err().map(|(code, _)| code)
}

fn status_line(vault_root: &Path, config_dir: &Path, key: &str, binding: &TrackerBinding, now: DateTime<Utc>, gh_on_path: bool) -> String {
    let mode = match (crate::tracker::mirrors_issues(&binding.provider), binding.readonly) {
        (false, _) => String::new(),
        (true, true) => " (readonly)".to_string(),
        (true, false) => " (writable)".to_string(),
    };
    let head = format!("{key}: {} {}{mode}", binding.provider, binding.scope());
    let path = log::path_for(vault_root, &binding.domain, &binding.project);
    let summary = match log::read_for(&path, &binding.provider) {
        Ok(summary) => summary,
        Err(error) => return format!("{head}, {error}"),
    };
    let view = crate::tracker::view::MirrorView::read_for(&path, &binding.provider).unwrap_or_default();
    let fresh = freshness::assess(&view, now, &freshness::process_alive);
    let blocked = cannot_pull(config_dir, binding, gh_on_path).map(|code| format!(", cannot pull ({})", code.as_str()));
    let last = summary.last_failure.map(|(at, code)| format!(", last error {} at {}", code.as_str(), stamp(at)));
    let problems = format!("{}{}{}", blocked.unwrap_or_default(), last.unwrap_or_default(), freshness_tail(&fresh));
    let failure = match problems.is_empty() {
        true => ", no errors".to_string(),
        false => problems,
    };
    let Some(pulled) = summary.last_pull_at else {
        return format!("{head}, never pulled{failure}");
    };
    let resync = summary.last_full_resync_at.map_or("never".to_string(), stamp);
    format!(
        "{head}, last pull {} ({} ago), last full resync {resync}, {} events{failure}",
        stamp(pulled),
        age(now - pulled),
        summary.event_count
    )
}

/// What the status line adds for a mirror that is not fresh and clean:
/// the stale words and reason, the running pull, or a pull that did not
/// finish. Empty when the mirror is fresh and clean.
fn freshness_tail(fresh: &freshness::Freshness) -> String {
    match (fresh.state, fresh.unfinished) {
        (freshness::State::Stale(reason), _) => format!(". Stale. Reason: {}.", reason.sentence()),
        (freshness::State::Running(since), _) => format!(", pull running since {}", stamp(since)),
        (freshness::State::Unreadable(code), _) => format!(". Could not read the mirror log: {}.", code.as_str()),
        (freshness::State::Fresh, Some(at)) => format!(", a pull started at {} and did not finish", stamp(at)),
        (freshness::State::Fresh, None) => String::new(),
    }
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

    fn fake_connect(_: &TrackerBinding, _: Option<&crate::tracker::credential::Credential>) -> Result<Box<dyn Adapter>, String> {
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
    fn a_pull_releases_the_project_claim_on_success_and_on_failure() {
        let (dir, config) = setup(false);
        let claim = crate::tracker::state::claim_path(dir.path(), "work", "claims");
        assert!(crate::tracker::state::claim(&claim, Utc::now()));
        pull(&config, dir.path(), None, Mode::Incremental, now(), &fake_connect).unwrap_err();
        assert!(!claim.exists(), "released after a failed pull");
        connect(dir.path(), "corr-linear", "t").unwrap();
        assert!(crate::tracker::state::claim(&claim, Utc::now()));
        pull(&config, dir.path(), None, Mode::Incremental, now(), &fake_connect).unwrap();
        assert!(!claim.exists(), "released after a completed pull");
    }

    #[test]
    fn a_provider_filter_pulls_only_those_providers_and_a_manual_pull_ignores_the_hold() {
        let (dir, config) = linear_and_github();
        let state_path = crate::tracker::state::path(dir.path(), "work", "claims");
        crate::tracker::state::record(&state_path, "linear", crate::tracker::state::Record::Failed(FailureCode::Provider), Utc::now()).unwrap();
        let lines = pull_providers(&config, dir.path(), Some("work/claims"), &["github".to_string()], Mode::Incremental, now(), &fake_connect).unwrap();
        assert_eq!(lines.len(), 1, "{lines:?}");
        let path = log::path_for(&config.vault_path, "work", "claims");
        assert_eq!(log::read_for(&path, "linear").unwrap().last_pull_at, None, "linear was not pulled");
        let manual = pull(&config, dir.path(), Some("work/claims"), Mode::Incremental, now(), &fake_connect).unwrap();
        assert_eq!(manual.len(), 2, "a manual pull ignores the hold: {manual:?}");
    }

    #[test]
    fn pull_rejects_an_unbound_project() {
        let (dir, config) = setup(false);
        let error = pull(&config, dir.path(), Some("work/nope"), Mode::Incremental, now(), &fake_connect).unwrap_err();
        assert!(error.contains("work/nope"), "{error}");
    }

    #[test]
    fn pull_reports_each_project_and_fails_when_any_fails() {
        let (dir, config) = setup(false);
        // No credential yet: the project fails and the run reports failure.
        let error = pull(&config, dir.path(), None, Mode::Incremental, now(), &fake_connect).unwrap_err();
        assert!(error.contains("work/claims") && error.contains("not configured"), "{error}");

        connect(dir.path(), "corr-linear", "t").unwrap();
        let lines = pull(&config, dir.path(), Some("work/claims"), Mode::Full, now(), &fake_connect).unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("work/claims") && lines[0].contains("full"), "{}", lines[0]);
    }

    #[test]
    fn pull_line_says_when_a_due_full_resync_made_the_pull_full() {
        let (dir, config) = setup(false);
        connect(dir.path(), "corr-linear", "t").unwrap();
        let first = pull(&config, dir.path(), None, Mode::Incremental, now(), &fake_connect).unwrap();
        assert_eq!(first, vec!["work/claims: full pull (no full resync on record, so this pull ran full) appended 0 events, 0 removed"]);
        let soon = now() + chrono::TimeDelta::hours(1);
        let second = pull(&config, dir.path(), None, Mode::Incremental, soon, &fake_connect).unwrap();
        assert_eq!(second, vec!["work/claims: incremental pull appended 0 events, 0 removed"]);
        let day_later = soon + crate::tracker::pull::FULL_RESYNC_MAX_AGE;
        let third = pull(&config, dir.path(), None, Mode::Incremental, day_later, &fake_connect).unwrap();
        assert_eq!(third, vec!["work/claims: full pull (last full resync over 24 hours ago, so this pull ran full) appended 0 events, 0 removed"]);
        let asked = pull(&config, dir.path(), None, Mode::Full, day_later, &fake_connect).unwrap();
        assert_eq!(asked, vec!["work/claims: full pull appended 0 events, 0 removed"]);
    }

    #[test]
    fn a_failed_automatic_full_reports_both_pulls_and_fails_the_run() {
        struct FullFails;
        impl Adapter for FullFails {
            fn pull(&self, _: Option<DateTime<Utc>>, full: bool, _: &mut Sink<'_>) -> Result<(), String> {
                match full {
                    true => Err("Linear request failed".to_string()),
                    false => Ok(()),
                }
            }
        }
        let full_fails = |_: &TrackerBinding, _: Option<&crate::tracker::credential::Credential>| -> Result<Box<dyn Adapter>, String> { Ok(Box::new(FullFails)) };
        let (dir, config) = setup(false);
        connect(dir.path(), "corr-linear", "t").unwrap();
        let error = pull(&config, dir.path(), None, Mode::Incremental, now(), &full_fails).unwrap_err();
        assert_eq!(
            error,
            "work/claims: automatic full pull failed (no full resync on record): Linear request failed (provider); incremental pull appended 0 events, 0 removed"
        );
        let next = pull(&config, dir.path(), None, Mode::Incremental, now() + chrono::TimeDelta::hours(1), &full_fails).unwrap();
        assert_eq!(next, vec!["work/claims: incremental pull appended 0 events, 0 removed"]);
    }

    #[test]
    fn status_reads_the_last_pull_from_a_pull_that_found_nothing() {
        let (dir, config) = setup(false);
        connect(dir.path(), "corr-linear", "t").unwrap();
        pull(&config, dir.path(), None, Mode::Incremental, now(), &fake_connect).unwrap();
        let later = now() + chrono::TimeDelta::minutes(5);
        pull(&config, dir.path(), None, Mode::Incremental, later, &fake_connect).unwrap();
        let line = &status(&config, dir.path(), later, None)[0];
        assert!(line.contains("last pull 2026-09-01T12:05:00Z (0m ago)"), "{line}");
        assert!(line.contains("last full resync 2026-09-01T12:00:00Z"), "the first pull runs full: {line}");
    }

    #[test]
    fn status_shows_pull_age_resync_count_and_readonly() {
        let (dir, config) = setup(true);
        let before = status(&config, dir.path(), now(), None);
        assert!(before[0].contains("never pulled"), "{}", before[0]);

        connect(dir.path(), "corr-linear", "lin_api_secret").unwrap();
        pull(&config, dir.path(), None, Mode::Full, now(), &fake_connect).unwrap();
        let later = now() + chrono::TimeDelta::minutes(90);
        let lines = status(&config, dir.path(), later, None);
        let line = &lines[0];
        assert!(line.contains("work/claims"), "{line}");
        assert!(line.contains("readonly"), "{line}");
        assert!(line.contains("last pull 2026-09-01T12:00:00Z (1h 30m ago)"), "{line}");
        assert!(line.contains("last full resync 2026-09-01T12:00:00Z"), "{line}");
        assert!(line.contains("2 events"), "the pull_started and full_resync markers: {line}");
        assert!(!line.contains("lin_api_secret"));
    }

    #[test]
    fn compact_reports_each_project_and_is_idempotent() {
        let (dir, config) = setup(false);
        let path = log::path_for(&config.vault_path, "work", "claims");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let row = r#"{"kind":"pull_completed","id":"p1","provider":"linear","external_key":"COR","external_id":"COR","occurred_at":"2026-09-01T12:00:00Z","title":"COR pull","raw":null}"#;
        std::fs::write(&path, format!("{}\n{row}\n{row}\n", crate::tracker::events::SCHEMA_HEADER)).unwrap();

        let lines = compact(&config, None, false).unwrap();
        assert!(lines[0].starts_with("work/claims: compacted to 1 events"), "{}", lines[0]);
        assert!(lines[0].contains("removed 1 duplicates"), "{}", lines[0]);
        assert!(
            lines[0].ends_with(". Search by meaning returns for this log after you run `wardwell reindex`."),
            "{}",
            lines[0]
        );
        assert_eq!(compact(&config, Some("work/claims"), false).unwrap(), vec!["work/claims: already compact, 1 events"]);
        assert!(compact(&config, Some("work/nope"), false).unwrap_err().contains("work/nope"));
        drop(dir);
    }

    /// Fails every pull with provider text that must never reach the vault.
    struct Broken;
    impl Adapter for Broken {
        fn pull(&self, _: Option<DateTime<Utc>>, _: bool, _: &mut Sink<'_>) -> Result<(), String> {
            Err("Linear returned HTTP 500".to_string())
        }
    }

    fn two_bindings() -> (tempfile::TempDir, WardwellConfig) {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        let yaml = format!(
            "vault_path: {}\nsession_sources: []\ntrackers:\n  work/claims:\n    provider: linear\n    team: COR\n    credential: corr-linear\n  work/ops:\n    provider: linear\n    team: OPS\n    credential: corr-linear\n",
            vault.display()
        );
        let config_path = dir.path().join("config.yml");
        std::fs::write(&config_path, yaml).unwrap();
        let config = crate::config::loader::load(Some(&config_path)).unwrap();
        connect(dir.path(), "corr-linear", "t").unwrap();
        (dir, config)
    }

    fn claims_breaks(binding: &TrackerBinding, _: Option<&crate::tracker::credential::Credential>) -> Result<Box<dyn Adapter>, String> {
        match binding.team.as_str() {
            "COR" => Ok(Box::new(Broken)),
            _ => Ok(Box::new(Empty)),
        }
    }

    #[test]
    fn one_failing_binding_does_not_stop_the_others_and_fails_the_run() {
        let (dir, config) = two_bindings();
        let error = pull(&config, dir.path(), None, Mode::Incremental, now(), &claims_breaks).unwrap_err();
        let lines: Vec<&str> = error.lines().collect();
        assert_eq!(lines.len(), 2, "{error}");
        assert!(lines[0].starts_with("work/ops: full pull (no full resync on record, so this pull ran full) appended"), "{error}");
        assert_eq!(
            lines[1],
            "work/claims: automatic full pull failed: Linear returned HTTP 500 (provider); incremental pull: Linear returned HTTP 500 (provider)"
        );
        let ops = log::read(&log::path_for(&config.vault_path, "work", "ops")).unwrap();
        assert_eq!(ops.last_pull_at, Some(now()));
        let claims = log::read(&log::path_for(&config.vault_path, "work", "claims")).unwrap();
        assert_eq!(claims.last_failure.map(|(_, code)| code.as_str()), Some("provider"));
    }

    /// `work/claims` bound to Linear and to GitHub, with the Linear token stored.
    fn linear_and_github() -> (tempfile::TempDir, WardwellConfig) {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        let yaml = format!(
            "vault_path: {}\nsession_sources: []\ntrackers:\n  work/claims:\n    - provider: linear\n      team: COR\n      credential: corr-linear\n    - provider: github\n      repository: acme/app\n",
            vault.display()
        );
        let config_path = dir.path().join("config.yml");
        std::fs::write(&config_path, yaml).unwrap();
        let config = crate::config::loader::load(Some(&config_path)).unwrap();
        connect(dir.path(), "corr-linear", "t").unwrap();
        (dir, config)
    }

    #[test]
    fn a_failing_provider_does_not_stop_the_other_and_fails_the_run() {
        for broken in ["github", "linear"] {
            let (dir, config) = linear_and_github();
            let connect = move |binding: &TrackerBinding, _: Option<&crate::tracker::credential::Credential>| -> Result<Box<dyn Adapter>, String> {
                match binding.provider == broken {
                    true => Ok(Box::new(Broken)),
                    false => Ok(Box::new(Empty)),
                }
            };
            let error = pull(&config, dir.path(), None, Mode::Incremental, now(), &connect).unwrap_err();
            let lines: Vec<&str> = error.lines().collect();
            assert_eq!(lines.len(), 2, "{error}");
            assert!(lines[0].starts_with("work/claims: ") && lines[0].contains("appended 0 events"), "the healthy binding pulled: {error}");
            assert!(lines[1].starts_with("work/claims: ") && lines[1].ends_with("(provider)"), "{error}");
            let path = log::path_for(&config.vault_path, "work", "claims");
            let healthy = if broken == "github" { "linear" } else { "github" };
            assert_eq!(log::read_for(&path, healthy).unwrap().last_pull_at, Some(now()), "{broken} broken");
            assert_eq!(log::read_for(&path, healthy).unwrap().last_failure, None);
            assert_eq!(log::read_for(&path, broken).unwrap().last_failure.map(|(_, c)| c), Some(FailureCode::Provider));
        }
    }

    #[test]
    fn compact_runs_once_for_a_project_with_two_bindings() {
        let (dir, config) = linear_and_github();
        pull(&config, dir.path(), None, Mode::Incremental, now(), &fake_connect).unwrap();
        let lines = compact(&config, None, false).unwrap();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("work/claims: already compact"), "{lines:?}");
        assert_eq!(compact(&config, Some("work/claims"), false).unwrap().len(), 1);
    }

    #[test]
    fn status_lists_each_binding_of_a_project_from_its_own_events() {
        let (dir, config) = linear_and_github();
        pull(&config, dir.path(), None, Mode::Incremental, now(), &fake_connect).unwrap();
        let later = now() + chrono::TimeDelta::minutes(10);
        let github_breaks = |binding: &TrackerBinding, _: Option<&crate::tracker::credential::Credential>| -> Result<Box<dyn Adapter>, String> {
            match binding.provider.as_str() {
                "github" => Ok(Box::new(Broken)),
                _ => Ok(Box::new(Empty)),
            }
        };
        pull(&config, dir.path(), None, Mode::Incremental, later, &github_breaks).unwrap_err();
        let lines = status_with(&config, dir.path(), later, None, false);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].starts_with("work/claims: linear COR (writable), last pull 2026-09-01T12:10:00Z"), "{}", lines[0]);
        assert!(lines[0].ends_with("no errors"), "{}", lines[0]);
        assert!(lines[1].starts_with("work/claims: github acme/app, last pull 2026-09-01T12:00:00Z (10m ago), last full resync never, 4 events"), "two starts, a completion, a failure: {}", lines[1]);
        assert!(lines[1].ends_with(", cannot pull (credential), last error provider at 2026-09-01T12:10:00Z"), "{}", lines[1]);
        let reachable = status_with(&config, dir.path(), later, None, true);
        assert!(reachable[1].ends_with("10m ago), last full resync never, 4 events, last error provider at 2026-09-01T12:10:00Z"), "{}", reachable[1]);
    }

    #[test]
    fn status_lists_every_binding_with_its_last_error() {
        let (dir, config) = two_bindings();
        pull(&config, dir.path(), None, Mode::Incremental, now(), &fake_connect).unwrap();
        let later = now() + chrono::TimeDelta::minutes(10);
        pull(&config, dir.path(), None, Mode::Incremental, later, &claims_breaks).unwrap_err();
        let lines = status(&config, dir.path(), later, None);
        assert_eq!(lines.len(), 3, "{lines:?}");
        let claims = lines.iter().find(|l| l.starts_with("work/claims")).unwrap();
        assert!(claims.contains("last pull 2026-09-01T12:00:00Z"), "{claims}");
        assert!(claims.ends_with("last error provider at 2026-09-01T12:10:00Z"), "{claims}");
        assert!(!claims.contains("HTTP 500"), "{claims}");
        let ops = lines.iter().find(|l| l.starts_with("work/ops")).unwrap();
        assert!(ops.ends_with("no errors"), "{ops}");
    }

    #[test]
    fn status_names_the_code_when_a_binding_cannot_pull() {
        let (dir, mut config) = two_bindings();
        std::fs::remove_file(crate::tracker::credential::path_in(dir.path(), "corr-linear").unwrap()).unwrap();
        let lines = status(&config, dir.path(), now(), None);
        assert!(lines[0].ends_with("never pulled, cannot pull (credential). Stale. Reason: No pull was tried."), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("no errors")), "{lines:?}");

        connect(dir.path(), "corr-linear", "t").unwrap();
        config.trackers.iter_mut().find(|b| b.project == "ops").unwrap().provider = "jira".into();
        let lines = status(&config, dir.path(), now(), None);
        assert!(lines[0].ends_with("never pulled. Stale. Reason: No pull was tried."), "{lines:?}");
        assert!(lines[1].ends_with("never pulled, cannot pull (unsupported_provider). Stale. Reason: No pull was tried."), "{lines:?}");
    }

    #[test]
    fn status_shows_a_refused_token_as_auth() {
        struct Revoked;
        impl Adapter for Revoked {
            fn pull(&self, _: Option<DateTime<Utc>>, _: bool, _: &mut Sink<'_>) -> Result<(), String> {
                Err(format!("Linear returned HTTP 401: {}", crate::tracker::adapter::AUTH_REFUSED))
            }
        }
        let revoked = |_: &TrackerBinding, _: Option<&crate::tracker::credential::Credential>| -> Result<Box<dyn Adapter>, String> { Ok(Box::new(Revoked)) };
        let (dir, config) = setup(false);
        connect(dir.path(), "corr-linear", "t").unwrap();
        let error = pull(&config, dir.path(), None, Mode::Incremental, now(), &revoked).unwrap_err();
        assert!(error.ends_with("(auth)"), "{error}");
        let line = &status(&config, dir.path(), now(), None)[0];
        assert!(line.ends_with("last error auth at 2026-09-01T12:00:00Z. Stale. Reason: The last pull failed: auth."), "{line}");
    }

    #[test]
    fn doctor_fails_the_run_when_a_check_fails() {
        let (dir, config) = setup(false);
        let unreachable = |_: &TrackerBinding, _: &crate::tracker::credential::Credential| -> Result<Box<dyn crate::tracker::linear::Transport>, String> {
            Err("never called without a credential".into())
        };
        let error = doctor(&config, dir.path(), &unreachable, &crate::tracker::doctor::connect_github).unwrap_err();
        assert_eq!(error.lines().count(), 5, "four checks and the mirror line: {error}");
        assert!(error.starts_with("work/claims: credential failed (credential)"), "{error}");
    }

    /// Writes `rows` after a completed pull at `pulled` into work/claims's log.
    fn seed(config: &WardwellConfig, pulled: DateTime<Utc>, rows: &[String]) {
        let path = log::path_for(&config.vault_path, "work", "claims");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let completed = format!(
            r#"{{"kind":"pull_completed","id":"p","provider":"linear","external_key":"COR","external_id":"COR","occurred_at":"{}","title":"p"}}"#,
            stamp(pulled)
        );
        let body: String = std::iter::once(completed).chain(rows.iter().cloned()).map(|row| format!("{row}\n")).collect();
        std::fs::write(path, format!("{}\n{body}", crate::tracker::events::SCHEMA_HEADER)).unwrap();
    }

    fn started(at: DateTime<Utc>, pid: u32) -> String {
        format!(r#"{{"kind":"pull_started","id":"s{pid}","provider":"linear","external_key":"COR","external_id":"COR","occurred_at":"{}","title":"s","pid":{pid}}}"#, stamp(at))
    }

    #[test]
    fn status_never_says_no_errors_for_a_stale_mirror() {
        let (dir, config) = setup(true);
        connect(dir.path(), "corr-linear", "t").unwrap();
        seed(&config, now() - chrono::TimeDelta::hours(5), &[]);
        let line = &status(&config, dir.path(), now(), None)[0];
        assert!(line.ends_with("1 events. Stale. Reason: No pull was tried."), "{line}");
        assert!(!line.contains("no errors"), "{line}");
        seed(&config, now() - chrono::TimeDelta::minutes(30), &[]);
        assert!(status(&config, dir.path(), now(), None)[0].ends_with("no errors"), "a fresh clean mirror");
    }

    #[test]
    fn status_shows_a_running_pull_and_its_start_time() {
        let (dir, config) = setup(true);
        connect(dir.path(), "corr-linear", "t").unwrap();
        seed(&config, now() - chrono::TimeDelta::hours(5), &[started(now() - chrono::TimeDelta::minutes(2), std::process::id())]);
        let line = &status(&config, dir.path(), now(), None)[0];
        assert!(line.ends_with(", pull running since 2026-09-01T11:58:00Z"), "{line}");
        assert!(!line.contains("no errors"), "{line}");
    }

    #[test]
    fn status_never_says_no_errors_for_an_unfinished_pull() {
        let (dir, config) = setup(true);
        connect(dir.path(), "corr-linear", "t").unwrap();
        let gone = u32::MAX;
        seed(&config, now() - chrono::TimeDelta::minutes(40), &[started(now() - chrono::TimeDelta::minutes(30), gone)]);
        let line = &status(&config, dir.path(), now(), None)[0];
        assert!(line.ends_with(", a pull started at 2026-09-01T11:30:00Z and did not finish"), "{line}");
        seed(&config, now() - chrono::TimeDelta::hours(3), &[started(now() - chrono::TimeDelta::minutes(30), gone)]);
        let line = &status(&config, dir.path(), now(), None)[0];
        assert!(line.ends_with(". Stale. Reason: A pull started at 2026-09-01T11:30:00Z and did not finish."), "{line}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn schedule_on_a_protected_vault_prints_the_one_sentence_and_still_installs() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let fake = crate::tracker::schedule::fake::Fake::new(&[]);
        let icloud = home.join("Library/Mobile Documents/iCloud~md~obsidian/Documents/Notes");
        let lines = schedule(&home, &home.join(".wardwell"), Some(&icloud), 3600, &fake, Path::new("/bin/wardwell"), 501).unwrap();
        assert_eq!(lines[0], "macOS asks for consent after every upgrade, and the session refresh needs none.");
        assert!(lines[1].starts_with("Scheduled tracker pull every 3600s"), "{lines:?}");
        let elsewhere = schedule(&home, &home.join(".wardwell"), Some(&home.join("notes")), 3600, &fake, Path::new("/bin/wardwell"), 501).unwrap();
        assert_eq!(elsewhere.len(), 1, "{elsewhere:?}");
    }

    #[test]
    fn status_ends_with_the_schedule_line() {
        let (dir, config) = setup(false);
        assert_eq!(
            status(&config, dir.path(), now(), None).last().unwrap(),
            "pull schedule: no launchd agent; session start and the running server refresh a mirror over an hour old"
        );
        assert_eq!(status(&config, dir.path(), now(), Some(900)).last().unwrap(), "pull schedule: every 900 s (plist on disk)");
    }
}
