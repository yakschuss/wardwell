//! Per-project lines `wardwell inject` adds at session start: a rot line
//! with the age of the newest history entry and decision, and for a
//! project bound to a tracker, a short section from its mirror.
//!
//! Does NOT pull, write, or decide which projects a session sees.

use crate::config::loader::WardwellConfig;
use crate::tracker::events::{FailureCode, StateCategory};
use crate::tracker::freshness::{self, Freshness, State};
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

/// The tracker section for a project folder, or None when it is not bound
/// or has nothing to say. The log is parsed once and the view shared. The
/// issue binding gives the section; each other binding adds one freshness
/// line when it is not fresh. Runs the offline doctor check against
/// credentials in `config_dir`.
pub fn project_tracker_lines(config: &WardwellConfig, config_dir: &Path, domain: &str, project_dir: &Path, now: DateTime<Utc>) -> Option<Vec<String>> {
    let project = project_dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let bindings = config.bindings_for(domain, project);
    if bindings.is_empty() {
        return None;
    }
    let read = MirrorView::read_by_provider(&project_dir.join(crate::tracker::events::FILE_NAME));
    let view_of = |provider: &str| read.as_ref().map(|views| views.get(provider).cloned().unwrap_or_default()).map_err(Clone::clone);
    let mut lines = Vec::new();
    for binding in &bindings {
        let view = view_of(&binding.provider);
        let local = crate::tracker::state::provider(&crate::tracker::state::path(config_dir, domain, project), &binding.provider);
        let fresh = freshness::assess_read(&view, local.as_ref(), now, &freshness::process_alive);
        match crate::tracker::mirrors_issues(&binding.provider) {
            true => {
                let blocked = crate::tracker::doctor::check_offline(config_dir, binding).err().map(|(code, _)| code);
                lines.extend(tracker_section(&view.unwrap_or_default(), &fresh, now, blocked));
            }
            false if fresh.state != State::Fresh => {
                lines.push(format!("{} mirror. {}", crate::tracker::provider_label(&binding.provider), fresh.sentence()));
            }
            false => {}
        }
    }
    (!lines.is_empty()).then_some(lines)
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

/// The tracker section for a mirror at `now`, with its freshness. When
/// pulls cannot run (`blocked`, from the offline doctor check), one line
/// with the code and the age. Otherwise the first line is the freshness
/// sentence in every state but fresh: never pulled with its reason,
/// running, stale, or unreadable. A mirror never pulled or unreadable
/// lists no issues, nor does one whose last pull failed or that is over 24
/// hours old; otherwise up to ten started issues, most recently updated first.
pub fn tracker_section(view: &MirrorView, fresh: &Freshness, now: DateTime<Utc>, blocked: Option<FailureCode>) -> Vec<String> {
    let age = view.last_pull_at.map(|at| age_words(now - at));
    if let Some(code) = blocked {
        let pulled = age.map_or("Never pulled.".to_string(), |age| format!("Last pulled {age} ago."));
        return vec![format!("Tracker mirror. Pulls cannot run: {}. {pulled}", code.as_str())];
    }
    let mut lines = match (fresh.state, &age) {
        (State::Fresh, Some(age)) => vec![format!("Tracker mirror. Last pulled {age} ago. Not authoritative.")],
        _ => vec![format!("Tracker mirror. {}", fresh.sentence())],
    };
    if age.is_none() || matches!(fresh.state, State::Unreadable(_)) {
        return lines;
    }
    let failed = view.failed_since_last_pull();
    let stale = view.last_pull_at.is_some_and(|at| now - at > STALE_AFTER);
    if let (Some(code), false) = (failed, matches!(fresh.state, State::Stale(_))) {
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

    /// The section as session start builds it; `alive` answers for every process id.
    fn section(view: &MirrorView, blocked: Option<FailureCode>, alive: bool) -> Vec<String> {
        let fresh = freshness::assess(view, now(), &|_| alive);
        tracker_section(view, &fresh, now(), blocked)
    }

    fn with_attempt(mut view: MirrorView, attempt: crate::tracker::view::Attempt) -> MirrorView {
        view.last_attempt = Some(attempt);
        if let crate::tracker::view::Attempt::Failed { at, code } = attempt {
            view.last_failure = Some((at, code));
        }
        view
    }

    #[test]
    fn a_fresh_mirror_lists_started_issues_newest_first_capped_at_ten() {
        let mut issues: Vec<MirroredIssue> = (1..=12).map(|n| issue(n, StateCategory::Started)).collect();
        issues.push(issue(20, StateCategory::Unstarted));
        let mut archived = issue(0, StateCategory::Started);
        archived.issue.archived_at = Some(now());
        issues.push(archived);
        let lines = section(&view(1, issues), None, false);
        assert_eq!(lines[0], "Tracker mirror. Last pulled 1 hour ago. Not authoritative.");
        assert_eq!(lines.len(), 1 + MAX_STARTED);
        assert_eq!(lines[1], "- COR-1 Work 1 (In Progress)");
        assert_eq!(lines[10], "- COR-10 Work 10 (In Progress)");
        assert!(!lines.iter().any(|l| l.contains("COR-20") || l.contains("COR-0 ")));
    }

    #[test]
    fn a_mirror_two_to_24_hours_old_says_stale_and_why_and_still_lists_issues() {
        let lines = section(&view(5, vec![issue(1, StateCategory::Started)]), None, false);
        assert_eq!(lines, vec![
            "Tracker mirror. Last pulled 5h ago. Stale. Reason: No pull was tried.",
            "- COR-1 Work 1 (In Progress)",
        ]);
    }

    #[test]
    fn a_stale_mirror_names_a_pull_that_did_not_finish() {
        let started = crate::tracker::view::Attempt::Started { at: now() - TimeDelta::hours(1), pid: 4242 };
        let lines = section(&with_attempt(view(5, vec![]), started), None, false);
        assert_eq!(lines, vec!["Tracker mirror. Last pulled 5h ago. Stale. Reason: A pull started at 2026-09-30T11:00:00Z and did not finish."]);
    }

    #[test]
    fn a_running_pull_says_since_when() {
        let started = crate::tracker::view::Attempt::Started { at: now() - TimeDelta::minutes(2), pid: 4242 };
        let lines = section(&with_attempt(view(5, vec![issue(1, StateCategory::Started)]), started), None, true);
        assert_eq!(lines, vec!["Tracker mirror. Last pulled 5h ago; pull running since 2026-09-30T11:58:00Z.", "- COR-1 Work 1 (In Progress)"]);
        let never = section(&with_attempt(MirrorView::default(), started), None, true);
        assert_eq!(never, vec!["Tracker mirror. Never pulled; pull running since 2026-09-30T11:58:00Z."]);
    }

    #[test]
    fn a_mirror_over_24_hours_old_hides_the_issue_list() {
        let lines = section(&view(25, vec![issue(1, StateCategory::Started)]), None, false);
        assert_eq!(lines, vec![
            "Tracker mirror. Last pulled 25h ago. Stale. Reason: No pull was tried.",
            "The mirror is more than 24 hours old. Run `wardwell tracker pull`.",
        ]);
    }

    #[test]
    fn a_stale_mirror_whose_last_pull_failed_names_the_code_and_hides_the_list() {
        let failed = crate::tracker::view::Attempt::Failed { at: now() - TimeDelta::hours(1), code: FailureCode::Auth };
        let lines = section(&with_attempt(view(2, vec![issue(1, StateCategory::Started)]), failed), None, false);
        assert_eq!(lines, vec!["Tracker mirror. Last pulled 2h ago. Stale. Reason: The last pull failed: auth."]);
    }

    #[test]
    fn a_fresh_mirror_whose_last_pull_failed_shows_only_the_age_and_the_failure() {
        let failed = crate::tracker::view::Attempt::Failed { at: now() - TimeDelta::minutes(30), code: FailureCode::Auth };
        let lines = section(&with_attempt(view(1, vec![issue(1, StateCategory::Started)]), failed), None, false);
        assert_eq!(lines, vec![
            "Tracker mirror. Last pulled 1 hour ago. Not authoritative.",
            "The last pull failed: auth. Run `wardwell tracker status`.",
        ]);
    }

    /// The old assertion pinned "Tracker mirror. Never pulled. Not
    /// authoritative." That line left out the stale reason the card
    /// requires in every surface, so a mirror whose first pull failed read
    /// the same as one never tried.
    #[test]
    fn a_mirror_never_pulled_says_why() {
        let lines = section(&MirrorView::default(), None, false);
        assert_eq!(lines, vec!["Tracker mirror. Never pulled. Stale. Reason: No pull was tried."]);
        let failed = crate::tracker::view::Attempt::Failed { at: now() - TimeDelta::minutes(5), code: FailureCode::Auth };
        let lines = section(&with_attempt(MirrorView::default(), failed), None, false);
        assert_eq!(lines, vec!["Tracker mirror. Never pulled. Stale. Reason: The last pull failed: auth."]);
    }

    #[test]
    fn a_mirror_whose_pulls_cannot_run_is_not_presented_as_current() {
        let fresh = view(1, vec![issue(1, StateCategory::Started)]);
        assert_eq!(
            section(&fresh, Some(FailureCode::Credential), false),
            vec!["Tracker mirror. Pulls cannot run: credential. Last pulled 1 hour ago."]
        );
        assert_eq!(
            section(&MirrorView::default(), Some(FailureCode::UnsupportedProvider), false),
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
        assert_eq!(ready, vec!["Tracker mirror. Never pulled. Stale. Reason: No pull was tried."]);
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
        assert_eq!(lines, vec![
            "GitHub mirror. Never pulled. Stale. Reason: The last pull failed: provider.",
            "Tracker mirror. Last pulled 1 hour ago. Not authoritative.",
            "- COR-1 Claims inbox (In Progress)",
        ], "one freshness line per binding that is not fresh");

        let github_only = crate::config::loader::parse(&yaml("    provider: github\n    repository: acme/app\n")).unwrap();
        assert_eq!(
            project_tracker_lines(&github_only, dir.path(), "work", &project, now()).unwrap(),
            vec!["GitHub mirror. Never pulled. Stale. Reason: The last pull failed: provider."]
        );
        std::fs::write(project.join("tracker.jsonl"), b"\xff").unwrap();
        assert_eq!(
            project_tracker_lines(&both, dir.path(), "work", &project, now()).unwrap(),
            vec!["GitHub mirror. Could not read the mirror log: log_read.", "Tracker mirror. Could not read the mirror log: log_read."]
        );
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
