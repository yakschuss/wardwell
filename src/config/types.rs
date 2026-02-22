use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

/// Validated domain name. Cannot be empty, cannot contain path separators.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct DomainName(String);

impl DomainName {
    pub fn new(name: &str) -> Result<Self, ConfigError> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(ConfigError::InvalidDomainName {
                name: name.to_string(),
                reason: "domain name cannot be empty".to_string(),
            });
        }
        if trimmed.contains('/') || trimmed.contains('\\') {
            return Err(ConfigError::InvalidDomainName {
                name: name.to_string(),
                reason: "domain name cannot contain path separators".to_string(),
            });
        }
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for DomainName {
    type Error = ConfigError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(&s)
    }
}

impl fmt::Display for DomainName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Validated path glob pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathGlob(String);

impl PathGlob {
    pub fn new(pattern: &str) -> Result<Self, ConfigError> {
        let trimmed = pattern.trim();
        if trimmed.is_empty() {
            return Err(ConfigError::InvalidPathGlob {
                pattern: pattern.to_string(),
                reason: "path glob cannot be empty".to_string(),
            });
        }
        if glob::Pattern::new(trimmed).is_err() {
            return Err(ConfigError::InvalidPathGlob {
                pattern: pattern.to_string(),
                reason: "invalid glob syntax".to_string(),
            });
        }
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Expand shell home directory prefix and return as absolute PathBuf.
    pub fn expand(&self) -> PathBuf {
        let s = &self.0;
        if let Some(rest) = s.strip_prefix("~/")
            && let Some(home) = dirs_home()
        {
            return home.join(rest);
        }
        PathBuf::from(s)
    }

    /// Check if a canonicalized path matches this glob.
    pub fn matches(&self, path: &std::path::Path) -> bool {
        let expanded = self.expand();
        let pattern_str = expanded.to_string_lossy();

        if let Ok(pattern) = glob::Pattern::new(&pattern_str)
            && pattern.matches_path(path)
        {
            return true;
        }

        // Extract base directory from glob (everything before first *)
        let base = pattern_str.split('*').next().unwrap_or(&pattern_str);
        let base_path = std::path::Path::new(base.trim_end_matches('/'));

        if path.starts_with(base_path) {
            return true;
        }

        // Canonicalize the base path (handles /tmp → /private/tmp on macOS, symlinks, etc.)
        if let Ok(canonical_base) = std::fs::canonicalize(base_path)
            && path.starts_with(&canonical_base)
        {
            return true;
        }

        false
    }
}

impl fmt::Display for PathGlob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Unique session identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(String);

impl SessionId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn from_string(s: String) -> Self {
        Self(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// All config errors.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid domain name '{name}': {reason}")]
    InvalidDomainName { name: String, reason: String },

    #[error("invalid path glob '{pattern}': {reason}")]
    InvalidPathGlob { pattern: String, reason: String },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("YAML parse error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("config file not found: {path}")]
    NotFound { path: String },

    #[error("empty domain configuration")]
    EmptyConfig,
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn domain_name_rejects_empty() {
        let r1 = DomainName::new("");
        assert!(r1.is_err(), "{r1:?}");
        let r2 = DomainName::new("  ");
        assert!(r2.is_err(), "{r2:?}");
    }

    #[test]
    fn domain_name_rejects_path_separators() {
        let r1 = DomainName::new("foo/bar");
        assert!(r1.is_err(), "{r1:?}");
        let r2 = DomainName::new("foo\\bar");
        assert!(r2.is_err(), "{r2:?}");
    }

    #[test]
    fn domain_name_accepts_valid() {
        let name = DomainName::new("personal").ok();
        assert!(name.is_some());
        assert_eq!(name.as_ref().map(|n| n.as_str()), Some("personal"));
    }

    #[test]
    fn path_glob_rejects_empty() {
        let result = PathGlob::new("");
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn path_glob_accepts_valid() {
        let r1 = PathGlob::new("~/projects/*");
        assert!(r1.is_ok(), "{r1:?}");
        let r2 = PathGlob::new("/tmp/test");
        assert!(r2.is_ok(), "{r2:?}");
    }

    #[test]
    fn session_id_unique() {
        let a = SessionId::new();
        let b = SessionId::new();
        assert_ne!(a, b);
    }
}
