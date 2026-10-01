//! Kanban reads through the MCP handlers on a project bound to a tracker
//! mirror.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::config::loader::TrackerBinding;
use crate::tracker::events::{Common, Event, IssueSnapshot, Priority, StateCategory};
use serde_json::{Value, json};

pub(super) struct Fixture {
    /// Holds the vault and config dir alive for the test.
    pub _dir: tempfile::TempDir,
    pub server: WardwellServer,
}

pub(super) fn binding(readonly: bool) -> TrackerBinding {
    TrackerBinding {
        domain: "work".into(),
        project: "claims".into(),
        provider: "linear".into(),
        team: "COR".into(),
        credential: "corr-linear".into(),
        readonly,
    }
}

/// A server with kanban on, `work/claims` bound to a mirror, one native
/// ticket in that project, and the given mirror events on disk.
pub(super) fn fixture(events: &[Event], readonly: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    std::fs::create_dir_all(vault.join("work/claims")).unwrap();
    let kanban = crate::kanban::store::KanbanStore::open(&dir.path().join("kanban.db"), vault.clone()).unwrap();
    kanban
        .create_item("Native claims task", "claims", "work", None, None, None, None, None, Some("hank"), None, None, None, &std::collections::HashMap::new())
        .unwrap();
    write_mirror(&vault, events);
    let index = Arc::new(crate::index::store::IndexStore::open(&dir.path().join("index.db")).unwrap());
    let mut trackers = std::collections::BTreeMap::new();
    trackers.insert("work/claims".to_string(), binding(readonly));
    let config = crate::config::loader::WardwellConfig {
        vault_path: vault,
        registry: crate::domain::registry::DomainRegistry::from_domains(vec![]),
        session_sources: vec![],
        exclude: vec![],
        ai: Default::default(),
        stop_hook: true,
        kanban_enabled: true,
        kanban_queries: std::collections::HashMap::new(),
        kanban_prefixes: std::collections::HashMap::new(),
        features: Default::default(),
        trackers,
    };
    let server = WardwellServer::new(config, index, Arc::new(Mutex::new(None)), None, Some(kanban));
    Fixture { _dir: dir, server }
}

pub(super) fn write_mirror(vault: &std::path::Path, events: &[Event]) {
    let path = crate::tracker::log::path_for(vault, "work", "claims");
    let mut summary = crate::tracker::log::read(&path).unwrap();
    crate::tracker::log::append_new(&path, events, &mut summary).unwrap();
}

pub(super) fn common(id: &str, key: &str, at: chrono::DateTime<chrono::Utc>) -> Common {
    Common {
        id: id.into(),
        provider: "linear".into(),
        external_key: key.into(),
        external_id: format!("{key}-uuid"),
        actor: None,
        occurred_at: at,
        title: format!("{key} title"),
        raw: Value::Null,
    }
}

pub(super) fn snapshot(key: &str, title: &str, state: &str, category: StateCategory, at: chrono::DateTime<chrono::Utc>) -> Event {
    Event::IssueUpserted {
        common: common(&format!("linear:issue:{key}:{at}"), key, at),
        issue: Box::new(IssueSnapshot {
            issue_title: title.into(),
            state: state.into(),
            state_category: category,
            priority: Priority::High,
            labels: vec!["billing".into()],
            url: Some(format!("https://example.com/{key}")),
            parent_key: Some("COR-5".into()),
            ..Default::default()
        }),
    }
}

pub(super) fn pulled(at: chrono::DateTime<chrono::Utc>) -> Event {
    Event::PullCompleted { common: common(&format!("wardwell:pull_completed:COR:{at}"), "COR", at), through: Some(at) }
}

