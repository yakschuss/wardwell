//! `wardwell uninstall` as a user runs it, with a temp HOME and config dir and
//! stub `launchctl` and `id` on a PATH that holds nothing else.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

struct Env {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    cfg: PathBuf,
    stub: PathBuf,
}

const MATCHER: &str = "mcp__linear__save_comment|mcp__linear__save_issue";

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let home = root.join("home");
    let cfg = root.join("cfg");
    let stub = root.join("stub");
    for dir in [&home.join(".claude"), &cfg, &stub, &home.join("Library/LaunchAgents")] {
        std::fs::create_dir_all(dir).unwrap();
    }
    write_script(&stub.join("launchctl"), &format!("#!/bin/sh\necho \"$*\" >> '{}'\nexit 0\n", root.join("launchctl.log").display()));
    write_script(&stub.join("id"), "#!/bin/sh\necho 501\n");
    std::fs::write(cfg.join("config.yml"), format!("vault_path: {}\nsession_sources: []\n", root.join("vault").display())).unwrap();
    let settings = json!({
        "model": "keep",
        "permissions": {"deny": ["mine", "mcp__linear__delete_comment"]},
        "hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": "'/w/wardwell' inject \"$(pwd)\""}]}],
            "Stop": [{"hooks": [{"type": "command", "command": "'/w/wardwell' resolve"}]}],
            "PreToolUse": [{"matcher": MATCHER, "hooks": [{"type": "command", "command": "'/w/wardwell' gate linear", "timeout": 5}]}]
        }
    });
    std::fs::write(home.join(".claude/settings.json"), serde_json::to_vec_pretty(&settings).unwrap()).unwrap();
    std::fs::write(home.join("Library/LaunchAgents/com.wardwell.tracker-pull.plist"), "plist").unwrap();
    Env { _tmp: tmp, home, cfg, stub }
}

fn write_script(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn record(e: &Env, text: &str) {
    std::fs::write(e.cfg.join("install-manifest.json"), text).unwrap();
}

fn uninstall(e: &Env) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_wardwell"))
        .arg("uninstall")
        .env_clear()
        .env("WARDWELL_GH_CANDIDATES", "")
        .env("HOME", &e.home)
        .env("WARDWELL_CONFIG_DIR", &e.cfg)
        .env("PATH", &e.stub)
        .output()
        .unwrap();
    (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

fn settings(e: &Env) -> Value {
    serde_json::from_slice(&std::fs::read(e.home.join(".claude/settings.json")).unwrap()).unwrap()
}

fn assert_hooks_and_pull_removed(e: &Env, out: &str) {
    let s = settings(e);
    assert!(s.get("hooks").is_none_or(|h| h.as_object().unwrap().is_empty()), "{s}\n{out}");
    assert!(!e.home.join("Library/LaunchAgents/com.wardwell.tracker-pull.plist").exists(), "{out}");
    let log = std::fs::read_to_string(e._tmp.path().join("launchctl.log")).unwrap();
    assert!(log.contains("bootout gui/501/com.wardwell.tracker-pull"), "{log}");
}

#[test]
fn a_clean_uninstall_reports_what_it_removed_and_exits_zero() {
    let e = env();
    record(&e, r#"{"version": 1, "claude_permissions_deny": ["mcp__linear__delete_comment"]}"#);
    let (ok, out) = uninstall(&e);
    assert!(ok, "{out}");
    assert_hooks_and_pull_removed(&e, &out);
    assert_eq!(settings(&e)["permissions"]["deny"], json!(["mine"]));
    assert!(out.contains("Removed:"), "{out}");
    assert!(out.contains("Linear gate"), "{out}");
    assert!(!out.contains("did not finish"), "{out}");
}

#[test]
fn an_install_record_from_a_newer_version_skips_only_the_deny_step() {
    for text in [
        r#"{"version": 2, "claude_permissions_deny": ["mcp__linear__delete_comment"]}"#,
        r#"{"version": 1, "claude_permissions_deny": [], "surprise": true}"#,
        "not json",
    ] {
        let e = env();
        record(&e, text);
        let (ok, out) = uninstall(&e);
        assert!(!ok, "{text}: {out}");
        assert_hooks_and_pull_removed(&e, &out);
        assert_eq!(settings(&e)["permissions"]["deny"], json!(["mine", "mcp__linear__delete_comment"]), "{text}");
        assert!(out.contains("SKIPPED"), "{out}");
        assert!(out.contains("deny entries left in place"), "{out}");
        assert!(out.contains("Uninstall did not finish"), "{out}");
        assert_eq!(std::fs::read_to_string(e.cfg.join("install-manifest.json")).unwrap(), text);
    }
}

#[cfg(unix)]
#[test]
fn an_unreadable_install_record_skips_only_the_deny_step() {
    use std::os::unix::fs::PermissionsExt;
    let e = env();
    record(&e, r#"{"version": 1, "claude_permissions_deny": ["mcp__linear__delete_comment"]}"#);
    let path = e.cfg.join("install-manifest.json");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&path).is_ok() {
        return; // running as root: the file is readable after all
    }
    let (ok, out) = uninstall(&e);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(!ok, "{out}");
    assert_hooks_and_pull_removed(&e, &out);
    assert!(out.contains("SKIPPED"), "{out}");
}

#[test]
fn a_settings_file_that_cannot_be_edited_fails_that_client_and_says_what_to_do() {
    let e = env();
    std::fs::write(e.home.join(".claude/settings.json"), "{").unwrap();
    let (ok, out) = uninstall(&e);
    assert!(!ok, "{out}");
    assert!(out.contains("Claude Code hooks were not removed"), "{out}");
    assert!(out.contains("then run `wardwell uninstall` again"), "{out}");
    assert!(!e.home.join("Library/LaunchAgents/com.wardwell.tracker-pull.plist").exists(), "{out}");
    assert_eq!(std::fs::read_to_string(e.home.join(".claude/settings.json")).unwrap(), "{");
    assert!(!out.contains("Removed: Session"), "{out}");
}
