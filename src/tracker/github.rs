//! GitHub adapter: the merged pull requests of one repository, translated
//! into `change_merged` events. Reads through the `gh` command when it is
//! on PATH and answers, else through the REST API with the token from
//! `wardwell tracker connect`. GitHub names stop here except inside `raw`.
//!
//! Does NOT write to GitHub, decide the cursor, or touch the vault. The
//! session memory path never reaches this module, so it never runs `gh`.

use crate::tracker::adapter::{AUTH_REFUSED, Adapter, Sink, UNREACHABLE};
use crate::tracker::events::{Common, Event, FailureCode, MergedChange};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;
use std::io::Read;
use std::time::{Duration, Instant};

const PROVIDER: &str = crate::tracker::GITHUB;
const API_ROOT: &str = "https://api.github.com";
/// A first pull, with no cursor and no `--full`, reads this many of the
/// most recently updated merged pull requests.
pub const FIRST_PULL_LIMIT: usize = 200;
/// GitHub search returns at most this many results; an incremental `gh`
/// read asks for all of them.
const SEARCH_LIMIT: usize = 1_000;
/// A full `gh` read lists every merged pull request up to this many.
const FULL_LIMIT: usize = 100_000;
/// The `gh pr list` fields the adapter reads.
pub const GH_FIELDS: &str = "number,title,body,author,mergedAt,url,baseRefName,id";
const PER_PAGE: usize = 100;
/// Upper bound on REST pages per pull so a misbehaving reply cannot loop forever.
const MAX_PAGES: usize = 1_000;
const RESPONSE_LIMIT: usize = 16 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);
/// A `gh` read that runs longer than this is stopped and counts as failed.
const GH_TIMEOUT: Duration = Duration::from_secs(120);

/// What one `gh` run gave back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GhOutcome {
    /// `gh` is not on PATH or could not start.
    Missing,
    /// `gh` ran and failed, or ran past its time limit.
    Failed,
    /// `gh` exited zero with this standard output.
    Output(Vec<u8>),
}

/// Runs `gh` with arguments. Injected so no test ever starts `gh`.
pub trait GhRunner {
    /// Run `gh <args>` and return what it gave back.
    fn run(&self, args: &[String]) -> GhOutcome;
}

/// GETs one REST path below the API root and returns the decoded JSON.
/// Injected so no test ever opens a socket.
pub trait Rest {
    /// GET `path`, such as `/repos/acme/app/pulls?state=closed`.
    fn get(&self, path: &str) -> Result<Value, String>;
}

/// Which reader answered a doctor check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The `gh` command answered.
    Gh,
    /// The REST API answered with the stored token.
    Token,
}

impl Route {
    /// The route as doctor prints it.
    pub fn describe(self) -> &'static str {
        match self {
            Route::Gh => "through gh",
            Route::Token => "through the API token",
        }
    }
}

/// Pulls the merged pull requests of one repository.
pub struct GitHub {
    repository: String,
    credential: String,
    gh: Box<dyn GhRunner>,
    rest: Option<Box<dyn Rest>>,
}

impl GitHub {
    /// An adapter for `repository` (`<owner>/<name>`). `rest` is None when no
    /// token is stored under `credential`; then only `gh` can read.
    pub fn new(repository: &str, credential: &str, gh: Box<dyn GhRunner>, rest: Option<Box<dyn Rest>>) -> Self {
        Self { repository: repository.to_string(), credential: credential.to_string(), gh, rest }
    }

    /// Doctor check: one cheap read of the repository. Tries `gh`, then the
    /// token. Unreachable when neither can read; `team_not_found` when the
    /// repository does not exist or is hidden from the reader.
    pub fn check(&self) -> Result<Route, FailureCode> {
        let args = strings(&["repo", "view", &self.repository, "--json", "nameWithOwner"]);
        if let GhOutcome::Output(_) = self.gh.run(&args) {
            return Ok(Route::Gh);
        }
        let rest = self.rest.as_ref().ok_or(FailureCode::Credential)?;
        match rest.get(&format!("/repos/{}", self.repository)) {
            Ok(_) => Ok(Route::Token),
            Err(error) if error.contains(AUTH_REFUSED) => Err(FailureCode::Auth),
            Err(error) if error.contains(NOT_FOUND) => Err(FailureCode::TeamNotFound),
            Err(_) => Err(FailureCode::Provider),
        }
    }

