//! Starts a detached `tracker pull` for one project when its mirror is due:
//! the last completed pull of any binding is older than an hour, no pull of
//! the project is running, and the 60 second cooldown allows. Session start
//! and the running server both call `refresh`. It decides from the local
//! refresh state file alone and returns at once; the pull runs in its own
//! process.
//!
//! Does NOT read the vault, parse the tracker log, pull, open the network,
//! take the project lock, or wait on the pull it starts.

use crate::config::loader::{TrackerBinding, WardwellConfig};
use crate::tracker::events::FailureCode;
use crate::tracker::freshness;
use crate::tracker::refresh::COOLDOWN;
use crate::tracker::state::{self, ProviderState};
use chrono::{DateTime, TimeDelta, Utc};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A binding whose last completed pull is older than this is due.
pub const REFRESH_AFTER: TimeDelta = TimeDelta::hours(1);

/// The line the session that started a refresh prints.
pub const STARTED_LINE: &str = "Refresh started in the background.";

/// A failed provider is not due again from the trigger for this long.
pub const HOLD_AFTER_FAILURE: TimeDelta = TimeDelta::hours(1);

/// Starts `wardwell tracker pull --project <key> --provider <p>...` for the
/// due providers without waiting for it. Injected so no test starts a real
/// process.
pub trait Spawner {
    fn spawn(&self, project: &str, providers: &[&str]) -> Result<(), String>;
}

/// What `refresh` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The refresh state cannot be written at this path, so a pull could
    /// not record itself; nothing starts.
    StateUnwritable(PathBuf),
    /// A detached pull was started.
    Started,
    /// The project has no tracker binding.
    NoBinding,
    /// Every binding pulled within the last hour.
    NotDue,
    /// Every binding that is due failed within the last hour.
    Held,
    /// A pull of the project is running in a live process.
    Running,
    /// A start, completion or failure is younger than the cooldown.
    Cooldown,
    /// Every due binding fails the offline check, so a pull cannot run.
    Blocked,
    /// Another start holds the project's claim.
    Claimed,
    /// The pull could not start; the state and a `spawn` marker record it.
    SpawnFailed,
}

/// The checks `refresh` asks of the host. Injected so tests choose them.
pub struct Probes<'a> {
    /// Whether a process id is running.
    pub alive: &'a dyn Fn(u32) -> bool,
    /// Whether a binding passes the offline doctor check.
    pub can_pull: &'a dyn Fn(&TrackerBinding) -> bool,
}

/// Where `refresh` reads and writes outside the vault.
pub struct Places<'a> {
    pub config: &'a WardwellConfig,
    pub config_dir: &'a Path,
    /// How long a spawn failure waits for its marker to reach the vault.
    pub vault_bound: std::time::Duration,
}

