//! `wardwell tracker doctor`: per binding, checks the credential file, one
//! cheap authenticated request, and that the bound team or repository
//! resolves. One line per check, a closed code on failure, never a token or
//! provider text.
//!
//! Does NOT pull or write anything.

use crate::config::loader::{TrackerBinding, WardwellConfig};
use crate::tracker::credential::{self, Credential};
use crate::tracker::events::FailureCode;
use crate::tracker::github::{GitHub, SystemGh};
use crate::tracker::linear::{self, HttpTransport, Transport};
use std::collections::BTreeMap;
use std::path::Path;

/// Builds the provider transport for a binding. Injected so tests never
/// reach a network.
pub type Probe<'a> = dyn Fn(&TrackerBinding, &Credential) -> Result<Box<dyn Transport>, String> + 'a;

/// Builds the GitHub reader for a binding. Injected so tests never start
/// `gh` or reach a network.
pub type GithubProbe<'a> = dyn Fn(&TrackerBinding, Option<&Credential>) -> GitHub + 'a;

/// The production GitHub reader: `gh` on PATH, then the stored token.
pub fn connect_github(binding: &TrackerBinding, credential: Option<&Credential>) -> GitHub {
    crate::tracker::pull::github_for(binding, credential, Box::new(SystemGh))
}

/// The production transport for a binding's provider.
pub fn connect_transport(binding: &TrackerBinding, credential: &Credential) -> Result<Box<dyn Transport>, String> {
    match binding.provider.as_str() {
        "linear" => Ok(Box::new(HttpTransport::new(credential.token().to_string()))),
        other => Err(format!("unsupported tracker provider '{other}'")),
    }
}

/// Per issue binding, four lines: credential, auth, team, kanban prefix;
/// `provider` takes the place of auth when the provider has no adapter. Per
/// github binding, two lines: token and repository. The bool is true when
/// every check passed.
pub fn run(config: &WardwellConfig, config_dir: &Path, probe: &Probe<'_>, github: &GithubProbe<'_>) -> (Vec<String>, bool) {
    run_with(config, config_dir, probe, github, &native_prefixes(config, &config_dir.join("kanban.db")))
}

/// `run`, with each binding's native kanban prefix given by binding key.
pub fn run_with(config: &WardwellConfig, config_dir: &Path, probe: &Probe<'_>, github: &GithubProbe<'_>, native: &BTreeMap<String, String>) -> (Vec<String>, bool) {
    if config.trackers.is_empty() {
        return (vec!["No trackers bound. Add a trackers section to config.yml.".to_string()], true);
    }
    let mut lines = Vec::new();
    let mut healthy = true;
    for binding in &config.trackers {
        let key = binding.key();
        let checks = match crate::tracker::mirrors_issues(&binding.provider) {
            true => {
                let mut checks = check_binding(config_dir, binding, probe);
                checks.push(("kanban prefix".to_string(), prefix_outcome(binding, native.get(&key))));
                checks
            }
            false => check_github(config_dir, binding, github),
        };
        healthy &= checks.iter().all(|(_, outcome)| matches!(outcome, Outcome::Ok | Outcome::Found(_)));
        lines.extend(checks.into_iter().map(|(name, outcome)| format!("{key}: {name} {}", outcome.describe())));
    }
    (lines, healthy)
}

/// The checks that need no network: the credential file exists with
/// owner-only permissions, and Wardwell has an adapter for the provider.
/// The error is a closed code and, for the credential, the path and fix.
/// A github binding passes without a token when `gh` is on PATH.
pub fn check_offline(config_dir: &Path, binding: &TrackerBinding) -> Result<(), (FailureCode, Option<String>)> {
    check_offline_with(config_dir, binding, crate::tracker::github::gh_on_path())
}

/// `check_offline`, told whether `gh` is on PATH.
pub fn check_offline_with(config_dir: &Path, binding: &TrackerBinding, gh_on_path: bool) -> Result<(), (FailureCode, Option<String>)> {
    if !crate::tracker::mirrors_issues(&binding.provider) {
        let stored = crate::tracker::pull::load_credential(config_dir, binding).map_err(|e| (e.code, Some(e.message)))?;
        return match (stored.is_some(), gh_on_path) {
            (false, false) => Err((FailureCode::Credential, Some(crate::tracker::github::unreachable_line(&binding.credential)))),
            _ => Ok(()),
        };
    }
    credential::path_in(config_dir, &binding.credential)
        .and_then(|path| credential::load(&path))
        .map_err(|message| (FailureCode::Credential, Some(message)))?;
    match crate::tracker::SUPPORTED_PROVIDERS.contains(&binding.provider.as_str()) {
        true => Ok(()),
        false => Err((FailureCode::UnsupportedProvider, None)),
    }
}