    /// The doctor and pull message when neither reader is available.
    pub fn unreachable_message(&self) -> String {
        unreachable_line(&self.credential)
    }

    /// Merged pull requests through `gh`, or None when `gh` did not answer.
    fn read_gh(&self, since: Option<DateTime<Utc>>, limit: Option<usize>) -> Option<Result<Vec<Value>, String>> {
        let args = gh_list_args(&self.repository, since, limit);
        match self.gh.run(&args) {
            GhOutcome::Output(bytes) => Some(
                serde_json::from_slice::<Value>(&bytes)
                    .ok()
                    .and_then(|v| v.as_array().cloned())
                    .ok_or_else(|| "gh returned output that is not a list of pull requests".to_string()),
            ),
            GhOutcome::Missing | GhOutcome::Failed => None,
        }
    }

    /// Merged pull requests through the REST API, most recently updated
    /// first, stopping at `limit` or at the first one updated before `since`.
    fn read_rest(&self, rest: &dyn Rest, since: Option<DateTime<Utc>>, limit: Option<usize>) -> Result<Vec<Value>, String> {
        let mut merged = Vec::new();
        for page in 1..=MAX_PAGES {
            let reply = rest.get(&rest_list_path(&self.repository, page))?;
            let nodes = reply.as_array().ok_or_else(|| "GitHub returned a reply that is not a list of pull requests".to_string())?;
            let mut older = false;
            for node in nodes {
                older |= since.is_some_and(|since| time(node, "updated_at").is_some_and(|updated| updated < since));
                if !node["merged_at"].is_string() || limit.is_some_and(|limit| merged.len() >= limit) {
                    continue;
                }
                merged.push(node.clone());
            }
            if older || nodes.len() < PER_PAGE || limit.is_some_and(|limit| merged.len() >= limit) {
                return Ok(merged);
            }
        }
        Err(format!("GitHub returned more than {MAX_PAGES} pages"))
    }
}

impl Adapter for GitHub {
    fn pull(&self, since: Option<DateTime<Utc>>, full: bool, sink: &mut Sink<'_>) -> Result<(), String> {
        let since = since.filter(|_| !full);
        let limit = (!full && since.is_none()).then_some(FIRST_PULL_LIMIT);
        let (nodes, source) = match (self.read_gh(since, limit), &self.rest) {
            (Some(read), _) => (read?, Source::Gh),
            (None, Some(rest)) => (self.read_rest(rest.as_ref(), since, limit)?, Source::Rest),
            (None, None) => return Err(self.unreachable_message()),
        };
        let mut events = Vec::new();
        for node in &nodes {
            let event = translate(&self.repository, node, source)?;
            if since.is_none_or(|since| event.common().occurred_at >= since) {
                events.push(event);
            }
        }
        for page in events.chunks(PER_PAGE) {
            sink(page.to_vec())?;
        }
        Ok(())
    }
}

/// Whether an executable `gh` is in a directory on PATH. Reads the file
/// system only; never starts `gh`.
pub fn gh_on_path() -> bool {
    gh_in(std::env::var_os("PATH").as_deref())
}

/// Whether an executable `gh` is in a directory of `path`, a PATH value.
pub fn gh_in(path: Option<&std::ffi::OsStr>) -> bool {
    let Some(path) = path else {
        return false;
    };
    std::env::split_paths(path).map(|dir| dir.join("gh")).any(|file| is_executable(&file))
}

