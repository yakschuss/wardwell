#![allow(clippy::unwrap_used, clippy::expect_used)]
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

const SOURCE: &str = "companion:claude:reply-test";
const PLAN: &str = "1c7218d7-b953-40a5-b1f0-7ac925adc532";
const ANSWER: &str = "5a1d29c6-ed59-4694-94b5-0c8d1627b17a";

fn run(dir: &Path, args: &[&str], input: Value) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wardwell"))
        .args(args)
        .env("WARDWELL_CONFIG_DIR", dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    write!(child.stdin.take().unwrap(), "{input}").unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).unwrap()
}
fn private_json(path: &Path, value: Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(path, value.to_string()).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}
fn journal_path(dir: &Path) -> std::path::PathBuf {
    dir.join("companions/wardwell-context")
        .join(format!("{:x}.json", Sha256::digest(SOURCE.as_bytes())))
}
fn fixture(dir: &Path) {
    private_json(
        &dir.join("hank/connection.json"),
        json!({"endpoint":"https://api.wardwell.app/mcp","token":"test-only-not-a-live-credential"}),
    );
    private_json(
        &journal_path(dir),
        json!({"source_key":SOURCE,"plan_id":PLAN,"current_revision":3,"response_cursor":"saved-cursor","pending_observations":[{"id":ANSWER,"node_id":"home","kind":"decision_response","response_key":"queue","response_detail":"Keep the calendar available in navigation.","expected_revision":3,"source_revision":3,"context_fingerprint":"immutable-context","context_snapshot":{"title":"Where should nurses land?","decision_options":[{"id":"queue","label":"Their team queue","detail":"Nursing or Behavioral Health"}]}}],"acknowledgements":[]}),
    );
}
fn begin(dir: &Path, prompt: &str) -> Value {
    run(
        dir,
        &["companion", "lifecycle", "begin", "--client", "claude"],
        json!({"hook_event_name":"UserPromptSubmit","session_id":"reply-test","prompt_id":prompt}),
    )
}
#[test]
fn ordinary_turn_delivers_answer_without_python_and_preserves_pending() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let first = begin(dir.path(), "one").to_string();
    assert!(first.contains("Where should nurses land?"));
    assert!(first.contains("Their team queue"));
    assert!(first.contains("Keep the calendar available"));
    let second = begin(dir.path(), "two").to_string();
    assert!(!second.contains("Where should nurses land?"));
    assert!(second.contains("remain") || second.contains("pending"));
    let request =
        json!({"action":"consume","source_key":SOURCE,"arguments":{"id":PLAN,"compact":true}});
    for _ in 0..2 {
        let compact = run(dir.path(), &["companion", "request"], request.clone()).to_string();
        assert!(compact.contains("Their team queue"));
        assert!(compact.contains("Keep the calendar available"));
    }
    let stored: Value =
        serde_json::from_slice(&fs::read(journal_path(dir.path())).unwrap()).unwrap();
    assert_eq!(stored["pending_observations"].as_array().unwrap().len(), 1);
    assert!(stored["acknowledgements"].as_array().unwrap().is_empty());
    let resume = run(
        dir.path(),
        &["companion", "lifecycle", "resume", "--client", "claude"],
        json!({"hook_event_name":"SessionStart","session_id":"reply-test","source":"resume"}),
    )
    .to_string();
    assert!(resume.contains("Their team queue"));
    assert!(resume.contains("Keep the calendar available"));
}
#[test]
fn no_known_plan_adds_no_response_context() {
    let dir = tempfile::tempdir().unwrap();
    let result = begin(dir.path(), "one").to_string();
    assert!(!result.contains("response refresh"));
    assert!(!result.contains("observations"));
}
