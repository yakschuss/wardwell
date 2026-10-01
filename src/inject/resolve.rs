//! Decides which vault project or domain a working directory belongs to, for
//! session start and the Stop check. Order: the git common directory's work
//! tree, so a linked worktree counts as its main checkout; then the longest
//! configured `projects:` path that contains the directory; then a vault
//! domain folder named like the directory, as before.
//!
//! Does NOT print, read project files, or run git itself: the caller passes
//! the git lookup in.

use crate::config::loader::WardwellConfig;
use crate::inject::git::{GitDirs, canonical};
use std::path::{Path, PathBuf};

/// Where a directory resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// A mapped vault project, by its `<domain>/<project>` key.
    Project { domain: String, project: String },
    /// A vault domain folder whose name equals the directory's name.
    Domain(PathBuf),
}

/// Resolve `cwd`, which the caller has made absolute. `git` returns the
/// repository directories for a path, or None outside a repository.
pub fn resolve(cwd: &Path, config: &WardwellConfig, git: impl Fn(&Path) -> Option<GitDirs>) -> Option<Resolution> {
    let cwd = canonical(cwd);
    candidates(&cwd, git)
        .iter()
        .find_map(|dir| mapped_project(dir, config))
        .or_else(|| domain_named(&cwd, &config.vault_path))
}

/// The directory as seen from the main checkout first, then as it is.
fn candidates(cwd: &Path, git: impl Fn(&Path) -> Option<GitDirs>) -> Vec<PathBuf> {
    let in_main = git(cwd).and_then(|dirs| {
        let relative = cwd.strip_prefix(&dirs.toplevel).ok()?;
        Some(dirs.main_worktree()?.join(relative))
    });
    in_main.into_iter().chain(std::iter::once(cwd.to_path_buf())).collect()
}

/// The project whose configured path is the longest prefix of `dir`.
fn mapped_project(dir: &Path, config: &WardwellConfig) -> Option<Resolution> {
    config
        .projects
        .values()
        .flat_map(|m| m.paths.iter().map(move |p| (m, canonical(p))))
        .filter(|(_, path)| dir.starts_with(path))
        .max_by_key(|(_, path)| path.components().count())
        .map(|(m, _)| Resolution::Project { domain: m.domain.clone(), project: m.project.clone() })
}

/// The vault domain folder named like `cwd`, unchanged from the old match.
fn domain_named(cwd: &Path, vault: &Path) -> Option<Resolution> {
    let name = cwd.file_name().and_then(|n| n.to_str())?;
    std::fs::read_dir(vault)
        .ok()?
        .flatten()
        .find(|e| e.path().is_dir() && e.file_name().to_string_lossy() == name)
        .map(|e| Resolution::Domain(e.path()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::inject::git::testing::{git, repo};

    fn config(dir: &Path, projects: &[(&str, &[&Path])]) -> WardwellConfig {
        let vault = dir.join("vault");
        std::fs::create_dir_all(vault.join("work")).unwrap();
        let mut yaml = format!("vault_path: {}\nsession_sources: []\nprojects:\n", vault.display());
        for (key, paths) in projects {
            yaml.push_str(&format!("  {key}:\n    paths:\n"));
            paths.iter().for_each(|p| yaml.push_str(&format!("      - {}\n", p.display())));
        }
        if projects.is_empty() {
            yaml = format!("vault_path: {}\nsession_sources: []\n", vault.display());
        }
        std::fs::write(dir.join("config.yml"), yaml).unwrap();
        crate::config::loader::load(Some(&dir.join("config.yml"))).unwrap()
    }

    fn project(key: &str) -> Option<Resolution> {
        let (domain, project) = key.split_once('/').unwrap();
        Some(Resolution::Project { domain: domain.into(), project: project.into() })
    }

    #[test]
    fn a_mapped_repo_and_its_subfolders_resolve_to_the_project() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("code/corrtex");
        repo(&repo_dir);
        std::fs::create_dir_all(repo_dir.join("app/models")).unwrap();
        let config = config(tmp.path(), &[("personal/corr-platform", &[&repo_dir])]);
        let git = crate::inject::git::dirs;
        assert_eq!(resolve(&repo_dir, &config, git), project("personal/corr-platform"));
        assert_eq!(resolve(&repo_dir.join("app/models"), &config, git), project("personal/corr-platform"));
    }

    #[test]
    fn a_linked_worktree_outside_the_mapped_path_resolves_through_its_main_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("code/corrtex");
        repo(&main);
        let linked = tmp.path().join("elsewhere/corrtex-cm-1");
        git(&main, &["worktree", "add", "-q", "-b", "cm-1", linked.to_str().unwrap()]);
        std::fs::create_dir_all(linked.join("app")).unwrap();
        let config = config(tmp.path(), &[("personal/corr-platform", &[&main])]);
        assert_eq!(resolve(&linked, &config, crate::inject::git::dirs), project("personal/corr-platform"));
        assert_eq!(resolve(&linked.join("app"), &config, crate::inject::git::dirs), project("personal/corr-platform"));
    }

    #[test]
    fn the_longest_configured_prefix_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let mono = tmp.path().join("mono");
        let sub = mono.join("services/billing");
        std::fs::create_dir_all(sub.join("src")).unwrap();
        let config = config(tmp.path(), &[("work/mono", &[&mono]), ("work/billing", &[&sub])]);
        let no_git = |_: &Path| None;
        assert_eq!(resolve(&sub.join("src"), &config, no_git), project("work/billing"));
        assert_eq!(resolve(&mono.join("services"), &config, no_git), project("work/mono"));
    }

    #[test]
    fn a_sibling_with_a_shared_name_prefix_does_not_match() {
        let tmp = tempfile::tempdir().unwrap();
        let mapped = tmp.path().join("corrtex");
        let sibling = tmp.path().join("corrtex-old");
        std::fs::create_dir_all(&mapped).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let config = config(tmp.path(), &[("personal/corr-platform", &[&mapped])]);
        assert_eq!(resolve(&sibling, &config, |_: &Path| None), None);
    }

    #[test]
    fn an_unmapped_directory_falls_back_to_the_domain_name_match_or_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config(tmp.path(), &[]);
        let work = tmp.path().join("checkouts/work");
        let other = tmp.path().join("checkouts/other");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let found = resolve(&work, &config, |_: &Path| None);
        assert_eq!(found, Some(Resolution::Domain(tmp.path().join("vault/work"))));
        assert_eq!(resolve(&other, &config, |_: &Path| None), None);
    }

    #[test]
    fn a_mapping_beats_a_domain_named_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("checkouts/work");
        std::fs::create_dir_all(&work).unwrap();
        let config = config(tmp.path(), &[("personal/notes", &[&work])]);
        assert_eq!(resolve(&work, &config, |_: &Path| None), project("personal/notes"));
    }
}
