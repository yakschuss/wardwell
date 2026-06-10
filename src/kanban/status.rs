//! Read-only macro status snapshot (CR-18).
//!
//! Turns kanban state into a stakeholder-facing markdown report: what shipped,
//! what's building, what's blocked, and workstream health. Deterministic —
//! counts and ticket fields first, no AI, no ticket mutation. The kanban stays
//! the source of truth; this is a view rendered from it.
//!
//! Data collection (`build_snapshot`) is kept separate from rendering
//! (`render_markdown` / `render_slack`) so both are deterministic and testable.

use crate::kanban::store::KanbanItem;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

const WORKSTREAM_PREFIX: &str = "workstream:";
const BLOCKED_TAGS: &[&str] = &["external-blocked", "vendor-blocked", "blocked"];
const LAUNCH_TAG: &str = "launch-scope";
/// Backlog tickets are out of scope unless they carry one of these (or a workstream tag).
const SCOPE_BACKLOG_TAGS: &[&str] = &["launch-scope", "external-blocked", "vendor-blocked"];
/// Parked/deferred work is excluded from the report and its progress math.
const EXCLUDE_TAGS: &[&str] = &["parked", "deferred"];

pub struct StatusOptions {
    pub recent_days: u64,
    pub include_backlog: bool,
}

