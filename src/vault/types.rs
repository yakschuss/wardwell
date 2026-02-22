use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The type of a vault file.
/// Unknown types (e.g. from Obsidian) deserialize to Reference.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VaultType {
    Project,
    Decision,
    Insight,
    Thread,
    Domain,
    #[default]
    Reference,
}

impl<'de> Deserialize<'de> for VaultType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "project" => VaultType::Project,
            "decision" => VaultType::Decision,
            "insight" => VaultType::Insight,
            "thread" => VaultType::Thread,
            "domain" => VaultType::Domain,
            "reference" => VaultType::Reference,
            _ => VaultType::Reference, // unknown types → Reference
        })
    }
}

impl std::fmt::Display for VaultType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Project => write!(f, "project"),
            Self::Decision => write!(f, "decision"),
            Self::Insight => write!(f, "insight"),
            Self::Thread => write!(f, "thread"),
            Self::Domain => write!(f, "domain"),
            Self::Reference => write!(f, "reference"),
        }
    }
}

/// Status of a vault entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Active,
    Completed,
    Blocked,
    Resolved,
    Abandoned,
    Superseded,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => write!(f, "active"),
            Self::Completed => write!(f, "completed"),
            Self::Blocked => write!(f, "blocked"),
            Self::Resolved => write!(f, "resolved"),
            Self::Abandoned => write!(f, "abandoned"),
            Self::Superseded => write!(f, "superseded"),
        }
    }
}

/// Confidence level of a vault entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Inferred,
    Proposed,
    Confirmed,
}

impl std::fmt::Display for Confidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Inferred => write!(f, "inferred"),
            Self::Proposed => write!(f, "proposed"),
            Self::Confirmed => write!(f, "confirmed"),
        }
    }
}

/// Parsed frontmatter from a vault file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frontmatter {
    #[serde(rename = "type", default)]
    pub file_type: VaultType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_lenient_status")]
    pub status: Option<Status>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_lenient_confidence")]
    pub confidence: Option<Confidence>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_lenient_date")]
    pub updated: Option<NaiveDate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default)]
    pub related: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Cross-domain read permissions (only meaningful for domain files).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub can_read: Vec<String>,
}

/// Lenient date deserializer: accepts "2026-02-15", "2026-02-15 11:00",
/// "2026-02-15T10:30:00", or any string starting with YYYY-MM-DD.
fn deserialize_lenient_date<'de, D>(deserializer: D) -> Result<Option<NaiveDate>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(deserializer)?;
    match opt {
        None => Ok(None),
        Some(s) => {
            // Try exact date first
            if let Ok(d) = NaiveDate::parse_from_str(&s, "%Y-%m-%d") {
                return Ok(Some(d));
            }
            // Extract just the date portion (first 10 chars: YYYY-MM-DD)
            if s.len() >= 10
                && let Ok(d) = NaiveDate::parse_from_str(&s[..10], "%Y-%m-%d")
            {
                return Ok(Some(d));
            }
            // Can't parse — silently skip rather than error
            Ok(None)
        }
    }
}

/// Lenient status deserializer: unknown values become None instead of erroring.
fn deserialize_lenient_status<'de, D>(deserializer: D) -> Result<Option<Status>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(deserializer)?;
    Ok(opt.and_then(|s| match s.as_str() {
        "active" => Some(Status::Active),
        "resolved" => Some(Status::Resolved),
        "abandoned" => Some(Status::Abandoned),
        "superseded" => Some(Status::Superseded),
        _ => None,
    }))
}

/// Lenient confidence deserializer: unknown values become None instead of erroring.
fn deserialize_lenient_confidence<'de, D>(deserializer: D) -> Result<Option<Confidence>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(deserializer)?;
    Ok(opt.and_then(|s| match s.as_str() {
        "inferred" => Some(Confidence::Inferred),
        "proposed" => Some(Confidence::Proposed),
        "confirmed" => Some(Confidence::Confirmed),
        _ => None,
    }))
}

/// A fully parsed vault file: path, frontmatter, and body.
#[derive(Debug, Clone)]
pub struct VaultFile {
    pub path: PathBuf,
    pub frontmatter: Frontmatter,
    pub body: String,
}

/// Errors from vault operations.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("no frontmatter found — file must start with '---'")]
    NoFrontmatter,

    #[error("malformed frontmatter: missing closing '---'")]
    UnclosedFrontmatter,

    #[error("frontmatter parse error: {0}")]
    Parse(#[from] serde_yaml::Error),

    #[error("IO error reading '{path}': {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
}
