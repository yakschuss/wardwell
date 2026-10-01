//! Linear adapter: GraphQL pull of one team's issues, translated into
//! Wardwell tracker events. This is the anticorruption layer; Linear names
//! stop here except inside each event's `raw`.
//!
//! Does NOT write to Linear, decide the cursor, or touch the vault.

use crate::tracker::adapter::{Adapter, Sink};
use crate::tracker::events::{Common, Event, IssueSnapshot, Priority, Relation, RelationKind, StateCategory};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::time::Duration;

const PROVIDER: &str = "linear";
const ENDPOINT: &str = "https://api.linear.app/graphql";
const REQUEST_LIMIT: usize = 256 * 1024;
const RESPONSE_LIMIT: usize = 4 * 1024 * 1024;
/// The transport's error when a reply exceeds `RESPONSE_LIMIT`. The adapter
/// retries that page smaller instead of failing.
const RESPONSE_TOO_LARGE: &str = "Linear response exceeds 4 MiB";
/// Issues requested per page before any size retry.
const PAGE_SIZE: u64 = 25;
const TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound on pages per pull so a misbehaving cursor cannot loop forever.
const MAX_PAGES: usize = 2_000;

const ISSUES_QUERY: &str = r#"query WardwellTrackerPull($first: Int!, $after: String, $filter: IssueFilter) {
  issues(first: $first, after: $after, includeArchived: true, orderBy: updatedAt, filter: $filter) {
    pageInfo { hasNextPage endCursor }
    nodes {
      id identifier title description url priority createdAt updatedAt archivedAt branchName
      parent { identifier }
      relations(first: 50) { nodes { type relatedIssue { identifier } } }
      inverseRelations(first: 50) { nodes { type issue { identifier } } }
      state { name type }
      team { key }
      project { name }
      assignee { name }
      creator { name }
      labels(first: 50) { nodes { name } }
      comments(first: 100) { nodes { id body createdAt updatedAt user { name } } }
      history(first: 100) { nodes { id createdAt actor { name } fromState { name type } toState { name type } } }
      attachments(first: 50) { nodes { id title url createdAt updatedAt creator { name } } }
    }
  }
}"#;

/// Posts one GraphQL body and returns the decoded JSON response.
/// Exists so tests can inject canned responses.
pub trait Transport {
    /// Send `body` to the provider and return its decoded JSON reply.
    fn post(&self, body: &Value) -> Result<Value, String>;
}

/// Pulls one Linear team.
pub struct Linear<T: Transport> {
    transport: T,
    team: String,
}

impl<T: Transport> Linear<T> {
    /// An adapter for the Linear team with key `team`, sending through `transport`.
    pub fn new(transport: T, team: &str) -> Self {
        Self { transport, team: team.to_string() }
    }

    fn filter(&self, since: Option<DateTime<Utc>>, full: bool) -> Value {
        let mut filter = json!({"team": {"key": {"eq": self.team}}});
        if let (Some(since), false) = (since, full) {
            filter["updatedAt"] = json!({"gte": since.to_rfc3339()});
        }
        filter
    }

    /// One page after `after`, halving the page size down to one issue while
    /// the reply is over the response cap, so one fat page cannot wedge the pull.
    fn fetch_page(&self, after: Option<&str>, filter: &Value) -> Result<Value, String> {
        let mut first = PAGE_SIZE;
        loop {
            let body = json!({"query": ISSUES_QUERY, "variables": {"first": first, "after": after, "filter": filter}});
            match self.transport.post(&body) {
                Err(error) if error == RESPONSE_TOO_LARGE && first > 1 => first /= 2,
                result => return result,
            }
        }
    }
}

impl<T: Transport> Adapter for Linear<T> {
    fn pull(&self, since: Option<DateTime<Utc>>, full: bool, sink: &mut Sink<'_>) -> Result<(), String> {
        let filter = self.filter(since, full);
        let mut after: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let response = self.fetch_page(after.as_deref(), &filter)?;
            let page = issues_page(&response)?;
            let mut events = Vec::new();
            for node in page.nodes {
                events.extend(translate_issue(node)?);
            }
            sink(events)?;
            match page.next {
                Some(cursor) => after = Some(cursor),
                None => return Ok(()),
            }
        }
        Err(format!("Linear pagination exceeded {MAX_PAGES} pages"))
    }
}

struct Page<'a> {
    nodes: &'a [Value],
    next: Option<String>,
}

