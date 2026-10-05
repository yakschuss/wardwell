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
        gate: false,
        repository: None,
    }
}

/// A server with kanban on, `work/claims` bound to a mirror, one native
/// ticket in that project, and the given mirror events on disk.
pub(super) fn fixture(events: &[Event], readonly: bool) -> Fixture {
    fixture_with(events, readonly, Some(true))
}

/// `fixture` with the kanban setting of `work/claims`: `Some(true)` keeps the
/// native board readable beside a readonly binding, None leaves it to the
/// precedence, `Some(false)` turns it off.
pub(super) fn fixture_with(events: &[Event], readonly: bool, kanban_setting: Option<bool>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    std::fs::create_dir_all(vault.join("work/claims")).unwrap();
    let kanban = crate::kanban::store::KanbanStore::open(&dir.path().join("kanban.db"), vault.clone()).unwrap();
    kanban
        .create_item("Native claims task", "claims", "work", None, None, None, None, None, Some("hank"), None, None, None, &std::collections::HashMap::new())
        .unwrap();
    write_mirror(&vault, events);
    let index = Arc::new(crate::index::store::IndexStore::open(&dir.path().join("index.db")).unwrap());
    let trackers = vec![binding(readonly)];
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
        projects: std::iter::once((
            "work/claims".to_string(),
            crate::config::loader::ProjectMapping { domain: "work".into(), project: "claims".into(), paths: vec![], kanban: kanban_setting },
        ))
        .collect(),
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

/// 200 open backlog issues where a higher key number is a newer update,
/// as in a real tracker.
fn newest_highest_200() -> Vec<Event> {
    let now = chrono::Utc::now();
    let mut events: Vec<Event> = (0..200i64)
        .map(|n| {
            let mut e = snapshot(&format!("COR-{n}"), "A typical backlog issue title here", "Backlog", StateCategory::Backlog, now - chrono::TimeDelta::days(9) + chrono::TimeDelta::minutes(n));
            if let Event::IssueUpserted { issue, .. } = &mut e {
                issue.description = Some("d".repeat(1500));
            }
            e
        })
        .collect();
    events.push(pulled(now));
    events
}

#[test]
fn list_and_query_keep_the_fifty_most_recently_updated_and_drop_descriptions() {
    let f = fixture(&newest_highest_200(), true);
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
        assert_eq!(&keys[..3], ["COR-199", "COR-198", "COR-197"], "{args}");
        assert_eq!(keys[49], "COR-150", "{args}");
        assert!(mirrored.iter().all(|i| i.get("description").is_none()), "{args}");
    }
    let get = kanban(&f.server, json!({"action": "get", "ticket_id": "COR-7"}));
    assert_eq!(get["item"]["description"].as_str().unwrap().len(), 1500, "get keeps the description");
}

#[test]
fn equal_update_times_fall_back_to_key_order() {
    let f = fixture(&backlog_of_200(), true);
    let response = kanban(&f.server, json!({"action": "list", "project": "claims"}));
    let keys = keys(&response);
    assert_eq!(&keys[1..4], ["COR-0", "COR-1", "COR-2"], "{:?}", &keys[..5]);
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
    Arc::get_mut(&mut f.server.config).unwrap().trackers.push(other);

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
    let trackers = vec![binding(true)];
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
        projects: Default::default(),
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

/// Every file under `dir` with its bytes.
fn files(dir: &std::path::Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    let mut found = std::collections::BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(&next).unwrap() {
            let path = entry.unwrap().path();
            match path.is_dir() {
                true => pending.push(path),
                false => {
                    found.insert(path.clone(), std::fs::read(&path).unwrap());
                }
            }
        }
    }
    found
}

pub(super) fn dispatch(server: &WardwellServer, args: Value) -> Value {
    let params: KanbanParams = serde_json::from_value(args).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let raw = runtime.block_on(server.wardwell_kanban(Parameters(params)));
    serde_json::from_str(&raw).unwrap()
}

