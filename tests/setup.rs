//! `wardwell setup` as a user runs it, with a temp HOME and config dir and
//! stub `launchctl` and `id` on a PATH that holds nothing else.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::path::Path;
use std::process::Command;

fn script(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn a_staging_failure_prints_no_outcome_before_nothing_was_written() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let home = root.join("home");
    let cfg = root.join("cfg");
    let stub = root.join("stub");
    let desktop = home.join("Library/Application Support/Claude/claude_desktop_config.json");
    for dir in [home.join(".claude"), cfg.clone(), stub.clone(), desktop.parent().unwrap().to_path_buf()] {
        std::fs::create_dir_all(dir).unwrap();
    }
    script(&stub.join("launchctl"), &format!("#!/bin/sh\necho \"$*\" >> '{}'\n", root.join("launchctl.log").display()));
    script(&stub.join("id"), "#!/bin/sh\necho 501\n");
    std::fs::write(&desktop, "{}").unwrap();
    std::fs::write(home.join(".claude/settings.json"), "{}").unwrap();
    std::fs::write(cfg.join("config.yml"), format!(
        "vault_path: {}\nsession_sources: []\ntrackers:\n  work/claims:\n    provider: linear\n    team: COR\n    credential: c\n    gate: true\n",
        root.join("vault").display()
    )).unwrap();
    std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o500)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_wardwell"))
        .args(["setup", "--yes"])
        .env_clear()
        .env("HOME", &home)
        .env("WARDWELL_CONFIG_DIR", &cfg)
        .env("PATH", &stub)
        .output()
        .unwrap();
    std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o700)).unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("nothing was written"), "{text}");
    assert!(!text.contains("OK Claude Desktop"), "{text}");
    assert!(!text.contains("  OK "), "{text}");
    assert_eq!(std::fs::read_to_string(&desktop).unwrap(), "{}");
    assert_eq!(std::fs::read_to_string(home.join(".claude/settings.json")).unwrap(), "{}");
    assert!(!root.join("launchctl.log").exists());
}

#[cfg(unix)]
#[test]
fn a_github_binding_without_gh_or_a_token_sets_up_and_doctor_names_the_connect_command() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let home = root.join("home");
    let cfg = root.join("cfg");
    let stub = root.join("stub");
    for dir in [home.join(".claude"), cfg.clone(), stub.clone(), root.join("vault/work/claims")] {
        std::fs::create_dir_all(dir).unwrap();
    }
    script(&stub.join("launchctl"), &format!("#!/bin/sh\necho \"$*\" >> '{}'\n", root.join("launchctl.log").display()));
    script(&stub.join("id"), "#!/bin/sh\necho 501\n");
    std::fs::write(home.join(".claude/settings.json"), "{}").unwrap();
    std::fs::write(cfg.join("config.yml"), format!(
        "vault_path: {}\nsession_sources: []\ntrackers:\n  work/claims:\n    - provider: github\n      repository: acme/app\n",
        root.join("vault").display()
    )).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_wardwell"))
            .args(args)
            .env_clear()
            .env("HOME", &home)
            .env("WARDWELL_CONFIG_DIR", &cfg)
            .env("PATH", &stub)
            .output()
            .unwrap()
    };
    let setup = run(&["setup", "--yes"]);
    let text = format!("{}{}", String::from_utf8_lossy(&setup.stdout), String::from_utf8_lossy(&setup.stderr));
    assert!(setup.status.success(), "{text}");
    assert!(!stub.join("gh").exists());

    let doctor = run(&["doctor"]);
    let text = format!("{}{}", String::from_utf8_lossy(&doctor.stdout), String::from_utf8_lossy(&doctor.stderr));
    // Doctor also looks at the fixed Homebrew paths, which a test cannot
    // empty; on a host with gh there, the row says gh was found.
    let host_gh = ["/opt/homebrew/bin/gh", "/usr/local/bin/gh"].iter().any(|p| Path::new(p).is_file());
    let expected = match host_gh {
        true => "Tracker work/claims github             \u{2713} gh found or a token stored",
        false => "github: unreachable, run `wardwell tracker connect github`",
    };
    assert!(text.contains(expected), "{text}");
}