pub(super) fn standard_mirror() -> Vec<Event> {
    let now = chrono::Utc::now();
    let hour = chrono::TimeDelta::hours(1);
    vec![
        snapshot("COR-12", "Claims inbox shows wrong payer", "In Progress", StateCategory::Started, now - hour * 2),
        snapshot("COR-13", "Payer lookup times out", "Done", StateCategory::Completed, now - hour * 2),
        snapshot("COR-14", "Old work", "Todo", StateCategory::Unstarted, now - chrono::TimeDelta::days(10)),
        pulled(now - hour),
    ]
}

pub(super) fn kanban(server: &WardwellServer, args: Value) -> Value {
    let mut args = args;
    args["action"] = args.get("action").cloned().unwrap_or(json!("list"));
    let params: KanbanParams = serde_json::from_value(args).unwrap();
    let store = server.kanban.as_ref().unwrap();
    let raw = match params.action.as_str() {
        "list" => server.kanban_list(store, &params),
        "get" => server.kanban_get(store, &params),
        "query" => server.kanban_query(store, &params),
        "search" => server.kanban_search(store, &params),
        other => panic!("unexpected action {other}"),
    };
    serde_json::from_str(&raw).unwrap()
}

fn keys(response: &Value) -> Vec<String> {
    response["items"].as_array().unwrap().iter().map(|i| i["ticket_id"].as_str().unwrap().to_string()).collect()
}

fn origins(response: &Value) -> Vec<String> {
    response["items"].as_array().unwrap().iter().map(|i| i["origin"].as_str().unwrap().to_string()).collect()
}

#[test]
fn list_merges_open_mirrored_issues_with_native_items() {
    let f = fixture(&standard_mirror(), true);
    let response = kanban(&f.server, json!({"action": "list", "project": "claims"}));
    assert_eq!(keys(&response), vec!["CL-1", "COR-12", "COR-14"], "{response}");
    assert_eq!(origins(&response), vec!["kanban", "tracker", "tracker"]);
    assert_eq!(response["total"], 3);
    let native = &response["items"][0];
    assert_eq!(native["source"], "hank", "native items keep their own source field");
    let mirrored = &response["items"][1];
    assert_eq!(mirrored["provider"], "linear");
    assert_eq!(mirrored["external_key"], "COR-12");
    assert_eq!(mirrored["state"], "In Progress");
    assert_eq!(mirrored["state_category"], "started");
    assert_eq!(mirrored["parent_key"], "COR-5");
    assert_eq!(mirrored["url"], "https://example.com/COR-12");
    assert_eq!(mirrored["last_pulled_age"], "1 hour ago");
    assert!(mirrored["last_pulled_at"].is_string());

    let done = kanban(&f.server, json!({"action": "list", "project": "claims", "include_done": true}));
    assert!(keys(&done).contains(&"COR-13".to_string()));
    let started = kanban(&f.server, json!({"action": "list", "project": "claims", "status": "in_progress"}));
    assert_eq!(keys(&started), vec!["COR-12"]);
}

#[test]
fn an_unbound_project_lists_only_native_items() {
    let f = fixture(&standard_mirror(), true);
    let response = kanban(&f.server, json!({"action": "list", "project": "billing"}));
    assert!(keys(&response).is_empty(), "{response}");
}

#[test]
fn search_finds_mirrored_issues_by_key_and_title() {
    let f = fixture(&standard_mirror(), true);
    let by_key = kanban(&f.server, json!({"action": "search", "query": "COR-12"}));
    assert_eq!(keys(&by_key), vec!["COR-12"]);
    let by_title = kanban(&f.server, json!({"action": "search", "query": "payer", "project": "claims"}));
    assert_eq!(keys(&by_title), vec!["COR-12", "COR-13"]);
    let native = kanban(&f.server, json!({"action": "search", "query": "Native"}));
    assert_eq!(origins(&native), vec!["kanban"]);
}

