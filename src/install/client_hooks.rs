//! Edits to Claude Code's `settings.json` value: Wardwell's hook handlers and
//! the deny list. Ownership is exact: a handler is Wardwell's only when its
//! executable is a Wardwell binary and its arguments equal a known shape.
//! Does NOT read or write files; the installer owns I/O, backups and consent.

use crate::companion::install::{owned_command, wardwell_args};
use serde_json::{Map, Value, json};
use std::path::Path;

/// One hook handler Wardwell owns: where it lives and what it runs.
pub struct Handler {
    pub event: &'static str,
    pub matcher: Option<&'static str>,
    pub args: &'static str,
    pub timeout: Option<u64>,
}

/// Session start: print the project's context.
pub const SESSION_START: Handler =
    Handler { event: "SessionStart", matcher: None, args: "inject \"$(pwd)\"", timeout: None };
/// Stop: the history check.
pub const STOP: Handler = Handler { event: "Stop", matcher: None, args: "resolve", timeout: None };
/// PreToolUse: the Linear write gate.
pub const GATE: Handler = Handler {
    event: "PreToolUse",
    matcher: Some(crate::gate::linear::MATCHER),
    args: "gate linear",
    timeout: Some(5),
};

/// Every handler `setup` may install, in plan order.
pub const ALL: [&Handler; 3] = [&SESSION_START, &STOP, &GATE];

/// Check the shapes this module edits, so a malformed file fails preflight.
pub fn validate(root: &Value) -> Result<(), String> {
    let object = root.as_object().ok_or("Claude settings must be a JSON object")?;
    if let Some(hooks) = object.get("hooks") {
        for (event, groups) in hooks.as_object().ok_or("hooks must be a JSON object")? {
            for group in groups.as_array().ok_or(format!("hooks.{event} must be an array"))? {
                let group = group.as_object().ok_or(format!("a hooks.{event} entry is not an object"))?;
                if let Some(handlers) = group.get("hooks") {
                    handlers.as_array().ok_or(format!("hooks.{event}[].hooks must be an array"))?;
                }
            }
        }
    }
    if let Some(permissions) = object.get("permissions") {
        let permissions = permissions.as_object().ok_or("permissions must be a JSON object")?;
        if let Some(deny) = permissions.get("deny") {
            deny.as_array().ok_or("permissions.deny must be an array")?;
        }
    }
    Ok(())
}

fn is_owned(handler: &Value, args: &str) -> bool {
    handler["type"] == "command" && handler["command"].as_str().and_then(wardwell_args) == Some(args)
}

fn matcher_fits(group: &Value, matcher: Option<&str>) -> bool {
    let current = group.get("matcher").and_then(Value::as_str).filter(|m| !m.is_empty());
    current == matcher
}

fn event_groups<'a>(root: &'a mut Value, event: &str) -> Result<&'a mut Vec<Value>, String> {
    root.as_object_mut()
        .ok_or("Claude settings must be a JSON object")?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("hooks must be a JSON object")?
        .entry(event)
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or(format!("hooks.{event} must be an array"))
}

/// Leave exactly one handler for `spec` running `command`. An owned handler
/// in a fitting group is updated in place so the file's order is kept.
pub fn ensure(root: &mut Value, spec: &Handler, command: &str) -> Result<(), String> {
    let groups = event_groups(root, spec.event)?;
    let mut kept = false;
    let mut emptied = Vec::new();
    for (index, group) in groups.iter_mut().enumerate() {
        let fits = matcher_fits(group, spec.matcher);
        if group.get("hooks").is_none() && is_owned(group, spec.args) {
            emptied.push(index);
            continue;
        }
        let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) else { continue };
        let before = handlers.len();
        handlers.retain_mut(|handler| {
            if !is_owned(handler, spec.args) {
                return true;
            }
            if kept || !fits {
                return false;
            }
            kept = true;
            update(handler, spec, command);
            true
        });
        if before > 0 && handlers.is_empty() {
            emptied.push(index);
        }
    }
    remove_indices(groups, &emptied);
    if !kept {
        let mut group = Map::new();
        if let Some(matcher) = spec.matcher {
            group.insert("matcher".into(), json!(matcher));
        }
        let mut handler = json!({});
        update(&mut handler, spec, command);
        group.insert("hooks".into(), json!([handler]));
        groups.push(Value::Object(group));
    }
    Ok(())
}

