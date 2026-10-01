//! Installs and removes the launchd agent that runs `wardwell tracker pull`
//! on an interval, an explicit choice for a vault outside the folders macOS
//! protects; `setup` installs none. Also says whether a plist on disk is
//! exactly the one Wardwell writes, and whether a vault is under a
//! protected folder. Does NOT pull, read config, or touch the tracker;
//! launchctl is reached only through `LaunchctlRunner` so tests never
//! invoke it.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The launchd label, which also names the plist file.
pub const LABEL: &str = "com.wardwell.tracker-pull";

/// Shortest interval accepted; anything faster hammers the provider.
pub const MIN_INTERVAL_SECONDS: u32 = 60;
/// Longest interval accepted; launchd's StartInterval is a signed 32-bit integer.
pub const MAX_INTERVAL_SECONDS: u32 = 2_147_483_647;

/// Folders under the home folder whose files macOS guards with a privacy
/// prompt. A launchd job that reads a vault under one waits on that prompt,
/// and macOS asks again for each new build.
pub const PROTECTED_FOLDERS: [&str; 4] = ["Library/Mobile Documents", "Documents", "Desktop", "Downloads"];

/// The sentence `tracker schedule` and `doctor` print for a vault under a protected folder.
pub const PROTECTED_SENTENCE: &str = "macOS asks for consent after every upgrade, and the session refresh needs none.";

/// Whether `vault` is under a folder of `home` that macOS protects. Compares
/// the paths as written and, where they exist, with links resolved.
pub fn is_protected(vault: &Path, home: &Path) -> bool {
    let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    PROTECTED_FOLDERS.iter().map(|folder| home.join(folder)).any(|folder| vault.starts_with(&folder) || real(vault).starts_with(real(&folder)))
}

/// The launchd agent at Wardwell's label path, as `setup`, `uninstall` and
/// `doctor` judge it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Agent {
    /// No plist at the label path.
    Absent,
    /// Exactly the plist Wardwell writes, for this program and interval,
    /// and the program is a Wardwell binary.
    Owned { program: PathBuf, interval: u32 },
    /// A plist at the label path that is not exactly Wardwell's.
    Foreign,
}

/// The agent at the label path under `home`. Owned only on an exact match
/// with what `launch_agent_plist` writes for its own program and interval,
/// with the log in `config_dir`, and a program named `wardwell` or
/// `wardwell-<version>`.
pub fn agent(home: &Path, config_dir: &Path) -> Agent {
    let Ok(body) = std::fs::read_to_string(plist_path(home)) else {
        return match plist_path(home).exists() {
            true => Agent::Foreign,
            false => Agent::Absent,
        };
    };
    match (scheduled_program(home), schedule_status(home)) {
        (Some(program), Some(interval)) if is_wardwell_program(&program) && body == launch_agent_plist(&program, interval, &log_path(config_dir)) => {
            Agent::Owned { program, interval }
        }
        _ => Agent::Foreign,
    }
}

fn is_wardwell_program(program: &Path) -> bool {
    program.is_absolute() && program.file_name().and_then(|n| n.to_str()).is_some_and(|n| n == "wardwell" || n.starts_with("wardwell-"))
}

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

