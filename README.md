# Wardwell

Persistent project memory for Claude Code. MCP server + CLI that gives your AI knowledge about your projects — what they are, where you left off, and what to do next.

Every Claude Code session starts from zero. You re-explain context, re-upload files, re-teach preferences. Wardwell fixes that. It indexes your project notes into a searchable vault, syncs project state as you work, and injects context automatically when you start a session.

## Install

```bash
brew tap yakschuss/wardwell
brew install wardwell
```

Or build from source:

```bash
cargo install --path .
```

Then run first-time setup:

```bash
wardwell init
```

This creates `~/.wardwell/`, generates a config, registers the MCP server in Claude Code, and installs the SessionStart hook. It walks you through each step interactively.

On an additional computer, or to repair agent connections without touching the
vault, run:

```bash
wardwell setup --dry-run
wardwell setup
```

`setup` reconciles Wardwell's user-scoped entries in Claude Code, Claude
Desktop, and Codex. It preserves unrelated MCP servers, backs up changed client
configs, and leaves hosted access disconnected until you approve OAuth.

## How It Works

Wardwell has three pieces:

1. **MCP server** — Claude Code connects to it automatically, giving the AI tools to search, read, and write your vault
2. **SessionStart hook** — injects project context when you open a Claude Code session, before you type anything
3. **Background services** — watches for file changes, indexes session history, generates summaries

### Vault Structure

Your vault is a directory of markdown and JSONL files. Domains are top-level folders (areas of work). Projects are subfolders within them:

```
vault_path/
  work/
    my-project/
      INDEX.md            # what this project is and why it matters
      current_state.md    # live state — focus, next action, blockers
      decisions.md        # architectural decisions with context
      history.jsonl       # timestamped log of what happened
      lessons.jsonl       # what went wrong, root cause, prevention
  personal/
    side-project/
      INDEX.md
      current_state.md
```

`INDEX.md` and `current_state.md` are created by `wardwell seed`. The rest are created automatically by the AI as you work — it syncs state, records decisions, and logs history through the MCP tools.

### File Formats

**current_state.md** — YAML frontmatter + markdown. The AI replaces this file on each sync:

```markdown
---
chat_name: my-project
updated: 2026-02-22 14:30
status: active
type: project
context: work
---

# My Project

## Focus
Implementing the authentication flow

## Next Action
Write integration tests for OAuth callback

## Commit Message
Add OAuth provider configuration
```

**history.jsonl** — append-only log. First line is a schema header:

```jsonl
{"_schema": "history", "_version": "1.0"}
{"date":"2026-02-22T14:30:00Z","title":"Add OAuth","status":"active","focus":"auth flow","next_action":"write tests","commit":"Add OAuth config","body":"Integrated OAuth2 provider..."}
```

**decisions.md** — newest first, prepended on each write:

```markdown
## 2026-02-22 — Use OAuth over JWT

OAuth gives us delegated auth without managing tokens ourselves.
Tradeoff: more redirect complexity, but we get refresh tokens for free.

---
```

**lessons.jsonl** — structured post-mortems:

```jsonl
{"_schema": "lessons", "_version": "1.0"}
{"date":"2026-02-22","title":"FTS5 duplicate entries","what_happened":"Re-indexed all files on every restart","root_cause":"No existence check before insert","prevention":"Use upsert pattern"}
```

## MCP Tools

