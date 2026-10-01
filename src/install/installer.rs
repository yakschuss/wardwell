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
}

#[derive(Debug)]
enum Pull {
    Keep,
    Manual,
    Install { home: PathBuf, config_dir: PathBuf, binary: PathBuf, interval: u32 },
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
}

impl Plan {
    /// True when applying would change nothing.
    pub fn is_noop(&self) -> bool {
        self.changes.is_empty() && matches!(self.pull, Pull::Keep | Pull::Manual)
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
    original: Value,
    value: Value,
    removing: bool,
}

impl Draft {
    fn read(path: PathBuf, removing: bool) -> Result<Draft, String> {
        let before = read_optional(&path)?;
        let value = match before.as_deref() {
            Some(bytes) => serde_json::from_slice(bytes)
                .map_err(|_| format!("Malformed JSON in {}; no files changed", path.display()))?,
            None => json!({}),
        };
        client_hooks::validate(&value).map_err(|error| format!("{error} in {}; no files changed", path.display()))?;
        Ok(Draft { path, before, original: value.clone(), value, removing })
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
        let mut after = serde_json::to_vec_pretty(&self.value).map_err(|_| "Could not encode Claude settings")?;
        after.push(b'\n');
        Ok(Some(Change { path: self.path, before: self.before, after }))
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
        memory_steps(&mut draft, &mut lines, &quoted)?;
        policy_steps(&mut draft, &mut lines, &quoted, policy, &mut record)?;
        changes.extend(draft.into_change()?);
        let next = Manifest { claude_permissions_deny: record, ..Manifest::default() };
        changes.extend(manifest_change(inputs.config_dir, recorded, &next, &mut lines)?);
    }
    let pull = pull_step(inputs, &mut lines);
    Ok(Plan { lines, changes, pull, policy: policy && inputs.claude_code, settings: inputs.claude_code })
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

fn manifest_change(config_dir: &Path, recorded: Option<(Vec<u8>, Manifest)>, next: &Manifest, lines: &mut Vec<Line>) -> Result<Option<Change>, String> {
    let path = manifest::path(config_dir);
    let label = "Install record (deny entries Wardwell added)".to_string();
    let (action, before) = match recorded {
        Some((_, old)) if old == *next => (Action::Unchanged, None),
        Some((bytes, _)) => (Action::UpdateBackup, Some(bytes)),
        None if next.claude_permissions_deny.is_empty() => return Ok(None),
        None => (Action::Create, None),
    };
    lines.push(Line { action, label, path: Some(path.clone()) });
    if action == Action::Unchanged {
        return Ok(None);
    }
    Ok(Some(Change { path, before, after: manifest::encode(next)? }))
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
        Ok(_) => Action::Update,
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
        },
    }
}

/// Preview `uninstall`: Wardwell's hooks in Claude settings, the deny entries
/// the install record lists and only those, and the pull service. The
/// Wardwell folder is never removed; the install record is emptied, not deleted.
pub fn uninstall_plan(home: &Path, config_dir: &Path) -> Result<Plan, String> {
    let mut lines = Vec::new();
    let mut changes = Vec::new();
    let recorded = manifest::read(config_dir)?;
    let mut draft = Draft::read(settings_path(home), true)?;
    let denies = recorded.as_ref().map(|(_, m)| m.claude_permissions_deny.clone()).unwrap_or_default();
    for (label, spec) in [("Session start hook", &SESSION_START), ("Stop hook", &STOP), ("Linear gate", &GATE)] {
        draft.step(&mut lines, label.into(), |v| {
            client_hooks::remove(v, spec);
            Ok(())
        })?;
    }
    draft.step(&mut lines, "Companion lifecycle hooks".into(), |v| {
        client_hooks::remove_where(v, client_hooks::is_companion_lifecycle);
        Ok(())
    })?;
    draft.step(&mut lines, format!("Deny entries Wardwell added ({})", denies.len()), |v| {
        client_hooks::remove_denies(v, &denies);
        Ok(())
    })?;
    changes.extend(draft.into_change()?);
    changes.extend(manifest_change(config_dir, recorded, &Manifest::default(), &mut lines)?);
    let plist = schedule::plist_path(home);
    let pull = match plist.exists() {
        true => {
            lines.push(Line { action: Action::Delete, label: "Tracker pull service".into(), path: Some(plist) });
            Pull::Remove { home: home.to_path_buf() }
        }
        false => Pull::Keep,
    };
    Ok(Plan { lines, changes, pull, policy: false, settings: false })
}

