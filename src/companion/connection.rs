use serde::Deserialize;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_TOKEN_BYTES: usize = 8 * 1024;
const API_ENDPOINT: &str = "https://api.wardwell.app/mcp";

#[derive(Deserialize)]
struct ConnectionFile {
    endpoint: String,
    token: String,
}

pub struct Connection {
    pub endpoint: String,
    pub token: String,
}

pub fn default_path() -> Result<PathBuf, String> {
    Ok(crate::config::loader::config_dir()
        .join("hank")
        .join("connection.json"))
}

pub fn load(path: &Path) -> Result<Connection, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "Hank connection not configured; local Wardwell tools remain available".to_string()
        } else {
            "Could not read the Hank connection file".to_string()
        }
    })?;
    validate_file_metadata(&metadata)?;

    if metadata.len() > MAX_FILE_BYTES {
        return Err("Hank connection file is too large".to_string());
    }

    let file =
        File::open(path).map_err(|_| "Could not read the Hank connection file".to_string())?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read the Hank connection file".to_string())?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("Hank connection file is too large".to_string());
    }

    let parsed: ConnectionFile = serde_json::from_slice(&bytes)
        .map_err(|_| "Hank connection file is malformed".to_string())?;
    validate_endpoint(&parsed.endpoint)?;
    validate_token(&parsed.token)?;

    Ok(Connection {
        endpoint: parsed.endpoint,
        token: parsed.token,
    })
}

pub fn save(path: &Path, token: &str) -> Result<(), String> {
    validate_endpoint(API_ENDPOINT)?;
    validate_token(token)?;

    let parent = path
        .parent()
        .ok_or_else(|| "Hank connection path has no parent directory".to_string())?;
    let parent_existed = fs::symlink_metadata(parent).is_ok();
    fs::create_dir_all(parent)
        .map_err(|_| "Could not create the Hank connection directory".to_string())?;
    if !parent_existed {
        set_directory_mode(parent, 0o700)?;
    }
    let parent_metadata = fs::symlink_metadata(parent)
        .map_err(|_| "Could not inspect the Hank connection directory".to_string())?;
    validate_directory_metadata(&parent_metadata)?;

    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            return Err("Hank connection path must not be a symlink".to_string());
        }
        if !metadata.file_type().is_file() {
            return Err("Hank connection path must be a regular file".to_string());
        }
    }

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temp_path = parent.join(format!(".connection.json.{}.{}", std::process::id(), nonce));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options
            .open(&temp_path)
            .map_err(|_| "Could not create the Hank connection file".to_string())?;
        set_file_mode(&file, 0o600)?;
        let contents = serde_json::to_vec(&serde_json::json!({
            "endpoint": API_ENDPOINT,
            "token": token,
        }))
        .map_err(|_| "Could not encode the Hank connection file".to_string())?;
        file.write_all(&contents)
            .and_then(|_| file.sync_all())
            .map_err(|_| "Could not write the Hank connection file".to_string())?;
        drop(file);
        fs::rename(&temp_path, path)
            .map_err(|_| "Could not install the Hank connection file".to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn validate_endpoint(endpoint: &str) -> Result<(), String> {
    if endpoint != API_ENDPOINT {
        return Err("Hank connection endpoint must be https://api.wardwell.app/mcp".to_string());
    }
    Ok(())
}

fn validate_token(token: &str) -> Result<(), String> {
    if token.is_empty() {
        return Err("Hank connection token is empty".to_string());
    }
    if token.len() > MAX_TOKEN_BYTES {
        return Err("Hank connection token is too large".to_string());
    }
    if token.chars().any(char::is_control) {
        return Err("Hank connection token contains control characters".to_string());
    }
    Ok(())
}

fn validate_file_metadata(metadata: &fs::Metadata) -> Result<(), String> {
    if metadata.file_type().is_symlink() {
        return Err("Hank connection file must not be a symlink".to_string());
    }
    if !metadata.file_type().is_file() {
        return Err("Hank connection file must be a regular file".to_string());
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err("Hank connection file permissions must be private".to_string());
    }
    Ok(())
}

fn validate_directory_metadata(metadata: &fs::Metadata) -> Result<(), String> {
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err("Hank connection directory must be a private directory".to_string());
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err("Hank connection directory permissions must be private".to_string());
    }
    Ok(())
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(unix)]
fn set_file_mode(file: &File, mode: u32) -> Result<(), String> {
    file.set_permissions(fs::Permissions::from_mode(mode))
        .map_err(|_| "Could not secure the Hank connection file".to_string())
}

