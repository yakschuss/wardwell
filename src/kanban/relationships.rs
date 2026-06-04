use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipType {
    Blocks,
    DependsOn,
    Feeds,
    ConsumesOutputFrom,
    Duplicates,
    Supersedes,
    Related,
}

impl RelationshipType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Blocks => "blocks",
            Self::DependsOn => "depends_on",
            Self::Feeds => "feeds",
            Self::ConsumesOutputFrom => "consumes_output_from",
            Self::Duplicates => "duplicates",
            Self::Supersedes => "supersedes",
            Self::Related => "related",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "blocks" => Some(Self::Blocks),
            "depends_on" => Some(Self::DependsOn),
            "feeds" => Some(Self::Feeds),
            "consumes_output_from" => Some(Self::ConsumesOutputFrom),
            "duplicates" => Some(Self::Duplicates),
            "supersedes" => Some(Self::Supersedes),
            "related" => Some(Self::Related),
            _ => None,
        }
    }

    pub fn all_names() -> &'static [&'static str] {
        &["blocks", "depends_on", "feeds", "consumes_output_from", "duplicates", "supersedes", "related"]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relationship {
    pub id: String,
    pub project: String,
    pub from_ticket_id: String,
    pub to_ticket_id: String,
    pub relationship_type: RelationshipType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum RelationshipEvent {
    #[serde(rename = "create_relationship")]
    Create(Relationship),
    #[serde(rename = "delete_relationship")]
    Delete {
        id: String,
        project: String,
        timestamp: String,
    },
}

pub fn jsonl_path(vault_root: &Path, domain: &str, project: &str) -> PathBuf {
    vault_root.join(domain).join(project).join("relationships.jsonl")
}

pub fn append_event(vault_root: &Path, domain: &str, project: &str, event: &RelationshipEvent) -> Result<(), std::io::Error> {
    let path = jsonl_path(vault_root, domain, project);
    let line = serde_json::to_string(event).map_err(std::io::Error::other)?;
    crate::kanban::jsonl::append_line(&path, Some(r#"{"_schema":"relationships","_version":"1.0"}"#), &line)
}

pub fn read_all(vault_root: &Path, domain: &str, project: &str) -> Vec<Relationship> {
    let path = jsonl_path(vault_root, domain, project);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let mut relationships: Vec<Relationship> = vec![];
    let mut deleted: std::collections::HashSet<String> = std::collections::HashSet::new();

    for line in content.lines() {
        if line.is_empty() || line.contains("\"_schema\"") { continue; }
        if let Ok(event) = serde_json::from_str::<RelationshipEvent>(line) {
            match event {
                RelationshipEvent::Create(rel) => { relationships.push(rel); }
                RelationshipEvent::Delete { id, .. } => { deleted.insert(id); }
            }
        }
    }

    relationships.retain(|r| !deleted.contains(&r.id));
    relationships
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_relationship_event() {
        let rel = Relationship {
            id: "rel-1".into(),
            project: "test".into(),
            from_ticket_id: "T-1".into(),
            to_ticket_id: "T-2".into(),
            relationship_type: RelationshipType::Blocks,
            description: Some("T-1 blocks T-2".into()),
            created_at: "2026-05-27T00:00:00Z".into(),
            source: Some("code".into()),
        };
        let event = RelationshipEvent::Create(rel.clone());
        let json = serde_json::to_string(&event).unwrap();
        let parsed: RelationshipEvent = serde_json::from_str(&json).unwrap();
        if let RelationshipEvent::Create(r) = parsed {
            assert_eq!(r.id, "rel-1");
            assert_eq!(r.relationship_type, RelationshipType::Blocks);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn append_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let rel = Relationship {
            id: "rel-1".into(),
            project: "proj".into(),
            from_ticket_id: "P-1".into(),
            to_ticket_id: "P-2".into(),
            relationship_type: RelationshipType::Feeds,
            description: None,
            created_at: "2026-05-27T00:00:00Z".into(),
            source: None,
        };
        append_event(dir.path(), "domain", "proj", &RelationshipEvent::Create(rel)).unwrap();
        let rels = read_all(dir.path(), "domain", "proj");
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].from_ticket_id, "P-1");
    }

    #[test]
    fn delete_removes_from_materialized() {
        let dir = tempfile::tempdir().unwrap();
        let rel = Relationship {
            id: "rel-1".into(),
            project: "proj".into(),
            from_ticket_id: "P-1".into(),
            to_ticket_id: "P-2".into(),
            relationship_type: RelationshipType::Related,
            description: None,
            created_at: "2026-05-27T00:00:00Z".into(),
            source: None,
        };
        append_event(dir.path(), "d", "proj", &RelationshipEvent::Create(rel)).unwrap();
        append_event(dir.path(), "d", "proj", &RelationshipEvent::Delete {
            id: "rel-1".into(),
            project: "proj".into(),
            timestamp: "2026-05-27T01:00:00Z".into(),
        }).unwrap();
        let rels = read_all(dir.path(), "d", "proj");
        assert_eq!(rels.len(), 0);
    }
}
