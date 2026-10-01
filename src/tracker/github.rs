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
pub const GH_FIELDS: &str = "number,title,body,author,mergedAt,url,baseRefName,id,updatedAt";
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
    /// No `gh` was found, or it could not start.
    Missing,
    /// `gh` exited 4, its code for a missing or refused sign-in.
    SignedOut,
    /// `gh` exited non-zero with this status, or None when a signal stopped it.
    Exited(Option<i32>),
    /// `gh` ran past this limit and was stopped.
    TimedOut(Duration),
    /// `gh` wrote more than this many bytes.
    Oversize(usize),
    /// `gh` exited zero with this standard output.
    Output(Vec<u8>),
}

/// The sentence for `gh` output that does not parse as a list.
const UNPARSEABLE: &str = "gh returned output that is not a list of pull requests";

/// The exit status `gh` uses when it is not signed in.
const GH_SIGNED_OUT: i32 = 4;

/// Why `gh` gave no list of pull requests.
enum GhFailure {
    /// No `gh` can read: none was found, or it is not signed in (the reason).
    Absent(Option<&'static str>),
    /// `gh` ran and failed, with the sentence that says how.
    Failed(String),
}

impl GhFailure {
    fn from(outcome: GhOutcome) -> Self {
        match outcome {
            GhOutcome::Missing => Self::Absent(None),
            GhOutcome::SignedOut => Self::Absent(Some("gh is not signed in")),
            GhOutcome::Exited(Some(status)) => Self::Failed(format!("gh exited with status {status}")),
            GhOutcome::Exited(None) => Self::Failed("gh was stopped by a signal".to_string()),
            GhOutcome::TimedOut(limit) => Self::Failed(format!("gh did not finish within {} seconds", limit.as_secs())),
            GhOutcome::Oversize(limit) => Self::Failed(format!("gh output exceeded {limit} bytes")),
            GhOutcome::Output(_) => Self::Failed(UNPARSEABLE.to_string()),
        }
    }

    /// The sentence to put before a token read's error, if any.
    fn sentence(&self) -> Option<String> {
        match self {
            Self::Absent(why) => why.map(str::to_string),
            Self::Failed(sentence) => Some(sentence.clone()),
        }
    }
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
    /// token. `credential` when neither can read; `provider` with the
    /// sentence when `gh` failed and no token is stored; `team_not_found`
    /// when the repository does not exist or is hidden from the reader.
    pub fn check(&self) -> Result<Route, (FailureCode, Option<String>)> {
        let args = strings(&["repo", "view", &self.repository, "--json", "nameWithOwner"]);
        let failure = match self.gh.run(&args) {
            GhOutcome::Output(_) => return Ok(Route::Gh),
            outcome => GhFailure::from(outcome),
        };
        let Some(rest) = self.rest.as_ref() else {
            return Err(match failure {
                GhFailure::Failed(sentence) => (FailureCode::Provider, Some(sentence)),
                GhFailure::Absent(_) => (FailureCode::Credential, Some(self.no_reader(&failure))),
            });
        };
        match rest.get(&format!("/repos/{}", self.repository)) {
            Ok(_) => Ok(Route::Token),
            Err(error) if error.contains(AUTH_REFUSED) => Err((FailureCode::Auth, None)),
            Err(error) if error.contains(NOT_FOUND) => Err((FailureCode::TeamNotFound, None)),
            Err(_) => Err((FailureCode::Provider, None)),
        }
    }

    /// The unreachable line, after the reason `gh` cannot read when there is one.
    fn no_reader(&self, failure: &GhFailure) -> String {
        match failure.sentence() {
            Some(why) => format!("{why}; {}", self.unreachable_message()),
            None => self.unreachable_message(),
        }
    }

