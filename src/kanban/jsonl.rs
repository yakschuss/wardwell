//! Resilient JSONL appends.
//!
//! iCloud's file provider can briefly reject a write with EPERM while it is
//! mid-sync on a file (or while the file is dataless/evicted). That window is
//! transient — a read materializes the file and a moment later the write
//! succeeds. Rather than surface it as a failed ticket/groom/proposal write,
//! the kanban writers append through here, which materializes + retries on
//! EPERM with short backoff. Non-transient errors return immediately.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

const MAX_ATTEMPTS: u32 = 6;
const INITIAL_BACKOFF: Duration = Duration::from_millis(40);
const MAX_BACKOFF: Duration = Duration::from_millis(800);

/// Append a single JSONL `line` (no trailing newline) to `path`, writing
/// `schema_header` first if the file is new or empty. Creates parent dirs.
/// Retries on transient EPERM; returns other errors immediately.
pub fn append_line(path: &Path, schema_header: Option<&str>, line: &str) -> io::Result<()> {
    let mut backoff = INITIAL_BACKOFF;
    let mut attempt = 0u32;
    loop {
        match try_append(path, schema_header, line) {
            Ok(()) => return Ok(()),
            Err(e) => {
                attempt += 1;
                if !is_transient(&e) || attempt >= MAX_ATTEMPTS {
                    return Err(e);
                }
                // A read forces an evicted/dataless iCloud file to download and
                // nudges the provider out of the state that produced EPERM.
                materialize(path);
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

fn try_append(path: &Path, schema_header: Option<&str>, line: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let needs_schema = match schema_header {
        Some(_) => !path.exists() || path.metadata()?.len() == 0,
        None => false,
    };
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    if needs_schema {
        if let Some(header) = schema_header {
            writeln!(file, "{header}")?;
        }
    }
    writeln!(file, "{line}")?;
    Ok(())
}

/// EPERM is errno 1 on both macOS and Linux. That's the transient iCloud
/// file-provider rejection we want to ride out; everything else is real.
fn is_transient(e: &io::Error) -> bool {
    e.raw_os_error() == Some(1)
}

/// Best-effort: reading an evicted/dataless iCloud file triggers its download.
fn materialize(path: &Path) {
    if let Ok(mut f) = File::open(path) {
        let mut buf = [0u8; 1];
        let _ = f.read(&mut buf);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn writes_schema_header_once_then_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/log.jsonl");
        let header = r#"{"_schema":"x","_version":"1.0"}"#;
        append_line(&path, Some(header), r#"{"i":1}"#).unwrap();
        append_line(&path, Some(header), r#"{"i":2}"#).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3, "header once + two records");
        assert!(lines[0].contains("_schema"));
        assert_eq!(lines[1], r#"{"i":1}"#);
        assert_eq!(lines[2], r#"{"i":2}"#);
        assert_eq!(content.matches("_schema").count(), 1, "header not duplicated");
    }

    #[test]
    fn no_header_when_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        append_line(&path, None, r#"{"i":1}"#).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "{\"i\":1}\n");
    }

    #[test]
    fn transient_predicate_matches_only_eperm() {
        // EPERM (1) is retried; EACCES (13) and others are returned immediately.
        assert!(is_transient(&io::Error::from_raw_os_error(1)));
        assert!(!is_transient(&io::Error::from_raw_os_error(13)));
        assert!(!is_transient(&io::Error::other("not an os error")));
    }
}
