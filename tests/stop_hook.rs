//! The Stop hooks as the client runs them: `wardwell resolve` and
//! `wardwell companion lifecycle stop`, with a temp HOME and config dir.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Env {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    cfg: PathBuf,
    code: PathBuf,
    /// The PATH the binary runs with: a folder made here holding only `git`.
    path: PathBuf,
}

fn git(dir: &Path, args: &[&str], date: Option<i64>) {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir);
    cmd.args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"]);
    cmd.args(args).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").stdout(Stdio::null()).stderr(Stdio::null());
    if let Some(at) = date {
        cmd.env("GIT_COMMITTER_DATE", format!("@{at} +0000")).env("GIT_AUTHOR_DATE", format!("@{at} +0000"));
    }
    assert!(cmd.status().unwrap().success(), "git {args:?}");
}

/// A mapped repo with an old commit, its vault folder, and a config.
fn env(extra_config: &str) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let home = root.join("home");
    let cfg = home.join(".wardwell");
    let code = root.join("code/corrtex");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::create_dir_all(root.join("vault/personal/corr")).unwrap();
    std::fs::create_dir_all(&code).unwrap();
    git(&code, &["init", "-q", "-b", "main"], None);
    git(&code, &["commit", "-q", "--allow-empty", "-m", "old"], Some(1_700_000_000));
    let yaml = format!("vault_path: {}\nsession_sources: []\nprojects:\n  personal/corr:\n    paths: [\"{}\"]\n{extra_config}", root.join("vault").display(), code.display());
    std::fs::write(cfg.join("config.yml"), yaml).unwrap();
    Env { _tmp: tmp, home, cfg, code, path: git_only_path(&root) }
}

/// A folder under `root` holding a link to the git this test uses, found
/// through `git --exec-path`, never by searching a system folder.
fn git_only_path(root: &Path) -> PathBuf {
    let out = Command::new("git").arg("--exec-path").output().unwrap();
    let exec = PathBuf::from(String::from_utf8(out.stdout).unwrap().trim());
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(exec.join("git"), bin.join("git")).unwrap();
    bin
}

fn wardwell(e: &Env, args: &[&str], input: &Value) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wardwell"))
        .args(args)
        .current_dir(&e.code)
        .env("HOME", &e.home)
        .env("WARDWELL_CONFIG_DIR", &e.cfg)
        .env("PATH", &e.path)
        .env("WARDWELL_GH_CANDIDATES", "")
        .env_remove("WARDWELL_STOP_CHECK")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    write!(child.stdin.take().unwrap(), "{input}").unwrap();
    child.wait_with_output().unwrap()
}

fn begin(e: &Env, session: &str) {
    let out = wardwell(e, &["companion", "lifecycle", "begin", "--client", "claude"], &json!({"session_id": session, "prompt_id": "p1", "hook_event_name": "UserPromptSubmit", "cwd": e.code}));
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

/// A commit whose reflog entry is a few seconds after now, so it is
/// after the session start whatever second the start fell in.
fn work(e: &Env) {
    let soon = chrono::Utc::now().timestamp() + 5;
    git(&e.code, &["commit", "-q", "--allow-empty", "-m", "work"], Some(soon));
}

fn stop_payload(e: &Env, session: &str) -> Value {
    json!({"session_id": session, "prompt_id": "p1", "hook_event_name": "Stop", "stop_hook_active": false, "cwd": e.code})
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn log(e: &Env) -> String {
    std::fs::read_to_string(e.cfg.join("stop-check/blocks.jsonl")).unwrap_or_default()
}

#[test]
fn resolve_blocks_once_then_allows() {
    let e = env("");
    begin(&e, "s1");
    work(&e);
    let first = wardwell(&e, &["resolve"], &stop_payload(&e, "s1"));
    assert!(first.status.success());
    let block: Value = serde_json::from_str(&stdout(&first)).unwrap();
    assert_eq!(block["decision"], "block");
    assert!(block["reason"].as_str().unwrap().starts_with("1 commit since "), "{block}");
    let second = wardwell(&e, &["resolve"], &stop_payload(&e, "s1"));
    assert!(second.status.success() && stdout(&second).is_empty());
    let garbage = Command::new(env!("CARGO_BIN_EXE_wardwell")).arg("resolve").env("WARDWELL_CONFIG_DIR", &e.cfg).env("HOME", &e.home).env("PATH", &e.path).env("WARDWELL_GH_CANDIDATES", "").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let out = { let mut c = garbage; write!(c.stdin.take().unwrap(), "not json").unwrap(); c.wait_with_output().unwrap() };
    assert!(out.status.success() && stdout(&out).is_empty(), "a bad payload allows");
}

#[test]
fn resolve_without_a_lifecycle_record_allows() {
    let e = env("");
    work(&e);
    let out = wardwell(&e, &["resolve"], &stop_payload(&e, "never-begun"));
    assert!(out.status.success() && stdout(&out).is_empty());
}

#[test]
fn lifecycle_stop_joins_the_companion_reason_and_the_stop_check() {
    let e = env("");
    begin(&e, "s2");
    work(&e);
    let out = wardwell(&e, &["companion", "lifecycle", "stop", "--client", "claude"], &stop_payload(&e, "s2"));
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let block: Value = serde_json::from_str(&stdout(&out)).unwrap();
    let reason = block["reason"].as_str().unwrap();
    assert!(reason.starts_with("Inspect the actual task"), "Companion first: {reason}");
    assert!(reason.ends_with("or set WARDWELL_STOP_CHECK=off."), "{reason}");
    let mut active = stop_payload(&e, "s2");
    active["stop_hook_active"] = json!(true);
    let again = wardwell(&e, &["companion", "lifecycle", "stop", "--client", "claude"], &active);
    let value: Value = serde_json::from_str(&stdout(&again)).unwrap();
    assert!(value.get("decision").is_none(), "{value}");
}

#[test]
fn a_companion_error_skips_the_stop_check() {
    let e = env("");
    begin(&e, "s3");
    work(&e);
    let mut unknown = stop_payload(&e, "s3");
    unknown["prompt_id"] = json!("never-opened");
    let out = wardwell(&e, &["companion", "lifecycle", "stop", "--client", "claude"], &unknown);
    assert!(!out.status.success(), "the Companion error stays an error");
    assert!(stdout(&out).is_empty());
    assert!(!log(&e).contains("s3"), "no block recorded: {}", log(&e));
    let ok = wardwell(&e, &["resolve"], &stop_payload(&e, "s3"));
    assert!(stdout(&ok).contains("no history entry"), "the marker was never claimed");
}

#[test]
fn stop_hook_false_in_config_turns_the_check_off() {
    let e = env("stop_hook: false\n");
    begin(&e, "s4");
    work(&e);
    let out = wardwell(&e, &["resolve"], &stop_payload(&e, "s4"));
    assert!(out.status.success() && stdout(&out).is_empty());
    assert!(log(&e).is_empty());
}
