use crate::domain::model::Domain;
use crate::domain::path::{check_dangerous_patterns, resolve_path};
use std::path::Path;

/// Result of an enforcement check.
#[derive(Debug, Clone)]
pub enum EnforcementResult {
    Allow,
    Block { reason: String },
}

impl EnforcementResult {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// The enforcement boundary — checks paths against domain boundaries.
pub struct BoundaryEnforcer<'a> {
    domain: &'a Domain,
}

impl<'a> BoundaryEnforcer<'a> {
    pub fn new(domain: &'a Domain) -> Self {
        Self { domain }
    }

    /// Check if a path access is allowed by the domain boundary.
    /// 1. Check for dangerous patterns in raw string
    /// 2. Canonicalize using filesystem (catches symlinks)
    /// 3. Check against domain boundaries
    pub fn check_path(&self, path_str: &str) -> EnforcementResult {
        if let Err(e) = check_dangerous_patterns(path_str) {
            return EnforcementResult::Block {
                reason: e.to_string(),
            };
        }

        let path = Path::new(path_str);
        let canonical = match resolve_path(path) {
            Ok(p) => p,
            Err(e) => {
                return EnforcementResult::Block {
                    reason: format!("path resolution failed: {e}"),
                };
            }
        };

        if self.domain.path_allowed(&canonical) {
            EnforcementResult::Allow
        } else {
            EnforcementResult::Block {
                reason: format!(
                    "path '{}' is outside domain boundary (resolved: '{}')",
                    path_str,
                    canonical.display()
                ),
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::config::types::{DomainName, PathGlob};
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Domain) {
        let dir = TempDir::new().unwrap();

        let test_file = dir.path().join("allowed.txt");
        std::fs::write(&test_file, "allowed content").ok();

        let domain = Domain {
            name: DomainName::new("test").unwrap(),
            paths: vec![PathGlob::new(&format!("{}/*", dir.path().display()))
                .unwrap()],
            aliases: HashMap::new(),
            can_read: Vec::new(),
        };

        (dir, domain)
    }

    #[test]
    fn allows_file_within_boundary() {
        let (dir, domain) = setup();
        let enforcer = BoundaryEnforcer::new(&domain);

        let test_file = dir.path().join("allowed.txt");
        let result = enforcer.check_path(&test_file.display().to_string());
        assert!(result.is_allowed());
    }

    #[test]
    fn blocks_file_outside_boundary() {
        let (_dir, domain) = setup();
        let enforcer = BoundaryEnforcer::new(&domain);

        let result = enforcer.check_path("/etc/passwd");
        assert!(!result.is_allowed());
    }

    #[test]
    fn blocks_traversal_attack() {
        let (_dir, domain) = setup();
        let enforcer = BoundaryEnforcer::new(&domain);

        let result = enforcer.check_path("../../../etc/passwd");
        assert!(!result.is_allowed());
    }

    #[test]
    fn blocks_url_encoded_traversal() {
        let (_dir, domain) = setup();
        let enforcer = BoundaryEnforcer::new(&domain);

        let result = enforcer.check_path("%2e%2e/%2e%2e/etc/passwd");
        assert!(!result.is_allowed());
    }

    #[test]
    fn blocks_null_byte() {
        let (_dir, domain) = setup();
        let enforcer = BoundaryEnforcer::new(&domain);

        let result = enforcer.check_path("/tmp/test\x00.txt");
        assert!(!result.is_allowed());
    }

    #[test]
    fn blocks_symlink_outside_boundary() {
        use std::os::unix::fs::symlink;
        let (dir, domain) = setup();

        let outside = TempDir::new().unwrap();
        let outside_file = outside.path().join("secret.txt");
        std::fs::write(&outside_file, "secret").ok();

        let link = dir.path().join("sneaky.txt");
        symlink(&outside_file, &link).ok();

        let enforcer = BoundaryEnforcer::new(&domain);

        let result = enforcer.check_path(&link.display().to_string());
        assert!(!result.is_allowed(), "symlink to outside should be blocked");
    }
}
