//! `wardwell project link`, `wardwell project list` and `wardwell project
//! kanban`: record which directories belong to a vault project, and whether
//! its kanban is on, in config.yml, with a backup, idempotently. `link`
//! previews first. A linked worktree records its main checkout.
//!
//! Does NOT create vault folders, resolve sessions (inject/resolve.rs does),
//! or touch any client settings.

use crate::companion::install::{atomic_write, backup_file, read_optional};
use crate::config::edit::{add_project_path, set_project_kanban};
use crate::config::loader::{WardwellConfig, parse};
use crate::inject::git::{GitDirs, canonical};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What the user asked `link` to do.
pub struct LinkRequest<'a> {
    /// `<domain>/<project>`, or None to infer it from the directory name.
    pub key: Option<&'a str>,
    /// The directory to link, absolute.
    pub dir: &'a Path,
    pub dry_run: bool,
    pub yes: bool,
}

/// How long `link` waits for another link to release config.yml.
pub const LOCK_WAIT: Duration = Duration::from_secs(10);

/// Plan, preview, confirm and apply one link in `config_dir/config.yml`.
/// `confirm` is asked once, only when a write is planned without `yes`.
pub fn link(config_dir: &Path, request: &LinkRequest, git: impl Fn(&Path) -> Option<GitDirs>, confirm: impl FnOnce() -> bool, out: &mut dyn Write) -> Result<(), String> {
    link_waiting(config_dir, request, git, confirm, out, LOCK_WAIT)
}

/// `link` holding `config.yml.lock` from the read to the rename. The lock is
/// released while the question waits for an answer; the write then checks
/// that config.yml did not change in between.
fn link_waiting(config_dir: &Path, request: &LinkRequest, git: impl Fn(&Path) -> Option<GitDirs>, confirm: impl FnOnce() -> bool, out: &mut dyn Write, wait: Duration) -> Result<(), String> {
    let path = config_dir.join("config.yml");
    // A dry run writes nothing, so it takes no lock and needs no writable folder.
    let mut lock = if request.dry_run { None } else { Some(Lock::take(&path, wait)?) };
    let (before, config) = read_config(&path)?;
    let dir = recorded_dir(request.dir, git)?;
    let key = match request.key {
        Some(key) => key.to_string(),
        None => infer_key(&config, &dir)?,
    };
    require_vault_folder(&config, &key)?;
    let say = |out: &mut dyn Write, line: String| writeln!(out, "{line}").map_err(|e| e.to_string());
    say(out, format!("wardwell project link\n\n  Link {} to {key}.\n  Proposed changes:", dir.display()))?;
    if let Some(through) = covering_path(&config, &key, &dir)? {
        say(out, format!("    {:<15} {}", "UNCHANGED", path.display()))?;
        return say(out, format!("\n  Already linked through {}. Nothing changed.", through.display()));
    }
    let after = add_project_path(&before, &key, &dir)?;
    say(out, format!("    {:<15} {}", "UPDATE + BACKUP", path.display()))?;
    if request.dry_run {
        return say(out, "\n  Dry run complete. Nothing changed.".into());
    }
    if !request.yes {
        drop(lock);
        if !confirm() {
            return say(out, "\n  Cancelled. Nothing changed.".into());
        }
        lock = Some(Lock::take(&path, wait)?);
    }
    let backup = write(&path, &before, &after)?;
    drop(lock);
    say(out, format!("\n  OK linked.\n    backup: {}", backup.display()))?;
    say(out, format!("\n  New sessions started in {} or its worktrees load {key}.\n  Sessions already running do not change.", dir.display()))
}

