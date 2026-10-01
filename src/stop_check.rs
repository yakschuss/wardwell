//! The Stop check: when a session made commits in a mapped project's
//! repository and wrote no history entry since it began, block the stop once
//! with one line of evidence and the command to run. It fails open: any
//! error, timeout, missing start time, or missing vault folder allows.
//! Only local git is read. `WARDWELL_STOP_CHECK=off` turns it off.
//!
//! Does NOT write history, count merged pull requests, touch the network, or
//! change the Companion checkpoint; `merge` only joins the two outputs.

use crate::config::loader::WardwellConfig;
use crate::inject::resolve::{Resolution, resolve};
use chrono::{DateTime, Local, NaiveDate, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The whole check, git included, ends within this budget. The Stop hook
/// runs under a 3 second client timeout shared with the Companion check.
pub const BUDGET: Duration = Duration::from_millis(1200);

/// The log of blocks, one JSON line each, under the state folder.
pub const LOG: &str = "blocks.jsonl";

/// Where the check keeps its once-per-session markers and its log.
pub fn state_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("stop-check")
}

/// False only when the switch says `off`.
pub fn enabled(switch: Option<&str>) -> bool {
    !switch.is_some_and(|v| v.trim().eq_ignore_ascii_case("off"))
}

/// The check as the Stop hooks run it: config from the config dir, the
/// switch from `WARDWELL_STOP_CHECK`, the start time from the session's
/// first lifecycle record. `stop_hook: false` in config.yml also turns it off.
pub fn check(client: crate::companion::lifecycle::Client, payload: &Value) -> Option<String> {
    let deadline = Instant::now() + BUDGET;
    let config = crate::config::loader::load(None).ok()?;
    let state = state_dir(&crate::config::loader::config_dir());
    let on = config.stop_hook && enabled(std::env::var("WARDWELL_STOP_CHECK").ok().as_deref());
    Check { enabled: on, config: &config, state_dir: &state, deadline }
        .evaluate(payload, |session| crate::companion::lifecycle::session_started_at(client, session, deadline))
}

/// What one evaluation reads.
pub struct Check<'a> {
    pub enabled: bool,
    pub config: &'a WardwellConfig,
    pub state_dir: &'a Path,
    pub deadline: Instant,
}

impl Check<'_> {
    /// The block reason for this Stop payload, or None to allow.
    pub fn evaluate(&self, payload: &Value, started_at: impl FnOnce(&str) -> Option<DateTime<Utc>>) -> Option<String> {
        if !self.enabled || payload["stop_hook_active"].as_bool() == Some(true) {
            return None;
        }
        let session = payload["session_id"].as_str().filter(|s| !s.is_empty())?;
        let cwd = payload["cwd"].as_str().map(PathBuf::from).or_else(|| std::env::current_dir().ok())?;
        let deadline = self.deadline;
        let Resolution::Project { domain, project } = resolve(&cwd, self.config, |d| crate::inject::git::dirs_by(d, deadline))? else {
            return None;
        };
        let project_dir = self.config.vault_path.join(&domain).join(&project);
        let marker = self.state_dir.join("blocked").join(hash(session));
        if !project_dir.is_dir() || marker.exists() {
            return None;
        }
        let start = started_at(session)?;
        let commits = crate::inject::git::commits_since(&cwd, start, deadline).filter(|n| *n > 0)?;
        if history_since(&project_dir, start) {
            return None;
        }
        claim(&marker)?;
        let key = format!("{domain}/{project}");
        log(self.state_dir, &json!({"at": Utc::now().to_rfc3339(), "session_id": session, "project": key, "commits": commits, "since": start.to_rfc3339()}));
        Some(reason(commits, start, &key))
    }
}

/// The Companion's Stop output with the check's block joined in. The
/// Companion runs first and its output is kept; when both block, one block
/// carries both reasons, Companion first.
pub fn merge(mut companion: Value, block: Option<String>) -> Value {
    let Some(reason) = block else {
        return companion;
    };
    let Some(object) = companion.as_object_mut() else {
        return companion;
    };
    let joined = match (object.get("decision").and_then(Value::as_str), object.get("reason").and_then(Value::as_str)) {
        (Some("block"), Some(first)) => format!("{first}\n\n{reason}"),
        _ => reason,
    };
    object.insert("decision".into(), json!("block"));
    object.insert("reason".into(), json!(joined));
    companion
}

