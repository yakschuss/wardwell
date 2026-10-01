//! The install record at `<config dir>/install-manifest.json`: the client
//! entries Wardwell added that ownership cannot prove by shape. Today that is
//! the deny list entries. A deny entry the user already had is never recorded,
//! so uninstall removes only what Wardwell added.
//! Does NOT record hooks; a hook is matched by its binary name and arguments.

use crate::companion::install::read_optional;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const FILE: &str = "install-manifest.json";
pub const VERSION: u32 = 1;

/// `{"version": 1, "claude_permissions_deny": ["mcp__linear__delete_comment", ...]}`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    /// Entries Wardwell appended to `permissions.deny` in `~/.claude/settings.json`.
    #[serde(default)]
    pub claude_permissions_deny: Vec<String>,
}

impl Default for Manifest {
    fn default() -> Self {
        Manifest { version: VERSION, claude_permissions_deny: Vec::new() }
    }
}

pub fn path(config_dir: &Path) -> PathBuf {
    config_dir.join(FILE)
}

/// The raw bytes and the parsed record, or None when there is no file.
/// A malformed or newer record is an error, so preflight stops.
pub fn read(config_dir: &Path) -> Result<Option<(Vec<u8>, Manifest)>, String> {
    let file = path(config_dir);
    let Some(bytes) = read_optional(&file)? else { return Ok(None) };
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .map_err(|_| format!("Malformed install record {}; no files changed", file.display()))?;
    if manifest.version != VERSION {
        return Err(format!("Install record {} has version {}; this binary reads {VERSION}", file.display(), manifest.version));
    }
    Ok(Some((bytes, manifest)))
}

pub fn encode(manifest: &Manifest) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec_pretty(manifest).map_err(|_| "Could not encode the install record")?;
    bytes.push(b'\n');
    Ok(bytes)
}