fn update(handler: &mut Value, spec: &Handler, command: &str) {
    handler["type"] = json!("command");
    handler["command"] = json!(command);
    if let Some(timeout) = spec.timeout
        && handler.get("timeout").is_none()
    {
        handler["timeout"] = json!(timeout);
    }
}

fn remove_indices(groups: &mut Vec<Value>, indices: &[usize]) {
    let mut index = 0;
    groups.retain(|_| {
        let keep = !indices.contains(&index);
        index += 1;
        keep
    });
}

/// Remove every handler, in every event, whose command satisfies `owned`.
/// Groups and events left empty by the removal go too. Returns the count.
pub fn remove_where(root: &mut Value, owned: impl Fn(&str) -> bool) -> usize {
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else { return 0 };
    let owned_handler = |handler: &Value| handler["type"] == "command" && handler["command"].as_str().is_some_and(&owned);
    let mut removed = 0;
    let mut empty_events = Vec::new();
    for (event, groups) in hooks.iter_mut() {
        let Some(groups) = groups.as_array_mut() else { continue };
        let had_groups = !groups.is_empty();
        let mut emptied = Vec::new();
        for (index, group) in groups.iter_mut().enumerate() {
            if group.get("hooks").is_none() && owned_handler(group) {
                emptied.push(index);
                removed += 1;
                continue;
            }
            let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) else { continue };
            let before = handlers.len();
            handlers.retain(|handler| !owned_handler(handler));
            removed += before - handlers.len();
            if before > 0 && handlers.is_empty() {
                emptied.push(index);
            }
        }
        remove_indices(groups, &emptied);
        if had_groups && groups.is_empty() {
            empty_events.push(event.clone());
        }
    }
    for event in empty_events {
        hooks.remove(&event);
    }
    removed
}

/// Remove every handler running `spec`'s arguments from a Wardwell binary.
pub fn remove(root: &mut Value, spec: &Handler) -> usize {
    remove_where(root, |command| wardwell_args(command) == Some(spec.args))
}

/// Commands of the handlers for `spec`, in file order.
pub fn commands(root: &Value, spec: &Handler) -> Vec<String> {
    let Some(groups) = root["hooks"][spec.event].as_array() else { return Vec::new() };
    groups
        .iter()
        .flat_map(|group| match group.get("hooks").and_then(Value::as_array) {
            Some(handlers) => handlers.clone(),
            None => vec![group.clone()],
        })
        .filter(|handler| is_owned(handler, spec.args))
        .filter_map(|handler| handler["command"].as_str().map(str::to_string))
        .collect()
}

/// The executable path of a hook command, quoted or not.
pub fn executable(command: &str) -> Option<&str> {
    let command = command.trim();
    match command.strip_prefix('\'') {
        Some(rest) => rest.split_once('\'').map(|(exe, _)| exe),
        None => command.split_once(' ').map(|(exe, _)| exe),
    }
}

/// True when a Companion lifecycle Stop hook for Claude is installed. It
/// already runs the history check, so a second Stop hook would repeat it.
pub fn companion_stop_present(root: &Value) -> bool {
    let Some(groups) = root["hooks"]["Stop"].as_array() else { return false };
    groups.iter().filter_map(|group| group.get("hooks").and_then(Value::as_array)).flatten().any(|handler| {
        handler["command"].as_str().and_then(wardwell_args) == Some("companion lifecycle stop --client claude")
    })
}

/// A Companion lifecycle handler (any action, any client), or `resolve`.
pub fn is_companion_lifecycle(command: &str) -> bool {
    owned_command(command)
}

/// True when the command runs the old Python Linear gate script: its last
/// word is a file named `linear-gate.py`, run directly or by a Python
/// interpreter (optionally through `env`). Nothing else matches.
pub fn is_python_gate(command: &str) -> bool {
    let words: Vec<&str> = command.split_whitespace().map(|word| word.trim_matches(['\'', '"'])).collect();
    let Some((script, before)) = words.split_last() else { return false };
    let name = |word: &str| Path::new(word).file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
    name(script) == "linear-gate.py" && before.iter().all(|word| name(word) == "env" || name(word).starts_with("python"))
}