    /// Merged pull requests and the reader that gave them: `gh` first, the
    /// token when `gh` cannot read or fails.
    fn read(&self, since: Option<DateTime<Utc>>, limit: Option<usize>) -> Result<(Vec<Value>, Source), String> {
        let failure = match self.read_gh(since, limit) {
            Ok(nodes) => return Ok((nodes, Source::Gh)),
            Err(failure) => failure,
        };
        match (&self.rest, &failure) {
            (Some(rest), _) => self.read_rest(rest.as_ref(), since, limit).map(|nodes| (nodes, Source::Rest)).map_err(|error| match failure.sentence() {
                Some(sentence) => format!("{sentence}; the API token read failed: {error}"),
                None => error,
            }),
            (None, GhFailure::Absent(_)) => Err(self.no_reader(&failure)),
            (None, GhFailure::Failed(sentence)) => Err(sentence.clone()),
        }
    }

    /// The doctor and pull message when neither reader is available.
    pub fn unreachable_message(&self) -> String {
        unreachable_line(&self.credential)
    }

    /// Merged pull requests through `gh`, or why `gh` gave none.
    fn read_gh(&self, since: Option<DateTime<Utc>>, limit: Option<usize>) -> Result<Vec<Value>, GhFailure> {
        let args = gh_list_args(&self.repository, since, limit);
        match self.gh.run(&args) {
            GhOutcome::Output(bytes) => parse_list(&bytes).ok_or_else(|| GhFailure::Failed(UNPARSEABLE.to_string())),
            outcome => Err(GhFailure::from(outcome)),
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
        let (nodes, source) = self.read(since, limit)?;
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

/// Where `gh` is looked for after PATH, in order. launchd runs the hourly
/// pull with PATH `/usr/bin:/bin:/usr/sbin:/sbin`, which holds neither.
pub const GH_CANDIDATES: [&str; 2] = ["/opt/homebrew/bin/gh", "/usr/local/bin/gh"];

/// The `gh` to run: the first executable `gh` in a directory of `path`, a
/// PATH value, else the first executable path in `candidates`. Reads the
/// file system only; never starts `gh`. The adapter, `tracker doctor` and
/// `doctor` all find `gh` through this.
pub fn locate_gh(path: Option<&std::ffi::OsStr>, candidates: &[std::path::PathBuf]) -> Option<std::path::PathBuf> {
    let on_path = path.into_iter().flat_map(std::env::split_paths).map(|dir| dir.join("gh"));
    on_path.chain(candidates.iter().cloned()).find(|file| is_executable(file))
}

/// `locate_gh` with this process's PATH and `GH_CANDIDATES`.
pub fn find_gh() -> Option<std::path::PathBuf> {
    let candidates: Vec<std::path::PathBuf> = GH_CANDIDATES.iter().map(std::path::PathBuf::from).collect();
    locate_gh(std::env::var_os("PATH").as_deref(), &candidates)
}

/// Whether `find_gh` finds a `gh`.
pub fn gh_available() -> bool {
    find_gh().is_some()
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

fn parse_list(bytes: &[u8]) -> Option<Vec<Value>> {
    serde_json::from_slice::<Value>(bytes).ok().and_then(|v| v.as_array().cloned())
}

/// The line doctor and a failed pull print when no reader is available.
pub fn unreachable_line(credential: &str) -> String {
    format!("github: {UNREACHABLE} {credential}`")
}

/// The arguments of the `gh pr list` read. A first pull asks for the
/// `FIRST_PULL_LIMIT` most recently updated; an incremental pull searches
/// for merged ones updated at or after `since`; a full pull lists every one.
pub fn gh_list_args(repository: &str, since: Option<DateTime<Utc>>, limit: Option<usize>) -> Vec<String> {
    let (limit, search) = match (since, limit) {
        (Some(since), _) => (SEARCH_LIMIT, Some(format!("is:merged updated:>={} sort:updated-desc", since.to_rfc3339_opts(SecondsFormat::Secs, true)))),
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

/// One merged pull request node as a `change_merged` event at the pull
/// request's update time, the time the GitHub cursor follows. The id is
/// stable; the log appends a revision only when the content changed.
fn translate(repository: &str, node: &Value, source: Source) -> Result<Event, String> {
    let (merged_key, updated_key, author, url, base, id) = match source {
        Source::Gh => ("mergedAt", "updatedAt", &node["author"]["login"], "url", &node["baseRefName"], "id"),
        Source::Rest => ("merged_at", "updated_at", &node["user"]["login"], "html_url", &node["base"]["ref"], "node_id"),
    };
    let number = node["number"].as_u64().ok_or_else(|| "GitHub pull request without a number".to_string())?;
    let merged_at = time(node, merged_key).ok_or_else(|| format!("GitHub pull request #{number} without a merge time"))?;
    let updated_at = time(node, updated_key).unwrap_or(merged_at).max(merged_at);
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
            occurred_at: updated_at,
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

/// Runs the `gh` that `locate_gh` found, in its own process group.
/// Standard error is discarded and nothing is echoed. A run past its time
/// limit has its whole group killed, so a child of `gh` that keeps standard
/// output open cannot hold the pull.
pub struct SystemGh {
    program: Option<std::path::PathBuf>,
    timeout: Duration,
    output_limit: usize,
}

impl SystemGh {
    /// The runner for the `gh` at `program`; None runs nothing and answers `Missing`.
    pub fn at(program: Option<std::path::PathBuf>) -> Self {
        Self { program, timeout: GH_TIMEOUT, output_limit: RESPONSE_LIMIT }
    }

    /// The runner for the `gh` that `find_gh` finds.
    pub fn located() -> Self {
        Self::at(find_gh())
    }

    /// The same runner with another time limit and output limit.
    pub fn with_limits(self, timeout: Duration, output_limit: usize) -> Self {
        Self { timeout, output_limit, ..self }
    }

    fn spawn(&self, program: &std::path::Path, args: &[String]) -> std::io::Result<std::process::Child> {
        let mut command = std::process::Command::new(program);
        command
            .args(args)
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("NO_COLOR", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        command.spawn()
    }
}

impl GhRunner for SystemGh {
    fn run(&self, args: &[String]) -> GhOutcome {
        let Some(program) = &self.program else {
            return GhOutcome::Missing;
        };
        let Ok(mut child) = self.spawn(program, args) else {
            return GhOutcome::Missing;
        };
        let deadline = Instant::now() + self.timeout;
        let Some(stdout) = child.stdout.take() else {
            stop(&mut child);
            return GhOutcome::Missing;
        };
        // Read on a thread so a full pipe never blocks; receive with a bound,
        // since a child of `gh` may hold the pipe open past `gh` itself.
        let (sender, receiver) = std::sync::mpsc::channel();
        let limit = self.output_limit;
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let read = stdout.take(limit as u64 + 1).read_to_end(&mut bytes).map(|_| bytes);
            let _ = sender.send(read);
        });
        let bytes = match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Ok(bytes)) if bytes.len() > limit => {
                stop(&mut child);
                return GhOutcome::Oversize(limit);
            }
            Ok(Ok(bytes)) => bytes,
            Ok(Err(_)) => Vec::new(),
            Err(_) => {
                stop(&mut child);
                return GhOutcome::TimedOut(self.timeout);
            }
        };
        let Some(status) = wait_until(&mut child, deadline) else {
            stop(&mut child);
            return GhOutcome::TimedOut(self.timeout);
        };
        match status.code() {
            Some(0) => GhOutcome::Output(bytes),
            Some(GH_SIGNED_OUT) => GhOutcome::SignedOut,
            code => GhOutcome::Exited(code),
        }
    }
}

/// The child's exit status once it exits, or None at `deadline`.
fn wait_until(child: &mut std::process::Child, deadline: Instant) -> Option<std::process::ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => return None,
        }
    }
}

/// Kill the child's whole process group, then reap the child. The group id
/// is the child's id, since it was spawned as a group leader.
fn stop(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let group = format!("-{}", child.id());
        let _ = std::process::Command::new("/bin/kill")
            .args(["-KILL", "--", &group])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
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

    /// A `gh pr list` node for pull request `number`, merged at `merged` and
    /// last updated at `updated`.
    pub(crate) fn pr(number: u64, title: &str, body: &str, merged: DateTime<Utc>, updated: DateTime<Utc>) -> Value {
        json!({
            "number": number, "title": title, "body": body, "author": {"login": "jdoe"},
            "mergedAt": merged.to_rfc3339(), "updatedAt": updated.to_rfc3339(),
            "url": format!("https://github.com/acme/app/pull/{number}"), "baseRefName": "main", "id": format!("PR_{number}")
        })
    }

    /// A `gh` over a set of merged pull requests that answers `pr list` the
    /// way GitHub does: the `updated:` qualifiers of `--search` filter, rows
    /// come most recently updated first, `--limit` caps them, and a reply
    /// over `RESPONSE_LIMIT` bytes is oversize.
    pub(crate) struct DatasetGh {
        pub nodes: Rc<RefCell<Vec<Value>>>,
        pub replies: Rc<RefCell<Vec<(Vec<String>, usize, usize)>>>,
    }

    impl DatasetGh {
        pub(crate) fn new(nodes: Vec<Value>) -> (Self, Rc<RefCell<Vec<Value>>>, Rc<RefCell<Vec<(Vec<String>, usize, usize)>>>) {
            let (nodes, replies) = (Rc::new(RefCell::new(nodes)), Rc::new(RefCell::new(vec![])));
            (Self { nodes: nodes.clone(), replies: replies.clone() }, nodes, replies)
        }
    }

    fn after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).map(String::as_str)
    }

