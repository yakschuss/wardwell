//! The careful installer for Wardwell's client wiring, shared by `setup`,
//! `init` and `uninstall`. Tier one, memory plumbing, is always planned: the
//! session-start hook and the Stop hook. Tier two, "Tracker policy, optional",
//! is planned only when a linear binding carries `gate: true`: the Linear gate
//! and the deny list. The hourly tracker pull is planned when any binding
//! exists. Preflight reads and validates every file before any write; apply
//! re-checks each file, backs it up beside itself (0600), and writes through a
//! temp file and a rename. Every path comes from the caller.
//! Does NOT ask for consent, print, edit MCP server entries, or touch the vault.

use crate::companion::install::{atomic_write, backup_file, read_optional, shell_quote};
use crate::config::loader::TrackerBinding;
use crate::gate::ruleset::LINEAR_UPDATES;
use crate::install::client_hooks::{self, GATE, Handler, SESSION_START, STOP};
use crate::install::json_doc;
use crate::install::manifest::{self, Manifest};
use crate::tracker::schedule::{self, LaunchctlRunner};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The label every tier-two plan line starts with.
pub const POLICY: &str = "Tracker policy, optional";
/// The pull interval when no agent is installed yet; an existing one is kept.
pub const PULL_INTERVAL: u32 = 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Create,
    UpdateBackup,
    /// A Wardwell-generated file replaced whole; nothing of the user's is in it.
    Update,
    Remove,
    /// A Wardwell-generated file deleted; nothing of the user's is in it.
    Delete,
    /// A step not taken because its input could not be read.
    Skipped,
    Unchanged,
    Off,
    Manual,
}

impl Action {
    pub fn word(self) -> &'static str {
        match self {
            Action::Create => "CREATE",
            Action::UpdateBackup => "UPDATE + BACKUP",
            Action::Update => "UPDATE",
            Action::Remove => "REMOVE + BACKUP",
            Action::Delete => "REMOVE",
            Action::Skipped => "SKIPPED",
            Action::Unchanged => "UNCHANGED",
            Action::Off => "OFF",
            Action::Manual => "MANUAL",
        }
    }
}

/// One line of the preview.
#[derive(Debug)]
pub struct Line {
    pub action: Action,
    pub label: String,
    pub path: Option<PathBuf>,
}

impl Line {
    pub fn render(&self) -> String {
        match &self.path {
            Some(path) => format!("    {:<15} {} \u{2192} {}", self.action.word(), self.label, path.display()),
            None => format!("    {:<15} {}", self.action.word(), self.label),
        }
    }
}

/// What the installer reads. Paths are parameters so tests never touch $HOME.
pub struct Inputs<'a> {
    pub home: &'a Path,
    pub config_dir: &'a Path,
    pub binary: &'a Path,
    pub trackers: &'a BTreeMap<String, TrackerBinding>,
    /// Edit Claude Code's settings; false when Claude Code is not installed.
    pub claude_code: bool,
    /// Install the pull as a launchd agent; false prints a cron line instead.
    pub launchd: bool,
}

#[derive(Debug)]
struct Change {
    path: PathBuf,
    before: Option<Vec<u8>>,
    after: Vec<u8>,
    /// The file's permission bits before the change, restored after it.
    mode: Option<u32>,
}

