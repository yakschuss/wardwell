# Wardwell

Personal AI context layer for Claude Code. MCP server + CLI that gives your AI persistent knowledge about your projects — what they are, where you left off, and what to do next.

Your AI already knows your projects. No uploads, no re-explaining, no starting from zero.

## Install

```bash
# Build from source
cargo install --path .

# First-run setup (generates config, wires MCP server + hook)
wardwell init
```

`wardwell init` does everything: creates `~/.wardwell/`, generates a minimal config, registers the MCP server in Claude Code, installs the SessionStart hook, and scaffolds your first `Agents/` directory.

## How It Works

**Vault path** points at your notes directory (Obsidian vault, plain markdown folder, whatever you use). Wardwell indexes it for full-text search.

**Agents/** is where project state lives. Each domain (a work area) contains projects. Each project has structured files that the AI reads and writes:

```
Agents/
  work/
    my-project/
      INDEX.md            # what and why
      current_state.md    # live state (synced by AI)
      decisions.md        # created by wardwell_write
      history.jsonl       # created by wardwell_write
      lessons.jsonl       # created by wardwell_write
  personal/
    my-app/
      INDEX.md
      current_state.md
```

**Three MCP tools** give Claude Code access to your vault:

- `wardwell_search` — full-text search, file reads, project history queries, prioritized work queue
- `wardwell_write` — sync project state, record decisions, append history, store lessons
- `wardwell_clipboard` — copy content to system clipboard (asks permission first)

**SessionStart hook** injects project context when you open a Claude Code session. If your current directory matches an `Agents/` domain, the AI gets a summary of active projects and their state — before you type anything.

## CLI Commands

```
wardwell serve       Start the MCP server (Claude Code calls this automatically)
wardwell init        First-run setup — config, MCP, hook, Agents/ directory
wardwell doctor      Check that everything is wired correctly
wardwell uninstall   Clean removal — MCP entries, hooks, markers (preserves vault)
wardwell inject .    Output project context for a directory (used by hooks)
wardwell reindex     Rebuild the vault search index from scratch
wardwell seed <path> Create domain or project folders under Agents/
```

### `wardwell seed`

Scaffold a new domain or project:

```bash
# Create a domain directory
wardwell seed work

# Create a project with INDEX.md + current_state.md templates
wardwell seed work/my-project
```

## Config

Config lives at `~/.wardwell/config.yml`. Minimal example:

```yaml
vault_path: ~/Notes
session_sources:
  - ~/.claude/projects/
exclude:
  - "*.png"
  - "*.jpg"
  - node_modules
  - .git
```

| Key | What it does |
|-|-|
| `vault_path` | Root directory to index for search (your notes/vault) |
| `session_sources` | Directories containing Claude Code session data |
| `exclude` | Glob patterns to skip during indexing |
| `agents_dir` | Override Agents/ location (defaults to `{vault_path}/Agents`) |

## Background Services

When running as an MCP server (`wardwell serve`), Wardwell runs background tasks:

- **File watcher** — detects vault changes and updates the search index in real time
- **Session indexer** — processes Claude Code session history for cross-session knowledge
- **Summarizer** — generates session summaries using Claude (configurable model)

## Development

```bash
# Lint (must pass clean)
cargo clippy --lib --bin wardwell

# Test
cargo test

# Build release
cargo build --release
```

Strict lints enforced: `deny(clippy::unwrap_used, expect_used, panic, todo, unimplemented)`. Zero `unsafe` blocks. All warnings are errors.

## License

MIT