/// The string entries of `permissions.deny`.
pub fn denied(root: &Value) -> Vec<String> {
    root["permissions"]["deny"]
        .as_array()
        .map(|deny| deny.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

/// Append each tool missing from `permissions.deny`. Returns those added.
pub fn add_denies(root: &mut Value, tools: &[&str]) -> Result<Vec<String>, String> {
    let present = denied(root);
    let missing: Vec<String> = tools.iter().filter(|tool| !present.iter().any(|p| p == *tool)).map(|t| t.to_string()).collect();
    if missing.is_empty() {
        return Ok(missing);
    }
    let deny = root
        .as_object_mut()
        .ok_or("Claude settings must be a JSON object")?
        .entry("permissions")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("permissions must be a JSON object")?
        .entry("deny")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or("permissions.deny must be an array")?;
    deny.extend(missing.iter().map(|tool| json!(tool)));
    Ok(missing)
}

/// Remove exactly these entries from `permissions.deny`. A deny list, then a
/// permissions object, left empty by the removal goes too.
pub fn remove_denies(root: &mut Value, tools: &[String]) {
    let Some(permissions) = root.get_mut("permissions").and_then(Value::as_object_mut) else { return };
    let Some(deny) = permissions.get_mut("deny").and_then(Value::as_array_mut) else { return };
    let before = deny.len();
    deny.retain(|entry| !entry.as_str().is_some_and(|e| tools.iter().any(|t| t == e)));
    if before > 0 && deny.is_empty() {
        permissions.remove("deny");
        if permissions.is_empty()
            && let Some(object) = root.as_object_mut()
        {
            object.remove("permissions");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const BIN: &str = "'/opt/homebrew/bin/wardwell'";

    fn cmd(args: &str) -> String {
        format!("{BIN} {args}")
    }

    #[test]
    fn ensure_adds_once_and_is_idempotent() {
        let mut root = json!({});
        ensure(&mut root, &GATE, &cmd("gate linear")).unwrap();
        let once = root.clone();
        ensure(&mut root, &GATE, &cmd("gate linear")).unwrap();
        assert_eq!(root, once);
        assert_eq!(root["hooks"]["PreToolUse"][0]["matcher"], crate::gate::linear::MATCHER);
        assert_eq!(root["hooks"]["PreToolUse"][0]["hooks"][0]["timeout"], 5);
    }

    #[test]
    fn ensure_updates_an_old_binary_path_in_place_and_keeps_order() {
        let mut root = json!({"hooks": {"SessionStart": [
            {"hooks": [{"type": "command", "command": "/usr/local/bin/wardwell inject \"$(pwd)\""}]},
            {"hooks": [{"type": "command", "command": "peon ping"}]}
        ]}});
        ensure(&mut root, &SESSION_START, &cmd("inject \"$(pwd)\"")).unwrap();
        assert_eq!(root["hooks"]["SessionStart"][0]["hooks"][0]["command"], cmd("inject \"$(pwd)\""));
        assert_eq!(root["hooks"]["SessionStart"][1]["hooks"][0]["command"], "peon ping");
    }

    #[test]
    fn ensure_collapses_duplicates_and_moves_out_of_a_wrong_matcher() {
        let mut root = json!({"hooks": {"PreToolUse": [
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "/a/wardwell gate linear"}, {"type": "command", "command": "rtk"}]},
            {"matcher": crate::gate::linear::MATCHER, "hooks": [{"type": "command", "command": "/b/wardwell gate linear"}]},
            {"matcher": crate::gate::linear::MATCHER, "hooks": [{"type": "command", "command": "/c/wardwell gate linear"}]}
        ]}});
        ensure(&mut root, &GATE, &cmd("gate linear")).unwrap();
        assert_eq!(commands(&root, &GATE), vec![cmd("gate linear")]);
        assert_eq!(root["hooks"]["PreToolUse"][0]["hooks"], json!([{"type": "command", "command": "rtk"}]));
        assert_eq!(root["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn ownership_is_exact_by_binary_name_and_arguments() {
        let foreign = [
            "echo wardwell inject \"$(pwd)\"",
            "other gate linear",
            "/bin/wardwell gate linear --strict",
            "/bin/wardwellx resolve",
            "/bin/notwardwell resolve",
        ];
        let handlers: Vec<Value> = foreign.iter().map(|c| json!({"type": "command", "command": c})).collect();
        let mut root = json!({"hooks": {"Stop": [{"hooks": handlers.clone()}], "PreToolUse": [{"hooks": handlers.clone()}]}});
        assert_eq!(remove(&mut root, &STOP), 0);
        assert_eq!(remove(&mut root, &GATE), 0);
        assert_eq!(root["hooks"]["Stop"][0]["hooks"].as_array().unwrap().len(), 5);
        assert!(!is_python_gate("echo linear-gate.py"));
        assert!(!is_python_gate("ruby ~/.claude/hooks/linear-gate/linear-gate.py"));
        assert!(!is_python_gate("python3 linear-gate.py.bak"));
        assert!(is_python_gate("python3 ~/.claude/hooks/linear-gate/linear-gate.py"));
        assert!(is_python_gate("/usr/bin/env python3 \"/Users/x/.claude/hooks/linear-gate/linear-gate.py\""));
        assert!(is_python_gate("~/.claude/hooks/linear-gate/linear-gate.py"));
    }

    #[test]
    fn remove_where_drops_emptied_groups_and_events_only() {
        let mut root = json!({"hooks": {
            "Stop": [{"hooks": [{"type": "command", "command": "/w/wardwell resolve"}]}],
            "SessionEnd": [],
            "PreToolUse": [{"matcher": "x", "hooks": [{"type": "command", "command": "python3 /h/linear-gate.py"}, {"type": "command", "command": "keep"}]}]
        }});
        assert_eq!(remove_where(&mut root, is_python_gate), 1);
        assert_eq!(remove(&mut root, &STOP), 1);
        assert!(root["hooks"].get("Stop").is_none());
        assert_eq!(root["hooks"]["SessionEnd"], json!([]));
        assert_eq!(root["hooks"]["PreToolUse"][0]["hooks"], json!([{"type": "command", "command": "keep"}]));
    }

    #[test]
    fn deny_entries_are_added_once_and_removed_exactly() {
        let mut root = json!({"permissions": {"deny": ["Bash(rm:*)", "mcp__linear__save_project"]}});
        let added = add_denies(&mut root, &["mcp__linear__save_project", "mcp__linear__delete_comment"]).unwrap();
        assert_eq!(added, vec!["mcp__linear__delete_comment"]);
        assert!(add_denies(&mut root, &["mcp__linear__delete_comment"]).unwrap().is_empty());
        remove_denies(&mut root, &added);
        assert_eq!(root["permissions"]["deny"], json!(["Bash(rm:*)", "mcp__linear__save_project"]));
        let mut fresh = json!({});
        let added = add_denies(&mut fresh, &["a"]).unwrap();
        remove_denies(&mut fresh, &added);
        assert_eq!(fresh, json!({}));
    }

    #[test]
    fn validate_rejects_shapes_it_cannot_edit() {
        for bad in [json!([]), json!({"hooks": []}), json!({"hooks": {"Stop": {}}}), json!({"hooks": {"Stop": [1]}}),
                    json!({"hooks": {"Stop": [{"hooks": {}}]}}), json!({"permissions": []}), json!({"permissions": {"deny": "x"}})] {
            assert!(validate(&bad).is_err(), "{bad}");
        }
        assert!(validate(&json!({"model": "x", "hooks": {"Stop": [{"hooks": []}]}, "permissions": {"deny": []}})).is_ok());
    }

    #[test]
    fn companion_stop_is_detected_only_for_claude() {
        let with = |c: &str| json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": c}]}]}});
        assert!(companion_stop_present(&with("'/w/wardwell' companion lifecycle stop --client claude")));
        assert!(!companion_stop_present(&with("'/w/wardwell' companion lifecycle stop --client codex")));
        assert!(!companion_stop_present(&with("echo companion lifecycle stop --client claude")));
    }
}
