use crate::domain::model::Domain;
use crate::vault::types::{Confidence, VaultType};
use std::path::Path;

/// Registry of active domains, loaded from vault files.
#[derive(Debug, Clone)]
pub struct DomainRegistry {
    domains: Vec<Domain>,
}

impl DomainRegistry {
    /// Build a registry by reading domain vault files from `vault_path/domains/`.
    /// Only loads files with `type: domain`, `confidence: confirmed`.
    pub fn from_vault(vault_path: &Path) -> Self {
        let domains_dir = vault_path.join("domains");
        if !domains_dir.exists() {
            return Self { domains: Vec::new() };
        }

        let entries = match std::fs::read_dir(&domains_dir) {
            Ok(e) => e,
            Err(_) => return Self { domains: Vec::new() },
        };

        let mut domains = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }

            let vf = match crate::vault::reader::read_file(&path) {
                Ok(vf) => vf,
                Err(_) => continue,
            };

            if vf.frontmatter.file_type != VaultType::Domain {
                continue;
            }

            if vf.frontmatter.confidence != Some(Confidence::Confirmed) {
                continue;
            }

            if let Ok(domain) = Domain::from_vault_file(&vf) {
                domains.push(domain);
            }
        }

        Self { domains }
    }

    /// Build a registry from pre-existing Domain structs (for config fallback / migration).
    pub fn from_domains(domains: Vec<Domain>) -> Self {
        Self { domains }
    }

    /// Build an empty registry.
    pub fn empty() -> Self {
        Self { domains: Vec::new() }
    }

    /// Resolve which domain a path belongs to.
    pub fn resolve(&self, cwd: &Path) -> Option<&Domain> {
        self.domains.iter().find(|d| {
            d.paths.iter().any(|p| {
                let expanded = p.expand();
                let base = expanded.to_string_lossy();
                let base_dir = base.split('*').next().unwrap_or(&base);
                let base_dir = base_dir.trim_end_matches('/');
                cwd.starts_with(base_dir)
            })
        })
    }

    pub fn is_empty(&self) -> bool {
        self.domains.is_empty()
    }

    pub fn all(&self) -> &[Domain] {
        &self.domains
    }

    pub fn names(&self) -> Vec<String> {
        self.domains.iter().map(|d| d.name.as_str().to_string()).collect()
    }

    /// Find a domain by name.
    pub fn find(&self, name: &str) -> Option<&Domain> {
        self.domains.iter().find(|d| d.name.as_str() == name)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::config::types::{DomainName, PathGlob};
    use std::collections::HashMap;

    fn make_domain(name: &str, path_glob: &str) -> Domain {
        Domain {
            name: DomainName::new(name).unwrap(),
            paths: vec![PathGlob::new(path_glob).unwrap()],
            aliases: HashMap::new(),
            can_read: Vec::new(),
        }
    }

    #[test]
    fn resolve_finds_matching_domain() {
        let reg = DomainRegistry::from_domains(vec![
            make_domain("work", "/tmp/work/*"),
            make_domain("personal", "/tmp/personal/*"),
        ]);

        let result = reg.resolve(Path::new("/tmp/work/project"));
        assert!(result.is_some());
        assert_eq!(result.map(|d| d.name.as_str()), Some("work"));
    }

    #[test]
    fn resolve_returns_none_for_unknown_path() {
        let reg = DomainRegistry::from_domains(vec![
            make_domain("work", "/tmp/work/*"),
        ]);

        assert!(reg.resolve(Path::new("/etc/something")).is_none());
    }

    #[test]
    fn empty_registry() {
        let reg = DomainRegistry::empty();
        assert!(reg.is_empty());
        assert!(reg.all().is_empty());
        assert!(reg.names().is_empty());
    }

    #[test]
    fn from_vault_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let reg = DomainRegistry::from_vault(dir.path());
        assert!(reg.is_empty());
    }

    #[test]
    fn from_vault_loads_confirmed_domains() {
        let dir = tempfile::tempdir().unwrap();
        let domains_dir = dir.path().join("domains");
        std::fs::create_dir_all(&domains_dir).unwrap();

        // Write a confirmed domain file
        std::fs::write(
            domains_dir.join("testdomain.md"),
            "---\ntype: domain\ndomain: testdomain\nconfidence: confirmed\nstatus: active\n---\n## Paths\n- /tmp/test/*\n\n## Aliases\n- code: /tmp/test\n",
        ).unwrap();

        // Write an inferred domain file (should be skipped)
        std::fs::write(
            domains_dir.join("inferred.md"),
            "---\ntype: domain\ndomain: inferred\nconfidence: inferred\n---\n## Paths\n- /tmp/inferred/*\n",
        ).unwrap();

        let reg = DomainRegistry::from_vault(dir.path());
        assert_eq!(reg.all().len(), 1);
        assert_eq!(reg.names(), vec!["testdomain"]);
        assert!(reg.resolve(Path::new("/tmp/test/foo")).is_some());
        assert!(reg.resolve(Path::new("/tmp/inferred/foo")).is_none());
    }

    #[test]
    fn find_by_name() {
        let reg = DomainRegistry::from_domains(vec![
            make_domain("work", "/tmp/work/*"),
            make_domain("personal", "/tmp/personal/*"),
        ]);

        assert!(reg.find("work").is_some());
        assert_eq!(reg.find("work").map(|d| d.name.as_str()), Some("work"));
        assert!(reg.find("nonexistent").is_none());
    }
}
