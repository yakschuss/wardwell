#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use wardwell::kanban::store::{KanbanStore, default_kanban_queries, merge_kanban_queries};

fn make_store() -> (tempfile::TempDir, KanbanStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = KanbanStore::open(&dir.path().join("kanban.db")).unwrap();
    (dir, store)
}

// -- Stress: bulk creation --

#[test]
fn create_100_items_in_one_project() {
    let (_dir, store) = make_store();
    let pf = HashMap::new();
    for i in 1..=100 {
        let title = format!("Item {i}");
        let item = store
            .create_item(&title, "bigproject", "work", None, None, None, None, None, None, &pf)
            .unwrap();
        assert_eq!(item.ticket_id, format!("BI-{i}"));
    }
    let items = store.list(None, None, None, None, true, None).unwrap();
    assert_eq!(items.len(), 100);
}

#[test]
fn create_items_across_20_projects() {
    let (_dir, store) = make_store();
    let pf = HashMap::new();
    let projects = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf",
        "hotel", "india", "juliet", "kilo", "lima", "mike", "november",
        "oscar", "papa", "quebec", "romeo", "sierra", "tango",
    ];
    for p in &projects {
        store
            .create_item(&format!("Task for {p}"), p, "work", None, None, None, None, None, None, &pf)
            .unwrap();
    }
    let items = store.list(None, None, None, None, false, None).unwrap();
    assert_eq!(items.len(), 20);

    // Each project should have a unique prefix
    let conn = store.conn().unwrap();
    let mut stmt = conn.prepare("SELECT prefix FROM kanban_projects").unwrap();
    let prefixes: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    assert_eq!(prefixes.len(), 20);
    // Check uniqueness
    let unique: std::collections::HashSet<_> = prefixes.iter().collect();
    assert_eq!(unique.len(), 20, "prefix collision detected: {:?}", prefixes);
}

// -- Edge cases: empty and boundary inputs --

#[test]
fn create_with_empty_title() {
    let (_dir, store) = make_store();
    let pf = HashMap::new();
    // Empty string is technically valid — just an empty title
    let item = store
        .create_item("", "proj", "work", None, None, None, None, None, None, &pf)
        .unwrap();
    assert_eq!(item.title, "");
}

#[test]
fn create_with_unicode_title() {
    let (_dir, store) = make_store();
    let pf = HashMap::new();
    let item = store
        .create_item("修复计费流程 🔥", "proj", "work", None, None, None, None, None, None, &pf)
        .unwrap();
    assert_eq!(item.title, "修复计费流程 🔥");
}

#[test]
fn create_with_very_long_title() {
    let (_dir, store) = make_store();
    let pf = HashMap::new();
    let long_title = "A".repeat(10_000);
    let item = store
        .create_item(&long_title, "proj", "work", None, None, None, None, None, None, &pf)
        .unwrap();
    assert_eq!(item.title.len(), 10_000);
}

#[test]
fn create_with_sql_injection_in_title() {
    let (_dir, store) = make_store();
    let pf = HashMap::new();
    let item = store
        .create_item(
            "'; DROP TABLE kanban_items; --",
            "proj",
            "work",
            None, None, None, None, None, None, &pf,
        )
        .unwrap();
    assert_eq!(item.title, "'; DROP TABLE kanban_items; --");

    // Table should still exist
    let items = store.list(None, None, None, None, false, None).unwrap();
    assert_eq!(items.len(), 1);
}

#[test]
fn create_with_invalid_status() {
    let (_dir, store) = make_store();
    let pf = HashMap::new();
    let result = store.create_item(
        "Task", "proj", "work", None, Some("invalid_status"), None, None, None, None, &pf,
    );
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("invalid status"));
}

#[test]
fn create_with_invalid_priority() {
    let (_dir, store) = make_store();
    let pf = HashMap::new();
    let result = store.create_item(
        "Task", "proj", "work", None, None, Some("critical"), None, None, None, &pf,
    );
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("invalid priority"));
}