/// Start a detached pull of `<domain>/<project>` through `spawner` when it
/// is due, deciding from the refresh state file alone. A missing or
/// unreadable state file makes every binding due. Before it spawns it takes
/// the project's claim file; the pull releases it when it ends. A spawn error is
/// recorded as a `spawn` failure in the state and in the log.
pub fn refresh(places: &Places<'_>, domain: &str, project: &str, now: DateTime<Utc>, spawner: &dyn Spawner, probes: &Probes<'_>) -> Outcome {
    let bindings = places.config.bindings_for(domain, project);
    if bindings.is_empty() {
        return Outcome::NoBinding;
    }
    let state_path = state::path(places.config_dir, domain, project);
    let states: BTreeMap<String, ProviderState> = match state::read(&state_path) {
        state::Read::Found(found) => found.providers,
        state::Read::Missing | state::Read::Unreadable => BTreeMap::new(),
    };
    let of = |binding: &TrackerBinding| states.get(&binding.provider).cloned().unwrap_or_default();
    if bindings.iter().any(|b| running(&of(b), now, probes.alive)) {
        return Outcome::Running;
    }
    if bindings.iter().any(|b| cooling(&of(b), now)) {
        return Outcome::Cooldown;
    }
    let stale: Vec<&TrackerBinding> = bindings.iter().copied().filter(|b| is_due(&of(b), now)).collect();
    let due: Vec<&TrackerBinding> = stale.iter().copied().filter(|b| !held(&of(b), now)).collect();
    let pullable: Vec<&TrackerBinding> = due.iter().copied().filter(|b| (probes.can_pull)(b)).collect();
    match (stale.is_empty(), due.is_empty(), pullable.is_empty()) {
        (true, _, _) => return Outcome::NotDue,
        (false, true, _) => return Outcome::Held,
        (false, false, true) => return Outcome::Blocked,
        (false, false, false) => {}
    }
    if let Err(path) = state::check_writable(places.config_dir, domain, project) {
        return Outcome::StateUnwritable(path);
    }
    let claim = state::claim_path(places.config_dir, domain, project);
    if !state::claim(&claim, now) {
        return Outcome::Claimed;
    }
    let providers: Vec<&str> = pullable.iter().map(|b| b.provider.as_str()).collect();
    match spawner.spawn(&format!("{domain}/{project}"), &providers) {
        Ok(()) => Outcome::Started,
        Err(_) => {
            state::release(&claim);
            record_spawn_failure(places, &state_path, &pullable, now);
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
    let places = Places { config, config_dir, vault_bound: crate::tracker::bounded::VAULT_BOUND };
    refresh(&places, domain, project, now, &spawner, &Probes { alive: &freshness::process_alive, can_pull: &can_pull })
}

/// How often the running server asks each bound project for a refresh.
pub const SERVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Ask `refresh` once for each bound project, in config order, and return
/// a log line for each project where a pull started or could not start.
pub fn refresh_bound(config: &WardwellConfig, refresh: &dyn Fn(&str, &str) -> Outcome) -> Vec<String> {
    let mut projects: Vec<(&str, &str)> = Vec::new();
    for binding in &config.trackers {
        let project = (binding.domain.as_str(), binding.project.as_str());
        if !projects.contains(&project) {
            projects.push(project);
        }
    }
    projects
        .into_iter()
        .filter_map(|(domain, project)| match refresh(domain, project) {
            Outcome::Started => Some(format!("tracker refresh started for {domain}/{project}")),
            Outcome::SpawnFailed => Some(format!("tracker refresh for {domain}/{project} could not start (spawn)")),
            Outcome::StateUnwritable(path) => Some(format!("tracker refresh for {domain}/{project} cannot write its state at {}", path.display())),
            _ => None,
        })
        .collect()
}

/// Run `task` every `period`, the first time one `period` after the call.
/// Runs until the process ends.
pub async fn every(period: std::time::Duration, mut task: impl FnMut()) {
    let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        task();
    }
}

/// Every `period`, the first time one `period` after the call, run `round`
/// with `config_dir` on a blocking thread. A round that fails or panics is
/// logged by the round or lost with its thread; it never stops the timer.
pub async fn serve_refresh(period: std::time::Duration, config_dir: PathBuf, round: fn(&Path)) {
    every(period, move || {
        let dir = config_dir.clone();
        tokio::task::spawn_blocking(move || round(&dir));
    })
    .await;
}

/// One server refresh round at `now`: the config read fresh through
/// `load`, then the refresh trigger with the real spawner for every bound
/// project. Returns the lines to log; a config that does not read is one.
pub fn serve_round(config_dir: &Path, load: impl FnOnce() -> Result<WardwellConfig, String>, now: DateTime<Utc>) -> Vec<String> {
    match load() {
        Ok(config) => refresh_bound(&config, &|domain, project| refresh_detached(&config, config_dir, domain, project, now)),
        Err(error) => vec![format!("tracker refresh skipped; config could not be read ({error})")],
    }
}

/// The time since `at`, or None when `at` is in the future: a future stamp
/// counts as old.
fn age(at: DateTime<Utc>, now: DateTime<Utc>) -> Option<TimeDelta> {
    let age = now - at;
    (age >= TimeDelta::zero()).then_some(age)
}

/// A pull started after the last completion and failure, in a live process,
/// younger than the deadline allows.
fn running(state: &ProviderState, now: DateTime<Utc>, alive: &dyn Fn(u32) -> bool) -> bool {
    state.open_start().is_some_and(|(at, pid)| {
        age(at, now).is_some_and(|age| age < freshness::running_at_most()) && pid.is_some_and(alive)
    })
}

/// The newest start, completion or failure is younger than `COOLDOWN`.
fn cooling(state: &ProviderState, now: DateTime<Utc>) -> bool {
    let cooldown = TimeDelta::from_std(COOLDOWN).unwrap_or(TimeDelta::seconds(60));
    [state.started_at, state.completed_at, state.failed_at].into_iter().flatten().filter_map(|at| age(at, now)).any(|age| age < cooldown)
}

/// The last completed pull is older than `REFRESH_AFTER`, in the future, or absent.
fn is_due(state: &ProviderState, now: DateTime<Utc>) -> bool {
    state.completed_at.and_then(|at| age(at, now)).is_none_or(|age| age > REFRESH_AFTER)
}

/// The provider failed after its last completion, less than
/// `HOLD_AFTER_FAILURE` ago. A failure stamped in the future counts as old.
fn held(state: &ProviderState, now: DateTime<Utc>) -> bool {
    state.open_failure().and_then(|(at, _)| age(at, now)).is_some_and(|age| age < HOLD_AFTER_FAILURE)
}

/// Best effort: a `spawn` failure in the state and a marker in the log for each binding.
fn record_spawn_failure(places: &Places<'_>, state_path: &Path, bindings: &[&TrackerBinding], now: DateTime<Utc>) {
    for binding in bindings {
        let _ = state::record(state_path, &binding.provider, state::Record::Failed(FailureCode::Spawn), now);
    }
    // The markers go to the vault on a helper thread; past the bound they
    // are skipped, and the local state already holds the failure.
    let markers: Vec<(PathBuf, crate::tracker::events::Event)> = bindings
        .iter()
        .map(|b| (crate::tracker::log::path_for(&places.config.vault_path, &b.domain, &b.project), crate::tracker::pull::pull_failed(b, now, FailureCode::Spawn, false)))
        .collect();
    let _ = crate::tracker::bounded::run(places.vault_bound, move || {
        for (log, marker) in &markers {
            let _ = crate::tracker::log::append_marker(log, marker);
        }
    });
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
    fn spawn(&self, project: &str, providers: &[&str]) -> Result<(), String> {
        use std::io::Write;
        let mut args = vec!["tracker", "pull", "--project", project];
        providers.iter().for_each(|provider| args.extend(["--provider", provider]));
        let mut log = self.open_log()?;
        let _ = writeln!(log, "{} refresh: {}", freshness::stamp(Utc::now()), args.join(" "));
        let errors = log.try_clone().map_err(|_| format!("could not open {}", self.log.display()))?;
        let mut command = std::process::Command::new(&self.program);
        command
            .args(&args)
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
    use crate::tracker::state::Record;
    use chrono::TimeZone;
    use std::cell::RefCell;

    /// Records each project it is asked to start, fails when told to, and
    /// can record the start the real child would record.
    struct Fake {
        started: RefCell<Vec<String>>,
        fail: bool,
        records_start: Option<(PathBuf, u32)>,
    }

    impl Fake {
        fn new() -> Self {
            Fake { started: RefCell::new(Vec::new()), fail: false, records_start: None }
        }
    }

    impl Spawner for Fake {
        fn spawn(&self, project: &str, providers: &[&str]) -> Result<(), String> {
            self.started.borrow_mut().push(format!("{project} {}", providers.join(",")));
            if let Some((path, pid)) = &self.records_start {
                state::record(path, "linear", Record::Started(*pid), now()).unwrap();
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

    fn ago(minutes: i64) -> DateTime<Utc> {
        now() - TimeDelta::minutes(minutes)
    }

    struct Setup {
        _dir: tempfile::TempDir,
        config: WardwellConfig,
        config_dir: PathBuf,
        state: PathBuf,
    }

    /// A config binding work/claims to linear, and to github when `both`.
    /// Its vault folder does not exist, so any vault read would find nothing.
    fn setup(both: bool) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let github = if both { "    - provider: github\n      repository: acme/app\n" } else { "" };
        let yaml = format!(
            "vault_path: {}\nsession_sources: []\ntrackers:\n  work/claims:\n    - provider: linear\n      team: COR\n      credential: c\n{github}",
            dir.path().join("no-vault").display()
        );
        let config_dir = dir.path().join("cfg");
        let state = state::path(&config_dir, "work", "claims");
        Setup { config: crate::config::loader::parse(&yaml).unwrap(), config_dir, state, _dir: dir }
    }

    fn record(setup: &Setup, provider: &str, record: Record, at: DateTime<Utc>) {
        state::record(&setup.state, provider, record, at).unwrap();
    }

    fn run(setup: &Setup, spawner: &Fake, alive: bool, can_pull: bool) -> Outcome {
        let alive = move |_: u32| alive;
        let can_pull = move |_: &TrackerBinding| can_pull;
        let places = Places { config: &setup.config, config_dir: &setup.config_dir, vault_bound: crate::tracker::bounded::VAULT_BOUND };
        refresh(&places, "work", "claims", now(), spawner, &Probes { alive: &alive, can_pull: &can_pull })
    }

    #[test]
    fn a_pull_older_than_an_hour_starts_one_detached_pull_for_the_project() {
        let s = setup(false);
        record(&s, "linear", Record::Completed, ago(61));
        let fake = Fake::new();
        assert_eq!(run(&s, &fake, false, true), Outcome::Started);
        assert_eq!(*fake.started.borrow(), vec!["work/claims linear"]);
    }

    #[test]
    fn a_missing_or_unreadable_state_file_is_due() {
        let s = setup(false);
        assert_eq!(run(&s, &Fake::new(), false, true), Outcome::Started);
        assert_eq!(run(&s, &Fake::new(), false, true), Outcome::Claimed, "due once: the claim holds until the pull ends");
        state::release(&state::claim_path(&s.config_dir, "work", "claims"));
        std::fs::create_dir_all(s.state.parent().unwrap()).unwrap();
        std::fs::write(&s.state, "{torn").unwrap();
        assert_eq!(run(&s, &Fake::new(), false, true), Outcome::Started);
    }

    #[test]
    fn the_trigger_never_reads_the_vault() {
        let s = setup(false);
        record(&s, "linear", Record::Completed, ago(10));
        let log = crate::tracker::log::path_for(&s.config.vault_path, "work", "claims");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "not a log at all").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o000)).unwrap();
        }
        assert_eq!(run(&s, &Fake::new(), false, true), Outcome::NotDue, "decided from the state file alone");
    }

    #[test]
    fn a_pull_within_the_hour_is_not_due() {
        let s = setup(false);
        record(&s, "linear", Record::Completed, ago(59));
        let fake = Fake::new();
        assert_eq!(run(&s, &fake, false, true), Outcome::NotDue);
        assert!(fake.started.borrow().is_empty());
    }

    #[test]
    fn any_due_binding_starts_one_pull_for_the_whole_project() {
        let s = setup(true);
        record(&s, "linear", Record::Completed, ago(10));
        record(&s, "github", Record::Completed, ago(120));
        let fake = Fake::new();
        assert_eq!(run(&s, &fake, false, true), Outcome::Started);
        assert_eq!(fake.started.borrow().len(), 1);
    }

    #[test]
    fn a_live_pull_of_any_binding_stops_a_second_start() {
        let s = setup(true);
        record(&s, "linear", Record::Completed, ago(120));
        record(&s, "github", Record::Started(4242), ago(3));
        let fake = Fake::new();
        assert_eq!(run(&s, &fake, true, true), Outcome::Running);
        assert!(fake.started.borrow().is_empty());
    }

    #[test]
    fn the_cooldown_covers_failures_and_starts() {
        let s = setup(false);
        record(&s, "linear", Record::Completed, ago(120));
        record(&s, "linear", Record::Started(4242), now() - TimeDelta::seconds(30));
        assert_eq!(run(&s, &Fake::new(), false, true), Outcome::Cooldown, "a start whose process is gone still cools down");
        record(&s, "linear", Record::Started(4242), ago(2));
        assert_eq!(run(&s, &Fake::new(), false, true), Outcome::Started, "an unfinished pull is retried after the cooldown");
    }

    #[test]
    fn a_failure_under_an_hour_old_holds_the_provider() {
        let s = setup(false);
        record(&s, "linear", Record::Completed, ago(180));
        record(&s, "linear", Record::Failed(FailureCode::Provider), ago(2));
        let fake = Fake::new();
        assert_eq!(run(&s, &fake, false, true), Outcome::Held);
        assert!(fake.started.borrow().is_empty());
        record(&s, "linear", Record::Failed(FailureCode::Provider), ago(61));
        assert_eq!(run(&s, &fake, false, true), Outcome::Started, "the hold ends after an hour");
    }

    #[test]
    fn a_held_provider_does_not_make_its_healthy_sibling_pull_again() {
        let s = setup(true);
        record(&s, "linear", Record::Completed, ago(180));
        record(&s, "linear", Record::Failed(FailureCode::Auth), ago(5));
        record(&s, "github", Record::Completed, ago(30));
        assert_eq!(run(&s, &Fake::new(), false, true), Outcome::Held, "github is fresh and linear is held");
        record(&s, "github", Record::Completed, ago(90));
        let fake = Fake::new();
        assert_eq!(run(&s, &fake, false, true), Outcome::Started);
        assert_eq!(*fake.started.borrow(), vec!["work/claims github"], "only the due provider is pulled");
    }

    #[test]
    fn times_stamped_in_the_future_count_as_old() {
        let s = setup(false);
        record(&s, "linear", Record::Completed, ago(180));
        record(&s, "linear", Record::Failed(FailureCode::Provider), ago(-180));
        assert_eq!(run(&s, &Fake::new(), true, true), Outcome::Started, "a failure 3 hours ahead does not hold");
        state::release(&state::claim_path(&s.config_dir, "work", "claims"));
        let s = setup(false);
        record(&s, "linear", Record::Completed, ago(180));
        record(&s, "linear", Record::Started(4242), ago(-10));
        assert_eq!(run(&s, &Fake::new(), true, true), Outcome::Started, "a start 10 minutes ahead is neither running nor cooling");
        let s = setup(false);
        record(&s, "linear", Record::Completed, ago(-180));
        assert_eq!(run(&s, &Fake::new(), false, true), Outcome::Started, "a completion 3 hours ahead is not fresh");
    }

    #[test]
    fn a_binding_that_cannot_pull_is_not_started() {
        let s = setup(false);
        let fake = Fake::new();
        assert_eq!(run(&s, &fake, false, false), Outcome::Blocked);
        assert!(fake.started.borrow().is_empty());
    }

    #[test]
    fn no_binding_starts_nothing() {
        let s = setup(false);
        let fake = Fake::new();
        let alive = |_: u32| false;
        let can_pull = |_: &TrackerBinding| true;
        let places = Places { config: &s.config, config_dir: &s.config_dir, vault_bound: crate::tracker::bounded::VAULT_BOUND };
        assert_eq!(refresh(&places, "work", "ops", now(), &fake, &Probes { alive: &alive, can_pull: &can_pull }), Outcome::NoBinding);
        assert!(fake.started.borrow().is_empty());
    }

    #[test]
    fn a_spawn_error_is_recorded_and_not_a_failure_of_the_caller() {
        let s = setup(false);
        record(&s, "linear", Record::Completed, ago(120));
        let fake = Fake { fail: true, ..Fake::new() };
        assert_eq!(run(&s, &fake, false, true), Outcome::SpawnFailed);
        assert_eq!(state::provider(&s.state, "linear").unwrap().open_failure(), Some((now(), FailureCode::Spawn)));
        let log = crate::tracker::log::path_for(&s.config.vault_path, "work", "claims");
        let view = crate::tracker::view::MirrorView::read_for(&log, "linear").unwrap();
        assert_eq!(view.last_failure, Some((now(), FailureCode::Spawn)), "the marker is the trace");
        assert_eq!(run(&s, &Fake::new(), false, true), Outcome::Cooldown, "the failure cools the next start down");
    }

    #[cfg(unix)]
    #[test]
    fn a_spawn_failure_does_not_wait_on_a_vault_that_does_not_answer() {
        let s = setup(false);
        let log = crate::tracker::log::path_for(&s.config.vault_path, "work", "claims");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        assert!(std::process::Command::new("/usr/bin/mkfifo").arg(&log).status().unwrap().success());
        let alive = |_: u32| false;
        let can_pull = |_: &TrackerBinding| true;
        let places = Places { config: &s.config, config_dir: &s.config_dir, vault_bound: std::time::Duration::from_millis(100) };
        let started = std::time::Instant::now();
        let outcome = refresh(&places, "work", "claims", now(), &Fake { fail: true, ..Fake::new() }, &Probes { alive: &alive, can_pull: &can_pull });
        assert_eq!(outcome, Outcome::SpawnFailed);
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "{:?}", started.elapsed());
        assert_eq!(state::provider(&s.state, "linear").unwrap().open_failure(), Some((now(), FailureCode::Spawn)), "the local state holds it first");
    }

    #[cfg(unix)]
    #[test]
    fn an_unwritable_refresh_location_starts_nothing_and_names_the_path() {
        use std::os::unix::fs::PermissionsExt;
        let s = setup(false);
        let folder = s.state.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&s.state).unwrap();
        let fake = Fake::new();
        assert_eq!(run(&s, &fake, false, true), Outcome::StateUnwritable(s.state.clone()), "a directory at the state path");
        assert_eq!(run(&s, &fake, false, true), Outcome::StateUnwritable(s.state.clone()), "and again, without a start");
        std::fs::remove_dir(&s.state).unwrap();
        std::fs::remove_dir(&folder).unwrap();
        std::fs::write(&folder, "a file").unwrap();
        assert_eq!(run(&s, &fake, false, true), Outcome::StateUnwritable(folder.clone()), "a file at the folder path");
        std::fs::remove_file(&folder).unwrap();
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o500)).unwrap();
        let outcome = run(&s, &fake, false, true);
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(outcome, Outcome::StateUnwritable(folder.clone()), "a folder that cannot be written");
        assert!(fake.started.borrow().is_empty(), "nothing started");
    }

    #[test]
    fn a_second_call_after_the_start_does_not_start_again() {
        let s = setup(false);
        record(&s, "linear", Record::Completed, ago(120));
        let fake = Fake { records_start: Some((s.state.clone(), 4242)), ..Fake::new() };
        assert_eq!(run(&s, &fake, true, true), Outcome::Started);
        assert_eq!(run(&s, &fake, true, true), Outcome::Running);
        assert_eq!(fake.started.borrow().len(), 1);
    }

    /// Counts starts across threads.
    struct Counting(std::sync::Mutex<usize>);

    impl Spawner for Counting {
        fn spawn(&self, _: &str, _: &[&str]) -> Result<(), String> {
            *self.0.lock().unwrap() += 1;
            Ok(())
        }
    }

    #[test]
    fn twenty_parallel_triggers_against_one_stale_project_start_one_pull() {
        let s = setup(false);
        record(&s, "linear", Record::Completed, ago(120));
        let spawner = Counting(std::sync::Mutex::new(0));
        let barrier = std::sync::Barrier::new(20);
        let outcomes: Vec<Outcome> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..20)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        let alive = |_: u32| false;
                        let can_pull = |_: &TrackerBinding| true;
                        let places = Places { config: &s.config, config_dir: &s.config_dir, vault_bound: crate::tracker::bounded::VAULT_BOUND };
                        refresh(&places, "work", "claims", Utc::now(), &spawner, &Probes { alive: &alive, can_pull: &can_pull })
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(*spawner.0.lock().unwrap(), 1, "{outcomes:?}");
        assert_eq!(outcomes.iter().filter(|o| **o == Outcome::Started).count(), 1);
        assert!(outcomes.iter().all(|o| matches!(o, Outcome::Started | Outcome::Claimed)), "{outcomes:?}");
    }

    #[test]
    fn a_spawn_error_releases_the_claim() {
        let s = setup(false);
        assert_eq!(run(&s, &Fake { fail: true, ..Fake::new() }, false, true), Outcome::SpawnFailed);
        assert!(!state::claim_path(&s.config_dir, "work", "claims").exists());
    }

    #[test]
    fn the_server_asks_each_bound_project_once_and_logs_starts_and_spawn_failures() {
        let yaml = "vault_path: /v\nsession_sources: []\ntrackers:\n  work/claims:\n    - provider: linear\n      team: COR\n      credential: c\n    - provider: github\n      repository: acme/app\n  work/ops:\n    provider: linear\n    team: OPS\n    credential: c\n  home/notes:\n    provider: linear\n    team: NOTE\n    credential: c\n";
        let config = crate::config::loader::parse(yaml).unwrap();
        let asked = RefCell::new(Vec::new());
        let refresh = |domain: &str, project: &str| {
            asked.borrow_mut().push(format!("{domain}/{project}"));
            match project {
                "claims" => Outcome::Started,
                "ops" => Outcome::SpawnFailed,
                _ => Outcome::NotDue,
            }
        };
        let mut lines = refresh_bound(&config, &refresh);
        let mut seen = asked.borrow().clone();
        seen.sort();
        assert_eq!(seen, vec!["home/notes", "work/claims", "work/ops"]);
        lines.sort();
        assert_eq!(lines, vec!["tracker refresh for work/ops could not start (spawn)", "tracker refresh started for work/claims"]);
    }

    #[test]
    fn the_timer_first_fires_one_period_after_it_starts() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let count = std::rc::Rc::new(std::cell::Cell::new(0));
        let seen = std::rc::Rc::clone(&count);
        runtime.block_on(async {
            let timer = every(std::time::Duration::from_millis(100), move || seen.set(seen.get() + 1));
            let _ = tokio::time::timeout(std::time::Duration::from_millis(50), timer).await;
        });
        assert_eq!(count.get(), 0, "nothing at start");
        let seen = std::rc::Rc::clone(&count);
        runtime.block_on(async {
            let timer = every(std::time::Duration::from_millis(40), move || seen.set(seen.get() + 1));
            let _ = tokio::time::timeout(std::time::Duration::from_millis(150), timer).await;
        });
        assert!((2..=4).contains(&count.get()), "{}", count.get());
    }

    static ROUNDS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn count_round(dir: &Path) {
        assert!(dir.ends_with("cfg"));
        ROUNDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    #[test]
    fn the_serve_timer_runs_rounds_on_its_injected_interval_after_one_interval() {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_time().build().unwrap();
        runtime.block_on(async {
            let timer = tokio::spawn(serve_refresh(std::time::Duration::from_millis(100), PathBuf::from("/nowhere/cfg"), count_round));
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert_eq!(ROUNDS.load(std::sync::atomic::Ordering::SeqCst), 0, "nothing at start");
            tokio::time::sleep(std::time::Duration::from_millis(330)).await;
            timer.abort();
        });
        let rounds = ROUNDS.load(std::sync::atomic::Ordering::SeqCst);
        assert!((2..=4).contains(&rounds), "{rounds}");
    }

    #[test]
    fn a_serve_round_logs_a_config_that_does_not_read_and_does_not_fail() {
        let lines = serve_round(Path::new("/nowhere"), || Err("bad yaml".to_string()), now());
        assert_eq!(lines, vec!["tracker refresh skipped; config could not be read (bad yaml)"]);
    }

    #[test]
    fn the_detached_spawner_reports_a_missing_program_as_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let spawner = DetachedPull { program: dir.path().join("gone/wardwell"), log: dir.path().join("tracker-pull.log") };
        let error = spawner.spawn("work/claims", &["github"]).unwrap_err();
        assert!(error.contains("could not start"), "{error}");
        let log = std::fs::read_to_string(dir.path().join("tracker-pull.log")).unwrap();
        assert!(log.ends_with("refresh: tracker pull --project work/claims --provider github\n"), "{log}");
        let unopenable = DetachedPull { program: dir.path().join("wardwell"), log: dir.path().join("no/such/dir/log") };
        assert!(unopenable.spawn("work/claims", &["github"]).unwrap_err().contains("could not open"));
    }
}
