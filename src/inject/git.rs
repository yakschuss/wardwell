//! Local git reads for session start and the Stop check: where a directory's
//! repository lives and how many commits it gained since a time. Every call
//! runs under a deadline and fails closed to None, never to a guess.
//!
//! Does NOT fetch, write, or read any remote.

use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long one git call may run before it is killed.
pub const DEADLINE: Duration = Duration::from_secs(2);

/// The work tree a directory sits in and the repository's common git directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitDirs {
    pub toplevel: PathBuf,
    pub common_dir: PathBuf,
}

impl GitDirs {
    /// The main checkout: the folder holding the common `.git` directory.
    /// None for a bare repository.
    pub fn main_worktree(&self) -> Option<PathBuf> {
        (self.common_dir.file_name()? == ".git").then(|| self.common_dir.parent().map(Path::to_path_buf))?
    }

    /// Where `dir`, inside this work tree, sits in the main checkout.
    pub fn in_main_worktree(&self, dir: &Path) -> Option<PathBuf> {
        let relative = dir.strip_prefix(&self.toplevel).ok()?;
        Some(self.main_worktree()?.join(relative).components().collect())
    }
}

/// `git rev-parse` for `dir`, or None outside a repository or on any error.
pub fn dirs(dir: &Path) -> Option<GitDirs> {
    dirs_by(dir, Instant::now() + DEADLINE)
}

/// `dirs`, killed at `deadline`.
pub fn dirs_by(dir: &Path, deadline: Instant) -> Option<GitDirs> {
    let out = run(dir, &["rev-parse", "--show-toplevel", "--git-common-dir"], deadline)?;
    let mut lines = out.lines();
    let toplevel = PathBuf::from(lines.next()?);
    let common = PathBuf::from(lines.next()?);
    let common = if common.is_absolute() { common } else { dir.join(common) };
    Some(GitDirs { toplevel: canonical(&toplevel), common_dir: canonical(&common) })
}

/// Commits reachable from HEAD in `dir`'s repository authored at or after
/// `since`, or None on any git error. A rebased commit keeps its old author
/// time and is not counted.
/// The call is killed at `deadline`.
pub fn commits_since(dir: &Path, since: DateTime<Utc>, deadline: Instant) -> Option<usize> {
    let after = format!("--since={}", since.timestamp());
    let out = run(dir, &["log", "--format=%at", &after, "HEAD"], deadline)?;
    // Git times are whole seconds; a commit in the start's own second is
    // not counted, so a commit made just before the start never is.
    let since = since.timestamp() + i64::from(since.timestamp_subsec_nanos() > 0);
    Some(out.lines().filter_map(|l| l.trim().parse::<i64>().ok()).filter(|t| *t >= since).count())
}

/// The canonical form of `path`, or `path` itself when it cannot be resolved.
pub fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Stdout of `git -C dir <args>` when it exits zero before `deadline`.
/// The caller's git environment is cleared so a hook's GIT_DIR cannot
/// point the call at another repository.
fn run(dir: &Path, args: &[&str], deadline: Instant) -> Option<String> {
    if Instant::now() > deadline {
        return None;
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    loop {
        match child.try_wait().ok()? {
            Some(status) if status.success() => break,
            Some(_) => return None,
            None if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    Some(out)
}

/// Temp repositories for tests. Global and system git config are ignored.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub mod testing {
    use std::path::Path;
    use std::process::Command;

    pub fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    /// A repository at `dir` with one commit.
    pub fn repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
        commit(dir, "first");
    }

    /// One empty commit dated `when`, a git date such as `@1700000000 +0000`,
    /// or now when None.
    pub fn commit_at(dir: &Path, message: &str, when: Option<&str>) {
        let mut cmd = vec!["commit", "-q", "--allow-empty", "-m", message];
        let date;
        if let Some(w) = when {
            date = format!("--date={w}");
            cmd.push(&date);
        }
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"])
            .args(&cmd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .envs(when.map(|w| ("GIT_COMMITTER_DATE", w.to_string())))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }

    pub fn commit(dir: &Path, message: &str) {
        commit_at(dir, message, None);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::testing::*;
    use super::*;

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    #[test]
    fn a_linked_worktree_names_the_main_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("corrtex");
        repo(&main);
        let linked = tmp.path().join("wt/feature");
        git(&main, &["worktree", "add", "-q", "-b", "feature", linked.to_str().unwrap()]);
        let found = dirs(&linked.join(".")).unwrap();
        assert_eq!(found.toplevel, canonical(&linked));
        assert_eq!(found.main_worktree(), Some(canonical(&main)));
        assert_eq!(dirs(&main).unwrap().main_worktree(), Some(canonical(&main)));
    }

    #[test]
    fn outside_a_repository_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(dirs(tmp.path()), None);
        assert_eq!(commits_since(tmp.path(), Utc::now(), far()), None);
    }

    #[test]
    fn commits_since_counts_only_commits_at_or_after_the_time() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        git(tmp.path(), &["init", "-q", "-b", "main"]);
        commit_at(tmp.path(), "old", Some("@1700000000 +0000"));
        commit_at(tmp.path(), "new1", Some("@1800000000 +0000"));
        commit_at(tmp.path(), "new2", Some("@1800000100 +0000"));
        let since = DateTime::from_timestamp(1_790_000_000, 0).unwrap();
        assert_eq!(commits_since(tmp.path(), since, far()), Some(2));
        let mid_second = DateTime::from_timestamp(1_800_000_000, 500_000_000).unwrap();
        assert_eq!(commits_since(tmp.path(), mid_second, far()), Some(1), "the start's own second is excluded");
        let later = DateTime::from_timestamp(1_900_000_000, 0).unwrap();
        assert_eq!(commits_since(tmp.path(), later, far()), Some(0));
        let passed = Instant::now() - Duration::from_millis(1);
        assert_eq!(commits_since(tmp.path(), since, passed), None, "a passed deadline fails to None");
    }
}
