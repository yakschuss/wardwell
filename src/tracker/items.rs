//! Shapes mirrored tracker issues as kanban read results and decides which
//! of them a kanban list, search, or named query returns.
//!
//! Does NOT read the log (see `view`), pull, or touch native kanban items
//! beyond tagging their origin.

use crate::config::loader::TrackerBinding;
use crate::tracker::view::{MirrorView, MirroredIssue, age_words};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Value, json};

/// Named kanban queries the mirror can answer, and the window each uses.
pub const MIRROR_QUERIES: &[&str] = &["recent", "stale"];

/// Note returned with a named query the mirror cannot answer.
pub const QUERY_NOT_MIRRORED: &str = "Tracker mirror items answer only the recent and stale queries.";

/// At most this many mirrored items in a `list` or `query` result.
pub const LIST_CAP: usize = 50;

/// At most this many mirrored items in a `search` result.
pub const SEARCH_CAP: usize = 20;

/// A mirrored issue as a `list`, `query` or `search` result: `mirrored`
/// without the description, which only `get` returns.
pub fn summary(binding: &TrackerBinding, view: &MirrorView, issue: &MirroredIssue, now: DateTime<Utc>) -> Value {
    let mut value = mirrored(binding, view, issue, now);
    if let Value::Object(fields) = &mut value {
        fields.remove("description");
    }
    value
}

/// Orders keys by team, then by number as a number: COR-2 before COR-10.
pub fn key_order(a: &str, b: &str) -> std::cmp::Ordering {
    let split = |key: &str| {
        let (team, number) = key.rsplit_once('-').unwrap_or((key, ""));
        (team.to_ascii_uppercase(), number.parse::<u64>().ok(), key.to_string())
    };
    split(a).cmp(&split(b))
}

/// A native kanban item as a read result: its own fields plus `origin: "kanban"`.
pub fn native(item: &impl serde::Serialize) -> Value {
    let mut value = serde_json::to_value(item).unwrap_or(Value::Null);
    if let Value::Object(fields) = &mut value {
        fields.insert("origin".into(), json!("kanban"));
    }
    value
}

/// A mirrored issue as a read result, with the mirror's pull time and its age.
pub fn mirrored(binding: &TrackerBinding, view: &MirrorView, issue: &MirroredIssue, now: DateTime<Utc>) -> Value {
    let snapshot = &issue.issue;
    json!({
        "origin": "tracker",
        "ticket_id": issue.key,
        "external_key": issue.key,
        "provider": issue.provider,
        "domain": binding.domain,
        "project": binding.project,
        "title": snapshot.issue_title,
        "description": snapshot.description,
        "state": snapshot.state,
        "state_category": snapshot.state_category,
        "priority": snapshot.priority,
        "assignee": snapshot.assignee,
        "labels": snapshot.labels,
        "parent_key": snapshot.parent_key,
        "relations": snapshot.relations,
        "url": snapshot.url,
        "updated_at": issue.updated_at,
        "archived_at": snapshot.archived_at,
        "removed_at": issue.removed_at,
        "last_pulled_at": view.last_pull_at,
        "last_pulled_age": pulled_age(view.last_pull_at, now),
    })
}

/// `3 hours ago`, or `never` when the mirror has no completed pull.
pub fn pulled_age(last_pull_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    last_pull_at.map_or("never".to_string(), |at| format!("{} ago", age_words(now - at)))
}

/// The kanban list filters as they apply to a mirrored issue.
#[derive(Debug, Default, Clone)]
pub struct ListFilter<'a> {
    /// Tracker state name or category.
    pub status: Option<&'a str>,
    /// Priority label.
    pub priority: Option<&'a str>,
    /// Assignee name.
    pub assignee: Option<&'a str>,
    /// Epic filter; the mirror has no epics.
    pub epic: Option<&'a str>,
    /// Label filter.
    pub tag: Option<&'a str>,
    /// Keep completed and canceled issues.
    pub include_done: bool,
}

