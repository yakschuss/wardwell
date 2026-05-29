//! Read-only planning lens. Derives an "execution map" for a parent ticket,
//! epic, or project area so a fresh agent can organize work without prior
//! conversation context. This module never mutates kanban data — it only reads
//! the materialized tickets, relationships, and questions it is handed.

use crate::kanban::questions::{Question, QuestionStatus};
use crate::kanban::relationships::Relationship;
use crate::kanban::store::KanbanItem;
use serde::Serialize;
use std::collections::{HashMap, HashSet};

// ---- Output types ----

#[derive(Debug, Clone, Serialize)]
pub struct ExecutionMap {
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_ticket_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epic: Option<String>,
    /// Active or review work that currently anchors execution.
    pub current_center: Vec<PlanItem>,
    /// Likely next build/review items.
    pub next_recommended: Vec<PlanItem>,
    /// Reconciliation, monitoring, audit, anomaly, guardrail, no-silent-failure work.
    pub safety_companions: Vec<PlanItem>,
    /// Work needed before external handoff, billing submission, or production launch.
    pub gates_before_externalization: Vec<PlanItem>,
    /// Data collection, discovery, integration research — proceeds without blocking core flow.
    pub parallel_tracks: Vec<PlanItem>,
    /// Useful but not needed for the immediate operating loop.
    pub later_expansion: Vec<PlanItem>,
    /// Blocked by external dependency, unanswered decision, or missing confirmation.
    pub blocked_or_parked: Vec<PlanItem>,
    /// Open questions that affect sequencing.
    pub open_questions: Vec<PlanQuestion>,
    /// Inferred edges worth proposing later — NOT created by this view.
    pub suggested_relationships: Vec<SuggestedRelationship>,
    pub confidence: String,
    pub caveats: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanItem {
    pub ticket_id: String,
    pub title: String,
    pub status: String,
    pub priority: String,
    /// One short, human-readable reason this ticket landed in its section.
    pub why_here: String,
    /// Supporting facts: relationship, parentage, status, priority, or note excerpt.
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanQuestion {
    pub id: String,
    pub question: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needed_for: Option<String>,
    pub why_here: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SuggestedRelationship {
    pub from_ticket_id: String,
    pub to_ticket_id: String,
    pub relationship_type: String,
    pub rationale: String,
}

pub struct PlanOptions {
    pub full: bool,
    pub limit: usize,
}

impl Default for PlanOptions {
    fn default() -> Self {
        Self { full: false, limit: 10 }
    }
}

// ---- Classification vocabulary ----

const SAFETY_KEYWORDS: &[&str] = &[
    "reconciliation", "reconcile", "anomaly", "monitoring", "monitor", "audit",
    "compliance", "guardrail", "silent failure", "no silent", "no-silent",
    "alerting", "integrity check", "sanity check",
];
const GATE_KEYWORDS: &[&str] = &[
    "handoff", "hand-off", "elation", "candid", "claim", "billing submission",
    "submit", "production", "go live", "go-live", "launch", "external",
    "customer-facing", "customer facing", "webhook", "invoice", "output to",
];
const PARALLEL_KEYWORDS: &[&str] = &[
    "data collection", "collect data", "research", "discovery", "investigate",
    "investigation", "confirm", "spike", "explore", "gather", "prototype",
];
const BLOCKED_KEYWORDS: &[&str] = &[
    "blocked on", "blocked by", "blocked", "waiting on", "waiting for", "parked",
    "on hold", "pending external", "awaiting", "depends on external", "stuck",
];
const LATER_KEYWORDS: &[&str] = &[
    "later", "future", "expansion", "eventually", "nice to have", "phase 2",
    "phase two", "down the road", "someday", "stretch",
];

// ---- Entry point ----

pub fn build_plan(
    project: &str,
    root_ticket_id: Option<&str>,
    epic: Option<&str>,
    items: &[KanbanItem],
    relationships: &[Relationship],
    questions: &[Question],
    opts: &PlanOptions,
) -> ExecutionMap {
    let item_map: HashMap<&str, &KanbanItem> = items.iter()
        .filter(|i| i.project == project)
        .map(|i| (i.ticket_id.as_str(), i))
        .collect();

    let relevant = gather_relevant(project, root_ticket_id, epic, items, relationships, &item_map);

    // The root container itself is the organizing node, not a work item — skip it
    // from the work buckets when it has children.
    let root_is_container = root_ticket_id
        .and_then(|r| item_map.get(r))
        .map(has_children(items))
        .unwrap_or(false);

    let mut center = Vec::new();
    let mut next = Vec::new();
    let mut safety = Vec::new();
    let mut gates = Vec::new();
    let mut parallel = Vec::new();
    let mut later = Vec::new();
    let mut blocked = Vec::new();
    let mut unlinked_count = 0usize;

    for tid in &relevant {
        let Some(item) = item_map.get(tid.as_str()) else { continue };
        if item.status == "done" { continue; }
        if root_is_container && root_ticket_id == Some(tid.as_str()) { continue; }

        let touched_by_rel = relationships.iter()
            .any(|r| r.from_ticket_id == *tid || r.to_ticket_id == *tid);
        if !touched_by_rel { unlinked_count += 1; }

        let (bucket, plan_item) = classify(item, root_ticket_id, relationships, &item_map, opts);
        match bucket {
            Bucket::Center => center.push(plan_item),
            Bucket::Next => next.push(plan_item),
            Bucket::Safety => safety.push(plan_item),
            Bucket::Gate => gates.push(plan_item),
            Bucket::Parallel => parallel.push(plan_item),
            Bucket::Later => later.push(plan_item),
            Bucket::Blocked => blocked.push(plan_item),
        }
    }

    for v in [&mut center, &mut next, &mut safety, &mut gates, &mut parallel, &mut later, &mut blocked] {
        sort_items(v);
        v.truncate(opts.limit);
    }

    let open_questions = build_questions(&relevant, questions, opts.limit);
    let suggested = suggest_relationships(&center, &next, &gates, &safety, relationships, opts.limit);
    let (confidence, caveats) = assess_confidence(
        &relevant, relationships, unlinked_count, &center, root_ticket_id, epic, &item_map,
    );

    ExecutionMap {
        project: project.to_string(),
        root_ticket_id: root_ticket_id.map(String::from),
        epic: epic.map(String::from),
        current_center: center,
        next_recommended: next,
        safety_companions: safety,
        gates_before_externalization: gates,
        parallel_tracks: parallel,
        later_expansion: later,
        blocked_or_parked: blocked,
        open_questions,
        suggested_relationships: suggested,
        confidence,
        caveats,
    }
}

// ---- Gathering ----

fn gather_relevant(
    project: &str,
    root_ticket_id: Option<&str>,
    epic: Option<&str>,
    items: &[KanbanItem],
    relationships: &[Relationship],
    item_map: &HashMap<&str, &KanbanItem>,
) -> Vec<String> {
    let mut set: HashSet<String> = HashSet::new();

    if let Some(root) = root_ticket_id {
        if item_map.contains_key(root) {
            set.insert(root.to_string());
        }
        // Descendant subtree via parent pointers (fixpoint over parent edges).
        let mut changed = true;
        while changed {
            changed = false;
            for item in items.iter().filter(|i| i.project == project) {
                if let Some(parent) = &item.parent {
                    if set.contains(parent) && !set.contains(&item.ticket_id) {
                        set.insert(item.ticket_id.clone());
                        changed = true;
                    }
                }
            }
        }
    }

    if let Some(ep) = epic {
        for item in items.iter().filter(|i| i.project == project && i.epic.as_deref() == Some(ep)) {
            set.insert(item.ticket_id.clone());
        }
    }

    // Project-only planning: no root, no epic → all project tickets.
    if root_ticket_id.is_none() && epic.is_none() {
        for item in items.iter().filter(|i| i.project == project) {
            set.insert(item.ticket_id.clone());
        }
    }

    // One hop of relationship neighbors that exist in this project.
    let core: Vec<String> = set.iter().cloned().collect();
    for r in relationships {
        if core.contains(&r.from_ticket_id) && item_map.contains_key(r.to_ticket_id.as_str()) {
            set.insert(r.to_ticket_id.clone());
        }
        if core.contains(&r.to_ticket_id) && item_map.contains_key(r.from_ticket_id.as_str()) {
            set.insert(r.from_ticket_id.clone());
        }
    }

    set.into_iter().collect()
}

fn has_children(items: &[KanbanItem]) -> impl Fn(&&KanbanItem) -> bool + '_ {
    move |root: &&KanbanItem| {
        !root.children.is_empty()
            || items.iter().any(|i| i.parent.as_deref() == Some(root.ticket_id.as_str()))
    }
}

// ---- Classification ----

enum Bucket {
    Center,
    Next,
    Safety,
    Gate,
    Parallel,
    Later,
    Blocked,
}

fn classify(
    item: &KanbanItem,
    root: Option<&str>,
    relationships: &[Relationship],
    item_map: &HashMap<&str, &KanbanItem>,
    opts: &PlanOptions,
) -> (Bucket, PlanItem) {
    let mut evidence: Vec<String> = Vec::new();

    let rel_block = relationship_blocker(item, relationships, item_map);
    let blocked_hit = locate(item, BLOCKED_KEYWORDS);

    // 1. Explicit blocked language (or an incomplete blocking dependency) wins.
    let (bucket, why) = if let Some((kw, ev)) = blocked_hit {
        evidence.push(ev);
        (Bucket::Blocked, format!("Blocked/parked — mentions \"{kw}\"; resolve before sequencing."))
    } else if let Some(blocker) = rel_block.clone() {
        evidence.push(format!("blocked by {blocker} (not yet done)"));
        (Bucket::Blocked, format!("Blocked by incomplete dependency {blocker}."))
    // 2. Active work anchors the current center of gravity.
    } else if item.status == "in_progress" || item.status == "review" {
        (Bucket::Center, format!("Actively {} — anchors current execution.", item.status))
    // 3. Role-based classification for not-yet-active work.
    } else if let Some((kw, ev)) = locate(item, SAFETY_KEYWORDS) {
        evidence.push(ev);
        (Bucket::Safety, format!("Safety/quality companion ({kw}) — should accompany the core build."))
    } else if let Some((kw, ev)) = locate(item, GATE_KEYWORDS) {
        evidence.push(ev);
        (Bucket::Gate, format!("Gates externalization ({kw}) — must land before external/customer-facing use."))
    } else if let Some((kw, ev)) = locate(item, PARALLEL_KEYWORDS) {
        evidence.push(ev);
        (Bucket::Parallel, format!("Parallel {kw} work — can proceed without blocking the core flow."))
    } else if matches!(item.priority.as_str(), "urgent" | "high")
        && matches!(item.status.as_str(), "todo" | "backlog")
    {
        (Bucket::Next, format!("{} priority and not started — a likely next build/review item.", cap(&item.priority)))
    } else if locate(item, LATER_KEYWORDS).is_some() || item.priority == "low" {
        if let Some((_, ev)) = locate(item, LATER_KEYWORDS) { evidence.push(ev); }
        (Bucket::Later, "Useful but not needed for the immediate operating loop.".to_string())
    } else if item.status == "todo" {
        (Bucket::Next, "Ready to start under this parent.".to_string())
    } else {
        (Bucket::Later, "Backlog item — not part of the immediate loop.".to_string())
    };

    // Supporting evidence (ordered by decision-relevance).
    if let Some(parent) = &item.parent {
        if root == Some(parent.as_str()) {
            evidence.push(format!("direct child of {parent}"));
        } else {
            evidence.push(format!("child of {parent}"));
        }
    }
    let rels = relationship_evidence(item, relationships);
    if opts.full {
        evidence.extend(rels);
    } else if let Some(first) = rels.into_iter().next() {
        evidence.push(first);
    }
    evidence.push(format!("status: {}", item.status));
    evidence.push(format!("priority: {}", item.priority));

    // Note a secondary role even when status already routed it to the center.
    if matches!(item.status.as_str(), "in_progress" | "review") {
        if let Some((kw, _)) = locate(item, GATE_KEYWORDS) {
            evidence.insert(0, format!("also gates externalization (\"{kw}\")"));
        } else if let Some((kw, _)) = locate(item, SAFETY_KEYWORDS) {
            evidence.insert(0, format!("also safety/quality work (\"{kw}\")"));
        }
    }

    // Full mode keeps a richer note excerpt for sequencing context.
    if opts.full {
        if let Some(note) = item.notes.last() {
            evidence.push(format!("latest note: \"{}\"", truncate(&note.text, 140)));
        }
    }

    let cap_evidence = if opts.full { 6 } else { 2 };
    evidence.truncate(cap_evidence);

    let plan_item = PlanItem {
        ticket_id: item.ticket_id.clone(),
        title: item.title.clone(),
        status: item.status.clone(),
        priority: item.priority.clone(),
        why_here: why,
        evidence,
    };
    (bucket, plan_item)
}

/// A relationship that makes `item` blocked by an incomplete ticket, if any.
fn relationship_blocker(
    item: &KanbanItem,
    relationships: &[Relationship],
    item_map: &HashMap<&str, &KanbanItem>,
) -> Option<String> {
    use crate::kanban::relationships::RelationshipType;
    let incomplete = |tid: &str| item_map.get(tid).map(|i| i.status != "done").unwrap_or(false);
    for r in relationships {
        match r.relationship_type {
            RelationshipType::Blocks if r.to_ticket_id == item.ticket_id && incomplete(&r.from_ticket_id) => {
                return Some(r.from_ticket_id.clone());
            }
            RelationshipType::DependsOn if r.from_ticket_id == item.ticket_id && incomplete(&r.to_ticket_id) => {
                return Some(r.to_ticket_id.clone());
            }
            _ => {}
        }
    }
    None
}

fn relationship_evidence(item: &KanbanItem, relationships: &[Relationship]) -> Vec<String> {
    let mut out = Vec::new();
    for r in relationships {
        if r.from_ticket_id == item.ticket_id {
            out.push(format!("{} {}", r.relationship_type.as_str(), r.to_ticket_id));
        } else if r.to_ticket_id == item.ticket_id {
            out.push(format!("{} {} (incoming)", r.relationship_type.as_str(), r.from_ticket_id));
        }
    }
    out
}

// ---- Questions ----

fn build_questions(relevant: &[String], questions: &[Question], limit: usize) -> Vec<PlanQuestion> {
    let set: HashSet<&str> = relevant.iter().map(String::as_str).collect();
    let mut out: Vec<PlanQuestion> = questions.iter()
        .filter(|q| q.status == QuestionStatus::Open)
        .filter(|q| match &q.ticket_id {
            Some(tid) => set.contains(tid.as_str()),
            None => true, // project-level questions can still govern sequencing
        })
        .map(|q| {
            let why_here = match &q.needed_for {
                Some(nf) => format!("Governs sequencing: {}", truncate(nf, 80)),
                None => match &q.ticket_id {
                    Some(tid) => format!("Open question on {tid} affecting sequencing."),
                    None => "Project-level open question affecting sequencing.".to_string(),
                },
            };
            PlanQuestion {
                id: q.id.clone(),
                question: truncate(&q.question, 160),
                ticket_id: q.ticket_id.clone(),
                needed_for: q.needed_for.clone(),
                why_here,
            }
        })
        .collect();
    out.truncate(limit);
    out
}

// ---- Suggested relationships (inferred, never created) ----

fn suggest_relationships(
    center: &[PlanItem],
    next: &[PlanItem],
    gates: &[PlanItem],
    safety: &[PlanItem],
    relationships: &[Relationship],
    limit: usize,
) -> Vec<SuggestedRelationship> {
    // The "core" is what execution currently hangs on: active work, else the top next item.
    let core = center.first().or_else(|| next.first());
    let Some(core) = core else { return Vec::new() };

    let has_edge = |a: &str, b: &str| {
        relationships.iter().any(|r| {
            (r.from_ticket_id == a && r.to_ticket_id == b)
                || (r.from_ticket_id == b && r.to_ticket_id == a)
        })
    };

    let mut out = Vec::new();
    for g in gates {
        if g.ticket_id != core.ticket_id && !has_edge(&g.ticket_id, &core.ticket_id) {
            out.push(SuggestedRelationship {
                from_ticket_id: g.ticket_id.clone(),
                to_ticket_id: core.ticket_id.clone(),
                relationship_type: "depends_on".to_string(),
                rationale: format!(
                    "{} externalizes work, so it likely depends on the core build {} landing first.",
                    g.ticket_id, core.ticket_id
                ),
            });
        }
    }
    for s in safety {
        if s.ticket_id != core.ticket_id && !has_edge(&s.ticket_id, &core.ticket_id) {
            out.push(SuggestedRelationship {
                from_ticket_id: s.ticket_id.clone(),
                to_ticket_id: core.ticket_id.clone(),
                relationship_type: "related".to_string(),
                rationale: format!(
                    "{} guards {} — worth linking so the safety work tracks the core build.",
                    s.ticket_id, core.ticket_id
                ),
            });
        }
    }
    out.truncate(limit);
    out
}

// ---- Confidence ----

fn assess_confidence(
    relevant: &[String],
    relationships: &[Relationship],
    unlinked_count: usize,
    center: &[PlanItem],
    root_ticket_id: Option<&str>,
    epic: Option<&str>,
    item_map: &HashMap<&str, &KanbanItem>,
) -> (String, Vec<String>) {
    let mut caveats = vec![
        "Heuristic classification from titles, status, priority, and notes — confirm before acting.".to_string(),
        "Read-only view: no tickets, relationships, notes, or questions were created or modified.".to_string(),
    ];

    let rel_in_scope = relationships.iter().any(|r| {
        relevant.iter().any(|t| t == &r.from_ticket_id) && relevant.iter().any(|t| t == &r.to_ticket_id)
    });

    let confidence = if relevant.is_empty() {
        caveats.push("No tickets matched the requested scope.".to_string());
        "low"
    } else if !rel_in_scope {
        caveats.push("No relationships recorded among these tickets — sequencing is inferred from priority and status only.".to_string());
        "low"
    } else if center.is_empty() {
        caveats.push("No active (in_progress/review) work — current center of gravity is inferred, not observed.".to_string());
        "medium"
    } else {
        "medium"
    };

    if unlinked_count > 0 && rel_in_scope {
        caveats.push(format!("{unlinked_count} ticket(s) have no relationships; their ordering is a best guess."));
    }

    // Note descendant depth so the reader knows the map may extend further.
    if let Some(root) = root_ticket_id {
        let grandchildren = item_map.values().any(|i| {
            i.parent.as_deref().is_some_and(|p| {
                item_map.get(p).and_then(|pi| pi.parent.as_deref()) == Some(root)
            })
        });
        if grandchildren {
            caveats.push("Subtree has multiple levels; deeper descendants are included but the map is flat.".to_string());
        }
    }
    let _ = epic;

    (confidence.to_string(), caveats)
}

// ---- Helpers ----

fn sort_items(items: &mut [PlanItem]) {
    items.sort_by(|a, b| {
        priority_rank(&a.priority)
            .cmp(&priority_rank(&b.priority))
            .then(status_rank(&a.status).cmp(&status_rank(&b.status)))
            .then(a.ticket_id.cmp(&b.ticket_id))
    });
}

fn priority_rank(p: &str) -> u8 {
    match p {
        "urgent" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 4,
    }
}

fn status_rank(s: &str) -> u8 {
    match s {
        "in_progress" => 0,
        "review" => 1,
        "todo" => 2,
        "backlog" => 3,
        _ => 4,
    }
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Locate the first matching keyword and return (keyword, evidence string citing
/// where it matched). Searches title, then description, then notes.
fn locate(item: &KanbanItem, keywords: &[&'static str]) -> Option<(&'static str, String)> {
    let title_lower = item.title.to_lowercase();
    for kw in keywords {
        if title_lower.contains(kw) {
            return Some((kw, format!("title mentions \"{kw}\"")));
        }
    }
    if let Some(desc) = &item.description {
        let desc_lower = desc.to_lowercase();
        for kw in keywords {
            if desc_lower.contains(kw) {
                return Some((kw, format!("description: \"{}\"", snippet(desc, kw, 80))));
            }
        }
    }
    for note in &item.notes {
        let note_lower = note.text.to_lowercase();
        for kw in keywords {
            if note_lower.contains(kw) {
                return Some((kw, format!("note: \"{}\"", snippet(&note.text, kw, 80))));
            }
        }
    }
    None
}

/// Short excerpt of `text` centered on `needle`, with ellipses when clipped.
fn snippet(text: &str, needle: &str, max: usize) -> String {
    let lower = text.to_lowercase();
    let pos = lower.find(&needle.to_lowercase()).unwrap_or(0);
    let start = text.floor_char_boundary(pos.saturating_sub(16));
    let end = text.floor_char_boundary((pos + needle.len() + max).min(text.len()));
    let mut out = text[start..end].trim().to_string();
    if start > 0 {
        out = format!("…{out}");
    }
    if end < text.len() {
        out = format!("{out}…");
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let end = s.floor_char_boundary(max);
        format!("{}…", &s[..end])
    }
}
