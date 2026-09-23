//! Install only Companion-owned client wiring; validate every input before writing.
use serde_json::{Map, Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const MAX_CONFIG: u64 = 2 * 1024 * 1024;
const START: &str = "<!-- wardwell-companion:start -->";
const END: &str = "<!-- wardwell-companion:end -->";
const INSTRUCTIONS: &str = "## Default Companion handoffs\n\nFor authorized coding work, use the installed wardwell-companion skill without waiting for a separate publication request. Publish genuine remaining owner tasks, decisions, and external waits to Hank. Read source-bound owner responses before asking again. Keep the lifecycle-supplied source identity and checkpoint token; record the current turn's honest outcome before finishing. Use the local Wardwell binary via MCP or `wardwell companion request`; never bypass its delivery journal with a different hosted connector. Unchanged work remains open. Do not invent owner homework, upload transcripts, poll idle sessions, or treat owner responses as external execution authority. Honor explicit local-only restrictions. Report queued/failed delivery distinctly from successful publication.\n";
const COMMAND: &str = "---\ndescription: Publish this session's operational handoff to Hank and read its owner responses.\n---\n\nApply the installed `wardwell-companion` skill. Maintain this conversation's existing source key and use the local Wardwell binary's MCP or `wardwell companion request` path. Publish real remaining tasks, decisions, and waits, then record the supplied lifecycle checkpoint. Read owner responses before asking repeated questions. Do not generate an ephemeral HTML page unless the user explicitly requests one. A queued request is not a successful publication.\n";

struct Change {
    path: PathBuf,
    before: Option<Vec<u8>>,
    after: Vec<u8>,
}

pub fn run(dry_run: bool, skill_file: Option<&Path>) -> Result<Value, String> {
    let home = dirs::home_dir().ok_or("Could not find the home directory")?;
    let binary = std::env::current_exe().map_err(|_| "Could not locate the Wardwell binary")?;
    install_at(&home, &binary, dry_run, skill_file)
}

fn install_at(
    home: &Path,
    binary: &Path,
    dry_run: bool,
    skill_file: Option<&Path>,
) -> Result<Value, String> {
    let mut changes = vec![
        hooks_change(&home.join(".claude/settings.json"), binary, "claude")?,
        hooks_change(&home.join(".codex/hooks.json"), binary, "codex")?,
    ];
    // Resolve/validate the skill before any settings are modified.
    let source = skill_file
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".codex/skills/wardwell-companion/SKILL.md"));
    let skill = read_optional(&source)?
        .ok_or("Canonical Companion skill is missing; supply --skill-file")?;
    let text = utf8(&skill)?;
    if !text.starts_with("---\n")
        || !text.contains("name: wardwell-companion\n")
        || !text.contains("description:")
    {
        return Err("Skill file is not the canonical wardwell-companion skill".into());
    }
    for relative in [
        ".claude/skills/wardwell-companion/SKILL.md",
        ".codex/skills/wardwell-companion/SKILL.md",
    ] {
        changes.push(change(home.join(relative), skill.clone())?);
    }
    for relative in [".claude/CLAUDE.md", ".codex/AGENTS.md"] {
        let path = home.join(relative);
        let before = read_optional(&path)?;
        let after = replace_block(before.as_deref().map(utf8).transpose()?.unwrap_or_default())?
            .into_bytes();
        changes.push(Change {
            path,
            before,
            after,
        });
    }
    let command_path = home.join(".claude/commands/companion.md");
    let before = read_optional(&command_path)?;
    let known = match before.as_deref() {
        None => true,
        Some(bytes) => {
            let text = utf8(bytes)?;
            text == COMMAND
                || (text.contains("A companion is an ephemeral HTML page")
                    && text.contains("/*COMPANION_DATA*/"))
        }
    };
    if !known {
        return Err(
            "Existing /companion is customized; no files changed. Review it before replacing it."
                .into(),
        );
    }
    changes.push(Change {
        path: command_path,
        before,
        after: COMMAND.as_bytes().to_vec(),
    });

    // A simultaneous editor must not be silently overwritten after preflight.
    for item in &changes {
        if read_optional(&item.path)? != item.before {
            return Err("Client configuration changed during preflight; rerun installation".into());
        }
    }
    let mut reports = Vec::new();
    for item in changes {
        let changed = item.before.as_deref() != Some(item.after.as_slice());
        let mut backup = None;
        if changed && !dry_run {
            if read_optional(&item.path)? != item.before {
                return Err(format!(
                    "{} changed during installation. Earlier changes have rollback backups; rerun after review.",
                    item.path.display()
                ));
            }
            backup = item
                .before
                .as_ref()
                .map(|bytes| backup_file(&item.path, bytes))
                .transpose()?;
            atomic_write(&item.path, &item.after).map_err(|error| {
                format!(
                    "{error}; any earlier changed files have .wardwell-backup files for rollback"
                )
            })?;
        }
        reports.push(json!({"path":item.path,"changed":changed,"backup_path":backup}));
    }
    Ok(
        json!({"status":if dry_run {"preview"}else{"installed"},"files":reports,
        "activation":{"claude":"Configuration installed, activation unverified until a native session fires the hooks.",
        "codex":"Review/trust the installed hooks using the native client controls. Installation does not bypass trust or prove activation.",
        "existing_sessions":"SessionStart is not retroactive; resume/start a session after activation."}}),
    )
}

