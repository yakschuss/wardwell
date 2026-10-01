# Wardwell Install & Init Spec

## Goal

One command. User's next Claude session has wardwell tools available automatically. They never see "MCP", "stdio", or "JSON-RPC". They just know Claude suddenly knows things.

```bash
brew install wardwell
wardwell init      # first run
wardwell setup     # preview, then install or repair; safe to rerun
wardwell doctor    # verify; says what it cannot verify
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

### Step 5 — Install Hooks (Claude Code only)

`init` calls the same installer as `wardwell setup`. Its preview lists the
installer's plan lines, and the step applies that plan. See `wardwell setup`
below for what is installed and how.

### Step 6 — Initial Index Build

If `~/.wardwell/vault/` has any .md files (from a previous install or manual seeding):
- Run full index build into `~/.wardwell/index.db`
- Report: "Indexed N files across M domains"

If vault is empty:
- Report: "Vault is empty. Context will build automatically as you use Claude."

### Step 7 — Verify

- Confirm config written
- Confirm connection entries added
- Confirm CLAUDE.md pointers placed
- Confirm hook installed
- Print: "Done. Restart Claude Desktop and/or start a new Claude Code session."

## `wardwell setup`

Sets up or repairs this computer. Never reads or changes the vault.

- One command. Preview first: each plan line is CREATE, UPDATE + BACKUP, UPDATE (a file only Wardwell writes), UNCHANGED, OFF or MANUAL. `--dry-run` writes nothing. It asks once. `--yes` skips the question.
- Idempotent. A second run prints the plan with UNCHANGED lines and "Nothing to change."
- Preflight: every client file, `~/.claude/settings.json`, `config.yml` and the install record are read and checked before any write. A malformed or conflicting file stops the run. Nothing is written.
- Each file is checked again just before it is written. A file changed since the preview stops the run.
- Backs up each changed file beside it, mode 0600. Writes through a temp file and a rename.
- Ownership is exact. A hook handler is Wardwell's only when its program's file name is `wardwell` or `wardwell-<digits>.<digits>...`, its arguments are exactly Wardwell's, and its group has Wardwell's matcher. A group with another matcher is the user's and is never moved or edited. A substring never matches.
- Rewrites keep what the user wrote: key order, number text, and the file's permission mode.

### Tier one: memory, always planned

In `~/.claude/settings.json`:

```json
{"hooks": {
  "SessionStart": [{"hooks": [{"type": "command", "command": "'/path/to/wardwell' inject \"$(pwd)\""}]}],
  "Stop": [{"hooks": [{"type": "command", "command": "'/path/to/wardwell' resolve"}]}]
}}
```

When the Companion Stop hook (`companion lifecycle stop --client claude`) is present, it runs the history check, so `resolve` is not added and an existing one is removed.

### Tier two: "Tracker policy, optional"

Planned only when a tracker binding has `provider: linear` and `gate: true`. Each line is labelled "Tracker policy, optional".

```json
{"hooks": {"PreToolUse": [{
  "matcher": "mcp__linear__save_comment|mcp__linear__save_issue",
  "hooks": [{"type": "command", "command": "'/path/to/wardwell' gate linear", "timeout": 5}]
}]},
"permissions": {"deny": [
  "mcp__linear__delete_comment", "mcp__linear__delete_attachment",
  "mcp__linear__retire_issue_label", "mcp__linear__retire_project_label",
  "mcp__linear__save_project", "mcp__linear__delete_status_update",
  "mcp__linear__delete_diff_comment"
]}}
```

- A hook entry that runs `linear-gate.py` (directly or through a Python interpreter) is removed, with its own plan line. The script file is not deleted. With the policy off, the entry is kept and the plan says how to replace it.
- With the policy off, a gate and recorded deny entries left by an earlier run are removed.
- The rules are the ruleset `linear-updates`, version 1, held as data in the binary. A per-project override file is a follow-up.

### Install record

`~/.wardwell/install-manifest.json` (under `WARDWELL_CONFIG_DIR` when set) lists the deny entries Wardwell added. An entry the user already had is not recorded. Uninstall removes only recorded entries.

```json
{"version": 1, "claude_permissions_deny": ["mcp__linear__delete_comment"], "created_keys": ["hooks"]}
```

### Tracker pull

When any binding exists, setup plans the hourly pull through `tracker schedule`'s code: a launchd agent, `~/Library/LaunchAgents/com.wardwell.tracker-pull.plist`. An interval already set is kept. Off macOS, the plan line is MANUAL with a crontab line.

### Activation

Installed is not active. Claude Code reads hooks and permissions when a session starts. Sessions already running do not change. Setup says this after it writes.

## `wardwell project link`

Records which directories belong to a vault project, so session start finds it.

- Writes one entry under `projects:` in `~/.wardwell/config.yml`.
- Previews first. The plan line is UPDATE + BACKUP or UNCHANGED. `--dry-run` writes nothing. It asks once. `--yes` skips the question.
- Idempotent. A directory already covered by the project changes nothing and says so.
- Preserves everything else. The crate has no comment-preserving YAML editor, so the entry is inserted as text. The result is parsed before writing. Keys outside `projects:` must be unchanged, and `projects:` must differ by the one path. Otherwise nothing is written.
- Backs up config.yml beside itself with mode 0600. Writes through a temp file and a rename.
- Holds `config.yml.lock` from the read to the rename, created exclusively. A dry run takes no lock. It waits up to 10 seconds for another link, then stops with a message. The lock is released while the question waits for an answer. The lock is removed on every exit.
- Refuses a project folder that does not exist in the vault, a directory linked to another project, and a `projects:` section written in flow style.
- Refuses a linked worktree and names its main checkout. Doctor fails a mapped linked worktree the same way.

## Stop check

Runs inside the Stop hook: `wardwell companion lifecycle stop`, and `wardwell resolve` too. The check needs the Companion lifecycle hooks for a session start time. `wardwell resolve` alone has none, so without those hooks it always allows.

- Resolves the project with the same mapping as session start. Allows when there is no mapped project or no vault folder.
- Start time: `opened_at` in the session's own lifecycle file, written once when the lifecycle hooks first record the session. Claude Code's Stop payload carries no start time. A file without it allows.
- Counts entries in this worktree's HEAD reflog since the start whose subject begins with `commit`. Allows on any git error. The whole check has a 1.2 second budget.
- Blocks when there are commits and no history entry since the start. One line names the count, the start time, and the command to run.
- Blocks at most once per session id, using a marker under `~/.wardwell/stop-check/blocked/`. Honours `stop_hook_active`.
- Logs each block to `~/.wardwell/stop-check/blocks.jsonl`.
- `WARDWELL_STOP_CHECK=off` or `stop_hook: false` turns it off.
- Order: the Companion check runs first and keeps its output. The Stop check runs only when the Companion check succeeds. When both block, one block carries both reasons.

## `wardwell uninstall`

Removes only Wardwell's entries.

1. Remove Wardwell's entries from the Desktop, Code and Codex MCP configs (preserve others)
2. Remove `<!-- wardwell:start -->` to `<!-- wardwell:end -->` from all CLAUDE.md files
3. Remove Wardwell's hook handlers from `~/.claude/settings.json` by exact match: session start, Stop, and the Linear gate. Back up the file first. The Companion install is not touched: its hooks in both clients, its skills, its command file and its instruction blocks stay. Uninstall prints one line saying so.
4. Remove the deny entries the install record lists, and no others. Empty the record; do not delete it.
5. Remove the tracker pull service.
6. **Do NOT delete `~/.wardwell/`** — that's the user's data. Print: "Your vault and config are preserved at ~/.wardwell/. Delete manually if desired."

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
- Gate ruleset name and version ✓
- Linear gate installed when a linear binding has `gate: true`, and its binary path is this binary ✓/✗
- Linear deny list complete ✓/✗
- Tracker pull service plist present and its program exists ✓/✗. Whether launchd loaded it is not checked.

All checks are offline.

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
GitHub releases with prebuilt binaries for macOS (arm64) and Linux (x86_64). The old curl-pipe-bash `install.sh` was deleted in 0.12.0; it cloned a placeholder repository. Release steps are in `docs/RELEASE.md`.

## Upgrade Path

`wardwell init` and `wardwell setup` are idempotent. On upgrade, run `wardwell setup`:
- Re-run init to update MCP config paths if binary moved
- Migrate config if schema changed (versioned config with migration)
- Re-inject CLAUDE.md pointers (template may have changed)
- Never touch vault content or proposals
