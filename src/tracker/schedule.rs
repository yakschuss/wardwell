//! Installs and removes the launchd agent that runs `wardwell tracker pull`
//! on an interval. Does NOT pull, read config, or touch the tracker; launchctl
//! is reached only through `LaunchctlRunner` so tests never invoke it.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The launchd label, which also names the plist file.
pub const LABEL: &str = "com.wardwell.tracker-pull";

/// Shortest interval accepted; anything faster hammers the provider.
pub const MIN_INTERVAL_SECONDS: u32 = 60;
/// Longest interval accepted; launchd's StartInterval is a signed 32-bit integer.
pub const MAX_INTERVAL_SECONDS: u32 = 2_147_483_647;

/// Runs one launchctl argv (program first).
pub trait LaunchctlRunner {
    fn run(&self, argv: &[String]) -> Result<(), String>;
}

/// Runs argv as a real process.
pub struct SystemRunner;

impl LaunchctlRunner for SystemRunner {
    fn run(&self, argv: &[String]) -> Result<(), String> {
        let (program, args) = argv.split_first().ok_or("empty command")?;
        let output = Command::new(program)
            .args(args)
            .output()
            .map_err(|error| format!("{program} could not start: {error}"))?;
        match output.status.success() {
            true => Ok(()),
            false => Err(format!("{program} failed: {}", String::from_utf8_lossy(&output.stderr).trim())),
        }
    }
}

/// The current user's numeric id, for the `gui/<uid>` launchd domain.
pub fn current_uid() -> Result<u32, String> {
    let output = Command::new("id").arg("-u").output().map_err(|error| format!("id -u: {error}"))?;
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|_| "could not read the current user id".to_string())
}

/// Where the agent plist lives under `home`.
pub fn plist_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
}

