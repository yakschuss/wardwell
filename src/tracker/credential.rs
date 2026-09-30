//! Tracker API credentials at `~/.wardwell/trackers/<name>.json`.
//!
//! Same posture as the Hank connection file: private 0600 file in a 0700
//! directory, no symlinks, bounded size, atomic replace, and no error message
//! ever echoes file contents. Does NOT decide which provider uses a token.

use serde::Deserialize;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_FILE_BYTES: u64 = 64 * 1024;
pub const MAX_TOKEN_BYTES: usize = 8 * 1024;
const DIRECTORY: &str = "trackers";

#[derive(Deserialize)]
struct CredentialFile {
    token: String,
}

/// A loaded tracker token. Debug output never includes it.
pub struct Credential {
    token: String,
}

impl Credential {
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credential(<redacted>)")
    }
}

/// `<config_dir>/trackers/<name>.json`, with `name` restricted to
/// letters, digits, `-` and `_` so it cannot leave the directory.
pub fn path_in(config_dir: &Path, name: &str) -> Result<PathBuf, String> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['-', '_'])
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid {
        return Err("tracker credential name must use letters, digits, '-' or '_'".to_string());
    }
    Ok(config_dir.join(DIRECTORY).join(format!("{name}.json")))
}

/// The credential path under the Wardwell config directory.
pub fn default_path(name: &str) -> Result<PathBuf, String> {
    path_in(&crate::config::loader::config_dir(), name)
}

pub fn load(path: &Path) -> Result<Credential, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("<name>");
            format!(
                "tracker credential not configured at {}; run `wardwell tracker connect {name} --token-stdin`",
                path.display()
            )
        } else {
            "Could not read the tracker credential file".to_string()
        }
    })?;
    validate_file_metadata(&metadata)?;
    if metadata.len() > MAX_FILE_BYTES {
        return Err("Tracker credential file is too large".to_string());
    }
    let file = File::open(path).map_err(|_| "Could not read the tracker credential file".to_string())?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read the tracker credential file".to_string())?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("Tracker credential file is too large".to_string());
    }
    let parsed: CredentialFile = serde_json::from_slice(&bytes)
        .map_err(|_| "Tracker credential file is malformed".to_string())?;
    validate_token(&parsed.token)?;
    Ok(Credential { token: parsed.token })
}

pub fn save(path: &Path, token: &str) -> Result<(), String> {
    validate_token(token)?;
    let parent = path
        .parent()
        .ok_or_else(|| "Tracker credential path has no parent directory".to_string())?;
    let parent_existed = fs::symlink_metadata(parent).is_ok();
    fs::create_dir_all(parent)
        .map_err(|_| "Could not create the tracker credential directory".to_string())?;
    if !parent_existed {
        set_directory_mode(parent, 0o700)?;
    }
    let parent_metadata = fs::symlink_metadata(parent)
        .map_err(|_| "Could not inspect the tracker credential directory".to_string())?;
    validate_directory_metadata(&parent_metadata)?;

    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            return Err("Tracker credential path must not be a symlink".to_string());
        }
        if !metadata.file_type().is_file() {
            return Err("Tracker credential path must be a regular file".to_string());
        }
    }

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temp_path = parent.join(format!(".credential.{}.{}", std::process::id(), nonce));
    let result = write_then_rename(&temp_path, path, token);
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn write_then_rename(temp_path: &Path, path: &Path, token: &str) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(temp_path)
        .map_err(|_| "Could not create the tracker credential file".to_string())?;
    set_file_mode(&file, 0o600)?;
    let contents = serde_json::to_vec(&serde_json::json!({ "token": token }))
        .map_err(|_| "Could not encode the tracker credential file".to_string())?;
    file.write_all(&contents)
        .and_then(|_| file.sync_all())
        .map_err(|_| "Could not write the tracker credential file".to_string())?;
    drop(file);
    fs::rename(temp_path, path).map_err(|_| "Could not install the tracker credential file".to_string())
}

