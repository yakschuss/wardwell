//! Local refresh state: one small JSON file per project under the config
//! dir, `refresh/<domain>__<project>.json`, never in the vault. Per provider
//! it holds the last start time and process id, the last completion time,
//! and the last failure time and code, all in wall-clock time. A pull
//! writes it at start, completion, failure and timeout; the refresh
//! trigger decides from it alone, so it never reads the vault.
//!
//! Does NOT write the tracker log; markers there stay the trace.

use crate::tracker::events::FailureCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The folder under the config dir that holds the state and claim files.
pub const DIR: &str = "refresh";

/// One provider's last start, completion and failure.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<FailureCode>,
}

impl ProviderState {
    /// The start time and pid of a pull that neither completed nor failed after it started.
    pub fn open_start(&self) -> Option<(DateTime<Utc>, Option<u32>)> {
        let started = self.started_at?;
        let ended = [self.completed_at, self.failed_at].into_iter().flatten().any(|at| at >= started);
        (!ended).then_some((started, self.pid))
    }

    /// The failure time and code when the failure is newer than the last completion.
    pub fn open_failure(&self) -> Option<(DateTime<Utc>, FailureCode)> {
        let failed = self.failed_at?;
        let code = self.code?;
        match self.completed_at {
            Some(completed) if completed >= failed => None,
            _ => Some((failed, code)),
        }
    }
}

/// The state file of one project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshState {
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderState>,
}

/// What reading a state file found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    Missing,
    Unreadable,
    Found(RefreshState),
}

/// What a pull records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Record {
    Started(u32),
    Completed,
    Failed(FailureCode),
}

/// `<config dir>/refresh/<domain>__<project>.json`.
pub fn path(config_dir: &Path, domain: &str, project: &str) -> PathBuf {
    config_dir.join(DIR).join(format!("{domain}__{project}.json"))
}

/// The claim file beside the state file.
pub fn claim_path(config_dir: &Path, domain: &str, project: &str) -> PathBuf {
    config_dir.join(DIR).join(format!("{domain}__{project}.claim"))
}

/// A claim younger than this stops another start; an older one is replaced.
pub const CLAIM_FOR: chrono::TimeDelta = chrono::TimeDelta::minutes(20);

/// Take the claim at `path`: create it new, holding this process's id and
/// `now`. An existing claim younger than `CLAIM_FOR` by its modified time,
/// against the wall clock, wins and this returns false. An older one, or one stamped in the future,
/// is moved aside by a rename, which only one taker can do, and the claim
/// is created again.
pub fn claim(path: &Path, now: DateTime<Utc>) -> bool {
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return false;
    }
    if create_claim(path, now) {
        return true;
    }
    let fresh = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|modified| Utc::now() - DateTime::<Utc>::from(modified))
        .is_ok_and(|age| age >= chrono::TimeDelta::zero() && age < CLAIM_FOR);
    if fresh {
        return false;
    }
    let aside = path.with_extension(format!("claim.stale.{}", uuid::Uuid::new_v4()));
    if std::fs::rename(path, &aside).is_err() {
        return false;
    }
    let _ = std::fs::remove_file(&aside);
    create_claim(path, now)
}

fn create_claim(path: &Path, now: DateTime<Utc>) -> bool {
    use std::io::Write;
    match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            let _ = writeln!(file, "{} {}", std::process::id(), now.to_rfc3339());
            true
        }
        Err(_) => false,
    }
}

/// Remove the claim at `path`; a missing one is fine.
pub fn release(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// The state at `path`.
pub fn read(path: &Path) -> Read {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_or(Read::Unreadable, Read::Found),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Read::Missing,
        Err(_) => Read::Unreadable,
    }
}

/// One provider's state from the file at `path`, when the file reads.
pub fn provider(path: &Path, provider: &str) -> Option<ProviderState> {
    match read(path) {
        Read::Found(state) => state.providers.get(provider).cloned(),
        Read::Missing | Read::Unreadable => None,
    }
}

