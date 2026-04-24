# Wardwell Kanban — MCP Tool Design Spec

## Overview

A shared project/work board accessible via MCP tool (`wardwell_kanban`). Used by Jack (conversationally via Hank, visually via web UI) and agents (programmatically before writing briefs, after completing work). Stores business objects: leads, deals, experiments, features, projects, deliverables.

## Storage

**SQLite primary** (`~/.wardwell/kanban.db`) — source of truth for all reads and writes. Separate from `index.db` because kanban is primary state (not a rebuildable cache).

**Vault audit trail** — every mutation appends one line to `{domain}/{project}/tickets.md` in the vault. This file is append-only, visible in Obsidian, searchable via FTS, and never read by the tool. It is a log, not a data store.

### Dual-Write Behavior

On every kanban mutation, the tool does two things:

1. **SQLite write** — the actual data change (INSERT/UPDATE)
2. **Vault append** — one line to `{domain}/{project}/tickets.md`

### Vault Audit Format

```markdown
# {Project} Tickets

- 04/24 SO-3 created: Fix billing flow [backlog] ⚡high 📅05/01
- 04/24 SO-3 → todo
- 04/25 SO-3 → in_progress | "Talked to David — needs sandbox testing"
- 04/25 SO-3 note: "Webhook logs show 403 on callback — auth header missing"
- 04/25 SO-3 deadline: 05/01 → 05/08 | "Waiting on Stripe sandbox access"
- 04/28 SO-3 → done | "Shipped, webhook fixed, verified in prod"
```

Event types:
- `created: {title} [{status}] {priority} {deadline}`
- `→ {new_status}` (with optional `| "note"`)
- `note: "{text}"`
- `deadline: {old} → {new}` (with optional `| "reason"`)
- `priority: {old} → {new}`
- `updated: {field changes}`

### Vault Path Resolution

`project` slug → `kanban_projects.domain` → `{vault_root}/{domain}/{project}/tickets.md`.

Domain is inferred on first ticket creation by walking `DomainRegistry` for a subdirectory matching the project slug. Stored in `kanban_projects` for subsequent lookups. Falls back to requiring explicit `domain` param if inference fails.

## Feature Gate

Kanban is **disabled by default**. Enabled via config:

```yaml
kanban:
  enabled: true
```

When disabled, the tool is removed from the MCP tool router at startup via `router.remove_route("wardwell_kanban")`. Agents never see it in the tool list — no wasted tokens on failed calls.

Implementation: `WardwellServer::new()` calls `Self::tool_router()` then conditionally `router.remove_route("wardwell_kanban")` before storing the router.

## SQLite Schema

### `kanban_projects`

| Column | Type | Notes |
|-|-|-|
| project | TEXT PRIMARY KEY | slug: "shulops" |
| prefix | TEXT UNIQUE NOT NULL | "SO" — auto-derived or overridden |
| domain | TEXT NOT NULL | "personal" — inferred or explicit |
| next_id | INTEGER NOT NULL DEFAULT 1 | counter for ticket IDs |

### `kanban_items`

| Column | Type | Notes |
|-|-|-|
| ticket_id | TEXT PRIMARY KEY | "SO-3" |
| project | TEXT NOT NULL | references kanban_projects.project |
| title | TEXT NOT NULL | |
| description | TEXT | nullable |
| status | TEXT NOT NULL DEFAULT 'backlog' | backlog, todo, in_progress, review, done |
| priority | TEXT NOT NULL DEFAULT 'medium' | low, medium, high, urgent |
| assignee | TEXT | nullable |
| deadline | TEXT | ISO date, nullable |
| source | TEXT | who created: hank, manual, cmo, etc. |
| created_at | TEXT NOT NULL | ISO 8601 datetime |
| updated_at | TEXT NOT NULL | ISO 8601 datetime |
| completed_at | TEXT | set when status → done, cleared if moved back |

### `kanban_notes`

| Column | Type | Notes |
|-|-|-|
| id | INTEGER PRIMARY KEY AUTOINCREMENT | |
| ticket_id | TEXT NOT NULL | references kanban_items.ticket_id |
| text | TEXT NOT NULL | |
| author | TEXT | nullable |
| created_at | TEXT NOT NULL | ISO 8601 datetime |

## Module Structure

```
src/kanban/
  mod.rs          — pub mod declarations
  store.rs        — KanbanStore: Mutex<Connection>, schema init, CRUD
  prefix.rs       — prefix derivation, project registration, domain inference
  audit.rs        — append_ticket_log() vault writer
```

`KanbanStore` owns a `Mutex<Connection>` to `kanban.db` (same pattern as `IndexStore`). Constructed in `run_serve()`, passed to `WardwellServer` as `Option<KanbanStore>` (`None` when disabled).

### MCP Shim

The `#[tool]` method in `server.rs` is a thin dispatcher (~50 lines):

```rust
#[tool(description = "...")]
async fn wardwell_kanban(&self, params: Parameters<KanbanParams>) -> String {
    let Some(ref kanban) = self.kanban else {
        return json_error("kanban is disabled");
    };
    let p = params.0;
    match p.action.as_str() {
        "list"   => kanban.list(&p),
        "create" => kanban.create(&p, &self.vault_root, &self.registry),
        "update" => kanban.update(&p, &self.vault_root),
        "move"   => kanban.move_item(&p, &self.vault_root),
        "note"   => kanban.add_note(&p, &self.vault_root),
        "query"  => kanban.query(&p),
        _        => json_error("unknown kanban action"),
    }
}
```

