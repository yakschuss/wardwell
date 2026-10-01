//! Per-project lines `wardwell inject` adds at session start: a rot line
//! with the age of the newest history entry and decision, and for a
//! project bound to a tracker, a short section from its mirror.
//!
//! Does NOT pull, write, or decide which projects a session sees.

use crate::config::loader::WardwellConfig;
use crate::tracker::events::{FailureCode, StateCategory};
use crate::tracker::view::{MirrorView, age_words};
use chrono::{DateTime, Local, NaiveDate, TimeDelta, Utc};
use std::path::Path;

/// A mirror whose last pull is older than this lists no issues.
pub const STALE_AFTER: TimeDelta = TimeDelta::hours(24);

/// At most this many started issues in the tracker section.
pub const MAX_STARTED: usize = 10;

/// The rot line for a project folder.
pub fn project_rot_line(project_dir: &Path, today: NaiveDate) -> String {
    rot_line(last_history_date(project_dir), last_decision_date(project_dir), today)
}

/// The tracker section for a project folder, or None when it is not bound.
/// Runs the offline doctor check against credentials in `config_dir`.
pub fn project_tracker_lines(config: &WardwellConfig, config_dir: &Path, domain: &str, project_dir: &Path, now: DateTime<Utc>) -> Option<Vec<String>> {
    let project = project_dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let binding = config.tracker_for(domain, project)?;
    let blocked = crate::tracker::doctor::check_offline(config_dir, binding).err().map(|(code, _)| code);
    let view = MirrorView::read_for(&project_dir.join(crate::tracker::events::FILE_NAME), &binding.provider).unwrap_or_default();
    Some(tracker_section(&view, now, blocked))
}

/// `Last history entry 12 days ago. Last decision 3 days ago.`
pub fn rot_line(last_history: Option<NaiveDate>, last_decision: Option<NaiveDate>, today: NaiveDate) -> String {
    let history = last_history.map_or("No history entries.".to_string(), |d| format!("Last history entry {}.", days_ago(d, today)));
    let decision = last_decision.map_or("No decisions.".to_string(), |d| format!("Last decision {}.", days_ago(d, today)));
    format!("{history} {decision}")
}

fn days_ago(date: NaiveDate, today: NaiveDate) -> String {
    match (today - date).num_days().max(0) {
        0 => "today".to_string(),
        1 => "1 day ago".to_string(),
        n => format!("{n} days ago"),
    }
}

/// The tracker section for a mirror at `now`. When pulls cannot run
/// (`blocked`, from the offline doctor check), one line with the code and
/// the age. A mirror never pulled says only that. A failed or stale last
/// pull shows only the age and the notice; otherwise up to ten started
/// issues, most recently updated first.
pub fn tracker_section(view: &MirrorView, now: DateTime<Utc>, blocked: Option<FailureCode>) -> Vec<String> {
    let age = view.last_pull_at.map(|at| age_words(now - at));
    if let Some(code) = blocked {
        let pulled = age.map_or("Never pulled.".to_string(), |age| format!("Last pulled {age} ago."));
        return vec![format!("Tracker mirror. Pulls cannot run: {}. {pulled}", code.as_str())];
    }
    let Some(age) = age else {
        return vec!["Tracker mirror. Never pulled. Not authoritative.".to_string()];
    };
    let mut lines = vec![format!("Tracker mirror. Last pulled {age} ago. Not authoritative.")];
    let failed = view.failed_since_last_pull();
    let stale = view.last_pull_at.is_some_and(|at| now - at > STALE_AFTER);
    if let Some(code) = failed {
        lines.push(format!("The last pull failed: {}. Run `wardwell tracker status`.", code.as_str()));
    }
    if stale {
        lines.push("The mirror is more than 24 hours old. Run `wardwell tracker pull`.".to_string());
    }
    if failed.is_some() || stale {
        return lines;
    }
    let mut started: Vec<_> = view.open().filter(|i| i.issue.state_category == StateCategory::Started).collect();
    started.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.key.cmp(&b.key)));
    lines.extend(started.into_iter().take(MAX_STARTED).map(|i| format!("- {} {} ({})", i.key, i.issue.issue_title, i.issue.state)));
    lines
}

/// Local date of the last entry in `history.jsonl`.
pub fn last_history_date(project_dir: &Path) -> Option<NaiveDate> {
    local_date(&last_history_stamp(project_dir)?)
}

