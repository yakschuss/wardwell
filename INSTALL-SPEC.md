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
