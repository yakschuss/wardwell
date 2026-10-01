//! Decides which vault project or domain a working directory belongs to, for
//! session start and the Stop check. Order: the git common directory's work
//! tree, so a linked worktree counts as its main checkout and only as that;
//! then the longest configured `projects:` path that contains the
//! directory; then a vault domain folder named like the directory, as before.
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
    // With no mappings there is nothing for git to find; skip the spawn.
    let mapped = (!config.projects.is_empty()).then(|| mapped_project(&match_dir(&cwd, git), config)).flatten();
    mapped.or_else(|| domain_named(&cwd, &config.vault_path))
}

/// The directory to match: inside a git work tree, the same place in the
/// main checkout only, so a linked worktree never borrows a mapping from
/// whatever folder it happens to sit in. Outside git, or for a bare
/// repository's worktree, the directory itself.
fn match_dir(cwd: &Path, git: impl Fn(&Path) -> Option<GitDirs>) -> PathBuf {
    git(cwd).and_then(|dirs| dirs.in_main_worktree(cwd)).unwrap_or_else(|| cwd.to_path_buf())
}

/// The project whose configured path is the longest prefix of `dir`,
/// among projects whose vault folder exists. A mapping to a missing folder
/// is skipped, so it never silences the directory.
fn mapped_project(dir: &Path, config: &WardwellConfig) -> Option<Resolution> {
    config
        .projects
        .values()
        .filter(|m| config.vault_path.join(&m.domain).join(&m.project).is_dir())
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
            std::fs::create_dir_all(vault.join(key)).unwrap();
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

    #[test]
    fn a_worktree_of_an_unmapped_repo_inside_a_mapped_tree_resolves_to_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mapped = tmp.path().join("code/corrtex");
        repo(&mapped);
        let unmapped = tmp.path().join("code/unmapped");
        repo(&unmapped);
        let feature = mapped.join(".worktrees/feature");
        git(&unmapped, &["worktree", "add", "-q", "-b", "f", feature.to_str().unwrap()]);
        let config = config(tmp.path(), &[("personal/corr-platform", &[&mapped])]);
        assert_eq!(resolve(&feature, &config, crate::inject::git::dirs), None);
    }

    #[test]
    fn a_mapping_without_a_vault_folder_falls_through_to_the_next_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path().join("checkouts");
        let work = outer.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let config = config(tmp.path(), &[("personal/outer", &[&outer]), ("personal/gone", &[&work])]);
        std::fs::remove_dir_all(tmp.path().join("vault/personal/gone")).unwrap();
        assert_eq!(resolve(&work, &config, |_: &Path| None), project("personal/outer"), "the shorter mapping");
        std::fs::remove_dir_all(tmp.path().join("vault/personal/outer")).unwrap();
        let found = resolve(&work, &config, |_: &Path| None);
        assert_eq!(found, Some(Resolution::Domain(tmp.path().join("vault/work"))), "then the domain-name match");
    }

    #[test]
    fn no_projects_means_no_git_lookup() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config(tmp.path(), &[]);
        let work = tmp.path().join("checkouts/work");
        std::fs::create_dir_all(&work).unwrap();
        let calls = std::cell::Cell::new(0);
        let counting = |_: &Path| {
            calls.set(calls.get() + 1);
            None
        };
        assert_eq!(resolve(&work, &config, counting), Some(Resolution::Domain(tmp.path().join("vault/work"))));
        assert_eq!(calls.get(), 0);
    }
}
