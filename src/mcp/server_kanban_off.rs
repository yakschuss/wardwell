//! The per-project kanban switch in the MCP tool: refuses the native board of
//! a project whose kanban is off, hides its native items from list, query and
//! search, and hides its native kanban files from text search. The tracker
//! mirror keeps answering.
//!
//! Does NOT decide whether a project is off (`WardwellConfig::kanban_on`
//! does) or write any setting.

use super::*;
use crate::kanban::store::{KanbanItem, KanbanStore};

impl WardwellServer {
    /// Refusal when the action reaches the native board of an off project.
    /// `list`, `query` and `search` never refuse: they drop the native items
    /// and still return the mirror. `get` refuses only for a native ticket.
    pub(super) fn off_refusal(&self, kanban: &KanbanStore, p: &KanbanParams) -> Option<String> {
        let targets = match p.action.as_str() {
            "list" | "query" | "search" => return None,
            "get" => p.ticket_id.iter().filter_map(|id| self.lookup_item_domain(kanban, id)).collect(),
            _ => self.write_targets(kanban, p),
        };
        let write = crate::tracker::LOCKED_KANBAN_ACTIONS.contains(&p.action.as_str());
        let (domain, project) = targets.iter().find(|(d, pr)| self.config.kanban_off_for_project(&format!("{d}/{pr}")))?;
        Some(crate::tracker::kanban_off_refusal(&self.config, domain, project, write))
    }

