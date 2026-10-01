//! Session start, as a client hook runs it, starts a detached pull of a
//! stale mirror and returns at once. The built binary runs with a temp
//! HOME, config dir and vault, and a stub `gh` on a PATH that holds nothing
//! else, so the GitHub read never reaches the network.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn script(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// The `kind` and `pid` of each row of the log after the header.
fn rows(log: &Path) -> Vec<(String, Option<u64>)> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .skip(1)
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .map(|row| (row["kind"].as_str().unwrap_or_default().to_string(), row["pid"].as_u64()))
        .collect()
}

#[cfg(unix)]
#[test]
fn session_start_starts_a_detached_pull_of_a_stale_mirror_and_returns_at_once() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let home = root.join("home");
    let cfg = root.join("cfg");
    let stub = root.join("stub");
    let code = root.join("code/app");
    let project = root.join("vault/work/claims");
    for dir in [&home, &cfg, &stub, &code, &project] {
        std::fs::create_dir_all(dir).unwrap();
    }
    // Every `gh pr list` read answers with no merged pull requests, after a
    // second, so the detached pull can be seen while it runs.
    script(&stub.join("gh"), "#!/bin/sh\n/bin/sleep 1\necho '[]'\n");
    std::fs::write(cfg.join("config.yml"), format!(
        "vault_path: {}\nsession_sources: []\nprojects:\n  work/claims:\n    paths:\n      - {}\ntrackers:\n  work/claims:\n    provider: github\n    repository: acme/app\n",
        root.join("vault").display(),
        code.display()
    )).unwrap();
    let two_hours_ago = (chrono::Utc::now() - chrono::TimeDelta::hours(2)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let log = project.join("tracker.jsonl");
    std::fs::write(&log, format!(
        "{{\"_schema\":\"tracker\",\"_version\":\"1.0\"}}\n{{\"kind\":\"pull_completed\",\"id\":\"seed\",\"provider\":\"github\",\"external_key\":\"acme/app\",\"external_id\":\"acme/app\",\"occurred_at\":\"{two_hours_ago}\",\"title\":\"seed\",\"through\":\"{two_hours_ago}\"}}\n"
    )).unwrap();

    let started = Instant::now();
    let child = Command::new(env!("CARGO_BIN_EXE_wardwell"))
        .args(["inject", code.to_str().unwrap()])
        .env_clear()
        .env("HOME", &home)
        .env("WARDWELL_CONFIG_DIR", &cfg)
        .env("PATH", &stub)
        .env("WARDWELL_GH_CANDIDATES", "")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let inject_pid = u64::from(child.id());
    let out = child.wait_with_output().unwrap();
    let took = started.elapsed();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{text}");
    assert!(took < Duration::from_secs(3), "session start took {took:?}: {text}");
    assert_eq!(text.matches("Refresh started in the background.").count(), 1, "{text}");

    // While it runs, the detached pull leads its own session and process
    // group, and its parent is gone: launchd or init adopted it.
    let running_pid = loop {
        if let Some(pid) = rows(&log).iter().find(|(kind, _)| kind == "pull_started").and_then(|(_, pid)| *pid) {
            break pid;
        }
        assert!(started.elapsed() < Duration::from_secs(10), "no pull_started");
        std::thread::sleep(Duration::from_millis(20));
    };
    let ps = Command::new("/bin/ps").args(["-o", "ppid=,pgid=", "-p", &running_pid.to_string()]).output().unwrap();
    let fields: Vec<u64> = String::from_utf8_lossy(&ps.stdout).split_whitespace().map(|f| f.parse().unwrap()).collect();
    assert_eq!(fields, vec![1, running_pid], "parent 1 and its own group: {}", String::from_utf8_lossy(&ps.stdout));

    let deadline = Instant::now() + Duration::from_secs(10);
    let finished = loop {
        let kinds = rows(&log);
        if kinds.iter().skip(1).any(|(kind, _)| kind == "pull_completed") || Instant::now() > deadline {
            break kinds;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let pull_log = std::fs::read_to_string(cfg.join("tracker-pull.log")).unwrap_or_default();
    let kinds: Vec<&str> = finished.iter().map(|(kind, _)| kind.as_str()).collect();
    assert_eq!(kinds, vec!["pull_completed", "pull_started", "pull_completed"], "{pull_log}");
    let pid = finished[1].1.unwrap();
    assert_ne!(pid, inject_pid, "the detached process wrote the markers, not session start");
    assert!(pull_log.contains("refresh: tracker pull --project work/claims"), "{pull_log}");
    assert!(pull_log.contains("work/claims: incremental pull appended 0 events, 0 removed"), "{pull_log}");

    // A second session start finds a fresh mirror and starts nothing.
    let again = Command::new(env!("CARGO_BIN_EXE_wardwell"))
        .args(["inject", code.to_str().unwrap()])
        .env_clear()
        .env("HOME", &home)
        .env("WARDWELL_CONFIG_DIR", &cfg)
        .env("PATH", &stub)
        .env("WARDWELL_GH_CANDIDATES", "")
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&again.stdout).contains("Refresh started"), "{}", String::from_utf8_lossy(&again.stdout));
}