// -- Status transitions --

#[test]
fn move_through_all_statuses() {
    let (_dir, store) = make_store();
    let pf = HashMap::new();
    store
        .create_item("Task", "proj", "work", None, Some("backlog"), None, None, None, None, &pf)
        .unwrap();

    let statuses = ["todo", "in_progress", "review", "done", "in_progress", "review", "done"];
    for status in &statuses {
        let (item, transition) = store.move_item("PR-1", status).unwrap();
        assert_eq!(item.status, *status);
        assert!(transition.contains('→'));
    }

    // Should have transition notes for each move
    let item = store.list(None, None, None, None, true, None).unwrap();
    assert_eq!(item[0].notes.len(), statuses.len());
}

#[test]
fn move_to_same_status() {
    let (_dir, store) = make_store();
    let pf = HashMap::new();
    store
        .create_item("Task", "proj", "work", None, Some("todo"), None, None, None, None, &pf)
        .unwrap();

    let (item, transition) = store.move_item("PR-1", "todo").unwrap();
    assert_eq!(item.status, "todo");
    assert_eq!(transition, "todo → todo");
}

#[test]
fn move_to_invalid_status() {
    let (_dir, store) = make_store();
    let pf = HashMap::new();
    store
        .create_item("Task", "proj", "work", None, None, None, None, None, None, &pf)
        .unwrap();

    let result = store.move_item("PR-1", "archived");
    assert!(result.is_err());
}

#[test]
fn move_nonexistent_ticket() {
    let (_dir, store) = make_store();
    let result = store.move_item("XX-999", "todo");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("not found"));
}

// -- Update edge cases --

#[test]
fn update_no_fields() {
    let (_dir, store) = make_store();
    let pf = HashMap::new();
    store
        .create_item("Task", "proj", "work", None, None, None, None, None, None, &pf)
        .unwrap();

    // Update with no fields changed — should just bump updated_at
    let item = store.update_item("PR-1", None, None, None, None, None, None).unwrap();
    assert_eq!(item.title, "Task");
}

#[test]
fn update_status_to_done_and_back() {
    let (_dir, store) = make_store();
    let pf = HashMap::new();
    store
        .create_item("Task", "proj", "work", None, None, None, None, None, None, &pf)
        .unwrap();

    let item = store
        .update_item("PR-1", None, None, Some("done"), None, None, None)
        .unwrap();
    assert!(item.completed_at.is_some());

    let item = store
        .update_item("PR-1", None, None, Some("todo"), None, None, None)
        .unwrap();
    assert!(item.completed_at.is_none());
}

#[test]
fn update_nonexistent_ticket() {
    let (_dir, store) = make_store();
    let result = store.update_item("XX-999", Some("New title"), None, None, None, None, None);
    assert!(result.is_err());
}

// -- Notes --

#[test]
fn add_many_notes() {
    let (_dir, store) = make_store();
    let pf = HashMap::new();
    store
        .create_item("Task", "proj", "work", None, None, None, None, None, None, &pf)
        .unwrap();

    for i in 1..=50 {
        store.add_note("PR-1", &format!("Note {i}"), Some("jack")).unwrap();
    }

    let items = store.list(None, None, None, None, false, None).unwrap();
    assert_eq!(items[0].notes.len(), 50);
}

#[test]
fn add_note_with_unicode() {
    let (_dir, store) = make_store();
    let pf = HashMap::new();
    store
        .create_item("Task", "proj", "work", None, None, None, None, None, None, &pf)
        .unwrap();

    store.add_note("PR-1", "Talked to David — needs 🧪 testing", None).unwrap();
    let items = store.list(None, None, None, None, false, None).unwrap();
    assert_eq!(items[0].notes[0].text, "Talked to David — needs 🧪 testing");
}

#[test]
fn add_note_to_nonexistent_ticket() {
    let (_dir, store) = make_store();
    let result = store.add_note("XX-999", "Note", None);
    assert!(result.is_err());
}

// -- Query edge cases --