/// Set the kanban switch of `key` in `config_dir/config.yml` and say the
/// resulting state. Writes a backup first; a setting already in place writes
/// nothing. Holds the config lock from the read to the rename.
pub fn kanban(config_dir: &Path, key: &str, on: bool, out: &mut dyn Write) -> Result<(), String> {
    let path = config_dir.join("config.yml");
    let _lock = Lock::take(&path, LOCK_WAIT)?;
    let (before, config) = read_config(&path)?;
    require_vault_folder(&config, key)?;
    let say = |out: &mut dyn Write, line: String| writeln!(out, "{line}").map_err(|e| e.to_string());
    say(out, format!("wardwell project kanban\n\n  Set the kanban of {key} {}.", if on { "on" } else { "off" }))?;
    if config.projects.get(key).and_then(|m| m.kanban) == Some(on) {
        say(out, format!("    {:<15} {}", "UNCHANGED", path.display()))?;
        return say(out, format!("\n  Already set. Nothing changed.\n  {key} {}", crate::install::doctor::kanban_words(&config, key)));
    }
    let after = set_project_kanban(&before, key, on)?;
    let backup = write(&path, &before, &after)?;
    say(out, format!("    {:<15} {}", "UPDATE + BACKUP", path.display()))?;
    let state = parse(&after).map_err(|e| e.to_string())?;
    say(out, format!("\n  OK set.\n    backup: {}\n  {key} {}", backup.display(), crate::install::doctor::kanban_words(&state, key)))?;
    say(out, "\n  Old kanban items stay in kanban.db untouched. Sessions already running do not change.".to_string())
}

/// `config.yml.lock` beside config.yml, created exclusively and removed when
/// dropped, on every exit path.
struct Lock(PathBuf);

impl Lock {
    fn take(config: &Path, wait: Duration) -> Result<Self, String> {
        let path = config.with_file_name("config.yml.lock");
        let started = Instant::now();
        loop {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && started.elapsed() < wait => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(format!("Another link holds {}. Nothing changed. If no link is running, delete that file and run again.", path.display()));
                }
                Err(_) => return Err(format!("Could not create {}. Nothing changed.", path.display())),
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Every mapping in `config_dir/config.yml`, one project per block.
pub fn list(config_dir: &Path, out: &mut dyn Write) -> Result<(), String> {
    let (_, config) = read_config(&config_dir.join("config.yml"))?;
    let mut text = String::from("wardwell project list\n\n");
    if config.projects.is_empty() {
        text.push_str("  No directories are linked. Run `wardwell project link` in a repository.\n");
    }
    for (key, mapping) in &config.projects {
        text.push_str(&format!("  {key}\n"));
        mapping.paths.iter().for_each(|p| text.push_str(&format!("    {}\n", p.display())));
    }
    out.write_all(text.as_bytes()).map_err(|e| e.to_string())
}

fn read_config(path: &Path) -> Result<(String, WardwellConfig), String> {
    let bytes = read_optional(path)?.ok_or_else(|| format!("No config at {}. Run `wardwell init` first.", path.display()))?;
    let text = String::from_utf8(bytes).map_err(|_| "config.yml is not UTF-8".to_string())?;
    let config = parse(&text).map_err(|e| format!("config.yml does not parse: {e}"))?;
    Ok((text, config))
}

/// The directory as it will be recorded: canonical. A linked worktree is
/// refused, naming its main checkout: worktrees resolve through it, so a
/// worktree's own path would never be used.
fn recorded_dir(dir: &Path, git: impl Fn(&Path) -> Option<GitDirs>) -> Result<PathBuf, String> {
    if !dir.is_dir() {
        return Err(format!("{} is not a directory; nothing changed.", dir.display()));
    }
    let dir = canonical(dir);
    match git(&dir).and_then(|g| g.in_main_worktree(&dir)).filter(|main| *main != dir) {
        Some(main) => Err(format!("{} is a linked worktree. Link its main checkout {} instead; worktrees resolve through it. Nothing changed.", dir.display(), main.display())),
        None => Ok(dir),
    }
}

/// The one vault project named like `dir`, or what to type instead.
fn infer_key(config: &WardwellConfig, dir: &Path) -> Result<String, String> {
    let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let found: Vec<String> = subfolders(&config.vault_path)
        .into_iter()
        .filter(|domain| domain.join(name).is_dir())
        .filter_map(|domain| Some(format!("{}/{name}", domain.file_name()?.to_str()?)))
        .collect();
    match found.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(format!("No vault project is named {name}. Name it: `wardwell project link <domain>/<project>`. Nothing changed.")),
        many => Err(format!("Several vault projects are named {name}: {}. Name one: `wardwell project link <domain>/<project>`. Nothing changed.", many.join(", "))),
    }
}

fn subfolders(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default();
    found.retain(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| !n.starts_with('.') && !n.starts_with('_')));
    found.sort();
    found
}