fn change(path: PathBuf, after: Vec<u8>) -> Result<Change, String> {
    Ok(Change {
        before: read_optional(&path)?,
        path,
        after,
    })
}
fn utf8(bytes: &[u8]) -> Result<&str, String> {
    std::str::from_utf8(bytes).map_err(|_| "Client instructions must be UTF-8".into())
}

fn replace_block(input: &str) -> Result<String, String> {
    let mut rest = input;
    let mut preserved = String::new();
    while let Some(start) = rest.find(START) {
        preserved.push_str(&rest[..start]);
        let tail = &rest[start + START.len()..];
        let end = tail
            .find(END)
            .ok_or("Unclosed Companion instruction marker; no files changed")?;
        if tail[..end].contains(START) {
            return Err("Nested Companion instruction markers; no files changed".into());
        }
        rest = &tail[end + END.len()..];
    }
    if rest.contains(END) || preserved.contains(END) {
        return Err("Unmatched Companion instruction marker; no files changed".into());
    }
    preserved.push_str(rest);
    Ok(format!(
        "{}\n\n{START}\n{INSTRUCTIONS}{END}\n",
        preserved.trim_end()
    ))
}

fn hooks_change(path: &Path, binary: &Path, client: &str) -> Result<Change, String> {
    let before = read_optional(path)?;
    let mut root: Value = match before.as_deref() {
        Some(bytes) => serde_json::from_slice(bytes)
            .map_err(|_| format!("Malformed JSON in {}; no files changed", path.display()))?,
        None => json!({}),
    };
    let object = root
        .as_object_mut()
        .ok_or("Client settings must be a JSON object")?;
    let hooks = object
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("hooks must be a JSON object")?;
    let command = shell_quote(binary)?;
    for (event, action, matcher) in [
        ("SessionStart", "resume", Some("resume")),
        ("UserPromptSubmit", "begin", None),
        ("Stop", "stop", None),
    ] {
        merge_event(hooks, event, action, matcher, client, &command)?;
    }
    let after =
        serde_json::to_vec_pretty(&root).map_err(|_| "Could not encode hook configuration")?;
    Ok(Change {
        path: path.into(),
        before,
        after,
    })
}