impl ListFilter<'_> {
    /// True when an open issue passes every filter. `status` matches the
    /// state name or category; `tag` matches a label. The mirror has no
    /// epics, so an epic filter excludes every mirrored issue.
    pub fn keeps(&self, issue: &MirroredIssue) -> bool {
        let snapshot = &issue.issue;
        issue.is_open()
            && self.epic.is_none()
            && (self.include_done || !issue.is_done())
            && self.status.is_none_or(|s| same_word(s, &snapshot.state) || same_word(s, &category(issue)))
            && self.priority.is_none_or(|p| same_word(p, &enum_name(&snapshot.priority)))
            && self.assignee.is_none_or(|a| snapshot.assignee.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(a)))
            && self.tag.is_none_or(|t| snapshot.labels.iter().any(|l| l.eq_ignore_ascii_case(t)))
    }
}

/// True when an open issue's key, title, or description contains `query`,
/// ignoring case.
pub fn search_keeps(issue: &MirroredIssue, query: &str) -> bool {
    let needle = query.to_lowercase();
    let snapshot = &issue.issue;
    issue.is_open()
        && [Some(issue.key.as_str()), Some(snapshot.issue_title.as_str()), snapshot.description.as_deref()]
            .into_iter()
            .flatten()
            .any(|text| text.to_lowercase().contains(&needle))
}

/// Whether an open issue answers the named query at `now`, or None when the
/// mirror cannot answer that query. `recent`: updated within two days.
/// `stale`: not done and not updated for seven days.
pub fn query_keeps(issue: &MirroredIssue, question: &str, now: DateTime<Utc>) -> Option<bool> {
    let age = now - issue.updated_at;
    let keeps = match question {
        "recent" => age < TimeDelta::days(2),
        "stale" => !issue.is_done() && age > TimeDelta::days(7),
        _ => return None,
    };
    Some(issue.is_open() && keeps)
}

fn category(issue: &MirroredIssue) -> String {
    enum_name(&issue.issue.state_category)
}

