use crate::config::types::{ConfigError, DomainName, PathGlob};
use crate::vault::types::{Confidence, VaultFile, VaultType};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A domain: name, filesystem boundaries, path aliases, and cross-domain read permissions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Domain {
    pub name: DomainName,
    pub paths: Vec<PathGlob>,
    #[serde(default)]
    pub aliases: HashMap<String, String>,
    /// Other domains this domain is allowed to search and read from.
    /// Omitted or empty = self-only access.
    #[serde(default)]
    pub can_read: Vec<String>,
}

impl Domain {
    /// Check if a canonicalized path is within this domain's boundaries.
    pub fn path_allowed(&self, path: &std::path::Path) -> bool {
        self.paths.iter().any(|g: &PathGlob| g.matches(path))
    }

    /// Build a Domain from a vault file with `type: domain` and `confidence: confirmed`.
    /// Name comes from frontmatter `domain` field, or filename stem.
    /// Paths from `## Paths` section, aliases from `## Aliases` section.
    pub fn from_vault_file(vf: &VaultFile) -> Result<Domain, ConfigError> {
        // Must be type: domain
        if vf.frontmatter.file_type != VaultType::Domain {
            return Err(ConfigError::InvalidDomainName {
                name: vf.path.display().to_string(),
                reason: "not a domain vault file".to_string(),
            });
        }

        // Must be confidence: confirmed
        if vf.frontmatter.confidence != Some(Confidence::Confirmed) {
            return Err(ConfigError::InvalidDomainName {
                name: vf.path.display().to_string(),
                reason: "domain not confirmed".to_string(),
            });
        }

        // Name from frontmatter domain field, or filename stem
        let name_str = vf.frontmatter.domain.clone().unwrap_or_else(|| {
            vf.path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        });
        let name = DomainName::new(&name_str)?;

        let mut paths = Vec::new();
        let mut aliases = HashMap::new();

        let mut current_section: Option<&str> = None;

        for line in vf.body.lines() {
            if line.starts_with("## Paths") {
                current_section = Some("paths");
                continue;
            } else if line.starts_with("## Aliases") {
                current_section = Some("aliases");
                continue;
            } else if line.starts_with("## ") {
                current_section = None;
                continue;
            }

            let trimmed = line.trim();
            if trimmed.is_empty() || !trimmed.starts_with("- ") {
                continue;
            }
            let item = trimmed.strip_prefix("- ").unwrap_or(trimmed);

            match current_section {
                Some("paths") => {
                    if let Ok(glob) = PathGlob::new(item) {
                        paths.push(glob);
                    }
                }
                Some("aliases") => {
                    if let Some((key, value)) = item.split_once(": ") {
                        aliases.insert(key.trim().to_string(), value.trim().to_string());
                    }
                }
                _ => {}
            }
        }

        let can_read = vf.frontmatter.can_read.clone();

        Ok(Domain { name, paths, aliases, can_read })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::vault::types::Frontmatter;
    use std::path::PathBuf;

    fn test_domain() -> Domain {
        let name = DomainName::new("test").unwrap();
        Domain {
            name,
            paths: vec![PathGlob::new("/tmp/test/*").unwrap()],
            aliases: HashMap::new(),
            can_read: Vec::new(),
        }
    }

    #[test]
    fn path_allowed_within_domain() {
        let domain = test_domain();
        assert!(domain.path_allowed(std::path::Path::new("/tmp/test/file.txt")));
    }

    #[test]
    fn path_denied_outside_domain() {
        let domain = test_domain();
        assert!(!domain.path_allowed(std::path::Path::new("/etc/passwd")));
    }

    #[test]
    fn from_vault_file_parses_domain() {
        let vf = VaultFile {
            path: PathBuf::from("/vault/domains/myapp.md"),
            frontmatter: Frontmatter {
                file_type: VaultType::Domain,
                domain: Some("myapp".to_string()),
                status: Some(crate::vault::types::Status::Active),
                confidence: Some(Confidence::Confirmed),
                updated: None,
                summary: None,
                related: Vec::new(),
                tags: Vec::new(),
                can_read: Vec::new(),
            },
            body: "## Paths\n- ~/Code/myapp-*/*\n- ~/Code/mycompany/*\n\n## Aliases\n- repos: ~/Code\n- docs: ~/Documents/myapp\n".to_string(),
        };

        let domain = Domain::from_vault_file(&vf);
        assert!(domain.is_ok(), "{domain:?}");
        let domain = domain.unwrap();
        assert_eq!(domain.name.as_str(), "myapp");
        assert_eq!(domain.paths.len(), 2);
        assert_eq!(domain.aliases.get("repos").map(|s| s.as_str()), Some("~/Code"));
        assert_eq!(domain.aliases.get("docs").map(|s| s.as_str()), Some("~/Documents/myapp"));
    }

    #[test]
    fn from_vault_file_uses_filename_stem_when_no_domain_field() {
        let vf = VaultFile {
            path: PathBuf::from("/vault/domains/personal.md"),
            frontmatter: Frontmatter {
                file_type: VaultType::Domain,
                domain: None,
                status: None,
                confidence: Some(Confidence::Confirmed),
                updated: None,
                summary: None,
                related: Vec::new(),
                tags: Vec::new(),
                can_read: Vec::new(),
            },
            body: "## Paths\n- ~/projects/*\n".to_string(),
        };

        let domain = Domain::from_vault_file(&vf);
        assert!(domain.is_ok(), "{domain:?}");
        assert_eq!(domain.unwrap().name.as_str(), "personal");
    }

    #[test]
    fn from_vault_file_rejects_non_domain_type() {
        let vf = VaultFile {
            path: PathBuf::from("/vault/project.md"),
            frontmatter: Frontmatter {
                file_type: VaultType::Project,
                domain: None,
                status: None,
                confidence: Some(Confidence::Confirmed),
                updated: None,
                summary: None,
                related: Vec::new(),
                tags: Vec::new(),
                can_read: Vec::new(),
            },
            body: String::new(),
        };

        let result = Domain::from_vault_file(&vf);
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn from_vault_file_rejects_unconfirmed() {
        let vf = VaultFile {
            path: PathBuf::from("/vault/domains/test.md"),
            frontmatter: Frontmatter {
                file_type: VaultType::Domain,
                domain: Some("test".to_string()),
                status: None,
                confidence: Some(Confidence::Inferred),
                updated: None,
                summary: None,
                related: Vec::new(),
                tags: Vec::new(),
                can_read: Vec::new(),
            },
            body: "## Paths\n- /tmp/*\n".to_string(),
        };

        let result = Domain::from_vault_file(&vf);
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn from_vault_file_parses_can_read() {
        let vf = VaultFile {
            path: PathBuf::from("/vault/domains/wardwell.md"),
            frontmatter: Frontmatter {
                file_type: VaultType::Domain,
                domain: Some("wardwell".to_string()),
                status: Some(crate::vault::types::Status::Active),
                confidence: Some(Confidence::Confirmed),
                updated: None,
                summary: None,
                related: Vec::new(),
                tags: Vec::new(),
                can_read: vec!["personal".to_string(), "general".to_string()],
            },
            body: "## Paths\n- ~/Code/wardwell/*\n".to_string(),
        };

        let domain = Domain::from_vault_file(&vf);
        assert!(domain.is_ok(), "{domain:?}");
        let domain = domain.unwrap();
        assert_eq!(domain.name.as_str(), "wardwell");
        assert_eq!(domain.can_read, vec!["personal", "general"]);
    }

    #[test]
    fn from_vault_file_empty_can_read_defaults() {
        let vf = VaultFile {
            path: PathBuf::from("/vault/domains/solo.md"),
            frontmatter: Frontmatter {
                file_type: VaultType::Domain,
                domain: Some("solo".to_string()),
                status: None,
                confidence: Some(Confidence::Confirmed),
                updated: None,
                summary: None,
                related: Vec::new(),
                tags: Vec::new(),
                can_read: Vec::new(),
            },
            body: "## Paths\n- /tmp/solo/*\n".to_string(),
        };

        let domain = Domain::from_vault_file(&vf);
        assert!(domain.is_ok(), "{domain:?}");
        assert!(domain.unwrap().can_read.is_empty());
    }
}
