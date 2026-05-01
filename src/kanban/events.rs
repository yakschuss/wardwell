use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum KanbanEvent {
    #[serde(rename = "create")]
    Create {
        ticket_id: String,
        title: String,
        project: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        group: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        epic: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tags: Vec<String>,
        #[serde(default = "default_backlog")]
        status: String,
        #[serde(default = "default_medium")]
        priority: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        deadline: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        assignee: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        timestamp: String,
    },
    #[serde(rename = "move")]
    Move {
        ticket_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        to: String,
        timestamp: String,
    },
    #[serde(rename = "update")]
    Update {
        ticket_id: String,
        fields: HashMap<String, serde_json::Value>,
        timestamp: String,
    },
    #[serde(rename = "note")]
    Note {
        ticket_id: String,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        author: Option<String>,
        timestamp: String,
    },
    #[serde(rename = "archive")]
    Archive {
        ticket_id: String,
        timestamp: String,
    },
    #[serde(rename = "attach")]
    Attach {
        ticket_id: String,
        attachment_id: String,
        filename: String,
        mime_type: String,
        size: u64,
        storage_path: String,
        timestamp: String,
    },
    #[serde(rename = "detach")]
    Detach {
        ticket_id: String,
        attachment_id: String,
        timestamp: String,
    },
}

fn default_backlog() -> String { "backlog".into() }
fn default_medium() -> String { "medium".into() }

impl KanbanEvent {
    pub fn ticket_id(&self) -> &str {
        match self {
            Self::Create { ticket_id, .. }
            | Self::Move { ticket_id, .. }
            | Self::Update { ticket_id, .. }
            | Self::Note { ticket_id, .. }
            | Self::Archive { ticket_id, .. }
            | Self::Attach { ticket_id, .. }
            | Self::Detach { ticket_id, .. } => ticket_id,
        }
    }

    pub fn timestamp(&self) -> &str {
        match self {
            Self::Create { timestamp, .. }
            | Self::Move { timestamp, .. }
            | Self::Update { timestamp, .. }
            | Self::Note { timestamp, .. }
            | Self::Archive { timestamp, .. }
            | Self::Attach { timestamp, .. }
            | Self::Detach { timestamp, .. } => timestamp,
        }
    }
}

pub fn jsonl_path(vault_root: &Path, domain: &str, project: &str) -> PathBuf {
    vault_root.join(domain).join(project).join("kanban.jsonl")
}

