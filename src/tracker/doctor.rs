//! `wardwell tracker doctor`: per binding, checks the credential file, one
//! cheap authenticated request, and that the bound team resolves. One line
//! per check, a closed code on failure, never a token or provider text.
//!
//! Does NOT pull or write anything.

use crate::config::loader::{TrackerBinding, WardwellConfig};
use crate::tracker::credential::{self, Credential};
use crate::tracker::events::FailureCode;
use crate::tracker::linear::{self, HttpTransport, Transport};
use std::collections::BTreeMap;
use std::path::Path;

/// Builds the provider transport for a binding. Injected so tests never
/// reach a network.
pub type Probe<'a> = dyn Fn(&TrackerBinding, &Credential) -> Result<Box<dyn Transport>, String> + 'a;

/// The production transport for a binding's provider.
pub fn connect_transport(binding: &TrackerBinding, credential: &Credential) -> Result<Box<dyn Transport>, String> {
    match binding.provider.as_str() {
        "linear" => Ok(Box::new(HttpTransport::new(credential.token().to_string()))),
        other => Err(format!("unsupported tracker provider '{other}'")),
    }
}

/// Three lines per bound project: credential, auth, team; `provider` takes
/// the place of auth when the provider has no adapter. The bool is true when
/// every check passed.
pub fn run(config: &WardwellConfig, config_dir: &Path, probe: &Probe<'_>) -> (Vec<String>, bool) {
    run_with(config, config_dir, probe, &native_prefixes(config, &config_dir.join("kanban.db")))
}

/// `run`, with each binding's native kanban prefix given by binding key.
/// Adds a fourth line per binding: whether the team key differs from it.
pub fn run_with(config: &WardwellConfig, config_dir: &Path, probe: &Probe<'_>, native: &BTreeMap<String, String>) -> (Vec<String>, bool) {
    if config.trackers.is_empty() {
        return (vec!["No trackers bound. Add a trackers section to config.yml.".to_string()], true);
    }
    let mut lines = Vec::new();
    let mut healthy = true;
    for binding in &config.trackers {
        let key = binding.key();
        let mut checks = check_binding(config_dir, binding, probe);
        checks.push(("kanban prefix".to_string(), prefix_outcome(binding, native.get(&key))));
        healthy &= checks.iter().all(|(_, outcome)| matches!(outcome, Outcome::Ok));
        lines.extend(checks.into_iter().map(|(name, outcome)| format!("{key}: {name} {}", outcome.describe())));
    }
    (lines, healthy)
}

/// The checks that need no network: the credential file exists with
/// owner-only permissions, and Wardwell has an adapter for the provider.
/// The error is a closed code and, for the credential, the path and fix.
pub fn check_offline(config_dir: &Path, binding: &TrackerBinding) -> Result<(), (FailureCode, Option<String>)> {
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
    Failed(FailureCode, Option<String>),
    Skipped,
}

impl Outcome {
    fn describe(&self) -> String {
        match self {
            Outcome::Ok => "ok".to_string(),
            Outcome::Failed(code, Some(detail)) => format!("failed ({}): {detail}", code.as_str()),
            Outcome::Failed(code, None) => format!("failed ({})", code.as_str()),
            Outcome::Skipped => "skipped".to_string(),
        }
    }

    fn from(result: Result<(), FailureCode>) -> Self {
        result.map_or_else(|code| Outcome::Failed(code, None), |_| Outcome::Ok)
    }
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
        run(config, dir, &probe)
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
        let (lines, healthy) = run(&config, dir.path(), &connect_transport);
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
        let (lines, healthy) = run_with(&config, dir.path(), &probe, &native);
        assert_eq!(
            lines.last().unwrap(),
            "work/claims: kanban prefix failed (prefix_collision): Tracker team key COR of work/claims equals the native kanban prefix COR of project claims. Set a different native prefix for claims in kanban.prefixes."
        );
        assert!(!healthy);
        let other = std::collections::BTreeMap::from([("work/claims".to_string(), "CL".to_string())]);
        let (lines, healthy) = run_with(&config, dir.path(), &probe, &other);
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
