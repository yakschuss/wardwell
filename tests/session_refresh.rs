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
    // Every `gh pr list` read answers with no merged pull requests.
    script(&stub.join("gh"), "#!/bin/sh\necho '[]'\n");
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
    script(&stub.join("gh"), &format!("#!/bin/sh\necho read >> '{}'\nsleep 0.3\necho '[]'\n", reads.display()));
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
    assert!(!cfg.join("refresh/work__claims.claim").exists(), "the pull released its claim");
}