fn require_vault_folder(config: &WardwellConfig, key: &str) -> Result<(), String> {
    let (domain, project) = crate::config::loader::project_key_parts(key).ok_or_else(|| format!("{key} is not <domain>/<project>; nothing changed."))?;
    let folder = config.vault_path.join(domain).join(project);
    if !folder.is_dir() {
        return Err(format!("{key} has no folder in the vault at {}. Create it with `wardwell seed {key}`, then link again. Nothing changed.", folder.display()));
    }
    Ok(())
}

/// The path of `key` that already contains `dir`. Err when another project
/// lists exactly `dir`.
fn covering_path(config: &WardwellConfig, key: &str, dir: &Path) -> Result<Option<PathBuf>, String> {
    if let Some((other, _)) = config.projects.iter().find(|(k, m)| *k != key && m.paths.iter().any(|p| canonical(p) == dir)) {
        return Err(format!("{} is already linked to {other}. Remove it there first. Nothing changed.", dir.display()));
    }
    let own = config.projects.get(key).map(|m| m.paths.as_slice()).unwrap_or_default();
    Ok(own.iter().find(|p| dir.starts_with(canonical(p))).cloned())
}

/// Back up `before` beside `path`, then replace `path` with `after`,
/// keeping its permissions. Refuses when the file changed since it was read.
fn write(path: &Path, before: &str, after: &str) -> Result<PathBuf, String> {
    if read_optional(path)?.as_deref() != Some(before.as_bytes()) {
        return Err("config.yml changed while the plan was made. Nothing changed; run the command again.".into());
    }
    let mode = std::fs::metadata(path).map(|m| m.permissions()).map_err(|_| "Could not inspect config.yml")?;
    let backup = backup_file(path, before.as_bytes())?;
    atomic_write(path, after.as_bytes()).map_err(|e| format!("{e}; the backup is at {}", backup.display()))?;
    std::fs::set_permissions(path, mode).map_err(|_| "Could not restore the permissions of config.yml")?;
    Ok(backup)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::inject::git::testing::{git as run_git, repo};

    const HEAD: &str = "# my config\nvault_path: VAULT # keep\nsession_sources: []\n";

    struct Fixture {
        _tmp: tempfile::TempDir,
        cfg: PathBuf,
        code: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("cfg");
        let vault = tmp.path().join("vault");
        std::fs::create_dir_all(vault.join("personal/corrtex")).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("config.yml"), HEAD.replace("VAULT", &vault.display().to_string())).unwrap();
        let code = canonical(tmp.path()).join("code/corrtex");
        repo(&code);
        Fixture { _tmp: tmp, cfg, code }
    }

    fn run(f: &Fixture, key: Option<&str>, dir: &Path, dry_run: bool, answer: bool) -> Result<String, String> {
        let mut out = Vec::new();
        let request = LinkRequest { key, dir, dry_run, yes: false };
        link(&f.cfg, &request, crate::inject::git::dirs, || answer, &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    fn config_text(f: &Fixture) -> String {
        std::fs::read_to_string(f.cfg.join("config.yml")).unwrap()
    }

    fn backups(f: &Fixture) -> Vec<PathBuf> {
        std::fs::read_dir(&f.cfg).unwrap().flatten().map(|e| e.path()).filter(|p| p.to_string_lossy().contains(".wardwell-backup-")).collect()
    }

    #[test]
    fn link_previews_writes_with_a_private_backup_and_a_second_run_changes_nothing() {
        let f = fixture();
        let original = config_text(&f);
        let out = run(&f, Some("personal/corrtex"), &f.code, false, true).unwrap();
        assert!(out.contains(&format!("UPDATE + BACKUP {}", f.cfg.join("config.yml").display())), "{out}");
        assert!(out.contains("Sessions already running do not change."), "{out}");
        let text = config_text(&f);
        assert!(text.starts_with(&original), "every existing byte kept: {text}");
        assert!(text.ends_with(&format!("- \"{}\"\n", f.code.display())), "no trailing slash: {text}");
        let config = parse(&text).unwrap();
        assert_eq!(config.projects["personal/corrtex"].paths, vec![f.code.clone()]);
        let saved = backups(&f);
        assert_eq!(saved.len(), 1);
        assert_eq!(std::fs::read_to_string(&saved[0]).unwrap(), original);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&saved[0]).unwrap().permissions().mode() & 0o777, 0o600);
        }

        let again = run(&f, Some("personal/corrtex"), &f.code, false, true).unwrap();
        assert!(again.contains("UNCHANGED") && again.contains("Nothing changed."), "{again}");
        assert_eq!(config_text(&f), text);
        assert_eq!(backups(&f).len(), 1);
    }

    #[test]
    fn dry_run_and_a_declined_prompt_write_nothing() {
        let f = fixture();
        let original = config_text(&f);
        let out = run(&f, Some("personal/corrtex"), &f.code, true, true).unwrap();
        assert!(out.contains("UPDATE + BACKUP") && out.contains("Dry run complete. Nothing changed."), "{out}");
        let out = run(&f, Some("personal/corrtex"), &f.code, false, false).unwrap();
        assert!(out.contains("Cancelled. Nothing changed."), "{out}");
        assert_eq!(config_text(&f), original);
        assert!(backups(&f).is_empty());
    }

    #[test]
    fn a_project_missing_from_the_vault_is_refused() {
        let f = fixture();
        let error = run(&f, Some("personal/nope"), &f.code, false, true).unwrap_err();
        assert!(error.contains("has no folder in the vault") && error.contains("wardwell seed personal/nope"), "{error}");
        assert!(backups(&f).is_empty());
    }

    #[test]
    fn a_linked_worktree_is_refused_naming_the_main_checkout() {
        let f = fixture();
        let linked = f.code.parent().unwrap().join("corrtex-wt");
        run_git(&f.code, &["worktree", "add", "-q", "-b", "wt", linked.to_str().unwrap()]);
        let error = run(&f, Some("personal/corrtex"), &linked, false, true).unwrap_err();
        assert!(error.contains("is a linked worktree") && error.contains(&format!("Link its main checkout {} instead", f.code.display())), "{error}");
        assert!(parse(&config_text(&f)).unwrap().projects.is_empty());
        assert!(backups(&f).is_empty());
    }

    #[test]
    fn without_a_key_the_directory_name_picks_the_one_vault_project() {
        let f = fixture();
        run(&f, None, &f.code, false, true).unwrap();
        assert!(parse(&config_text(&f)).unwrap().projects.contains_key("personal/corrtex"));
        let other = f.code.parent().unwrap().join("scratch");
        std::fs::create_dir_all(&other).unwrap();
        let error = run(&f, None, &other, false, true).unwrap_err();
        assert!(error.contains("No vault project is named scratch"), "{error}");
    }

    #[test]
    fn a_directory_linked_to_another_project_is_refused() {
        let f = fixture();
        run(&f, Some("personal/corrtex"), &f.code, false, true).unwrap();
        let vault = parse(&config_text(&f)).unwrap().vault_path;
        std::fs::create_dir_all(vault.join("work/other")).unwrap();
        let error = run(&f, Some("work/other"), &f.code, false, true).unwrap_err();
        assert!(error.contains("already linked to personal/corrtex"), "{error}");
    }

    #[test]
    fn list_shows_each_project_and_its_paths() {
        let f = fixture();
        let mut out = Vec::new();
        list(&f.cfg, &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("No directories are linked."));
        run(&f, Some("personal/corrtex"), &f.code, false, true).unwrap();
        let mut out = Vec::new();
        list(&f.cfg, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), format!("wardwell project list\n\n  personal/corrtex\n    {}\n", f.code.display()));
    }

    #[test]
    fn eight_parallel_links_of_eight_paths_keep_all_eight_and_leave_no_lock() {
        let f = fixture();
        let root = f.code.parent().unwrap().to_path_buf();
        let dirs: Vec<PathBuf> = (0..8).map(|i| root.join(format!("c{i}"))).collect();
        dirs.iter().for_each(|d| std::fs::create_dir_all(d).unwrap());
        std::thread::scope(|scope| {
            for dir in &dirs {
                let cfg = f.cfg.clone();
                scope.spawn(move || {
                    let request = LinkRequest { key: Some("personal/corrtex"), dir, dry_run: false, yes: true };
                    link(&cfg, &request, |_: &Path| None, || true, &mut Vec::new()).unwrap();
                });
            }
        });
        let paths = &parse(&config_text(&f)).unwrap().projects["personal/corrtex"].paths;
        assert_eq!(paths.len(), 8, "{paths:?}");
        assert!(!f.cfg.join("config.yml.lock").exists());
    }

    #[test]
    fn a_held_lock_times_out_with_a_clear_message_and_is_not_removed() {
        let f = fixture();
        std::fs::write(f.cfg.join("config.yml.lock"), "").unwrap();
        let error = link_with_wait(&f.cfg, &LinkRequest { key: Some("personal/corrtex"), dir: &f.code, dry_run: false, yes: true }, std::time::Duration::from_millis(100)).unwrap_err();
        assert!(error.contains("config.yml.lock") && error.contains("Nothing changed"), "{error}");
        assert!(f.cfg.join("config.yml.lock").exists(), "another process's lock is left alone");
        assert_eq!(parse(&config_text(&f)).unwrap().projects.len(), 0);
    }

    #[test]
    fn every_exit_path_removes_the_lock() {
        let f = fixture();
        let lock = f.cfg.join("config.yml.lock");
        run(&f, Some("personal/corrtex"), &f.code, true, true).unwrap();
        assert!(!lock.exists(), "dry run");
        run(&f, Some("personal/corrtex"), &f.code, false, false).unwrap();
        assert!(!lock.exists(), "cancelled");
        run(&f, Some("personal/nope"), &f.code, false, true).unwrap_err();
        assert!(!lock.exists(), "refused");
        run(&f, Some("personal/corrtex"), &f.code, false, true).unwrap();
        run(&f, Some("personal/corrtex"), &f.code, false, true).unwrap();
        assert!(!lock.exists(), "written and unchanged");
    }

    fn link_with_wait(cfg: &Path, request: &LinkRequest, wait: std::time::Duration) -> Result<(), String> {
        link_waiting(cfg, request, |_: &Path| None, || true, &mut Vec::new(), wait)
    }

    #[test]
    fn a_key_with_dot_segments_is_refused() {
        let f = fixture();
        for key in ["personal/..", "../personal", "personal/corrtex/"] {
            let error = run(&f, Some(key), &f.code, false, true).unwrap_err();
            assert!(error.contains("is not <domain>/<project>"), "{key}: {error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn link_keeps_the_permissions_of_config_yml() {
        use std::os::unix::fs::PermissionsExt;
        for mode in [0o644, 0o640] {
            let f = fixture();
            let path = f.cfg.join("config.yml");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            run(&f, Some("personal/corrtex"), &f.code, false, true).unwrap();
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, mode);
        }
    }

    #[test]
    fn a_config_changed_after_the_plan_is_not_overwritten() {
        let f = fixture();
        let path = f.cfg.join("config.yml");
        let planned = config_text(&f);
        std::fs::write(&path, format!("{planned}# edited by hand\n")).unwrap();
        let error = write(&path, &planned, "vault_path: /x\n").unwrap_err();
        assert!(error.contains("config.yml changed while the plan was made"), "{error}");
        assert!(config_text(&f).ends_with("# edited by hand\n"));
        assert!(backups(&f).is_empty());
    }

    #[test]
    fn several_vault_projects_with_the_directory_name_need_a_key() {
        let f = fixture();
        let vault = parse(&config_text(&f)).unwrap().vault_path;
        std::fs::create_dir_all(vault.join("work/corrtex")).unwrap();
        let error = run(&f, None, &f.code, false, true).unwrap_err();
        assert!(error.contains("Several vault projects are named corrtex: personal/corrtex, work/corrtex"), "{error}");
        assert!(backups(&f).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn dry_run_needs_no_lock_and_no_writable_config_dir() {
        use std::os::unix::fs::PermissionsExt;
        let f = fixture();
        std::fs::set_permissions(&f.cfg, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = run(&f, Some("personal/corrtex"), &f.code, true, true);
        std::fs::set_permissions(&f.cfg, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = result.unwrap();
        assert!(out.contains("UPDATE + BACKUP") && out.contains("Dry run complete. Nothing changed."), "{out}");
    }

    fn kanban_run(f: &Fixture, key: &str, on: bool) -> Result<String, String> {
        let mut out = Vec::new();
        kanban(&f.cfg, key, on, &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    fn enable_board(f: &Fixture) {
        let text = format!("{}kanban:\n  enabled: true\n", config_text(f));
        std::fs::write(f.cfg.join("config.yml"), text).unwrap();
    }

    #[test]
    fn kanban_off_writes_the_setting_with_a_backup_prints_the_state_and_a_second_run_changes_nothing() {
        let f = fixture();
        enable_board(&f);
        let original = config_text(&f);
        let out = kanban_run(&f, "personal/corrtex", false).unwrap();
        assert!(out.contains("UPDATE + BACKUP") && out.contains("personal/corrtex kanban: off, no tracker binding."), "{out}");
        assert!(out.contains("Old kanban items stay in kanban.db untouched."), "{out}");
        let text = config_text(&f);
        assert!(text.starts_with(&original), "every existing byte kept: {text}");
        assert_eq!(parse(&text).unwrap().projects["personal/corrtex"].kanban, Some(false));
        let saved = backups(&f);
        assert_eq!(saved.len(), 1);
        assert_eq!(std::fs::read_to_string(&saved[0]).unwrap(), original);
        let again = kanban_run(&f, "personal/corrtex", false).unwrap();
        assert!(again.contains("UNCHANGED") && again.contains("Nothing changed.") && again.contains("kanban: off"), "{again}");
        assert_eq!(config_text(&f), text);
        assert_eq!(backups(&f).len(), 1);
        assert!(!f.cfg.join("config.yml.lock").exists());
    }

    #[test]
    fn kanban_on_flips_the_setting_and_refuses_a_global_false_without_writing() {
        let f = fixture();
        let error = kanban_run(&f, "personal/corrtex", true).unwrap_err();
        assert!(error.contains("kanban.enabled"), "{error}");
        assert!(backups(&f).is_empty());
        enable_board(&f);
        kanban_run(&f, "personal/corrtex", false).unwrap();
        let out = kanban_run(&f, "personal/corrtex", true).unwrap();
        assert!(out.contains("personal/corrtex kanban: on."), "{out}");
        assert_eq!(parse(&config_text(&f)).unwrap().projects["personal/corrtex"].kanban, Some(true));
    }

    #[test]
    fn kanban_refuses_a_project_with_no_vault_folder_and_a_bad_key() {
        let f = fixture();
        let error = kanban_run(&f, "personal/nope", false).unwrap_err();
        assert!(error.contains("has no folder in the vault"), "{error}");
        assert!(kanban_run(&f, "personal", false).unwrap_err().contains("is not <domain>/<project>"));
        assert!(backups(&f).is_empty());
        assert!(!f.cfg.join("config.yml.lock").exists());
    }
}
