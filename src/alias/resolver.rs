use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Errors during alias resolution.
#[derive(Debug, thiserror::Error)]
pub enum AliasError {
    #[error("unknown alias '{name}'")]
    UnknownAlias { name: String },
    #[error("unknown domain '{name}'")]
    UnknownDomain { name: String },
    #[error("resolved path '{path}' is outside domain boundaries")]
    OutsideBoundary { path: String },
    #[error("path expansion failed for '{path}': {reason}")]
    ExpansionFailed { path: String, reason: String },
}

/// Resolves `{alias:name}` and `{domain:name}` references to absolute paths.
/// Domain boundary enforcement ensures aliases can't escape their domain.
pub struct AliasResolver {
    aliases: HashMap<String, PathBuf>,
    domain_roots: HashMap<String, PathBuf>,
    domain_paths: Vec<PathBuf>,
}

impl AliasResolver {
    /// Create a resolver from a domain's alias config and path globs.
    pub fn new(
        aliases: &HashMap<String, String>,
        domain_name: &str,
        domain_path_globs: &[String],
    ) -> Self {
        let mut resolved_aliases = HashMap::new();
        for (name, path) in aliases {
            resolved_aliases.insert(name.clone(), expand_home(path));
        }

        let mut domain_roots = HashMap::new();
        let mut domain_paths = Vec::new();
        for glob in domain_path_globs {
            let base = glob_base(glob);
            let expanded = expand_home(&base);
            domain_paths.push(expanded.clone());
            domain_roots.entry(domain_name.to_string()).or_insert(expanded);
        }

        Self {
            aliases: resolved_aliases,
            domain_roots,
            domain_paths,
        }
    }

    /// Resolve a path string that may contain `{alias:name}` or `{domain:name}` references.
    /// Returns the fully resolved, absolute filesystem path.
    pub fn resolve(&self, path_str: &str) -> Result<PathBuf, AliasError> {
        let mut resolved = path_str.to_string();

        // Replace {alias:name} references
        while let Some(start) = resolved.find("{alias:") {
            let end = resolved[start..].find('}').ok_or_else(|| AliasError::ExpansionFailed {
                path: path_str.to_string(),
                reason: "unclosed {alias:...} reference".to_string(),
            })? + start;
            let name = &resolved[start + 7..end];
            let alias_path = self.aliases.get(name).ok_or_else(|| AliasError::UnknownAlias {
                name: name.to_string(),
            })?;
            resolved = format!("{}{}{}", &resolved[..start], alias_path.display(), &resolved[end + 1..]);
        }

        // Replace {domain:name} references
        while let Some(start) = resolved.find("{domain:") {
            let end = resolved[start..].find('}').ok_or_else(|| AliasError::ExpansionFailed {
                path: path_str.to_string(),
                reason: "unclosed {domain:...} reference".to_string(),
            })? + start;
            let name = &resolved[start + 8..end];
            let domain_root = self.domain_roots.get(name).ok_or_else(|| AliasError::UnknownDomain {
                name: name.to_string(),
            })?;
            resolved = format!("{}{}{}", &resolved[..start], domain_root.display(), &resolved[end + 1..]);
        }

        // Expand ~ to home directory
        let path = expand_home(&resolved);

        // Verify the resolved path falls within domain boundaries
        self.check_boundary(&path)?;

        Ok(path)
    }

    /// Resolve a path without boundary checking (for entry point paths that may
    /// intentionally reference the domain root).
    pub fn resolve_unchecked(&self, path_str: &str) -> Result<PathBuf, AliasError> {
        let mut resolved = path_str.to_string();

        while let Some(start) = resolved.find("{alias:") {
            let end = resolved[start..].find('}').ok_or_else(|| AliasError::ExpansionFailed {
                path: path_str.to_string(),
                reason: "unclosed {alias:...} reference".to_string(),
            })? + start;
            let name = &resolved[start + 7..end];
            let alias_path = self.aliases.get(name).ok_or_else(|| AliasError::UnknownAlias {
                name: name.to_string(),
            })?;
            resolved = format!("{}{}{}", &resolved[..start], alias_path.display(), &resolved[end + 1..]);
        }

        while let Some(start) = resolved.find("{domain:") {
            let end = resolved[start..].find('}').ok_or_else(|| AliasError::ExpansionFailed {
                path: path_str.to_string(),
                reason: "unclosed {domain:...} reference".to_string(),
            })? + start;
            let name = &resolved[start + 8..end];
            let domain_root = self.domain_roots.get(name).ok_or_else(|| AliasError::UnknownDomain {
                name: name.to_string(),
            })?;
            resolved = format!("{}{}{}", &resolved[..start], domain_root.display(), &resolved[end + 1..]);
        }

        Ok(expand_home(&resolved))
    }