#[cfg(unix)]
fn set_directory_mode(path: &Path, mode: u32) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|_| "Could not secure the Hank connection directory".to_string())
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
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_connection(path: &Path, contents: &str) {
        fs::write(path, contents).expect("write test connection");
        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .expect("secure test connection");
    }

    #[test]
    fn loads_valid_connection() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("connection.json");
        write_connection(
            &path,
            r#"{"endpoint":"https://api.wardwell.app/mcp","token":"test-token"}"#,
        );

        let connection = load(&path).expect("valid connection");
        assert_eq!(connection.endpoint, API_ENDPOINT);
        assert_eq!(connection.token, "test-token");
    }

    #[test]
    fn reports_missing_connection_without_external_setup() {
        let dir = tempdir().expect("tempdir");
        let error = load(&dir.path().join("missing.json"))
            .err()
            .expect("missing connection");
        assert!(error.contains("Hank connection not configured"));
        assert!(error.contains("local Wardwell tools remain available"));
    }

    #[test]
    fn rejects_malformed_connection_without_echoing_contents() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("connection.json");
        let secret = "super-secret-malformed-token";
        write_connection(&path, &format!("{{\"token\":\"{secret}"));

        let error = load(&path).err().expect("malformed connection");
        assert!(error.contains("malformed"));
        assert!(!error.contains(secret));
    }

    #[test]
    fn rejects_oversized_connection() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("connection.json");
        write_connection(&path, &"x".repeat(MAX_FILE_BYTES as usize + 1));

        let error = load(&path).err().expect("oversized connection");
        assert!(error.contains("too large"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_group_or_other_readable_connection() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("connection.json");
        write_connection(
            &path,
            r#"{"endpoint":"https://api.wardwell.app/mcp","token":"test-token"}"#,
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("loosen permissions");

        let error = load(&path).err().expect("insecure connection");
        assert!(error.contains("permissions must be private"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_connection() {
        let dir = tempdir().expect("tempdir");
        let parent = dir.path().join("hank");
        fs::create_dir(&parent).expect("parent directory");
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))
            .expect("secure parent directory");
        let target = parent.join("target.json");
        let link = parent.join("connection.json");
        write_connection(
            &target,
            r#"{"endpoint":"https://api.wardwell.app/mcp","token":"test-token"}"#,
        );
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        let error = load(&link).err().expect("symlink connection");
        assert!(error.contains("must not be a symlink"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn saves_atomically_with_private_modes_and_round_trips() {
        let dir = tempdir().expect("tempdir");
        let parent = dir.path().join("hank");
        let path = parent.join("connection.json");
        save(&path, "test-token").expect("save connection");

        assert_eq!(
            fs::metadata(&parent)
                .expect("parent metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path)
                .expect("file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let connection = load(&path).expect("saved connection");
        assert_eq!(connection.token, "test-token");
    }

    #[cfg(unix)]
    #[test]
    fn save_rejects_symlink_target_without_replacing_it() {
        let dir = tempdir().expect("tempdir");
        let parent = dir.path().join("hank");
        fs::create_dir(&parent).expect("parent directory");
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))
            .expect("secure parent directory");
        let target = parent.join("target.json");
        let link = parent.join("connection.json");
        write_connection(&target, "untouched");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        let error = save(&link, "test-token").err().expect("symlink target");
        assert!(error.contains("must not be a symlink"), "{error}");
        assert_eq!(
            fs::read_to_string(&target).expect("target contents"),
            "untouched"
        );
    }
}