fn issues_page(response: &Value) -> Result<Page<'_>, String> {
    if let Some(errors) = response.get("errors").and_then(Value::as_array) {
        let message = errors
            .first()
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(format!("Linear returned an error: {}", truncate(message, 200)));
    }
    let issues = &response["data"]["issues"];
    let nodes = issues["nodes"].as_array().ok_or("Linear response has no issue list")?;
    let info = issues["pageInfo"].as_object().ok_or("Linear response has no page info")?;
    let has_next = info.get("hasNextPage").and_then(Value::as_bool).unwrap_or(false);
    let next = match (has_next, info.get("endCursor").and_then(Value::as_str)) {
        (false, _) => None,
        (true, Some(cursor)) => Some(cursor.to_string()),
        (true, None) => return Err("Linear reported another page without a cursor".to_string()),
    };
    Ok(Page { nodes, next })
}

fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn name_of(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(|v| text(v, "name"))
}

fn time(value: &Value, key: &str) -> Result<Option<DateTime<Utc>>, String> {
    match value.get(key).and_then(Value::as_str) {
        None => Ok(None),
        Some(raw) => DateTime::parse_from_rfc3339(raw)
            .map(|t| Some(t.with_timezone(&Utc)))
            .map_err(|_| format!("Linear returned an invalid {key}")),
    }
}

fn required_time(value: &Value, key: &str) -> Result<(DateTime<Utc>, String), String> {
    let parsed = time(value, key)?.ok_or_else(|| format!("Linear record is missing {key}"))?;
    Ok((parsed, text(value, key).unwrap_or_default()))
}

fn nodes<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value[key]["nodes"].as_array().map(Vec::as_slice).unwrap_or(&[])
}

/// The issue identity shared by every event translated from one node.
struct IssueRef {
    key: String,
    id: String,
    heading: String,
}

impl IssueRef {
    /// Fails when `entity` has no id: the event id would collide with any
    /// other id-less sibling updated at the same time.
    fn common(&self, kind: &str, entity: &Value, stamp: (DateTime<Utc>, String), actor: Option<String>, title: String) -> Result<Common, String> {
        let entity_id = text(entity, "id").ok_or_else(|| format!("Linear {kind} on {} is missing its id", self.key))?;
        Ok(Common {
            id: format!("{PROVIDER}:{kind}:{entity_id}:{}", stamp.1),
            provider: PROVIDER.to_string(),
            external_key: self.key.clone(),
            external_id: self.id.clone(),
            actor,
            occurred_at: stamp.0,
            title,
            raw: entity.clone(),
        })
    }
}

fn translate_issue(node: &Value) -> Result<Vec<Event>, String> {
    let key = text(node, "identifier").ok_or("Linear issue is missing its identifier")?;
    let issue_title = text(node, "title").unwrap_or_default();
    let issue = IssueRef {
        heading: format!("{key} {issue_title}"),
        id: text(node, "id").ok_or("Linear issue is missing its id")?,
        key,
    };
    let mut events = vec![snapshot_event(&issue, node, issue_title)?];
    for comment in nodes(node, "comments") {
        events.push(comment_event(&issue, comment)?);
    }
    for entry in nodes(node, "history") {
        if let Some(event) = state_event(&issue, entry)? {
            events.push(event);
        }
    }
    for attachment in nodes(node, "attachments") {
        events.push(link_event(&issue, attachment)?);
    }
    Ok(events)
}

fn snapshot_event(issue: &IssueRef, node: &Value, issue_title: String) -> Result<Event, String> {
    let state = name_of(node, "state").unwrap_or_default();
    let archived_at = time(node, "archivedAt")?;
    let snapshot = IssueSnapshot {
        issue_title,
        description: text(node, "description"),
        state_category: StateCategory::from_type(node["state"]["type"].as_str().unwrap_or_default()),
        state: state.clone(),
        priority: Priority::from_level(node["priority"].as_i64().unwrap_or(0)),
        team: node.get("team").and_then(|t| text(t, "key")),
        project: name_of(node, "project"),
        assignee: name_of(node, "assignee"),
        creator: name_of(node, "creator"),
        labels: nodes(node, "labels").iter().filter_map(|l| text(l, "name")).collect(),
        url: text(node, "url"),
        created_at: time(node, "createdAt")?,
        archived_at,
        parent_key: node.get("parent").and_then(|p| text(p, "identifier")),
        relations: relations(node),
        branch_name: text(node, "branchName"),
    };
    let heading = match &snapshot.parent_key {
        Some(parent) => format!("{} (sub-issue of {parent})", issue.heading),
        None => issue.heading.clone(),
    };
    let title = match archived_at {
        Some(_) => format!("{heading}: archived ({state})"),
        None => format!("{heading}: {state}"),
    };
    let mut raw = node.clone();
    if let Some(object) = raw.as_object_mut() {
        for child in ["comments", "history", "attachments"] {
            object.remove(child);
        }
    }
    let mut common = issue.common("issue", node, required_time(node, "updatedAt")?, None, title)?;
    // Linear does not bump updatedAt on archive or on a new relation, so the
    // archive time and a digest of the structure join the id. A snapshot with
    // neither keeps the id earlier logs hold, so they still dedup.
    if let Some(raw_archived) = text(node, "archivedAt") {
        common.id = format!("{}:{raw_archived}", common.id);
    }
    if let Some(digest) = snapshot.structure_digest() {
        common.id = format!("{}:{digest}", common.id);
    }
    common.raw = raw;
    Ok(Event::IssueUpserted { common, issue: Box::new(snapshot) })
}