fn merge_event(
    hooks: &mut Map<String, Value>,
    event: &str,
    action: &str,
    matcher: Option<&str>,
    client: &str,
    binary: &str,
) -> Result<(), String> {
    let groups = hooks
        .entry(event)
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or("Hook event must be an array")?;
    for group in groups.iter_mut() {
        if !group.is_object() {
            return Err("Malformed hook group; no files changed".into());
        }
        if let Some(handlers) = group.get_mut("hooks") {
            let handlers = handlers
                .as_array_mut()
                .ok_or("Hook handlers must be an array")?;
            handlers.retain(|handler| {
                !(handler["type"] == "command"
                    && handler["command"].as_str().is_some_and(owned_command))
            });
        }
    }
    groups.retain(|group| {
        group
            .get("hooks")
            .and_then(Value::as_array)
            .is_none_or(|handlers| !handlers.is_empty())
    });
    let mut group = Map::new();
    if let Some(value) = matcher {
        group.insert("matcher".into(), json!(value));
    }
    group.insert("hooks".into(),json!([{"type":"command","command":format!("{binary} companion lifecycle {action} --client {client}"),"timeout":if action=="resume"{6}else{3}}]));
    groups.push(Value::Object(group));
    Ok(())
}

fn owned_command(command: &str) -> bool {
    let command = command.trim();
    let pair = if let Some(rest) = command.strip_prefix('\'') {
        rest.split_once('\'').map(|(exe, args)| (exe, args.trim()))
    } else {
        command
            .split_once(' ')
            .map(|(exe, args)| (exe, args.trim()))
    };
    let Some((exe, args)) = pair else {
        return false;
    };
    let Some(name) = Path::new(exe).file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    let owned = name == "wardwell"
        || name.strip_prefix("wardwell-").is_some_and(|suffix| {
            suffix.starts_with(|c: char| c.is_ascii_digit())
                && suffix
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'))
        });
    if !owned {
        return false;
    }
    if args == "resolve" {
        return true;
    }
    // Local context injection is deliberately preserved. Match only our exact
    // lifecycle shape, never a shell snippet mentioning Wardwell or another app.
    ["begin", "resume", "stop"].iter().any(|action| {
        ["claude", "codex"]
            .iter()
            .any(|client| args == format!("companion lifecycle {action} --client {client}"))
    })
}