fn validate_token(token: &str) -> Result<(), String> {
    if token.is_empty() {
        return Err("Tracker token is empty".to_string());
    }
    if token.len() > MAX_TOKEN_BYTES {
        return Err("Tracker token is too large".to_string());
    }
    if token.chars().any(char::is_control) {
        return Err("Tracker token contains control characters".to_string());
    }
    Ok(())
}

fn validate_file_metadata(metadata: &fs::Metadata) -> Result<(), String> {
    if metadata.file_type().is_symlink() {
        return Err("Tracker credential file must not be a symlink".to_string());
    }
    if !metadata.file_type().is_file() {
        return Err("Tracker credential file must be a regular file".to_string());
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err("Tracker credential file permissions must be private".to_string());
    }
    Ok(())
}

fn validate_directory_metadata(metadata: &fs::Metadata) -> Result<(), String> {
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err("Tracker credential directory must be a private directory".to_string());
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err("Tracker credential directory permissions must be private".to_string());
    }
    Ok(())
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(unix)]
fn set_file_mode(file: &File, mode: u32) -> Result<(), String> {
    file.set_permissions(fs::Permissions::from_mode(mode))
        .map_err(|_| "Could not secure the tracker credential file".to_string())
}

#[cfg(unix)]
fn set_directory_mode(path: &Path, mode: u32) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|_| "Could not secure the tracker credential directory".to_string())
}

#[cfg(not(unix))]
fn set_file_mode(_file: &File, _mode: u32) -> Result<(), String> {
    Ok(())
}

#[cfg(not(unix))]
fn set_directory_mode(_path: &Path, _mode: u32) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_private(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn path_rejects_names_that_escape_the_directory() {
        let dir = tempdir().unwrap();
        for bad in ["", "../x", "a/b", ".hidden", "a b"] {
            assert!(path_in(dir.path(), bad).is_err(), "{bad}");
        }
        assert_eq!(
            path_in(dir.path(), "corr-linear_1").unwrap(),
            dir.path().join("trackers").join("corr-linear_1.json")
        );
    }

    #[test]
    fn missing_credential_names_the_connect_command() {
        let dir = tempdir().unwrap();
        let error = load(&dir.path().join("nope.json")).err().unwrap();
        assert!(error.contains("not configured"), "{error}");
        assert!(error.contains("wardwell tracker connect"), "{error}");
    }

    #[test]
    fn malformed_credential_does_not_echo_contents() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("c.json");
        write_private(&path, "{\"token\":\"lin_api_secret");
        let error = load(&path).err().unwrap();
        assert!(error.contains("malformed"));
        assert!(!error.contains("lin_api_secret"));
    }

    #[test]
    fn rejects_empty_and_control_character_tokens() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("trackers").join("c.json");
        assert!(save(&path, "").is_err());
        assert!(save(&path, "a\nb").is_err());
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn saves_private_file_and_round_trips() {
        let dir = tempdir().unwrap();
        let parent = dir.path().join("trackers");
        let path = parent.join("c.json");
        save(&path, "lin_api_test").unwrap();
        assert_eq!(fs::metadata(&parent).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(load(&path).unwrap().token(), "lin_api_test");
        save(&path, "lin_api_rotated").unwrap();
        assert_eq!(load(&path).unwrap().token(), "lin_api_rotated");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_group_readable_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("c.json");
        write_private(&path, r#"{"token":"t"}"#);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load(&path).err().unwrap().contains("permissions must be private"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_on_load_and_save() {
        let dir = tempdir().unwrap();
        let parent = dir.path().join("trackers");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        let target = parent.join("target.json");
        let link = parent.join("c.json");
        write_private(&target, r#"{"token":"t"}"#);
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(load(&link).err().unwrap().contains("must not be a symlink"));
        assert!(save(&link, "other").err().unwrap().contains("must not be a symlink"));
        assert_eq!(fs::read_to_string(&target).unwrap(), r#"{"token":"t"}"#);
    }

    #[test]
    fn debug_output_redacts_the_token() {
        let credential = Credential { token: "lin_api_secret".into() };
        assert!(!format!("{credential:?}").contains("lin_api_secret"));
    }
}
