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
    /// Tickets whose title/description and notes imply materially different scopes —
    /// these need a scope decision before they can be sequenced with confidence.
    pub needs_clarification: Vec<NeedsClarification>,
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

/// A ticket whose literal scope (title/description) disagrees with a note,
/// so planning can't place it confidently until the scope is pinned down.
#[derive(Debug, Clone, Serialize)]
pub struct NeedsClarification {
    pub ticket_id: String,
    pub title: String,
    pub conflict_summary: String,
    pub narrow_reading: String,
    pub broad_reading: String,
    pub why_it_blocks_planning: String,
    pub suggested_resolution_options: Vec<String>,
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
// Signals that a ticket turns produced data/output into a human-operable step —
// a strong candidate for "what's next" once the core is active.
const NEXT_KEYWORDS: &[&str] = &[
    "summary", "review", "output", "generation", "generate", "workflow",
    "surface", "queue", "approval", "operable", "dashboard",
];
// Bounded-deliverable markers used to detect scope conflicts (narrow title vs.
// a note describing a much broader pipeline).
const NARROW_MARKERS: &[&str] = &[
    "summary", "review", "generation", "generate", "report", "digest",
    "surface", "queue", "approval", "dashboard", "statement",
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

    // The set of tickets we will classify (relevant, non-done, not the container root).
    let in_scope: Vec<&KanbanItem> = relevant.iter()
        .filter_map(|t| item_map.get(t.as_str()).copied())
        .filter(|i| i.status != "done")
        .filter(|i| !(root_is_container && root_ticket_id == Some(i.ticket_id.as_str())))
        .collect();

    // Pass 1: blocked tickets (explicit language wins over status). Notes count
    // for blocked detection only.
    let mut blocked_info: HashMap<String, (String, String)> = HashMap::new();
    for item in &in_scope {
        if let Some((kw, ev)) = locate_titledesc(item, BLOCKED_KEYWORDS)
            .or_else(|| locate_notes(item, BLOCKED_KEYWORDS))
        {
            blocked_info.insert(
                item.ticket_id.clone(),
                (ev, format!("Blocked/parked — mentions \"{kw}\"; resolve before sequencing.")),
            );
        } else if let Some(blocker) = relationship_blocker(item, relationships, &item_map) {
            blocked_info.insert(
                item.ticket_id.clone(),
                (format!("blocked by {blocker} (not yet done)"), format!("Blocked by incomplete dependency {blocker}.")),
            );
        }
    }

    // Center of gravity: active/review work that isn't blocked.
    let center_ids: HashSet<String> = in_scope.iter()
        .filter(|i| matches!(i.status.as_str(), "in_progress" | "review"))
        .filter(|i| !blocked_info.contains_key(&i.ticket_id))
        .map(|i| i.ticket_id.clone())
        .collect();

    let mut center = Vec::new();
    let mut next = Vec::new();
    let mut safety = Vec::new();
    let mut gates = Vec::new();
    let mut parallel = Vec::new();
    let mut later = Vec::new();
    let mut blocked = Vec::new();
    let mut needs_clarification = Vec::new();
    let mut unlinked_count = 0usize;

    for item in &in_scope {
        let touched_by_rel = relationships.iter()
            .any(|r| r.from_ticket_id == item.ticket_id || r.to_ticket_id == item.ticket_id);
        if !touched_by_rel { unlinked_count += 1; }

        if let Some(clarify) = detect_scope_conflict(item) {
            needs_clarification.push(clarify);
        }

        let (bucket, plan_item) = classify(
            item, root_ticket_id, relationships, &item_map,
            &center_ids, blocked_info.get(&item.ticket_id), opts,
        );
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

    // Cross-link: a ticket can sit in a work bucket AND need scope clarification.
    // Surface the flag inline so the two views connect.
    let clarify_ids: HashSet<&str> = needs_clarification.iter().map(|n| n.ticket_id.as_str()).collect();
    let ev_cap = if opts.full { 6 } else { 2 };
    for v in [&mut center, &mut next, &mut safety, &mut gates, &mut parallel, &mut later, &mut blocked] {
        for item in v.iter_mut() {
            if clarify_ids.contains(item.ticket_id.as_str()) {
                item.evidence.insert(0, "⚠ scope conflict — see needs_clarification".to_string());
                item.evidence.truncate(ev_cap);
            }
        }
        sort_items(v);
        v.truncate(opts.limit);
    }
    needs_clarification.truncate(opts.limit);

    let open_questions = build_questions(&relevant, questions, opts.limit);
    let suggested = suggest_relationships(&center, &next, &gates, &safety, relationships, opts.limit);
    let (confidence, mut caveats) = assess_confidence(
        &relevant, relationships, unlinked_count, &center, root_ticket_id, epic, &item_map,
    );
    if !needs_clarification.is_empty() {
        caveats.push(format!(
            "{} ticket(s) mix a narrow deliverable with a broader scope — see needs_clarification; their sequencing is uncertain until scope is confirmed.",
            needs_clarification.len()
        ));
    }

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
        needs_clarification,
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
    center_ids: &HashSet<String>,
    blocked: Option<&(String, String)>,
    opts: &PlanOptions,
) -> (Bucket, PlanItem) {
    let mut evidence: Vec<String> = Vec::new();
    // Role classification reads the ticket's literal scope — title + description.
    // Notes are supporting evidence / conflict detection only, never the primary
    // signal (a single architecture note must not override the description).
    let downstream = downstream_of_center(item, center_ids, relationships, item_map);

    // 1. Blocked (precomputed: explicit language in title/desc/notes, or an
    //    incomplete blocking dependency) — wins over everything.
    let (bucket, why) = if let Some((ev, why)) = blocked {
        evidence.push(ev.clone());
        (Bucket::Blocked, why.clone())
    // 2. Active work anchors the current center of gravity.
    } else if item.status == "in_progress" || item.status == "review" {
        (Bucket::Center, format!("Actively {} — anchors current execution.", item.status))
    // 3. Genuine safety/quality role from title/description.
    } else if let Some((kw, ev)) = locate_titledesc(item, SAFETY_KEYWORDS) {
        evidence.push(ev);
        (Bucket::Safety, format!("Safety/quality companion ({kw}) — should accompany the core build."))
    // 4. Externalization gate from title/description.
    } else if let Some((kw, ev)) = locate_titledesc(item, GATE_KEYWORDS) {
        evidence.push(ev);
        (Bucket::Gate, format!("Gates externalization ({kw}) — must land before external/customer-facing use."))
    // 5. Turns produced data/output into a human-operable step → likely next.
    } else if let Some((kw, ev)) = locate_titledesc(item, NEXT_KEYWORDS) {
        evidence.push(ev);
        (Bucket::Next, format!("Produces a reviewable {kw} surface — a likely next step once the core is active."))
    // 6. Independent research/data-collection track.
    } else if let Some((kw, ev)) = locate_titledesc(item, PARALLEL_KEYWORDS) {
        evidence.push(ev);
        (Bucket::Parallel, format!("Parallel {kw} work — can proceed without blocking the core flow."))
    // 7. High-priority, not-started, not blocked → next.
    } else if matches!(item.priority.as_str(), "urgent" | "high")
        && matches!(item.status.as_str(), "todo" | "backlog")
    {
        (Bucket::Next, format!("{} priority and not started — a likely next build/review item.", cap(&item.priority)))
    // 8. Directly downstream of active work → next, even without a keyword.
    } else if let Some(c) = &downstream {
        evidence.push(format!("downstream of active {c}"));
        (Bucket::Next, format!("Directly downstream of active {c} — the natural next step once it lands."))
    } else if locate_titledesc(item, LATER_KEYWORDS).is_some() || item.priority == "low" {
        if let Some((_, ev)) = locate_titledesc(item, LATER_KEYWORDS) { evidence.push(ev); }
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
        if let Some((kw, _)) = locate_titledesc(item, GATE_KEYWORDS) {
            evidence.insert(0, format!("also gates externalization (\"{kw}\")"));
        } else if let Some((kw, _)) = locate_titledesc(item, SAFETY_KEYWORDS) {
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

    // Lead with the execution-useful direction: the active center produces output
    // that the next items consume → "CM-92 feeds CM-6". This reads as a forward
    // sequencing edge, which is what an execution map wants.
    if let Some(c) = center.first() {
        for n in next {
            if n.ticket_id != c.ticket_id && !has_edge(&c.ticket_id, &n.ticket_id) {
                out.push(SuggestedRelationship {
                    from_ticket_id: c.ticket_id.clone(),
                    to_ticket_id: n.ticket_id.clone(),
                    relationship_type: "feeds".to_string(),
                    rationale: format!(
                        "{0} is the active center and {1} turns its output into the next step — {0} feeds {1} (clearer for sequencing than {1} depends_on {0}).",
                        c.ticket_id, n.ticket_id
                    ),
                });
            }
        }
    }

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
        "Heuristic classification: title, description, and status are primary; notes are supporting evidence and conflict detection only. Confirm before acting.".to_string(),
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

/// Locate the first keyword in the ticket's literal scope (title, then
/// description). This is the ONLY surface used for role classification — notes
/// are deliberately excluded so one stray note can't reclassify a ticket.
fn locate_titledesc(item: &KanbanItem, keywords: &[&'static str]) -> Option<(&'static str, String)> {
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
    None
}

/// Locate the first keyword in the ticket's notes. Used only for blocked
/// detection and as supporting (not primary) evidence.
fn locate_notes(item: &KanbanItem, keywords: &[&'static str]) -> Option<(&'static str, String)> {
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

/// If `item` sits directly downstream of an active (center) ticket via a
/// relationship, return that center ticket id.
fn downstream_of_center(
    item: &KanbanItem,
    center_ids: &HashSet<String>,
    relationships: &[Relationship],
    item_map: &HashMap<&str, &KanbanItem>,
) -> Option<String> {
    use crate::kanban::relationships::RelationshipType::*;
    let _ = item_map;
    for r in relationships {
        let downstream = match r.relationship_type {
            // center feeds item / center blocks item → item is downstream of center
            Feeds | Blocks if r.to_ticket_id == item.ticket_id && center_ids.contains(&r.from_ticket_id) => {
                Some(r.from_ticket_id.clone())
            }
            // item depends_on center / item consumes_output_from center
            DependsOn | ConsumesOutputFrom if r.from_ticket_id == item.ticket_id && center_ids.contains(&r.to_ticket_id) => {
                Some(r.to_ticket_id.clone())
            }
            _ => None,
        };
        if downstream.is_some() {
            return downstream;
        }
    }
    None
}

/// True if the text describes an end-to-end / full-pipeline scope.
fn broad_scope(text: &str) -> bool {
    let l = text.to_lowercase();
    const PHRASES: &[&str] = &[
        "full downstream", "downstream pipeline", "full pipeline", "entire pipeline",
        "whole pipeline", "complete pipeline", "end to end", "end-to-end",
        "calculation to payer", "soup to nuts", "all the way to", "entire flow", "whole flow",
    ];
    if PHRASES.iter().any(|p| l.contains(p)) {
        return true;
    }
    // Generic: "pipeline" qualified by a breadth word.
    l.contains("pipeline")
        && ["full", "entire", "whole", "complete", "downstream", "end to end"]
            .iter()
            .any(|b| l.contains(b))
}

/// Detect a ticket whose title/description scope a bounded deliverable while a
/// note describes a materially broader, end-to-end scope.
fn detect_scope_conflict(item: &KanbanItem) -> Option<NeedsClarification> {
    let title_lower = item.title.to_lowercase();
    let desc_lower = item.description.as_deref().unwrap_or("").to_lowercase();
    // If the literal scope is already broad, there's no hidden conflict.
    if broad_scope(&title_lower) || broad_scope(&desc_lower) {
        return None;
    }
    let (narrow_kw, _) = locate_titledesc(item, NARROW_MARKERS)?;
    let broad_note = item.notes.iter().find(|n| broad_scope(&n.text))?;

    let desc_snip = item.description.as_deref().map(|d| truncate(d.trim(), 100)).unwrap_or_default();
    let narrow_reading = if desc_snip.is_empty() {
        item.title.clone()
    } else {
        format!("{} — {}", item.title, desc_snip)
    };
    let broad_reading = truncate(broad_note.text.trim(), 160);

    Some(NeedsClarification {
        ticket_id: item.ticket_id.clone(),
        title: item.title.clone(),
        conflict_summary: format!(
            "Title/description scope a bounded '{narrow_kw}' deliverable, but a note describes a much broader end-to-end scope."
        ),
        narrow_reading,
        broad_reading,
        why_it_blocks_planning: format!(
            "A '{narrow_kw}' surface is a plausible next step after the current center, but a full downstream pipeline is a multi-step epic — sequencing and effort can't be set until the scope is confirmed."
        ),
        suggested_resolution_options: vec![
            format!("Split {0}: keep it as the narrow '{1}' deliverable and open a separate ticket/epic for the broader pipeline.", item.ticket_id, narrow_kw),
            format!("Confirm intended scope with the owner and update {}'s description so title, description, and notes agree.", item.ticket_id),
            "If the broad scope is intended, promote this to a parent/epic and break the pipeline into children.".to_string(),
        ],
    })
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