## MCP Tool Interface

### Tool Name: `wardwell_kanban`

### Params (flat struct, action-routed)

| Param | Type | Used By | Notes |
|-|-|-|-|
| action | string | all | REQUIRED: list, create, update, move, note, query |
| ticket_id | string | update, move, note | |
| project | string | list, create, query | filter or assign |
| domain | string | create | optional, inferred from project directory |
| title | string | create, update | |
| description | string | create, update | |
| status | string | list, move, update | filter or set |
| priority | string | list, create, update | filter or set |
| assignee | string | list, create, update | filter or set |
| deadline | string | create, update | ISO date |
| source | string | create | who created it |
| text | string | note | note content |
| include_done | bool | list | default false |
| question | string | query | name of a configured query |

### Actions

#### `list`
Return items, optionally filtered by project, status, priority, assignee.

Returns: `{ "items": [...], "total": N, "returned": N }`

#### `create`
Create a new item. `title` and `project` required. Returns created item with assigned `ticket_id`.

Returns: `{ "created": true, "item": { ... } }`

#### `update`
Update fields on an existing item. Only provided fields change. `ticket_id` required.

Returns: `{ "updated": true, "item": { ... } }`

#### `move`
Status transition shortcut. `ticket_id` and `status` required. Auto-logs transition to notes.

Returns: `{ "moved": true, "item": { ... }, "transition": "old → new" }`

#### `note`
Append a timestamped note to an item. `ticket_id` and `text` required.

Returns: `{ "noted": true, "item": { ... } }`

#### `query`
Run a named query. Returns items matching criteria.

Returns: same shape as `list`.

## Dynamic Query System

Query questions are config-driven. Ship with defaults, user can add/modify without recompiling:

```yaml
kanban:
  enabled: true
  queries:
    overdue: "status != 'done' AND deadline < date('now')"
    stale: "status != 'done' AND updated_at < datetime('now', '-7 days')"
    no_deadline: "status != 'done' AND deadline IS NULL"
    blocked: "status = 'backlog'"
    recent: "updated_at > datetime('now', '-2 days')"
```

Defaults are baked into code and used when config omits the `queries` key. If the config defines `queries:`, the config map is **merged over** defaults — config entries override matching defaults, unmentioned defaults survive, and new config entries are added. This lets users tweak one query (e.g., change "stale" from 7 to 14 days) without losing the rest.

The `query` action looks up the question name, gets the WHERE clause, executes it against `kanban_items`. Unknown question names return an error listing available queries.

## Ticket ID Prefixes

### Auto-Derivation

1. Uppercase first two characters of project slug: "shulops" → "SO"
2. Check `kanban_projects` for collision
3. If collision: try first + third char, then first + last char
4. If all collide: error, require explicit prefix

### Config Override

```yaml
kanban:
  prefixes:
    shulops: "SO"
    shipping: "SH"
```

Config overrides always win. Stored in `kanban_projects` on first ticket creation.

## Config Schema Additions

```yaml
kanban:
  enabled: bool          # default false — tool not registered when false
  queries:               # optional — override default query definitions
    name: "SQL WHERE clause"
  prefixes:              # optional — override auto-derived ticket ID prefixes
    project_slug: "PREFIX"
```

Added to `RawConfig` as `kanban: Option<RawKanbanConfig>`. `WardwellConfig` exposes `kanban_enabled: bool`, `kanban_queries: HashMap<String, String>`, `kanban_prefixes: HashMap<String, String>`.

## Archiving

None. Done items stay in SQLite indefinitely. `include_done: false` (the default on `list`) filters them from normal results. The vault audit trail is the long-term record.

## Domain Enforcement

Kanban operates within the session's domain context:

- **Domain-scoped sessions**: `list` and `query` filter to items in `allowed_domains`. `create` validates that the project's domain is in `allowed_domains`.
- **Domainless sessions**: full access, no filtering.

This matches existing wardwell enforcement patterns.

## Agent Integration Patterns

### CMO reads board before writing brief
```
wardwell_kanban action:"list" project:"shulops"
```

### Strategist checks cross-project health
```
wardwell_kanban action:"list"
wardwell_kanban action:"query" question:"overdue"
```

### Temporal sequencer reads for scheduling
```
wardwell_kanban action:"list" status:"todo"
wardwell_kanban action:"list" status:"in_progress"
```
Sequencer is read-only. Does not mutate kanban items.

### Agent completes work
```
wardwell_kanban action:"move" ticket_id:"SO-3" status:"done"
wardwell_kanban action:"note" ticket_id:"SO-3" text:"Shipped, webhook fixed"
```

### Hank creates from conversation
```
wardwell_kanban action:"create" title:"Fix billing flow" project:"shulops"
wardwell_kanban action:"move" ticket_id:"SO-3" status:"done"
wardwell_kanban action:"list" project:"shulops"
```

## What This Replaces

- **task-queue.md** — kanban items replace flat task queues
- **current_state.md Focus/Next Action** — kanban items are more granular and stateful

What it does NOT replace:
- **reminders.md** — time-triggered alerts (different purpose)
- **current_state.md** — still useful for high-level project narrative
- **MEMORY.md** — behavioral corrections (different purpose)

## Open Questions (Deferred)

1. **Web UI** — board visualization, not part of this MCP tool spec
2. **Assignee-based routing** — agents auto-claiming items (future capability)
3. **Item linking** — blocking/blocked-by relationships between tickets (future action)
4. **Bulk operations** — batch status changes (future action if needed)
