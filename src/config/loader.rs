use crate::config::types::{ConfigError, DomainName, PathGlob};
use crate::domain::model::Domain;
use crate::domain::registry::DomainRegistry;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Top-level wardwell configuration.
#[derive(Debug)]
pub struct WardwellConfig {
    pub vault_path: PathBuf,
    pub registry: DomainRegistry,
    pub session_sources: Vec<PathBuf>,
    pub exclude: Vec<String>,
    pub ai: AiConfig,
}

/// AI configuration for session summarization.
#[derive(Debug, Clone)]
pub struct AiConfig {
    /// Model for summarization. Defaults to "haiku".
    pub summarize_model: String,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            summarize_model: "haiku".to_string(),
        }
    }
}

/// Raw YAML representation of config.yml.
#[derive(Debug, Deserialize)]
struct RawConfig {
    vault_path: String,
    #[serde(default)]
    domains: HashMap<String, RawDomainEntry>,
    /// Ignored — kept for backwards compatibility with old configs.
    #[serde(default)]
    #[allow(dead_code)]
    sources: Vec<String>,
    #[serde(default)]
    session_sources: Vec<String>,
    /// Ignored — kept for backwards compatibility with old configs.
    #[serde(default)]
    #[allow(dead_code)]
    seed_paths: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    /// Ignored — kept for backwards compatibility with old configs.
    #[serde(default)]
    #[allow(dead_code)]
    agents_dir: Option<String>,
    #[serde(default)]
    ai: Option<RawAiConfig>,
}

#[derive(Debug, Deserialize)]
struct RawDomainEntry {
    paths: Vec<String>,
    #[serde(default)]
    aliases: HashMap<String, String>,
    #[serde(default)]
    can_read: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawAiConfig {
    summarize_model: Option<String>,
    /// Ignored — kept for backwards compatibility with old configs.
    #[serde(default)]
    #[allow(dead_code)]
    synthesize_model: Option<String>,
}

/// Load and parse wardwell config.
/// Falls back to `~/.wardwell/config.yml` if no path given.
pub fn load(path: Option<&Path>) -> Result<WardwellConfig, ConfigError> {
    let config_path = match path {
        Some(p) => p.to_path_buf(),
        None => config_dir().join("config.yml"),
    };

    if !config_path.exists() {
        return Err(ConfigError::NotFound {
            path: config_path.display().to_string(),
        });
    }

    let contents = std::fs::read_to_string(&config_path)?;
    let raw: RawConfig = serde_yaml::from_str(&contents)?;

    let vault_path = expand_tilde(&raw.vault_path);

    // Try loading domains from vault first (new vault-object model)
    let vault_registry = DomainRegistry::from_vault(&vault_path);

    let registry = if !vault_registry.is_empty() {
        vault_registry
    } else if !raw.domains.is_empty() {
        // Fall back to config domains (migration path)
        let mut config_domains = Vec::new();
        for (name, entry) in &raw.domains {
            let domain_name = DomainName::new(name)?;
            let mut paths = Vec::new();
            for p in &entry.paths {
                paths.push(PathGlob::new(p)?);
            }
            config_domains.push(Domain {
                name: domain_name,
                paths,
                aliases: entry.aliases.clone(),
                can_read: entry.can_read.clone(),
            });
        }
        DomainRegistry::from_domains(config_domains)
    } else {
        DomainRegistry::empty()
    };

    let session_sources = raw.session_sources.iter().map(|s| expand_tilde(s)).collect();
    let exclude = raw.exclude;

    let ai = match raw.ai {
        Some(raw_ai) => {
            let defaults = AiConfig::default();
            AiConfig {
                summarize_model: raw_ai.summarize_model.unwrap_or(defaults.summarize_model),
            }
        }
        None => AiConfig::default(),
    };

    Ok(WardwellConfig {
        vault_path,
        registry,
        session_sources,
        exclude,
        ai,
    })
}

/// Resolve the wardwell config directory. Defaults to ~/.wardwell.
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("WARDWELL_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".wardwell")
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_config(yaml: &str) -> Option<NamedTempFile> {
        NamedTempFile::new().ok().and_then(|mut f| {
            f.write_all(yaml.as_bytes()).ok()?;
            Some(f)
        })
    }

    #[test]
    fn load_valid_config() {
        let yaml = r#"
vault_path: /tmp/test-vault

domains:
  personal:
    paths:
      - /tmp/notes/*
    aliases:
      vault: /tmp/notes
  work:
    paths:
      - /tmp/work/*

session_sources:
  - /tmp/sessions/

"#;
        let f = write_config(yaml).unwrap();
        let config = load(Some(f.path())).unwrap();
        // Config domains are loaded as fallback (no vault domain files exist)
        assert_eq!(config.registry.all().len(), 2);
        assert_eq!(config.vault_path.display().to_string(), "/tmp/test-vault");
    }

    #[test]
    fn load_missing_file_errors() {
        let result = load(Some(Path::new("/nonexistent/config.yml")));
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn load_empty_domains() {
        let yaml = r#"
vault_path: /tmp/vault
domains: {}
session_sources: []
"#;
        let f = write_config(yaml).unwrap();
        let config = load(Some(f.path())).unwrap();
        assert_eq!(config.registry.all().len(), 0);
    }

    #[test]
    fn load_config_with_can_read() {
        let yaml = r#"
vault_path: /tmp/test-vault

domains:
  wardwell:
    paths:
      - /tmp/wardwell/*
    can_read: [personal, general]
  personal:
    paths:
      - /tmp/personal/*

session_sources: []
"#;
        let f = write_config(yaml).unwrap();
        let config = load(Some(f.path())).unwrap();
        let wardwell = config.registry.find("wardwell").unwrap();
        assert_eq!(wardwell.can_read, vec!["personal", "general"]);

        let personal = config.registry.find("personal").unwrap();
        assert!(personal.can_read.is_empty());
    }

    #[test]
    fn expand_tilde_with_home() {
        let result = expand_tilde("~/documents");
        let home = dirs::home_dir().unwrap();
        assert_eq!(result, home.join("documents"));
    }

    #[test]
    fn expand_tilde_absolute_path() {
        let result = expand_tilde("/absolute/path");
        assert_eq!(result, PathBuf::from("/absolute/path"));
    }

    #[test]
    fn load_config_with_unknown_keys() {
        let yaml = r#"
vault_path: /tmp/test-vault
session_sources: []
future_key: some_value
another_unknown:
  nested: true
"#;
        let f = write_config(yaml).unwrap();
        let config = load(Some(f.path()));
        assert!(config.is_ok(), "{config:?}");
    }
}