#[cfg(unix)]
fn is_executable(file: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(file).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(file: &std::path::Path) -> bool {
    file.is_file()
}

/// The line doctor and a failed pull print when no reader is available.
pub fn unreachable_line(credential: &str) -> String {
    format!("github: {UNREACHABLE} {credential}`")
}

/// The arguments of the `gh pr list` read. A first pull asks for the
/// `FIRST_PULL_LIMIT` most recently updated; an incremental pull searches
/// for those merged at or after `since`; a full pull lists every one.
pub fn gh_list_args(repository: &str, since: Option<DateTime<Utc>>, limit: Option<usize>) -> Vec<String> {
    let (limit, search) = match (since, limit) {
        (Some(since), _) => (SEARCH_LIMIT, Some(format!("merged:>={} sort:updated-desc", since.to_rfc3339_opts(SecondsFormat::Secs, true)))),
        (None, Some(limit)) => (limit, Some("sort:updated-desc".to_string())),
        (None, None) => (FULL_LIMIT, None),
    };
    let limit = limit.to_string();
    let mut args = strings(&["pr", "list", "--repo", repository, "--state", "merged", "--limit", &limit, "--json", GH_FIELDS]);
    if let Some(search) = search {
        args.extend(strings(&["--search", &search]));
    }
    args
}

/// The REST path of one page of the repository's closed pull requests,
/// most recently updated first.
pub fn rest_list_path(repository: &str, page: usize) -> String {
    format!("/repos/{repository}/pulls?state=closed&sort=updated&direction=desc&per_page={PER_PAGE}&page={page}")
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

/// Which reader a pull request node came from; the two name fields differently.
#[derive(Clone, Copy)]
enum Source {
    Gh,
    Rest,
}

/// One merged pull request node as a `change_merged` event. The id is
/// stable, so a re-pull of the same pull request appends nothing.
fn translate(repository: &str, node: &Value, source: Source) -> Result<Event, String> {
    let (merged_key, author, url, base, id) = match source {
        Source::Gh => ("mergedAt", &node["author"]["login"], "url", &node["baseRefName"], "id"),
        Source::Rest => ("merged_at", &node["user"]["login"], "html_url", &node["base"]["ref"], "node_id"),
    };
    let number = node["number"].as_u64().ok_or_else(|| "GitHub pull request without a number".to_string())?;
    let merged_at = time(node, merged_key).ok_or_else(|| format!("GitHub pull request #{number} without a merge time"))?;
    let title = node["title"].as_str().unwrap_or_default().to_string();
    let base_branch = base.as_str().map(str::to_string);
    let reference = format!("{repository}#{number}");
    let into = base_branch.as_deref().map(|b| format!(" into {b}")).unwrap_or_default();
    let change = MergedChange {
        number,
        keys: crate::index::fts::ticket_keys(&title),
        title: title.clone(),
        body: node["body"].as_str().filter(|b| !b.is_empty()).map(str::to_string),
        author: author.as_str().map(str::to_string),
        merged_at,
        url: node[url].as_str().map(str::to_string),
        base_branch,
    };
    Ok(Event::ChangeMerged {
        common: Common {
            id: format!("{PROVIDER}:{reference}"),
            provider: PROVIDER.to_string(),
            external_key: reference.clone(),
            external_id: node[id].as_str().map_or_else(|| number.to_string(), str::to_string),
            actor: change.author.clone(),
            occurred_at: merged_at,
            title: format!("{reference} merged{into}: {title}"),
            raw: node.clone(),
        },
        change: Box::new(change),
    })
}

fn time(node: &Value, key: &str) -> Option<DateTime<Utc>> {
    node[key].as_str().and_then(|t| DateTime::parse_from_rfc3339(t).ok()).map(|t| t.with_timezone(&Utc))
}

/// Error text when the API answers 404, so doctor can name a missing repository.
const NOT_FOUND: &str = "GitHub returned HTTP 404";

/// Runs the `gh` on PATH. Standard error is discarded and nothing is
/// echoed; a run past `GH_TIMEOUT` is killed and counts as failed.
pub struct SystemGh;

impl GhRunner for SystemGh {
    fn run(&self, args: &[String]) -> GhOutcome {
        let spawned = std::process::Command::new("gh")
            .args(args)
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("NO_COLOR", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn();
        let Ok(mut child) = spawned else {
            return GhOutcome::Missing;
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            return GhOutcome::Failed;
        };
        // Read on a thread so a full pipe never blocks the wait below.
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout.take(RESPONSE_LIMIT as u64 + 1).read_to_end(&mut bytes).map(|_| bytes)
        });
        let deadline = Instant::now() + GH_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        match (status, reader.join()) {
            (Some(status), Ok(Ok(bytes))) if status.success() && bytes.len() <= RESPONSE_LIMIT => GhOutcome::Output(bytes),
            _ => GhOutcome::Failed,
        }
    }
}

/// Bounded HTTPS reads from the GitHub REST API with a bearer token. Never
/// logs or echoes the token.
pub struct HttpRest {
    token: String,
}

impl HttpRest {
    /// A client that authenticates with `token`.
    pub fn new(token: String) -> Self {
        Self { token }
    }
}

impl Rest for HttpRest {
    fn get(&self, path: &str) -> Result<Value, String> {
        let url = format!("{API_ROOT}{path}");
        // Run on a private thread and runtime so callers need not care
        // whether they are inside an async context.
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|_| "could not start the GitHub HTTP runtime".to_string())?;
                    runtime.block_on(send(&self.token, &url))
                })
                .join()
                .map_err(|_| "GitHub request thread failed".to_string())?
        })
    }
}

