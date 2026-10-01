use crate::config::types::{ConfigError, DomainName, PathGlob};
use crate::domain::model::Domain;
use crate::domain::registry::DomainRegistry;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Feature flags for optional MCP capabilities.
#[derive(Debug, Clone)]
pub struct FeatureFlags {
    pub partial_reads: bool,
    pub graph_navigation: bool,
    pub entity_resolution: bool,
    pub unlinked_mentions: bool,
}

impl Default for FeatureFlags {
    fn default() -> Self {
        Self {
            partial_reads: true,
            graph_navigation: true,
            entity_resolution: true,
            unlinked_mentions: true,
        }
    }
}

/// Top-level wardwell configuration.
#[derive(Debug)]
pub struct WardwellConfig {
    pub vault_path: PathBuf,
    pub registry: DomainRegistry,
    pub session_sources: Vec<PathBuf>,
    pub exclude: Vec<String>,
    pub ai: AiConfig,
    /// Whether the stop hook prompts for session logging. Defaults to true.
    pub stop_hook: bool,
    /// Whether the kanban MCP tool is enabled. Defaults to false.
    pub kanban_enabled: bool,
    /// Named FTS queries for kanban columns (column name → query string).
    pub kanban_queries: HashMap<String, String>,
    /// Prefix mappings for kanban item display (prefix → label).
    pub kanban_prefixes: HashMap<String, String>,
    /// Feature flags for optional MCP capabilities.
    pub features: FeatureFlags,
    /// Tracker bindings in `<domain>/<project>` order, then in file order
    /// within a project. At most one per provider per project.
    pub trackers: Vec<TrackerBinding>,
    /// Working directories mapped to vault projects, keyed by `<domain>/<project>`.
    pub projects: BTreeMap<String, ProjectMapping>,
}

impl WardwellConfig {
    /// The issue tracker binding of a vault project, if one is configured.
    /// A binding that mirrors merged changes is not an issue tracker, so the
    /// kanban read path, the read-only lock, and session start never see it.
    pub fn tracker_for(&self, domain: &str, project: &str) -> Option<&TrackerBinding> {
        self.bindings_for(domain, project).into_iter().find(|b| crate::tracker::mirrors_issues(&b.provider))
    }

    /// Every binding of a vault project, in file order.
    pub fn bindings_for(&self, domain: &str, project: &str) -> Vec<&TrackerBinding> {
        self.trackers.iter().filter(|b| b.domain == domain && b.project == project).collect()
    }

    /// The bindings that mirror issues, the ones kanban and session start read.
    pub fn issue_bindings(&self) -> impl Iterator<Item = &TrackerBinding> {
        self.trackers.iter().filter(|b| crate::tracker::mirrors_issues(&b.provider))
    }
}

/// Binds one vault project to an external tracker it mirrors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerBinding {
    pub domain: String,
    pub project: String,
    /// Adapter name, e.g. `linear`.
    pub provider: String,
    /// Provider team key the project mirrors, e.g. `COR`. Empty for a
    /// provider that mirrors a repository.
    pub team: String,
    /// Name of the credential file under `~/.wardwell/trackers/`.
    pub credential: String,
    /// When true, kanban write actions on this project are refused.
    pub readonly: bool,
    /// When true, `wardwell setup` installs the tracker policy for this
    /// provider: the write gate and the deny list. Optional, off by default.
    pub gate: bool,
    /// `<owner>/<name>` of the repository a `github` binding mirrors.
    pub repository: Option<String>,
}

impl TrackerBinding {
    /// `<domain>/<project>`, the config key the binding sits under.
    pub fn key(&self) -> String {
        format!("{}/{}", self.domain, self.project)
    }

    /// What the binding mirrors at its provider: the repository when it has
    /// one, else the team key. Names the binding in markers and status lines.
    pub fn scope(&self) -> &str {
        self.repository.as_deref().unwrap_or(&self.team)
    }
}

/// Maps working directories to one vault project, so a session started in
/// any of `paths` reads and writes that project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectMapping {
    pub domain: String,
    pub project: String,
    /// Absolute directories, tilde expanded, without a trailing slash.
    pub paths: Vec<PathBuf>,
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
    #[serde(default = "default_true")]
    stop_hook: bool,
    #[serde(default)]
    kanban: Option<RawKanbanConfig>,
    #[serde(default)]
    features: Option<RawFeatureFlags>,
    #[serde(default)]
    trackers: BTreeMap<String, serde_yaml::Value>,
    #[serde(default)]
    projects: ProjectEntries,
}