#[test]
fn a_write_to_a_mirrored_key_is_refused_and_writes_nothing() {
    for readonly in [true, false] {
        let f = fixture(&standard_mirror(), readonly);
        let before = files(&f._dir.path().join("vault"));
        for args in [
            json!({"action": "move", "ticket_id": "COR-12", "status": "done"}),
            json!({"action": "note", "ticket_id": "cor-12", "text": "hi"}),
            json!({"action": "update", "ticket_id": "COR-12", "title": "x"}),
            json!({"action": "groom", "ticket_id": "COR-12"}),
            json!({"action": "relationship_create", "from_ticket_id": "CL-1", "to_ticket_id": "COR-12", "relationship_type": "blocks"}),
        ] {
            let response = dispatch(&f.server, args.clone());
            let error = response["error"].as_str().unwrap_or_default();
            assert!(error.contains("COR-12 is mirrored from Linear team COR"), "readonly={readonly} {args}: {response}");
            assert!(error.contains("Edit it in Linear"), "{error}");
        }
        assert_eq!(files(&f._dir.path().join("vault")), before, "readonly={readonly}");
        let native = dispatch(&f.server, json!({"action": "note", "ticket_id": "CL-1", "text": "still writable"}));
        match readonly {
            true => assert!(native["error"].as_str().unwrap().contains("read-only mirror"), "{native}"),
            false => assert!(native.get("error").is_none(), "{native}"),
        }
    }
}

#[test]
fn get_returns_removed_and_archived_issues_with_their_flags() {
    let now = chrono::Utc::now();
    let hour = chrono::TimeDelta::hours(1);
    let mut archived = snapshot("COR-20", "Archived", "Done", StateCategory::Completed, now - hour * 3);
    if let Event::IssueUpserted { issue, .. } = &mut archived {
        issue.archived_at = Some(now - hour * 3);
    }
    let events = vec![
        archived,
        snapshot("COR-21", "Gone", "Todo", StateCategory::Unstarted, now - hour * 4),
        Event::IssueRemoved { common: common("wardwell:issue_removed:COR-21", "COR-21", now - hour * 2) },
        pulled(now - hour),
    ];
    let (f, calls) = refresh_fixture_over(&events, vec![], None);
    let archived = kanban(&f.server, json!({"action": "get", "ticket_id": "COR-20"}));
    assert!(archived["item"]["archived_at"].is_string(), "{archived}");
    assert_eq!(archived["refreshed"], false);
    let removed = kanban(&f.server, json!({"action": "get", "ticket_id": "cor-21"}));
    assert!(removed["item"]["removed_at"].is_string(), "{removed}");
    assert_eq!(removed["item"]["ticket_id"], "COR-21");
    assert!(calls.lock().unwrap().is_empty(), "a hit never refreshes");
    let listed = kanban(&f.server, json!({"action": "list", "project": "claims", "include_done": true}));
    assert!(!keys(&listed).iter().any(|k| k == "COR-20" || k == "COR-21"), "{listed}");
}

#[test]
fn a_domain_scoped_session_sees_no_mirror_outside_its_domains() {
    let (mut f, calls) = refresh_fixture(vec![], None);
    f.server.allowed_domains = vec!["personal".into()];
    let listed = kanban(&f.server, json!({"action": "list"}));
    assert!(origins(&listed).iter().all(|o| o == "kanban"), "{listed}");
    let searched = kanban(&f.server, json!({"action": "search", "query": "payer"}));
    assert!(keys(&searched).is_empty(), "{searched}");
    let queried = kanban(&f.server, json!({"action": "query", "question": "recent"}));
    assert!(origins(&queried).iter().all(|o| o == "kanban"), "{queried}");
    let got = kanban(&f.server, json!({"action": "get", "ticket_id": "COR-12"}));
    assert!(got.get("item").is_none(), "{got}");
    assert_eq!(got["refresh_reason"], "no_binding");
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn get_while_the_project_lock_is_held_fails_fast_with_lock_busy() {
    let (f, calls) = refresh_fixture(vec![], None);
    let path = crate::tracker::log::path_for(&f.server.vault_root, "work", "claims");
    let _held = crate::tracker::lock::acquire(&path, crate::tracker::lock::TEST_FREE_WAIT).unwrap();
    let started = std::time::Instant::now();
    let response = kanban(&f.server, json!({"action": "get", "ticket_id": "COR-77"}));
    assert_eq!(response["refreshed"], false, "{response}");
    assert_eq!(response["refresh_reason"], "pull_failed:lock_busy");
    assert!(response["error"].is_string());
    assert!(started.elapsed() < std::time::Duration::from_secs(10), "waited {:?}", started.elapsed());
    assert!(calls.lock().unwrap().is_empty(), "the adapter is never asked while the lock is held");
}

/// Like `FakeAdapter`, but each pull takes `delay` before it delivers.
struct SlowAdapter {
    events: Vec<Event>,
    delay: std::time::Duration,
    calls: Arc<Mutex<Vec<bool>>>,
}

impl crate::tracker::adapter::Adapter for SlowAdapter {
    fn pull(&self, _: Option<chrono::DateTime<chrono::Utc>>, full: bool, sink: &mut crate::tracker::adapter::Sink<'_>) -> Result<(), String> {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push(full);
        }
        std::thread::sleep(self.delay);
        sink(self.events.clone())
    }
}