/// The `date` of the last entry in `history.jsonl` as written, read from
/// the file's tail: the last line with a `date`. The window grows only when
/// the tail holds no such line.
pub fn last_history_stamp(project_dir: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    const FIRST_WINDOW: u64 = 64 * 1024;
    const MAX_WINDOW: u64 = 4 * 1024 * 1024;
    let mut file = std::fs::File::open(project_dir.join("history.jsonl")).ok()?;
    let len = file.metadata().ok()?.len();
    let mut window = FIRST_WINDOW;
    loop {
        let start = len.saturating_sub(window);
        let mut tail = Vec::new();
        file.seek(SeekFrom::Start(start)).ok()?;
        file.read_to_end(&mut tail).ok()?;
        let text = String::from_utf8_lossy(&tail);
        // A window that starts mid-file may begin mid-line; drop that piece.
        let whole = text.lines().skip(usize::from(start > 0));
        let found = whole.collect::<Vec<_>>().into_iter().rev().find_map(entry_date);
        if found.is_some() || start == 0 || window >= MAX_WINDOW {
            return found;
        }
        window *= 4;
    }
}

fn entry_date(line: &str) -> Option<String> {
    let entry = serde_json::from_str::<serde_json::Value>(line).ok()?;
    entry.get("date").and_then(|d| d.as_str()).filter(|d| local_date(d).is_some()).map(str::to_string)
}

/// Newest `## YYYY-MM-DD` heading date in `decisions.md`.
pub fn last_decision_date(project_dir: &Path) -> Option<NaiveDate> {
    let content = std::fs::read_to_string(project_dir.join("decisions.md")).ok()?;
    content
        .lines()
        .filter_map(|line| line.strip_prefix("## "))
        .filter_map(|rest| rest.get(..10).and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()))
        .max()
}