pub fn append_event(vault_root: &Path, domain: &str, project: &str, event: &KanbanEvent) -> Result<(), std::io::Error> {
    let dir = vault_root.join(domain).join(project);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("kanban.jsonl");

    let needs_schema = !path.exists() || path.metadata()?.len() == 0;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    if needs_schema {
        writeln!(file, r#"{{"_schema":"kanban","_version":"1.0"}}"#)?;
    }
    let line = serde_json::to_string(event).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    writeln!(file, "{line}")?;
    Ok(())
}

pub fn read_events(vault_root: &Path, domain: &str, project: &str) -> Vec<KanbanEvent> {
    let path = jsonl_path(vault_root, domain, project);
    read_events_from_path(&path)
}

pub fn read_events_from_path(path: &Path) -> Vec<KanbanEvent> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    content
        .lines()
        .filter(|l| !l.is_empty())
        .filter(|l| !l.contains("\"_schema\""))
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Read the last line of a JSONL file looking for a meta entry with next_id.
/// Falls back to scanning create events if no meta line found.
pub fn next_ticket_number(vault_root: &Path, domain: &str, project: &str, prefix: &str) -> i64 {
    let path = jsonl_path(vault_root, domain, project);
    if let Ok(content) = std::fs::read_to_string(&path) {
        // Check last non-empty line for meta
        for line in content.lines().rev() {
            let trimmed = line.trim();
            if trimmed.is_empty() { continue; }
            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(trimmed) {
                if meta.get("_meta").is_some() {
                    if let Some(n) = meta.get("next_id").and_then(|v| v.as_i64()) {
                        return n;
                    }
                }
            }
            break; // only check last non-empty line
        }
        // Fallback: scan create events
        let prefix_dash = format!("{prefix}-");
        let mut max = 0i64;
        for line in content.lines() {
            if let Ok(event) = serde_json::from_str::<KanbanEvent>(line) {
                if let KanbanEvent::Create { ticket_id, .. } = &event {
                    if let Some(num_str) = ticket_id.strip_prefix(&prefix_dash) {
                        if let Ok(n) = num_str.parse::<i64>() {
                            if n > max { max = n; }
                        }
                    }
                }
            }
        }
        max + 1
    } else {
        1
    }
}

/// Append a meta line with the current next_id for fast lookup.
pub fn append_meta(vault_root: &Path, domain: &str, project: &str, prefix: &str, next_id: i64) -> Result<(), std::io::Error> {
    let dir = vault_root.join(domain).join(project);
    let path = dir.join("kanban.jsonl");
    let mut file = std::fs::OpenOptions::new().append(true).open(&path)?;
    writeln!(file, r#"{{"_meta":true,"prefix":"{prefix}","next_id":{next_id}}}"#)?;
    Ok(())
}

pub fn scan_all_jsonl(vault_root: &Path) -> Vec<(String, String, Vec<KanbanEvent>)> {
    let mut results = vec![];
    let entries = match std::fs::read_dir(vault_root) {
        Ok(e) => e,
        Err(_) => return results,
    };
    for domain_entry in entries.flatten() {
        if !domain_entry.path().is_dir() { continue; }
        let domain_name = domain_entry.file_name().to_string_lossy().to_string();
        if domain_name.starts_with('.') { continue; }
        let projects = match std::fs::read_dir(domain_entry.path()) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for proj_entry in projects.flatten() {
            if !proj_entry.path().is_dir() { continue; }
            let proj_name = proj_entry.file_name().to_string_lossy().to_string();
            let jsonl = proj_entry.path().join("kanban.jsonl");
            if jsonl.exists() {
                let events = read_events_from_path(&jsonl);
                if !events.is_empty() {
                    results.push((domain_name.clone(), proj_name, events));
                }
            }
        }
    }
    results
}

/// Materialized item state from replaying events.
#[derive(Debug, Clone)]
pub struct MaterializedItem {
    pub ticket_id: String,
    pub project: String,
    pub group: Option<String>,
    pub epic: Option<String>,
    pub parent: Option<String>,
    pub tags: Vec<String>,
    pub domain: String,
    pub title: String,
    pub description: Option<String>,
    pub status: String,
    pub priority: String,
    pub assignee: Option<String>,
    pub deadline: Option<String>,
    pub source: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
    pub archived: bool,
    pub notes: Vec<MaterializedNote>,
    pub attachments: Vec<MaterializedAttachment>,
}

#[derive(Debug, Clone)]
pub struct MaterializedAttachment {
    pub attachment_id: String,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
    pub storage_path: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct MaterializedNote {
    pub text: String,
    pub author: Option<String>,
    pub created_at: String,
}

pub fn materialize(domain: &str, events: &[KanbanEvent]) -> Vec<MaterializedItem> {
    let mut items: HashMap<String, MaterializedItem> = HashMap::new();

    for event in events {
        match event {
            KanbanEvent::Create {
                ticket_id, title, project, group, epic, parent, tags, status, priority,
                description, deadline, assignee, source, timestamp,
            } => {
                let completed_at = if status == "done" { Some(timestamp.clone()) } else { None };
                items.insert(ticket_id.clone(), MaterializedItem {
                    ticket_id: ticket_id.clone(),
                    project: project.clone(),
                    group: group.clone(),
                    epic: epic.clone(),
                    parent: parent.clone(),
                    tags: tags.clone(),
                    domain: domain.to_string(),
                    title: title.clone(),
                    description: description.clone(),
                    status: status.clone(),
                    priority: priority.clone(),
                    assignee: assignee.clone(),
                    deadline: deadline.clone(),
                    source: source.clone(),
                    created_at: timestamp.clone(),
                    updated_at: timestamp.clone(),
                    completed_at,
                    archived: false,
                    notes: vec![],
                    attachments: vec![],
                });
            }
            KanbanEvent::Move { ticket_id, to, timestamp, .. } => {
                if let Some(item) = items.get_mut(ticket_id) {
                    let old_status = item.status.clone();
                    item.status = to.clone();
                    item.updated_at = timestamp.clone();
                    if to == "done" {
                        item.completed_at = Some(timestamp.clone());
                    } else {
                        item.completed_at = None;
                    }
                    item.notes.push(MaterializedNote {
                        text: format!("Status: {old_status} → {to}"),
                        author: None,
                        created_at: timestamp.clone(),
                    });
                }
            }
            KanbanEvent::Update { ticket_id, fields, timestamp } => {
                if let Some(item) = items.get_mut(ticket_id) {
                    if let Some(serde_json::Value::String(v)) = fields.get("title") { item.title = v.clone(); }
                    if let Some(serde_json::Value::String(v)) = fields.get("description") { item.description = Some(v.clone()); }
                    if let Some(serde_json::Value::String(v)) = fields.get("status") {
                        item.status = v.clone();
                        if v == "done" { item.completed_at = Some(timestamp.clone()); }
                        else { item.completed_at = None; }
                    }
                    if let Some(serde_json::Value::String(v)) = fields.get("priority") { item.priority = v.clone(); }
                    if let Some(serde_json::Value::String(v)) = fields.get("assignee") { item.assignee = Some(v.clone()); }
                    if let Some(serde_json::Value::String(v)) = fields.get("deadline") { item.deadline = Some(v.clone()); }
                    if let Some(serde_json::Value::String(v)) = fields.get("epic") { item.epic = Some(v.clone()); }
                    if let Some(v) = fields.get("parent") {
                        match v {
                            serde_json::Value::String(s) => item.parent = Some(s.clone()),
                            serde_json::Value::Null => item.parent = None,
                            _ => {}
                        }
                    }
                    if let Some(serde_json::Value::Array(arr)) = fields.get("tags") {
                        item.tags = arr.iter().filter_map(|v| v.as_str().map(String::from)).collect();
                    }
                    item.updated_at = timestamp.clone();
                }
            }
            KanbanEvent::Note { ticket_id, text, author, timestamp } => {
                if let Some(item) = items.get_mut(ticket_id) {
                    item.notes.push(MaterializedNote {
                        text: text.clone(),
                        author: author.clone(),
                        created_at: timestamp.clone(),
                    });
                    item.updated_at = timestamp.clone();
                }
            }
            KanbanEvent::Archive { ticket_id, timestamp } => {
                if let Some(item) = items.get_mut(ticket_id) {
                    item.archived = true;
                    item.updated_at = timestamp.clone();
                }
            }
            KanbanEvent::Attach { ticket_id, attachment_id, filename, mime_type, size, storage_path, timestamp } => {
                if let Some(item) = items.get_mut(ticket_id) {
                    item.attachments.push(MaterializedAttachment {
                        attachment_id: attachment_id.clone(),
                        filename: filename.clone(),
                        mime_type: mime_type.clone(),
                        size: *size,
                        storage_path: storage_path.clone(),
                        created_at: timestamp.clone(),
                    });
                    item.updated_at = timestamp.clone();
                }
            }
            KanbanEvent::Detach { ticket_id, attachment_id, timestamp } => {
                if let Some(item) = items.get_mut(ticket_id) {
                    item.attachments.retain(|a| a.attachment_id != *attachment_id);
                    item.updated_at = timestamp.clone();
                }
            }
        }
    }

    items.into_values().collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_create_event() {
        let event = KanbanEvent::Create {
            group: None,
            epic: None,
            parent: None,
            tags: vec![],
            ticket_id: "SH-1".into(),
            title: "Fix billing".into(),
            project: "shulops".into(),
            status: "backlog".into(),
            priority: "high".into(),
            description: Some("Details".into()),
            deadline: Some("2026-05-01".into()),
            assignee: None,
            source: Some("hank".into()),
            timestamp: "2026-04-24T18:00:00Z".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: KanbanEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.ticket_id(), "SH-1");
    }

    #[test]
    fn roundtrip_move_event() {
        let event = KanbanEvent::Move {
            ticket_id: "SH-1".into(),
            from: Some("backlog".into()),
            to: "todo".into(),
            timestamp: "2026-04-25T10:00:00Z".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"move\""));
        let parsed: KanbanEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.ticket_id(), "SH-1");
    }

    #[test]
    fn roundtrip_update_event() {
        let mut fields = HashMap::new();
        fields.insert("title".into(), serde_json::Value::String("New title".into()));
        let event = KanbanEvent::Update {
            ticket_id: "SH-1".into(),
            fields,
            timestamp: "2026-04-25T14:00:00Z".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: KanbanEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.ticket_id(), "SH-1");
    }

    #[test]
    fn append_and_read_events() {
        let dir = tempfile::tempdir().unwrap();
        let event = KanbanEvent::Create {
            group: None,
            epic: None,
            parent: None,
            tags: vec![],
            ticket_id: "SH-1".into(),
            title: "Test".into(),
            project: "shulops".into(),
            status: "backlog".into(),
            priority: "medium".into(),
            description: None,
            deadline: None,
            assignee: None,
            source: None,
            timestamp: "2026-04-24T18:00:00Z".into(),
        };
        append_event(dir.path(), "personal", "shulops", &event).unwrap();

        let events = read_events(dir.path(), "personal", "shulops");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].ticket_id(), "SH-1");
    }

    #[test]
    fn next_ticket_number_from_meta() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path();
        // Write events + meta
        let event = KanbanEvent::Create {
            group: None,
            epic: None,
            parent: None,
            tags: vec![],
            ticket_id: "SH-1".into(), title: "A".into(), project: "shulops".into(),
            status: "backlog".into(), priority: "medium".into(),
            description: None, deadline: None, assignee: None, source: None,
            timestamp: "2026-04-24T18:00:00Z".into(),
        };
        append_event(vault, "personal", "shulops", &event).unwrap();
        append_meta(vault, "personal", "shulops", "SH", 2).unwrap();

        assert_eq!(next_ticket_number(vault, "personal", "shulops", "SH"), 2);
    }

    #[test]
    fn next_ticket_number_fallback_scan() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path();
        // Write events without meta
        let event = KanbanEvent::Create {
            group: None,
            epic: None,
            parent: None,
            tags: vec![],
            ticket_id: "SH-3".into(), title: "B".into(), project: "shulops".into(),
            status: "backlog".into(), priority: "medium".into(),
            description: None, deadline: None, assignee: None, source: None,
            timestamp: "2026-04-24T19:00:00Z".into(),
        };
        append_event(vault, "personal", "shulops", &event).unwrap();

        assert_eq!(next_ticket_number(vault, "personal", "shulops", "SH"), 4);
    }

    #[test]
    fn next_ticket_number_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(next_ticket_number(dir.path(), "personal", "shulops", "SH"), 1);
    }

    #[test]
    fn materialize_full_lifecycle() {
        let events = vec![
            KanbanEvent::Create {
                group: None,
            epic: None,
            parent: None,
            tags: vec![],
            ticket_id: "SH-1".into(), title: "Fix billing".into(), project: "shulops".into(),
                status: "backlog".into(), priority: "high".into(),
                description: Some("Details".into()), deadline: Some("2026-05-01".into()),
                assignee: None, source: Some("hank".into()),
                timestamp: "2026-04-24T18:00:00Z".into(),
            },
            KanbanEvent::Move {
                ticket_id: "SH-1".into(), from: Some("backlog".into()), to: "todo".into(),
                timestamp: "2026-04-25T10:00:00Z".into(),
            },
            KanbanEvent::Update {
                ticket_id: "SH-1".into(),
                fields: {
                    let mut f = HashMap::new();
                    f.insert("assignee".into(), serde_json::Value::String("jack".into()));
                    f
                },
                timestamp: "2026-04-25T11:00:00Z".into(),
            },
            KanbanEvent::Note {
                ticket_id: "SH-1".into(), text: "Talked to David".into(),
                author: Some("jack".into()), timestamp: "2026-04-25T12:00:00Z".into(),
            },
            KanbanEvent::Move {
                ticket_id: "SH-1".into(), from: Some("todo".into()), to: "done".into(),
                timestamp: "2026-04-26T10:00:00Z".into(),
            },
        ];

        let items = materialize("personal", &events);
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.status, "done");
        assert_eq!(item.assignee.as_deref(), Some("jack"));
        assert!(item.completed_at.is_some());
        assert_eq!(item.notes.len(), 3); // 2 move notes + 1 manual note
    }
}
