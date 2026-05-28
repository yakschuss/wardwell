use crate::kanban::questions::{Question, QuestionStatus};
use crate::kanban::relationships::Relationship;
use crate::kanban::store::KanbanItem;
use crate::kanban::verification::Verification;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize)]
pub struct RealityCheck {
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epic: Option<String>,
    pub urgent_backlog: Vec<TicketSummary>,
    pub active_without_epic: Vec<TicketSummary>,
    pub epic_tickets_by_status: HashMap<String, Vec<TicketSummary>>,
    pub open_questions: Vec<Question>,
    pub blocked_or_dependent: Vec<DependencySignal>,
    pub tickets_with_no_deadline: Vec<TicketSummary>,
    pub done_with_open_children: Vec<TicketSummary>,
    pub stale_tickets: Vec<TicketSummary>,
    pub relationship_graph: Vec<Relationship>,
    pub signals: Vec<Signal>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stale_verifications: Vec<VerificationSignal>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TicketSummary {
    pub ticket_id: String,
    pub title: String,
    pub status: String,
    pub priority: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epic: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DependencySignal {
    pub ticket_id: String,
    pub title: String,
    pub blocked_by: Vec<String>,
    pub blocks: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Signal {
    #[serde(rename = "type")]
    pub signal_type: String,
    pub ticket_id: String,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerificationSignal {
    pub ticket_id: String,
    pub confidence: String,
    pub verified_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl TicketSummary {
    pub fn from_item(item: &KanbanItem) -> Self {
        Self {
            ticket_id: item.ticket_id.clone(),
            title: item.title.clone(),
            status: item.status.clone(),
            priority: item.priority.clone(),
            epic: item.epic.clone(),
            deadline: item.deadline.clone(),
            updated_at: Some(item.updated_at.clone()),
        }
    }
}

pub fn build_reality_check(
    project: &str,
    epic_filter: Option<&str>,
    items: &[KanbanItem],
    relationships: &[Relationship],
    questions: &[Question],
    verifications: &[Verification],
    include_done: bool,
    stale_after_days: u64,
) -> RealityCheck {
    let now = chrono::Utc::now();
    let stale_threshold = now - chrono::Duration::days(stale_after_days as i64);
    let stale_str = stale_threshold.to_rfc3339();

    let filtered: Vec<&KanbanItem> = items.iter()
        .filter(|i| i.project == project)
        .filter(|i| include_done || i.status != "done")
        .filter(|i| {
            if let Some(epic) = epic_filter {
                i.epic.as_deref() == Some(epic)
            } else {
                true
            }
        })
        .collect();

    let urgent_backlog: Vec<TicketSummary> = filtered.iter()
        .filter(|i| i.status == "backlog" && (i.priority == "urgent" || i.priority == "high"))
        .map(|i| TicketSummary::from_item(i))
        .collect();

    let active_without_epic: Vec<TicketSummary> = if epic_filter.is_none() {
        items.iter()
            .filter(|i| i.project == project)
            .filter(|i| i.status == "in_progress" || i.status == "todo")
            .filter(|i| i.epic.is_none())
            .map(TicketSummary::from_item)
            .collect()
    } else {
        vec![]
    };

    let mut epic_tickets_by_status: HashMap<String, Vec<TicketSummary>> = HashMap::new();
    for item in &filtered {
        epic_tickets_by_status
            .entry(item.status.clone())
            .or_default()
            .push(TicketSummary::from_item(item));
    }

    let open_questions: Vec<Question> = questions.iter()
        .filter(|q| q.project == project && q.status == QuestionStatus::Open)
        .filter(|q| {
            if let Some(epic) = epic_filter {
                if let Some(tid) = &q.ticket_id {
                    filtered.iter().any(|i| &i.ticket_id == tid)
                } else {
                    // Project-level questions always show for epic queries
                    let _ = epic;
                    true
                }
            } else {
                true
            }
        })
        .cloned()
        .collect();

    let ticket_ids: Vec<&str> = filtered.iter().map(|i| i.ticket_id.as_str()).collect();
    let relevant_rels: Vec<Relationship> = relationships.iter()
        .filter(|r| r.project == project)
        .filter(|r| ticket_ids.contains(&r.from_ticket_id.as_str()) || ticket_ids.contains(&r.to_ticket_id.as_str()))
        .cloned()
        .collect();

    let mut blocked_or_dependent: Vec<DependencySignal> = vec![];
    for item in &filtered {
        let blocked_by: Vec<String> = relevant_rels.iter()
            .filter(|r| {
                (r.to_ticket_id == item.ticket_id && matches!(r.relationship_type, crate::kanban::relationships::RelationshipType::Blocks))
                || (r.from_ticket_id == item.ticket_id && matches!(r.relationship_type, crate::kanban::relationships::RelationshipType::DependsOn))
            })
            .map(|r| if r.to_ticket_id == item.ticket_id { r.from_ticket_id.clone() } else { r.to_ticket_id.clone() })
            .collect();
        let blocks: Vec<String> = relevant_rels.iter()
            .filter(|r| r.from_ticket_id == item.ticket_id && matches!(r.relationship_type, crate::kanban::relationships::RelationshipType::Blocks))
            .map(|r| r.to_ticket_id.clone())
            .collect();
        if !blocked_by.is_empty() || !blocks.is_empty() {
            blocked_or_dependent.push(DependencySignal {
                ticket_id: item.ticket_id.clone(),
                title: item.title.clone(),
                blocked_by,
                blocks,
            });
        }
    }

    let tickets_with_no_deadline: Vec<TicketSummary> = filtered.iter()
        .filter(|i| i.deadline.is_none() && i.status != "done")
        .map(|i| TicketSummary::from_item(i))
        .collect();

    let all_project_items: Vec<&KanbanItem> = items.iter()
        .filter(|i| i.project == project)
        .collect();
    let done_with_open_children: Vec<TicketSummary> = all_project_items.iter()
        .filter(|i| i.status == "done")
        .filter(|i| {
            i.children.iter().any(|c| c.status != "done")
        })
        .map(|i| TicketSummary::from_item(i))
        .collect();

    let stale_tickets: Vec<TicketSummary> = filtered.iter()
        .filter(|i| i.status != "done" && i.updated_at < stale_str)
        .map(|i| TicketSummary::from_item(i))
        .collect();

    let mut signals: Vec<Signal> = vec![];
    for item in &urgent_backlog {
        signals.push(Signal {
            signal_type: "urgent_not_started".into(),
            ticket_id: item.ticket_id.clone(),
            summary: format!("Urgent/high ticket '{}' remains in backlog", item.title),
        });
    }
    for item in &stale_tickets {
        signals.push(Signal {
            signal_type: "stale".into(),
            ticket_id: item.ticket_id.clone(),
            summary: format!("Ticket '{}' not updated in {}+ days", item.title, stale_after_days),
        });
    }
    for item in &done_with_open_children {
        signals.push(Signal {
            signal_type: "done_with_open_children".into(),
            ticket_id: item.ticket_id.clone(),
            summary: format!("Ticket '{}' is done but has open subtasks", item.title),
        });
    }

    let mut stale_verifications: Vec<VerificationSignal> = vec![];
    for item in &filtered {
        if let Some(v) = crate::kanban::verification::latest_for_ticket(verifications, &item.ticket_id) {
            if matches!(v.confidence, crate::kanban::verification::Confidence::Stale | crate::kanban::verification::Confidence::Contradicted) {
                stale_verifications.push(VerificationSignal {
                    ticket_id: v.ticket_id.clone(),
                    confidence: format!("{:?}", v.confidence).to_lowercase(),
                    verified_at: v.verified_at.clone(),
                    summary: v.summary.clone(),
                });
                signals.push(Signal {
                    signal_type: format!("verification_{}", format!("{:?}", v.confidence).to_lowercase()),
                    ticket_id: v.ticket_id.clone(),
                    summary: format!("Ticket '{}' verification is {:?}", item.title, v.confidence),
                });
            }
        }
    }

    // Duplicate title detection
    let mut title_counts: HashMap<&str, Vec<&str>> = HashMap::new();
    for item in &filtered {
        title_counts.entry(&item.title).or_default().push(&item.ticket_id);
    }
    for (title, ids) in &title_counts {
        if ids.len() > 1 {
            signals.push(Signal {
                signal_type: "possible_duplicate_title".into(),
                ticket_id: ids[0].to_string(),
                summary: format!("Title '{}' appears on {} tickets: {}", title, ids.len(), ids.join(", ")),
            });
        }
    }

    RealityCheck {
        project: project.into(),
        epic: epic_filter.map(String::from),
        urgent_backlog,
        active_without_epic,
        epic_tickets_by_status,
        open_questions,
        blocked_or_dependent,
        tickets_with_no_deadline,
        done_with_open_children,
        stale_tickets,
        relationship_graph: relevant_rels,
        signals,
        stale_verifications,
    }
}