/// The plist body for an agent that runs `<binary> tracker pull` every
/// `interval_seconds` and at load, logging both streams to `log_path`.
pub fn launch_agent_plist(binary: &Path, interval_seconds: u32, log_path: &Path) -> String {
    let binary = xml_escape(&binary.display().to_string());
    let log = xml_escape(&log_path.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{binary}</string>
    <string>tracker</string>
    <string>pull</string>
  </array>
  <key>StartInterval</key>
  <integer>{interval_seconds}</integer>
  <key>RunAtLoad</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn cron_line(binary: &Path, interval_seconds: u32) -> String {
    let minutes = (interval_seconds / 60).max(1);
    format!("*/{minutes} * * * * {} tracker pull", binary.display())
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_string()).collect()
}

fn domain(uid: u32) -> String {
    format!("gui/{uid}")
}

/// Install (or replace) the agent and load it. Returns a line to print.
pub fn schedule(
    home: &Path,
    config_dir: &Path,
    interval_seconds: u32,
    runner: &dyn LaunchctlRunner,
    current_exe: &Path,
    uid: u32,
) -> Result<String, String> {
    if !(MIN_INTERVAL_SECONDS..=MAX_INTERVAL_SECONDS).contains(&interval_seconds) {
        return Err(format!(
            "interval must be between {MIN_INTERVAL_SECONDS} and {MAX_INTERVAL_SECONDS} seconds"
        ));
    }
    let binary = current_exe.canonicalize().unwrap_or_else(|_| current_exe.to_path_buf());
    if !cfg!(target_os = "macos") {
        return Err(format!(
            "launchd scheduling is macOS only. Add this line to your crontab:\n{}",
            cron_line(&binary, interval_seconds)
        ));
    }
    let path = plist_path(home);
    let parent = path.parent().ok_or("no LaunchAgents directory")?;
    std::fs::create_dir_all(parent).map_err(|error| format!("could not create the LaunchAgents directory: {error}"))?;
    let body = launch_agent_plist(&binary, interval_seconds, &config_dir.join("tracker-pull.log"));
    // Replace any existing copy cleanly; it may not be loaded, so ignore failure.
    let _ = runner.run(&argv(&["launchctl", "bootout", &format!("{}/{LABEL}", domain(uid))]));
    std::fs::write(&path, body).map_err(|error| format!("could not write {LABEL}.plist: {error}"))?;
    load(runner, uid, &path)?;
    Ok(format!("Scheduled tracker pull every {interval_seconds}s ({})", path.display()))
}

fn load(runner: &dyn LaunchctlRunner, uid: u32, path: &Path) -> Result<(), String> {
    let plist = path.display().to_string();
    let first_error = match runner.run(&argv(&["launchctl", "bootstrap", &domain(uid), &plist])) {
        Ok(()) => None,
        Err(bootstrap) => {
            runner.run(&argv(&["launchctl", "load", &plist])).map_err(|load| format!("{bootstrap}; {load}"))?;
            Some(bootstrap)
        }
    };
    runner
        .run(&argv(&["launchctl", "print", &format!("{}/{LABEL}", domain(uid))]))
        .map_err(|print| match first_error {
            Some(bootstrap) => format!("{LABEL} is not loaded after scheduling ({print}); first error: {bootstrap}"),
            None => format!("{LABEL} is not loaded after scheduling ({print})"),
        })
}

/// Unload the agent and delete its plist. A missing file is not an error.
pub fn unschedule(home: &Path, runner: &dyn LaunchctlRunner, uid: u32) -> Result<String, String> {
    let path = plist_path(home);
    let _ = runner.run(&argv(&["launchctl", "bootout", &format!("{}/{LABEL}", domain(uid))]));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(format!("Removed {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok("Tracker pull was not scheduled".to_string()),
        Err(error) => Err(format!("could not remove {LABEL}.plist: {error}")),
    }
}

/// The interval in the plist on disk, or None when it is not installed.
pub fn schedule_status(home: &Path) -> Option<u32> {
    let body = std::fs::read_to_string(plist_path(home)).ok()?;
    let after = body.split("<key>StartInterval</key>").nth(1)?;
    let value = after.split("<integer>").nth(1)?.split("</integer>").next()?;
    value.trim().parse().ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Fake {
        calls: RefCell<Vec<Vec<String>>>,
        fail: Vec<&'static str>,
    }

    impl Fake {
        fn new(fail: &[&'static str]) -> Self {
            Fake { calls: RefCell::new(Vec::new()), fail: fail.to_vec() }
        }
        fn verbs(&self) -> Vec<String> {
            self.calls.borrow().iter().map(|call| call[1].clone()).collect()
        }
    }

    impl LaunchctlRunner for Fake {
        fn run(&self, argv: &[String]) -> Result<(), String> {
            self.calls.borrow_mut().push(argv.to_vec());
            match self.fail.contains(&argv[1].as_str()) {
                true => Err(format!("{} failed: already loaded", argv[1])),
                false => Ok(()),
            }
        }
    }

    #[test]
    fn plist_has_label_arguments_interval_runatload_and_both_logs() {
        let xml = launch_agent_plist(Path::new("/bin/wardwell"), 900, Path::new("/cfg/tracker-pull.log"));
        assert!(xml.contains("<string>com.wardwell.tracker-pull</string>"));
        assert!(xml.contains("<string>/bin/wardwell</string>\n    <string>tracker</string>\n    <string>pull</string>"));
        assert!(xml.contains("<key>StartInterval</key>\n  <integer>900</integer>"));
        assert!(xml.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(xml.contains("<key>StandardOutPath</key>\n  <string>/cfg/tracker-pull.log</string>"));
        assert!(xml.contains("<key>StandardErrorPath</key>\n  <string>/cfg/tracker-pull.log</string>"));
    }

    #[test]
    fn plist_escapes_xml_in_paths() {
        let xml = launch_agent_plist(Path::new("/a&b/wardwell"), 60, Path::new("/l"));
        assert!(xml.contains("/a&amp;b/wardwell"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn schedule_writes_the_plist_boots_out_first_then_bootstraps() {
        let home = tempfile::tempdir().unwrap();
        let fake = Fake::new(&[]);
        schedule(home.path(), Path::new("/cfg"), 1800, &fake, Path::new("/bin/wardwell"), 501).unwrap();
        assert_eq!(fake.verbs(), vec!["bootout", "bootstrap", "print"]);
        let calls = fake.calls.borrow();
        assert_eq!(calls[0][2], "gui/501/com.wardwell.tracker-pull");
        assert_eq!(calls[1][2], "gui/501");
        assert!(calls[1][3].ends_with("Library/LaunchAgents/com.wardwell.tracker-pull.plist"));
        assert_eq!(schedule_status(home.path()), Some(1800));
        let body = std::fs::read_to_string(plist_path(home.path())).unwrap();
        assert!(body.contains("/cfg/tracker-pull.log"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn schedule_ignores_bootout_failure_and_falls_back_to_load() {
        let home = tempfile::tempdir().unwrap();
        let fake = Fake::new(&["bootout", "bootstrap"]);
        schedule(home.path(), Path::new("/cfg"), 3600, &fake, Path::new("/bin/wardwell"), 501).unwrap();
        assert_eq!(fake.verbs(), vec!["bootout", "bootstrap", "load", "print"]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn schedule_fails_when_bootstrap_and_load_both_fail() {
        let home = tempfile::tempdir().unwrap();
        let fake = Fake::new(&["bootstrap", "load"]);
        assert!(schedule(home.path(), Path::new("/cfg"), 3600, &fake, Path::new("/bin/wardwell"), 501).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn schedule_fails_when_load_succeeds_but_the_job_is_not_loaded() {
        let home = tempfile::tempdir().unwrap();
        let fake = Fake::new(&["bootout", "bootstrap", "print"]);
        let error = schedule(home.path(), Path::new("/cfg"), 3600, &fake, Path::new("/bin/wardwell"), 501).unwrap_err();
        assert_eq!(fake.verbs(), vec!["bootout", "bootstrap", "load", "print"]);
        assert!(error.contains("bootstrap failed: already loaded"), "{error}");
        assert!(!error.contains(&home.path().display().to_string()), "{error}");
        assert_eq!(fake.calls.borrow()[3][2], "gui/501/com.wardwell.tracker-pull");
    }

    #[test]
    fn schedule_rejects_intervals_outside_the_bounds_and_writes_nothing() {
        for interval in [0, 1, 59, 2_147_483_648, u32::MAX] {
            let home = tempfile::tempdir().unwrap();
            let fake = Fake::new(&[]);
            let error = schedule(home.path(), Path::new("/cfg"), interval, &fake, Path::new("/bin/wardwell"), 501).unwrap_err();
            assert!(error.contains("60") && error.contains("2147483647"), "{error}");
            assert!(fake.calls.borrow().is_empty());
            assert!(!plist_path(home.path()).exists());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn schedule_accepts_both_edges_of_the_interval_bounds() {
        for interval in [60, 2_147_483_647] {
            let home = tempfile::tempdir().unwrap();
            let fake = Fake::new(&[]);
            schedule(home.path(), Path::new("/cfg"), interval, &fake, Path::new("/bin/wardwell"), 501).unwrap();
            assert_eq!(schedule_status(home.path()), Some(interval));
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn schedule_off_macos_returns_the_cron_line_and_writes_nothing() {
        let home = tempfile::tempdir().unwrap();
        let fake = Fake::new(&[]);
        let error = schedule(home.path(), Path::new("/cfg"), 3600, &fake, Path::new("/bin/wardwell"), 501).unwrap_err();
        assert!(error.contains("*/60 * * * * /bin/wardwell tracker pull"), "{error}");
        assert!(!plist_path(home.path()).exists());
        assert!(fake.calls.borrow().is_empty());
    }

    #[test]
    fn cron_line_converts_seconds_to_minutes() {
        assert_eq!(cron_line(Path::new("/w"), 3600), "*/60 * * * * /w tracker pull");
        assert_eq!(cron_line(Path::new("/w"), 10), "*/1 * * * * /w tracker pull");
    }

    #[test]
    fn unschedule_boots_out_removes_the_file_and_tolerates_a_missing_one() {
        let home = tempfile::tempdir().unwrap();
        let path = plist_path(home.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, launch_agent_plist(Path::new("/w"), 60, Path::new("/l"))).unwrap();
        let fake = Fake::new(&[]);
        unschedule(home.path(), &fake, 501).unwrap();
        assert!(!path.exists());
        assert_eq!(fake.verbs(), vec!["bootout"]);
        assert!(unschedule(home.path(), &Fake::new(&["bootout"]), 501).is_ok());
    }

    #[test]
    fn status_is_none_without_a_plist_and_reads_the_interval_with_one() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(schedule_status(home.path()), None);
        let path = plist_path(home.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, launch_agent_plist(Path::new("/w"), 7200, Path::new("/l"))).unwrap();
        assert_eq!(schedule_status(home.path()), Some(7200));
    }
}
