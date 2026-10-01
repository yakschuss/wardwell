//! `wardwell gate linear` as Claude Code runs it: a payload on stdin, the
//! decision on stdout, exit 0 in every case.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::io::Write;
use std::process::{Command, Stdio};

fn gate(stdin: &str) -> (bool, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wardwell"))
        .args(["gate", "linear"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The gate stops reading at its size cap, so a larger write may see a
    // closed pipe; that is the behaviour under test, not a failure.
    let _ = child.stdin.take().unwrap().write_all(stdin.as_bytes());
    let out = child.wait_with_output().unwrap();
    (out.status.success(), String::from_utf8(out.stdout).unwrap())
}

#[test]
fn a_bad_comment_prints_the_deny_json_and_exits_zero() {
    let (ok, out) = gate(r#"{"tool_name":"mcp__linear__save_comment","tool_input":{"body":"Done: x"}}"#);
    assert!(ok);
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(value["hookSpecificOutput"]["permissionDecisionReason"].as_str().unwrap().starts_with("Linear gate: "));
}

#[test]
fn a_malformed_payload_prints_nothing_and_exits_zero() {
    assert_eq!(gate("not json"), (true, String::new()));
}

#[test]
fn another_tool_prints_nothing() {
    assert_eq!(gate(r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#), (true, String::new()));
}

const BAD: &str = r#"{"tool_name":"mcp__linear__save_comment","tool_input":{"body":"Done: x %s"}}"#;

#[test]
fn a_lone_surrogate_escape_is_allowed_because_the_parser_cannot_read_it() {
    assert_eq!(gate(&BAD.replace("%s", "\\ud83d")), (true, String::new()));
}

#[test]
fn nesting_past_the_parser_limit_is_allowed() {
    let deep = format!(r#"{{"tool_name":"mcp__linear__save_comment","tool_input":{{"body":"Done: x","junk":{}{}}}}}"#, "[".repeat(200), "]".repeat(200));
    assert_eq!(gate(&deep), (true, String::new()));
}

#[test]
fn input_over_the_size_cap_is_allowed() {
    let big = BAD.replace("%s", &"pad ".repeat(270_000));
    assert!(big.len() > 1024 * 1024);
    assert_eq!(gate(&big), (true, String::new()));
    let (ok, out) = gate(&BAD.replace("%s", "small"));
    assert!(ok && out.contains("deny"), "{out}");
}