#[test]
fn query_includes_mirrored_issues_it_can_answer_and_says_when_it_cannot() {
    let f = fixture(&standard_mirror(), true);
    let stale = kanban(&f.server, json!({"action": "query", "question": "stale", "project": "claims"}));
    assert_eq!(keys(&stale), vec!["COR-14"], "{stale}");
    assert!(stale.get("tracker_note").is_none());
    let recent = kanban(&f.server, json!({"action": "query", "question": "recent", "project": "claims"}));
    assert_eq!(keys(&recent), vec!["CL-1", "COR-12", "COR-13"]);
    let overdue = kanban(&f.server, json!({"action": "query", "question": "overdue", "project": "claims"}));
    assert_eq!(overdue["tracker_note"], crate::tracker::items::QUERY_NOT_MIRRORED);
}

#[test]
fn get_returns_native_and_mirrored_items_by_origin() {
    let f = fixture(&standard_mirror(), true);
    let native = kanban(&f.server, json!({"action": "get", "ticket_id": "CL-1"}));
    assert_eq!(native["item"]["origin"], "kanban");
    assert_eq!(native["item"]["title"], "Native claims task");
    assert!(native.get("refreshed").is_none());
    let mirrored = kanban(&f.server, json!({"action": "get", "ticket_id": "COR-13"}));
    assert_eq!(mirrored["item"]["origin"], "tracker");
    assert_eq!(mirrored["item"]["state"], "Done");
    assert_eq!(mirrored["refreshed"], false);
}

#[test]
fn writes_stay_refused_on_a_readonly_project() {
    let f = fixture(&standard_mirror(), true);
    let params: KanbanParams = serde_json::from_value(json!({"action": "create", "project": "claims", "domain": "work", "title": "x"})).unwrap();
    let refusal = f.server.tracker_refusal(f.server.kanban.as_ref().unwrap(), &params).unwrap();
    assert!(refusal.contains("read-only mirror of Linear team COR"), "{refusal}");
    let writable = fixture(&standard_mirror(), false);
    assert!(writable.server.tracker_refusal(writable.server.kanban.as_ref().unwrap(), &params).is_none());
}

/// Hands each pull `events`, or fails with `error`; counts pulls and
/// records whether each asked for a full pull.
struct FakeAdapter {
    events: Vec<Event>,
    error: Option<String>,
    calls: Arc<Mutex<Vec<bool>>>,
}

impl crate::tracker::adapter::Adapter for FakeAdapter {
    fn pull(&self, _: Option<chrono::DateTime<chrono::Utc>>, full: bool, sink: &mut crate::tracker::adapter::Sink<'_>) -> Result<(), String> {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push(full);
        }
        sink(self.events.clone())?;
        self.error.clone().map_or(Ok(()), Err)
    }
}

/// `fixture` with a saved credential and a fake adapter behind the
/// server's connect seam. Returns the recorded pulls.
fn refresh_fixture(events: Vec<Event>, error: Option<&str>) -> (Fixture, Arc<Mutex<Vec<bool>>>) {
    refresh_fixture_over(&standard_mirror(), events, error)
}

/// `refresh_fixture` over the given mirror events.
fn refresh_fixture_over(mirror: &[Event], events: Vec<Event>, error: Option<&str>) -> (Fixture, Arc<Mutex<Vec<bool>>>) {
    let mut f = fixture(mirror, true);
    let config_dir = f._dir.path().join("config");
    let path = crate::tracker::credential::path_in(&config_dir, "corr-linear").unwrap();
    crate::tracker::credential::save(&path, "t").unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = calls.clone();
    let error = error.map(str::to_string);
    f.server.tracker_config_dir = config_dir;
    f.server.tracker_connect = Arc::new(move |_, _| {
        Ok(Box::new(FakeAdapter { events: events.clone(), error: error.clone(), calls: recorded.clone() }) as Box<dyn crate::tracker::adapter::Adapter>)
    });
    (f, calls)
}

