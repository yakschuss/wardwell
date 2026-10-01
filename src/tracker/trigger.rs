//! Starts a detached `tracker pull` for one project when its mirror is due:
//! the last completed pull of any binding is older than an hour, no pull of
//! the project is running, and the 60 second cooldown allows. Session start
//! and the running server both call `refresh`. It reads the markers and
//! returns at once; the pull runs in its own process.
//!
//! Does NOT pull, open the network, take the project lock, or wait on the
//! pull it starts.

use crate::config::loader::{TrackerBinding, WardwellConfig};
use crate::tracker::events::FailureCode;
use crate::tracker::freshness::{self, State};
use crate::tracker::refresh::COOLDOWN;
use crate::tracker::view::MirrorView;
use crate::tracker::{log, pull};
use chrono::{DateTime, TimeDelta, Utc};
use std::path::{Path, PathBuf};

/// A binding whose last completed pull is older than this is due.
pub const REFRESH_AFTER: TimeDelta = TimeDelta::hours(1);

/// The line the session that started a refresh prints.
pub const STARTED_LINE: &str = "Refresh started in the background.";

/// Starts `wardwell tracker pull --project <key>` without waiting for it.
/// Injected so no test starts a real process.
pub trait Spawner {
    fn spawn(&self, project: &str) -> Result<(), String>;
}

/// What `refresh` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A detached pull was started.
    Started,
    /// The project has no tracker binding.
    NoBinding,
    /// Every binding pulled within the last hour.
    NotDue,
    /// A pull of the project is running in a live process.
    Running,
    /// A pull marker of the project is younger than the cooldown.
    Cooldown,
    /// Every due binding fails the offline check, so a pull cannot run.
    Blocked,
    /// The pull could not start; a `spawn` pull_failed marker records it.
    SpawnFailed,
}

/// The checks `refresh` asks of the host. Injected so tests choose them.
pub struct Probes<'a> {
    /// Whether a process id is running.
    pub alive: &'a dyn Fn(u32) -> bool,
    /// Whether a binding passes the offline doctor check.
    pub can_pull: &'a dyn Fn(&TrackerBinding) -> bool,
}

/// Start a detached pull of `<domain>/<project>` through `spawner` when it
/// is due. Reads only the project's tracker log. A spawn error is written
/// as a pull_failed marker with the code `spawn` for each due binding.
pub fn refresh(config: &WardwellConfig, domain: &str, project: &str, now: DateTime<Utc>, spawner: &dyn Spawner, probes: &Probes<'_>) -> Outcome {
    let bindings = config.bindings_for(domain, project);
    if bindings.is_empty() {
        return Outcome::NoBinding;
    }
    let path = log::path_for(&config.vault_path, domain, project);
    let views: Vec<(&TrackerBinding, MirrorView)> =
        bindings.into_iter().map(|b| (b, MirrorView::read_for(&path, &b.provider).unwrap_or_default())).collect();
    if views.iter().any(|(_, view)| matches!(freshness::assess(view, now, probes.alive).state, State::Running(_))) {
        return Outcome::Running;
    }
    if views.iter().any(|(_, view)| cooling(view, now)) {
        return Outcome::Cooldown;
    }
    let due: Vec<&TrackerBinding> = views.iter().filter(|(_, view)| is_due(view, now)).map(|(b, _)| *b).collect();
    let pullable: Vec<&TrackerBinding> = due.iter().copied().filter(|b| (probes.can_pull)(b)).collect();
    match (due.is_empty(), pullable.is_empty()) {
        (true, _) => return Outcome::NotDue,
        (false, true) => return Outcome::Blocked,
        (false, false) => {}
    }
    match spawner.spawn(&format!("{domain}/{project}")) {
        Ok(()) => Outcome::Started,
        Err(_) => {
            record_spawn_failure(&path, &pullable, now);
            Outcome::SpawnFailed
        }
    }
}