    /// The native items whose project has its board on.
    pub(super) fn visible_native<'a>(&self, kanban: &KanbanStore, items: &'a [KanbanItem]) -> Vec<&'a KanbanItem> {
        let mut domains: std::collections::HashMap<&str, Option<String>> = std::collections::HashMap::new();
        items
            .iter()
            .filter(|item| {
                let domain = domains.entry(item.project.as_str()).or_insert_with(|| self.lookup_project_domain(kanban, &item.project));
                domain.as_ref().is_none_or(|d| !self.config.kanban_off_for_project(&format!("{d}/{}", item.project)))
            })
            .collect()
    }

    /// Say in the response that the named project's native board is off.
    pub(super) fn add_off_note(&self, kanban: &KanbanStore, response: &mut serde_json::Value, p: &KanbanParams) {
        let Some(project) = &p.project else { return };
        let handler_domain = p.domain.clone().or_else(|| self.infer_domain_for_project(project));
        let domains = [self.lookup_project_domain(kanban, project), handler_domain];
        let off = domains.into_iter().flatten().find(|d| self.config.kanban_off_for_project(&format!("{d}/{project}")));
        if let Some(domain) = off {
            response["kanban_off"] = serde_json::json!(crate::tracker::kanban_off_refusal(&self.config, &domain, project, false));
        }
    }

    /// Refusal when `path` is native kanban text of an off project.
    pub(super) fn read_refusal(&self, path: &str) -> Option<String> {
        let (domain, project) = crate::kanban::native_file_project(path.trim_start_matches('/'))?;
        self.config.kanban_off_for_project(&format!("{domain}/{project}")).then(|| crate::tracker::kanban_off_refusal(&self.config, domain, project, false))
    }

    /// Keyword search that returns `limit` hits that are not native kanban
    /// text of an off project, when that many exist: it asks the index for
    /// more until the limit is filled or the index has no more.
    pub(super) fn keyword_search(&self, query: &str, domains: Option<Vec<String>>, limit: usize) -> Result<crate::index::fts::SearchResults, crate::index::store::IndexError> {
        let mut suggestions = Vec::new();
        let found = self.filled(limit, |n| {
            let q = crate::index::fts::SearchQuery { query: query.to_string(), domains: domains.clone(), types: Vec::new(), status: None, limit: n };
            let r = self.index.search(&q)?;
            suggestions = r.suggestions;
            Ok(r.results)
        }, |r| r.path.as_str())?;
        Ok(crate::index::fts::SearchResults { total: found.len(), results: found, suggestions })
    }

    /// `keyword_search` for the hybrid index.
    pub(super) fn hybrid_filled(&self, embedder: &mut crate::index::embed::Embedder, query: &str, domains: Option<&[String]>, limit: usize) -> Result<crate::index::hybrid::HybridResults, crate::index::store::IndexError> {
        let found = self.filled(limit, |n| Ok(crate::index::hybrid::hybrid_search(&self.index, embedder, query, n, domains)?.chunks), |c| c.path.as_str())?;
        Ok(crate::index::hybrid::HybridResults { total: found.len(), chunks: found })
    }

    fn filled<T>(&self, limit: usize, mut fetch: impl FnMut(usize) -> Result<Vec<T>, crate::index::store::IndexError>, path: impl Fn(&T) -> &str) -> Result<Vec<T>, crate::index::store::IndexError> {
        if limit == 0 {
            return fetch(0);
        }
        let mut asked = limit;
        loop {
            let mut found = fetch(asked)?;
            let exhausted = found.len() < asked;
            found.retain(|entry| match crate::kanban::native_file_project(path(entry)) {
                Some((domain, project)) => !self.config.kanban_off_for_project(&format!("{domain}/{project}")),
                None => true,
            });
            if found.len() >= limit || exhausted {
                found.truncate(limit);
                return Ok(found);
            }
            asked = asked.saturating_mul(4);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::super::tracker_tests::{dispatch, fixture_with, kanban, standard_mirror};
    use serde_json::{Value, json};

    const READONLY_SENTENCE: &str = "work/claims is a read-only mirror of Linear team COR.";
    const WRITE_OFF: &str = "The kanban is off for work/claims. Edit it in Linear team COR. Nothing was written.";
    const READ_OFF: &str = "The kanban is off for work/claims. Read its work in Linear team COR; wardwell_kanban list still returns the mirror.";

    fn error(response: &Value) -> String {
        response["error"].as_str().unwrap_or_default().to_string()
    }

    fn origins(response: &Value) -> Vec<String> {
        response["items"].as_array().unwrap().iter().map(|i| i["origin"].as_str().unwrap().to_string()).collect()
    }

    fn native_count(f: &super::super::tracker_tests::Fixture) -> usize {
        f.server.kanban.as_ref().unwrap().list(None, None, None, None, None, None, true, None).unwrap().len()
    }

    #[test]
    fn a_write_to_an_off_project_with_a_writable_binding_says_where_to_edit_and_writes_nothing() {
        let f = fixture_with(&standard_mirror(), false, Some(false));
        let before = native_count(&f);
        let response = dispatch(&f.server, json!({"action": "create", "project": "claims", "domain": "work", "title": "x"}));
        assert_eq!(error(&response), WRITE_OFF);
        assert_eq!(native_count(&f), before);
    }

    #[test]
    fn the_readonly_refusal_still_comes_first_for_a_write() {
        let f = fixture_with(&standard_mirror(), true, Some(false));
        let response = dispatch(&f.server, json!({"action": "note", "ticket_id": "CL-1", "text": "x"}));
        assert!(error(&response).starts_with(READONLY_SENTENCE), "{response}");
    }

    #[test]
    fn an_inferred_off_project_refuses_native_reads_with_the_read_sentence() {
        let f = fixture_with(&standard_mirror(), true, None);
        for args in [
            json!({"action": "get", "ticket_id": "CL-1"}),
            json!({"action": "reality_check", "project": "claims", "domain": "work"}),
            json!({"action": "plan", "project": "claims", "domain": "work"}),
            json!({"action": "question_list", "project": "claims", "domain": "work"}),
            json!({"action": "relationship_list", "project": "claims", "domain": "work"}),
            json!({"action": "proposal_list", "project": "claims", "domain": "work"}),
            json!({"action": "hygiene_suggestions", "project": "claims", "domain": "work"}),
        ] {
            let response = dispatch(&f.server, args.clone());
            assert_eq!(error(&response), READ_OFF, "{args}");
        }
    }

    #[test]
    fn writes_that_only_plan_or_report_also_refuse_with_the_write_sentence() {
        let f = fixture_with(&standard_mirror(), false, Some(false));
        for action in ["groom", "status", "export_roadmap", "question_create", "proposal_create"] {
            let response = dispatch(&f.server, json!({"action": action, "project": "claims", "domain": "work"}));
            assert_eq!(error(&response), WRITE_OFF, "{action}");
        }
    }

    #[test]
    fn list_query_search_and_get_still_return_the_tracker_mirror() {
        let f = fixture_with(&standard_mirror(), true, None);
        let listed = kanban(&f.server, json!({"action": "list", "project": "claims"}));
        assert_eq!(origins(&listed), vec!["tracker", "tracker"], "{listed}");
        assert_eq!(listed["kanban_off"], READ_OFF);
        let queried = kanban(&f.server, json!({"action": "query", "question": "recent", "project": "claims"}));
        assert!(origins(&queried).iter().all(|o| o == "tracker"), "{queried}");
        let searched = kanban(&f.server, json!({"action": "search", "query": "task", "project": "claims"}));
        assert!(origins(&searched).iter().all(|o| o == "tracker"), "{searched}");
        let searched = kanban(&f.server, json!({"action": "search", "query": "payer", "project": "claims"}));
        assert_eq!(searched["total"], 2, "{searched}");
        let got = dispatch(&f.server, json!({"action": "get", "ticket_id": "COR-12"}));
        assert_eq!(got["item"]["origin"], "tracker", "{got}");
    }

    #[test]
    fn an_off_project_with_no_tracker_binding_says_so() {
        let mut f = fixture_with(&standard_mirror(), false, Some(false));
        std::sync::Arc::get_mut(&mut f.server.config).unwrap().trackers.clear();
        let response = dispatch(&f.server, json!({"action": "create", "project": "claims", "domain": "work", "title": "x"}));
        assert_eq!(error(&response), "The kanban is off for work/claims. It has no tracker binding.");
        let response = dispatch(&f.server, json!({"action": "plan", "project": "claims", "domain": "work"}));
        assert_eq!(error(&response), "The kanban is off for work/claims. It has no tracker binding.");
    }

    #[test]
    fn an_explicit_on_keeps_the_native_board_readable_beside_a_readonly_binding() {
        let f = fixture_with(&standard_mirror(), true, Some(true));
        let listed = kanban(&f.server, json!({"action": "list", "project": "claims"}));
        assert_eq!(origins(&listed)[0], "kanban");
        assert!(listed.get("kanban_off").is_none());
        let plan = dispatch(&f.server, json!({"action": "plan", "project": "claims", "domain": "work"}));
        assert!(plan.get("error").is_none(), "{plan}");
        let write = dispatch(&f.server, json!({"action": "note", "ticket_id": "CL-1", "text": "x"}));
        assert!(error(&write).starts_with(READONLY_SENTENCE), "{write}");
    }

    #[test]
    fn another_project_keeps_its_native_board_while_an_off_project_is_hidden() {
        let f = fixture_with(&standard_mirror(), true, None);
        let store = f.server.kanban.as_ref().unwrap();
        std::fs::create_dir_all(f.server.vault_root.join("work/other")).unwrap();
        store.create_item("Other task", "other", "work", None, None, None, None, None, Some("hank"), None, None, None, &std::collections::HashMap::new()).unwrap();
        let listed = kanban(&f.server, json!({"action": "list"}));
        let titles: Vec<&str> = listed["items"].as_array().unwrap().iter().filter_map(|i| i["title"].as_str()).collect();
        assert!(titles.contains(&"Other task"), "{titles:?}");
        assert!(!titles.contains(&"Native claims task"), "{titles:?}");
        let create = dispatch(&f.server, json!({"action": "create", "project": "other", "domain": "work", "title": "More"}));
        assert!(create.get("error").is_none(), "{create}");
    }

    fn search(f: &super::super::tracker_tests::Fixture, query: &str) -> Value {
        let params: super::super::SearchParams = serde_json::from_value(json!({"action": "search", "query": query, "limit": 5})).unwrap();
        serde_json::from_str(&f.server.action_search(&params)).unwrap()
    }

    fn paths(response: &Value) -> Vec<String> {
        response["results"].as_array().unwrap().iter().map(|r| r["path"].as_str().unwrap().to_string()).collect()
    }

    fn index_vault(f: &super::super::tracker_tests::Fixture) {
        let vault = &f.server.vault_root;
        std::fs::write(vault.join("work/claims/tickets.md"), "# claims Tickets\n\n- 10/01 CL-1 zebrafish created\n").unwrap();
        std::fs::write(vault.join("work/claims/current_state.md"), "# State\n\nzebrafish notes live here\n").unwrap();
        crate::index::builder::IndexBuilder::build_filtered(&f.server.index, vault, &[], None).unwrap();
    }

    #[test]
    fn search_leaves_out_native_kanban_files_of_an_off_project_and_keeps_its_other_files() {
        let f = fixture_with(&standard_mirror(), true, None);
        index_vault(&f);
        let found = paths(&search(&f, "zebrafish"));
        assert_eq!(found, vec!["work/claims/current_state.md"], "{found:?}");
    }

    #[test]
    fn search_still_returns_native_kanban_files_when_the_board_is_on() {
        let f = fixture_with(&standard_mirror(), true, Some(true));
        index_vault(&f);
        let found = paths(&search(&f, "zebrafish"));
        assert!(found.contains(&"work/claims/tickets.md".to_string()), "{found:?}");
    }

    #[test]
    fn search_fills_the_limit_past_many_native_files_of_an_off_project() {
        let f = fixture_with(&standard_mirror(), true, None);
        let vault = &f.server.vault_root;
        std::fs::create_dir_all(vault.join("work/te")).unwrap();
        std::fs::create_dir_all(vault.join("work/claims/status")).unwrap();
        for i in 0..6 {
            std::fs::write(vault.join(format!("work/claims/status/s{i}.md")), "zebrafish zebrafish zebrafish zebrafish zebrafish\n").unwrap();
        }
        std::fs::write(vault.join("work/te/current_state.md"), "# State\n\nzebrafish once\n").unwrap();
        crate::index::builder::IndexBuilder::build_filtered(&f.server.index, vault, &[], None).unwrap();
        let params: super::super::SearchParams = serde_json::from_value(json!({"action": "search", "query": "zebrafish", "limit": 1})).unwrap();
        let found: Value = serde_json::from_str(&f.server.action_search(&params)).unwrap();
        assert_eq!(paths(&found), vec!["work/te/current_state.md"], "{found}");
    }

    #[test]
    fn reading_a_native_kanban_file_of_an_off_project_is_refused_and_other_files_are_not() {
        let f = fixture_with(&standard_mirror(), true, None);
        index_vault(&f);
        let read = |path: &str| -> Value {
            let params: super::super::SearchParams = serde_json::from_value(json!({"action": "read", "path": path})).unwrap();
            serde_json::from_str(&f.server.action_read(&params)).unwrap()
        };
        for path in ["work/claims/tickets.md", "/work/claims/tickets.md"] {
            assert_eq!(error(&read(path)), READ_OFF, "{path}");
        }
        assert!(read("work/claims/current_state.md").get("error").is_none());
        let on = fixture_with(&standard_mirror(), true, Some(true));
        index_vault(&on);
        let params: super::super::SearchParams = serde_json::from_value(json!({"action": "read", "path": "work/claims/tickets.md"})).unwrap();
        let body: Value = serde_json::from_str(&on.server.action_read(&params)).unwrap();
        assert!(body.get("error").is_none(), "{body}");
    }
}