Wardwell exposes three tools to Claude Code via the [Model Context Protocol](https://modelcontextprotocol.io):

### wardwell_search

Search the vault, read files, query history, or get a prioritized work queue.

| Action | Required params | What it does |
|-|-|-|
| `search` | `query` | Full-text search across all indexed vault files |
| `read` | `path` | Read a file by path (relative to vault root or absolute) |
| `history` | `query` | Search across history.jsonl files. Optional: `domain`, `project`, `since` |
| `orchestrate` | — | Returns prioritized queue: active projects, blocked, recently completed |
| `context` | `session_id` | Full context for a Claude Code session: summary, vault state, related files |

Optional on all: `domain` (filter to domain), `limit` (max results, default 5).

### wardwell_write

Write project state, record decisions, log history, or store lessons.

| Action | Required params | What it does |
|-|-|-|
| `sync` | `domain`, `project`, `snapshot` | Replaces current_state.md. Optionally appends to history.jsonl |
| `decide` | `domain`, `project`, `decision` | Prepends to decisions.md |
| `append_history` | `domain`, `project`, `history_entry` | Appends to history.jsonl without changing state |
| `lesson` | `domain`, `project`, `lesson` | Appends to lessons.jsonl |

**snapshot** fields: `status`, `focus`, `next_action`, `commit_message` (required), `why_this_matters`, `open_questions`, `blockers`, `waiting_on` (optional).

**decision** fields: `title`, `body`.

**history_entry** fields: `title`, `body`.

**lesson** fields: `title`, `what_happened`, `root_cause`, `prevention`.

### wardwell_clipboard

Copies content to the system clipboard via `pbcopy`. The AI is instructed to always ask permission before using this.

## SessionStart Hook

When you open a Claude Code session, wardwell checks if your current directory name matches a domain folder in your vault. If it does, it prints a summary of active projects and their state — this gets injected into the session as context.

The hook runs `wardwell inject "$(pwd)"` and outputs the content of `current_state.md` files found under the matching domain.

## CLI Commands

```
wardwell serve                Start the MCP server (full access)
wardwell serve --domain work  Start scoped to a specific domain
wardwell init                 First-run setup — interactive walkthrough
wardwell setup --dry-run      Preview agent config repair; never changes the vault
wardwell setup                Configure detected agents, with backups and one consent gate
wardwell doctor               Check that everything is wired correctly
wardwell uninstall            Clean removal — MCP entries, hooks, markers (preserves vault)
wardwell inject .             Output project context for a directory (used by hooks)
wardwell reindex              Rebuild the vault search index from scratch
wardwell seed <path>          Create domain or project folders
wardwell tracker connect <name> --token-stdin   Store a tracker API token
wardwell tracker pull [--project <d/p>] [--full] Mirror tracker events into the vault
wardwell tracker status       Last pull, last error, last full resync, event count per project
wardwell tracker doctor       Check each binding's credential, auth and team
wardwell tracker compact [--project <d/p>] [--force]   Move raw payloads to the sidecar, drop duplicates
```

### wardwell init

Interactive setup that walks you through:

1. Detecting or choosing your vault path (auto-detects Obsidian vaults)
2. Previewing all mutations before making them
3. Injecting the MCP server config into Claude Code and Claude Desktop
4. Installing the SessionStart hook
5. Injecting wardwell markers into CLAUDE.md
6. Building the search index

Each step can be skipped. Skipped steps are listed at the end with manual instructions. Re-running `init` is safe — it detects existing config and updates in place.

### wardwell setup

Configures or repairs this computer after Wardwell itself is installed. Unlike
`init`, it does not inspect, create, index, or change vault files. Before any
write it preflights every detected client and aborts on malformed or conflicting
configuration. OAuth approval and a successful publish/refresh remain required
before the hosted app is considered connected.

### wardwell seed

Scaffold a new domain or project:

```bash
# Create a domain directory
wardwell seed work

# Create a project with INDEX.md + current_state.md templates
wardwell seed work/my-project
```

Seed is additive only — it refuses to overwrite existing projects.

### wardwell doctor

Checks that everything is wired correctly:

- Config exists and parses
- Vault directory exists with indexed files
- Domains detected
- Index built
- Local context and hosted-app MCP entries configured in Claude Code and Codex
- Local context configured in Claude Desktop; hosted access remains an account connector
- SessionStart hook registered
- Claude CLI available (for summarizer)

`doctor` verifies configuration, not authorization. Complete OAuth and publish
or refresh one brief to prove the live end-to-end connection.

## Optional Hank Companion (customer-zero candidate)

The existing `wardwell-context` connection also exposes `wardwell_companion`.
It forwards only explicitly supplied Companion evidence and work; it does not
upload vault files or change local kanban. Local tools remain usable when Hank
is disconnected.

Use `status` to check the hosted connection and `schema` to discover the current
hosted capture/work inputs. Source-owned calls supply the stable `source_key`
from their conversation journal and an `arguments` object. `discover` is a
tenant-scoped read and does not require a source key:

For large publish payloads, `arguments_file` may replace `arguments`. It must be
an absolute path to a regular JSON file no larger than 200,000 bytes; the binary
reads and validates it locally and never sends the path to Hank. `list`, `get`,
and `discover` also accept an optional boolean `arguments.compact`.

| Action | Hosted operation |
| --- | --- |
| `capture` | `capture_submit`; `conversation_key` must match `source_key` |
| `publish` | `work_plan_publish`; source key must match and revisions use CAS |
| `discover` | List tenant-visible plans whose `workstream` exactly matches the required nonempty `arguments.workstream` string |
| `list` | List only this source's plans, for recovery |
| `get` | Read a plan after checking its source key |
| `responses` | Read the source's durable owner responses and current source document |
| `consume` | Stage the next response page in a private local journal; returns existing staged responses before fetching again |
| `acknowledge` | Record that named staged observation IDs were persisted locally; this does not claim execution or completion |

Use `discover` when an agent needs exact targets from another session before it
adds a cited cross-plan reference to its own next WorkPlan version. Discovery
does not grant access to another source's `get` or `responses` calls and does not
transfer execution authority.

Conversation keys prevent accidental cross-session routing; they are not
credentials or isolation against other processes on the same OS account. The
hosted installation credential establishes customer/workspace permissions.
Publication and saved answers do not authorize external execution.

The binary reads its private connection from
`~/.wardwell/hank/connection.json` (under `WARDWELL_CONFIG_DIR` when set).
The customer-zero bootstrap accepts an already-issued installation credential
via `wardwell companion connect --token-stdin`; never put credentials in command
arguments or agent prompts. `wardwell companion status` checks hosted read access
without displaying credentials. Interactive account sign-in and renewal are not
yet implemented by this slice.

Keep one durable source journal per conversation. On transport failure, retain
pending work and reconcile through `list`, `get`, or `responses` before replaying
the exact request. The forwarding layer does not retry writes automatically or
wake idle agents. This version supports the hosted endpoint's JSON responses;
SSE-only endpoints are rejected explicitly. No kanban migration is required.
`consume` stores journals under the Wardwell config directory with owner-only
permissions and will not advance pagination while staged observations remain.

## Config

Config lives at `~/.wardwell/config.yml`. Generated by `wardwell init`.

```yaml
# Wardwell config

vault_path: ~/Notes

session_sources:
  - ~/.claude/projects/

exclude:
  - node_modules
  - .git
  - vendor
  - target
  - .obsidian
  - .trash
```

| Key | What it does |
|-|-|
| `vault_path` | Root directory — domains and projects live here, indexed for search |
| `session_sources` | Directories containing Claude Code session data (for session indexer) |
| `exclude` | Directory/file names to skip during indexing |
| `domains` | Optional domain config with path patterns and aliases (migration path) |
| `ai.summarize_model` | Claude model for session summarization (default: `haiku`) |
| `trackers` | Optional `<domain>/<project>` bindings to an issue tracker (see Tracker mirror) |

## Tracker mirror

Wardwell can mirror an external issue tracker into the vault as a read-only
event log that pulls only append to. Each bound project gets `<domain>/<project>/tracker.jsonl`
(header `{"_schema":"tracker","_version":"1.0"}`), indexed like any other JSONL,
so `wardwell_search` finds tracker history. A ticket key in a query, such as
`COR-12` or `COR-12 OR COR-13`, is matched as a key. Linear is the first
provider. Wardwell never writes back to the tracker.

One tracker per project: a binding maps one project folder to one team. A
second tracker, or a second team, is a second project folder with its own
binding and credential.

Bind projects in `~/.wardwell/config.yml`:

```yaml
trackers:
  work/claims:            # <domain>/<project>
    provider: linear
    team: COR             # Linear team key
    credential: corr-linear
    readonly: true        # refuse kanban writes on this project
```

Store the token (a Linear personal API key) from standard input. It is written to
`~/.wardwell/trackers/<name>.json` with owner-only permissions and is never
printed or logged:

```sh
pbpaste | wardwell tracker connect corr-linear --token-stdin
```

Then:

```sh
wardwell tracker pull                          # every bound project
wardwell tracker pull --project work/claims    # one project
wardwell tracker pull --full                   # re-pull everything, record removals
wardwell tracker pull --full --allow-empty     # accept an empty result and remove every issue
wardwell tracker status                        # every binding: last pull, last error
wardwell tracker doctor                        # credential, auth, team per binding
wardwell tracker compact [--project work/claims] [--force]
```

Each event carries `kind`, which is one of `issue_upserted`, `comment_upserted`,
`state_changed`, `link_added`, `issue_removed`, `full_resync`, `pull_completed`
or `pull_failed`. It also carries `provider`, `external_key` such as `COR-12`,
`external_id`, `actor`, `occurred_at`, and a readable `title`. An `issue_upserted` snapshot holds the
issue's fields in provider-neutral names: `issue_title`, `description`,
`state`, `state_category`, `priority`, `team`, `project`, `assignee`,
`creator`, `labels`, `url`, `created_at`, `archived_at`, `parent_key`,
`branch_name`, and `relations`, a list of `{kind, key}` with kind `related`,
`blocks`, `blocked_by` or `duplicate_of`. A provider link type outside those
four stays only in the raw payload. The title of a sub-issue's snapshot names
its parent. A snapshot's id includes a short digest of its structure, so a
re-parent or a new relation records a new snapshot even when the provider did
not bump the issue's update time.

The log rows are light. The provider's raw payload for each event goes to
`tracker.raw.jsonl` beside the log, one line per event id, written before the
log row. The indexer and the watcher skip every `*.raw.jsonl` file. Logs
written by earlier versions carry `raw` inline and still read; `wardwell
tracker compact` migrates them. There is no cursor file: each
pull that delivers every page ends with a `pull_completed` marker whose `through`
is the newest provider time seen, and the next incremental pull starts one hour
before the latest marker's `through`. Pages are appended as they arrive and
duplicate events are skipped by id, so a pull that fails part way keeps what it
read but does not move the cursor; the next pull re-requests from the same
point whatever order the provider returned pages in. With no marker, a pull
starts from the beginning. `--full` re-pulls every issue (archived included),
appends `issue_removed` for issues the tracker no longer returns, and ends with
a `full_resync` marker that also sets the cursor. Linear does not timestamp
every change; a new relation or the archive of an old issue can leave the
update time alone, so an incremental pull would miss it. A pull therefore runs
full, and says so in its output line, when the newest `full_resync` marker is
more than 24 hours old or there is none. With the hourly schedule that is one
full pull a day. A full pull that returns no issues while the mirror holds
open ones removes nothing: it fails with `empty_full_result`, since a renamed
team key or a token that lost access looks the same as an emptied tracker.
Only `--full --allow-empty` accepts an empty result; the automatic full pull
never does. When an automatic full pull fails, the same run goes on with
an incremental pull and the output line reports both; the run still exits
non-zero. No automatic full is tried again for 6 hours after that failure,
so incremental pulls keep the mirror moving meanwhile. An explicit `--full`
is never held back. An issue that was removed and then comes back reappears: its
snapshot is appended even though its id is in the log, under the id
suffixed `:restored:<removal time>`. `status` reads the last pull
time from the latest marker. The mirror is not authoritative; if the
tracker goes away, the log stays as a searchable archive.

One binding that fails does not stop the others. A failure after the log is
open appends a `pull_failed` marker with a closed `code` and no provider text.
The codes are `auth` when the provider refuses the token, `provider` for any
other provider failure, `empty_full_result`, `log_read` and `log_write`. Three failures write
nothing to the log: a missing credential is `credential`, an unknown provider
is `unsupported_provider`, and a held lock is `lock_busy`. `status` lists every
binding with its last error. It also checks, without a network call, that the
provider is known and the credential reads, and prints `cannot pull` with the
code when either fails; it says `no errors` only when both pass and no pull
failed. `pull` exits non-zero when any binding failed.

`doctor` prints three lines per binding: whether the credential file exists
with owner-only permissions, whether the provider accepts the token on one
cheap request, and whether the team key resolves. A failure names one code:
`credential`, `auth`, `provider` or `team_not_found`. `auth` means the
provider refused the token; any other provider error, including a GraphQL
error that is not about authentication, is `provider`. It never prints a
token. `config.yml` is rejected at load when a binding names a provider
Wardwell has no adapter for; the error lists the supported ones. Should one
reach `doctor` anyway, its second line reads `provider failed
(unsupported_provider)`.

`compact` is the only command that rewrites a tracker log, and only
`tracker.jsonl`; it may because the log is a re-pullable mirror, not a system
of record. It moves inline `raw` into the sidecar and removes exact duplicate
events. It takes a per-project lock file, `tracker.lock`, that `pull` also
takes. A pull that finds the lock held waits up to 30 seconds, then fails with
`lock_busy`. Before it writes anything it refuses a log in which two rows
share an id but differ; it names the id, `--force` does not override it, and
the two rows must be resolved by hand. It then writes the sidecar, verifies
that every moved payload reads back from it, writes the new log beside the old
one, links the old one to `tracker.jsonl.bak.new`, renames the new log into
place, and only then renames `.bak.new` over `tracker.jsonl.bak`. If any step
fails, the log and the previous backup are left as they were. The
backup stays until the next compact, which refuses to run while it exists
unless given `--force`. A log that is already compact is left alone. A
compacted log is searchable by text at once; search by meaning returns for it
after you run `wardwell reindex`.

With `readonly: true`, every kanban MCP action that appends to a file in that
project's folder or its ticket audit log (create, update, move, note, attach,
detach, sequence, groom, relationship_create, relationship_delete,
question_create, question_update, question_answer, question_invalidate,
proposal_create, proposal_approve, proposal_reject, proposal_apply, verify,
status, and export_roadmap, which saves a PDF into the project folder) refuses
and says to edit in the tracker. Reads are unaffected.