/// The native kanban prefix of each bound project, by binding key, read
/// from the kanban database at `kanban_db` without writing or creating it.
pub fn native_prefixes(config: &WardwellConfig, kanban_db: &Path) -> BTreeMap<String, String> {
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let Ok(conn) = rusqlite::Connection::open_with_flags(kanban_db, flags) else {
        return BTreeMap::new();
    };
    config
        .issue_bindings()
        .filter_map(|b| crate::kanban::store::native_prefix_in(&conn, &b.project, &config.kanban_prefixes).map(|prefix| (b.key(), prefix)))
        .collect()
}

/// The prefix check as a doctor outcome, with the collision sentence.
pub fn prefix_failure(binding: &TrackerBinding, native: Option<&String>) -> Option<String> {
    native.and_then(|prefix| crate::tracker::prefix_collision(binding, prefix))
}

fn prefix_outcome(binding: &TrackerBinding, native: Option<&String>) -> Outcome {
    prefix_failure(binding, native).map_or(Outcome::Ok, |sentence| Outcome::Failed(FailureCode::PrefixCollision, Some(sentence)))
}

enum Outcome {
    Ok,
    /// Passed, with what the check found.
    Found(String),
    Failed(FailureCode, Option<String>),
    Skipped,
}

impl Outcome {
    fn describe(&self) -> String {
        match self {
            Outcome::Ok => "ok".to_string(),
            Outcome::Found(text) => text.clone(),
            Outcome::Failed(code, Some(detail)) => format!("failed ({}): {detail}", code.as_str()),
            Outcome::Failed(code, None) => format!("failed ({})", code.as_str()),
            Outcome::Skipped => "skipped".to_string(),
        }
    }

    fn from(result: Result<(), FailureCode>) -> Self {
        result.map_or_else(|code| Outcome::Failed(code, None), |_| Outcome::Ok)
    }
}

/// Token and repository lines for a github binding. A missing token is not
/// a failure while `gh` can read; a token that fails its checks is.
fn check_github(config_dir: &Path, binding: &TrackerBinding, github: &GithubProbe<'_>) -> Vec<(String, Outcome)> {
    let repository = format!("github repository {}", binding.scope());
    let credential = match crate::tracker::pull::load_credential(config_dir, binding) {
        Ok(credential) => credential,
        Err(error) => {
            return vec![
                ("github token".to_string(), Outcome::Failed(error.code, Some(error.message))),
                (repository, Outcome::Skipped),
            ];
        }
    };
    let token = match credential.is_some() {
        true => Outcome::Ok,
        false => Outcome::Found("not stored; gh reads alone".to_string()),
    };
    let reader = github(binding, credential.as_ref());
    let reached = match reader.check() {
        Ok(route) => Outcome::Found(format!("ok {}", route.describe())),
        Err(FailureCode::Credential) => Outcome::Failed(FailureCode::Credential, Some(reader.unreachable_message())),
        Err(code) => Outcome::Failed(code, None),
    };
    vec![("github token".to_string(), token), (repository, reached)]
}