/// Linear relations in Wardwell's kinds, sorted and deduplicated. `relations`
/// reads from this issue's side, `inverseRelations` from the other issue's.
/// Types with no Wardwell kind (similar, the duplicated-by side) stay in raw.
fn relations(node: &Value) -> Vec<Relation> {
    let outward = nodes(node, "relations").iter().filter_map(|r| {
        let kind = match r["type"].as_str()? {
            "related" => RelationKind::Related,
            "blocks" => RelationKind::Blocks,
            "duplicate" => RelationKind::DuplicateOf,
            _ => return None,
        };
        Some(Relation { kind, key: text(&r["relatedIssue"], "identifier")? })
    });
    let inward = nodes(node, "inverseRelations").iter().filter_map(|r| {
        let kind = match r["type"].as_str()? {
            "related" => RelationKind::Related,
            "blocks" => RelationKind::BlockedBy,
            _ => return None,
        };
        Some(Relation { kind, key: text(&r["issue"], "identifier")? })
    });
    let mut all: Vec<Relation> = outward.chain(inward).collect();
    all.sort();
    all.dedup();
    all
}

fn comment_event(issue: &IssueRef, comment: &Value) -> Result<Event, String> {
    let actor = name_of(comment, "user");
    let title = format!("{}: comment by {}", issue.heading, actor.as_deref().unwrap_or("someone"));
    Ok(Event::CommentUpserted {
        common: issue.common("comment", comment, required_time(comment, "updatedAt")?, actor, title)?,
        body: text(comment, "body").unwrap_or_default(),
    })
}

fn state_event(issue: &IssueRef, entry: &Value) -> Result<Option<Event>, String> {
    let Some(to) = name_of(entry, "toState") else {
        return Ok(None);
    };
    let from = name_of(entry, "fromState");
    let title = match &from {
        Some(from) => format!("{}: state {from} to {to}", issue.heading),
        None => format!("{}: state set to {to}", issue.heading),
    };
    let actor = name_of(entry, "actor");
    Ok(Some(Event::StateChanged {
        common: issue.common("history", entry, required_time(entry, "createdAt")?, actor, title)?,
        from,
        to,
    }))
}

fn link_event(issue: &IssueRef, attachment: &Value) -> Result<Event, String> {
    let url = text(attachment, "url").unwrap_or_default();
    let link_title = text(attachment, "title");
    let title = format!("{}: link {}", issue.heading, link_title.as_deref().unwrap_or(&url));
    let actor = name_of(attachment, "creator");
    Ok(Event::LinkAdded {
        common: issue.common("attachment", attachment, required_time(attachment, "updatedAt")?, actor, title)?,
        url,
        link_title,
    })
}

/// Bounded HTTPS transport to Linear. Personal API keys are sent raw in the
/// `Authorization` header. Never logs or echoes the token.
pub struct HttpTransport {
    token: String,
}

impl HttpTransport {
    /// A transport that authenticates with `token`.
    pub fn new(token: String) -> Self {
        Self { token }
    }
}

impl Transport for HttpTransport {
    fn post(&self, body: &Value) -> Result<Value, String> {
        let bytes = serde_json::to_vec(body).map_err(|_| "could not encode Linear request".to_string())?;
        if bytes.len() > REQUEST_LIMIT {
            return Err("Linear request exceeds 256 KiB".to_string());
        }
        // Run on a private thread and runtime so callers need not care
        // whether they are inside an async context.
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|_| "could not start the Linear HTTP runtime".to_string())?;
                    runtime.block_on(send(&self.token, bytes))
                })
                .join()
                .map_err(|_| "Linear request thread failed".to_string())?
        })
    }
}