/// The `projects:` entries in file order, duplicates kept so they can be
/// refused by name instead of silently overwritten.
#[derive(Debug, Default)]
struct ProjectEntries(Vec<(String, serde_yaml::Value)>);

impl<'de> Deserialize<'de> for ProjectEntries {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Entries;
        impl<'de> serde::de::Visitor<'de> for Entries {
            type Value = ProjectEntries;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a mapping of <domain>/<project> to its paths")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<ProjectEntries, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry::<String, serde_yaml::Value>()? {
                    entries.push(entry);
                }
                Ok(ProjectEntries(entries))
            }
        }
        deserializer.deserialize_map(Entries)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProjectEntry {
    paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawTrackerBinding {
    provider: String,
    #[serde(default)]
    team: Option<String>,
    #[serde(default)]
    credential: Option<String>,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    readonly: bool,
    #[serde(default)]
    gate: bool,
}

fn default_true() -> bool {
    true
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
struct RawKanbanConfig {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    queries: HashMap<String, String>,
    #[serde(default)]
    prefixes: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct RawFeatureFlags {
    #[serde(default = "default_true")]
    partial_reads: bool,
    #[serde(default = "default_true")]
    graph_navigation: bool,
    #[serde(default = "default_true")]
    entity_resolution: bool,
    #[serde(default = "default_true")]
    unlinked_mentions: bool,
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

    parse(&std::fs::read_to_string(&config_path)?)
}

/// Parse config.yml text. Domains still load from the vault it names.
pub fn parse(contents: &str) -> Result<WardwellConfig, ConfigError> {
    let raw: RawConfig = serde_yaml::from_str(contents)?;

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

    let (kanban_enabled, kanban_queries, kanban_prefixes) = match raw.kanban {
        Some(k) => (k.enabled, k.queries, k.prefixes),
        None => (false, HashMap::new(), HashMap::new()),
    };

    let features = match raw.features {
        Some(f) => FeatureFlags {
            partial_reads: f.partial_reads,
            graph_navigation: f.graph_navigation,
            entity_resolution: f.entity_resolution,
            unlinked_mentions: f.unlinked_mentions,
        },
        None => FeatureFlags::default(),
    };

    let trackers = tracker_bindings(raw.trackers)?;
    reject_prefix_collisions(&trackers, &kanban_prefixes)?;
    let projects = project_mappings(raw.projects)?;

    Ok(WardwellConfig {
        vault_path,
        registry,
        session_sources,
        exclude,
        ai,
        stop_hook: raw.stop_hook,
        kanban_enabled,
        kanban_queries,
        kanban_prefixes,
        features,
        trackers,
        projects,
    })
}

fn project_mappings(raw: ProjectEntries) -> Result<BTreeMap<String, ProjectMapping>, ConfigError> {
    let invalid = |key: &str, reason: String| ConfigError::InvalidProjectMapping { key: key.to_string(), reason };
    let mut mappings: BTreeMap<String, ProjectMapping> = BTreeMap::new();
    let mut owners: BTreeMap<PathBuf, String> = BTreeMap::new();
    for (key, value) in raw.0 {
        if mappings.contains_key(&key) {
            return Err(invalid(&key, "the key appears twice under `projects`; merge its paths into one entry".into()));
        }
        let (domain, project) = split_project_key(&key).map_err(|_| invalid(&key, "key must be <domain>/<project>".into()))?;
        let entry: RawProjectEntry = serde_yaml::from_value(value)
            .map_err(|e| invalid(&key, format!("{e}; a project takes only `paths`")))?;
        if entry.paths.is_empty() {
            return Err(invalid(&key, "`paths` needs at least one path".into()));
        }
        let paths: Vec<PathBuf> = entry.paths.iter().map(|p| project_path(&key, p)).collect::<Result<_, _>>()?;
        for path in &paths {
            // Existing folders compare by their real path, so a symlink to a
            // folder mapped elsewhere is caught.
            let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            if let Some(owner) = owners.insert(real, key.clone()) {
                let place = if owner == key { "twice in this entry".to_string() } else { format!("under both {owner} and {key}") };
                return Err(invalid(&key, format!("path {} is listed {place}; keep it under one project", path.display())));
            }
        }
        mappings.insert(key, ProjectMapping { domain, project, paths });
    }
    Ok(mappings)
}

/// One mapped directory: tilde expanded, absolute, no trailing slash.
fn project_path(key: &str, raw: &str) -> Result<PathBuf, ConfigError> {
    let path = expand_tilde(raw.trim());
    if !path.is_absolute() {
        return Err(ConfigError::InvalidProjectMapping {
            key: key.to_string(),
            reason: format!("path '{raw}' must be absolute or start with ~/"),
        });
    }
    Ok(path.components().collect())
}

fn tracker_bindings(raw: BTreeMap<String, serde_yaml::Value>) -> Result<Vec<TrackerBinding>, ConfigError> {
    let mut bindings = Vec::new();
    for (key, value) in raw {
        let (domain, project) = split_project_key(&key)?;
        let invalid = |reason: String| ConfigError::InvalidTrackerBinding { key: key.clone(), reason };
        let entries = match value {
            serde_yaml::Value::Sequence(entries) => entries,
            mapping @ serde_yaml::Value::Mapping(_) => vec![mapping],
            _ => return Err(invalid("the entry must be a mapping or a list of mappings".into())),
        };
        let mut providers: Vec<String> = Vec::new();
        for entry in entries {
            let entry: RawTrackerBinding = serde_yaml::from_value(entry).map_err(|e| invalid(e.to_string()))?;
            if providers.contains(&entry.provider) {
                return Err(invalid(format!(
                    "provider {} appears twice; a project takes at most one binding per provider",
                    entry.provider
                )));
            }
            providers.push(entry.provider.clone());
            bindings.push(tracker_binding(&domain, &project, entry).map_err(invalid)?);
        }
    }
    Ok(bindings)
}

/// One binding from its raw entry, with the fields its provider takes.
fn tracker_binding(domain: &str, project: &str, entry: RawTrackerBinding) -> Result<TrackerBinding, String> {
    let supported = crate::tracker::SUPPORTED_PROVIDERS;
    if !supported.contains(&entry.provider.as_str()) {
        return Err(format!("provider '{}' is not supported; supported: {}", entry.provider, supported.join(", ")));
    }
    let provider = entry.provider.as_str();
    let refuse = |field: &str| format!("provider {provider} does not take `{field}`");
    let (team, repository) = match crate::tracker::mirrors_issues(provider) {
        true => {
            if entry.repository.is_some() {
                return Err(refuse("repository"));
            }
            (entry.team.ok_or_else(|| format!("provider {provider} needs `team`"))?, None)
        }
        false => {
            match (entry.team.is_some(), entry.readonly, entry.gate) {
                (true, _, _) => return Err(refuse("team")),
                (_, true, _) => return Err(refuse("readonly")),
                (_, _, true) => return Err(refuse("gate")),
                _ => {}
            }
            let repository = entry.repository.ok_or_else(|| format!("provider {provider} needs `repository`"))?;
            (String::new(), Some(repository_name(&repository)?))
        }
    };
    let credential = match (entry.credential, repository.is_some()) {
        (Some(credential), _) => credential,
        (None, true) => provider.to_string(),
        (None, false) => return Err(format!("provider {provider} needs `credential`")),
    };
    Ok(TrackerBinding {
        domain: domain.to_string(),
        project: project.to_string(),
        provider: entry.provider,
        team,
        credential,
        readonly: entry.readonly,
        gate: entry.gate,
        repository,
    })
}

/// `<owner>/<name>` with two non-empty segments of letters, digits, `-`, `_` or `.`.
fn repository_name(raw: &str) -> Result<String, String> {
    let segment = |s: &str| !s.is_empty() && s != "." && s != ".." && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    match raw.trim().split_once('/') {
        Some((owner, name)) if segment(owner) && segment(name) => Ok(raw.trim().to_string()),
        _ => Err(format!("repository '{raw}' must be <owner>/<name>")),
    }
}

/// A tracker key and a native kanban ticket id must never share a prefix,
/// or a lookup could not tell them apart. `prefixes` maps project to prefix.
fn reject_prefix_collisions(
    trackers: &[TrackerBinding],
    prefixes: &HashMap<String, String>,
) -> Result<(), ConfigError> {
    let normal = |prefix: &str| prefix.trim_end_matches('-').to_ascii_uppercase();
    for binding in trackers.iter().filter(|b| crate::tracker::mirrors_issues(&b.provider)) {
        let key = binding.key();
        let mut clashes: Vec<(&String, &String)> =
            prefixes.iter().filter(|(_, prefix)| normal(prefix) == normal(&binding.team)).collect();
        clashes.sort();
        if let Some((project, prefix)) = clashes.first() {
            return Err(ConfigError::InvalidTrackerBinding {
                key: key.clone(),
                reason: format!(
                    "team key {} equals the native kanban prefix {prefix} of project {project}; change one of them",
                    binding.team
                ),
            });
        }
    }
    Ok(())
}

/// The domain and project of a `<domain>/<project>` key, or None when it
/// has another number of segments, an empty segment, or a `.` or `..`.
pub fn project_key_parts(key: &str) -> Option<(&str, &str)> {
    let (domain, project) = key.split_once('/')?;
    let ok = |s: &str| !s.is_empty() && s != "." && s != ".." && !s.contains('/');
    (ok(domain) && ok(project)).then_some((domain, project))
}

fn split_project_key(key: &str) -> Result<(String, String), ConfigError> {
    match project_key_parts(key) {
        Some((domain, project)) => Ok((domain.to_string(), project.to_string())),
        None => Err(ConfigError::InvalidTrackerBinding {
            key: key.to_string(),
            reason: "key must be <domain>/<project>".to_string(),
        }),
    }
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

    #[test]
    fn kanban_absent_defaults_to_disabled() {
        let yaml = r#"
vault_path: /tmp/test-vault
session_sources: []
"#;
        let f = write_config(yaml).unwrap();
        let config = load(Some(f.path())).unwrap();
        assert!(!config.kanban_enabled);
        assert!(config.kanban_queries.is_empty());
        assert!(config.kanban_prefixes.is_empty());
    }

    #[test]
    fn kanban_minimal_section() {
        let yaml = r#"
vault_path: /tmp/test-vault
session_sources: []
kanban:
  enabled: true
"#;
        let f = write_config(yaml).unwrap();
        let config = load(Some(f.path())).unwrap();
        assert!(config.kanban_enabled);
        assert!(config.kanban_queries.is_empty());
        assert!(config.kanban_prefixes.is_empty());
    }

    #[test]
    fn kanban_full_section() {
        let yaml = r#"
vault_path: /tmp/test-vault
session_sources: []
kanban:
  enabled: true
  queries:
    backlog: "status:backlog"
    active: "status:active type:project"
  prefixes:
    "P-": project
    "T-": task
"#;
        let f = write_config(yaml).unwrap();
        let config = load(Some(f.path())).unwrap();
        assert!(config.kanban_enabled);
        assert_eq!(config.kanban_queries.get("backlog").unwrap(), "status:backlog");
        assert_eq!(config.kanban_queries.get("active").unwrap(), "status:active type:project");
        assert_eq!(config.kanban_prefixes.get("P-").unwrap(), "project");
        assert_eq!(config.kanban_prefixes.get("T-").unwrap(), "task");
    }
    #[test]
    fn trackers_absent_defaults_to_empty() {
        let yaml = "vault_path: /tmp/test-vault\nsession_sources: []\n";
        let f = write_config(yaml).unwrap();
        let config = load(Some(f.path())).unwrap();
        assert!(config.trackers.is_empty());
    }

    #[test]
    fn trackers_section_binds_projects() {
        let yaml = r#"
vault_path: /tmp/test-vault
session_sources: []
trackers:
  work/claims:
    provider: linear
    team: COR
    credential: corr-linear
    readonly: true
  work/other:
    provider: linear
    team: OTH
    credential: corr-linear
"#;
        let f = write_config(yaml).unwrap();
        let config = load(Some(f.path())).unwrap();
        let claims = config.tracker_for("work", "claims").unwrap();
        assert_eq!(claims.domain, "work");
        assert_eq!(claims.project, "claims");
        assert_eq!(claims.provider, "linear");
        assert_eq!(claims.team, "COR");
        assert_eq!(claims.credential, "corr-linear");
        assert!(claims.readonly);
        assert!(!config.tracker_for("work", "other").unwrap().readonly);
        assert_eq!(config.tracker_for("work", "claims").map(|b| b.team.as_str()), Some("COR"));
        assert!(config.tracker_for("work", "missing").is_none());
    }

    #[test]
    fn trackers_gate_is_optional_and_defaults_to_false() {
        let yaml = "vault_path: /tmp/v\nsession_sources: []\ntrackers:\n  work/claims:\n    provider: linear\n    team: COR\n    credential: c\n    gate: true\n  work/other:\n    provider: linear\n    team: OTH\n    credential: c\n";
        let f = write_config(yaml).unwrap();
        let config = load(Some(f.path())).unwrap();
        assert!(config.tracker_for("work", "claims").unwrap().gate);
        assert!(!config.tracker_for("work", "other").unwrap().gate);
    }

    #[test]
    fn a_scalar_tracker_entry_is_a_list_of_one() {
        let yaml = "vault_path: /tmp/v\nsession_sources: []\ntrackers:\n  work/claims:\n    provider: linear\n    team: COR\n    credential: c\n";
        let f = write_config(yaml).unwrap();
        let config = load(Some(f.path())).unwrap();
        assert_eq!(config.trackers.len(), 1);
        assert_eq!(config.bindings_for("work", "claims").len(), 1);
        assert_eq!(config.trackers[0].key(), "work/claims");
        assert_eq!(config.trackers[0].repository, None);
    }

    #[test]
    fn a_tracker_entry_may_list_one_binding_per_provider() {
        let yaml = r#"
vault_path: /tmp/v
session_sources: []
trackers:
  work/claims:
    - provider: linear
      team: COR
      credential: corr-linear
      readonly: true
      gate: true
    - provider: github
      repository: acme/claims-app
  work/other:
    provider: linear
    team: OTH
    credential: corr-linear
"#;
        let f = write_config(yaml).unwrap();
        let config = load(Some(f.path())).unwrap();
        assert_eq!(config.trackers.len(), 3);
        let claims = config.bindings_for("work", "claims");
        let providers: Vec<&str> = claims.iter().map(|b| b.provider.as_str()).collect();
        assert_eq!(providers, vec!["linear", "github"], "file order within a project");
        let github = claims[1];
        assert_eq!(github.repository.as_deref(), Some("acme/claims-app"));
        assert_eq!(github.credential, "github", "the credential defaults to the provider name");
        assert_eq!(github.scope(), "acme/claims-app");
        assert!(!github.readonly && !github.gate);
        let issues = config.tracker_for("work", "claims").unwrap();
        assert_eq!((issues.provider.as_str(), issues.team.as_str(), issues.scope()), ("linear", "COR", "COR"));
        assert!(issues.gate && issues.readonly);
        let keys: Vec<String> = config.trackers.iter().map(|b| b.key()).collect();
        assert_eq!(keys, vec!["work/claims", "work/claims", "work/other"]);
    }

    #[test]
    fn a_project_with_only_a_github_binding_has_no_issue_tracker() {
        let yaml = "vault_path: /tmp/v\nsession_sources: []\ntrackers:\n  work/claims:\n    provider: github\n    repository: acme/app\n    credential: gh-work\n";
        let f = write_config(yaml).unwrap();
        let config = load(Some(f.path())).unwrap();
        assert!(config.tracker_for("work", "claims").is_none(), "kanban, inject and the gate read issue trackers only");
        assert_eq!(config.bindings_for("work", "claims")[0].credential, "gh-work");
    }

    #[test]
    fn a_second_binding_for_one_provider_is_a_load_error_naming_project_and_provider() {
        let yaml = "vault_path: /tmp/v\nsession_sources: []\ntrackers:\n  work/claims:\n    - provider: github\n      repository: acme/a\n    - provider: linear\n      team: COR\n      credential: c\n    - provider: github\n      repository: acme/b\n";
        let f = write_config(yaml).unwrap();
        let error = load(Some(f.path())).err().expect("duplicate provider").to_string();
        assert!(error.contains("work/claims"), "{error}");
        assert!(error.contains("provider github appears twice"), "{error}");
    }

    #[test]
    fn a_github_binding_needs_an_owner_and_name_repository() {
        for repository in ["", "acme", "acme/", "/app", "acme/app/extra", "acme app/x"] {
            let yaml = format!("vault_path: /tmp/v\nsession_sources: []\ntrackers:\n  work/claims:\n    provider: github\n    repository: \"{repository}\"\n");
            let f = write_config(&yaml).unwrap();
            let error = load(Some(f.path())).err().expect("bad repository").to_string();
            assert!(error.contains("work/claims") && error.contains("<owner>/<name>"), "{repository}: {error}");
        }
        let missing = "vault_path: /tmp/v\nsession_sources: []\ntrackers:\n  work/claims:\n    provider: github\n";
        let f = write_config(missing).unwrap();
        assert!(load(Some(f.path())).err().expect("no repository").to_string().contains("needs `repository`"));
    }

    #[test]
    fn fields_of_the_other_provider_are_load_errors() {
        let cases = [
            ("provider: github\n    repository: acme/app\n    team: COR\n", "`team`"),
            ("provider: github\n    repository: acme/app\n    gate: true\n", "`gate`"),
            ("provider: github\n    repository: acme/app\n    readonly: true\n", "`readonly`"),
            ("provider: linear\n    team: COR\n    credential: c\n    repository: acme/app\n", "`repository`"),
            ("provider: linear\n    credential: c\n", "needs `team`"),
            ("provider: linear\n    team: COR\n", "needs `credential`"),
        ];
        for (entry, named) in cases {
            let yaml = format!("vault_path: /tmp/v\nsession_sources: []\ntrackers:\n  work/claims:\n    {entry}");
            let f = write_config(&yaml).unwrap();
            let error = load(Some(f.path())).err().expect(entry).to_string();
            assert!(error.contains("work/claims") && error.contains(named), "{entry}: {error}");
        }
    }

    #[test]
    fn a_tracker_entry_that_is_neither_a_mapping_nor_a_list_is_a_load_error() {
        let yaml = "vault_path: /tmp/v\nsession_sources: []\ntrackers:\n  work/claims: linear\n";
        let f = write_config(yaml).unwrap();
        let error = load(Some(f.path())).err().expect("scalar").to_string();
        assert!(error.contains("work/claims") && error.contains("a mapping or a list of mappings"), "{error}");
    }

    #[test]
    fn trackers_reject_malformed_project_key() {
        for key in ["claims", "work/claims/extra", "/claims", "work/"] {
            let yaml = format!("vault_path: /tmp/v\nsession_sources: []\ntrackers:\n  \"{key}\":\n    provider: linear\n    team: COR\n    credential: c\n");
            let f = write_config(&yaml).unwrap();
            let error = load(Some(f.path())).err().expect("malformed key");
            assert!(error.to_string().contains(key), "{error}");
        }
    }

    #[test]
    fn trackers_reject_an_unknown_provider_and_list_the_supported_ones() {
        let yaml = "vault_path: /tmp/v\nsession_sources: []\ntrackers:\n  work/claims:\n    provider: jira\n    team: COR\n    credential: c\n";
        let f = write_config(yaml).unwrap();
        let error = load(Some(f.path())).err().expect("unknown provider").to_string();
        assert!(error.contains("work/claims"), "{error}");
        assert!(error.contains("'jira'"), "{error}");
        assert!(error.contains("supported: linear, github"), "{error}");
    }

    #[test]
    fn trackers_reject_a_team_key_that_is_a_native_kanban_prefix() {
        let yaml = "vault_path: /tmp/v\nsession_sources: []\nkanban:\n  enabled: true\n  prefixes:\n    billing: cor\ntrackers:\n  work/claims:\n    provider: linear\n    team: COR\n    credential: c\n";
        let f = write_config(yaml).unwrap();
        let error = load(Some(f.path())).err().expect("prefix collision").to_string();
        assert!(error.contains("work/claims"), "{error}");
        assert!(error.contains("team key COR"), "{error}");
        assert!(error.contains("kanban prefix cor of project billing"), "{error}");

        let other = "vault_path: /tmp/v\nsession_sources: []\nkanban:\n  enabled: true\n  prefixes:\n    billing: BIL\ntrackers:\n  work/claims:\n    provider: linear\n    team: COR\n    credential: c\n";
        let f = write_config(other).unwrap();
        assert!(load(Some(f.path())).is_ok());
    }

    #[test]
    fn projects_absent_defaults_to_empty() {
        let f = write_config("vault_path: /tmp/v\nsession_sources: []\n").unwrap();
        assert!(load(Some(f.path())).unwrap().projects.is_empty());
    }

    #[test]
    fn projects_map_directories_to_a_vault_project_with_tilde_expansion() {
        let yaml = "vault_path: /tmp/v\nsession_sources: []\nprojects:\n  personal/corr-platform:\n    paths:\n      - ~/Code/Corr/corrtex\n      - /srv/corrtex/\n";
        let f = write_config(yaml).unwrap();
        let config = load(Some(f.path())).unwrap();
        let mapping = config.projects.get("personal/corr-platform").unwrap();
        assert_eq!(mapping.domain, "personal");
        assert_eq!(mapping.project, "corr-platform");
        let home = dirs::home_dir().unwrap();
        assert_eq!(mapping.paths, vec![home.join("Code/Corr/corrtex"), PathBuf::from("/srv/corrtex")]);
    }

    #[test]
    fn projects_reject_an_unknown_key_by_name() {
        let yaml = "vault_path: /tmp/v\nsession_sources: []\nprojects:\n  work/claims:\n    path: /srv/claims\n";
        let f = write_config(yaml).unwrap();
        let error = load(Some(f.path())).err().expect("unknown key").to_string();
        assert!(error.contains("work/claims"), "{error}");
        assert!(error.contains("unknown field `path`"), "{error}");
        assert!(error.contains("only `paths`"), "{error}");
    }

    #[test]
    fn projects_reject_a_malformed_key_empty_paths_and_relative_paths() {
        for (yaml, needle) in [
            ("projects:\n  claims:\n    paths: [/srv/claims]\n", "key must be <domain>/<project>"),
            ("projects:\n  work/claims:\n    paths: []\n", "at least one path"),
            ("projects:\n  work/claims:\n    paths: [code/claims]\n", "absolute"),
        ] {
            let f = write_config(&format!("vault_path: /tmp/v\nsession_sources: []\n{yaml}")).unwrap();
            let error = load(Some(f.path())).err().expect("rejected").to_string();
            assert!(error.contains(needle), "{error}");
        }
    }

    #[test]
    fn projects_reject_one_path_under_two_projects_naming_both() {
        let yaml = "vault_path: /tmp/v\nsession_sources: []\nprojects:\n  work/a:\n    paths: [/srv/code]\n  work/b:\n    paths: [/srv/code/]\n";
        let f = write_config(yaml).unwrap();
        let error = load(Some(f.path())).err().expect("shared path").to_string();
        assert!(error.contains("work/a") && error.contains("work/b") && error.contains("/srv/code"), "{error}");
    }

    #[test]
    fn projects_reject_a_duplicate_key_by_name() {
        let yaml = "vault_path: /tmp/v\nsession_sources: []\nprojects:\n  work/a:\n    paths: [/srv/one]\n  work/a:\n    paths: [/srv/two]\n";
        let f = write_config(yaml).unwrap();
        let error = load(Some(f.path())).err().expect("duplicate key").to_string();
        assert!(error.contains("work/a") && error.contains("twice"), "{error}");
    }

    #[test]
    fn projects_reject_dot_and_empty_segments() {
        for key in ["work/..", "../work", "./a", "work/.", "/a", "a/", "a//b"] {
            let yaml = format!("vault_path: /tmp/v\nsession_sources: []\nprojects:\n  \"{key}\":\n    paths: [/srv/x]\n");
            let f = write_config(&yaml).unwrap();
            let error = load(Some(f.path())).err().unwrap_or_else(|| panic!("{key} accepted")).to_string();
            assert!(error.contains("<domain>/<project>"), "{key}: {error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn projects_reject_a_symlink_to_a_folder_mapped_under_another_project() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("code");
        std::fs::create_dir_all(&real).unwrap();
        let link = tmp.path().join("alias");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let yaml = format!("vault_path: /tmp/v\nsession_sources: []\nprojects:\n  work/a:\n    paths: [\"{}\"]\n  work/b:\n    paths: [\"{}\"]\n", real.display(), link.display());
        let error = parse(&yaml).err().expect("same folder twice").to_string();
        assert!(error.contains("work/a") && error.contains("work/b"), "{error}");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        let fine = format!("vault_path: /tmp/v\nsession_sources: []\nprojects:\n  work/a:\n    paths: [\"{}\"]\n  work/b:\n    paths: [\"{}\"]\n", real.display(), other.display());
        assert!(parse(&fine).is_ok());
    }
}