The kanban read actions `get`, `list`, `query` and `search` include the
mirrored issues of a bound project. A mirrored item has `origin: "tracker"`
and carries the provider, the external key, the tracker's state name and
category, the parent, the relations, the url, `last_pulled_at`, and that
pull's age in plain words, such as `3 hours ago`. A native kanban item keeps
its fields, including its own `source`, and gains `origin: "kanban"`. `list`
and `search` leave out removed and archived issues; `list` also leaves out
completed and canceled ones unless `include_done` is set. The mirror has no
epics or deadlines, so of the named queries it answers only `recent` and
`stale`. Any other query returns a `tracker_note` that says so. Outside
`get`, mirrored items carry no description. They sort by key with the number
compared as a number, so COR-2 comes before COR-10. `list` and `query` return
at most 50 mirrored items and `search` at most 20. When more match, the
result adds `tracker_truncated` with the count left out.

`get` for a key the mirror does not hold runs one incremental pull for the
binding that owns the key, then looks again. It never runs a full pull, and
each binding refreshes at most once a minute. The result carries `refreshed`,
true when a pull completed, and `refresh_reason`: `found_after_pull`,
`still_missing`, `cooldown`, `pull_failed:<code>`, `no_binding` or
`never_pulled`. A mirror with no pull marker is not refreshed, because an
incremental pull with no cursor would read the whole team. Run `wardwell
tracker pull` first. `list`, `query` and `search` never pull.

