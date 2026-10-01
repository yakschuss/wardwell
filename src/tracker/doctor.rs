//! `wardwell tracker doctor`: per binding, checks the credential file, one
//! cheap authenticated request, and that the bound team resolves. One line
//! per check, a closed code on failure, never a token or provider text.
//!
//! Does NOT pull or write anything.

use crate::config::loader::{TrackerBinding, WardwellConfig};
use crate::tracker::credential::{self, Credential};
use crate::tracker::events::FailureCode;
use crate::tracker::linear::{self, HttpTransport, Transport};
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

/// Three lines per bound project: credential, auth, team. The bool is true
/// when every check passed.
pub fn run(config: &WardwellConfig, config_dir: &Path, probe: &Probe<'_>) -> (Vec<String>, bool) {
    if config.trackers.is_empty() {
        return (vec!["No trackers bound. Add a trackers section to config.yml.".to_string()], true);
    }
    let mut lines = Vec::new();
    let mut healthy = true;
    for (key, binding) in &config.trackers {
        let checks = check_binding(config_dir, binding, probe);
        healthy &= checks.iter().all(|(_, outcome)| matches!(outcome, Outcome::Ok));
        lines.extend(checks.into_iter().map(|(name, outcome)| format!("{key}: {name} {}", outcome.describe())));
    }
    (lines, healthy)
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
    let names = ["credential".to_string(), "auth".to_string(), format!("team {}", binding.team)];
    // Credential errors name the path and the fix, never the file contents.
    let credential = credential::path_in(config_dir, &binding.credential).and_then(|path| credential::load(&path));
    let transport = credential
        .as_ref()
        .map_err(|message| Outcome::Failed(FailureCode::Credential, Some(message.clone())))
        .and_then(|c| probe(binding, c).map_err(|_| Outcome::Failed(FailureCode::UnsupportedProvider, None)));
    let outcomes = match transport {
        Err(failure) => [failure, Outcome::Skipped, Outcome::Skipped],
        Ok(transport) => {
            let auth = Outcome::from(linear::check_auth(transport.as_ref()));
            let team = match auth {
                Outcome::Ok => Outcome::from(linear::check_team(transport.as_ref(), &binding.team)),
                _ => Outcome::Skipped,
            };
            [Outcome::Ok, auth, team]
        }
    };
    names.into_iter().zip(outcomes).collect()
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
        assert_eq!(lines, vec!["work/claims: credential ok", "work/claims: auth ok", "work/claims: team COR ok"]);
        assert!(healthy);
    }

    #[test]
    fn missing_credential_fails_and_skips_the_remote_checks() {
        let (dir, config) = setup(false);
        let (lines, healthy) = doctor_with(dir.path(), &config, viewer_ok(), json!([{"key": "COR"}]));
        assert!(lines[0].starts_with("work/claims: credential failed (credential): tracker credential not configured"), "{}", lines[0]);
        assert_eq!(&lines[1..], ["work/claims: auth skipped", "work/claims: team COR skipped"]);
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
            Ok(json!({"errors": [{"message": "Authentication required for lin_api_secret"}]})),
        ] {
            let (lines, healthy) = doctor_with(dir.path(), &config, viewer, json!([{"key": "COR"}]));
            assert_eq!(&lines[1..], ["work/claims: auth failed (auth)", "work/claims: team COR skipped"]);
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