/// The newest block for `key` in the log: its time and commit count.
pub fn last_block(state_dir: &Path, key: &str) -> Option<(DateTime<Utc>, u64)> {
    let text = std::fs::read_to_string(state_dir.join(LOG)).ok()?;
    text.lines().rev().filter_map(|l| serde_json::from_str::<Value>(l).ok()).find(|v| v["project"] == key).and_then(|v| {
        let at = DateTime::parse_from_rfc3339(v["at"].as_str()?).ok()?.with_timezone(&Utc);
        Some((at, v["commits"].as_u64()?))
    })
}

/// `2 commits since 14:02, no history entry. Run ...`
fn reason(commits: usize, start: DateTime<Utc>, key: &str) -> String {
    let local = start.with_timezone(&Local);
    let when = if local.date_naive() == Local::now().date_naive() { local.format("%H:%M") } else { local.format("%b %-d %H:%M") };
    let noun = if commits == 1 { "commit" } else { "commits" };
    format!("{commits} {noun} since {when}, no history entry. Run wardwell_write append_history for {key}, or set WARDWELL_STOP_CHECK=off.")
}

/// True when the newest history entry was written at or after `start`. An
/// entry with a date and no time counts from that date on.
fn history_since(project_dir: &Path, start: DateTime<Utc>) -> bool {
    let Some(stamp) = crate::inject::session::last_history_stamp(project_dir) else {
        return false;
    };
    match DateTime::parse_from_rfc3339(&stamp) {
        Ok(at) => at >= start,
        Err(_) => stamp.get(..10).and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()).is_some_and(|d| d >= start.date_naive()),
    }
}

/// Create the once marker. None when it exists or cannot be written, so a
/// check that cannot remember blocking never blocks.
fn claim(marker: &Path) -> Option<()> {
    std::fs::create_dir_all(marker.parent()?).ok()?;
    std::fs::OpenOptions::new().write(true).create_new(true).open(marker).ok().map(|_| ())
}

fn log(state_dir: &Path, entry: &Value) {
    let line = format!("{entry}\n");
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir.join(LOG))
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