At session start, `wardwell inject` prints a rot line for each project
folder in the matched domain. It reads like this: Last history entry 12 days
ago. Last decision 3 days ago. A bound project also gets a tracker section.
Its first line is "Tracker mirror. Last pulled 2 hours ago. Not
authoritative." Up to ten issues in a started state follow, each with key,
title and state. When the last pull failed or is more than 24 hours old, the
section shows only the age and that notice.

`wardwell doctor` prints one row per binding. It checks only the credential
file and the provider, with no network call, and names `wardwell tracker
doctor` for the live check.

```sh
wardwell tracker schedule [--interval-seconds 3600]
wardwell tracker unschedule
```

`schedule` installs a launchd agent (macOS) that runs `wardwell tracker pull` at
load and every interval, using the binary you ran it with, and replaces any
existing agent of the same label; output goes to `~/.wardwell/tracker-pull.log`.
The interval must be 60 to 2147483647 seconds. On other hosts `schedule` prints a
crontab line instead, marked approximate when cron cannot express the interval.
`unschedule` stops the agent and removes its plist. `status` reports the interval
read from the plist on disk, not whether launchd has the job loaded.

Follow-up, not in this version: importing a tracker's CSV or JSON export from a
file instead of pulling over the API. Also a follow-up: a debounce in the
watcher. Each change to a tracker log makes the indexer hash every indexed
line to detect a rewrite; an index on `vault_chunks(path, chunk_index)`,
added to an existing `index.db` when it opens, keeps the stored-hash lookup
to that file's rows.

