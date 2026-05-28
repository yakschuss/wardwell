use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    Pending,
    Approved,
    Rejected,
    Applied,
    Cancelled,
}

impl ProposalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Applied => "applied",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum ChangeOperation {
    #[serde(rename = "update_ticket")]
    UpdateTicket {
        ticket_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        priority: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        epic: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tags: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        deadline: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    #[serde(rename = "append_note")]
    AppendNote {
        ticket_id: String,
        text: String,
    },
    #[serde(rename = "create_relationship")]
    CreateRelationship {
        from_ticket_id: String,
        to_ticket_id: String,
        relationship_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    #[serde(rename = "create_question")]
    CreateQuestion {
        question: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        ticket_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        current_assumption: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        evidence: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        needed_for: Option<String>,
    },
    #[serde(rename = "answer_question")]
    AnswerQuestion {
        question_id: String,
        answer: String,
    },
    #[serde(rename = "invalidate_question")]
    InvalidateQuestion {
        question_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub id: String,
    pub project: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub status: ProposalStatus,
    pub changes: Vec<ChangeOperation>,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ticket_snapshots: Vec<TicketSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketSnapshot {
    pub ticket_id: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum ProposalEvent {
    #[serde(rename = "create_proposal")]
    Create(Proposal),
    #[serde(rename = "approve_proposal")]
    Approve {
        id: String,
        project: String,
        timestamp: String,
    },
    #[serde(rename = "reject_proposal")]
    Reject {
        id: String,
        project: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        timestamp: String,
    },
    #[serde(rename = "apply_proposal")]
    Apply {
        id: String,
        project: String,
        timestamp: String,
    },
    #[serde(rename = "cancel_proposal")]
    Cancel {
        id: String,
        project: String,
        timestamp: String,
    },
}

pub fn jsonl_path(vault_root: &Path, domain: &str, project: &str) -> PathBuf {
    vault_root.join(domain).join(project).join("proposals.jsonl")
}

pub fn append_event(vault_root: &Path, domain: &str, project: &str, event: &ProposalEvent) -> Result<(), std::io::Error> {
    let dir = vault_root.join(domain).join(project);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("proposals.jsonl");
    let needs_schema = !path.exists() || path.metadata()?.len() == 0;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    if needs_schema {
        writeln!(file, r#"{{"_schema":"proposals","_version":"1.0"}}"#)?;
    }
    let line = serde_json::to_string(event).map_err(std::io::Error::other)?;
    writeln!(file, "{line}")?;
    Ok(())
}

pub fn read_all(vault_root: &Path, domain: &str, project: &str) -> Vec<Proposal> {
    let path = jsonl_path(vault_root, domain, project);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let mut proposals: std::collections::HashMap<String, Proposal> = std::collections::HashMap::new();

    for line in content.lines() {
        if line.is_empty() || line.contains("\"_schema\"") { continue; }
        if let Ok(event) = serde_json::from_str::<ProposalEvent>(line) {
            match event {
                ProposalEvent::Create(p) => { proposals.insert(p.id.clone(), p); }
                ProposalEvent::Approve { id, timestamp, .. } => {
                    if let Some(p) = proposals.get_mut(&id) {
                        p.status = ProposalStatus::Approved;
                        p.decided_at = Some(timestamp);
                    }
                }
                ProposalEvent::Reject { id, timestamp, .. } => {
                    if let Some(p) = proposals.get_mut(&id) {
                        p.status = ProposalStatus::Rejected;
                        p.decided_at = Some(timestamp);
                    }
                }
                ProposalEvent::Apply { id, timestamp, .. } => {
                    if let Some(p) = proposals.get_mut(&id) {
                        p.status = ProposalStatus::Applied;
                        p.applied_at = Some(timestamp);
                    }
                }
                ProposalEvent::Cancel { id, timestamp, .. } => {
                    if let Some(p) = proposals.get_mut(&id) {
                        p.status = ProposalStatus::Cancelled;
                        p.decided_at = Some(timestamp);
                    }
                }
            }
        }
    }

    proposals.into_values().collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_proposal() {
        let p = Proposal {
            id: "prop-1".into(),
            project: "test".into(),
            title: "Add epic to tickets".into(),
            description: None,
            status: ProposalStatus::Pending,
            changes: vec![
                ChangeOperation::UpdateTicket {
                    ticket_id: "T-1".into(),
                    status: None,
                    priority: None,
                    epic: Some("operational-loop-v1".into()),
                    tags: None,
                    parent: None,
                    deadline: None,
                    title: None,
                    description: None,
                },
                ChangeOperation::CreateRelationship {
                    from_ticket_id: "T-1".into(),
                    to_ticket_id: "T-2".into(),
                    relationship_type: "feeds".into(),
                    description: Some("PCC ingestion feeds TCM detection".into()),
                },
            ],
            created_at: "2026-05-27T00:00:00Z".into(),
            decided_at: None,
            applied_at: None,
            source: Some("code".into()),
            ticket_snapshots: vec![
                TicketSnapshot { ticket_id: "T-1".into(), updated_at: "2026-05-26T00:00:00Z".into() },
            ],
        };
        let json = serde_json::to_string(&p).unwrap();
        let parsed: Proposal = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.changes.len(), 2);
    }

    #[test]
    fn lifecycle_pending_to_applied() {
        let dir = tempfile::tempdir().unwrap();
        let p = Proposal {
            id: "prop-1".into(),
            project: "proj".into(),
            title: "Test proposal".into(),
            description: None,
            status: ProposalStatus::Pending,
            changes: vec![],
            created_at: "2026-05-27T00:00:00Z".into(),
            decided_at: None,
            applied_at: None,
            source: None,
            ticket_snapshots: vec![],
        };
        append_event(dir.path(), "d", "proj", &ProposalEvent::Create(p)).unwrap();
        append_event(dir.path(), "d", "proj", &ProposalEvent::Approve {
            id: "prop-1".into(), project: "proj".into(), timestamp: "2026-05-27T01:00:00Z".into(),
        }).unwrap();
        append_event(dir.path(), "d", "proj", &ProposalEvent::Apply {
            id: "prop-1".into(), project: "proj".into(), timestamp: "2026-05-27T02:00:00Z".into(),
        }).unwrap();

        let props = read_all(dir.path(), "d", "proj");
        assert_eq!(props.len(), 1);
        assert_eq!(props[0].status, ProposalStatus::Applied);
        assert!(props[0].applied_at.is_some());
    }

    #[test]
    fn rejected_proposal() {
        let dir = tempfile::tempdir().unwrap();
        let p = Proposal {
            id: "prop-2".into(),
            project: "proj".into(),
            title: "Bad proposal".into(),
            description: None,
            status: ProposalStatus::Pending,
            changes: vec![],
            created_at: "2026-05-27T00:00:00Z".into(),
            decided_at: None,
            applied_at: None,
            source: None,
            ticket_snapshots: vec![],
        };
        append_event(dir.path(), "d", "proj", &ProposalEvent::Create(p)).unwrap();
        append_event(dir.path(), "d", "proj", &ProposalEvent::Reject {
            id: "prop-2".into(), project: "proj".into(),
            reason: Some("Not needed".into()),
            timestamp: "2026-05-27T01:00:00Z".into(),
        }).unwrap();

        let props = read_all(dir.path(), "d", "proj");
        assert_eq!(props[0].status, ProposalStatus::Rejected);
    }
}