/// Three `get` calls for `key`, started 50 ms apart on their own threads,
/// against an adapter that takes 500 ms and returns `events`.
fn three_concurrent_gets(key: &'static str, events: Vec<Event>) -> (Vec<Value>, usize) {
    let (mut f, calls) = refresh_fixture(vec![], None);
    let recorded = calls.clone();
    f.server.tracker_connect = Arc::new(move |_, _| {
        Ok(Box::new(SlowAdapter { events: events.clone(), delay: std::time::Duration::from_millis(500), calls: recorded.clone() }) as Box<dyn crate::tracker::adapter::Adapter>)
    });
    let server = Arc::new(f.server);
    let handles: Vec<_> = (0..3u64)
        .map(|i| {
            let server = server.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(50 * i));
                kanban(&server, json!({"action": "get", "ticket_id": key}))
            })
        })
        .collect();
    let responses = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let pulls = calls.lock().unwrap().len();
    (responses, pulls)
}

#[test]
fn concurrent_misses_for_a_key_the_pull_brings_make_one_pull() {
    let now = chrono::Utc::now();
    let (responses, pulls) = three_concurrent_gets("COR-99", vec![snapshot("COR-99", "New", "Todo", StateCategory::Unstarted, now)]);
    assert_eq!(pulls, 1, "{responses:?}");
    assert!(responses.iter().all(|r| r["item"]["ticket_id"] == "COR-99"), "{responses:?}");
}

#[test]
fn concurrent_misses_for_a_key_still_missing_make_one_pull() {
    let (responses, pulls) = three_concurrent_gets("COR-98", vec![]);
    assert_eq!(pulls, 1, "{responses:?}");
    let reasons: Vec<&str> = responses.iter().map(|r| r["refresh_reason"].as_str().unwrap()).collect();
    assert_eq!(reasons.iter().filter(|r| **r == "still_missing").count(), 1, "{reasons:?}");
    assert_eq!(reasons.iter().filter(|r| **r == "cooldown").count(), 2, "{reasons:?}");
}

#[test]
fn a_mirror_only_project_keeps_its_mirror_when_the_team_key_is_the_prefix_it_would_derive() {
    let now = chrono::Utc::now();
    let mut f = fixture(&[], true);
    let existing = vec!["CL".to_string()];
    let derived = crate::kanban::prefix::resolve_prefix("intake", &std::collections::HashMap::new(), &existing).unwrap();
    let mut intake = binding(true);
    intake.project = "intake".into();
    intake.team = derived.clone();
    Arc::get_mut(&mut f.server.config).unwrap().trackers.push(intake);
    std::fs::create_dir_all(f.server.vault_root.join("work/intake")).unwrap();
    let path = crate::tracker::log::path_for(&f.server.vault_root, "work", "intake");
    let mut summary = crate::tracker::log::read(&path).unwrap();
    let key = format!("{derived}-5");
    crate::tracker::log::append_new(&path, &[snapshot(&key, "Mirrored five", "In Progress", StateCategory::Started, now), pulled(now)], &mut summary).unwrap();

    let listed = kanban(&f.server, json!({"action": "list", "project": "intake"}));
    assert!(listed.to_string().contains("Mirrored five"), "{listed}");
    assert!(listed["tracker_note"].is_null(), "{listed}");
    let got = kanban(&f.server, json!({"action": "get", "ticket_id": key, "project": "intake"}));
    assert_eq!(got["item"]["origin"], "tracker", "{got}");
}