/// `refresh` with the real spawner and probes: this binary started in its
/// own session, `kill -0` for live processes, and the offline doctor check
/// against credentials in `config_dir`.
pub fn refresh_detached(config: &WardwellConfig, config_dir: &Path, domain: &str, project: &str, now: DateTime<Utc>) -> Outcome {
    let spawner = DetachedPull::this_binary(config_dir);
    let can_pull = |binding: &TrackerBinding| crate::tracker::doctor::check_offline(config_dir, binding).is_ok();
    refresh(config, domain, project, now, &spawner, &Probes { alive: &freshness::process_alive, can_pull: &can_pull })
}

/// The last completed pull is older than `REFRESH_AFTER`, or none exists.
fn is_due(view: &MirrorView, now: DateTime<Utc>) -> bool {
    view.last_pull_at.is_none_or(|at| now - at > REFRESH_AFTER)
}

/// The newest completed, failed or started marker is younger than `COOLDOWN`.
fn cooling(view: &MirrorView, now: DateTime<Utc>) -> bool {
    let newest = [view.last_pull_at, view.last_failure.map(|(at, _)| at), view.last_started.map(|(at, _)| at)].into_iter().flatten().max();
    let cooldown = TimeDelta::from_std(COOLDOWN).unwrap_or(TimeDelta::seconds(60));
    newest.is_some_and(|at| now - at < cooldown)
}

/// Best effort: a `spawn` pull_failed marker for each binding.
fn record_spawn_failure(path: &Path, bindings: &[&TrackerBinding], now: DateTime<Utc>) {
    for binding in bindings {
        let marker = pull::pull_failed(binding, now, FailureCode::Spawn, false);
        let _ = log::read_for(path, &binding.provider).and_then(|mut summary| log::append_new(path, &[marker], &mut summary));
    }
}

/// Starts this binary as `tracker pull --project <key>` in its own session,
/// with standard input closed and both output streams appended to the pull
/// log. A thread reaps the process when it ends, so a long-lived caller
/// leaves no zombie; the caller never waits.
pub struct DetachedPull {
    pub program: PathBuf,
    pub log: PathBuf,
}

impl DetachedPull {
    /// The spawner for the running binary, logging to `tracker-pull.log`
    /// in `config_dir`. When the binary cannot be found, every spawn fails
    /// and is recorded as a `spawn` marker.
    pub fn this_binary(config_dir: &Path) -> Self {
        let program = std::env::current_exe().unwrap_or_default();
        Self { program, log: crate::tracker::schedule::log_path(config_dir) }
    }

    fn open_log(&self) -> Result<std::fs::File, String> {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options.open(&self.log).map_err(|_| format!("could not open {}", self.log.display()))
    }
}