/// Record `record` for `provider` at `at`, through a temp file and a
/// rename. A missing or unreadable file starts empty.
pub fn record(path: &Path, provider: &str, record: Record, at: DateTime<Utc>) -> Result<(), String> {
    let mut state = match read(path) {
        Read::Found(state) => state,
        Read::Missing | Read::Unreadable => RefreshState::default(),
    };
    let entry = state.providers.entry(provider.to_string()).or_default();
    match record {
        Record::Started(pid) => {
            entry.started_at = Some(at);
            entry.pid = Some(pid);
        }
        Record::Completed => entry.completed_at = Some(at),
        Record::Failed(code) => {
            entry.failed_at = Some(at);
            entry.code = Some(code);
        }
    }
    write(path, &state)
}

fn write(path: &Path, state: &RefreshState) -> Result<(), String> {
    let failed = || format!("could not write {}", path.display());
    let parent = path.parent().ok_or_else(failed)?;
    std::fs::create_dir_all(parent).map_err(|_| failed())?;
    let bytes = serde_json::to_vec(state).map_err(|_| failed())?;
    let temp = parent.join(format!(".{}.{}.{}", path.file_name().and_then(|n| n.to_str()).unwrap_or("state"), std::process::id(), uuid::Uuid::new_v4()));
    let written = std::fs::write(&temp, bytes).and_then(|_| std::fs::rename(&temp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written.map_err(|_| failed())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 1, 9, minute, 0).unwrap()
    }

    #[test]
    fn records_round_trip_per_provider_and_leave_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = path(dir.path(), "work", "claims");
        assert_eq!(file, dir.path().join("refresh/work__claims.json"));
        assert_eq!(read(&file), Read::Missing);
        record(&file, "linear", Record::Started(42), at(1)).unwrap();
        record(&file, "github", Record::Failed(FailureCode::Provider), at(2)).unwrap();
        record(&file, "linear", Record::Completed, at(3)).unwrap();
        let Read::Found(state) = read(&file) else { panic!() };
        assert_eq!(state.providers["linear"], ProviderState { started_at: Some(at(1)), pid: Some(42), completed_at: Some(at(3)), ..Default::default() });
        assert_eq!(state.providers["github"].open_failure(), Some((at(2), FailureCode::Provider)));
        assert_eq!(std::fs::read_dir(dir.path().join(DIR)).unwrap().count(), 1);
    }

    #[test]
    fn an_unreadable_file_reads_as_such_and_a_record_replaces_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = path(dir.path(), "work", "claims");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "{not json").unwrap();
        assert_eq!(read(&file), Read::Unreadable);
        record(&file, "linear", Record::Completed, at(1)).unwrap();
        assert!(matches!(read(&file), Read::Found(_)));
    }

    fn age_claim(file: &Path, by: chrono::TimeDelta) {
        let modified = std::time::SystemTime::from(Utc::now() - by);
        std::fs::OpenOptions::new().write(true).open(file).unwrap().set_modified(modified).unwrap();
    }

    #[test]
    fn a_young_claim_wins_and_an_old_one_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let file = claim_path(dir.path(), "work", "claims");
        let now = Utc::now();
        assert!(claim(&file, now));
        assert!(!claim(&file, now), "held");
        age_claim(&file, CLAIM_FOR - chrono::TimeDelta::minutes(1));
        assert!(!claim(&file, now), "19 minutes old still holds");
        age_claim(&file, CLAIM_FOR + chrono::TimeDelta::minutes(1));
        assert!(claim(&file, now), "an old claim is replaced");
        age_claim(&file, -chrono::TimeDelta::hours(1));
        assert!(claim(&file, now), "a claim stamped in the future counts as old");
        release(&file);
        assert!(!file.exists());
        assert!(claim(&file, now));
    }

    #[test]
    fn a_start_is_open_until_a_later_completion_or_failure() {
        let mut state = ProviderState { started_at: Some(at(5)), pid: Some(7), completed_at: Some(at(1)), ..Default::default() };
        assert_eq!(state.open_start(), Some((at(5), Some(7))));
        state.failed_at = Some(at(6));
        state.code = Some(FailureCode::Timeout);
        assert_eq!(state.open_start(), None);
        assert_eq!(state.open_failure(), Some((at(6), FailureCode::Timeout)));
        state.completed_at = Some(at(7));
        assert_eq!(state.open_failure(), None);
    }
}