#[test]
fn query_on_empty_db() {
    let (_dir, store) = make_store();
    let queries = default_kanban_queries();
    let items = store.query("overdue", &queries, None, None).unwrap();
    assert!(items.is_empty());
}

#[test]
fn query_with_custom_query() {
    let (_dir, store) = make_store();
    let pf = HashMap::new();
    store
        .create_item("Urgent", "proj", "work", None, Some("todo"), Some("urgent"), None, None, None, &pf)
        .unwrap();
    store
        .create_item("Low", "proj", "work", None, Some("todo"), Some("low"), None, None, None, &pf)
        .unwrap();

    let mut queries = HashMap::new();
    queries.insert("urgent_only".to_string(), "priority = 'urgent'".to_string());

    let items = store.query("urgent_only", &queries, None, None).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].title, "Urgent");
}

#[test]
fn query_merge_preserves_defaults() {
    let mut config = HashMap::new();
    config.insert("custom".to_string(), "priority = 'urgent'".to_string());

    let merged = merge_kanban_queries(&config);
    assert!(merged.contains_key("overdue"));
    assert!(merged.contains_key("stale"));
    assert!(merged.contains_key("custom"));
}

#[test]
fn query_merge_overrides_default() {
    let mut config = HashMap::new();
    config.insert("stale".to_string(), "status != 'done' AND updated_at < datetime('now', '-14 days')".to_string());

    let merged = merge_kanban_queries(&config);
    assert!(merged["stale"].contains("-14 days"));
}

#[test]
fn validate_queries_catches_bad_sql() {
    let (_dir, store) = make_store();
    let mut queries = HashMap::new();
    queries.insert("bad".to_string(), "THIS IS NOT SQL".to_string());

    let result = store.validate_queries(&queries);
    assert!(result.is_err(), "should reject invalid SQL WHERE clause");
}

#[test]
fn validate_queries_accepts_good_sql() {
    let (_dir, store) = make_store();
    let queries = default_kanban_queries();
    store.validate_queries(&queries).unwrap();
}

// -- List filtering combinations --

#[test]
fn list_with_all_filters() {
    let (_dir, store) = make_store();
    let pf = HashMap::new();

    store.create_item("Match", "proj", "work", None, Some("todo"), Some("high"), Some("jack"), None, None, &pf).unwrap();
    store.create_item("Wrong status", "proj", "work", None, Some("backlog"), Some("high"), Some("jack"), None, None, &pf).unwrap();
    store.create_item("Wrong priority", "proj", "work", None, Some("todo"), Some("low"), Some("jack"), None, None, &pf).unwrap();
    store.create_item("Wrong assignee", "proj", "work", None, Some("todo"), Some("high"), Some("alice"), None, None, &pf).unwrap();
    store.create_item("Wrong project", "other", "work", None, Some("todo"), Some("high"), Some("jack"), None, None, &pf).unwrap();

    let items = store
        .list(Some("proj"), Some("todo"), Some("high"), Some("jack"), false, None)
        .unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].title, "Match");
}

#[test]
fn list_ordering_priority_then_recency() {
    let (_dir, store) = make_store();
    let pf = HashMap::new();

    store.create_item("Low", "proj", "work", None, None, Some("low"), None, None, None, &pf).unwrap();
    store.create_item("Urgent", "proj", "work", None, None, Some("urgent"), None, None, None, &pf).unwrap();
    store.create_item("High", "proj", "work", None, None, Some("high"), None, None, None, &pf).unwrap();

    let items = store.list(None, None, None, None, false, None).unwrap();
    assert_eq!(items[0].priority, "urgent");
    assert_eq!(items[1].priority, "high");
    assert_eq!(items[2].priority, "low");
}

// -- Prefix edge cases --