impl Spawner for DetachedPull {
    fn spawn(&self, project: &str) -> Result<(), String> {
        use std::io::Write;
        let mut log = self.open_log()?;
        let _ = writeln!(log, "{} refresh: tracker pull --project {project}", freshness::stamp(Utc::now()));
        let errors = log.try_clone().map_err(|_| format!("could not open {}", self.log.display()))?;
        let mut command = std::process::Command::new(&self.program);
        command
            .args(["tracker", "pull", "--project", project])
            .stdin(std::process::Stdio::null())
            .stdout(log)
            .stderr(errors);
        #[cfg(unix)]
        new_session(&mut command);
        let mut child = command.spawn().map_err(|_| format!("could not start {}", self.program.display()))?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

/// Make the child the leader of a new session, so it has no controlling
/// terminal and outlives the caller's session and process group.
#[cfg(unix)]
fn new_session(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the hook runs in the child between fork and exec and calls
    // only setsid, which is async-signal-safe and allocates nothing.
    unsafe {
        command.pre_exec(|| match libc::setsid() {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::cell::RefCell;

    /// Records each project it is asked to start, fails when told to, and
    /// can write the pull_started marker the real child would write.
    struct Fake {
        started: RefCell<Vec<String>>,
        fail: bool,
        writes_start: Option<(PathBuf, u32)>,
    }

    impl Fake {
        fn new() -> Self {
            Fake { started: RefCell::new(Vec::new()), fail: false, writes_start: None }
        }
    }

    impl Spawner for Fake {
        fn spawn(&self, project: &str) -> Result<(), String> {
            self.started.borrow_mut().push(project.to_string());
            if let Some((path, pid)) = &self.writes_start {
                let row = started_row(now(), *pid, "linear");
                let body = std::fs::read_to_string(path).unwrap_or_default();
                std::fs::write(path, format!("{body}{row}\n")).unwrap();
            }
            match self.fail {
                true => Err("could not start /gone/wardwell".to_string()),
                false => Ok(()),
            }
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 1, 9, 0, 0).unwrap()
    }

    fn stamp(minutes_ago: i64) -> String {
        freshness::stamp(now() - TimeDelta::minutes(minutes_ago))
    }

    fn completed(minutes_ago: i64, provider: &str) -> String {
        format!(r#"{{"kind":"pull_completed","id":"p{minutes_ago}{provider}","provider":"{provider}","external_key":"k","external_id":"k","occurred_at":"{}","title":"p"}}"#, stamp(minutes_ago))
    }

    fn started_row(at: DateTime<Utc>, pid: u32, provider: &str) -> String {
        format!(r#"{{"kind":"pull_started","id":"s{pid}{at}","provider":"{provider}","external_key":"k","external_id":"k","occurred_at":"{}","title":"s","pid":{pid}}}"#, freshness::stamp(at))
    }

    fn failed(minutes_ago: i64) -> String {
        format!(r#"{{"kind":"pull_failed","id":"f{minutes_ago}","provider":"linear","external_key":"k","external_id":"k","occurred_at":"{}","title":"f","code":"provider"}}"#, stamp(minutes_ago))
    }

    /// A config binding work/claims to linear, and to github when `both`,
    /// with `rows` in its log.
    fn setup(both: bool, rows: &[String]) -> (tempfile::TempDir, WardwellConfig, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        let path = log::path_for(&vault, "work", "claims");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        if !rows.is_empty() {
            let body: String = rows.iter().map(|r| format!("{r}\n")).collect();
            std::fs::write(&path, format!("{}\n{body}", crate::tracker::events::SCHEMA_HEADER)).unwrap();
        }
        let github = if both { "    - provider: github\n      repository: acme/app\n" } else { "" };
        let yaml = format!(
            "vault_path: {}\nsession_sources: []\ntrackers:\n  work/claims:\n    - provider: linear\n      team: COR\n      credential: c\n{github}",
            vault.display()
        );
        (dir, crate::config::loader::parse(&yaml).unwrap(), path)
    }

    fn run(config: &WardwellConfig, spawner: &Fake, alive: bool, can_pull: bool) -> Outcome {
        let alive = move |_: u32| alive;
        let can_pull = move |_: &TrackerBinding| can_pull;
        refresh(config, "work", "claims", now(), spawner, &Probes { alive: &alive, can_pull: &can_pull })
    }

    #[test]
    fn a_pull_older_than_an_hour_starts_one_detached_pull_for_the_project() {
        let (_dir, config, _) = setup(false, &[completed(61, "linear")]);
        let fake = Fake::new();
        assert_eq!(run(&config, &fake, false, true), Outcome::Started);
        assert_eq!(*fake.started.borrow(), vec!["work/claims"]);
    }

    #[test]
    fn a_mirror_never_pulled_is_due() {
        let (_dir, config, _) = setup(false, &[]);
        let fake = Fake::new();
        assert_eq!(run(&config, &fake, false, true), Outcome::Started);
    }

    #[test]
    fn a_pull_within_the_hour_is_not_due() {
        let (_dir, config, _) = setup(false, &[completed(59, "linear")]);
        let fake = Fake::new();
        assert_eq!(run(&config, &fake, false, true), Outcome::NotDue);
        assert!(fake.started.borrow().is_empty());
    }

    #[test]
    fn any_due_binding_starts_one_pull_for_the_whole_project() {
        let (_dir, config, _) = setup(true, &[completed(10, "linear"), completed(120, "github")]);
        let fake = Fake::new();
        assert_eq!(run(&config, &fake, false, true), Outcome::Started);
        assert_eq!(fake.started.borrow().len(), 1);
    }

    #[test]
    fn a_live_pull_of_any_binding_stops_a_second_start() {
        let (_dir, config, _) = setup(true, &[completed(120, "linear"), started_row(now() - TimeDelta::minutes(3), 4242, "github")]);
        let fake = Fake::new();
        assert_eq!(run(&config, &fake, true, true), Outcome::Running);
        assert!(fake.started.borrow().is_empty());
    }

    #[test]
    fn the_cooldown_covers_failures_and_starts() {
        let (_dir, config, _) = setup(false, &[completed(120, "linear"), failed(0)]);
        assert_eq!(run(&config, &Fake::new(), false, true), Outcome::Cooldown);
        let (_dir, config, _) = setup(false, &[completed(120, "linear"), started_row(now() - TimeDelta::seconds(30), 4242, "linear")]);
        assert_eq!(run(&config, &Fake::new(), false, true), Outcome::Cooldown, "a start whose process is gone still cools down");
        let (_dir, config, _) = setup(false, &[completed(120, "linear"), started_row(now() - TimeDelta::minutes(2), 4242, "linear")]);
        assert_eq!(run(&config, &Fake::new(), false, true), Outcome::Started, "an unfinished pull is retried after the cooldown");
    }

    #[test]
    fn a_binding_that_cannot_pull_is_not_started() {
        let (_dir, config, _) = setup(false, &[completed(120, "linear")]);
        let fake = Fake::new();
        assert_eq!(run(&config, &fake, false, false), Outcome::Blocked);
        assert!(fake.started.borrow().is_empty());
    }

    #[test]
    fn no_binding_starts_nothing() {
        let (_dir, config, _) = setup(false, &[]);
        let fake = Fake::new();
        let alive = |_: u32| false;
        let can_pull = |_: &TrackerBinding| true;
        let outcome = refresh(&config, "work", "ops", now(), &fake, &Probes { alive: &alive, can_pull: &can_pull });
        assert_eq!(outcome, Outcome::NoBinding);
        assert!(fake.started.borrow().is_empty());
    }

    #[test]
    fn a_spawn_error_is_a_spawn_marker_and_not_a_failure_of_the_caller() {
        let (_dir, config, path) = setup(false, &[completed(120, "linear")]);
        let fake = Fake { fail: true, ..Fake::new() };
        assert_eq!(run(&config, &fake, false, true), Outcome::SpawnFailed);
        let view = MirrorView::read_for(&path, "linear").unwrap();
        assert_eq!(view.last_failure, Some((now(), FailureCode::Spawn)));
        assert_eq!(run(&config, &Fake::new(), false, true), Outcome::Cooldown, "the marker cools the next start down");
    }

    #[test]
    fn a_second_call_after_the_start_does_not_start_again() {
        let (_dir, config, path) = setup(false, &[completed(120, "linear")]);
        let fake = Fake { writes_start: Some((path, 4242)), ..Fake::new() };
        assert_eq!(run(&config, &fake, true, true), Outcome::Started);
        assert_eq!(run(&config, &fake, true, true), Outcome::Running);
        assert_eq!(fake.started.borrow().len(), 1);
    }

    #[test]
    fn the_trigger_never_takes_the_project_lock() {
        let (_dir, config, path) = setup(false, &[completed(120, "linear")]);
        let held = crate::tracker::lock::acquire(&path, std::time::Duration::ZERO).unwrap();
        let started = std::time::Instant::now();
        assert_eq!(run(&config, &Fake::new(), false, true), Outcome::Started);
        assert!(started.elapsed() < std::time::Duration::from_millis(500), "{:?}", started.elapsed());
        drop(held);
    }

    #[test]
    fn the_detached_spawner_reports_a_missing_program_as_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let spawner = DetachedPull { program: dir.path().join("gone/wardwell"), log: dir.path().join("tracker-pull.log") };
        let error = spawner.spawn("work/claims").unwrap_err();
        assert!(error.contains("could not start"), "{error}");
        let log = std::fs::read_to_string(dir.path().join("tracker-pull.log")).unwrap();
        assert!(log.ends_with("refresh: tracker pull --project work/claims\n"), "{log}");
        let unopenable = DetachedPull { program: dir.path().join("wardwell"), log: dir.path().join("no/such/dir/log") };
        assert!(unopenable.spawn("work/claims").unwrap_err().contains("could not open"));
    }
}