fn mode_of(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).ok().map(|meta| meta.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn restore_mode(path: &Path, mode: Option<u32>) -> Result<(), String> {
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|_| format!("Could not restore the permissions of {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

#[derive(Debug)]
enum Pull {
    Keep,
    Manual,
    /// `backup`: the plist on disk was edited by hand; save it before replacing it.
    Install { home: PathBuf, config_dir: PathBuf, binary: PathBuf, interval: u32, backup: bool },
    Remove { home: PathBuf },
}

/// A previewed set of changes. Applying it writes exactly what it shows.
#[derive(Debug)]
pub struct Plan {
    pub lines: Vec<Line>,
    changes: Vec<Change>,
    pull: Pull,
    policy: bool,
    settings: bool,
    /// Steps that cannot be taken, each a sentence that says what to do.
    pub failures: Vec<String>,
}

impl Plan {
    /// True when applying would change nothing.
    pub fn is_noop(&self) -> bool {
        self.changes.is_empty() && matches!(self.pull, Pull::Keep | Pull::Manual)
    }

    /// Labels of the lines that remove something, for a summary after apply.
    pub fn removals(&self) -> Vec<String> {
        self.lines.iter().filter(|line| matches!(line.action, Action::Remove | Action::Delete)).map(|line| line.label.clone()).collect()
    }

    /// What the user must do before the change is active, said honestly.
    pub fn activation(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.settings {
            notes.push("Installed is not active. Claude Code reads hooks and permissions when a session starts.".to_string());
            notes.push("Sessions already running do not change. Start a new session, then run `wardwell doctor`.".to_string());
        }
        if self.policy {
            notes.push(format!(
                "The Linear gate checks Linear comments and issues from the next session, with ruleset {} v{}.",
                LINEAR_UPDATES.name, LINEAR_UPDATES.version
            ));
        }
        match self.pull {
            Pull::Install { .. } => notes.push("launchd runs the tracker pull now and then on its interval. `wardwell tracker status` shows the last pull.".to_string()),
            Pull::Manual => notes.push("The tracker pull is not scheduled. Add the cron line above.".to_string()),
            Pull::Keep | Pull::Remove { .. } => {}
        }
        notes
    }
}

pub fn settings_path(home: &Path) -> PathBuf {
    home.join(".claude/settings.json")
}

/// True when a linear binding opts into the tracker policy.
pub fn policy_enabled(trackers: &BTreeMap<String, TrackerBinding>) -> bool {
    trackers.values().any(|binding| binding.provider == "linear" && binding.gate)
}

/// Claude settings being edited in memory, one planned step at a time.
struct Draft {
    path: PathBuf,
    before: Option<Vec<u8>>,
    /// The original text as a document, so a rewrite keeps what the user wrote.
    doc: Option<json_doc::Node>,
    original: Value,
    value: Value,
    removing: bool,
}

impl Draft {
    fn read(path: PathBuf, removing: bool) -> Result<Draft, String> {
        if let Ok(target) = std::fs::read_link(&path) {
            return Err(format!(
                "{} is a symbolic link to {}. Run setup against the real file, or replace the link with a regular file. No files changed.",
                path.display(),
                target.display()
            ));
        }
        let before = read_optional(&path)?;
        let value = match before.as_deref() {
            Some(bytes) if bytes.iter().all(u8::is_ascii_whitespace) => json!({}),
            Some(bytes) => serde_json::from_slice(bytes)
                .map_err(|_| format!("Malformed JSON in {}; no files changed", path.display()))?,
            None => json!({}),
        };
        if !value.is_object() {
            return Err(format!(
                "{} must be a JSON object, such as {{\"hooks\": {{}}}}; it holds {}. No files changed.",
                path.display(),
                kind(&value)
            ));
        }
        client_hooks::validate(&value).map_err(|error| format!("{error} in {}; no files changed", path.display()))?;
        let doc = match before.as_deref().map(std::str::from_utf8) {
            Some(Ok(text)) if !text.trim().is_empty() => Some(json_doc::parse(text)?),
            Some(Err(_)) => return Err(format!("{} is not UTF-8; no files changed", path.display())),
            _ => None,
        };
        Ok(Draft { path, before, doc, original: value.clone(), value, removing })
    }

    /// Apply one edit and record its plan line.
    fn step(&mut self, lines: &mut Vec<Line>, label: String, edit: impl FnOnce(&mut Value) -> Result<(), String>) -> Result<(), String> {
        let previous = self.value.clone();
        edit(&mut self.value)?;
        let action = match (self.value == previous, self.removing, self.before.is_some()) {
            (true, _, _) => Action::Unchanged,
            (false, true, _) => Action::Remove,
            (false, false, true) => Action::UpdateBackup,
            (false, false, false) => Action::Create,
        };
        lines.push(Line { action, label, path: Some(self.path.clone()) });
        Ok(())
    }

    fn has(&self, owned: impl Fn(&str) -> bool) -> bool {
        client_hooks::remove_where(&mut self.value.clone(), owned) > 0
    }

    fn into_change(self) -> Result<Option<Change>, String> {
        if self.value == self.original {
            return Ok(None);
        }
        let after = match &self.doc {
            Some(doc) => json_doc::render(&json_doc::reconcile(doc, &self.value)).into_bytes(),
            None => {
                let mut after = serde_json::to_vec_pretty(&self.value).map_err(|_| "Could not encode Claude settings")?;
                after.push(b'\n');
                after
            }
        };
        let mode = mode_of(&self.path);
        Ok(Some(Change { path: self.path, before: self.before, after, mode }))
    }
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn command(quoted: &str, spec: &Handler) -> String {
    format!("{quoted} {}", spec.args)
}

/// Preview `setup`: read and validate everything, write nothing.
pub fn plan(inputs: &Inputs) -> Result<Plan, String> {
    let mut lines = Vec::new();
    let mut changes = Vec::new();
    let policy = policy_enabled(inputs.trackers);
    if inputs.claude_code {
        let recorded = manifest::read(inputs.config_dir)?;
        let mut draft = Draft::read(settings_path(inputs.home), false)?;
        let quoted = shell_quote(inputs.binary)?;
        let mut record = recorded.as_ref().map(|(_, m)| m.claude_permissions_deny.clone()).unwrap_or_default();
        let mut created = recorded.as_ref().map(|(_, m)| m.created_keys.clone()).unwrap_or_default();
        memory_steps(&mut draft, &mut lines, &quoted)?;
        policy_steps(&mut draft, &mut lines, &quoted, policy, &mut record)?;
        prune_created(&mut draft.value, &mut created);
        note_created(&draft.original, &draft.value, &mut created);
        changes.extend(draft.into_change()?);
        let next = Manifest { claude_permissions_deny: record, created_keys: created, ..Manifest::default() };
        changes.extend(manifest_change(inputs.config_dir, recorded, &next, &mut lines)?);
    }
    let pull = pull_step(inputs, &mut lines);
    Ok(Plan { lines, changes, pull, policy: policy && inputs.claude_code, settings: inputs.claude_code, failures: Vec::new() })
}

/// Tier one: the session-start hook and the Stop hook.
fn memory_steps(draft: &mut Draft, lines: &mut Vec<Line>, quoted: &str) -> Result<(), String> {
    draft.step(lines, "Session start hook (project context)".into(), |v| client_hooks::ensure(v, &SESSION_START, &command(quoted, &SESSION_START)))?;
    if client_hooks::companion_stop_present(&draft.value) {
        return draft.step(lines, "Stop hook: the Companion Stop hook runs the history check".into(), |v| {
            client_hooks::remove(v, &STOP);
            Ok(())
        });
    }
    draft.step(lines, "Stop hook (history check)".into(), |v| client_hooks::ensure(v, &STOP, &command(quoted, &STOP)))
}

/// Tier two: the gate, the deny list, and the old Python gate's removal; or,
/// with the policy off, removal of what an earlier setup installed.
fn policy_steps(draft: &mut Draft, lines: &mut Vec<Line>, quoted: &str, policy: bool, record: &mut Vec<String>) -> Result<(), String> {
    let python = draft.has(client_hooks::is_python_gate);
    let tools = LINEAR_UPDATES.denied_tools;
    if policy {
        let gate_label = format!("{POLICY}: Linear gate, ruleset {} v{}", LINEAR_UPDATES.name, LINEAR_UPDATES.version);
        draft.step(lines, gate_label, |v| client_hooks::ensure(v, &GATE, &command(quoted, &GATE)))?;
        draft.step(lines, format!("{POLICY}: deny list, {} destructive Linear tools", tools.len()), |v| {
            let stale: Vec<String> = record.iter().filter(|t| !tools.contains(&t.as_str())).cloned().collect();
            client_hooks::remove_denies(v, &stale);
            record.retain(|t| tools.contains(&t.as_str()));
            record.extend(client_hooks::add_denies(v, tools)?);
            Ok(())
        })?;
        if python {
            draft.step(lines, format!("{POLICY}: remove the old Python Linear gate hook (linear-gate.py); the script file stays"), |v| {
                client_hooks::remove_where(v, client_hooks::is_python_gate);
                Ok(())
            })?;
        }
        return Ok(());
    }
    if draft.has(|c| crate::companion::install::wardwell_args(c) == Some(GATE.args)) || !record.is_empty() {
        let label = format!("{POLICY}: remove the Linear gate and Wardwell's deny entries; no linear binding has gate: true");
        draft.step(lines, label, |v| {
            client_hooks::remove(v, &GATE);
            client_hooks::remove_denies(v, record);
            Ok(())
        })?;
        record.clear();
    } else {
        lines.push(Line { action: Action::Off, label: format!("{POLICY}: off; no linear binding has gate: true"), path: None });
    }
    if python {
        let label = format!("{POLICY}: old Python Linear gate (linear-gate.py) kept; set gate: true on the linear binding to replace it");
        lines.push(Line { action: Action::Unchanged, label, path: Some(draft.path.clone()) });
    }
    Ok(())
}

/// Settings keys whose creation the install record notes, innermost first.
const CREATED_KEYS: [&str; 3] = ["permissions.deny", "permissions", "hooks"];

fn key_at<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match key.split_once('.') {
        Some((outer, inner)) => value.get(outer)?.get(inner),
        None => value.get(key),
    }
}

/// Note the keys this run created; forget the ones that are gone.
fn note_created(original: &Value, value: &Value, created: &mut Vec<String>) {
    for key in CREATED_KEYS {
        let listed = created.iter().any(|k| k == key);
        match (key_at(original, key).is_some(), key_at(value, key).is_some()) {
            (false, true) if !listed => created.push(key.to_string()),
            (_, false) => created.retain(|k| k != key),
            _ => {}
        }
    }
}

/// Remove each key Wardwell created that is now empty, and forget it.
fn prune_created(value: &mut Value, created: &mut Vec<String>) {
    for key in CREATED_KEYS {
        let empty = key_at(value, key).is_some_and(|v| v.as_object().is_some_and(|o| o.is_empty()) || v.as_array().is_some_and(|a| a.is_empty()));
        if !empty || !created.iter().any(|k| k == key) {
            continue;
        }
        let parent = match key.split_once('.') {
            Some((outer, _)) => value.get_mut(outer),
            None => Some(&mut *value),
        };
        if let Some(object) = parent.and_then(Value::as_object_mut) {
            object.remove(key.rsplit('.').next().unwrap_or(key));
        }
        created.retain(|k| k != key);
    }
}

fn manifest_change(config_dir: &Path, recorded: Option<(Vec<u8>, Manifest)>, next: &Manifest, lines: &mut Vec<Line>) -> Result<Option<Change>, String> {
    let path = manifest::path(config_dir);
    let label = "Install record (entries and keys Wardwell added)".to_string();
    let next = &manifest::normalized(next);
    let (action, before) = match recorded {
        Some((_, old)) if old == *next => (Action::Unchanged, None),
        Some((bytes, _)) => (Action::UpdateBackup, Some(bytes)),
        None if *next == Manifest::default() => return Ok(None),
        None => (Action::Create, None),
    };
    lines.push(Line { action, label, path: Some(path.clone()) });
    if action == Action::Unchanged {
        return Ok(None);
    }
    let mode = mode_of(&path);
    Ok(Some(Change { path, before, after: manifest::encode(next)?, mode }))
}

/// True when `current` is exactly what Wardwell writes for its own program
/// and interval, so replacing it loses nothing the user wrote.
fn is_generated(current: &str, inputs: &Inputs) -> bool {
    let program = schedule::scheduled_program(inputs.home);
    let interval = schedule::schedule_status(inputs.home);
    match (program, interval) {
        (Some(program), Some(interval)) => current == schedule::launch_agent_plist(&program, interval, &schedule::log_path(inputs.config_dir)),
        _ => false,
    }
}

fn pull_step(inputs: &Inputs, lines: &mut Vec<Line>) -> Pull {
    if inputs.trackers.is_empty() {
        let plist = schedule::plist_path(inputs.home);
        if !inputs.launchd || !plist.exists() {
            return Pull::Keep;
        }
        lines.push(Line { action: Action::Delete, label: "Tracker pull service: no tracker binding".into(), path: Some(plist) });
        return Pull::Remove { home: inputs.home.to_path_buf() };
    }
    // The path the hooks get, never resolved, so an upgrade keeps it valid.
    let binary = inputs.binary.to_path_buf();
    if !inputs.launchd {
        let label = format!("Hourly tracker pull: launchd is macOS only. Add to your crontab: 0 * * * * {} tracker pull", binary.display());
        lines.push(Line { action: Action::Manual, label, path: None });
        return Pull::Manual;
    }
    let path = schedule::plist_path(inputs.home);
    let interval = schedule::schedule_status(inputs.home).unwrap_or(PULL_INTERVAL);
    let expected = schedule::launch_agent_plist(&binary, interval, &schedule::log_path(inputs.config_dir));
    let action = match std::fs::read_to_string(&path) {
        Ok(current) if current == expected => Action::Unchanged,
        Ok(current) if is_generated(&current, inputs) => Action::Update,
        Ok(_) => Action::UpdateBackup,
        Err(_) => Action::Create,
    };
    lines.push(Line { action, label: format!("Tracker pull service, every {interval}s"), path: Some(path) });
    match action {
        Action::Unchanged => Pull::Keep,
        _ => Pull::Install {
            home: inputs.home.to_path_buf(),
            config_dir: inputs.config_dir.to_path_buf(),
            binary: inputs.binary.to_path_buf(),
            interval,
            backup: action == Action::UpdateBackup,
        },
    }
}

/// Preview `uninstall`: Wardwell's hooks in Claude settings, the deny entries
/// the install record lists and only those, and the pull service. The
/// Companion install is left whole: its hooks, skills, command and blocks. The
/// Wardwell folder is never removed; the install record is emptied, not deleted.
pub fn uninstall_plan(home: &Path, config_dir: &Path) -> Plan {
    let mut lines = Vec::new();
    let mut changes = Vec::new();
    let mut failures = Vec::new();
    let path = settings_path(home);
    match Draft::read(path.clone(), true) {
        Ok(draft) => {
            if let Err(error) = settings_removal(draft, config_dir, &mut lines, &mut changes) {
                failures.push(format!("Claude Code hooks were not removed: {error} Fix the file, then run `wardwell uninstall` again."));
            }
        }
        Err(error) => {
            lines.push(Line { action: Action::Skipped, label: "Claude Code hooks and deny entries: settings could not be edited".into(), path: Some(path) });
            failures.push(format!("Claude Code hooks were not removed: {error} Fix or replace the file, then run `wardwell uninstall` again."));
        }
    }
    if let Some(skipped) = lines.iter().find(|line| line.action == Action::Skipped && line.label.starts_with(DENY_LABEL)) {
        failures.push(skipped.label.clone());
    }
    lines.push(Line {
        action: Action::Unchanged,
        label: "The Companion install was not removed: its hooks, skills, command and instructions stay. Remove it separately if you want.".into(),
        path: None,
    });
    let plist = schedule::plist_path(home);
    let pull = match plist.exists() {
        true => {
            lines.push(Line { action: Action::Delete, label: "Tracker pull service".into(), path: Some(plist) });
            Pull::Remove { home: home.to_path_buf() }
        }
        false => Pull::Keep,
    };
    Plan { lines, changes, pull, policy: false, settings: false, failures }
}

const DENY_LABEL: &str = "Deny entries Wardwell added";

/// The settings part of uninstall. An install record that cannot be read
/// skips only the deny step: hooks are matched by shape and still go.
fn settings_removal(mut draft: Draft, config_dir: &Path, lines: &mut Vec<Line>, changes: &mut Vec<Change>) -> Result<(), String> {
    for (label, spec) in [("Session start hook", &SESSION_START), ("Stop hook", &STOP), ("Linear gate", &GATE)] {
        draft.step(lines, label.into(), |v| {
            client_hooks::remove(v, spec);
            Ok(())
        })?;
    }
    let recorded = match manifest::read(config_dir) {
        Ok(recorded) => Some(recorded),
        Err(error) => {
            let error = error.trim_end_matches('.');
            let label = format!("{DENY_LABEL}: {error}; deny entries left in place. Remove them by hand, or fix the record and run uninstall again.");
            lines.push(Line { action: Action::Skipped, label, path: Some(manifest::path(config_dir)) });
            None
        }
    };
    if let Some(recorded) = &recorded {
        let denies = recorded.as_ref().map(|(_, m)| m.claude_permissions_deny.clone()).unwrap_or_default();
        draft.step(lines, format!("{DENY_LABEL} ({})", denies.len()), |v| {
            client_hooks::remove_denies(v, &denies);
            Ok(())
        })?;
        let mut created = recorded.as_ref().map(|(_, m)| m.created_keys.clone()).unwrap_or_default();
        prune_created(&mut draft.value, &mut created);
    }
    changes.extend(draft.into_change()?);
    if let Some(recorded) = recorded {
        changes.extend(manifest_change(config_dir, recorded, &Manifest::default(), lines)?);
    }
    Ok(())
}

/// An apply that stopped: the lines of the steps that ran, and why it stopped.
#[derive(Debug)]
pub struct Failed {
    pub lines: Vec<String>,
    pub message: String,
}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failed {}

fn stop(lines: &[String], message: impl Into<String>) -> Failed {
    Failed { lines: lines.to_vec(), message: message.into() }
}

/// Write the plan. Every file is re-checked against the preview, staged as a
/// temp file beside its target, and backed up before the first rename. Files
/// are renamed in plan order: settings first, then the install record. When a
/// later rename fails, the files already renamed are restored from their
/// backups. The pull service runs last.
pub fn apply(plan: &Plan, runner: &dyn LaunchctlRunner, uid: &dyn Fn() -> Result<u32, String>) -> Result<Vec<String>, Failed> {
    apply_with(plan, runner, uid, &|from, to| std::fs::rename(from, to))
}

type Rename = dyn Fn(&Path, &Path) -> std::io::Result<()>;

fn apply_with(plan: &Plan, runner: &dyn LaunchctlRunner, uid: &dyn Fn() -> Result<u32, String>, rename: &Rename) -> Result<Vec<String>, Failed> {
    let mut lines = Vec::new();
    for change in &plan.changes {
        if read_optional(&change.path).map_err(|e| stop(&lines, e))? != change.before {
            return Err(stop(&lines, format!("{} changed since the preview; nothing was written. Run the command again.", change.path.display())));
        }
    }
    let mut staged: Vec<PathBuf> = Vec::new();
    let discard = |staged: &[PathBuf]| staged.iter().for_each(|temp| drop(std::fs::remove_file(temp)));
    for change in &plan.changes {
        match stage(&change.path, &change.after, change.mode) {
            Ok(temp) => staged.push(temp),
            Err(error) => {
                discard(&staged);
                return Err(stop(&lines, format!("Could not stage {} ({error}); nothing was written.", change.path.display())));
            }
        }
    }
    let mut backups = Vec::new();
    for change in &plan.changes {
        match change.before.as_ref().map(|bytes| backup_file(&change.path, bytes)).transpose() {
            Ok(backup) => backups.push(backup),
            Err(error) => {
                discard(&staged);
                return Err(stop(&lines, format!("{error} for {}; nothing was written.", change.path.display())));
            }
        }
    }
    for (index, (change, temp)) in plan.changes.iter().zip(&staged).enumerate() {
        if let Err(error) = rename(temp, &change.path) {
            discard(&staged[index..]);
            let restored = restore(&plan.changes[..index], &mut lines);
            return Err(stop(&lines, format!("Could not write {} ({error}). {restored}", change.path.display())));
        }
        sync_parent(&change.path);
        lines.push(format!("  OK {}", change.path.display()));
        if let Some(backup) = &backups[index] {
            lines.push(format!("    backup: {}", backup.display()));
        }
    }
    let pull = match &plan.pull {
        Pull::Install { home, config_dir, binary, interval, backup } => {
            let saved = match backup {
                true => read_optional(&schedule::plist_path(home))
                    .and_then(|bytes| bytes.map(|bytes| backup_file(&schedule::plist_path(home), &bytes)).transpose()),
                false => Ok(None),
            };
            match saved {
                Ok(Some(saved)) => {
                    lines.push(format!("    backup: {}", saved.display()));
                }
                Ok(None) => {}
                Err(error) => return Err(stop(&lines, format!("Tracker pull service: {error}; the plist was not replaced."))),
            }
            uid().and_then(|uid| schedule::schedule(home, config_dir, *interval, runner, binary, uid)).map(Some)
        }
        Pull::Remove { home } => uid().and_then(|uid| schedule::unschedule(home, runner, uid)).map(Some),
        Pull::Keep | Pull::Manual => Ok(None),
    };
    match pull {
        Ok(Some(line)) => lines.push(format!("  OK {line}")),
        Ok(None) => {}
        Err(error) => return Err(stop(&lines, format!("Tracker pull service: {error}"))),
    }
    Ok(lines)
}

/// Write `bytes` to a new temp file beside `path` with `mode` (0600 when the
/// file is new), synced to disk. Returns the temp path.
fn stage(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<PathBuf, String> {
    use std::io::Write;
    let parent = path.parent().ok_or("no parent directory")?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err("the destination is a symbolic link".into());
    }
    let temp = parent.join(format!(".wardwell-install-{}", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let written = options
        .open(&temp)
        .and_then(|mut file| file.write_all(bytes).and_then(|_| file.sync_all()))
        .map_err(|error| error.to_string())
        .and_then(|_| restore_mode(&temp, mode.or(Some(0o600))));
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    Ok(temp)
}

fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::File::open(parent).and_then(|dir| dir.sync_all());
    }
}

/// Put back the files already renamed: their original bytes and mode, or
/// no file when there was none. Returns the sentence for the failure.
fn restore(done: &[Change], lines: &mut Vec<String>) -> String {
    if done.is_empty() {
        return "Nothing was written.".into();
    }
    let mut failed = Vec::new();
    for change in done {
        let result = match &change.before {
            Some(bytes) => atomic_write(&change.path, bytes).and_then(|_| restore_mode(&change.path, change.mode)),
            None => std::fs::remove_file(&change.path).map_err(|error| error.to_string()),
        };
        match result {
            Ok(()) => lines.push(format!("  RESTORED {}", change.path.display())),
            Err(_) => failed.push(change.path.display().to_string()),
        }
    }
    match failed.is_empty() {
        true => "The files already written were restored from their backups. Nothing changed.".into(),
        false => format!("Could not restore {}; its .wardwell-backup file beside it holds the original.", failed.join(", ")),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::tracker::schedule::fake::Fake;
    use std::fs;

    const BIN: &str = "/nonexistent-wardwell-test/bin/wardwell";

    struct Home {
        _dir: tempfile::TempDir,
        home: PathBuf,
        cfg: PathBuf,
    }

    fn home() -> Home {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cfg = home.join(".wardwell");
        fs::create_dir_all(&cfg).unwrap();
        Home { _dir: dir, home, cfg }
    }

    fn binding(provider: &str, gate: bool) -> BTreeMap<String, TrackerBinding> {
        let binding = TrackerBinding {
            domain: "personal".into(),
            project: "corr".into(),
            provider: provider.into(),
            team: "COR".into(),
            credential: "c".into(),
            readonly: true,
            gate,
        };
        BTreeMap::from([("personal/corr".to_string(), binding)])
    }

    fn plan_for(h: &Home, trackers: &BTreeMap<String, TrackerBinding>, launchd: bool) -> Plan {
        plan(&Inputs { home: &h.home, config_dir: &h.cfg, binary: Path::new(BIN), trackers, claude_code: true, launchd }).unwrap()
    }

    fn run(h: &Home, trackers: &BTreeMap<String, TrackerBinding>) -> Plan {
        let plan = plan_for(h, trackers, false);
        apply(&plan, &Fake::new(&[]), &|| Ok(501)).unwrap();
        plan
    }

    fn settings(h: &Home) -> Value {
        serde_json::from_slice(&fs::read(settings_path(&h.home)).unwrap()).unwrap()
    }

    fn put_settings(h: &Home, value: Value) {
        let path = settings_path(&h.home);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }

    fn rendered(plan: &Plan) -> String {
        plan.lines.iter().map(Line::render).collect::<Vec<_>>().join("\n")
    }

    fn action_of(plan: &Plan, label_start: &str) -> Action {
        plan.lines.iter().find(|l| l.label.starts_with(label_start)).unwrap_or_else(|| panic!("no line {label_start}: {}", rendered(plan))).action
    }

    #[test]
    fn tier_one_is_always_planned_created_then_unchanged() {
        let h = home();
        let first = run(&h, &BTreeMap::new());
        assert_eq!(action_of(&first, "Session start hook"), Action::Create);
        assert_eq!(action_of(&first, "Stop hook"), Action::Create);
        assert_eq!(action_of(&first, POLICY), Action::Off);
        let s = settings(&h);
        assert_eq!(client_hooks::commands(&s, &SESSION_START), vec![format!("'{BIN}' inject \"$(pwd)\"")]);
        assert_eq!(client_hooks::commands(&s, &STOP), vec![format!("'{BIN}' resolve")]);
        assert!(s.get("permissions").is_none());
        assert_eq!(manifest::read(&h.cfg).unwrap().unwrap().1.created_keys, vec!["hooks"]);
        let second = plan_for(&h, &BTreeMap::new(), false);
        assert!(second.is_noop(), "{}", rendered(&second));
        assert!(second.lines.iter().all(|l| matches!(l.action, Action::Unchanged | Action::Off)));
    }

    #[test]
    fn plan_writes_nothing() {
        let h = home();
        let plan = plan_for(&h, &binding("linear", true), false);
        assert!(!plan.is_noop());
        assert!(!h.home.join(".claude").exists());
        assert!(!manifest::path(&h.cfg).exists());
    }

    #[test]
    fn preserves_foreign_settings_and_backs_up_with_owner_only_mode() {
        let h = home();
        let original = json!({"model": "keep", "hooks": {"SessionStart": [
            {"hooks": [{"type": "command", "command": "/usr/local/bin/wardwell inject \"$(pwd)\""}, {"type": "command", "command": "peon ping"}]}
        ], "Stop": [{"hooks": [{"type": "command", "command": "rtk check"}]}]}, "permissions": {"allow": ["Bash(ls)"]}});
        put_settings(&h, original.clone());
        let plan = run(&h, &BTreeMap::new());
        assert_eq!(action_of(&plan, "Session start hook"), Action::UpdateBackup);
        let s = settings(&h);
        assert_eq!(s["model"], "keep");
        assert_eq!(s["permissions"], json!({"allow": ["Bash(ls)"]}));
        assert_eq!(s["hooks"]["SessionStart"][0]["hooks"][1]["command"], "peon ping");
        assert_eq!(s["hooks"]["Stop"][0]["hooks"][0]["command"], "rtk check");
        let backups: Vec<PathBuf> = fs::read_dir(h.home.join(".claude")).unwrap().flatten().map(|e| e.path())
            .filter(|p| p.to_string_lossy().contains(".wardwell-backup-")).collect();
        assert_eq!(backups.len(), 1);
        let restored: Value = serde_json::from_slice(&fs::read(&backups[0]).unwrap()).unwrap();
        assert_eq!(restored, original);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&backups[0]).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn policy_installs_gate_and_deny_list_and_records_only_what_it_added() {
        let h = home();
        put_settings(&h, json!({"permissions": {"deny": ["Bash(rm:*)", "mcp__linear__save_project"]}}));
        let plan = run(&h, &binding("linear", true));
        assert!(rendered(&plan).contains("Tracker policy, optional: Linear gate, ruleset linear-updates v1"));
        let s = settings(&h);
        assert_eq!(client_hooks::commands(&s, &GATE), vec![format!("'{BIN}' gate linear")]);
        assert_eq!(s["hooks"]["PreToolUse"][0]["matcher"], "mcp__linear__save_comment|mcp__linear__save_issue");
        let denied = client_hooks::denied(&s);
        for tool in LINEAR_UPDATES.denied_tools {
            assert!(denied.contains(&tool.to_string()), "{tool}");
        }
        assert_eq!(denied.iter().filter(|d| *d == "mcp__linear__save_project").count(), 1);
        let (_, record) = manifest::read(&h.cfg).unwrap().unwrap();
        assert_eq!(record.claude_permissions_deny.len(), 6);
        assert!(!record.claude_permissions_deny.contains(&"mcp__linear__save_project".to_string()));
        assert!(plan_for(&h, &binding("linear", true), false).is_noop());
    }

    #[test]
    fn policy_needs_gate_true_on_a_linear_binding() {
        let h = home();
        let plan = run(&h, &binding("linear", false));
        assert!(rendered(&plan).contains("OFF             Tracker policy, optional: off; no linear binding has gate: true"));
        let s = settings(&h);
        assert!(client_hooks::commands(&s, &GATE).is_empty());
        assert!(s.get("permissions").is_none());
    }

    #[test]
    fn turning_the_policy_off_removes_only_what_wardwell_added() {
        let h = home();
        put_settings(&h, json!({"permissions": {"deny": ["mcp__linear__save_project"]}}));
        run(&h, &binding("linear", true));
        let plan = run(&h, &binding("linear", false));
        assert_eq!(action_of(&plan, POLICY), Action::UpdateBackup);
        let s = settings(&h);
        assert!(client_hooks::commands(&s, &GATE).is_empty());
        assert_eq!(client_hooks::denied(&s), vec!["mcp__linear__save_project"]);
        assert!(manifest::read(&h.cfg).unwrap().unwrap().1.claude_permissions_deny.is_empty());
    }

    #[test]
    fn python_gate_is_replaced_only_when_the_policy_is_on() {
        let h = home();
        let python = json!({"hooks": {"PreToolUse": [{"matcher": "mcp__linear__save_comment|mcp__linear__save_issue",
            "hooks": [{"type": "command", "command": "python3 ~/.claude/hooks/linear-gate/linear-gate.py"}]}]}});
        put_settings(&h, python);
        let off = run(&h, &BTreeMap::new());
        assert!(rendered(&off).contains("linear-gate.py) kept"));
        assert!(h.home.join(".claude/settings.json").exists());
        assert_eq!(client_hooks::remove_where(&mut settings(&h), client_hooks::is_python_gate), 1);
        let on = run(&h, &binding("linear", true));
        assert!(rendered(&on).contains("remove the old Python Linear gate hook (linear-gate.py)"));
        let s = settings(&h);
        assert_eq!(client_hooks::remove_where(&mut s.clone(), client_hooks::is_python_gate), 0);
        assert_eq!(s["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn companion_stop_hook_replaces_the_resolve_hook() {
        let h = home();
        put_settings(&h, json!({"hooks": {"Stop": [
            {"hooks": [{"type": "command", "command": "'/w/wardwell' companion lifecycle stop --client claude", "timeout": 3}]},
            {"hooks": [{"type": "command", "command": "/w/wardwell resolve"}]}
        ]}}));
        let plan = run(&h, &BTreeMap::new());
        assert!(rendered(&plan).contains("Companion Stop hook runs the history check"));
        let s = settings(&h);
        assert!(client_hooks::commands(&s, &STOP).is_empty());
        assert_eq!(s["hooks"]["Stop"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn malformed_files_stop_preflight_and_nothing_is_written() {
        let h = home();
        let path = settings_path(&h.home);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{").unwrap();
        let inputs = Inputs { home: &h.home, config_dir: &h.cfg, binary: Path::new(BIN), trackers: &binding("linear", true), claude_code: true, launchd: false };
        assert!(plan(&inputs).unwrap_err().contains("no files changed"));
        fs::write(&path, r#"{"hooks": {"Stop": {}}}"#).unwrap();
        assert!(plan(&inputs).is_err());
        fs::write(&path, "{}").unwrap();
        fs::write(manifest::path(&h.cfg), "not json").unwrap();
        assert!(plan(&inputs).unwrap_err().contains("install record"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "{}");
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    const USER_TEXT: &str = "{\n  \"zeta\": 12345678901234567890123,\n  \"alpha\": 1e3,\n  \"hooks\": {\n    \"SessionStart\": [\n      {\n        \"matcher\": \"startup\",\n        \"hooks\": [\n          {\n            \"type\": \"command\",\n            \"command\": \"peon ping\"\n          }\n        ]\n      }\n    ]\n  }\n}\n";

    #[cfg(unix)]
    #[test]
    fn setup_and_uninstall_keep_what_the_user_wrote() {
        use std::os::unix::fs::PermissionsExt;
        let h = home();
        let path = settings_path(&h.home);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, USER_TEXT).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        run(&h, &binding("linear", true));
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("{\n  \"zeta\": 12345678901234567890123,\n  \"alpha\": 1e3,\n  \"hooks\": {\n    \"SessionStart\": [\n      {\n        \"matcher\": \"startup\",\n        \"hooks\": [\n          {\n            \"type\": \"command\",\n            \"command\": \"peon ping\""), "{text}");
        let s = settings(&h);
        assert_eq!(s["hooks"]["SessionStart"].as_array().unwrap().len(), 2, "{text}");
        assert_eq!(client_hooks::commands(&s, &SESSION_START), vec![format!("'{BIN}' inject \"$(pwd)\"")]);
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o644);
        assert!(plan_for(&h, &binding("linear", true), false).is_noop());
        let plan = uninstall_plan(&h.home, &h.cfg);
        apply(&plan, &Fake::new(&[]), &|| Ok(501)).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), USER_TEXT);
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o644);
    }

    #[test]
    fn wardwell_hooks_under_user_matchers_are_updated_in_place_and_removed_alone() {
        let h = home();
        put_settings(&h, json!({"hooks": {
            "SessionStart": [{"matcher": "startup", "hooks": [{"type": "command", "command": "/old/wardwell inject \"$(pwd)\""}, {"type": "command", "command": "peon"}]}],
            "PreToolUse": [{"matcher": "mcp__linear__.*", "hooks": [{"type": "command", "command": "/old/wardwell gate linear"}, {"type": "command", "command": "audit"}]}]
        }}));
        run(&h, &binding("linear", true));
        let s = settings(&h);
        assert_eq!(client_hooks::commands(&s, &SESSION_START), vec![format!("'{BIN}' inject \"$(pwd)\"")]);
        assert_eq!(client_hooks::commands(&s, &GATE), vec![format!("'{BIN}' gate linear")]);
        assert_eq!(s["hooks"]["SessionStart"].as_array().unwrap().len(), 1, "{s}");
        assert_eq!(s["hooks"]["SessionStart"][0]["matcher"], "startup");
        assert_eq!(s["hooks"]["PreToolUse"].as_array().unwrap().len(), 1, "{s}");
        assert_eq!(s["hooks"]["PreToolUse"][0]["matcher"], "mcp__linear__.*");
        assert!(plan_for(&h, &binding("linear", true), false).is_noop());
        apply(&uninstall_plan(&h.home, &h.cfg), &Fake::new(&[]), &|| Ok(501)).unwrap();
        let s = settings(&h);
        assert!(client_hooks::commands(&s, &SESSION_START).is_empty());
        assert!(client_hooks::commands(&s, &GATE).is_empty());
        assert_eq!(s["hooks"]["SessionStart"], json!([{"matcher": "startup", "hooks": [{"type": "command", "command": "peon"}]}]));
        assert_eq!(s["hooks"]["PreToolUse"], json!([{"matcher": "mcp__linear__.*", "hooks": [{"type": "command", "command": "audit"}]}]));
    }

    #[test]
    fn a_changed_group_never_takes_a_removed_groups_text() {
        let h = home();
        let text = "{\n  \"hooks\": {\n    \"PreToolUse\": [\n      {\n        \"matcher\": \"mcp__linear__save_comment|mcp__linear__save_issue\",\n        \"hooks\": [\n          {\n            \"type\": \"command\",\n            \"command\": \"python3 ~/.claude/hooks/linear-gate/linear-gate.py\"\n          }\n        ]\n      },\n      {\n        \"matcher\": \"mcp__linear__.*\",\n        \"hooks\": [\n          {\n            \"type\": \"command\",\n            \"command\": \"audit\",\n            \"timeout\": 2e0\n          },\n          {\n            \"type\": \"command\",\n            \"command\": \"/old/wardwell gate linear\"\n          }\n        ]\n      }\n    ]\n  }\n}\n";
        let path = settings_path(&h.home);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        run(&h, &binding("linear", true));
        let after = fs::read_to_string(&path).unwrap();
        assert!(after.contains("\"timeout\": 2e0"), "{after}");
        assert!(!after.contains("linear-gate.py"), "{after}");
        assert!(after.contains("\"matcher\": \"mcp__linear__.*\""), "{after}");
    }

    #[test]
    fn an_empty_settings_file_is_an_empty_object() {
        let h = home();
        let path = settings_path(&h.home);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, " \n").unwrap();
        let plan = run(&h, &BTreeMap::new());
        assert_eq!(action_of(&plan, "Session start hook"), Action::UpdateBackup);
        assert_eq!(client_hooks::commands(&settings(&h), &STOP).len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_settings_file_is_refused_and_names_its_target() {
        let h = home();
        let real = h.home.join("dotfiles/settings.json");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::write(&real, "{}").unwrap();
        fs::create_dir_all(h.home.join(".claude")).unwrap();
        std::os::unix::fs::symlink(&real, settings_path(&h.home)).unwrap();
        let inputs = Inputs { home: &h.home, config_dir: &h.cfg, binary: Path::new(BIN), trackers: &BTreeMap::new(), claude_code: true, launchd: false };
        let error = plan(&inputs).unwrap_err();
        assert!(error.contains(&real.display().to_string()), "{error}");
        assert!(error.contains("Run setup against the real file, or replace the link"), "{error}");
        assert!(!uninstall_plan(&h.home, &h.cfg).failures.is_empty());
        assert_eq!(fs::read_to_string(&real).unwrap(), "{}");
    }

    #[test]
    fn settings_that_are_json_but_not_an_object_are_refused() {
        let h = home();
        put_settings(&h, json!([1, 2]));
        let inputs = Inputs { home: &h.home, config_dir: &h.cfg, binary: Path::new(BIN), trackers: &BTreeMap::new(), claude_code: true, launchd: false };
        let error = plan(&inputs).unwrap_err();
        assert!(error.contains("must be a JSON object"), "{error}");
        assert!(error.contains("an array"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn an_unwritable_config_dir_leaves_settings_unchanged() {
        use std::os::unix::fs::PermissionsExt;
        let h = home();
        put_settings(&h, json!({"model": "keep"}));
        let before = fs::read(settings_path(&h.home)).unwrap();
        let plan = plan_for(&h, &binding("linear", true), false);
        fs::set_permissions(&h.cfg, fs::Permissions::from_mode(0o500)).unwrap();
        let failed = apply(&plan, &Fake::new(&[]), &|| Ok(501)).unwrap_err();
        fs::set_permissions(&h.cfg, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(failed.message.contains("install-manifest.json"), "{}", failed.message);
        assert_eq!(fs::read(settings_path(&h.home)).unwrap(), before);
        let leftovers: Vec<_> = fs::read_dir(h.home.join(".claude")).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
        assert!(!manifest::path(&h.cfg).exists());
    }

    #[test]
    fn a_failed_record_rename_restores_settings_and_keeps_the_lines() {
        let h = home();
        put_settings(&h, json!({"model": "keep"}));
        let before = fs::read(settings_path(&h.home)).unwrap();
        let plan = plan_for(&h, &binding("linear", true), false);
        let rename = |from: &Path, to: &Path| match to.ends_with(manifest::FILE) {
            true => Err(std::io::Error::other("disk full")),
            false => fs::rename(from, to),
        };
        let failed = apply_with(&plan, &Fake::new(&[]), &|| Ok(501), &rename).unwrap_err();
        assert!(failed.message.contains("disk full"), "{}", failed.message);
        assert!(failed.message.contains("restored"), "{}", failed.message);
        assert!(failed.lines.iter().any(|l| l.contains("settings.json")), "{:?}", failed.lines);
        assert_eq!(fs::read(settings_path(&h.home)).unwrap(), before);
        assert!(!manifest::path(&h.cfg).exists());
        let temps = fs::read_dir(&h.cfg).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with(".wardwell-install-")).count();
        assert_eq!(temps, 0);
    }

    #[test]
    fn a_concurrent_edit_after_the_preview_writes_nothing() {
        let h = home();
        put_settings(&h, json!({}));
        let plan = plan_for(&h, &binding("linear", true), false);
        fs::write(settings_path(&h.home), "{\"model\": \"new\"}").unwrap();
        assert!(apply(&plan, &Fake::new(&[]), &|| Ok(501)).unwrap_err().message.contains("changed since the preview"));
        assert_eq!(fs::read_to_string(settings_path(&h.home)).unwrap(), "{\"model\": \"new\"}");
        assert!(!manifest::path(&h.cfg).exists());
    }

    #[test]
    fn without_claude_code_only_the_pull_is_planned() {
        let h = home();
        let trackers = binding("linear", true);
        let plan = plan(&Inputs { home: &h.home, config_dir: &h.cfg, binary: Path::new(BIN), trackers: &trackers, claude_code: false, launchd: false }).unwrap();
        assert_eq!(plan.lines.len(), 1);
        assert_eq!(plan.lines[0].action, Action::Manual);
        assert!(plan.lines[0].label.contains("0 * * * * /nonexistent-wardwell-test/bin/wardwell tracker pull"));
    }

    #[test]
    fn removing_every_binding_removes_the_pull_service() {
        let h = home();
        let plist = schedule::plist_path(&h.home);
        fs::create_dir_all(plist.parent().unwrap()).unwrap();
        fs::write(&plist, schedule::launch_agent_plist(Path::new(BIN), 3600, &schedule::log_path(&h.cfg))).unwrap();
        let plan = plan_for(&h, &BTreeMap::new(), true);
        assert_eq!(action_of(&plan, "Tracker pull service"), Action::Delete);
        assert!(rendered(&plan).contains("REMOVE          Tracker pull service: no tracker binding"), "{}", rendered(&plan));
        assert!(!plan.is_noop());
        let fake = Fake::new(&[]);
        apply(&plan, &fake, &|| Ok(501)).unwrap();
        assert_eq!(fake.verbs(), vec!["bootout"]);
        assert!(!plist.exists());
        assert!(plan_for(&h, &BTreeMap::new(), true).is_noop());
    }

    #[test]
    fn no_binding_plans_no_pull() {
        let h = home();
        let plan = plan_for(&h, &BTreeMap::new(), true);
        assert!(!rendered(&plan).contains("pull"));
    }

    #[test]
    fn pull_service_is_planned_from_a_binding_and_installed_through_schedule() {
        let h = home();
        let trackers = binding("linear", false);
        let first = plan_for(&h, &trackers, true);
        assert_eq!(action_of(&first, "Tracker pull service, every 3600s"), Action::Create);
        if !cfg!(target_os = "macos") {
            return;
        }
        let fake = Fake::new(&[]);
        apply(&first, &fake, &|| Ok(501)).unwrap();
        assert_eq!(fake.verbs(), vec!["bootout", "bootstrap", "print"]);
        assert_eq!(schedule::schedule_status(&h.home), Some(3600));
        let second = plan_for(&h, &trackers, true);
        assert_eq!(action_of(&second, "Tracker pull service"), Action::Unchanged);
        assert!(second.is_noop());
        let fake = Fake::new(&[]);
        apply(&second, &fake, &|| Ok(501)).unwrap();
        assert!(fake.calls.borrow().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn the_pull_plan_uses_the_symlink_path_the_hooks_get() {
        let h = home();
        let versioned = h.home.join("Cellar/wardwell/0.12.0/bin");
        fs::create_dir_all(&versioned).unwrap();
        fs::write(versioned.join("wardwell"), "").unwrap();
        fs::create_dir_all(h.home.join("bin")).unwrap();
        let link = h.home.join("bin/wardwell");
        std::os::unix::fs::symlink(versioned.join("wardwell"), &link).unwrap();
        let plist = schedule::plist_path(&h.home);
        fs::create_dir_all(plist.parent().unwrap()).unwrap();
        fs::write(&plist, schedule::launch_agent_plist(&link, 3600, &schedule::log_path(&h.cfg))).unwrap();
        let trackers = binding("linear", false);
        let plan = plan(&Inputs { home: &h.home, config_dir: &h.cfg, binary: &link, trackers: &trackers, claude_code: false, launchd: true }).unwrap();
        assert_eq!(action_of(&plan, "Tracker pull service"), Action::Unchanged, "{}", rendered(&plan));
    }

    #[test]
    fn an_existing_pull_interval_is_kept_and_a_moved_binary_updates_it() {
        let h = home();
        let plist = schedule::plist_path(&h.home);
        fs::create_dir_all(plist.parent().unwrap()).unwrap();
        fs::write(&plist, schedule::launch_agent_plist(Path::new("/old/wardwell"), 900, &schedule::log_path(&h.cfg))).unwrap();
        let plan = plan_for(&h, &binding("linear", false), true);
        assert_eq!(action_of(&plan, "Tracker pull service, every 900s"), Action::Update);
    }

    #[test]
    fn uninstall_removes_only_wardwell_entries_and_keeps_the_folder() {
        let h = home();
        put_settings(&h, json!({"model": "keep", "permissions": {"deny": ["mcp__linear__save_project", "Bash(rm:*)"]},
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "rtk check"}]}],
                      "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "'/w/wardwell' companion lifecycle begin --client claude"}]}]}}));
        run(&h, &binding("linear", true));
        let plist = schedule::plist_path(&h.home);
        fs::create_dir_all(plist.parent().unwrap()).unwrap();
        fs::write(&plist, "x").unwrap();
        let plan = uninstall_plan(&h.home, &h.cfg);
        assert_eq!(action_of(&plan, "Linear gate"), Action::Remove);
        let fake = Fake::new(&[]);
        apply(&plan, &fake, &|| Ok(501)).unwrap();
        assert_eq!(fake.verbs(), vec!["bootout"]);
        assert!(!plist.exists());
        let s = settings(&h);
        assert_eq!(s["model"], "keep");
        assert_eq!(client_hooks::denied(&s), vec!["mcp__linear__save_project", "Bash(rm:*)"]);
        assert_eq!(s["hooks"], json!({"Stop": [{"hooks": [{"type": "command", "command": "rtk check"}]}],
            "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "'/w/wardwell' companion lifecycle begin --client claude"}]}]}));
        assert!(h.cfg.is_dir());
        assert!(manifest::read(&h.cfg).unwrap().unwrap().1.claude_permissions_deny.is_empty());
        let again = uninstall_plan(&h.home, &h.cfg);
        assert!(again.is_noop(), "{}", rendered(&again));
    }

    #[test]
    fn uninstall_leaves_the_companion_install_alone() {
        let h = home();
        let companion = json!({"hooks": {
            "SessionStart": [{"matcher": "resume", "hooks": [{"type": "command", "command": "'/w/wardwell' companion lifecycle resume --client claude", "timeout": 6}]}],
            "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "'/w/wardwell' companion lifecycle begin --client claude", "timeout": 3}]}],
            "Stop": [{"hooks": [{"type": "command", "command": "'/w/wardwell' companion lifecycle stop --client claude", "timeout": 3}]}]}});
        put_settings(&h, companion.clone());
        let others = [
            (".codex/hooks.json", r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"'/w/wardwell' companion lifecycle stop --client codex"}]}]}}"#),
            (".claude/skills/wardwell-companion/SKILL.md", "---\nname: wardwell-companion\n---\n"),
            (".codex/skills/wardwell-companion/SKILL.md", "---\nname: wardwell-companion\n---\n"),
            (".claude/commands/companion.md", "command"),
            (".claude/CLAUDE.md", "<!-- wardwell-companion:start -->\nx\n<!-- wardwell-companion:end -->\n"),
            (".codex/AGENTS.md", "<!-- wardwell-companion:start -->\nx\n<!-- wardwell-companion:end -->\n"),
        ];
        for (relative, text) in others {
            fs::create_dir_all(h.home.join(relative).parent().unwrap()).unwrap();
            fs::write(h.home.join(relative), text).unwrap();
        }
        let plan = uninstall_plan(&h.home, &h.cfg);
        assert!(rendered(&plan).contains("Companion install was not removed"), "{}", rendered(&plan));
        apply(&plan, &Fake::new(&[]), &|| Ok(501)).unwrap();
        assert_eq!(settings(&h), companion);
        for (relative, text) in others {
            assert_eq!(fs::read_to_string(h.home.join(relative)).unwrap(), text, "{relative}");
        }
    }

    #[test]
    fn uninstall_removes_an_empty_hooks_object_only_when_setup_created_it() {
        let h = home();
        put_settings(&h, json!({"model": "keep"}));
        run(&h, &binding("linear", true));
        apply(&uninstall_plan(&h.home, &h.cfg), &Fake::new(&[]), &|| Ok(501)).unwrap();
        assert_eq!(settings(&h), json!({"model": "keep"}));
        let h = home();
        put_settings(&h, json!({"model": "keep", "hooks": {}, "permissions": {"deny": []}}));
        run(&h, &binding("linear", true));
        apply(&uninstall_plan(&h.home, &h.cfg), &Fake::new(&[]), &|| Ok(501)).unwrap();
        assert_eq!(settings(&h), json!({"model": "keep", "hooks": {}, "permissions": {"deny": []}}));
    }

    #[test]
    fn the_record_never_lists_an_entry_twice() {
        let h = home();
        put_settings(&h, json!({"permissions": {"deny": ["mcp__linear__delete_comment"]}}));
        fs::write(manifest::path(&h.cfg), r#"{"version": 1, "claude_permissions_deny": ["mcp__linear__delete_comment", "mcp__linear__delete_comment"]}"#).unwrap();
        run(&h, &binding("linear", true));
        let record = manifest::read(&h.cfg).unwrap().unwrap().1;
        let mut unique = record.claude_permissions_deny.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), record.claude_permissions_deny.len(), "{record:?}");
        assert_eq!(record.claude_permissions_deny.len(), 7);
    }

    #[test]
    fn a_hand_edited_plist_is_backed_up_before_it_is_replaced() {
        let h = home();
        let plist = schedule::plist_path(&h.home);
        fs::create_dir_all(plist.parent().unwrap()).unwrap();
        let generated_elsewhere = schedule::launch_agent_plist(Path::new("/old/wardwell"), 3600, &schedule::log_path(&h.cfg));
        fs::write(&plist, &generated_elsewhere).unwrap();
        assert_eq!(action_of(&plan_for(&h, &binding("linear", false), true), "Tracker pull service"), Action::Update);
        let edited = generated_elsewhere.replace("<key>RunAtLoad</key>", "<key>Nice</key><integer>5</integer><key>RunAtLoad</key>");
        fs::write(&plist, &edited).unwrap();
        let plan = plan_for(&h, &binding("linear", false), true);
        assert_eq!(action_of(&plan, "Tracker pull service"), Action::UpdateBackup);
        if !cfg!(target_os = "macos") {
            return;
        }
        let lines = apply(&plan, &Fake::new(&[]), &|| Ok(501)).unwrap();
        let backup = lines.iter().find_map(|l| l.trim().strip_prefix("backup: ")).unwrap();
        assert_eq!(fs::read_to_string(backup).unwrap(), edited);
    }

    #[test]
    fn uninstall_on_a_clean_home_writes_nothing() {
        let h = home();
        let plan = uninstall_plan(&h.home, &h.cfg);
        assert!(plan.is_noop());
        assert!(plan.lines.iter().all(|l| l.action == Action::Unchanged));
        apply(&plan, &Fake::new(&[]), &|| Ok(501)).unwrap();
        assert!(!h.home.join(".claude").exists());
        assert!(!manifest::path(&h.cfg).exists());
    }
}