#[test]
fn get_on_a_miss_pulls_once_incrementally_and_finds_the_issue() {
    let now = chrono::Utc::now();
    let (f, calls) = refresh_fixture(vec![snapshot("COR-99", "New issue", "Todo", StateCategory::Unstarted, now)], None);
    let response = kanban(&f.server, json!({"action": "get", "ticket_id": "COR-99"}));
    assert_eq!(response["refreshed"], true, "{response}");
    assert_eq!(response["refresh_reason"], "found_after_pull");
    assert_eq!(response["item"]["origin"], "tracker");
    assert_eq!(response["item"]["title"], "New issue");
    assert_eq!(*calls.lock().unwrap(), vec![false], "one incremental pull, never full, though no full resync is on record");
}

#[test]
fn a_second_miss_within_the_cooldown_does_not_pull() {
    let (f, calls) = refresh_fixture(vec![], None);
    let first = kanban(&f.server, json!({"action": "get", "ticket_id": "COR-98"}));
    assert_eq!(first["refreshed"], true, "{first}");
    assert_eq!(first["refresh_reason"], "still_missing");
    assert!(first["error"].as_str().unwrap().contains("COR-98"));
    let second = kanban(&f.server, json!({"action": "get", "ticket_id": "COR-97"}));
    assert_eq!(second["refreshed"], false);
    assert_eq!(second["refresh_reason"], "cooldown");
    assert!(second["error"].is_string());
    assert_eq!(calls.lock().unwrap().len(), 1);
}

#[test]
fn a_failed_refresh_returns_the_miss_with_the_code() {
    let (f, _) = refresh_fixture(vec![], Some("Linear request failed"));
    let response = kanban(&f.server, json!({"action": "get", "ticket_id": "COR-96"}));
    assert_eq!(response["refreshed"], false, "{response}");
    assert_eq!(response["refresh_reason"], "pull_failed:provider");
    assert!(response["error"].is_string());
}