#[test]
fn a_team_key_equal_to_the_derived_native_prefix_leaves_the_mirror_out_with_a_note() {
    let now = chrono::Utc::now();
    let mut f = fixture(&[], false);
    Arc::get_mut(&mut f.server.config).unwrap().trackers[0].team = "CL".into();
    write_mirror(&f.server.vault_root, &[snapshot("CL-5", "Mirrored five", "In Progress", StateCategory::Started, now), pulled(now)]);
    let sentence = "Tracker team key CL of work/claims equals the native kanban prefix CL of project claims. Set a different native prefix for claims in kanban.prefixes.";
    for args in [
        json!({"action": "list", "project": "claims"}),
        json!({"action": "search", "query": "Mirrored"}),
        json!({"action": "query", "question": "recent"}),
        json!({"action": "get", "ticket_id": "CL-5"}),
    ] {
        let response = kanban(&f.server, args.clone());
        assert!(response["tracker_note"].as_str().unwrap_or_default().contains(sentence), "{args}: {response}");
        assert!(!response.to_string().contains("Mirrored five"), "{args}: {response}");
    }
    let native = kanban(&f.server, json!({"action": "get", "ticket_id": "CL-1"}));
    assert_eq!(native["item"]["origin"], "kanban");
    let p: KanbanParams = serde_json::from_value(json!({"action": "move", "ticket_id": "CL-1", "status": "done"})).unwrap();
    assert!(f.server.tracker_refusal(f.server.kanban.as_ref().unwrap(), &p).is_none(), "the native ticket stays writable");
}

/// A change the GitHub mirror of `acme/app` saw merged at `at`, naming COR-12.
fn merged_change(at: chrono::DateTime<chrono::Utc>) -> Event {
    let mut common = common("github:acme/app#42", "acme/app#42", at);
    common.provider = "github".into();
    common.title = "acme/app#42 merged into main: COR-12 Fix the claims inbox".into();
    Event::ChangeMerged {
        common,
        change: Box::new(crate::tracker::events::MergedChange { number: 42, title: "COR-12 Fix the claims inbox".into(), merged_at: at, keys: vec!["COR-12".into()], ..Default::default() }),
    }
}

#[test]
fn the_kanban_read_path_ignores_merged_changes_and_github_markers() {
    let now = chrono::Utc::now();
    let (mut f, calls) = refresh_fixture(vec![], None);
    let mut github = binding(false);
    github.provider = "github".into();
    github.team = String::new();
    github.repository = Some("acme/app".into());
    Arc::get_mut(&mut f.server.config).unwrap().trackers.push(github);
    let mut github_pull = pulled(now);
    if let Event::PullCompleted { common, .. } = &mut github_pull {
        common.provider = "github".into();
        common.id = format!("wardwell:pull_completed:acme/app:{now}");
    }
    write_mirror(&f.server.vault_root, &[merged_change(now), github_pull]);

    let listed = kanban(&f.server, json!({"action": "list", "project": "claims"}));
    assert_eq!(keys(&listed), vec!["CL-1", "COR-12", "COR-14"], "{listed}");
    assert_eq!(listed["items"][1]["last_pulled_age"], "1 hour ago", "the github marker is not the issue mirror's pull: {listed}");
    let searched = kanban(&f.server, json!({"action": "search", "query": "claims inbox"}));
    assert!(!searched.to_string().contains("acme/app#42"), "{searched}");
    let queried = kanban(&f.server, json!({"action": "query", "question": "in_progress", "project": "claims"}));
    assert!(!queried.to_string().contains("acme/app#42"), "{queried}");

    let got = kanban(&f.server, json!({"action": "get", "ticket_id": "acme/app#42"}));
    assert!(got["item"].is_null(), "{got}");
    assert!(calls.lock().unwrap().is_empty(), "no refresh for a merged change: {got}");
}