fn enum_name(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

/// Equal ignoring case, with spaces, hyphens and underscores alike.
fn same_word(a: &str, b: &str) -> bool {
    let fold = |s: &str| s.to_lowercase().replace([' ', '-'], "_");
    fold(a) == fold(b)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::tracker::events::{IssueSnapshot, Priority, Relation, RelationKind, StateCategory};
    use chrono::TimeZone;

    fn at(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, day, 12, 0, 0).unwrap()
    }

    fn issue(key: &str, state: &str, category: StateCategory) -> MirroredIssue {
        MirroredIssue {
            key: key.into(),
            provider: "linear".into(),
            external_id: format!("{key}-id"),
            issue: IssueSnapshot {
                issue_title: "Claims inbox shows wrong payer".into(),
                description: Some("Payer column".into()),
                state: state.into(),
                state_category: category,
                priority: Priority::High,
                assignee: Some("Jane Doe".into()),
                labels: vec!["billing".into()],
                url: Some("https://example.com/COR-12".into()),
                parent_key: Some("COR-5".into()),
                relations: vec![Relation { kind: RelationKind::Blocks, key: "COR-14".into() }],
                ..Default::default()
            },
            updated_at: at(10),
            removed_at: None,
        }
    }

    fn binding() -> TrackerBinding {
        TrackerBinding {
            domain: "work".into(),
            project: "claims".into(),
            provider: "linear".into(),
            team: "COR".into(),
            credential: "c".into(),
            readonly: true,
        }
    }

    #[test]
    fn mirrored_item_carries_origin_structure_and_pull_age() {
        let view = MirrorView { last_pull_at: Some(at(10)), ..Default::default() };
        let item = mirrored(&binding(), &view, &issue("COR-12", "In Progress", StateCategory::Started), at(10) + TimeDelta::hours(3));
        assert_eq!(item["origin"], "tracker");
        assert_eq!(item["provider"], "linear");
        assert_eq!(item["external_key"], "COR-12");
        assert_eq!(item["ticket_id"], "COR-12");
        assert_eq!(item["project"], "claims");
        assert_eq!(item["state"], "In Progress");
        assert_eq!(item["state_category"], "started");
        assert_eq!(item["parent_key"], "COR-5");
        assert_eq!(item["relations"], json!([{"kind": "blocks", "key": "COR-14"}]));
        assert_eq!(item["url"], "https://example.com/COR-12");
        assert_eq!(item["last_pulled_at"], json!(at(10)));
        assert_eq!(item["last_pulled_age"], "3 hours ago");
        let never = mirrored(&binding(), &MirrorView::default(), &issue("COR-12", "Todo", StateCategory::Unstarted), at(10));
        assert_eq!(never["last_pulled_age"], "never");
        assert!(never["last_pulled_at"].is_null());
    }

    #[test]
    fn keys_sort_by_team_then_number() {
        let mut keys = vec!["COR-10", "COR-2", "ABC-3", "cor-1", "COR-x"];
        keys.sort_by(|a, b| key_order(a, b));
        assert_eq!(keys, vec!["ABC-3", "COR-x", "cor-1", "COR-2", "COR-10"]);
    }

    #[test]
    fn summary_drops_only_the_description() {
        let view = MirrorView::default();
        let open = issue("COR-12", "Todo", StateCategory::Unstarted);
        let full = mirrored(&binding(), &view, &open, at(10));
        let short = summary(&binding(), &view, &open, at(10));
        assert_eq!(full["description"], "Payer column");
        assert!(short.get("description").is_none());
        assert_eq!(short["url"], full["url"]);
    }

    #[test]
    fn native_items_keep_their_fields_and_gain_origin() {
        let value = native(&json!({"ticket_id": "SH-1", "source": "hank"}));
        assert_eq!(value, json!({"ticket_id": "SH-1", "source": "hank", "origin": "kanban"}));
    }

    #[test]
    fn list_filter_reads_state_priority_assignee_and_labels() {
        let started = issue("COR-12", "In Progress", StateCategory::Started);
        let keeps = |filter: ListFilter| filter.keeps(&started);
        assert!(keeps(ListFilter::default()));
        assert!(keeps(ListFilter { status: Some("in_progress"), ..Default::default() }));
        assert!(keeps(ListFilter { status: Some("started"), ..Default::default() }));
        assert!(!keeps(ListFilter { status: Some("backlog"), ..Default::default() }));
        assert!(keeps(ListFilter { priority: Some("high"), ..Default::default() }));
        assert!(!keeps(ListFilter { priority: Some("low"), ..Default::default() }));
        assert!(keeps(ListFilter { assignee: Some("jane doe"), ..Default::default() }));
        assert!(!keeps(ListFilter { assignee: Some("John Roe"), ..Default::default() }));
        assert!(keeps(ListFilter { tag: Some("Billing"), ..Default::default() }));
        assert!(!keeps(ListFilter { epic: Some("claims"), ..Default::default() }), "the mirror has no epics");
    }

    #[test]
    fn list_leaves_out_done_unless_asked_and_never_shows_removed_or_archived() {
        let done = issue("COR-1", "Done", StateCategory::Completed);
        assert!(!ListFilter::default().keeps(&done));
        assert!(ListFilter { include_done: true, ..Default::default() }.keeps(&done));
        let mut removed = issue("COR-2", "Todo", StateCategory::Unstarted);
        removed.removed_at = Some(at(11));
        let mut archived = issue("COR-3", "Todo", StateCategory::Unstarted);
        archived.issue.archived_at = Some(at(11));
        for gone in [&removed, &archived] {
            assert!(!ListFilter { include_done: true, ..Default::default() }.keeps(gone));
            assert!(!search_keeps(gone, "COR"));
            assert_eq!(query_keeps(gone, "recent", at(10)), Some(false));
        }
    }

    #[test]
    fn search_matches_key_title_and_description_ignoring_case() {
        let open = issue("COR-12", "Todo", StateCategory::Unstarted);
        assert!(search_keeps(&open, "cor-12"));
        assert!(search_keeps(&open, "WRONG PAYER"));
        assert!(search_keeps(&open, "payer column"));
        assert!(!search_keeps(&open, "COR-13"));
    }

    #[test]
    fn query_answers_recent_and_stale_only() {
        let open = issue("COR-12", "Todo", StateCategory::Unstarted);
        assert_eq!(query_keeps(&open, "recent", at(11)), Some(true));
        assert_eq!(query_keeps(&open, "recent", at(13)), Some(false));
        assert_eq!(query_keeps(&open, "stale", at(18)), Some(true));
        assert_eq!(query_keeps(&open, "stale", at(12)), Some(false));
        let done = issue("COR-1", "Done", StateCategory::Completed);
        assert_eq!(query_keeps(&done, "stale", at(30)), Some(false));
        assert_eq!(query_keeps(&open, "overdue", at(11)), None);
        assert_eq!(MIRROR_QUERIES, ["recent", "stale"]);
    }
}