impl Default for StatusOptions {
    fn default() -> Self {
        Self { recent_days: 7, include_backlog: false }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TicketLine {
    pub ticket_id: String,
    pub title: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub workstreams: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkstreamGroup {
    pub workstream: String,
    pub tickets: Vec<TicketLine>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkstreamHealth {
    pub workstream: String,
    pub done: usize,
    pub review: usize,
    pub in_progress: usize,
    pub todo: usize,
    pub backlog: usize,
    pub status_word: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusSnapshot {
    pub project: String,
    pub platform: String,
    pub generated_at: String,
    pub recent_days: u64,
    pub executive_summary: Vec<String>,
    pub shipped_recently: Vec<TicketLine>,
    pub building_now: Vec<WorkstreamGroup>,
    pub blocked: Vec<TicketLine>,
    pub workstream_health: Vec<WorkstreamHealth>,
    pub uncategorized: Vec<TicketLine>,
}

impl StatusSnapshot {
    pub fn workstream_count(&self) -> usize { self.workstream_health.len() }
    pub fn uncategorized_count(&self) -> usize { self.uncategorized.len() }
    pub fn blocked_count(&self) -> usize { self.blocked.len() }
    pub fn recently_shipped_count(&self) -> usize { self.shipped_recently.len() }
}

// ---- Build ----

pub fn build_snapshot(
    project: &str,
    platform: &str,
    items: &[KanbanItem],
    generated_at: &str,
    opts: &StatusOptions,
) -> StatusSnapshot {
    let now = chrono::Utc::now();
    let threshold = (now - chrono::Duration::days(opts.recent_days as i64)).to_rfc3339();

    let scoped: Vec<&KanbanItem> = items.iter()
        .filter(|i| i.project == project)
        .filter(|i| !excluded(i))
        .filter(|i| in_scope(i, &threshold, opts.include_backlog))
        .collect();

    // Shipped recently — done within the window, newest first.
    let mut shipped_recently: Vec<TicketLine> = scoped.iter()
        .filter(|i| i.status == "done")
        .map(|i| line(i))
        .collect();
    shipped_recently.sort_by(|a, b| b.completed_at.cmp(&a.completed_at));

    // Building now — in_progress/review that carry a workstream tag, grouped.
    // A ticket with multiple workstream tags appears under each.
    let mut groups: BTreeMap<String, Vec<TicketLine>> = BTreeMap::new();
    for i in scoped.iter().filter(|i| matches!(i.status.as_str(), "in_progress" | "review")) {
        for ws in workstreams_of(i) {
            groups.entry(ws).or_default().push(line(i));
        }
    }
    let building_now: Vec<WorkstreamGroup> = groups.into_iter()
        .map(|(workstream, tickets)| WorkstreamGroup { workstream, tickets })
        .collect();

    // Blocked / needs help — from explicit tags only (no fragile NLP in v1).
    let blocked: Vec<TicketLine> = scoped.iter()
        .filter(|i| is_blocked(i))
        .map(|i| line(i))
        .collect();

    let workstream_health = build_health(&scoped);

    // Uncategorized — active work with no workstream tag (a hygiene signal).
    let uncategorized: Vec<TicketLine> = scoped.iter()
        .filter(|i| matches!(i.status.as_str(), "in_progress" | "review" | "todo"))
        .filter(|i| workstreams_of(i).is_empty())
        .map(|i| line(i))
        .collect();

    let executive_summary = build_summary(
        &shipped_recently, &building_now, &blocked, &workstream_health, &uncategorized, &scoped, opts.recent_days,
    );

    StatusSnapshot {
        project: project.to_string(),
        platform: platform.to_string(),
        generated_at: generated_at.to_string(),
        recent_days: opts.recent_days,
        executive_summary,
        shipped_recently,
        building_now,
        blocked,
        workstream_health,
        uncategorized,
    }
}

fn in_scope(item: &KanbanItem, threshold: &str, include_backlog: bool) -> bool {
    match item.status.as_str() {
        "done" => item.completed_at.as_deref().map(|c| c >= threshold).unwrap_or(false),
        "review" | "in_progress" | "todo" => true,
        "backlog" => include_backlog
            || SCOPE_BACKLOG_TAGS.iter().any(|t| has_tag(item, t))
            || !workstreams_of(item).is_empty(),
        _ => false,
    }
}

fn build_health(scoped: &[&KanbanItem]) -> Vec<WorkstreamHealth> {
    let mut map: BTreeMap<String, WorkstreamHealth> = BTreeMap::new();
    let mut blocked_ws: BTreeSet<String> = BTreeSet::new();
    for i in scoped {
        let ws_list = workstreams_of(i);
        if is_blocked(i) {
            for ws in &ws_list { blocked_ws.insert(ws.clone()); }
        }
        for ws in ws_list {
            let h = map.entry(ws.clone()).or_insert_with(|| WorkstreamHealth {
                workstream: ws.clone(), done: 0, review: 0, in_progress: 0, todo: 0, backlog: 0,
                status_word: String::new(),
            });
            match i.status.as_str() {
                "done" => h.done += 1,
                "review" => h.review += 1,
                "in_progress" => h.in_progress += 1,
                "todo" => h.todo += 1,
                "backlog" => h.backlog += 1,
                _ => {}
            }
        }
    }
    let mut out: Vec<WorkstreamHealth> = map.into_values().collect();
    for h in out.iter_mut() {
        h.status_word = status_word(h, blocked_ws.contains(&h.workstream));
    }
    out
}

/// A status word, never a backlog-wide percentage.
fn status_word(h: &WorkstreamHealth, blocked: bool) -> String {
    if blocked {
        "Blocked externally".into()
    } else if h.in_progress > 0 || h.review > 0 {
        if h.done > 0 { "Shipping".into() } else { "Building".into() }
    } else if h.done > 0 && h.todo == 0 && h.backlog == 0 {
        "Shipped".into()
    } else if h.todo > 0 {
        "Mostly shaped".into()
    } else {
        "Needs triage".into()
    }
}

#[allow(clippy::too_many_arguments)]
fn build_summary(
    shipped: &[TicketLine],
    building: &[WorkstreamGroup],
    blocked: &[TicketLine],
    health: &[WorkstreamHealth],
    uncategorized: &[TicketLine],
    scoped: &[&KanbanItem],
    recent_days: u64,
) -> Vec<String> {
    let mut s = Vec::new();
    if !shipped.is_empty() {
        s.push(format!("{} ticket(s) shipped in the last {} days.", shipped.len(), recent_days));
    }
    let building_count: usize = building.iter().map(|g| g.tickets.len()).sum();
    if building_count > 0 {
        s.push(format!("{} item(s) building now across {} workstream(s).", building_count, health.len()));
    }
    if !blocked.is_empty() {
        let ids: Vec<&str> = blocked.iter().map(|t| t.ticket_id.as_str()).take(6).collect();
        s.push(format!("{} item(s) blocked / need help: {}.", blocked.len(), ids.join(", ")));
    }
    let launch = scoped.iter().filter(|i| has_tag(i, LAUNCH_TAG) && i.status != "done").count();
    if launch > 0 {
        s.push(format!("{launch} launch-scope item(s) in flight."));
    }
    if !uncategorized.is_empty() {
        s.push(format!("{} active ticket(s) lack a workstream tag (board hygiene).", uncategorized.len()));
    }
    if s.is_empty() {
        s.push("No active work in scope.".into());
    }
    s.truncate(6);
    s
}

// ---- Render ----

pub fn render_markdown(s: &StatusSnapshot) -> String {
    let date = day(&s.generated_at);
    let mut o = String::new();
    let _ = writeln!(o, "# {} Status — {}", s.platform.to_uppercase(), date);
    let _ = writeln!(o, "\n_Project: {} · generated {} · read-only snapshot from kanban_", s.project, s.generated_at);

    let _ = writeln!(o, "\n## Executive Summary\n");
    for b in &s.executive_summary { let _ = writeln!(o, "- {b}"); }

    let _ = writeln!(o, "\n## Shipped Recently (last {} days)\n", s.recent_days);
    if s.shipped_recently.is_empty() { let _ = writeln!(o, "- (none)"); }
    for t in &s.shipped_recently {
        let when = t.completed_at.as_deref().map(day).filter(|d| !d.is_empty());
        match when {
            Some(d) => { let _ = writeln!(o, "- {} {} ({})", t.ticket_id, t.title, d); }
            None => { let _ = writeln!(o, "- {} {}", t.ticket_id, t.title); }
        }
    }

    let _ = writeln!(o, "\n## Building Now\n");
    if s.building_now.is_empty() { let _ = writeln!(o, "- (nothing in progress with a workstream tag)"); }
    for g in &s.building_now {
        let _ = writeln!(o, "**{}**", g.workstream);
        for t in &g.tickets { let _ = writeln!(o, "- {} {} [{}]", t.ticket_id, t.title, t.status); }
        let _ = writeln!(o);
    }

    let _ = writeln!(o, "## Blocked / Needs Help\n");
    if s.blocked.is_empty() { let _ = writeln!(o, "- (none flagged)"); }
    for t in &s.blocked { let _ = writeln!(o, "- {} {} [{}]", t.ticket_id, t.title, t.status); }

    let _ = writeln!(o, "\n## Workstream Health\n");
    if s.workstream_health.is_empty() { let _ = writeln!(o, "- (no workstream tags in scope)"); }
    for h in &s.workstream_health {
        let _ = writeln!(o, "- **{}** — {} (done {}, review {}, in progress {}, todo {}, backlog {})",
            h.workstream, h.status_word, h.done, h.review, h.in_progress, h.todo, h.backlog);
    }

    let _ = writeln!(o, "\n## Uncategorized\n");
    if s.uncategorized.is_empty() { let _ = writeln!(o, "- (all active work is tagged)"); }
    for t in &s.uncategorized { let _ = writeln!(o, "- {} {} [{}]", t.ticket_id, t.title, t.status); }

    let _ = writeln!(o, "\n## Copy-Ready Slack Version\n");
    let _ = writeln!(o, "```\n{}\n```", render_slack(s));
    o
}

pub fn render_slack(s: &StatusSnapshot) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "{} STATUS — {}", s.platform.to_uppercase(), day(&s.generated_at));
    if !s.shipped_recently.is_empty() {
        let items: Vec<String> = s.shipped_recently.iter().take(8)
            .map(|t| format!("{} {}", t.ticket_id, t.title)).collect();
        let _ = writeln!(o, "\nShipped recently: {}.", items.join(", "));
    }
    let mut seen = BTreeSet::new();
    let building: Vec<String> = s.building_now.iter()
        .flat_map(|g| g.tickets.iter())
        .filter(|t| seen.insert(t.ticket_id.clone()))
        .map(|t| format!("{} {}", t.ticket_id, t.title))
        .collect();
    if !building.is_empty() {
        let _ = writeln!(o, "\nBuilding now: {}.", building.join(", "));
    }
    if !s.blocked.is_empty() {
        let b: Vec<String> = s.blocked.iter().map(|t| format!("{} {}", t.ticket_id, t.title)).collect();
        let _ = writeln!(o, "\nNeeds help / external blockers: {}.", b.join(", "));
    }
    if !s.workstream_health.is_empty() {
        let _ = writeln!(o, "\nWorkstream health:");
        for h in &s.workstream_health {
            let _ = writeln!(o, "{} — {}", h.workstream, h.status_word.to_lowercase());
        }
    }
    o.trim_end().to_string()
}

// ---- helpers ----

fn workstreams_of(item: &KanbanItem) -> Vec<String> {
    item.tags.iter()
        .filter_map(|t| t.strip_prefix(WORKSTREAM_PREFIX))
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

fn has_tag(item: &KanbanItem, tag: &str) -> bool {
    item.tags.iter().any(|t| t == tag)
}

fn is_blocked(item: &KanbanItem) -> bool {
    BLOCKED_TAGS.iter().any(|b| has_tag(item, b))
}

fn excluded(item: &KanbanItem) -> bool {
    EXCLUDE_TAGS.iter().any(|e| has_tag(item, e))
}

fn line(item: &KanbanItem) -> TicketLine {
    TicketLine {
        ticket_id: item.ticket_id.clone(),
        title: item.title.clone(),
        status: item.status.clone(),
        completed_at: item.completed_at.clone(),
        workstreams: workstreams_of(item),
    }
}

/// The YYYY-MM-DD prefix of an RFC3339 timestamp.
fn day(ts: &str) -> &str {
    ts.get(0..10).unwrap_or(ts)
}

/// Vault-relative path for a status artifact.
pub fn artifact_relative_path(domain: &str, project: &str, platform: &str, generated_at: &str) -> String {
    format!("{domain}/{project}/status/{platform}-status-{}.md", day(generated_at))
}
