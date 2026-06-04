use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationSource {
    User,
    Code,
    Git,
    Meeting,
    Board,
    Agent,
}

impl VerificationSource {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "user" => Some(Self::User),
            "code" => Some(Self::Code),
            "git" => Some(Self::Git),
            "meeting" => Some(Self::Meeting),
            "board" => Some(Self::Board),
            "agent" => Some(Self::Agent),
            _ => None,
        }
    }

    pub fn all_names() -> &'static [&'static str] {
        &["user", "code", "git", "meeting", "board", "agent"]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Verified,
    Likely,
    Stale,
    Contradicted,
}

impl Confidence {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "verified" => Some(Self::Verified),
            "likely" => Some(Self::Likely),
            "stale" => Some(Self::Stale),
            "contradicted" => Some(Self::Contradicted),
            _ => None,
        }
    }

    pub fn all_names() -> &'static [&'static str] {
        &["verified", "likely", "stale", "contradicted"]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verification {
    pub id: String,
    pub ticket_id: String,
    pub project: String,
    pub verified_at: String,
    pub verification_source: VerificationSource,
    pub confidence: Confidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum VerificationEvent {
    #[serde(rename = "verify")]
    Verify(Verification),
}

pub fn jsonl_path(vault_root: &Path, domain: &str, project: &str) -> PathBuf {
    vault_root.join(domain).join(project).join("verifications.jsonl")
}

pub fn append_event(vault_root: &Path, domain: &str, project: &str, event: &VerificationEvent) -> Result<(), std::io::Error> {
    let path = vault_root.join(domain).join(project).join("verifications.jsonl");
    let line = serde_json::to_string(event).map_err(std::io::Error::other)?;
    crate::kanban::jsonl::append_line(&path, Some(r#"{"_schema":"verifications","_version":"1.0"}"#), &line)
}

pub fn read_all(vault_root: &Path, domain: &str, project: &str) -> Vec<Verification> {
    let path = jsonl_path(vault_root, domain, project);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    content.lines()
        .filter(|l| !l.is_empty() && !l.contains("\"_schema\""))
        .filter_map(|l| serde_json::from_str::<VerificationEvent>(l).ok())
        .map(|VerificationEvent::Verify(v)| v)
        .collect()
}

pub fn latest_for_ticket<'a>(verifications: &'a [Verification], ticket_id: &str) -> Option<&'a Verification> {
    verifications.iter()
        .filter(|v| v.ticket_id == ticket_id)
        .max_by(|a, b| a.verified_at.cmp(&b.verified_at))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_verification() {
        let v = Verification {
            id: "v-1".into(),
            ticket_id: "T-1".into(),
            project: "test".into(),
            verified_at: "2026-05-27T00:00:00Z".into(),
            verification_source: VerificationSource::User,
            confidence: Confidence::Verified,
            summary: Some("Confirmed in standup".into()),
            source: Some("code".into()),
        };
        let event = VerificationEvent::Verify(v);
        let json = serde_json::to_string(&event).unwrap();
        let parsed: VerificationEvent = serde_json::from_str(&json).unwrap();
        let VerificationEvent::Verify(parsed_v) = parsed;
        assert_eq!(parsed_v.ticket_id, "T-1");
        assert_eq!(parsed_v.confidence, Confidence::Verified);
    }

    #[test]
    fn append_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let v = Verification {
            id: "v-1".into(),
            ticket_id: "P-1".into(),
            project: "proj".into(),
            verified_at: "2026-05-27T00:00:00Z".into(),
            verification_source: VerificationSource::Meeting,
            confidence: Confidence::Likely,
            summary: None,
            source: None,
        };
        append_event(dir.path(), "d", "proj", &VerificationEvent::Verify(v)).unwrap();
        let vs = read_all(dir.path(), "d", "proj");
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0].verification_source, VerificationSource::Meeting);
    }

    #[test]
    fn latest_for_ticket_picks_most_recent() {
        let vs = vec![
            Verification {
                id: "v-1".into(), ticket_id: "T-1".into(), project: "p".into(),
                verified_at: "2026-05-26T00:00:00Z".into(),
                verification_source: VerificationSource::User, confidence: Confidence::Verified,
                summary: None, source: None,
            },
            Verification {
                id: "v-2".into(), ticket_id: "T-1".into(), project: "p".into(),
                verified_at: "2026-05-27T00:00:00Z".into(),
                verification_source: VerificationSource::Agent, confidence: Confidence::Stale,
                summary: None, source: None,
            },
            Verification {
                id: "v-3".into(), ticket_id: "T-2".into(), project: "p".into(),
                verified_at: "2026-05-28T00:00:00Z".into(),
                verification_source: VerificationSource::Code, confidence: Confidence::Verified,
                summary: None, source: None,
            },
        ];
        let latest = latest_for_ticket(&vs, "T-1");
        assert_eq!(latest.map(|v| v.id.as_str()), Some("v-2"));
    }
}