async fn send(token: &str, url: &str) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .build()
        .map_err(|_| "could not create the GitHub HTTP client".to_string())?;
    let mut response = client
        .get(url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(reqwest::header::USER_AGENT, "wardwell")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|_| "GitHub request failed".to_string())?;
    let status = response.status();
    if status.is_redirection() {
        return Err("GitHub redirects are not allowed".to_string());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| "could not read the GitHub response".to_string())? {
        if bytes.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
            return Err("GitHub response exceeds 16 MiB".to_string());
        }
        bytes.extend_from_slice(&chunk);
    }
    reply(status.as_u16(), serde_json::from_slice(&bytes).ok())
}

/// The decoded reply for an HTTP status. 401 and 403 are a refused token;
/// the body is never passed on.
fn reply(status: u16, decoded: Option<Value>) -> Result<Value, String> {
    match (status, decoded) {
        (401 | 403, _) => Err(format!("GitHub returned HTTP {status}: {AUTH_REFUSED}")),
        (404, _) => Err(NOT_FOUND.to_string()),
        (200..=299, Some(value)) => Ok(value),
        (200..=299, None) => Err("GitHub returned invalid JSON".to_string()),
        (_, _) => Err(format!("GitHub returned HTTP {status}")),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Answers every `gh` run with `outcome` and records the arguments.
    pub(crate) struct FakeGh {
        pub outcome: GhOutcome,
        pub calls: Rc<RefCell<Vec<Vec<String>>>>,
    }

    impl GhRunner for FakeGh {
        fn run(&self, args: &[String]) -> GhOutcome {
            self.calls.borrow_mut().push(args.to_vec());
            self.outcome.clone()
        }
    }

    /// Answers REST paths from a list of pages and records the paths.
    pub(crate) struct FakeRest {
        pub pages: Vec<Result<Value, String>>,
        pub calls: Rc<RefCell<Vec<String>>>,
    }

    impl Rest for FakeRest {
        fn get(&self, path: &str) -> Result<Value, String> {
            self.calls.borrow_mut().push(path.to_string());
            let page = self.calls.borrow().len() - 1;
            self.pages.get(page).cloned().unwrap_or(Ok(json!([])))
        }
    }

    pub(crate) fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 1, hour, 0, 0).unwrap()
    }

    /// A `gh pr list` node merged at `hour`.
    pub(crate) fn gh_node(number: u64, title: &str, hour: u32) -> Value {
        json!({
            "number": number, "title": title, "body": "Fixes the inbox.",
            "author": {"login": "jdoe", "is_bot": false}, "mergedAt": at(hour).to_rfc3339(),
            "url": format!("https://github.com/acme/app/pull/{number}"), "baseRefName": "main", "id": format!("PR_{number}")
        })
    }

    /// A REST pull request node updated at `hour`, merged then when `merged`.
    fn rest_node(number: u64, hour: u32, merged: bool) -> Value {
        json!({
            "number": number, "title": format!("COR-{number} change"), "body": null,
            "user": {"login": "jdoe"}, "merged_at": if merged { json!(at(hour).to_rfc3339()) } else { Value::Null },
            "updated_at": at(hour).to_rfc3339(), "html_url": format!("https://github.com/acme/app/pull/{number}"),
            "base": {"ref": "main"}, "node_id": format!("PR_{number}")
        })
    }

    pub(crate) fn gh_with(outcome: GhOutcome) -> (Box<dyn GhRunner>, Rc<RefCell<Vec<Vec<String>>>>) {
        let calls = Rc::new(RefCell::new(vec![]));
        (Box::new(FakeGh { outcome, calls: calls.clone() }), calls)
    }

    fn rest_with(pages: Vec<Result<Value, String>>) -> (Box<dyn Rest>, Rc<RefCell<Vec<String>>>) {
        let calls = Rc::new(RefCell::new(vec![]));
        (Box::new(FakeRest { pages, calls: calls.clone() }), calls)
    }

    fn collect(adapter: &GitHub, since: Option<DateTime<Utc>>, full: bool) -> Result<Vec<Event>, String> {
        let mut events = Vec::new();
        adapter.pull(since, full, &mut |page| {
            events.extend(page);
            Ok(())
        })?;
        Ok(events)
    }

    fn output(nodes: Vec<Value>) -> GhOutcome {
        GhOutcome::Output(serde_json::to_vec(&nodes).unwrap())
    }

    #[test]
    fn gh_nodes_become_change_merged_events() {
        let (gh, calls) = gh_with(output(vec![gh_node(42, "COR-12: Fix the claims inbox (cm-3)", 10)]));
        let adapter = GitHub::new("acme/app", "github", gh, None);
        let events = collect(&adapter, None, false).unwrap();
        assert_eq!(calls.borrow().len(), 1);
        let Event::ChangeMerged { common, change } = &events[0] else { panic!("{events:?}") };
        assert_eq!(common.id, "github:acme/app#42");
        assert_eq!(common.provider, "github");
        assert_eq!(common.external_key, "acme/app#42");
        assert_eq!(common.external_id, "PR_42");
        assert_eq!(common.actor.as_deref(), Some("jdoe"));
        assert_eq!(common.occurred_at, at(10));
        assert_eq!(common.title, "acme/app#42 merged into main: COR-12: Fix the claims inbox (cm-3)");
        assert_eq!(common.raw["number"], 42, "the raw node goes to the sidecar");
        assert_eq!(change.number, 42);
        assert_eq!(change.title, "COR-12: Fix the claims inbox (cm-3)");
        assert_eq!(change.body.as_deref(), Some("Fixes the inbox."));
        assert_eq!(change.author.as_deref(), Some("jdoe"));
        assert_eq!(change.merged_at, at(10));
        assert_eq!(change.url.as_deref(), Some("https://github.com/acme/app/pull/42"));
        assert_eq!(change.base_branch.as_deref(), Some("main"));
        assert_eq!(change.keys, vec!["COR-12", "CM-3"]);
    }

    #[test]
    fn gh_arguments_bound_the_first_pull_search_from_the_cursor_and_list_all_on_full() {
        let fields = GH_FIELDS;
        let first = gh_list_args("acme/app", None, Some(FIRST_PULL_LIMIT)).join(" ");
        assert_eq!(first, format!("pr list --repo acme/app --state merged --limit 200 --json {fields} --search sort:updated-desc"));
        let since = gh_list_args("acme/app", Some(at(9)), None).join(" ");
        assert_eq!(since, format!("pr list --repo acme/app --state merged --limit 1000 --json {fields} --search merged:>=2026-09-01T09:00:00Z sort:updated-desc"));
        let full = gh_list_args("acme/app", None, None).join(" ");
        assert_eq!(full, format!("pr list --repo acme/app --state merged --limit 100000 --json {fields}"));

        for (since, full, expected) in [(None, false, &first), (Some(at(9)), false, &since), (Some(at(9)), true, &full), (None, true, &full)] {
            let (gh, calls) = gh_with(output(vec![]));
            collect(&GitHub::new("acme/app", "github", gh, None), since, full).unwrap();
            assert_eq!(&calls.borrow()[0].join(" "), expected);
        }
    }

    #[test]
    fn without_gh_the_token_reads_the_rest_api_until_the_cursor() {
        let (gh, _) = gh_with(GhOutcome::Missing);
        let page_one: Vec<Value> = (0..PER_PAGE as u64).map(|n| rest_node(500 - n, 20, n % 2 == 0)).collect();
        let mut page_two = vec![rest_node(10, 12, true)];
        page_two.extend((0..PER_PAGE as u64 - 1).map(|n| rest_node(300 + n, 8, true)));
        let (rest, calls) = rest_with(vec![Ok(json!(page_one)), Ok(json!(page_two)), Ok(json!([rest_node(1, 1, true)]))]);
        let adapter = GitHub::new("acme/app", "github", gh, Some(rest));
        let events = collect(&adapter, Some(at(10)), false).unwrap();
        assert_eq!(*calls.borrow(), vec![rest_list_path("acme/app", 1), rest_list_path("acme/app", 2)], "stops at a page reaching past the cursor");
        assert_eq!(rest_list_path("acme/app", 2), "/repos/acme/app/pulls?state=closed&sort=updated&direction=desc&per_page=100&page=2");
        assert_eq!(events.len(), 51, "50 merged on page one, #10 on page two; unmerged and older ones are left out");
        let Event::ChangeMerged { common, change } = events.last().unwrap() else { panic!() };
        assert_eq!((common.id.as_str(), common.external_id.as_str()), ("github:acme/app#10", "PR_10"));
        assert_eq!(change.body, None);
        assert_eq!(change.url.as_deref(), Some("https://github.com/acme/app/pull/10"));
        assert_eq!(change.keys, vec!["COR-10"]);
    }

    #[test]
    fn the_rest_first_pull_stops_at_the_limit_and_full_reads_every_page() {
        let full_page = |base: u64| -> Value { json!((0..PER_PAGE as u64).map(|n| rest_node(base - n, 10, true)).collect::<Vec<_>>()) };
        let pages = || vec![Ok(full_page(1000)), Ok(full_page(900)), Ok(full_page(800)), Ok(json!([rest_node(1, 1, true)]))];
        let (gh, _) = gh_with(GhOutcome::Failed);
        let (rest, calls) = rest_with(pages());
        let first = collect(&GitHub::new("acme/app", "github", gh, Some(rest)), None, false).unwrap();
        assert_eq!(first.len(), FIRST_PULL_LIMIT);
        assert_eq!(calls.borrow().len(), 2);

        let (gh, _) = gh_with(GhOutcome::Failed);
        let (rest, calls) = rest_with(pages());
        let all = collect(&GitHub::new("acme/app", "github", gh, Some(rest)), Some(at(9)), true).unwrap();
        assert_eq!(all.len(), 301);
        assert_eq!(calls.borrow().len(), 4);
    }

    #[test]
    fn gh_is_tried_first_and_the_token_is_not_used_when_gh_answers() {
        let (gh, _) = gh_with(output(vec![gh_node(1, "x", 9)]));
        let (rest, calls) = rest_with(vec![]);
        collect(&GitHub::new("acme/app", "github", gh, Some(rest)), None, false).unwrap();
        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn neither_gh_nor_a_token_is_unreachable_and_names_the_connect_command() {
        for outcome in [GhOutcome::Missing, GhOutcome::Failed] {
            let (gh, _) = gh_with(outcome);
            let error = collect(&GitHub::new("acme/app", "github", gh, None), None, false).unwrap_err();
            assert_eq!(error, "github: unreachable, run `wardwell tracker connect github`");
            assert!(error.contains(UNREACHABLE), "{error}");
            let (gh, _) = gh_with(GhOutcome::Missing);
            let named = GitHub::new("acme/app", "gh-work", gh, None).unreachable_message();
            assert_eq!(named, "github: unreachable, run `wardwell tracker connect gh-work`");
        }
    }

    #[test]
    fn a_refused_token_says_so_without_the_body() {
        assert_eq!(reply(401, Some(json!({"message": "Bad credentials ghp_secret"}))).unwrap_err(), format!("GitHub returned HTTP 401: {AUTH_REFUSED}"));
        assert_eq!(reply(404, None).unwrap_err(), NOT_FOUND);
        assert_eq!(reply(500, Some(json!({"message": "ghp_secret"}))).unwrap_err(), "GitHub returned HTTP 500");
        assert_eq!(reply(200, Some(json!([]))).unwrap(), json!([]));
    }

    #[test]
    fn unparsable_gh_output_is_a_provider_error_not_a_fallback() {
        let (gh, _) = gh_with(GhOutcome::Output(b"not json".to_vec()));
        let (rest, calls) = rest_with(vec![]);
        let error = collect(&GitHub::new("acme/app", "github", gh, Some(rest)), None, false).unwrap_err();
        assert!(error.contains("not a list"), "{error}");
        assert!(calls.borrow().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn gh_on_path_looks_for_an_executable_file_without_running_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path = std::env::join_paths([dir.path().join("none"), bin.clone()]).unwrap();
        assert!(!gh_in(Some(&path)));
        std::fs::write(bin.join("gh"), "#!/bin/sh\nexit 1\n").unwrap();
        assert!(!gh_in(Some(&path)), "not executable");
        std::fs::set_permissions(bin.join("gh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(gh_in(Some(&path)));
        assert!(!gh_in(None));
    }

    #[test]
    fn check_reports_the_route_or_a_closed_code() {
        let (gh, calls) = gh_with(output(vec![]));
        assert_eq!(GitHub::new("acme/app", "github", gh, None).check(), Ok(Route::Gh));
        assert_eq!(calls.borrow()[0].join(" "), "repo view acme/app --json nameWithOwner");
        let cases: Vec<(Option<Result<Value, String>>, Result<Route, FailureCode>)> = vec![
            (Some(Ok(json!({"full_name": "acme/app"}))), Ok(Route::Token)),
            (Some(Err(format!("GitHub returned HTTP 401: {AUTH_REFUSED}"))), Err(FailureCode::Auth)),
            (Some(Err(NOT_FOUND.to_string())), Err(FailureCode::TeamNotFound)),
            (Some(Err("GitHub request failed".to_string())), Err(FailureCode::Provider)),
            (None, Err(FailureCode::Credential)),
        ];
        for (answer, expected) in cases {
            let (gh, _) = gh_with(GhOutcome::Missing);
            let rest = answer.map(|a| rest_with(vec![a]));
            let paths = rest.as_ref().map(|(_, calls)| calls.clone());
            let adapter = GitHub::new("acme/app", "github", gh, rest.map(|(r, _)| r));
            assert_eq!(adapter.check(), expected);
            if let Some(paths) = paths {
                assert_eq!(*paths.borrow(), vec!["/repos/acme/app"]);
            }
        }
    }
}
