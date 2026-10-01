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

Then run three commands:

```bash
wardwell init       # first run: vault, config, agent entries, hooks, index
wardwell setup      # preview, then install or repair hooks, tracker policy and the pull
wardwell doctor     # check the wiring; it says what it cannot check
```

`init` creates `~/.wardwell/`, writes a config, registers Wardwell in your
agent clients, installs the hooks, and builds the index. It walks you through
each step.

`setup` is safe to run at any time, on any computer. It never reads or changes
the vault. It shows a plan first, with CREATE, UPDATE + BACKUP or UNCHANGED on
each line. `--dry-run` writes nothing. It asks once before it writes. `--yes`
skips the question. A second run changes nothing and says so. Before any write
it reads and checks every file it will change. A malformed or conflicting file
stops the run, and nothing is written. It keeps every entry that is not
Wardwell's. A hook is Wardwell's only when its program is a Wardwell binary
and its arguments are exactly Wardwell's. Each changed file is saved beside
itself first, with owner-only permissions, and written through a temp file
and a rename.

A rewrite of `~/.claude/settings.json` uses two-space indentation, LF line
ends, and a final newline. Key order, number text and string escapes stay as
you wrote them. A file already in that layout stays byte-identical outside
Wardwell's entries. When a rewrite will change the file's layout, the plan
line says "UPDATE + BACKUP, reformats the file", and the backup keeps the
original.

`setup` installs in two tiers:

- **Memory, always.** The session-start hook and the Stop hook in
  `~/.claude/settings.json`. When the Companion Stop hook is installed, it
  already runs the history check, so no second Stop hook is added.