    fn stamp(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&Utc)
    }

    impl GhRunner for DatasetGh {
        fn run(&self, args: &[String]) -> GhOutcome {
            if args.first().is_some_and(|a| a == "repo") {
                return GhOutcome::Output(b"{}".to_vec());
            }
            let limit: usize = after(args, "--limit").unwrap().parse().unwrap();
            let search = after(args, "--search").unwrap_or_default();
            let mut rows: Vec<Value> = self.nodes.borrow().clone();
            for term in search.split_whitespace() {
                if let Some(from) = term.strip_prefix("updated:>=") {
                    rows.retain(|r| time(r, "updatedAt").unwrap() >= stamp(from));
                } else if let Some((from, to)) = term.strip_prefix("updated:").and_then(|t| t.split_once("..")) {
                    rows.retain(|r| (stamp(from)..=stamp(to)).contains(&time(r, "updatedAt").unwrap()));
                }
            }
            rows.sort_by_key(|r| std::cmp::Reverse(time(r, "updatedAt").unwrap()));
            rows.truncate(limit);
            let bytes = serde_json::to_vec(&rows).unwrap();
            self.replies.borrow_mut().push((args.to_vec(), rows.len(), bytes.len()));
            match bytes.len() > RESPONSE_LIMIT {
                true => GhOutcome::Oversize(RESPONSE_LIMIT),
                false => GhOutcome::Output(bytes),
            }
        }
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
        assert!(fields.ends_with(",updatedAt"), "{fields}");
        let first = gh_list_args("acme/app", None, Some(FIRST_PULL_LIMIT)).join(" ");
        assert_eq!(first, format!("pr list --repo acme/app --state merged --limit 200 --json {fields} --search sort:updated-desc"));
        let since = gh_list_args("acme/app", Some(at(9)), None).join(" ");
        assert_eq!(since, format!("pr list --repo acme/app --state merged --limit 1000 --json {fields} --search is:merged updated:>=2026-09-01T09:00:00Z sort:updated-desc"));
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
        let (gh, _) = gh_with(GhOutcome::Exited(Some(1)));
        let (rest, calls) = rest_with(pages());
        let first = collect(&GitHub::new("acme/app", "github", gh, Some(rest)), None, false).unwrap();
        assert_eq!(first.len(), FIRST_PULL_LIMIT);
        assert_eq!(calls.borrow().len(), 2);

        let (gh, _) = gh_with(GhOutcome::Exited(Some(1)));
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
        let cases = [
            (GhOutcome::Missing, "github: unreachable, run `wardwell tracker connect github`"),
            (GhOutcome::SignedOut, "gh is not signed in; github: unreachable, run `wardwell tracker connect github`"),
        ];
        for (outcome, expected) in cases {
            let (gh, _) = gh_with(outcome);
            let error = collect(&GitHub::new("acme/app", "github", gh, None), None, false).unwrap_err();
            assert_eq!(error, expected);
            assert!(error.contains(UNREACHABLE), "{error}");
        }
        let (gh, _) = gh_with(GhOutcome::Missing);
        let named = GitHub::new("acme/app", "gh-work", gh, None).unreachable_message();
        assert_eq!(named, "github: unreachable, run `wardwell tracker connect gh-work`");
    }

    /// Each way `gh` can fail, with the sentence a pull reports for it.
    fn gh_failures() -> Vec<(GhOutcome, &'static str)> {
        vec![
            (GhOutcome::Exited(Some(1)), "gh exited with status 1"),
            (GhOutcome::Exited(None), "gh was stopped by a signal"),
            (GhOutcome::TimedOut(Duration::from_secs(120)), "gh did not finish within 120 seconds"),
            (GhOutcome::Oversize(RESPONSE_LIMIT), "gh output exceeded 16777216 bytes"),
            (GhOutcome::Output(b"not json".to_vec()), "gh returned output that is not a list of pull requests"),
        ]
    }

    #[test]
    fn each_gh_failure_has_its_own_sentence_and_is_never_unreachable() {
        for (outcome, sentence) in gh_failures() {
            let (gh, _) = gh_with(outcome);
            let error = collect(&GitHub::new("acme/app", "github", gh, None), None, false).unwrap_err();
            assert_eq!(error, sentence);
            assert!(!error.contains(UNREACHABLE) && !error.contains(AUTH_REFUSED), "{error}");
        }
    }

    #[test]
    fn a_failing_gh_falls_through_to_the_token() {
        for (outcome, sentence) in gh_failures().into_iter().chain([(GhOutcome::SignedOut, "gh is not signed in")]) {
            let (gh, _) = gh_with(outcome);
            let (rest, calls) = rest_with(vec![Ok(json!([rest_node(5, 10, true)]))]);
            let events = collect(&GitHub::new("acme/app", "github", gh, Some(rest)), None, false).unwrap();
            assert_eq!(events.len(), 1, "{sentence}");
            assert_eq!(calls.borrow().len(), 1, "{sentence}");
        }
        let (gh, _) = gh_with(GhOutcome::Exited(Some(1)));
        let (rest, _) = rest_with(vec![Err("GitHub returned HTTP 500".to_string())]);
        let error = collect(&GitHub::new("acme/app", "github", gh, Some(rest)), None, false).unwrap_err();
        assert_eq!(error, "gh exited with status 1; the API token read failed: GitHub returned HTTP 500");
    }

    #[test]
    fn a_refused_token_says_so_without_the_body() {
        assert_eq!(reply(401, Some(json!({"message": "Bad credentials ghp_secret"}))).unwrap_err(), format!("GitHub returned HTTP 401: {AUTH_REFUSED}"));
        assert_eq!(reply(404, None).unwrap_err(), NOT_FOUND);
        assert_eq!(reply(500, Some(json!({"message": "ghp_secret"}))).unwrap_err(), "GitHub returned HTTP 500");
        assert_eq!(reply(200, Some(json!([]))).unwrap(), json!([]));
    }

    /// An executable `gh` script in `dir` with `body` after the shebang.
    #[cfg(unix)]
    pub(crate) fn stub_gh(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("gh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn the_system_runner_reports_each_outcome_of_a_stub() {
        let dir = tempfile::tempdir().unwrap();
        let count = std::cell::Cell::new(0);
        let run = |body: &str, limit: usize| {
            count.set(count.get() + 1);
            let stub = stub_gh(&dir.path().join(count.get().to_string()), body);
            SystemGh::at(Some(stub)).with_limits(Duration::from_secs(1), limit).run(&strings(&["pr", "list"]))
        };
        assert_eq!(run("echo \"$1 $2\"", 1024), GhOutcome::Output(b"pr list\n".to_vec()), "arguments reach gh");
        assert_eq!(run("echo '[{\"number\":1}]'\nexit 1", 1024), GhOutcome::Exited(Some(1)), "JSON on stdout does not hide a failed exit");
        assert_eq!(run("echo '[]'\nexit 4", 1024), GhOutcome::SignedOut);
        assert_eq!(run("kill -TERM $$", 1024), GhOutcome::Exited(None));
        let big = "head -c 4096 /dev/zero | tr '\\0' 'x'";
        assert_eq!(run(big, 1024), GhOutcome::Oversize(1024));
        assert_eq!(run(big, 4096), GhOutcome::Output(vec![b'x'; 4096]), "exactly the limit is not oversize");
        let started = Instant::now();
        assert_eq!(run("sleep 30", 1024), GhOutcome::TimedOut(Duration::from_secs(1)));
        assert!(started.elapsed() < Duration::from_millis(2500), "{:?}", started.elapsed());
        assert_eq!(SystemGh::at(None).run(&[]), GhOutcome::Missing);
        assert_eq!(SystemGh::at(Some(dir.path().join("absent"))).run(&[]), GhOutcome::Missing);
    }

    #[cfg(unix)]
    #[test]
    fn a_child_of_gh_holding_stdout_does_not_outlast_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let timeout = Duration::from_secs(1);
        for (n, body) in ["sleep 30 &\necho '[]'\nexit 0", "echo '['\nsleep 30 &\nsleep 30"].into_iter().enumerate() {
            let stub = stub_gh(&dir.path().join(n.to_string()), body);
            let runner = SystemGh::at(Some(stub)).with_limits(timeout, RESPONSE_LIMIT);
            let started = Instant::now();
            assert_eq!(runner.run(&[]), GhOutcome::TimedOut(timeout), "{body}");
            assert!(started.elapsed() < timeout + Duration::from_millis(1500), "{body}: {:?}", started.elapsed());
        }
    }

    #[cfg(unix)]
    #[test]
    fn gh_is_found_on_path_then_at_the_candidates_without_running_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let on_path = dir.path().join("bin");
        let homebrew = dir.path().join("homebrew");
        let local = dir.path().join("local");
        let candidates = vec![homebrew.join("gh"), local.join("gh")];
        let bare = std::ffi::OsString::from("/usr/bin:/bin:/usr/sbin:/sbin");
        let path = std::env::join_paths([dir.path().join("none"), on_path.clone()]).unwrap();
        assert_eq!(locate_gh(Some(&bare), &candidates), None);
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(local.join("gh"), "#!/bin/sh\nexit 1\n").unwrap();
        assert_eq!(locate_gh(Some(&bare), &candidates), None, "not executable");
        std::fs::set_permissions(local.join("gh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(locate_gh(Some(&bare), &candidates), Some(local.join("gh")));
        stub_gh(&homebrew, "exit 1");
        assert_eq!(locate_gh(Some(&bare), &candidates), Some(homebrew.join("gh")), "the Homebrew path comes first");
        stub_gh(&on_path, "exit 1");
        assert_eq!(locate_gh(Some(&path), &candidates), Some(on_path.join("gh")), "PATH comes before the candidates");
        assert_eq!(locate_gh(None, &candidates), Some(homebrew.join("gh")));
        assert_eq!(GH_CANDIDATES, ["/opt/homebrew/bin/gh", "/usr/local/bin/gh"]);
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
            assert_eq!(adapter.check().map_err(|(code, _)| code), expected);
            if let Some(paths) = paths {
                assert_eq!(*paths.borrow(), vec!["/repos/acme/app"]);
            }
        }
    }
}