fn local_date(value: &str) -> Option<NaiveDate> {
    DateTime::parse_from_rfc3339(value)
        .map(|t| t.with_timezone(&Local).date_naive())
        .ok()
        .or_else(|| value.get(..10).and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::tracker::events::IssueSnapshot;
    use crate::tracker::view::MirroredIssue;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 30, 12, 0, 0).unwrap()
    }

    fn date(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, day).unwrap()
    }

    fn issue(n: i64, category: StateCategory) -> MirroredIssue {
        MirroredIssue {
            key: format!("COR-{n}"),
            provider: "linear".into(),
            external_id: format!("id-{n}"),
            issue: IssueSnapshot {
                issue_title: format!("Work {n}"),
                state: "In Progress".into(),
                state_category: category,
                ..Default::default()
            },
            updated_at: now() - TimeDelta::minutes(n),
            removed_at: None,
        }
    }

    fn view(pulled_hours_ago: i64, issues: Vec<MirroredIssue>) -> MirrorView {
        MirrorView {
            issues: issues.into_iter().map(|i| (i.key.clone(), i)).collect(),
            last_pull_at: Some(now() - TimeDelta::hours(pulled_hours_ago)),
            ..Default::default()
        }
    }

    #[test]
    fn rot_line_names_both_ages_in_days() {
        assert_eq!(rot_line(Some(date(18)), Some(date(27)), date(30)), "Last history entry 12 days ago. Last decision 3 days ago.");
        assert_eq!(rot_line(Some(date(30)), Some(date(29)), date(30)), "Last history entry today. Last decision 1 day ago.");
        assert_eq!(rot_line(None, None, date(30)), "No history entries. No decisions.");
    }

    #[test]
    fn a_fresh_mirror_lists_started_issues_newest_first_capped_at_ten() {
        let mut issues: Vec<MirroredIssue> = (1..=12).map(|n| issue(n, StateCategory::Started)).collect();
        issues.push(issue(20, StateCategory::Unstarted));
        let mut archived = issue(0, StateCategory::Started);
        archived.issue.archived_at = Some(now());
        issues.push(archived);
        let lines = tracker_section(&view(3, issues), now(), None);
        assert_eq!(lines[0], "Tracker mirror. Last pulled 3 hours ago. Not authoritative.");
        assert_eq!(lines.len(), 1 + MAX_STARTED);
        assert_eq!(lines[1], "- COR-1 Work 1 (In Progress)");
        assert_eq!(lines[10], "- COR-10 Work 10 (In Progress)");
        assert!(!lines.iter().any(|l| l.contains("COR-20") || l.contains("COR-0 ")));
    }

    #[test]
    fn a_stale_mirror_shows_only_the_age_and_the_notice() {
        let lines = tracker_section(&view(25, vec![issue(1, StateCategory::Started)]), now(), None);
        assert_eq!(lines, vec![
            "Tracker mirror. Last pulled 1 day ago. Not authoritative.",
            "The mirror is more than 24 hours old. Run `wardwell tracker pull`.",
        ]);
    }

    #[test]
    fn a_failed_last_pull_shows_only_the_age_and_the_failure() {
        let mut failed = view(2, vec![issue(1, StateCategory::Started)]);
        failed.last_failure = Some((now() - TimeDelta::hours(1), FailureCode::Auth));
        let lines = tracker_section(&failed, now(), None);
        assert_eq!(lines, vec![
            "Tracker mirror. Last pulled 2 hours ago. Not authoritative.",
            "The last pull failed: auth. Run `wardwell tracker status`.",
        ]);
    }

    #[test]
    fn a_mirror_never_pulled_says_only_that() {
        let lines = tracker_section(&MirrorView::default(), now(), None);
        assert_eq!(lines, vec!["Tracker mirror. Never pulled. Not authoritative."]);
    }

    #[test]
    fn a_mirror_whose_pulls_cannot_run_is_not_presented_as_current() {
        let fresh = view(1, vec![issue(1, StateCategory::Started)]);
        assert_eq!(
            tracker_section(&fresh, now(), Some(FailureCode::Credential)),
            vec!["Tracker mirror. Pulls cannot run: credential. Last pulled 1 hour ago."]
        );
        assert_eq!(
            tracker_section(&MirrorView::default(), now(), Some(FailureCode::UnsupportedProvider)),
            vec!["Tracker mirror. Pulls cannot run: unsupported_provider. Never pulled."]
        );
    }

    #[test]
    fn project_lines_run_the_offline_check_against_the_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        let project = vault.join("work/claims");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(dir.path().join("config.yml"), format!(
            "vault_path: {}\nsession_sources: []\ntrackers:\n  work/claims:\n    provider: linear\n    team: COR\n    credential: c\n",
            vault.display()
        ))
        .unwrap();
        let config = crate::config::loader::load(Some(&dir.path().join("config.yml"))).unwrap();
        let missing = project_tracker_lines(&config, dir.path(), "work", &project, now()).unwrap();
        assert_eq!(missing, vec!["Tracker mirror. Pulls cannot run: credential. Never pulled."]);
        let path = crate::tracker::credential::path_in(dir.path(), "c").unwrap();
        crate::tracker::credential::save(&path, "t").unwrap();
        let ready = project_tracker_lines(&config, dir.path(), "work", &project, now()).unwrap();
        assert_eq!(ready, vec!["Tracker mirror. Never pulled. Not authoritative."]);
        assert!(project_tracker_lines(&config, dir.path(), "work", &vault.join("work/ops"), now()).is_none());
    }

    #[test]
    fn the_section_reads_the_issue_binding_and_ignores_merged_changes() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        let project = vault.join("work/claims");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("tracker.jsonl"), crate::tracker::view::two_provider_log()).unwrap();
        let yaml = |trackers: &str| format!("vault_path: {}\nsession_sources: []\ntrackers:\n  work/claims:\n{trackers}", vault.display());
        let both = crate::config::loader::parse(&yaml("    - provider: github\n      repository: acme/app\n    - provider: linear\n      team: COR\n      credential: c\n")).unwrap();
        crate::tracker::credential::save(&crate::tracker::credential::path_in(dir.path(), "c").unwrap(), "t").unwrap();
        let lines = project_tracker_lines(&both, dir.path(), "work", &project, now()).unwrap();
        assert_eq!(lines, vec!["Tracker mirror. Last pulled 1 hour ago. Not authoritative.", "- COR-1 Claims inbox (In Progress)"]);

        let github_only = crate::config::loader::parse(&yaml("    provider: github\n    repository: acme/app\n")).unwrap();
        assert!(project_tracker_lines(&github_only, dir.path(), "work", &project, now()).is_none(), "no issue binding, no section");
    }

    #[test]
    fn dates_read_from_history_and_decisions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("history.jsonl"),
            "{\"_schema\":\"history\"}\n{\"date\":\"2026-09-20\",\"title\":\"a\"}\n{\"date\":\"2026-09-18\",\"title\":\"b\"}\nnot json\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("decisions.md"), "# p Decisions\n\n## 2026-09-27 — Pick\n\nbody\n\n## 2026-09-01 — Old\n").unwrap();
        assert_eq!(last_history_date(dir.path()), Some(date(18)), "the last entry, not the newest date");
        assert_eq!(last_decision_date(dir.path()), Some(date(27)));
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(last_history_date(empty.path()), None);
        assert_eq!(last_decision_date(empty.path()), None);
    }

    #[test]
    fn the_last_history_entry_is_read_from_a_large_file_tail() {
        let dir = tempfile::tempdir().unwrap();
        let mut body = String::from("{\"_schema\":\"history\"}\n");
        for _ in 0..3000 {
            body.push_str(&format!("{{\"date\":\"2026-09-01\",\"title\":\"{}\"}}\n", "x".repeat(800)));
        }
        body.push_str("{\"date\":\"2026-09-25T10:00:00+00:00\",\"title\":\"last\"}\n");
        body.push_str("not json\n");
        std::fs::write(dir.path().join("history.jsonl"), &body).unwrap();
        assert!(body.len() > 2_000_000);
        assert_eq!(last_history_date(dir.path()), Some(date(25)));
    }

    #[test]
    fn a_history_line_longer_than_the_first_window_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!("{{\"date\":\"2026-09-21\",\"title\":\"{}\"}}\n", "x".repeat(200_000));
        std::fs::write(dir.path().join("history.jsonl"), body).unwrap();
        assert_eq!(last_history_date(dir.path()), Some(date(21)));
    }
}