## Domain Scoping

Wardwell supports domain-level access control. When started with `--domain`, the server is scoped to that domain and its `can_read` peers — all other domains are invisible.

```bash
# Scoped to work domain only
wardwell serve --domain work

# Or via environment variable
WARDWELL_DOMAIN=work wardwell serve
```

In scoped mode:
- **Search** returns only results from allowed domains
- **Read** rejects paths outside allowed domains
- **Write** rejects writes to other domains
- **Orchestrate/history/retrospective/patterns** only see allowed domains
- Client-provided domain parameters cannot override the scope

Without `--domain`, the server runs in domainless mode with full access (backwards compatible).

### MCP config for multi-domain isolation

```json
{
  "mcpServers": {
    "wardwell-work": {
      "command": "wardwell",
      "args": ["serve", "--domain", "work"]
    },
    "wardwell-personal": {
      "command": "wardwell",
      "args": ["serve", "--domain", "personal"]
    }
  }
}
```

### Cross-domain read access

Domains can grant read access to other domains via `can_read` in the domain vault file:

```yaml
---
type: domain
domain: work
confidence: confirmed
can_read:
  - shared
---
```

This allows the `work` session to search and read from `shared`, but not write to it.

## Background Services

When running as an MCP server (`wardwell serve`), Wardwell runs background tasks:

- **File watcher** — detects vault changes and updates the FTS5 search index in real time
- **Session indexer** — processes Claude Code session JSONL files from `session_sources`
- **Summarizer** — generates session summaries using `claude` CLI (runs every 5 minutes)

## Architecture

Single Rust binary, no runtime dependencies beyond `claude` CLI (optional, for summarization).

- **Search** — SQLite FTS5 full-text search with fuzzy fallback via string similarity
- **Storage** — plain markdown and JSONL files on disk. No proprietary format, no lock-in
- **MCP** — [rmcp](https://github.com/anthropics/rmcp) framework, stdio transport
- **File watching** — [notify](https://github.com/notify-rs/notify) for cross-platform filesystem events

### What lives where

| Path | Contents |
|-|-|
| `~/.wardwell/config.yml` | Configuration |
| `~/.wardwell/index.db` | SQLite FTS5 search index |
| `~/.wardwell/sessions.db` | Session metadata index |
| `~/.wardwell/summaries/` | Cached session summaries |
| `{vault_path}/` | Your vault — domains, projects, knowledge |

## Development

```bash
# Lint (must pass clean — warnings are errors)
cargo clippy --lib --bin wardwell

# Test (193 tests)
cargo test

# Build release
cargo build --release
```

Strict lints: `deny(clippy::unwrap_used, expect_used, panic, todo, unimplemented)`. Zero `unsafe` blocks.

## Requirements

- macOS (Apple Silicon) or Linux (x86_64)
- Claude Code (for MCP integration)
- `claude` CLI (optional — only needed for session summarization)

Intel Macs are not supported — the ONNX Runtime dependency (used for semantic search embeddings) does not provide prebuilt binaries for x86_64-apple-darwin. You can build from source with `cargo install --path .` if you need it, but embedding may not work.

## License

Business Source License 1.1 — see [LICENSE](LICENSE). Non-commercial use permitted. Converts to Apache 2.0 on April 6, 2030.