async fn send(token: &str, body: Vec<u8>) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .build()
        .map_err(|_| "could not create the Linear HTTP client".to_string())?;
    let mut response = client
        .post(ENDPOINT)
        .header(reqwest::header::AUTHORIZATION, token)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json")
        .body(body)
        .send()
        .await
        .map_err(|_| "Linear request failed".to_string())?;
    let status = response.status();
    if status.is_redirection() {
        return Err("Linear redirects are not allowed".to_string());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| "could not read the Linear response".to_string())? {
        if bytes.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
            return Err(RESPONSE_TOO_LARGE.to_string());
        }
        bytes.extend_from_slice(&chunk);
    }
    let decoded: Option<Value> = serde_json::from_slice(&bytes).ok();
    match (status.is_success(), decoded) {
        (true, Some(value)) => Ok(value),
        (true, None) => Err("Linear returned invalid JSON".to_string()),
        // GraphQL errors arrive with 400; surface their message, not the body.
        (false, Some(value)) if value.get("errors").is_some() => Ok(value),
        (false, _) => Err(format!("Linear returned HTTP {}", status.as_u16())),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::tracker::adapter::Adapter;
    use crate::tracker::events::{Event, Priority, RelationKind, StateCategory};
    use chrono::TimeZone;
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    struct FakeTransport {
        responses: RefCell<VecDeque<Value>>,
        requests: RefCell<Vec<Value>>,
    }

    impl FakeTransport {
        fn new(responses: Vec<Value>) -> Self {
            Self { responses: RefCell::new(responses.into()), requests: RefCell::new(vec![]) }
        }
    }

    impl Transport for &FakeTransport {
        fn post(&self, body: &Value) -> Result<Value, String> {
            self.requests.borrow_mut().push(body.clone());
            self.responses.borrow_mut().pop_front().ok_or_else(|| "no canned response".to_string())
        }
    }

    fn issue(id: &str, key: &str, title: &str, updated: &str, archived: Option<&str>) -> Value {
        json!({
            "id": id, "identifier": key, "title": title,
            "description": "Payer column shows the secondary payer.",
            "url": format!("https://linear.app/corr/issue/{key}"),
            "priority": 2,
            "createdAt": "2026-08-01T10:00:00.000Z",
            "updatedAt": updated,
            "archivedAt": archived,
            "state": {"name": "In Progress", "type": "started"},
            "team": {"key": "COR"},
            "project": {"name": "Claims"},
            "assignee": {"name": "Jane Doe"},
            "creator": {"name": "John Roe"},
            "labels": {"nodes": [{"name": "billing"}]},
            "comments": {"nodes": [{
                "id": format!("{id}-c1"), "body": "Seen on the March batch too.",
                "createdAt": "2026-09-01T11:00:00.000Z", "updatedAt": "2026-09-01T11:00:00.000Z",
                "user": {"name": "Jane Doe"}
            }]},
            "history": {"nodes": [
                {"id": format!("{id}-h1"), "createdAt": "2026-09-01T12:00:00.000Z",
                 "actor": {"name": "Jane Doe"},
                 "fromState": {"name": "Todo", "type": "unstarted"},
                 "toState": {"name": "In Progress", "type": "started"}},
                {"id": format!("{id}-h2"), "createdAt": "2026-09-01T12:30:00.000Z",
                 "actor": {"name": "Jane Doe"}, "fromState": null, "toState": null}
            ]},
            "attachments": {"nodes": [{
                "id": format!("{id}-a1"), "title": "PR 42", "url": "https://github.com/corr/app/pull/42",
                "createdAt": "2026-09-01T13:00:00.000Z", "updatedAt": "2026-09-01T13:00:00.000Z",
                "creator": {"name": "John Roe"}
            }]}
        })
    }

    /// `issue` plus a parent, a branch, and relations in both directions,
    /// including link types Wardwell has no kind for.
    fn structured_issue(id: &str, key: &str, updated: &str) -> Value {
        let mut node = issue(id, key, "Claims inbox shows wrong payer", updated, None);
        node["parent"] = json!({"identifier": "COR-5"});
        node["branchName"] = json!("jane/cor-12-claims-inbox");
        node["relations"] = json!({"nodes": [
            {"type": "blocks", "relatedIssue": {"identifier": "COR-14"}},
            {"type": "duplicate", "relatedIssue": {"identifier": "COR-3"}},
            {"type": "related", "relatedIssue": {"identifier": "COR-20"}},
            {"type": "similar", "relatedIssue": {"identifier": "COR-21"}}
        ]});
        node["inverseRelations"] = json!({"nodes": [
            {"type": "blocks", "issue": {"identifier": "COR-9"}},
            {"type": "related", "issue": {"identifier": "COR-22"}},
            {"type": "duplicate", "issue": {"identifier": "COR-23"}}
        ]});
        node
    }

    fn page(nodes: Vec<Value>, next: Option<&str>) -> Value {
        json!({"data": {"issues": {
            "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next},
            "nodes": nodes
        }}})
    }

    /// Run a pull and gather every page's events in order.
    fn collect(adapter: &dyn Adapter, since: Option<DateTime<Utc>>, full: bool) -> Result<Vec<Event>, String> {
        let mut events = Vec::new();
        adapter.pull(since, full, &mut |page| {
            events.extend(page);
            Ok(())
        })?;
        Ok(events)
    }

    #[test]
    fn each_page_reaches_the_sink_before_a_later_page_fails() {
        let transport = FakeTransport::new(vec![page(
            vec![issue("i1", "COR-12", "Claims inbox shows wrong payer", "2026-09-01T14:00:00.000Z", None)],
            Some("cursor-1"),
        )]);
        let mut delivered: Vec<Vec<Event>> = Vec::new();
        let result = Linear::new(&transport, "COR").pull(None, false, &mut |page| {
            delivered.push(page);
            Ok(())
        });
        assert_eq!(result.unwrap_err(), "no canned response");
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].len(), 4);
    }

    #[test]
    fn follows_pagination_and_translates_every_kind() {
        let transport = FakeTransport::new(vec![
            page(vec![issue("i1", "COR-12", "Claims inbox shows wrong payer", "2026-09-01T14:00:00.000Z", None)], Some("cursor-1")),
            page(vec![issue("i2", "COR-13", "Old export", "2026-09-02T09:00:00.000Z", Some("2026-09-02T09:00:00.000Z"))], None),
        ]);
        let adapter = Linear::new(&transport, "COR");
        let events = collect(&adapter, None, false).unwrap();

        let requests = transport.requests.borrow();
        assert_eq!(requests.len(), 2);
        assert!(requests[0]["variables"]["after"].is_null());
        assert_eq!(requests[1]["variables"]["after"], "cursor-1");
        assert_eq!(requests[0]["variables"]["filter"], json!({"team": {"key": {"eq": "COR"}}}));
        assert!(requests[0]["query"].as_str().unwrap().contains("includeArchived: true"));

        // 1 snapshot + 1 comment + 1 state change + 1 link per issue; the
        // history entry without a state transition is not an event.
        assert_eq!(events.len(), 8);
        let Event::IssueUpserted { common, issue } = &events[0] else { panic!("snapshot first") };
        assert_eq!(common.provider, "linear");
        assert_eq!(common.external_key, "COR-12");
        assert_eq!(common.external_id, "i1");
        assert_eq!(common.id, "linear:issue:i1:2026-09-01T14:00:00.000Z");
        assert_eq!(common.occurred_at, Utc.with_ymd_and_hms(2026, 9, 1, 14, 0, 0).unwrap());
        assert_eq!(common.title, "COR-12 Claims inbox shows wrong payer: In Progress");
        assert_eq!(issue.state_category, StateCategory::Started);
        assert_eq!(issue.priority, Priority::High);
        assert_eq!(issue.labels, vec!["billing".to_string()]);
        assert_eq!(issue.assignee.as_deref(), Some("Jane Doe"));
        assert_eq!(issue.project.as_deref(), Some("Claims"));
        assert!(common.raw.get("comments").is_none(), "children archived in their own events");
        assert_eq!(common.raw["identifier"], "COR-12");

        let titles: Vec<&str> = events.iter().map(|e| e.common().title.as_str()).collect();
        assert!(titles.contains(&"COR-12 Claims inbox shows wrong payer: state Todo to In Progress"));
        assert!(titles.contains(&"COR-12 Claims inbox shows wrong payer: comment by Jane Doe"));
        assert!(titles.contains(&"COR-12 Claims inbox shows wrong payer: link PR 42"));

        let state = events.iter().find_map(|e| match e {
            Event::StateChanged { common, from, to } => Some((common, from, to)),
            _ => None,
        }).unwrap();
        assert_eq!(state.0.actor.as_deref(), Some("Jane Doe"));
        assert_eq!(state.1.as_deref(), Some("Todo"));
        assert_eq!(state.2, "In Progress");
        assert_eq!(state.0.id, "linear:history:i1-h1:2026-09-01T12:00:00.000Z");

        let comment = events.iter().find_map(|e| match e {
            Event::CommentUpserted { common, body } => Some((common, body)),
            _ => None,
        }).unwrap();
        assert_eq!(comment.1, "Seen on the March batch too.");
        assert_eq!(comment.0.raw["id"], "i1-c1");
    }

    #[test]
    fn query_requests_parent_branch_and_both_relation_directions() {
        for field in ["parent { identifier }", "branchName", "relations(", "relatedIssue { identifier }", "inverseRelations(", "issue { identifier }"] {
            assert!(ISSUES_QUERY.contains(field), "{field}");
        }
    }

    #[test]
    fn snapshot_carries_parent_branch_and_known_relations() {
        let transport = FakeTransport::new(vec![page(vec![structured_issue("i1", "COR-12", "2026-09-01T14:00:00.000Z")], None)]);
        let events = collect(&Linear::new(&transport, "COR"), None, false).unwrap();
        let Event::IssueUpserted { common, issue } = &events[0] else { panic!("snapshot first") };
        assert_eq!(issue.parent_key.as_deref(), Some("COR-5"));
        assert_eq!(issue.branch_name.as_deref(), Some("jane/cor-12-claims-inbox"));
        let relations: Vec<(RelationKind, &str)> = issue.relations.iter().map(|r| (r.kind, r.key.as_str())).collect();
        assert_eq!(relations, vec![
            (RelationKind::Related, "COR-20"),
            (RelationKind::Related, "COR-22"),
            (RelationKind::Blocks, "COR-14"),
            (RelationKind::BlockedBy, "COR-9"),
            (RelationKind::DuplicateOf, "COR-3"),
        ]);
        assert_eq!(issue.labels, vec!["billing".to_string()]);
        assert_eq!(issue.project.as_deref(), Some("Claims"));
        assert_eq!(issue.assignee.as_deref(), Some("Jane Doe"));
        assert_eq!(issue.priority, Priority::High);
        assert_eq!(issue.url.as_deref(), Some("https://linear.app/corr/issue/COR-12"));
        assert_eq!(common.title, "COR-12 Claims inbox shows wrong payer (sub-issue of COR-5): In Progress");
        let digest = issue.structure_digest().unwrap();
        assert_eq!(common.id, format!("linear:issue:i1:2026-09-01T14:00:00.000Z:{digest}"));
        let raw = common.raw.to_string();
        assert!(raw.contains("similar") && raw.contains("COR-21") && raw.contains("COR-23"), "unknown types stay in raw");
    }

    #[test]
    fn archived_sub_issue_title_names_the_parent() {
        let mut node = structured_issue("i1", "COR-12", "2026-09-01T14:00:00.000Z");
        node["archivedAt"] = json!("2026-09-02T09:00:00.000Z");
        let transport = FakeTransport::new(vec![page(vec![node], None)]);
        let events = collect(&Linear::new(&transport, "COR"), None, false).unwrap();
        let Event::IssueUpserted { common, issue } = &events[0] else { panic!("snapshot first") };
        assert_eq!(common.title, "COR-12 Claims inbox shows wrong payer (sub-issue of COR-5): archived (In Progress)");
        let digest = issue.structure_digest().unwrap();
        assert_eq!(common.id, format!("linear:issue:i1:2026-09-01T14:00:00.000Z:2026-09-02T09:00:00.000Z:{digest}"));
    }

    #[test]
    fn structure_change_without_an_updated_at_bump_appends_a_new_snapshot() {
        use crate::config::TrackerBinding;
        use crate::tracker::pull::pull_project;
        let updated = "2026-09-01T14:00:00.000Z";
        let first = structured_issue("i1", "COR-12", updated);
        let mut reparented = first.clone();
        reparented["parent"] = json!({"identifier": "COR-6"});
        let mut related = reparented.clone();
        related["relations"]["nodes"].as_array_mut().unwrap().push(json!({"type": "blocks", "relatedIssue": {"identifier": "COR-30"}}));
        let transport = FakeTransport::new(vec![
            page(vec![first], None),
            page(vec![reparented.clone()], None),
            page(vec![related], None),
            page(vec![reparented], None),
        ]);
        let adapter = Linear::new(&transport, "COR");
        let binding = TrackerBinding {
            domain: "work".into(),
            project: "claims".into(),
            provider: "linear".into(),
            team: "COR".into(),
            credential: "corr-linear".into(),
            readonly: true,
        };
        let vault = tempfile::tempdir().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 9, 2, 10, 0, 0).unwrap();
        assert_eq!(pull_project(vault.path(), &binding, &adapter, false, now).unwrap().appended, 4);
        assert_eq!(pull_project(vault.path(), &binding, &adapter, false, now).unwrap().appended, 1, "re-parent");
        assert_eq!(pull_project(vault.path(), &binding, &adapter, false, now).unwrap().appended, 1, "new relation");
        assert_eq!(pull_project(vault.path(), &binding, &adapter, false, now).unwrap().appended, 0, "a structure already seen dedups");
    }

    #[test]
    fn issue_without_parent_relations_or_branch_keeps_its_legacy_id() {
        let transport = FakeTransport::new(vec![page(vec![issue("i1", "COR-12", "T", "2026-09-01T14:00:00.000Z", None)], None)]);
        let events = collect(&Linear::new(&transport, "COR"), None, false).unwrap();
        assert_eq!(events[0].common().id, "linear:issue:i1:2026-09-01T14:00:00.000Z");
        assert_eq!(events[0].common().title, "COR-12 T: In Progress");
    }

    #[test]
    fn archived_issue_is_a_snapshot_with_archived_at() {
        let transport = FakeTransport::new(vec![page(
            vec![issue("i2", "COR-13", "Old export", "2026-09-02T09:00:00.000Z", Some("2026-09-02T09:00:00.000Z"))],
            None,
        )]);
        let events = collect(&Linear::new(&transport, "COR"), None, true).unwrap();
        let Event::IssueUpserted { common, issue } = &events[0] else { panic!("snapshot") };
        assert_eq!(issue.archived_at, Some(Utc.with_ymd_and_hms(2026, 9, 2, 9, 0, 0).unwrap()));
        assert_eq!(common.title, "COR-13 Old export: archived (In Progress)");
    }

    #[test]
    fn archive_without_an_updated_at_bump_appends_one_new_upsert() {
        use crate::config::TrackerBinding;
        use crate::tracker::{log, pull::pull_project};
        let (updated, archived) = ("2026-09-02T09:00:00.000Z", "2026-09-02T09:02:00.000Z");
        let transport = FakeTransport::new(vec![
            page(vec![issue("i2", "COR-13", "Old export", updated, None)], None),
            page(vec![issue("i2", "COR-13", "Old export", updated, Some(archived))], None),
            page(vec![issue("i2", "COR-13", "Old export", updated, Some(archived))], None),
        ]);
        let adapter = Linear::new(&transport, "COR");
        let binding = TrackerBinding {
            domain: "work".into(),
            project: "claims".into(),
            provider: "linear".into(),
            team: "COR".into(),
            credential: "corr-linear".into(),
            readonly: true,
        };
        let vault = tempfile::tempdir().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 9, 2, 10, 0, 0).unwrap();
        let first = pull_project(vault.path(), &binding, &adapter, false, now).unwrap();
        assert_eq!(first.appended, 4, "snapshot, comment, state change, link");
        let second = pull_project(vault.path(), &binding, &adapter, false, now).unwrap();
        assert_eq!(second.appended, 1, "only the archived snapshot is new");
        let third = pull_project(vault.path(), &binding, &adapter, false, now).unwrap();
        assert_eq!(third.appended, 0);

        let content = std::fs::read_to_string(log::path_for(vault.path(), "work", "claims")).unwrap();
        let upserts: Vec<Event> = content
            .lines()
            .filter_map(|line| serde_json::from_str::<Event>(line).ok())
            .filter(|event| matches!(event, Event::IssueUpserted { .. }))
            .collect();
        assert_eq!(upserts.len(), 2);
        let archived_at: Vec<_> = upserts
            .iter()
            .map(|event| match event {
                Event::IssueUpserted { issue, .. } => issue.archived_at,
                _ => None,
            })
            .collect();
        assert_eq!(archived_at, vec![None, Some(Utc.with_ymd_and_hms(2026, 9, 2, 9, 2, 0).unwrap())]);
        assert_eq!(upserts[0].common().id, format!("linear:issue:i2:{updated}"), "unarchived ids are unchanged");
    }

    #[test]
    fn full_pull_keeps_an_archived_issue_present() {
        use crate::config::TrackerBinding;
        use crate::tracker::pull::pull_project;
        let transport = FakeTransport::new(vec![
            page(vec![issue("i2", "COR-13", "Old export", "2026-09-02T09:00:00.000Z", None)], None),
            page(vec![issue("i2", "COR-13", "Old export", "2026-09-02T09:00:00.000Z", Some("2026-09-02T09:02:00.000Z"))], None),
        ]);
        let adapter = Linear::new(&transport, "COR");
        let binding = TrackerBinding {
            domain: "work".into(),
            project: "claims".into(),
            provider: "linear".into(),
            team: "COR".into(),
            credential: "corr-linear".into(),
            readonly: true,
        };
        let vault = tempfile::tempdir().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 9, 2, 10, 0, 0).unwrap();
        pull_project(vault.path(), &binding, &adapter, true, now).unwrap();
        let again = pull_project(vault.path(), &binding, &adapter, true, now).unwrap();
        assert_eq!(again.removed, 0);
        let content = std::fs::read_to_string(crate::tracker::log::path_for(vault.path(), "work", "claims")).unwrap();
        assert!(!content.contains("issue_removed"), "{content}");
    }

    #[test]
    fn incremental_pull_filters_by_updated_at_unless_full() {
        let since = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        let transport = FakeTransport::new(vec![page(vec![], None), page(vec![], None)]);
        let adapter = Linear::new(&transport, "COR");
        collect(&adapter, Some(since), false).unwrap();
        collect(&adapter, Some(since), true).unwrap();
        let requests = transport.requests.borrow();
        assert_eq!(
            requests[0]["variables"]["filter"]["updatedAt"],
            json!({"gte": "2026-09-01T00:00:00+00:00"})
        );
        assert!(requests[1]["variables"]["filter"].get("updatedAt").is_none());
    }

    /// Reports the response-size error while the requested page size is
    /// above `fits`, then serves one page of `issues`.
    struct SizeLimitedTransport {
        fits: u64,
        requests: RefCell<Vec<u64>>,
    }

    impl Transport for &SizeLimitedTransport {
        fn post(&self, body: &Value) -> Result<Value, String> {
            let first = body["variables"]["first"].as_u64().unwrap();
            self.requests.borrow_mut().push(first);
            match first > self.fits {
                true => Err(RESPONSE_TOO_LARGE.to_string()),
                false => Ok(page(vec![issue("i1", "COR-12", "Big issue", "2026-09-01T14:00:00.000Z", None)], None)),
            }
        }
    }

    #[test]
    fn oversized_page_is_retried_at_half_size_until_it_fits() {
        let transport = SizeLimitedTransport { fits: 3, requests: RefCell::new(vec![]) };
        let events = collect(&Linear::new(&transport, "COR"), None, false).unwrap();
        assert_eq!(*transport.requests.borrow(), vec![25, 12, 6, 3]);
        assert_eq!(events[0].common().external_key, "COR-12");
    }

    #[test]
    fn page_that_is_too_large_even_at_one_issue_fails() {
        let transport = SizeLimitedTransport { fits: 0, requests: RefCell::new(vec![]) };
        let error = collect(&Linear::new(&transport, "COR"), None, false).unwrap_err();
        assert_eq!(error, RESPONSE_TOO_LARGE);
        assert_eq!(*transport.requests.borrow(), vec![25, 12, 6, 3, 1]);
    }

    #[test]
    fn child_without_an_id_fails_the_pull_like_an_issue_without_one() {
        for child in ["comments", "history", "attachments"] {
            let mut node = issue("i1", "COR-12", "Claims inbox", "2026-09-01T14:00:00.000Z", None);
            node[child]["nodes"][0].as_object_mut().unwrap().remove("id");
            let transport = FakeTransport::new(vec![page(vec![node], None)]);
            let error = collect(&Linear::new(&transport, "COR"), None, false).unwrap_err();
            assert!(error.contains("missing its id"), "{child}: {error}");
        }
    }

    #[test]
    fn graphql_errors_fail_the_pull() {
        let transport = FakeTransport::new(vec![json!({"errors": [{"message": "Team not found"}]})]);
        let error = collect(&Linear::new(&transport, "NOPE"), None, false).unwrap_err();
        assert!(error.contains("Team not found"), "{error}");
    }

    #[test]
    fn malformed_page_fails_instead_of_looping() {
        let transport = FakeTransport::new(vec![json!({"data": {"issues": {"nodes": []}}})]);
        assert!(collect(&Linear::new(&transport, "COR"), None, false).is_err());
    }

    #[test]
    fn oversized_request_is_rejected_before_any_connection() {
        let transport = HttpTransport::new("token".into());
        let body = json!({"query": "x".repeat(REQUEST_LIMIT + 1)});
        assert_eq!(transport.post(&body).unwrap_err(), "Linear request exceeds 256 KiB");
    }
}