fn hash(text: &str) -> String {
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::inject::git::testing::{commit_at, git, repo};

    struct Fixture {
        _tmp: tempfile::TempDir,
        config: WardwellConfig,
        state: PathBuf,
        code: PathBuf,
        project: PathBuf,
    }

    const START: i64 = 1_800_000_000;

    fn start() -> DateTime<Utc> {
        DateTime::from_timestamp(START, 0).unwrap()
    }

    /// A mapped repo with one commit before the session start, and the
    /// project folder in the vault.
    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("code/corrtex");
        std::fs::create_dir_all(&code).unwrap();
        git(&code, &["init", "-q", "-b", "main"]);
        commit_at(&code, "before", Some(&format!("@{} +0000", START - 3600)));
        let project = tmp.path().join("vault/personal/corr-platform");
        std::fs::create_dir_all(&project).unwrap();
        let yaml = format!("vault_path: {}\nsession_sources: []\nprojects:\n  personal/corr-platform:\n    paths: [\"{}\"]\n", tmp.path().join("vault").display(), code.display());
        let config = crate::config::loader::parse(&yaml).unwrap();
        let state = tmp.path().join("cfg/stop-check");
        Fixture { _tmp: tmp, config, state, code, project }
    }

    fn after_start(f: &Fixture, n: i64) {
        for i in 0..n {
            commit_at(&f.code, "work", Some(&format!("@{} +0000", START + 60 + i)));
        }
    }

    fn check(f: &Fixture, enabled: bool) -> Check<'_> {
        Check { enabled, config: &f.config, state_dir: &f.state, deadline: Instant::now() + Duration::from_secs(10) }
    }

    fn payload(f: &Fixture, session: &str, active: bool) -> Value {
        json!({"session_id": session, "hook_event_name": "Stop", "stop_hook_active": active, "cwd": f.code})
    }

    fn eval(f: &Fixture, session: &str) -> Option<String> {
        check(f, true).evaluate(&payload(f, session, false), |_| Some(start()))
    }

    #[test]
    fn commits_without_an_entry_block_once_per_session_and_log_it() {
        let f = fixture();
        after_start(&f, 2);
        let reason = eval(&f, "s-1").expect("blocks");
        assert!(reason.starts_with("2 commits since "), "{reason}");
        assert!(reason.ends_with(", no history entry. Run wardwell_write append_history for personal/corr-platform, or set WARDWELL_STOP_CHECK=off."), "{reason}");
        assert!(!reason.contains('\n'), "one line");
        assert_eq!(eval(&f, "s-1"), None, "never twice for one session");
        assert!(eval(&f, "s-2").is_some(), "another session blocks on its own");
        let log = std::fs::read_to_string(f.state.join(LOG)).unwrap();
        assert_eq!(log.lines().count(), 2);
        let (_, commits) = last_block(&f.state, "personal/corr-platform").unwrap();
        assert_eq!(commits, 2);
    }

    #[test]
    fn stop_hook_active_allows() {
        let f = fixture();
        after_start(&f, 1);
        assert_eq!(check(&f, true).evaluate(&payload(&f, "s-1", true), |_| Some(start())), None);
        assert_eq!(eval(&f, "s-1").map(|r| r.starts_with("1 commit since")), Some(true));
    }

    #[test]
    fn a_git_error_fails_open() {
        let f = fixture();
        after_start(&f, 1);
        std::fs::remove_dir_all(f.code.join(".git")).unwrap();
        assert_eq!(eval(&f, "s-1"), None);
        let f = fixture();
        after_start(&f, 1);
        let passed = Check { deadline: Instant::now() - Duration::from_millis(1), ..check(&f, true) };
        assert_eq!(passed.evaluate(&payload(&f, "s-1", false), |_| Some(start())), None, "a timeout allows");
        assert!(!f.state.exists(), "nothing recorded when allowing");
    }

    #[test]
    fn the_off_switch_allows() {
        let f = fixture();
        after_start(&f, 3);
        assert!(!enabled(Some("off")) && !enabled(Some(" OFF ")));
        assert!(enabled(None) && enabled(Some("on")));
        assert_eq!(check(&f, false).evaluate(&payload(&f, "s-1", false), |_| Some(start())), None);
    }

    #[test]
    fn no_commits_since_the_start_allows() {
        let f = fixture();
        assert_eq!(eval(&f, "s-1"), None);
    }

    #[test]
    fn an_entry_written_since_the_start_allows_and_an_older_one_does_not() {
        let f = fixture();
        after_start(&f, 1);
        std::fs::write(f.project.join("history.jsonl"), format!("{{\"date\":\"{}\",\"title\":\"old\"}}\n", DateTime::from_timestamp(START - 60, 0).unwrap().to_rfc3339())).unwrap();
        assert!(eval(&f, "s-1").is_some());
        std::fs::write(f.project.join("history.jsonl"), format!("{{\"date\":\"{}\",\"title\":\"new\"}}\n", DateTime::from_timestamp(START + 120, 0).unwrap().to_rfc3339())).unwrap();
        assert_eq!(eval(&f, "s-2"), None);
    }

    #[test]
    fn no_vault_folder_no_start_time_or_no_mapping_allows() {
        let f = fixture();
        after_start(&f, 1);
        assert_eq!(check(&f, true).evaluate(&payload(&f, "s-1", false), |_| None), None, "no start time");
        let mut elsewhere = payload(&f, "s-1", false);
        elsewhere["cwd"] = json!(f.code.parent().unwrap());
        assert_eq!(check(&f, true).evaluate(&elsewhere, |_| Some(start())), None, "unmapped directory");
        std::fs::remove_dir_all(&f.project).unwrap();
        assert_eq!(eval(&f, "s-1"), None, "no vault folder");
    }

    #[test]
    fn a_linked_worktree_counts_its_own_branch() {
        let f = fixture();
        let linked = f.code.parent().unwrap().join("corrtex-wt");
        git(&f.code, &["worktree", "add", "-q", "-b", "wt", linked.to_str().unwrap()]);
        commit_at(&linked, "work", Some(&format!("@{} +0000", START + 60)));
        let mut p = payload(&f, "s-1", false);
        p["cwd"] = json!(linked);
        assert!(check(&f, true).evaluate(&p, |_| Some(start())).is_some_and(|r| r.starts_with("1 commit since")));
    }

    #[test]
    fn merge_keeps_the_companion_output_and_joins_reasons() {
        assert_eq!(merge(json!({}), None), json!({}));
        assert_eq!(merge(json!({}), Some("ours".into())), json!({"decision":"block","reason":"ours"}));
        assert_eq!(merge(json!({"decision":"block","reason":"theirs"}), Some("ours".into())), json!({"decision":"block","reason":"theirs\n\nours"}));
        assert_eq!(merge(json!({"systemMessage":"m"}), Some("ours".into())), json!({"systemMessage":"m","decision":"block","reason":"ours"}));
        let companion = json!({"decision":"block","reason":"theirs"});
        assert_eq!(merge(companion.clone(), None), companion);
    }
}