fn check_binding(config_dir: &Path, binding: &TrackerBinding, probe: &Probe<'_>) -> Vec<(String, Outcome)> {
    let team = format!("team {}", binding.team);
    // Credential errors name the path and the fix, never the file contents.
    let credential = match credential::path_in(config_dir, &binding.credential).and_then(|path| credential::load(&path)) {
        Ok(credential) => credential,
        Err(message) => {
            return vec![
                ("credential".to_string(), Outcome::Failed(FailureCode::Credential, Some(message))),
                ("auth".to_string(), Outcome::Skipped),
                (team, Outcome::Skipped),
            ];
        }
    };
    let Ok(transport) = probe(binding, &credential) else {
        return vec![
            ("credential".to_string(), Outcome::Ok),
            ("provider".to_string(), Outcome::Failed(FailureCode::UnsupportedProvider, None)),
            (team, Outcome::Skipped),
        ];
    };
    let auth = Outcome::from(linear::check_auth(transport.as_ref()));
    let team_outcome = match auth {
        Outcome::Ok => Outcome::from(linear::check_team(transport.as_ref(), &binding.team)),
        _ => Outcome::Skipped,
    };
    vec![("credential".to_string(), Outcome::Ok), ("auth".to_string(), auth), (team, team_outcome)]
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// Answers the viewer and team queries with canned replies.
    struct Canned {
        viewer: Result<Value, String>,
        teams: Value,
    }

    impl Transport for Canned {
        fn post(&self, body: &Value) -> Result<Value, String> {
            match body["query"].as_str().unwrap_or_default().contains("viewer") {
                true => self.viewer.clone(),
                false => Ok(json!({"data": {"teams": {"nodes": self.teams.clone()}}})),
            }
        }
    }

    fn viewer_ok() -> Result<Value, String> {
        Ok(json!({"data": {"viewer": {"id": "u1"}}}))
    }

    fn setup(token: bool) -> (tempfile::TempDir, WardwellConfig) {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        let yaml = format!(
            "vault_path: {}\nsession_sources: []\ntrackers:\n  work/claims:\n    provider: linear\n    team: COR\n    credential: corr-linear\n",
            vault.display()
        );
        let config_path = dir.path().join("config.yml");
        std::fs::write(&config_path, yaml).unwrap();
        if token {
            let path = credential::path_in(dir.path(), "corr-linear").unwrap();
            credential::save(&path, "lin_api_secret").unwrap();
        }
        let config = crate::config::loader::load(Some(&config_path)).unwrap();
        (dir, config)
    }

    fn doctor_with(dir: &Path, config: &WardwellConfig, viewer: Result<Value, String>, teams: Value) -> (Vec<String>, bool) {
        let probe = move |_: &TrackerBinding, c: &Credential| -> Result<Box<dyn Transport>, String> {
            assert_eq!(c.token(), "lin_api_secret");
            Ok(Box::new(Canned { viewer: viewer.clone(), teams: teams.clone() }))
        };
        run(config, dir, &probe, &no_gh)
    }

    /// A GitHub reader whose `gh` is missing and which reads REST from `rest`.
    fn no_gh(binding: &TrackerBinding, credential: Option<&Credential>) -> GitHub {
        let (gh, _) = crate::tracker::github::tests::gh_with(crate::tracker::github::GhOutcome::Missing);
        crate::tracker::pull::github_for(binding, credential, gh)
    }

    #[test]
    fn healthy_binding_prints_one_ok_line_per_check() {
        let (dir, config) = setup(true);
        let (lines, healthy) = doctor_with(dir.path(), &config, viewer_ok(), json!([{"key": "COR"}]));
        assert_eq!(lines, vec!["work/claims: credential ok", "work/claims: auth ok", "work/claims: team COR ok", "work/claims: kanban prefix ok"]);
        assert!(healthy);
    }

    #[test]
    fn missing_credential_fails_and_skips_the_remote_checks() {
        let (dir, config) = setup(false);
        let (lines, healthy) = doctor_with(dir.path(), &config, viewer_ok(), json!([{"key": "COR"}]));
        assert!(lines[0].starts_with("work/claims: credential failed (credential): tracker credential not configured"), "{}", lines[0]);
        assert_eq!(&lines[1..3], ["work/claims: auth skipped", "work/claims: team COR skipped"]);
        assert!(!healthy);
    }

    #[cfg(unix)]
    #[test]
    fn loose_credential_permissions_fail_the_credential_check() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, config) = setup(true);
        let path = credential::path_in(dir.path(), "corr-linear").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let (lines, healthy) = doctor_with(dir.path(), &config, viewer_ok(), json!([{"key": "COR"}]));
        assert_eq!(lines[0], "work/claims: credential failed (credential): Tracker credential file permissions must be private");
        assert!(!healthy);
    }

    #[test]
    fn refused_token_fails_auth_without_echoing_anything() {
        let (dir, config) = setup(true);
        for viewer in [
            Err(format!("Linear returned HTTP 401: {}", crate::tracker::adapter::AUTH_REFUSED)),
            Ok(json!({"errors": [{"message": "Authentication required for lin_api_secret", "extensions": {"code": "AUTHENTICATION_ERROR"}}]})),
            Ok(json!({"errors": [{"message": "Authentication required", "extensions": {"type": "authentication error"}}]})),
        ] {
            let (lines, healthy) = doctor_with(dir.path(), &config, viewer, json!([{"key": "COR"}]));
            assert_eq!(&lines[1..3], ["work/claims: auth failed (auth)", "work/claims: team COR skipped"]);
            assert!(!lines.join("\n").contains("lin_api_secret"));
            assert!(!healthy);
        }
    }

    #[test]
    fn unreachable_provider_is_a_provider_failure() {
        let (dir, config) = setup(true);
        let (lines, _) = doctor_with(dir.path(), &config, Err("Linear request failed".into()), json!([]));
        assert_eq!(lines[1], "work/claims: auth failed (provider)");
    }

    #[test]
    fn a_graphql_error_that_is_not_authentication_is_a_provider_failure() {
        let (dir, config) = setup(true);
        let viewer = Ok(json!({"errors": [{"message": "Rate limit exceeded", "extensions": {"code": "RATELIMITED"}}]}));
        let (lines, healthy) = doctor_with(dir.path(), &config, viewer, json!([{"key": "COR"}]));
        assert_eq!(&lines[1..3], ["work/claims: auth failed (provider)", "work/claims: team COR skipped"]);
        assert!(!healthy);
    }

    #[test]
    fn an_unknown_provider_is_a_provider_failure_not_a_credential_one() {
        let (dir, mut config) = setup(true);
        config.trackers[0].provider = "jira".into();
        let (lines, healthy) = run(&config, dir.path(), &connect_transport, &no_gh);
        assert_eq!(
            lines,
            vec!["work/claims: credential ok", "work/claims: provider failed (unsupported_provider)", "work/claims: team COR skipped", "work/claims: kanban prefix ok"]
        );
        assert!(!healthy);
    }

    #[test]
    fn offline_checks_read_the_credential_and_provider_without_a_probe() {
        let (dir, config) = setup(true);
        let binding = &config.trackers[0];
        assert_eq!(check_offline(dir.path(), binding), Ok(()));
        let mut unknown = binding.clone();
        unknown.provider = "jira".into();
        assert_eq!(check_offline(dir.path(), &unknown), Err((FailureCode::UnsupportedProvider, None)));
        let (empty, config) = setup(false);
        let (code, detail) = check_offline(empty.path(), &config.trackers[0]).unwrap_err();
        assert_eq!(code, FailureCode::Credential);
        assert!(detail.unwrap().contains("not configured"));
    }

    #[test]
    fn a_team_key_equal_to_a_native_prefix_fails_the_prefix_check() {
        let (dir, config) = setup(true);
        let native = std::collections::BTreeMap::from([("work/claims".to_string(), "COR".to_string())]);
        let probe = |_: &TrackerBinding, _: &Credential| -> Result<Box<dyn Transport>, String> {
            Ok(Box::new(Canned { viewer: viewer_ok(), teams: json!([{"key": "COR"}]) }))
        };
        let (lines, healthy) = run_with(&config, dir.path(), &probe, &no_gh, &native);
        assert_eq!(
            lines.last().unwrap(),
            "work/claims: kanban prefix failed (prefix_collision): Tracker team key COR of work/claims equals the native kanban prefix COR of project claims. Set a different native prefix for claims in kanban.prefixes."
        );
        assert!(!healthy);
        let other = std::collections::BTreeMap::from([("work/claims".to_string(), "CL".to_string())]);
        let (lines, healthy) = run_with(&config, dir.path(), &probe, &no_gh, &other);
        assert_eq!(lines.last().unwrap(), "work/claims: kanban prefix ok");
        assert!(healthy);
    }

    #[test]
    fn native_prefixes_read_the_store_without_creating_it() {
        let (dir, config) = setup(true);
        let db = dir.path().join("kanban.db");
        assert!(native_prefixes(&config, &db).is_empty());
        assert!(!db.exists(), "doctor never creates the kanban database");
        let store = crate::kanban::store::KanbanStore::open(&db, config.vault_path.clone()).unwrap();
        store.create_item("t", "claims", "work", None, None, None, None, None, None, None, None, None, &std::collections::HashMap::new()).unwrap();
        drop(store);
        assert_eq!(native_prefixes(&config, &db)["work/claims"], "CL");
    }

    fn github_config(dir: &Path) -> WardwellConfig {
        let yaml = format!(
            "vault_path: {}\nsession_sources: []\ntrackers:\n  work/claims:\n    - provider: linear\n      team: COR\n      credential: corr-linear\n    - provider: github\n      repository: acme/app\n",
            dir.join("vault").display()
        );
        crate::config::loader::parse(&yaml).unwrap()
    }

    /// A GitHub reader with `gh` giving `outcome` and REST answering `rest`.
    fn github_probe(outcome: crate::tracker::github::GhOutcome, rest: Option<Result<Value, String>>) -> impl Fn(&TrackerBinding, Option<&Credential>) -> GitHub {
        move |binding, credential| {
            let (gh, _) = crate::tracker::github::tests::gh_with(outcome.clone());
            let rest = credential.and(rest.clone()).map(|answer| {
                Box::new(crate::tracker::github::tests::FakeRest { pages: vec![answer], calls: Default::default() }) as Box<dyn crate::tracker::github::Rest>
            });
            GitHub::new(binding.scope(), &binding.credential, gh, rest)
        }
    }

    fn linear_ok(_: &TrackerBinding, _: &Credential) -> Result<Box<dyn Transport>, String> {
        Ok(Box::new(Canned { viewer: viewer_ok(), teams: json!([{"key": "COR"}]) }))
    }

    #[test]
    fn a_github_binding_gets_a_token_line_and_a_repository_line() {
        use crate::tracker::github::GhOutcome;
        let dir = tempfile::tempdir().unwrap();
        credential::save(&credential::path_in(dir.path(), "corr-linear").unwrap(), "lin_api_secret").unwrap();
        let config = github_config(dir.path());
        let native = BTreeMap::new();

        let (lines, healthy) = run_with(&config, dir.path(), &linear_ok, &github_probe(GhOutcome::Output(b"{}".to_vec()), None), &native);
        assert_eq!(lines[4..], ["work/claims: github token not stored; gh reads alone", "work/claims: github repository acme/app ok through gh"]);
        assert_eq!(lines[0], "work/claims: credential ok", "the linear lines are unchanged");
        assert!(healthy, "{lines:?}");

        let (lines, healthy) = run_with(&config, dir.path(), &linear_ok, &github_probe(GhOutcome::Missing, None), &native);
        assert_eq!(lines[5], "work/claims: github repository acme/app failed (credential): github: unreachable, run `wardwell tracker connect github`");
        assert!(!healthy);

        credential::save(&credential::path_in(dir.path(), "github").unwrap(), "ghp_secret").unwrap();
        let cases = [
            (Ok(json!({"full_name": "acme/app"})), "ok through the API token"),
            (Err(format!("GitHub returned HTTP 401: {}", crate::tracker::adapter::AUTH_REFUSED)), "failed (auth)"),
            (Err("GitHub returned HTTP 404".to_string()), "failed (team_not_found)"),
        ];
        for (answer, expected) in cases {
            let (lines, _) = run_with(&config, dir.path(), &linear_ok, &github_probe(GhOutcome::Failed, Some(answer)), &native);
            assert_eq!(lines[4], "work/claims: github token ok");
            assert_eq!(lines[5], format!("work/claims: github repository acme/app {expected}"));
            assert!(!lines.join("\n").contains("ghp_secret"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_github_token_with_loose_permissions_fails_and_skips_the_repository() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = credential::path_in(dir.path(), "github").unwrap();
        credential::save(&path, "ghp_secret").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let config = github_config(dir.path());
        let (lines, healthy) = run_with(&config, dir.path(), &linear_ok, &github_probe(crate::tracker::github::GhOutcome::Missing, None), &BTreeMap::new());
        assert_eq!(lines[4], "work/claims: github token failed (credential): Tracker credential file permissions must be private");
        assert_eq!(lines[5], "work/claims: github repository acme/app skipped");
        assert!(!healthy);
    }

    #[test]
    fn offline_check_for_github_needs_gh_on_path_or_a_token() {
        let dir = tempfile::tempdir().unwrap();
        let config = github_config(dir.path());
        let github = &config.trackers[1];
        assert_eq!(check_offline_with(dir.path(), github, true), Ok(()));
        assert_eq!(
            check_offline_with(dir.path(), github, false),
            Err((FailureCode::Credential, Some("github: unreachable, run `wardwell tracker connect github`".to_string())))
        );
        credential::save(&credential::path_in(dir.path(), "github").unwrap(), "ghp_secret").unwrap();
        assert_eq!(check_offline_with(dir.path(), github, false), Ok(()));
    }

    #[test]
    fn unknown_team_fails_the_team_check() {
        let (dir, config) = setup(true);
        let (lines, healthy) = doctor_with(dir.path(), &config, viewer_ok(), json!([]));
        assert_eq!(lines[2], "work/claims: team COR failed (team_not_found)");
        assert!(!healthy);
    }

    #[test]
    fn team_query_asks_for_the_bound_key() {
        struct Recording(std::cell::RefCell<Vec<Value>>);
        impl Transport for Recording {
            fn post(&self, body: &Value) -> Result<Value, String> {
                self.0.borrow_mut().push(body.clone());
                Ok(json!({"data": {"teams": {"nodes": [{"key": "COR"}]}}}))
            }
        }
        let recording = Recording(std::cell::RefCell::new(vec![]));
        linear::check_team(&recording, "COR").unwrap();
        assert_eq!(recording.0.borrow()[0]["variables"]["key"], "COR");
    }
}