#[cfg(unix)]
#[test]
fn twenty_session_starts_in_one_second_start_one_pull_and_one_provider_read() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (home, cfg, stub, code, project) = (root.join("home"), root.join("cfg"), root.join("stub"), root.join("code/app"), root.join("vault/work/claims"));
    for dir in [&home, &cfg, &stub, &code, &project] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let reads = root.join("gh-reads");
    script(&stub.join("gh"), &format!("#!/bin/sh\necho read >> '{}'\n/bin/sleep 0.3\necho '[]'\n", reads.display()));
    std::fs::write(cfg.join("config.yml"), format!(
        "vault_path: {}\nsession_sources: []\nprojects:\n  work/claims:\n    paths:\n      - {}\ntrackers:\n  work/claims:\n    provider: github\n    repository: acme/app\n",
        root.join("vault").display(),
        code.display()
    )).unwrap();
    let two_hours_ago = (chrono::Utc::now() - chrono::TimeDelta::hours(2)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let log = project.join("tracker.jsonl");
    std::fs::write(&log, format!(
        "{{\"_schema\":\"tracker\",\"_version\":\"1.0\"}}\n{{\"kind\":\"pull_completed\",\"id\":\"seed\",\"provider\":\"github\",\"external_key\":\"acme/app\",\"external_id\":\"acme/app\",\"occurred_at\":\"{two_hours_ago}\",\"title\":\"seed\",\"through\":\"{two_hours_ago}\"}}\n"
    )).unwrap();
    let started = Instant::now();
    let children: Vec<_> = (0..20)
        .map(|_| {
            Command::new(env!("CARGO_BIN_EXE_wardwell"))
                .args(["inject", code.to_str().unwrap()])
                .env_clear()
                .env("HOME", &home)
                .env("WARDWELL_CONFIG_DIR", &cfg)
                .env("PATH", &stub)
                .env("WARDWELL_GH_CANDIDATES", "")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    assert!(started.elapsed() < Duration::from_secs(1), "twenty session starts began within {:?}", started.elapsed());
    let printed: usize = children
        .into_iter()
        .map(|child| String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).matches("Refresh started in the background.").count())
        .sum();
    assert_eq!(printed, 1);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !rows(&log).iter().skip(1).any(|(kind, _)| kind == "pull_completed") && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(500));
    let kinds: Vec<String> = rows(&log).into_iter().map(|(kind, _)| kind).collect();
    assert_eq!(kinds.iter().filter(|k| *k == "pull_started").count(), 1, "{kinds:?}");
    assert_eq!(std::fs::read_to_string(&reads).unwrap().lines().count(), 1, "one provider read");
    assert!(!cfg.join("refresh/work/claims.claim").exists(), "the pull released its claim");
}

#[cfg(unix)]
#[test]
fn a_pull_past_its_deadline_is_stopped_and_records_the_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (home, cfg, stub, project) = (root.join("home"), root.join("cfg"), root.join("stub"), root.join("vault/work/claims"));
    for dir in [&home, &cfg, &stub, &project] {
        std::fs::create_dir_all(dir).unwrap();
    }
    script(&stub.join("gh"), "#!/bin/sh\n/bin/sleep 20\necho '[]'\n");
    std::fs::write(cfg.join("config.yml"), format!(
        "vault_path: {}\nsession_sources: []\ntrackers:\n  work/claims:\n    provider: github\n    repository: acme/app\n",
        root.join("vault").display()
    )).unwrap();
    let started = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_wardwell"))
        .args(["tracker", "pull", "--project", "work/claims"])
        .env_clear()
        .env("HOME", &home)
        .env("WARDWELL_CONFIG_DIR", &cfg)
        .env("PATH", &stub)
        .env("WARDWELL_GH_CANDIDATES", "")
        .env("WARDWELL_PULL_DEADLINE_SECONDS", "2")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(!out.status.success(), "{text}");
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    assert!(text.contains("tracker pull stopped after 2 seconds; recorded timeout for work/claims github"), "{text}");
    let state = std::fs::read_to_string(cfg.join("refresh/work/claims.json")).unwrap();
    assert!(state.contains("\"code\":\"timeout\""), "{state}");
    let kinds: Vec<String> = rows(&project.join("tracker.jsonl")).into_iter().map(|(kind, _)| kind).collect();
    assert_eq!(kinds, vec!["pull_started", "pull_failed"], "the timeout marker follows the start");
    assert!(std::fs::read_to_string(project.join("tracker.jsonl")).unwrap().contains("\"code\":\"timeout\""));
}