#[test]
fn a_key_no_binding_owns_does_not_pull() {
    let (f, calls) = refresh_fixture(vec![], None);
    let response = kanban(&f.server, json!({"action": "get", "ticket_id": "XYZ-1"}));
    assert_eq!(response["refreshed"], false);
    assert_eq!(response["refresh_reason"], "no_binding");
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn list_query_and_search_never_pull() {
    let (f, calls) = refresh_fixture(vec![], None);
    kanban(&f.server, json!({"action": "list", "project": "claims"}));
    kanban(&f.server, json!({"action": "search", "query": "COR-95"}));
    kanban(&f.server, json!({"action": "query", "question": "recent"}));
    assert!(calls.lock().unwrap().is_empty());
}

/// 200 open backlog issues with long descriptions, last updated 30 days ago.
fn backlog_of_200() -> Vec<Event> {
    let now = chrono::Utc::now();
    let mut events: Vec<Event> = (0..200)
        .map(|n| {
            let mut e = snapshot(&format!("COR-{n}"), "A typical backlog issue title here", "Backlog", StateCategory::Backlog, now - chrono::TimeDelta::days(30));
            if let Event::IssueUpserted { issue, .. } = &mut e {
                issue.description = Some("d".repeat(1500));
            }
            e
        })
        .collect();
    events.push(pulled(now));
    events
}

fn raw_kanban(server: &WardwellServer, args: Value) -> String {
    let params: KanbanParams = serde_json::from_value(args).unwrap();
    let store = server.kanban.as_ref().unwrap();
    match params.action.as_str() {
        "list" => server.kanban_list(store, &params),
        "query" => server.kanban_query(store, &params),
        "search" => server.kanban_search(store, &params),
        other => panic!("unexpected action {other}"),
    }
}

#[test]
fn list_and_query_cap_mirrored_items_sort_by_key_number_and_drop_descriptions() {
    let f = fixture(&backlog_of_200(), true);
    for args in [
        json!({"action": "list", "project": "claims"}),
        json!({"action": "list"}),
        json!({"action": "query", "question": "stale"}),
    ] {
        let raw = raw_kanban(&f.server, args.clone());
        assert!(raw.len() < 40 * 1024, "{args}: {} bytes", raw.len());
        let response: Value = serde_json::from_str(&raw).unwrap();
        let mirrored: Vec<&Value> = response["items"].as_array().unwrap().iter().filter(|i| i["origin"] == "tracker").collect();
        assert_eq!(mirrored.len(), 50, "{args}");
        assert_eq!(response["tracker_truncated"], 150, "{args}");
        let keys: Vec<&str> = mirrored.iter().map(|i| i["ticket_id"].as_str().unwrap()).collect();
        assert_eq!(&keys[..4], ["COR-0", "COR-1", "COR-2", "COR-3"], "{args}");
        assert_eq!(keys[10], "COR-10");
        assert!(mirrored.iter().all(|i| i.get("description").is_none()), "{args}");
    }
    let get = kanban(&f.server, json!({"action": "get", "ticket_id": "COR-7"}));
    assert_eq!(get["item"]["description"].as_str().unwrap().len(), 1500, "get keeps the description");
}

#[test]
fn a_small_mirror_reports_no_truncation() {
    let f = fixture(&standard_mirror(), true);
    let response = kanban(&f.server, json!({"action": "list", "project": "claims"}));
    assert!(response.get("tracker_truncated").is_none(), "{response}");
}

#[test]
fn search_caps_mirrored_items_at_twenty() {
    let f = fixture(&backlog_of_200(), true);
    let raw = raw_kanban(&f.server, json!({"action": "search", "query": "backlog"}));
    let response: Value = serde_json::from_str(&raw).unwrap();
    let keys = keys(&response);
    assert_eq!(keys.len(), 20);
    assert_eq!(&keys[..3], ["COR-0", "COR-1", "COR-2"]);
    assert_eq!(response["tracker_truncated"], 180);
    assert!(response["items"][0].get("description").is_none());
}

#[test]
fn a_miss_on_a_mirror_never_pulled_does_not_pull_the_whole_team() {
    let now = chrono::Utc::now();
    let never = vec![snapshot("COR-1", "Seen once", "Todo", StateCategory::Unstarted, now)];
    let (f, calls) = refresh_fixture_over(&never, vec![snapshot("COR-2", "x", "Todo", StateCategory::Unstarted, now)], None);
    let response = kanban(&f.server, json!({"action": "get", "ticket_id": "COR-2"}));
    assert_eq!(response["refreshed"], false, "{response}");
    assert_eq!(response["refresh_reason"], "never_pulled");
    assert!(calls.lock().unwrap().is_empty());
    let (empty, calls) = refresh_fixture_over(&[], vec![], None);
    let response = kanban(&empty.server, json!({"action": "get", "ticket_id": "COR-2"}));
    assert_eq!(response["refresh_reason"], "never_pulled");
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn a_named_project_refreshes_only_for_its_own_team_key() {
    let (f, calls) = refresh_fixture(vec![], None);
    let foreign = kanban(&f.server, json!({"action": "get", "ticket_id": "CL-999", "project": "claims"}));
    assert_eq!(foreign["refresh_reason"], "no_binding", "{foreign}");
    let bare = kanban(&f.server, json!({"action": "get", "ticket_id": "nohyphen", "project": "claims"}));
    assert_eq!(bare["refresh_reason"], "no_binding");
    assert!(calls.lock().unwrap().is_empty());
    let own = kanban(&f.server, json!({"action": "get", "ticket_id": "COR-999", "project": "claims"}));
    assert_eq!(own["refresh_reason"], "still_missing");
}

#[test]
fn domain_picks_between_two_projects_of_the_same_name() {
    let now = chrono::Utc::now();
    let (mut f, calls) = refresh_fixture(vec![snapshot("COR-99", "New", "Todo", StateCategory::Unstarted, now)], None);
    let vault = f.server.vault_root.clone();
    let personal = crate::tracker::log::path_for(&vault, "personal", "claims");
    let mut summary = crate::tracker::log::read(&personal).unwrap();
    crate::tracker::log::append_new(&personal, &[snapshot("COR-50", "Personal only", "Todo", StateCategory::Unstarted, now), pulled(now - chrono::TimeDelta::hours(1))], &mut summary).unwrap();
    let mut other = binding(true);
    other.domain = "personal".into();
    Arc::get_mut(&mut f.server.config).unwrap().trackers.insert("personal/claims".into(), other);

    let work_only = kanban(&f.server, json!({"action": "get", "ticket_id": "COR-50", "project": "claims", "domain": "work"}));
    assert_eq!(work_only["refresh_reason"], "still_missing", "{work_only}");
    let listed = kanban(&f.server, json!({"action": "list", "project": "claims", "domain": "personal"}));
    assert_eq!(keys(&listed).iter().filter(|k| k.starts_with("COR")).collect::<Vec<_>>(), vec!["COR-50"]);

    let refreshed = kanban(&f.server, json!({"action": "get", "ticket_id": "COR-99", "project": "claims", "domain": "personal"}));
    assert_eq!(refreshed["refresh_reason"], "found_after_pull", "{refreshed}");
    assert_eq!(refreshed["item"]["domain"], "personal");
    assert_eq!(calls.lock().unwrap().len(), 2);
    assert!(crate::tracker::view::MirrorView::read(&personal).unwrap().get("COR-99").is_some());
}

#[test]
fn two_servers_on_one_vault_share_the_cooldown_through_the_log() {
    let (first, calls) = refresh_fixture(vec![], None);
    let mut trackers = std::collections::BTreeMap::new();
    trackers.insert("work/claims".to_string(), binding(true));
    let config = crate::config::loader::WardwellConfig {
        vault_path: first.server.vault_root.clone(),
        registry: crate::domain::registry::DomainRegistry::from_domains(vec![]),
        session_sources: vec![],
        exclude: vec![],
        ai: Default::default(),
        stop_hook: true,
        kanban_enabled: true,
        kanban_queries: std::collections::HashMap::new(),
        kanban_prefixes: std::collections::HashMap::new(),
        features: Default::default(),
        trackers,
    };
    let mut second = WardwellServer::new(
        config,
        Arc::new(crate::index::store::IndexStore::open(&first._dir.path().join("index2.db")).unwrap()),
        Arc::new(Mutex::new(None)),
        None,
        Some(crate::kanban::store::KanbanStore::open(&first._dir.path().join("kanban2.db"), first.server.vault_root.clone()).unwrap()),
    );
    second.tracker_config_dir = first.server.tracker_config_dir.clone();
    second.tracker_connect = first.server.tracker_connect.clone();
    let a = kanban(&first.server, json!({"action": "get", "ticket_id": "COR-404"}));
    assert_eq!(a["refresh_reason"], "still_missing", "{a}");
    let b = kanban(&second, json!({"action": "get", "ticket_id": "COR-404"}));
    assert_eq!(b["refresh_reason"], "cooldown", "{b}");
    assert_eq!(calls.lock().unwrap().len(), 1);
}

#[test]
fn a_recent_failed_pull_also_cools_the_binding_down() {
    let now = chrono::Utc::now();
    let mut mirror = standard_mirror();
    mirror.push(Event::PullFailed { common: common("wardwell:pull_failed:COR:x", "COR", now - chrono::TimeDelta::seconds(10)), code: crate::tracker::events::FailureCode::Provider, automatic_full: false });
    let (f, calls) = refresh_fixture_over(&mirror, vec![], None);
    let response = kanban(&f.server, json!({"action": "get", "ticket_id": "COR-404"}));
    assert_eq!(response["refresh_reason"], "cooldown", "{response}");
    assert!(calls.lock().unwrap().is_empty());
}