/// The log both of the agent's streams go to.
pub fn log_path(config_dir: &Path) -> PathBuf {
    config_dir.join("tracker-pull.log")
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

/// The cron time fields for an interval, and whether they only approximate it.
fn cron_fields(interval_seconds: u32) -> (String, bool) {
    let hours = interval_seconds / 3600;
    let whole_minutes = interval_seconds.is_multiple_of(60);
    match (interval_seconds.is_multiple_of(3600), hours, whole_minutes) {
        (true, 1, _) => ("0 * * * *".to_string(), false),
        (true, 2..=24, _) => (format!("0 */{hours} * * *"), false),
        (_, _, true) if interval_seconds / 60 < 60 => (format!("*/{} * * * *", interval_seconds / 60), false),
        _ => {
            let minutes = ((interval_seconds + 30) / 60).clamp(1, 59);
            (format!("*/{minutes} * * * *"), true)
        }
    }
}

/// The crontab line for non-macOS hosts, with a note when it is approximate.
fn cron_line(binary: &Path, interval_seconds: u32) -> String {
    let (fields, approximate) = cron_fields(interval_seconds);
    let line = format!("{fields} {} tracker pull", binary.display());
    match approximate {
        true => format!("{line}\nThis is approximate: cron cannot express every {interval_seconds} seconds."),
        false => line,
    }
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
    // Never resolve symlinks: a package manager's stable link (for example
    // /opt/homebrew/bin/wardwell) survives an upgrade; its versioned target does not.
    let binary = current_exe.to_path_buf();
    if !cfg!(target_os = "macos") {
        return Err(format!(
            "launchd scheduling is macOS only. Add this line to your crontab:\n{}",
            cron_line(&binary, interval_seconds)
        ));
    }
    let path = plist_path(home);
    let parent = path.parent().ok_or("no LaunchAgents directory")?;
    std::fs::create_dir_all(parent).map_err(|error| format!("could not create the LaunchAgents directory: {error}"))?;
    let body = launch_agent_plist(&binary, interval_seconds, &log_path(config_dir));
    std::fs::create_dir_all(config_dir).map_err(|error| format!("could not create the config directory: {error}"))?;
    let staged = path.with_extension("plist.tmp");
    std::fs::write(&staged, body).map_err(|error| format!("could not write {LABEL}.plist: {error}"))?;
    // Replace any existing copy cleanly; it may not be loaded, so ignore failure.
    let _ = runner.run(&argv(&["launchctl", "bootout", &format!("{}/{LABEL}", domain(uid))]));
    std::fs::rename(&staged, &path).map_err(|error| {
        let _ = std::fs::remove_file(&staged);
        format!("could not install {LABEL}.plist: {error}")
    })?;
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

/// The program the plist on disk runs, or None when it is not installed.
pub fn scheduled_program(home: &Path) -> Option<PathBuf> {
    let body = std::fs::read_to_string(plist_path(home)).ok()?;
    let after = body.split("<key>ProgramArguments</key>").nth(1)?;
    let value = after.split("<string>").nth(1)?.split("</string>").next()?;
    Some(PathBuf::from(value.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")))
}

/// The interval in the plist on disk, or None when it is not installed.
pub fn schedule_status(home: &Path) -> Option<u32> {
    let body = std::fs::read_to_string(plist_path(home)).ok()?;
    let after = body.split("<key>StartInterval</key>").nth(1)?;
    let value = after.trim_start().strip_prefix("<integer>")?.split("</integer>").next()?;
    value.trim().parse().ok()
}

/// A launchctl runner that records each argv and fails the named verbs, so
/// tests never invoke launchctl.
#[cfg(test)]
pub(crate) mod fake {
    use super::LaunchctlRunner;
    use std::cell::RefCell;

    pub(crate) struct Fake {
        pub(crate) calls: RefCell<Vec<Vec<String>>>,
        fail: Vec<&'static str>,
    }

    impl Fake {
        pub(crate) fn new(fail: &[&'static str]) -> Self {
            Fake { calls: RefCell::new(Vec::new()), fail: fail.to_vec() }
        }
        pub(crate) fn verbs(&self) -> Vec<String> {
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
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::fake::Fake;
    use super::*;

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
    fn a_vault_under_a_protected_folder_of_the_given_home_is_protected() {
        let home = Path::new("/nonexistent-home/jane");
        for vault in ["Library/Mobile Documents/iCloud~md~obsidian/Documents/Notes", "Documents/vault", "Desktop/v", "Downloads/v"] {
            assert!(is_protected(&home.join(vault), home), "{vault}");
        }
        for vault in ["notes", "Library/Application Support/vault", "Documentsx/v"] {
            assert!(!is_protected(&home.join(vault), home), "{vault}");
        }
        assert!(!is_protected(Path::new("/nonexistent-other/Documents/v"), home), "only the given home");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_into_a_protected_folder_is_protected() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        std::fs::create_dir_all(home.join("Documents/vault")).unwrap();
        std::os::unix::fs::symlink(home.join("Documents/vault"), root.path().join("vault")).unwrap();
        assert!(is_protected(&root.path().join("vault"), &home));
    }

    #[test]
    fn only_the_exact_plist_for_a_wardwell_program_is_owned() {
        let home = tempfile::tempdir().unwrap();
        let cfg = home.path().join(".wardwell");
        assert_eq!(agent(home.path(), &cfg), Agent::Absent);
        let path = plist_path(home.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let write = |program: &str, log: &Path| std::fs::write(&path, launch_agent_plist(Path::new(program), 900, log)).unwrap();
        write("/Users/jane/.wardwell/bin/wardwell-0.12.0", &log_path(&cfg));
        assert_eq!(agent(home.path(), &cfg), Agent::Owned { program: PathBuf::from("/Users/jane/.wardwell/bin/wardwell-0.12.0"), interval: 900 });
        write("/opt/homebrew/bin/wardwell", &log_path(&cfg));
        assert!(matches!(agent(home.path(), &cfg), Agent::Owned { .. }));
        write("/usr/local/bin/other-tool", &log_path(&cfg));
        assert_eq!(agent(home.path(), &cfg), Agent::Foreign, "not a Wardwell program");
        write("/opt/homebrew/bin/wardwell", Path::new("/elsewhere.log"));
        assert_eq!(agent(home.path(), &cfg), Agent::Foreign, "not the log Wardwell writes");
        let edited = launch_agent_plist(Path::new("/opt/homebrew/bin/wardwell"), 900, &log_path(&cfg)).replace("<key>RunAtLoad</key>", "<key>Nice</key><integer>5</integer><key>RunAtLoad</key>");
        std::fs::write(&path, edited).unwrap();
        assert_eq!(agent(home.path(), &cfg), Agent::Foreign, "edited by hand");
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
        schedule(home.path(), &home.path().join("cfg"), 1800, &fake, Path::new("/bin/wardwell"), 501).unwrap();
        assert_eq!(fake.verbs(), vec!["bootout", "bootstrap", "print"]);
        let calls = fake.calls.borrow();
        assert_eq!(calls[0][2], "gui/501/com.wardwell.tracker-pull");
        assert_eq!(calls[1][2], "gui/501");
        assert!(calls[1][3].ends_with("Library/LaunchAgents/com.wardwell.tracker-pull.plist"));
        assert_eq!(schedule_status(home.path()), Some(1800));
        let body = std::fs::read_to_string(plist_path(home.path())).unwrap();
        assert!(body.contains("cfg/tracker-pull.log"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn schedule_writes_the_symlink_path_so_the_agent_survives_an_upgrade() {
        let root = tempfile::tempdir().unwrap();
        let versioned = root.path().join("Cellar/wardwell/0.12.0/bin");
        std::fs::create_dir_all(&versioned).unwrap();
        std::fs::write(versioned.join("wardwell"), "").unwrap();
        std::fs::create_dir_all(root.path().join("bin")).unwrap();
        let link = root.path().join("bin/wardwell");
        std::os::unix::fs::symlink(versioned.join("wardwell"), &link).unwrap();
        schedule(root.path(), &root.path().join("cfg"), 3600, &Fake::new(&[]), &link, 501).unwrap();
        assert_eq!(scheduled_program(root.path()), Some(link));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn schedule_ignores_bootout_failure_and_falls_back_to_load() {
        let home = tempfile::tempdir().unwrap();
        let fake = Fake::new(&["bootout", "bootstrap"]);
        schedule(home.path(), &home.path().join("cfg"), 3600, &fake, Path::new("/bin/wardwell"), 501).unwrap();
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
        let error = schedule(home.path(), &home.path().join("cfg"), 3600, &fake, Path::new("/bin/wardwell"), 501).unwrap_err();
        assert_eq!(fake.verbs(), vec!["bootout", "bootstrap", "load", "print"]);
        assert!(error.contains("bootstrap failed: already loaded"), "{error}");
        assert!(!error.contains(&home.path().display().to_string()), "{error}");
        assert_eq!(fake.calls.borrow()[3][2], "gui/501/com.wardwell.tracker-pull");
    }

    #[cfg(all(unix, target_os = "macos"))]
    #[test]
    fn schedule_leaves_the_old_job_untouched_when_the_write_fails() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let path = plist_path(home.path());
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let fake = Fake::new(&[]);
        let result = schedule(home.path(), Path::new("/cfg"), 3600, &fake, Path::new("/bin/wardwell"), 501);
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
        assert!(fake.calls.borrow().is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn schedule_creates_the_config_dir_so_launchd_can_open_the_log() {
        let home = tempfile::tempdir().unwrap();
        let config_dir = home.path().join(".wardwell/nested");
        let fake = Fake::new(&[]);
        schedule(home.path(), &config_dir, 3600, &fake, Path::new("/bin/wardwell"), 501).unwrap();
        assert!(config_dir.is_dir());
    }

    #[test]
    fn schedule_rejects_intervals_outside_the_bounds_and_writes_nothing() {
        for interval in [0, 1, 59, 2_147_483_648, u32::MAX] {
            let home = tempfile::tempdir().unwrap();
            let fake = Fake::new(&[]);
            let error = schedule(home.path(), &home.path().join("cfg"), interval, &fake, Path::new("/bin/wardwell"), 501).unwrap_err();
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
            schedule(home.path(), &home.path().join("cfg"), interval, &fake, Path::new("/bin/wardwell"), 501).unwrap();
            assert_eq!(schedule_status(home.path()), Some(interval));
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn schedule_off_macos_returns_the_cron_line_and_writes_nothing() {
        let home = tempfile::tempdir().unwrap();
        let fake = Fake::new(&[]);
        let error = schedule(home.path(), Path::new("/cfg"), 3600, &fake, Path::new("/bin/wardwell"), 501).unwrap_err();
        assert!(error.contains("0 * * * * /bin/wardwell tracker pull"), "{error}");
        assert!(!plist_path(home.path()).exists());
        assert!(fake.calls.borrow().is_empty());
    }

    #[test]
    fn cron_line_uses_hours_minutes_or_a_marked_approximation() {
        let line = |seconds| cron_line(Path::new("/w"), seconds);
        assert_eq!(line(3600), "0 * * * * /w tracker pull");
        assert_eq!(line(7200), "0 */2 * * * /w tracker pull");
        assert_eq!(line(86_400), "0 */24 * * * /w tracker pull");
        assert_eq!(line(60), "*/1 * * * * /w tracker pull");
        assert_eq!(line(900), "*/15 * * * * /w tracker pull");
        assert_eq!(line(3540), "*/59 * * * * /w tracker pull");
        assert!(line(90).starts_with("*/2 * * * * /w tracker pull\n"));
        assert!(line(90).contains("approximate"));
        assert!(line(5400).starts_with("*/59 * * * * /w tracker pull\n"));
        assert!(line(90_000).contains("approximate"));
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
    fn scheduled_program_reads_the_unescaped_binary_path() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(scheduled_program(home.path()), None);
        let path = plist_path(home.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, launch_agent_plist(Path::new("/a&b/wardwell"), 60, Path::new("/l"))).unwrap();
        assert_eq!(scheduled_program(home.path()), Some(PathBuf::from("/a&b/wardwell")));
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

    #[test]
    fn status_ignores_calendar_intervals_and_non_integer_values() {
        let home = tempfile::tempdir().unwrap();
        let path = plist_path(home.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let calendar = "<dict><key>StartCalendarInterval</key><dict><key>Minute</key><integer>5</integer></dict></dict>";
        std::fs::write(&path, calendar).unwrap();
        assert_eq!(schedule_status(home.path()), None);
        let string = "<key>StartInterval</key>\n  <string>900</string>\n<key>Other</key><integer>5</integer>";
        std::fs::write(&path, string).unwrap();
        assert_eq!(schedule_status(home.path()), None);
        std::fs::write(&path, "<key>StartInterval</key>\n\t <integer>900</integer>").unwrap();
        assert_eq!(schedule_status(home.path()), Some(900));
    }
}