fn shell_quote(path: &Path) -> Result<String, String> {
    let text = path.to_str().ok_or("Wardwell binary path is not UTF-8")?;
    if text.chars().any(char::is_control) {
        return Err("Invalid binary path".into());
    }
    Ok(format!("'{}'", text.replace('\'', "'\\''")))
}
fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::symlink_metadata(path) {
        Ok(meta)
            if meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= MAX_CONFIG =>
        {
            let mut bytes = Vec::new();
            File::open(path)
                .and_then(|file| file.take(MAX_CONFIG + 1).read_to_end(&mut bytes))
                .map_err(|_| format!("Could not read {}", path.display()))?;
            if bytes.len() as u64 > MAX_CONFIG {
                return Err("Client file exceeds size limit".into());
            }
            Ok(Some(bytes))
        }
        Ok(_) => Err(format!("{} is not a bounded regular file", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(format!("Could not inspect {}", path.display())),
    }
}
fn backup_file(path: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("Config path has no filename")?;
    let backup = path.with_file_name(format!("{name}.wardwell-backup-{}", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(&backup)
        .map_err(|_| "Could not create rollback backup")?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| "Could not write rollback backup")?;
    Ok(backup)
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("Config path has no parent")?;
    fs::create_dir_all(parent).map_err(|_| "Could not create client directory")?;
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("Client destination became a symlink".into());
    }
    let temp = parent.join(format!(".wardwell-install-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options
            .open(&temp)
            .map_err(|_| "Could not create temporary config")?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| "Could not write config")?;
        drop(file);
        fs::rename(&temp, path).map_err(|_| "Could not install config")?;
        File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|_| "Could not sync client directory")
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(str::to_owned)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    fn put(home: &Path, relative: &str, text: &str) {
        let p = home.join(relative);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    fn skill(home: &Path) -> PathBuf {
        put(
            home,
            "skill.md",
            "---\nname: wardwell-companion\ndescription: test\n---\nInstructions\n",
        );
        home.join("skill.md")
    }
    #[test]
    fn preserves_context_mixed_hooks_and_settings_and_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let h = d.path();
        let s = skill(h);
        put(
            h,
            ".claude/settings.json",
            r#"{"model":"keep","hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"/opt/homebrew/bin/wardwell inject \"$(pwd)\""},{"type":"command","command":"peon ping"}]}],"Stop":[{"hooks":[{"type":"command","command":"rtk check"},{"type":"command","command":"/opt/homebrew/bin/wardwell resolve"}]}],"SessionEnd":[{"hooks":[{"type":"command","command":"peon bye"}]}]}}"#,
        );
        put(h, ".claude/CLAUDE.md", "Personal instructions\n");
        let report = install_at(h, Path::new("/opt/wardwell"), false, Some(&s)).unwrap();
        let settings = fs::read_to_string(h.join(".claude/settings.json")).unwrap();
        for value in [
            "wardwell inject",
            "peon ping",
            "peon bye",
            "rtk check",
            "keep",
        ] {
            assert!(settings.contains(value));
        }
        assert!(!settings.contains("wardwell resolve"));
        let backup = report["files"][0]["backup_path"].as_str().unwrap();
        assert!(
            fs::read_to_string(backup)
                .unwrap()
                .contains("wardwell resolve")
        );
        let again = install_at(h, Path::new("/opt/wardwell"), false, Some(&s)).unwrap();
        assert!(
            again["files"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["changed"] == false)
        );
        assert!(
            fs::read_to_string(h.join(".claude/CLAUDE.md"))
                .unwrap()
                .starts_with("Personal instructions")
        );
    }
    #[test]
    fn malformed_second_client_leaves_first_untouched() {
        let d = tempfile::tempdir().unwrap();
        let h = d.path();
        let s = skill(h);
        put(h, ".claude/settings.json", "{}");
        put(h, ".codex/hooks.json", "{");
        assert!(install_at(h, Path::new("/wardwell"), false, Some(&s)).is_err());
        assert_eq!(
            fs::read_to_string(h.join(".claude/settings.json")).unwrap(),
            "{}"
        );
        assert_eq!(fs::read_dir(h.join(".claude")).unwrap().count(), 1);
    }
    #[test]
    fn unknown_command_is_preserved_and_dry_run_writes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let h = d.path();
        let s = skill(h);
        let report = install_at(h, Path::new("/wardwell"), true, Some(&s)).unwrap();
        assert_eq!(report["status"], "preview");
        assert!(!h.join(".claude").exists());
        put(h, ".claude/commands/companion.md", "My custom workflow");
        assert!(install_at(h, Path::new("/wardwell"), false, Some(&s)).is_err());
        assert!(!h.join(".claude/settings.json").exists());
        assert_eq!(
            fs::read_to_string(h.join(".claude/commands/companion.md")).unwrap(),
            "My custom workflow"
        );
    }
    #[test]
    fn recognized_html_command_is_backed_up_and_redirected() {
        let d = tempfile::tempdir().unwrap();
        let h = d.path();
        let s = skill(h);
        put(
            h,
            ".claude/commands/companion.md",
            "A companion is an ephemeral HTML page /*COMPANION_DATA*/",
        );
        install_at(h, Path::new("/wardwell"), false, Some(&s)).unwrap();
        assert_eq!(
            fs::read_to_string(h.join(".claude/commands/companion.md")).unwrap(),
            COMMAND
        );
    }
    #[test]
    fn ownership_is_narrow_and_duplicate_markers_collapse() {
        assert!(!owned_command("echo wardwell resolve"));
        assert!(!owned_command(
            "other companion lifecycle stop --client codex"
        ));
        assert!(!owned_command("wardwell inject ."));
        assert!(owned_command(
            "'/Users/x/.wardwell/bin/wardwell-0.11.1+companion.1' companion lifecycle stop --client codex"
        ));
        let text = format!("Keep\n{START}\nold\n{END}\nMiddle\n{START}\nold\n{END}\nEnd\n");
        let result = replace_block(&text).unwrap();
        assert_eq!(result.matches(START).count(), 1);
        assert!(result.contains("Middle"));
        assert!(result.contains("End"));
    }
}
