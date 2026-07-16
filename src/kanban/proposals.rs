use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalIntent {
    ContextCapture,
    ContextMigration,
    Closure,
    PriorityChange,
    Split,
    Merge,
    Supersede,
    DecisionRecord,
}

impl ProposalIntent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ContextCapture => "context_capture",
            Self::ContextMigration => "context_migration",
            Self::Closure => "closure",
            Self::PriorityChange => "priority_change",
            Self::Split => "split",
            Self::Merge => "merge",
            Self::Supersede => "supersede",
            Self::DecisionRecord => "decision_record",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "context_capture" => Some(Self::ContextCapture),
            "context_migration" => Some(Self::ContextMigration),
            "closure" => Some(Self::Closure),
            "priority_change" => Some(Self::PriorityChange),
            "split" => Some(Self::Split),
            "merge" => Some(Self::Merge),
            "supersede" => Some(Self::Supersede),
            "decision_record" => Some(Self::DecisionRecord),
            _ => None,
        }
    }

    pub fn all_names() -> &'static [&'static str] {
        &["context_capture", "context_migration", "closure", "priority_change", "split", "merge", "supersede", "decision_record"]
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
        #[serde(skip_serializing_if = "Option::is_none")]
        interaction_type: Option<crate::kanban::questions::QuestionInteractionType>,
        #[serde(skip_serializing_if = "Option::is_none")]
        interaction_options: Option<Vec<crate::kanban::questions::QuestionOption>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        interaction_placeholder: Option<String>,
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
pub struct RiskFlag {
    pub code: String,
    pub severity: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextTransfer {
    pub from_ticket_id: String,
    pub to_ticket_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClosureSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shipped_scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_shipped: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_destination: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<ProposalIntent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub risk_flags: Vec<RiskFlag>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_transfers: Vec<ContextTransfer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closure_summary: Option<ClosureSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewer_questions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketSnapshot {
    pub ticket_id: String,
    pub updated_at: String,
}

// ---- Review / risk analysis ----

#[derive(Debug, Clone, Serialize)]
pub struct ProposalReview {
    pub summary: ProposalSummary,
    pub risk_flags: Vec<RiskFlag>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProposalSummary {
    pub decision_requested: String,
    /// Notes, relationships, and decisions added to tickets (context kept in place).
    pub context_preserved: Vec<String>,
    /// Explicit source → destination custody moves to a successor ticket.
    pub context_transfers: Vec<ContextTransfer>,
    pub state_changes: Vec<StateChange>,
    /// Questions created by this proposal or still open and blocking a closure.
    pub unresolved_questions: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StateChange {
    pub ticket_id: String,
    pub field: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

/// Scannable, review-critical metadata for one proposal — no raw operations.
#[derive(Debug, Clone, Serialize)]
pub struct ProposalListEntry {
    pub id: String,
    pub title: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<String>,
    pub affected_ticket_ids: Vec<String>,
    pub state_change_count: usize,
    pub context_preserved_count: usize,
    pub context_transfer_count: usize,
    pub risk_flag_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk_summary: Option<String>,
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
    let mut proposals: HashMap<String, Proposal> = HashMap::new();

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

// ---- Review computation ----

pub fn summarize_proposal(proposal: &Proposal) -> ProposalSummary {
    let mut context_preserved = Vec::new();
    let mut state_changes = Vec::new();
    let mut unresolved_questions = proposal.reviewer_questions.clone();
    let mut closing_ids = Vec::new();
    let mut priority_ids = Vec::new();

    for change in &proposal.changes {
        match change {
            ChangeOperation::UpdateTicket { ticket_id, status, priority, epic, tags, parent, deadline, title, description } => {
                if let Some(s) = status {
                    state_changes.push(StateChange { ticket_id: ticket_id.clone(), field: "status".into(), from: None, to: Some(s.clone()) });
                    if s == "done" { closing_ids.push(ticket_id.as_str()); }
                }
                if let Some(p) = priority {
                    state_changes.push(StateChange { ticket_id: ticket_id.clone(), field: "priority".into(), from: None, to: Some(p.clone()) });
                    priority_ids.push(ticket_id.as_str());
                }
                if let Some(e) = epic {
                    state_changes.push(StateChange { ticket_id: ticket_id.clone(), field: "epic".into(), from: None, to: Some(e.clone()) });
                }
                if tags.is_some() {
                    state_changes.push(StateChange { ticket_id: ticket_id.clone(), field: "tags".into(), from: None, to: Some("[updated]".into()) });
                }
                if let Some(p) = parent {
                    state_changes.push(StateChange { ticket_id: ticket_id.clone(), field: "parent".into(), from: None, to: Some(p.clone()) });
                }
                if let Some(d) = deadline {
                    state_changes.push(StateChange { ticket_id: ticket_id.clone(), field: "deadline".into(), from: None, to: Some(d.clone()) });
                }
                if let Some(t) = title {
                    state_changes.push(StateChange { ticket_id: ticket_id.clone(), field: "title".into(), from: None, to: Some(truncate_str(t, 60)) });
                }
                if description.is_some() {
                    state_changes.push(StateChange { ticket_id: ticket_id.clone(), field: "description".into(), from: None, to: Some("[updated]".into()) });
                }
            }
            // A note stays on the same ticket — context preserved in place, NOT migrated.
            ChangeOperation::AppendNote { ticket_id, text } => {
                context_preserved.push(format!("Note on {}: {}", ticket_id, truncate_str(text, 60)));
            }
            ChangeOperation::CreateRelationship { from_ticket_id, to_ticket_id, relationship_type, .. } => {
                context_preserved.push(format!("Link {} → {} ({})", from_ticket_id, to_ticket_id, relationship_type));
            }
            // A newly created question is an open thread, not preserved context.
            ChangeOperation::CreateQuestion { question, ticket_id, .. } => {
                let scope = ticket_id.as_deref().unwrap_or("project");
                unresolved_questions.push(format!("New question on {}: {}", scope, truncate_str(question, 60)));
            }
            ChangeOperation::AnswerQuestion { question_id, .. } => {
                context_preserved.push(format!("Answer recorded for question {}", question_id));
            }
            ChangeOperation::InvalidateQuestion { question_id, .. } => {
                context_preserved.push(format!("Question {} marked obsolete", question_id));
            }
        }
    }

    // closure_summary describes what shipped — knowledge kept, so it counts as preserved.
    if let Some(ref cs) = proposal.closure_summary {
        if let Some(ref scope) = cs.shipped_scope {
            context_preserved.push(format!("Shipped: {}", truncate_str(scope, 80)));
        }
        if let Some(ref ns) = cs.not_shipped {
            context_preserved.push(format!("Not shipped: {}", truncate_str(ns, 80)));
        }
    }

    let decision_requested = build_decision_summary(proposal, &closing_ids, &priority_ids);

    ProposalSummary {
        decision_requested,
        context_preserved,
        context_transfers: proposal.context_transfers.clone(),
        state_changes,
        unresolved_questions,
    }
}

pub fn review_proposal(
    proposal: &Proposal,
    items: &[crate::kanban::store::KanbanItem],
    questions: &[crate::kanban::questions::Question],
    relationships: &[crate::kanban::relationships::Relationship],
) -> ProposalReview {
    let mut summary = summarize_proposal(proposal);
    let risk_flags = compute_risk_flags(proposal, items, questions, relationships, &mut summary);
    ProposalReview { summary, risk_flags }
}

/// Unique ticket IDs touched by a proposal's operations, sorted for stable output.
pub fn affected_ticket_ids(proposal: &Proposal) -> Vec<String> {
    let mut set: HashSet<String> = HashSet::new();
    for change in &proposal.changes {
        match change {
            ChangeOperation::UpdateTicket { ticket_id, .. } | ChangeOperation::AppendNote { ticket_id, .. } => {
                set.insert(ticket_id.clone());
            }
            ChangeOperation::CreateRelationship { from_ticket_id, to_ticket_id, .. } => {
                set.insert(from_ticket_id.clone());
                set.insert(to_ticket_id.clone());
            }
            ChangeOperation::CreateQuestion { ticket_id, .. } => {
                if let Some(tid) = ticket_id { set.insert(tid.clone()); }
            }
            ChangeOperation::AnswerQuestion { .. } | ChangeOperation::InvalidateQuestion { .. } => {}
        }
    }
    let mut ids: Vec<String> = set.into_iter().collect();
    ids.sort();
    ids
}

/// Build a scannable list entry — review-critical metadata, no raw operations.
/// Risk flags are recomputed fresh against current board state so a list reflects
/// the board as it stands now, not as it was when the proposal was filed.
pub fn list_entry(
    proposal: &Proposal,
    items: &[crate::kanban::store::KanbanItem],
    questions: &[crate::kanban::questions::Question],
    relationships: &[crate::kanban::relationships::Relationship],
) -> ProposalListEntry {
    let review = review_proposal(proposal, items, questions, relationships);
    ProposalListEntry {
        id: proposal.id.clone(),
        title: proposal.title.clone(),
        status: proposal.status.as_str().to_string(),
        intent: proposal.intent.map(|i| i.as_str().to_string()),
        created_at: proposal.created_at.clone(),
        decided_at: proposal.decided_at.clone(),
        affected_ticket_ids: affected_ticket_ids(proposal),
        state_change_count: review.summary.state_changes.len(),
        context_preserved_count: review.summary.context_preserved.len(),
        context_transfer_count: review.summary.context_transfers.len(),
        risk_flag_count: review.risk_flags.len(),
        risk_summary: risk_summary_line(&review.risk_flags),
    }
}

/// A one-line, human-scannable summary of a proposal's risk flags.
pub fn risk_summary_line(flags: &[RiskFlag]) -> Option<String> {
    if flags.is_empty() {
        return None;
    }
    let joined = flags.iter()
        .map(|f| f.message.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let prefix = format!("⚠ {} risk(s): ", flags.len());
    Some(format!("{}{}", prefix, truncate_str(&joined, 240)))
}

fn compute_risk_flags(
    proposal: &Proposal,
    items: &[crate::kanban::store::KanbanItem],
    questions: &[crate::kanban::questions::Question],
    relationships: &[crate::kanban::relationships::Relationship],
    summary: &mut ProposalSummary,
) -> Vec<RiskFlag> {
    let mut flags = Vec::new();

    let item_map: HashMap<&str, &crate::kanban::store::KanbanItem> = items.iter()
        .map(|i| (i.ticket_id.as_str(), i))
        .collect();

    let mut closing_tickets: Vec<String> = Vec::new();
    let mut priority_changes: Vec<String> = Vec::new();
    let mut unique_tickets: HashSet<String> = HashSet::new();
    // Tickets that declare an *outgoing* successor link in *this* proposal —
    // i.e. they point at where their context continues. Incoming links don't count.
    let mut outgoing_link_in_proposal: HashSet<String> = HashSet::new();

    for change in &proposal.changes {
        match change {
            ChangeOperation::UpdateTicket { ticket_id, status, priority, .. } => {
                unique_tickets.insert(ticket_id.clone());
                if status.as_deref() == Some("done") {
                    closing_tickets.push(ticket_id.clone());
                }
                if let Some(new_pri) = priority {
                    if let Some(item) = item_map.get(ticket_id.as_str()) {
                        if item.priority != *new_pri {
                            priority_changes.push(ticket_id.clone());
                            // Enrich state change with "from" value
                            for sc in summary.state_changes.iter_mut() {
                                if sc.ticket_id == *ticket_id && sc.field == "priority" {
                                    sc.from = Some(item.priority.clone());
                                }
                            }
                        }
                    } else {
                        // No board snapshot to compare against — still a priority change.
                        priority_changes.push(ticket_id.clone());
                    }
                }
                if let Some(new_status) = status {
                    if let Some(item) = item_map.get(ticket_id.as_str()) {
                        for sc in summary.state_changes.iter_mut() {
                            if sc.ticket_id == *ticket_id && sc.field == "status" && sc.to.as_deref() == Some(new_status.as_str()) {
                                sc.from = Some(item.status.clone());
                            }
                        }
                    }
                }
            }
            ChangeOperation::AppendNote { ticket_id, .. } => {
                unique_tickets.insert(ticket_id.clone());
            }
            ChangeOperation::CreateRelationship { from_ticket_id, to_ticket_id, .. } => {
                unique_tickets.insert(from_ticket_id.clone());
                unique_tickets.insert(to_ticket_id.clone());
                outgoing_link_in_proposal.insert(from_ticket_id.clone());
            }
            ChangeOperation::CreateQuestion { ticket_id, .. } => {
                if let Some(tid) = ticket_id { unique_tickets.insert(tid.clone()); }
            }
            _ => {}
        }
    }

    let has_rationale = proposal.rationale.as_deref().is_some_and(|r| !r.trim().is_empty());

    // Closure-safety rule (the headline 0.10.2 invariant):
    // Any ticket moved to done MUST declare where its context lives via *structured*
    // closure metadata, or it gets a specific orphaned-context flag. Notes, a
    // free-text rationale, an obsolete-intent label, or an incoming link are NOT
    // custody declarations — a note on the ticket preserves context in place, it
    // does not say where the remaining work is tracked.
    //
    // Structured closure metadata, any one of:
    //   1. a closure_summary block (with at least one field set),
    //   2. a context_transfer whose source is this ticket,
    //   3. an outgoing successor link declared in this proposal (this → elsewhere).
    for tid in &closing_tickets {
        let has_closure_summary = proposal.closure_summary.as_ref().is_some_and(|cs| {
            cs.shipped_scope.is_some() || cs.context_destination.is_some() || cs.not_shipped.is_some()
        });
        let has_transfer = proposal.context_transfers.iter().any(|ct| ct.from_ticket_id == *tid);
        let has_outgoing_link = outgoing_link_in_proposal.contains(tid);
        if !has_closure_summary && !has_transfer && !has_outgoing_link {
            flags.push(RiskFlag {
                code: "closure_without_context".into(),
                severity: "high".into(),
                message: format!(
                    "Moves {tid} to done without a closure summary or declared context destination — record what shipped, what didn't, and where remaining context lives (a note on {tid} preserves context in place but does not transfer custody)."
                ),
                ticket_id: Some(tid.clone()),
            });
        }
    }

    // Risk: closing a context-wrapper ticket while its children are still open.
    for tid in &closing_tickets {
        if let Some(item) = item_map.get(tid.as_str()) {
            let open_children: Vec<&str> = item.children.iter()
                .filter(|c| c.status != "done")
                .map(|c| c.ticket_id.as_str())
                .collect();
            if !open_children.is_empty() {
                flags.push(RiskFlag {
                    code: "parent_closure_open_children".into(),
                    severity: "warning".into(),
                    message: format!(
                        "Closes {tid} while {} child ticket(s) are still open ({}); their context would be orphaned.",
                        open_children.len(), open_children.join(", ")
                    ),
                    ticket_id: Some(tid.clone()),
                });
            }
        }
    }

    // Risk: closing a ticket that still has unresolved questions.
    for tid in &closing_tickets {
        let open: Vec<&crate::kanban::questions::Question> = questions.iter()
            .filter(|q| q.ticket_id.as_deref() == Some(tid.as_str()) && q.status == crate::kanban::questions::QuestionStatus::Open)
            .collect();
        if !open.is_empty() {
            flags.push(RiskFlag {
                code: "closure_unresolved_questions".into(),
                severity: "warning".into(),
                message: format!(
                    "Closes {tid} with {} unresolved question(s) still open; resolve or migrate them before closing.",
                    open.len()
                ),
                ticket_id: Some(tid.clone()),
            });
            for q in open {
                summary.unresolved_questions.push(format!("[{}] {}", tid, q.question));
            }
        }
    }

    // Risk: reprioritizing without explaining the milestone/sequencing impact.
    if !priority_changes.is_empty() && !has_rationale {
        flags.push(RiskFlag {
            code: "priority_change_no_rationale".into(),
            severity: "warning".into(),
            message: format!(
                "Changes priority on {} without explaining milestone or sequencing impact.",
                priority_changes.join(", ")
            ),
            ticket_id: priority_changes.first().cloned(),
        });
    }

    // Risk: a single proposal that both closes work and reshuffles priorities on
    // tickets that aren't connected — two different decisions bundled together.
    if !closing_tickets.is_empty() && !priority_changes.is_empty() {
        let closing_set: HashSet<&str> = closing_tickets.iter().map(String::as_str).collect();
        let priority_set: HashSet<&str> = priority_changes.iter().map(String::as_str).collect();
        let overlap = closing_set.iter().any(|t| priority_set.contains(t));
        if !overlap && !sets_connected(&closing_set, &priority_set, relationships, &item_map) {
            flags.push(RiskFlag {
                code: "mixed_intent_batch".into(),
                severity: "warning".into(),
                message: "Combines closure and priority changes across unrelated tickets; consider splitting by intent.".into(),
                ticket_id: None,
            });
        }
    }

    // Risk: many unrelated tickets in one proposal.
    if unique_tickets.len() > 3 {
        let connected = relationships.iter().any(|r| {
            unique_tickets.contains(&r.from_ticket_id) && unique_tickets.contains(&r.to_ticket_id)
        }) || unique_tickets.iter().any(|tid| {
            item_map.get(tid.as_str())
                .and_then(|item| item.parent.as_ref())
                .is_some_and(|p| unique_tickets.contains(p))
        });
        if !connected {
            let mut ids: Vec<&str> = unique_tickets.iter().map(String::as_str).collect();
            ids.sort_unstable();
            flags.push(RiskFlag {
                code: "unrelated_batch".into(),
                severity: "warning".into(),
                message: format!(
                    "Touches {} unrelated tickets ({}) in one proposal; consider splitting by intent.",
                    unique_tickets.len(), ids.join(", ")
                ),
                ticket_id: None,
            });
        }
    }

    flags
}

/// True if any ticket in `a` is linked to any ticket in `b` via a relationship
/// (either direction) or a parent/child edge.
fn sets_connected(
    a: &HashSet<&str>,
    b: &HashSet<&str>,
    relationships: &[crate::kanban::relationships::Relationship],
    item_map: &HashMap<&str, &crate::kanban::store::KanbanItem>,
) -> bool {
    let rel_link = relationships.iter().any(|r| {
        (a.contains(r.from_ticket_id.as_str()) && b.contains(r.to_ticket_id.as_str()))
            || (b.contains(r.from_ticket_id.as_str()) && a.contains(r.to_ticket_id.as_str()))
    });
    if rel_link {
        return true;
    }
    // parent/child across the two sets
    let parent_link = |x: &HashSet<&str>, y: &HashSet<&str>| {
        x.iter().any(|tid| {
            item_map.get(tid)
                .and_then(|item| item.parent.as_deref())
                .is_some_and(|p| y.contains(p))
        })
    };
    parent_link(a, b) || parent_link(b, a)
}

fn build_decision_summary(proposal: &Proposal, closing: &[&str], priority: &[&str]) -> String {
    let mut parts = Vec::new();
    if let Some(ref intent) = proposal.intent {
        parts.push(format!("Intent: {}", intent.as_str()));
    }
    if !closing.is_empty() {
        parts.push(format!("Close {}: {}", closing.len(), closing.join(", ")));
    }
    if !priority.is_empty() {
        parts.push(format!("Reprioritize: {}", priority.join(", ")));
    }
    if parts.is_empty() {
        proposal.title.clone()
    } else {
        parts.join("; ")
    }
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let end = s.floor_char_boundary(max);
        format!("{}…", &s[..end])
    }
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
            intent: None, rationale: None, risk_flags: vec![],
            context_transfers: vec![], closure_summary: None, reviewer_questions: vec![],
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
            intent: None, rationale: None, risk_flags: vec![],
            context_transfers: vec![], closure_summary: None, reviewer_questions: vec![],
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
            intent: None, rationale: None, risk_flags: vec![],
            context_transfers: vec![], closure_summary: None, reviewer_questions: vec![],
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
