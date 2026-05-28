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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<Summary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub top_signals: Vec<Signal>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub active_work: Vec<TicketSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub urgent_backlog: Vec<TicketSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub open_questions: Vec<QuestionSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blocked_or_dependent: Vec<DependencySignal>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub done_with_open_children: Vec<TicketSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stale_tickets: Vec<TicketSummary>,
    // Full-mode only sections
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tickets_by_status: Option<HashMap<String, Vec<TicketSummary>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_without_epic: Option<Vec<TicketSummary>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tickets_with_no_deadline: Option<Vec<TicketSummary>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relationship_graph: Option<Vec<Relationship>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_verifications: Option<Vec<VerificationSignal>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub total_tickets: usize,
    pub in_progress: usize,
    pub todo: usize,
    pub backlog: usize,
    pub urgent_backlog_count: usize,
    pub active_without_epic_count: usize,
    pub open_question_count: usize,
    pub relationship_count: usize,
    pub stale_ticket_count: usize,
    pub done_with_open_children_count: usize,
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
pub struct QuestionSummary {
    pub id: String,
    pub question: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_assumption: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needed_for: Option<String>,
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

impl QuestionSummary {
    pub fn from_question(q: &Question) -> Self {
        Self {
            id: q.id.clone(),
            question: q.question.clone(),
            ticket_id: q.ticket_id.clone(),
            current_assumption: q.current_assumption.clone(),
            needed_for: q.needed_for.clone(),
        }
    }
}

pub struct RealityCheckOptions {
    pub compact: bool,
    pub limit: usize,
    pub include_done: bool,
    pub stale_after_days: u64,
}

impl Default for RealityCheckOptions {
    fn default() -> Self {
        Self { compact: true, limit: 10, include_done: false, stale_after_days: 14 }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build_reality_check(
    project: &str,
    epic_filter: Option<&str>,
    items: &[KanbanItem],
    relationships: &[Relationship],
    questions: &[Question],
    verifications: &[Verification],
    opts: &RealityCheckOptions,
) -> RealityCheck {
    let now = chrono::Utc::now();
    let stale_threshold = now - chrono::Duration::days(opts.stale_after_days as i64);
    let stale_str = stale_threshold.to_rfc3339();

    let project_items: Vec<&KanbanItem> = items.iter()
        .filter(|i| i.project == project)
        .collect();

    let filtered: Vec<&KanbanItem> = project_items.iter()
        .filter(|i| opts.include_done || i.status != "done")
        .filter(|i| {
            if let Some(epic) = epic_filter { i.epic.as_deref() == Some(epic) } else { true }
        })
        .copied()
        .collect();

    // Urgent backlog
    let all_urgent_backlog: Vec<TicketSummary> = filtered.iter()
        .filter(|i| i.status == "backlog" && (i.priority == "urgent" || i.priority == "high"))
        .map(|i| TicketSummary::from_item(i))
        .collect();

    // Active work (in_progress + review)
    let all_active: Vec<TicketSummary> = filtered.iter()
        .filter(|i| i.status == "in_progress" || i.status == "review")
        .map(|i| TicketSummary::from_item(i))
        .collect();

    // Active without epic (only when no epic filter)
    let all_active_without_epic: Vec<TicketSummary> = if epic_filter.is_none() {
        project_items.iter()
            .filter(|i| (i.status == "in_progress" || i.status == "todo") && i.epic.is_none())
            .map(|i| TicketSummary::from_item(i))
            .collect()
    } else {
        vec![]
    };

    // Open questions
    let all_open_questions: Vec<QuestionSummary> = questions.iter()
        .filter(|q| q.project == project && q.status == QuestionStatus::Open)
        .filter(|q| {
            if epic_filter.is_some() {
                if let Some(tid) = &q.ticket_id {
                    filtered.iter().any(|i| &i.ticket_id == tid)
                } else { true }
            } else { true }
        })
        .map(QuestionSummary::from_question)
        .collect();

    // Relationships
    let ticket_ids: Vec<&str> = filtered.iter().map(|i| i.ticket_id.as_str()).collect();
    let relevant_rels: Vec<Relationship> = relationships.iter()
        .filter(|r| r.project == project)
        .filter(|r| ticket_ids.contains(&r.from_ticket_id.as_str()) || ticket_ids.contains(&r.to_ticket_id.as_str()))
        .cloned()
        .collect();

    // Blocked/dependent
    let all_blocked: Vec<DependencySignal> = build_dependency_signals(&filtered, &relevant_rels);

    // No deadline
    let all_no_deadline: Vec<TicketSummary> = filtered.iter()
        .filter(|i| i.deadline.is_none() && i.status != "done")
        .map(|i| TicketSummary::from_item(i))
        .collect();

    // Done with open children
    let all_done_open_children: Vec<TicketSummary> = project_items.iter()
        .filter(|i| i.status == "done" && i.children.iter().any(|c| c.status != "done"))
        .map(|i| TicketSummary::from_item(i))
        .collect();

    // Stale
    let all_stale: Vec<TicketSummary> = filtered.iter()
        .filter(|i| i.status != "done" && i.updated_at < stale_str)
        .map(|i| TicketSummary::from_item(i))
        .collect();

    // Signals
    let mut all_signals = build_signals(
        &all_urgent_backlog, &all_stale, &all_done_open_children,
        &filtered, verifications, opts.stale_after_days,
    );

    // Stale verifications
    let all_stale_verifications: Vec<VerificationSignal> = build_stale_verifications(&filtered, verifications);

    // Tickets by status (full mode only, or always computed for summary)
    let mut tickets_by_status: HashMap<String, Vec<TicketSummary>> = HashMap::new();
    for item in &filtered {
        tickets_by_status.entry(item.status.clone()).or_default().push(TicketSummary::from_item(item));
    }

    // Duplicate title detection
    let mut title_counts: HashMap<&str, Vec<&str>> = HashMap::new();
    for item in &filtered {
        title_counts.entry(&item.title).or_default().push(&item.ticket_id);
    }
    for (title, ids) in &title_counts {
        if ids.len() > 1 {
            all_signals.push(Signal {
                signal_type: "possible_duplicate_title".into(),
                ticket_id: ids[0].to_string(),
                summary: format!("Title '{}' appears on {} tickets: {}", title, ids.len(), ids.join(", ")),
            });
        }
    }

    // Summary counts
    let summary = Summary {
        total_tickets: filtered.len(),
        in_progress: filtered.iter().filter(|i| i.status == "in_progress").count(),
        todo: filtered.iter().filter(|i| i.status == "todo").count(),
        backlog: filtered.iter().filter(|i| i.status == "backlog").count(),
        urgent_backlog_count: all_urgent_backlog.len(),
        active_without_epic_count: all_active_without_epic.len(),
        open_question_count: all_open_questions.len(),
        relationship_count: relevant_rels.len(),
        stale_ticket_count: all_stale.len(),
        done_with_open_children_count: all_done_open_children.len(),
    };

    let limit = opts.limit;

    if opts.compact {
        RealityCheck {
            project: project.into(),
            epic: epic_filter.map(String::from),
            summary: Some(summary),
            top_signals: truncate(all_signals, limit),
            active_work: truncate(all_active, limit),
            urgent_backlog: truncate(all_urgent_backlog, limit),
            open_questions: truncate(all_open_questions, limit),
            blocked_or_dependent: truncate(all_blocked, limit),
            done_with_open_children: truncate(all_done_open_children, limit),
            stale_tickets: truncate(all_stale, limit),
            tickets_by_status: None,
            active_without_epic: None,
            tickets_with_no_deadline: None,
            relationship_graph: None,
            stale_verifications: None,
        }
    } else {
        RealityCheck {
            project: project.into(),
            epic: epic_filter.map(String::from),
            summary: Some(summary),
            top_signals: all_signals,
            active_work: all_active,
            urgent_backlog: all_urgent_backlog,
            open_questions: all_open_questions,
            blocked_or_dependent: all_blocked,
            done_with_open_children: all_done_open_children,
            stale_tickets: all_stale,
            tickets_by_status: Some(tickets_by_status),
            active_without_epic: if epic_filter.is_none() { Some(all_active_without_epic) } else { None },
            tickets_with_no_deadline: Some(all_no_deadline),
            relationship_graph: Some(relevant_rels),
            stale_verifications: if all_stale_verifications.is_empty() { None } else { Some(all_stale_verifications) },
        }
    }
}

fn truncate<T>(mut v: Vec<T>, limit: usize) -> Vec<T> {
    v.truncate(limit);
    v
}

fn build_dependency_signals(filtered: &[&KanbanItem], rels: &[Relationship]) -> Vec<DependencySignal> {
    let mut result = vec![];
    for item in filtered {
        let blocked_by: Vec<String> = rels.iter()
            .filter(|r| {
                (r.to_ticket_id == item.ticket_id && matches!(r.relationship_type, crate::kanban::relationships::RelationshipType::Blocks))
                || (r.from_ticket_id == item.ticket_id && matches!(r.relationship_type, crate::kanban::relationships::RelationshipType::DependsOn))
            })
            .map(|r| if r.to_ticket_id == item.ticket_id { r.from_ticket_id.clone() } else { r.to_ticket_id.clone() })
            .collect();
        let blocks: Vec<String> = rels.iter()
            .filter(|r| r.from_ticket_id == item.ticket_id && matches!(r.relationship_type, crate::kanban::relationships::RelationshipType::Blocks))
            .map(|r| r.to_ticket_id.clone())
            .collect();
        if !blocked_by.is_empty() || !blocks.is_empty() {
            result.push(DependencySignal {
                ticket_id: item.ticket_id.clone(),
                title: item.title.clone(),
                blocked_by,
                blocks,
            });
        }
    }
    result
}

fn build_signals(
    urgent_backlog: &[TicketSummary],
    stale: &[TicketSummary],
    done_open_children: &[TicketSummary],
    filtered: &[&KanbanItem],
    verifications: &[Verification],
    stale_after_days: u64,
) -> Vec<Signal> {
    let mut signals = vec![];
    for item in urgent_backlog {
        signals.push(Signal {
            signal_type: "urgent_not_started".into(),
            ticket_id: item.ticket_id.clone(),
            summary: format!("Urgent/high ticket '{}' remains in backlog", item.title),
        });
    }
    for item in stale {
        signals.push(Signal {
            signal_type: "stale".into(),
            ticket_id: item.ticket_id.clone(),
            summary: format!("Ticket '{}' not updated in {}+ days", item.title, stale_after_days),
        });
    }
    for item in done_open_children {
        signals.push(Signal {
            signal_type: "done_with_open_children".into(),
            ticket_id: item.ticket_id.clone(),
            summary: format!("Ticket '{}' is done but has open subtasks", item.title),
        });
    }
    for item in filtered {
        if let Some(v) = crate::kanban::verification::latest_for_ticket(verifications, &item.ticket_id) {
            if matches!(v.confidence, crate::kanban::verification::Confidence::Stale | crate::kanban::verification::Confidence::Contradicted) {
                signals.push(Signal {
                    signal_type: format!("verification_{}", format!("{:?}", v.confidence).to_lowercase()),
                    ticket_id: v.ticket_id.clone(),
                    summary: format!("Ticket '{}' verification is {:?}", item.title, v.confidence),
                });
            }
        }
    }
    signals
}

fn build_stale_verifications(filtered: &[&KanbanItem], verifications: &[Verification]) -> Vec<VerificationSignal> {
    let mut result = vec![];
    for item in filtered {
        if let Some(v) = crate::kanban::verification::latest_for_ticket(verifications, &item.ticket_id) {
            if matches!(v.confidence, crate::kanban::verification::Confidence::Stale | crate::kanban::verification::Confidence::Contradicted) {
                result.push(VerificationSignal {
                    ticket_id: v.ticket_id.clone(),
                    confidence: format!("{:?}", v.confidence).to_lowercase(),
                    verified_at: v.verified_at.clone(),
                    summary: v.summary.clone(),
                });
            }
        }
    }
    result
}

// ---- Hygiene Suggestions ----

#[derive(Debug, Clone, Serialize)]
pub struct HygieneSuggestion {
    #[serde(rename = "type")]
    pub suggestion_type: String,
    pub ticket_id: String,
    pub excerpt: String,
    pub suggested_action: String,
}

pub fn build_hygiene_suggestions(
    items: &[KanbanItem],
    relationships: &[Relationship],
    epic_filter: Option<&str>,
    limit: usize,
) -> Vec<HygieneSuggestion> {
    let mut suggestions = vec![];

    let question_patterns = [
        "assumption to confirm",
        "assumption:",
        "open question:",
        "question:",
        "unclear whether",
        "need to confirm",
        "tbd:",
        "to be determined",
    ];

    let relationship_patterns = [
        ("feeds", "feeds"),
        ("blocked by", "blocks"),
        ("depends on", "depends_on"),
        ("blocks", "blocks"),
        ("supersedes", "supersedes"),
        ("replaces", "supersedes"),
        ("prerequisite", "depends_on"),
    ];

    let has_ticket_ref = |text: &str| -> bool {
        let bytes = text.as_bytes();
        for i in 0..bytes.len().saturating_sub(3) {
            if bytes[i].is_ascii_uppercase() && bytes.get(i + 1).map_or(false, |b| b.is_ascii_uppercase()) {
                if let Some(dash_pos) = bytes[i..].iter().position(|&b| b == b'-') {
                    let after_dash = i + dash_pos + 1;
                    if after_dash < bytes.len() && bytes[after_dash].is_ascii_digit() {
                        return true;
                    }
                }
            }
        }
        false
    };

    for item in items {
        if let Some(epic) = epic_filter {
            if item.epic.as_deref() != Some(epic) { continue; }
        }
        if item.status == "done" { continue; }

        let searchable = build_searchable_text(item);
        let lower = searchable.to_lowercase();

        for pattern in &question_patterns {
            if lower.contains(pattern) {
                let excerpt = extract_excerpt(&searchable, pattern, 120);
                suggestions.push(HygieneSuggestion {
                    suggestion_type: "question_candidate".into(),
                    ticket_id: item.ticket_id.clone(),
                    excerpt,
                    suggested_action: "question_create".into(),
                });
                break;
            }
        }

        for (pattern, rel_type) in &relationship_patterns {
            if lower.contains(pattern) {
                if has_ticket_ref(&searchable) {
                    let excerpt = extract_excerpt(&searchable, pattern, 120);
                    suggestions.push(HygieneSuggestion {
                        suggestion_type: "relationship_candidate".into(),
                        ticket_id: item.ticket_id.clone(),
                        excerpt,
                        suggested_action: format!("relationship_create (type: {})", rel_type),
                    });
                    break;
                }
            }
        }

        if item.epic.is_some() {
            let has_rels = relationships.iter().any(|r| r.from_ticket_id == item.ticket_id || r.to_ticket_id == item.ticket_id);
            if !has_rels && (item.status == "in_progress" || item.status == "todo") {
                suggestions.push(HygieneSuggestion {
                    suggestion_type: "relationship_gap".into(),
                    ticket_id: item.ticket_id.clone(),
                    excerpt: format!("Active ticket in epic '{}' has no relationships — consider adding dependencies or feeds", item.epic.as_deref().unwrap_or("?")),
                    suggested_action: "relationship_create".into(),
                });
            }
        }

        if suggestions.len() >= limit { break; }
    }

    suggestions.truncate(limit);
    suggestions
}

fn build_searchable_text(item: &KanbanItem) -> String {
    let mut text = String::new();
    if let Some(ref desc) = item.description {
        text.push_str(desc);
    }
    for note in &item.notes {
        text.push('\n');
        text.push_str(&note.text);
    }
    text
}

fn extract_excerpt(text: &str, pattern: &str, max_len: usize) -> String {
    let lower = text.to_lowercase();
    if let Some(pos) = lower.find(pattern) {
        let start = pos.saturating_sub(20);
        let start = text.ceil_char_boundary(start);
        let end = (pos + pattern.len() + 80).min(text.len());
        let end = text.floor_char_boundary(end);
        let excerpt = &text[start..end];
        if excerpt.len() > max_len {
            let boundary = text.floor_char_boundary(start + max_len);
            format!("{}…", &text[start..boundary])
        } else {
            excerpt.to_string()
        }
    } else {
        let boundary = text.floor_char_boundary(max_len.min(text.len()));
        text[..boundary].to_string()
    }
}
