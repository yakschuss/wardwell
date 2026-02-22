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
wardwell serve       Start the MCP server (Claude Code calls this automatically)
wardwell init        First-run setup — interactive walkthrough
wardwell doctor      Check that everything is wired correctly
wardwell uninstall   Clean removal — MCP entries, hooks, markers (preserves vault)
wardwell inject .    Output project context for a directory (used by hooks)
wardwell reindex     Rebuild the vault search index from scratch
wardwell seed <path> Create domain or project folders
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
- MCP configured in Claude Code and Desktop
- SessionStart hook registered
- Claude CLI available (for summarizer)

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

# Test (133 tests)
cargo test

# Build release
cargo build --release
```

Strict lints: `deny(clippy::unwrap_used, expect_used, panic, todo, unimplemented)`. Zero `unsafe` blocks.

## Requirements

- Rust 1.75+ (edition 2024)
- macOS or Linux
- Claude Code (for MCP integration)
- `claude` CLI (optional — only needed for session summarization)

## License

MIT