- **Tracker policy, optional.** Only when a tracker binding has `provider:
  linear` and `gate: true`. The Linear gate, a PreToolUse hook, and a deny
  list for destructive Linear tools. See [Linear gate](#linear-gate).

When any binding exists, `setup` also installs the hourly tracker pull, a
launchd agent on macOS. On other hosts it prints a crontab line. When no
binding is left, `setup` removes the agent.

Installed is not active. Claude Code reads hooks and permissions when a
session starts. Sessions already running do not change. Start a new session,
then run `wardwell doctor`.

`setup` also reconciles Wardwell's connection entries in Claude Code, Claude
Desktop, and Codex. It preserves unrelated connections and leaves hosted access
disconnected until you approve OAuth.

To remove Wardwell's wiring:

```bash
wardwell uninstall
```

It removes Wardwell's connection entries, its hooks, the deny entries it
added and no others, the pull service, and its CLAUDE.md markers. It never
deletes the Wardwell folder. It leaves the Companion install whole and says so.

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

When you open a Claude Code session, the hook runs `wardwell inject "$(pwd)"`. Wardwell decides which project the directory belongs to, in this order:

1. A git worktree counts as its main checkout. A linked worktree anywhere on disk resolves like the repository it came from.
2. The longest directory under `projects:` in config.yml that contains the directory wins.
3. A directory named like a vault domain folder prints that domain's projects, as before.

A mapping whose vault folder does not exist is skipped, and the next rule applies. `wardwell doctor` reports the missing folder.

A mapped project prints its summary, a rot line with the age of its last history entry and last decision, and its tracker section when it has a binding. No match prints nothing.

### Link a repository to a project

Run this once in the repository:

```bash
wardwell project link personal/corr-platform
```

Wardwell shows the change first. It asks once before writing. `--dry-run` writes nothing. `--yes` skips the question. `--path <dir>` links another directory. Leave out the project to use the one vault project named like the directory.

The command adds the directory under `projects:` in config.yml. Comments and other keys stay as they are. The old file is saved beside it with owner-only permissions. A second run changes nothing and says so. A project folder that does not exist in the vault is refused; create it with `wardwell seed` first. A linked worktree is refused. Link its main checkout instead; worktrees resolve through it. `wardwell doctor` fails a mapped path that is a linked worktree and names the main checkout.

`wardwell project list` shows each project and its directories. Sessions already running do not change. The mapping applies from the next session.

## Linear gate

The tracker policy tier. Turn it on per binding with `gate: true`:

```yaml
trackers:
  personal/corr-platform:
    provider: linear
    team: COR
    credential: corr-linear
    gate: true
```

Then run `wardwell setup`. The plan shows each line under "Tracker policy,
optional". It adds two things to `~/.claude/settings.json`:

- A PreToolUse hook with matcher `mcp__linear__save_comment|mcp__linear__save_issue`
  that runs `wardwell gate linear`.
- Deny entries for the destructive Linear tools: delete comment, delete
  attachment, retire issue label, retire project label, save project, delete
  status update, delete diff comment.

When a hook entry runs the old Python gate, `linear-gate.py`, setup removes
that entry and says so in the plan. The script file stays. With the policy
off, setup leaves the Python entry in place and says how to replace it.
Setting `gate: false` later and running setup removes the gate and the deny
entries Wardwell added.

The gate reads the PreToolUse payload on standard input. It prints a deny
decision with the reason, or nothing to allow. It always exits 0. A payload it
cannot read is allowed. The rules are the ruleset `linear-updates`, version 1,
held as data in the binary:

- A comment's first line starts with `Shipped:`, `Needs info:`, `Blocked:` or
  `Follow-up:`, and text follows the colon.
- The comment has a "For the team" section, and each field of its shape is
  present and not empty.
- A created issue has the fields Asked by, What changes for whom, and Done
  when, and a parent issue or a template.
- The team section has fewer than 80 words. Each sentence has 20 words or
  fewer. No parentheses, semicolons, dashes, "e.g." or "i.e.".
- A comment's team section has no backticks, paths, links, a number sign
  followed by digits, snake_case or CamelCase tokens, file extensions, or the
  words PR, merge, deploy, migration, webhook, adapter.
- Each `COR-` key is `COR-` and digits. Each date is valid.
- A session sets no priority. It sets no state, except `state: "Triage"` when
  it creates an issue.

A per-project override, such as a `.wardwell/tracker-updates.md` file, is a
follow-up. This version does not read one.

## Install record

Some entries cannot be proven Wardwell's by their shape. A deny entry such as
`mcp__linear__save_project` may also be one you added yourself. So setup
records the deny entries it adds in `~/.wardwell/install-manifest.json`:

```json
{
  "version": 1,
  "claude_permissions_deny": ["mcp__linear__delete_comment"],
  "created_keys": ["hooks"]
}
```

An entry you already had is never recorded, and no entry is listed twice.
Uninstall, or setup with the policy off, removes only the recorded entries.
The record also lists the settings keys Wardwell created, `hooks`,
`permissions` and `permissions.deny`, as `created_keys`. Uninstall removes
such a key only when it is listed there and is empty, so a key you had stays. Hooks are not recorded; they
are matched by binary name and arguments. The file lives under
`WARDWELL_CONFIG_DIR` when that is set.

## Stop check

When a session stops, Wardwell checks that work was recorded. In a mapped project with a vault folder, it counts the commits this worktree made since the session began. It reads the worktree's own HEAD reflog. A pull, a checkout, a rebase or a merge does not count, so other people's commits never do. With commits and no history entry written since then, it blocks the stop once with one line, for example:

```
2 commits since 14:02, no history entry. Run wardwell_write append_history for personal/corr-platform, or set WARDWELL_STOP_CHECK=off.
```

The session begins when the Companion lifecycle hooks first record it. The time is stored once in that session's own file. Without those hooks there is no start time, and the check allows.

- It blocks at most once per session.
- It allows when the agent is already continuing from a Stop block.
- It allows on any error or timeout, and when the project has no vault folder.
- It reads only the local git repository. Merged pull requests are not counted.
- An amend counts as the commit it replaces. A session that only amended counts one.
- These do not count, because git records them as something other than a commit:
  - a cherry-pick
  - a revert
  - `git am`
  - a pull
  - a checkout
  - a rebase
  - a merge, unless you finish it with your own `git commit`
- Only a history entry in this project counts. An entry written to a different project does not satisfy the check for this one.
- Two sessions that share one checkout cannot be told apart. A commit by either counts for both. Give each session its own worktree.
- Each block is logged to `~/.wardwell/stop-check/blocks.jsonl`. `wardwell doctor` shows the last one per project.
- `WARDWELL_STOP_CHECK=off` turns it off. So does `stop_hook: false` in config.yml.

The Companion Stop check runs first. When both block, one block carries both reasons, the Companion's first.

## CLI Commands

```
wardwell serve                Start the MCP server (full access)
wardwell serve --domain work  Start scoped to a specific domain
wardwell init                 First-run setup — interactive walkthrough
wardwell setup --dry-run      Preview agent config, hooks, tracker policy and pull; never changes the vault
wardwell setup [--yes]        Apply that plan, with backups and one consent gate
wardwell doctor               Check that everything is wired correctly
wardwell uninstall            Remove Wardwell's entries, hooks, deny entries and pull (preserves vault)
wardwell gate linear          PreToolUse hook: check a Linear write (reads JSON from stdin)
wardwell inject .             Output project context for a directory, used by hooks
wardwell project link [<d/p>] [--path <dir>] [--dry-run] [--yes]   Link a directory to a vault project
wardwell project list         Show each linked project and its directories
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
3. Adding Wardwell's connection to Claude Code and Claude Desktop
4. Installing the hooks, with the same preview, backups and exact matching as `setup`
5. Injecting wardwell markers into CLAUDE.md
6. Building the search index

Each step can be skipped. Skipped steps are listed at the end with manual instructions. Re-running `init` is safe — it detects existing config and updates in place.

### wardwell setup

Configures or repairs this computer after Wardwell itself is installed. Unlike
`init`, it does not inspect, create, index, or change vault files. Before any
write it preflights every detected client and every settings file, and aborts
on malformed or conflicting configuration. It installs the two tiers described
under [Install](#install) and the tracker pull. `init` uses the same installer
for its hooks. OAuth approval and a successful publish/refresh remain required
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
- Local context and hosted-app connection entries configured in Claude Code and Codex
- Local context configured in Claude Desktop; hosted access remains an account connector
- SessionStart hook registered
- Gate ruleset: its name and version, `linear-updates v1`
- Linear gate: installed when a linear binding has `gate: true`, and running this binary
- Linear deny list: every destructive Linear tool denied
- Tracker pull service: the plist is present and its program exists. Whether launchd loaded it is not checked.
- Each linked project: whether each directory exists, the age of the last history entry and last decision, the last pull when bound, and the last stop-check block
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
| `trackers` | Optional `<domain>/<project>` bindings to an issue tracker (see Tracker mirror). `gate: true` on a linear binding turns on the Linear gate (see Linear gate). |
| `projects` | Optional `<domain>/<project>` entries, each with `paths:`, a list of directories. Session start and the Stop check use them. `~/` is expanded. Paths must be absolute. Any other key in an entry is an error. |
| `stop_hook` | Set to `false` to turn off the Stop check. Default `true`. |

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
    gate: true            # optional: install the Linear gate and deny list (see Linear gate)
```

A binding's team key may not equal a native kanban prefix set under
`kanban.prefixes`. `config.yml` is rejected at load when it does, and the
error names the binding, the team key and the project. A prefix the kanban
derives for a project is only known to the kanban database, so it is checked
where that database is open. When a binding's team key equals its project's
native prefix, kanban reads leave that mirror out and add a `tracker_note`
that names the collision and says to set a different native prefix in
`kanban.prefixes`. `wardwell tracker doctor` prints a `kanban prefix` line per
binding, and the `wardwell doctor` row fails with the same sentence.

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

`doctor` prints four lines per binding. They check whether the credential
file exists with owner-only permissions, whether the provider accepts the token on one
cheap request, whether the team key resolves, and whether the team key
differs from the project's native kanban prefix. A failure names one code:
`credential`, `auth`, `provider`, `team_not_found` or `prefix_collision`. `auth` means the
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
and says to edit in the tracker. Reads are unaffected. A write action that
names a key held in a mirror, such as a `move` of COR-12, is refused on
read-only and writable bindings alike. The refusal names the provider and
says to edit the issue there. It writes nothing.

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
`get`, mirrored items carry no description. `list` and `query` return the 50
most recently updated mirrored items, newest first. `search` returns at most
20, sorted by key with the number compared as a number, so COR-2 comes before
COR-10. When more match, the result adds `tracker_truncated` with the count
left out.

`get` for a key the mirror does not hold runs one incremental pull for the
binding that owns the key, then looks again. A binding owns a key when its
team key is the key's prefix. A `project` or `domain` in the call narrows the
bindings first, and every kanban read honours them. A refresh never runs a
full pull. It waits at most 2 seconds for the project lock. The cooldown
comes from the log: no refresh runs within a minute of the newest
`pull_completed`, `full_resync` or `pull_failed` marker, so every server on
the vault shares it. The result carries `refreshed`,
true when a pull completed, and `refresh_reason`: `found_after_pull`,
`still_missing`, `cooldown`, `pull_failed:<code>`, `no_binding` or
`never_pulled`. A mirror with no pull marker is not refreshed, because an
incremental pull with no cursor would read the whole team. Run `wardwell
tracker pull` first. `list`, `query` and `search` never pull.

At session start, `wardwell inject` prints what it printed before: the
domain's `current_state.md` when it has one, else a summary of each project
that has its own. Every project without a tracker binding prints exactly
that. Under a bound project it adds one rot line and a tracker section. The
rot line reads like this: Last history entry 12 days ago. Last decision 3
days ago. The history age comes from the last entry at the end of
`history.jsonl`. When the domain's own state file is
printed, only the bound projects follow it, each under its own header.
Folders that are hidden or start with an underscore get no added lines. The
section's first line is "Tracker mirror. Last pulled 2 hours ago. Not
authoritative." Up to ten issues in a started state follow, each with key,
title and state. When the last pull failed or is more than 24 hours old, the
section shows only the age and that notice. A mirror never pulled says only
"Tracker mirror. Never pulled. Not authoritative." Inject also runs the
offline doctor check. When pulls cannot run, the section is one line, such as
"Tracker mirror. Pulls cannot run: credential. Last pulled 3 days ago." It
lists no issues.

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
`wardwell setup` installs the same agent at the hourly default when any binding
exists, keeps an interval you set, and `wardwell uninstall` removes it.
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