#[cfg(unix)]
#[test]
fn a_pull_blocked_reading_the_log_records_its_start_and_status_says_it_did_not_finish() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (home, cfg, stub, project) = (root.join("home"), root.join("cfg"), root.join("stub"), root.join("vault/work/claims"));
    for dir in [&home, &cfg, &stub, &project] {
        std::fs::create_dir_all(dir).unwrap();
    }
    script(&stub.join("gh"), "#!/bin/sh\necho '[]'\n");
    std::fs::write(cfg.join("config.yml"), format!(
        "vault_path: {}\nsession_sources: []\ntrackers:\n  work/claims:\n    provider: github\n    repository: acme/app\n",
        root.join("vault").display()
    )).unwrap();
    let log = project.join("tracker.jsonl");
    assert!(Command::new("/usr/bin/mkfifo").arg(&log).status().unwrap().success());
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_wardwell"))
            .args(args)
            .env_clear()
            .env("HOME", &home)
            .env("WARDWELL_CONFIG_DIR", &cfg)
            .env("PATH", &stub)
            .env("WARDWELL_GH_CANDIDATES", "")
            .env("WARDWELL_PULL_DEADLINE_SECONDS", "1")
            .output()
            .unwrap()
    };
    let started = Instant::now();
    let pull = run(&["tracker", "pull", "--project", "work/claims"]);
    let text = String::from_utf8_lossy(&pull.stderr).to_string();
    assert!(!pull.status.success(), "{text}");
    assert!(started.elapsed() < Duration::from_secs(15), "{:?}", started.elapsed());
    assert!(text.contains("recorded timeout for work/claims github"), "{text}");
    let state = std::fs::read_to_string(cfg.join("refresh/work/claims.json")).unwrap();
    assert!(state.contains("started_at") && state.contains("\"code\":\"timeout\""), "{state}");
    std::fs::remove_file(&log).unwrap();
    std::fs::write(&log, "{\"_schema\":\"tracker\",\"_version\":\"1.0\"}\n").unwrap();
    let status = String::from_utf8_lossy(&run(&["tracker", "status"]).stdout).to_string();
    assert!(status.contains("Stale. Reason: A pull started at ") && status.contains(" and did not finish."), "{status}");
}

#[cfg(unix)]
#[test]
fn session_start_never_hangs_on_a_mirror_log_that_does_not_answer() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (home, cfg, stub, code, project) = (root.join("home"), root.join("cfg"), root.join("stub"), root.join("code/app"), root.join("vault/work/claims"));
    for dir in [&home, &cfg, &stub, &code, &project] {
        std::fs::create_dir_all(dir).unwrap();
    }
    script(&stub.join("gh"), "#!/bin/sh\necho '[]'\n");
    std::fs::write(cfg.join("config.yml"), format!(
        "vault_path: {}\nsession_sources: []\nprojects:\n  work/claims:\n    paths:\n      - {}\ntrackers:\n  work/claims:\n    provider: github\n    repository: acme/app\n",
        root.join("vault").display(),
        code.display()
    )).unwrap();
    assert!(Command::new("/usr/bin/mkfifo").arg(project.join("tracker.jsonl")).status().unwrap().success());
    // A first run of a freshly built binary can wait on the system's scan of
    // it; run it once so the timing below is session start's own.
    Command::new(env!("CARGO_BIN_EXE_wardwell")).arg("--version").output().unwrap();
    let started = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_wardwell"))
        .args(["inject", code.to_str().unwrap()])
        .env_clear()
        .env("HOME", &home)
        .env("WARDWELL_CONFIG_DIR", &cfg)
        .env("PATH", &stub)
        .env("WARDWELL_GH_CANDIDATES", "")
        .env("WARDWELL_PULL_DEADLINE_SECONDS", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let took = started.elapsed();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(took < Duration::from_secs(1), "session start took {took:?}: {text}");
    assert!(text.starts_with("**work/claims**\n  No history entries. No decisions.\n"), "{text}");
    assert!(text.contains("  Could not read the mirror log in time.\n"), "{text}");
}
