# Wardwell Install & Init Spec

## Goal

One command. User's next Claude session has wardwell tools available automatically. They never see "MCP", "stdio", or "JSON-RPC". They just know Claude suddenly knows things.

```bash
brew install wardwell && wardwell init
```

## `wardwell init`

Interactive first-run setup. Idempotent — safe to re-run.

### Step 1 — Generate Config

Create `~/.wardwell/config.yml` with sensible defaults.

**Auto-detection:**
- Scan `~/.claude/projects/` for existing session directories → infer project names and paths
- Scan `~/Code/` (or configurable) for git repos → suggest domain groupings
- Detect Obsidian vault if present (look for `.obsidian/` directories)

**Prompt user for:**
- Confirm or adjust detected domains
- Add any aliases
- Nothing else. Defaults for everything else.

If user just hits enter through everything, they get a working config from auto-detection alone.

### Step 2 — Create Vault Directory

```
~/.wardwell/vault/
~/.wardwell/proposals/
~/.wardwell/summaries/
~/.wardwell/index.db  (created on first run of server)
~/.wardwell/sessions.db (created on first run of daemon)
```

### Step 3 — Inject MCP Server Config

**Claude Desktop:**

Read `~/Library/Application Support/Claude/claude_desktop_config.json`. If doesn't exist, create it. Merge in:

```json
{
  "mcpServers": {
    "wardwell": {
      "command": "/path/to/wardwell",
      "args": ["serve"]
    }
  }
}
```

Preserve all existing entries. Never overwrite other MCP servers.

**Claude Code (global):**

Read `~/.claude/settings.json`. Merge in the same entry under `mcpServers`. Preserve existing.

**Path resolution:** Use the actual installed binary path. If installed via cargo, `~/.cargo/bin/wardwell`. If via homebrew, `/opt/homebrew/bin/wardwell`. Detect at init time, write absolute path.

### Step 4 — Inject CLAUDE.md Pointer

Scan all directories matching configured domain paths for existing CLAUDE.md files. For each one found, plus `~/.claude/CLAUDE.md` (global):

1. Read existing content
2. Look for `<!-- wardwell:start -->` / `<!-- wardwell:end -->` markers
3. If markers exist: replace between them
4. If no markers: append the section
5. Never touch content outside markers

### Step 5 — Install Session Start Hook (Claude Code only)

Register a `SessionStart` hook in `~/.claude/settings.json`:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "type": "command",
        "command": "/path/to/wardwell inject \"$(pwd)\""
      }
    ]
  }
}
```

Preserves existing hooks. Uses absolute binary path detected at init time.

### Step 6 — Initial Index Build

If `~/.wardwell/vault/` has any .md files (from a previous install or manual seeding):
- Run full index build into `~/.wardwell/index.db`
- Report: "Indexed N files across M domains"

If vault is empty:
- Report: "Vault is empty. Context will build automatically as you use Claude."

### Step 7 — Verify

- Confirm config written
- Confirm MCP entries injected
- Confirm CLAUDE.md pointers placed
- Confirm hook installed
- Print: "Done. Restart Claude Desktop and/or start a new Claude Code session."

## `wardwell project link`

Records which directories belong to a vault project, so session start finds it.

- Writes one entry under `projects:` in `~/.wardwell/config.yml`.
- Previews first. The plan line is UPDATE + BACKUP or UNCHANGED. `--dry-run` writes nothing. It asks once. `--yes` skips the question.
- Idempotent. A directory already covered by the project changes nothing and says so.
- Preserves everything else. The crate has no comment-preserving YAML editor, so the entry is inserted as text. The result is parsed before writing. Keys outside `projects:` must be unchanged, and `projects:` must differ by the one path. Otherwise nothing is written.
- Backs up config.yml beside itself with mode 0600. Writes through a temp file and a rename.
- Refuses a project folder that does not exist in the vault, a directory linked to another project, and a `projects:` section written in flow style.
- A linked worktree records its main checkout.

## Stop check

Runs inside the Stop hook: `wardwell companion lifecycle stop` when the Companion hooks are installed, and `wardwell resolve` otherwise.

- Resolves the project with the same mapping as session start. Allows when there is no mapped project or no vault folder.
- Start time: the earliest lifecycle generation of the session id. Claude Code's Stop payload carries no start time.
- Counts entries in this worktree's HEAD reflog since the start whose subject begins with `commit`. Allows on any git error. The whole check has a 1.2 second budget.
- Blocks when there are commits and no history entry since the start. One line names the count, the start time, and the command to run.
- Blocks at most once per session id, using a marker under `~/.wardwell/stop-check/blocked/`. Honours `stop_hook_active`.
- Logs each block to `~/.wardwell/stop-check/blocks.jsonl`.
- `WARDWELL_STOP_CHECK=off` or `stop_hook: false` turns it off.
- Order: the Companion check runs first and keeps its output. The Stop check runs only when the Companion check succeeds. When both block, one block carries both reasons.

## `wardwell uninstall`

Clean removal. Reverse of init.

1. Remove wardwell entry from Desktop MCP config (preserve others)
2. Remove wardwell entry from Code MCP config (preserve others)
3. Remove `<!-- wardwell:start -->` to `<!-- wardwell:end -->` from all CLAUDE.md files
4. Remove hook script
5. **Do NOT delete `~/.wardwell/`** — that's the user's data. Print: "Your vault and config are preserved at ~/.wardwell/. Delete manually if desired."

## `wardwell doctor`

Diagnostic command. Checks everything is wired correctly.

- Config exists and parses ✓/✗
- Vault directory exists ✓/✗
- Index exists and has N entries ✓/✗
- Desktop MCP config has wardwell entry ✓/✗
- Code MCP config has wardwell entry ✓/✗
- CLAUDE.md pointers found in N locations ✓/✗
- SessionStart hook registered in settings.json ✓/✗
- Binary path in MCP configs matches actual binary location ✓/✗
- Session sources exist and have N sessions ✓/✗
- Each linked project: each directory exists, ages of the last history entry and decision, the last pull when bound, the last stop-check block ✓/✗

## Distribution

### Phase 1 — Cargo
```bash
cargo install wardwell
wardwell init
```

### Phase 2 — Homebrew
```bash
brew install wardwell
wardwell init
```

Homebrew tap initially, move to core if there's demand.

### Phase 3 — Binary releases
GitHub releases with prebuilt binaries for macOS (arm64, x86_64) and Linux. Curl-pipe-bash installer that downloads binary + runs init.

## Upgrade Path

`wardwell init` is idempotent. On upgrade:
- Re-run init to update MCP config paths if binary moved
- Migrate config if schema changed (versioned config with migration)
- Re-inject CLAUDE.md pointers (template may have changed)
- Never touch vault content or proposals