/// Write the plan. Every file is re-checked against the preview first, so a
/// concurrent edit stops the run before anything is written.
pub fn apply(plan: &Plan, runner: &dyn LaunchctlRunner, uid: &dyn Fn() -> Result<u32, String>) -> Result<Vec<String>, String> {
    for change in &plan.changes {
        if read_optional(&change.path)? != change.before {
            return Err(format!("{} changed since the preview; nothing was written. Run the command again.", change.path.display()));
        }
    }
    let mut report = Vec::new();
    for change in &plan.changes {
        if read_optional(&change.path)? != change.before {
            return Err(format!(
                "{} changed during installation. Earlier files have .wardwell-backup files for rollback; rerun after review.",
                change.path.display()
            ));
        }
        let backup = change.before.as_ref().map(|bytes| backup_file(&change.path, bytes)).transpose()?;
        atomic_write(&change.path, &change.after)
            .map_err(|error| format!("{error}; any earlier changed files have .wardwell-backup files for rollback"))?;
        report.push(format!("  OK {}", change.path.display()));
        if let Some(backup) = backup {
            report.push(format!("    backup: {}", backup.display()));
        }
    }
    match &plan.pull {
        Pull::Install { home, config_dir, binary, interval } => {
            report.push(format!("  OK {}", schedule::schedule(home, config_dir, *interval, runner, binary, uid()?)?));
        }
        Pull::Remove { home } => report.push(format!("  OK {}", schedule::unschedule(home, runner, uid()?)?)),
        Pull::Keep | Pull::Manual => {}
    }
    Ok(report)
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
        assert!(!manifest::path(&h.cfg).exists());
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

    #[test]
    fn a_concurrent_edit_after_the_preview_writes_nothing() {
        let h = home();
        put_settings(&h, json!({}));
        let plan = plan_for(&h, &binding("linear", true), false);
        fs::write(settings_path(&h.home), "{\"model\": \"new\"}").unwrap();
        assert!(apply(&plan, &Fake::new(&[]), &|| Ok(501)).unwrap_err().contains("changed since the preview"));
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
        let plan = uninstall_plan(&h.home, &h.cfg).unwrap();
        assert_eq!(action_of(&plan, "Linear gate"), Action::Remove);
        let fake = Fake::new(&[]);
        apply(&plan, &fake, &|| Ok(501)).unwrap();
        assert_eq!(fake.verbs(), vec!["bootout"]);
        assert!(!plist.exists());
        let s = settings(&h);
        assert_eq!(s["model"], "keep");
        assert_eq!(client_hooks::denied(&s), vec!["mcp__linear__save_project", "Bash(rm:*)"]);
        assert_eq!(s["hooks"], json!({"Stop": [{"hooks": [{"type": "command", "command": "rtk check"}]}]}));
        assert!(h.cfg.is_dir());
        assert!(manifest::read(&h.cfg).unwrap().unwrap().1.claude_permissions_deny.is_empty());
        let again = uninstall_plan(&h.home, &h.cfg).unwrap();
        assert!(again.is_noop(), "{}", rendered(&again));
    }

    #[test]
    fn uninstall_on_a_clean_home_writes_nothing() {
        let h = home();
        let plan = uninstall_plan(&h.home, &h.cfg).unwrap();
        assert!(plan.is_noop());
        assert!(plan.lines.iter().all(|l| l.action == Action::Unchanged));
        apply(&plan, &Fake::new(&[]), &|| Ok(501)).unwrap();
        assert!(!h.home.join(".claude").exists());
        assert!(!manifest::path(&h.cfg).exists());
    }
}