    /// Check if a path is within the domain's boundaries.
    fn check_boundary(&self, path: &Path) -> Result<(), AliasError> {
        // If no domain paths configured, skip boundary check
        if self.domain_paths.is_empty() {
            return Ok(());
        }

        for domain_path in &self.domain_paths {
            if path.starts_with(domain_path) {
                return Ok(());
            }
            // Also check canonicalized versions for symlink resolution
            if let Ok(canonical_domain) = std::fs::canonicalize(domain_path)
                && let Ok(canonical_path) = std::fs::canonicalize(path)
                && canonical_path.starts_with(&canonical_domain)
            {
                return Ok(());
            }
        }

        Err(AliasError::OutsideBoundary {
            path: path.display().to_string(),
        })
    }

    /// Get all configured aliases and their resolved paths.
    pub fn aliases(&self) -> &HashMap<String, PathBuf> {
        &self.aliases
    }
}

/// Expand `~` prefix to home directory.
fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

/// Extract the base directory from a glob pattern (everything before the first `*`).
fn glob_base(glob: &str) -> String {
    let expanded = if let Some(rest) = glob.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            format!("{home}/{rest}")
        } else {
            glob.to_string()
        }
    } else {
        glob.to_string()
    };

    let base = expanded.split('*').next().unwrap_or(&expanded);
    base.trim_end_matches('/').to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn test_resolver() -> AliasResolver {
        let mut aliases = HashMap::new();
        aliases.insert("vault".to_string(), "/tmp/test-vault".to_string());
        aliases.insert("agents".to_string(), "/tmp/test-vault/personal".to_string());

        AliasResolver::new(
            &aliases,
            "personal",
            &["/tmp/test-vault/*".to_string()],
        )
    }

    #[test]
    fn resolve_alias_basic() {
        let resolver = test_resolver();
        let result = resolver.resolve("{alias:vault}/INDEX.md");
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            result.ok().as_ref().map(|p| p.display().to_string()),
            Some("/tmp/test-vault/INDEX.md".to_string())
        );
    }

    #[test]
    fn resolve_alias_nested() {
        let resolver = test_resolver();
        let result = resolver.resolve("{alias:agents}/wardwell.md");
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            result.ok().as_ref().map(|p| p.display().to_string()),
            Some("/tmp/test-vault/personal/wardwell.md".to_string())
        );
    }

    #[test]
    fn resolve_domain_reference() {
        let resolver = test_resolver();
        let result = resolver.resolve("{domain:personal}/notes.md");
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            result.ok().as_ref().map(|p| p.display().to_string()),
            Some("/tmp/test-vault/notes.md".to_string())
        );
    }

    #[test]
    fn resolve_unknown_alias_errors() {
        let resolver = test_resolver();
        let result = resolver.resolve("{alias:nonexistent}/file.md");
        assert!(result.is_err(), "{result:?}");
        let err = format!("{}", result.err().unwrap_or(AliasError::UnknownAlias { name: String::new() }));
        assert!(err.contains("nonexistent"));
    }

    #[test]
    fn resolve_cross_domain_rejection() {
        let resolver = test_resolver();
        // Path that resolves outside the domain boundary
        let result = resolver.resolve("/etc/passwd");
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn resolve_plain_path_within_boundary() {
        let resolver = test_resolver();
        let result = resolver.resolve("/tmp/test-vault/some/file.md");
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn resolve_unchecked_skips_boundary() {
        let resolver = test_resolver();
        let result = resolver.resolve_unchecked("/etc/passwd");
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn glob_base_extracts_directory() {
        assert_eq!(glob_base("/tmp/test/*"), "/tmp/test");
        assert_eq!(glob_base("/tmp/test/**/*.rs"), "/tmp/test");
        assert_eq!(glob_base("/tmp/test"), "/tmp/test");
    }

    #[test]
    fn aliases_returns_configured() {
        let resolver = test_resolver();
        assert_eq!(resolver.aliases().len(), 2);
        assert!(resolver.aliases().contains_key("vault"));
        assert!(resolver.aliases().contains_key("agents"));
    }
}
