//! `wardwell tracker connect github` as a user runs it, with a temp HOME and
//! config dir. The token arrives on stdin and is never echoed.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::io::Write;
use std::process::{Command, Stdio};

#[cfg(unix)]
#[test]
fn connect_github_stores_the_token_privately_and_never_echoes_it() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cfg = tmp.path().join("cfg");
    std::fs::create_dir_all(&home).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_wardwell"))
        .args(["tracker", "connect", "github", "--token-stdin"])
        .env_clear()
        .env("HOME", &home)
        .env("WARDWELL_CONFIG_DIR", &cfg)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"ghp_test_secret\n").unwrap();
    let out = child.wait_with_output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{text}");
    assert!(text.contains("Saved tracker credential 'github'"), "{text}");
    assert!(!text.contains("ghp_test_secret"), "{text}");

    let dir = cfg.join("trackers");
    let file = dir.join("github.json");
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    let stored: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(stored, serde_json::json!({"token": "ghp_test_secret"}), "the trailing newline is dropped");
}