#[test]
fn prefix_derivation_with_numbers() {
    let (_dir, store) = make_store();
    let pf = HashMap::new();
    store
        .create_item("Task", "v2app", "work", None, None, None, None, None, None, &pf)
        .unwrap();

    let conn = store.conn().unwrap();
    let prefix: String = conn
        .query_row("SELECT prefix FROM kanban_projects WHERE project = 'v2app'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(prefix, "V2");
}

#[test]
fn prefix_with_config_override() {
    let (_dir, store) = make_store();
    let mut pf = HashMap::new();
    pf.insert("myproj".to_string(), "ZZ".to_string());

    store
        .create_item("Task", "myproj", "work", None, None, None, None, None, None, &pf)
        .unwrap();

    let conn = store.conn().unwrap();
    let prefix: String = conn
        .query_row("SELECT prefix FROM kanban_projects WHERE project = 'myproj'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(prefix, "ZZ");
}

// -- Concurrent-ish access (same connection, sequential) --

#[test]
fn rapid_create_move_note_cycle() {
    let (_dir, store) = make_store();
    let pf = HashMap::new();

    for i in 1..=20 {
        let title = format!("Ticket {i}");
        let item = store
            .create_item(&title, "proj", "work", None, None, None, None, None, None, &pf)
            .unwrap();
        let tid = &item.ticket_id;

        store.move_item(tid, "in_progress").unwrap();
        store.add_note(tid, "Working on it", Some("bot")).unwrap();
        store.move_item(tid, "done").unwrap();
        store.add_note(tid, "Completed", Some("bot")).unwrap();
    }

    let done = store.list(None, Some("done"), None, None, true, None).unwrap();
    assert_eq!(done.len(), 20);

    for item in &done {
        assert!(item.completed_at.is_some());
        // 2 move notes + 2 manual notes = 4
        assert_eq!(item.notes.len(), 4, "ticket {} has {} notes", item.ticket_id, item.notes.len());
    }
}

// -- Domain enforcement --

#[test]
fn list_with_domain_filter() {
    let (_dir, store) = make_store();
    let pf = HashMap::new();

    store.create_item("Work task", "proj", "work", None, None, None, None, None, None, &pf).unwrap();
    store.create_item("Personal task", "hobby", "personal", None, None, None, None, None, None, &pf).unwrap();

    let work_only = store
        .list(None, None, None, None, false, Some(&["work".to_string()]))
        .unwrap();
    assert_eq!(work_only.len(), 1);
    assert_eq!(work_only[0].title, "Work task");

    let all = store.list(None, None, None, None, false, None).unwrap();
    assert_eq!(all.len(), 2);
}

// -- Audit trail --

#[test]
fn audit_trail_full_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    std::fs::create_dir_all(vault.join("work/proj")).unwrap();

    wardwell::kanban::audit::append_ticket_log(&vault, "work", "proj", "PR-1 created: Test [backlog]")
        .unwrap();
    wardwell::kanban::audit::append_ticket_log(&vault, "work", "proj", "PR-1 → todo").unwrap();
    wardwell::kanban::audit::append_ticket_log(&vault, "work", "proj", "PR-1 → in_progress").unwrap();
    wardwell::kanban::audit::append_ticket_log(&vault, "work", "proj", "PR-1 note: \"Working\"")
        .unwrap();
    wardwell::kanban::audit::append_ticket_log(&vault, "work", "proj", "PR-1 → done").unwrap();

    let content = std::fs::read_to_string(vault.join("work/proj/tickets.md")).unwrap();
    let lines: Vec<&str> = content.lines().filter(|l| l.starts_with("- ")).collect();
    assert_eq!(lines.len(), 5);
    assert!(content.starts_with("# proj Tickets"));
}

#[test]
fn audit_trail_special_characters() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");

    wardwell::kanban::audit::append_ticket_log(
        &vault,
        "work",
        "proj",
        "PR-1 note: \"Contains 'quotes' and \\backslashes and 日本語\"",
    )
    .unwrap();

    let content = std::fs::read_to_string(vault.join("work/proj/tickets.md")).unwrap();
    assert!(content.contains("'quotes'"));
    assert!(content.contains("\\backslashes"));
    assert!(content.contains("日本語"));
}
