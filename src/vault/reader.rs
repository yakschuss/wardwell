use crate::vault::frontmatter::parse_frontmatter;
use crate::vault::types::{VaultError, VaultFile};
use std::path::{Path, PathBuf};

/// Read a single vault file, parsing its frontmatter and body.
/// Files without frontmatter are indexed with default metadata (type: reference).
pub fn read_file(path: &Path) -> Result<VaultFile, VaultError> {
    let content = std::fs::read_to_string(path).map_err(|e| VaultError::Io {
        path: path.display().to_string(),
        source: e,
    })?;

    match parse_frontmatter(&content) {
        Ok((frontmatter, body)) => Ok(VaultFile {
            path: path.to_path_buf(),
            frontmatter,
            body,
        }),
        Err(VaultError::NoFrontmatter | VaultError::UnclosedFrontmatter) => {
            // No frontmatter — index the whole file with defaults.
            // Infer summary from first non-empty line.
            let summary = content.lines()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim_start_matches('#').trim().to_string());

            Ok(VaultFile {
                path: path.to_path_buf(),
                frontmatter: crate::vault::types::Frontmatter {
                    file_type: crate::vault::types::VaultType::Reference,
                    domain: None,
                    status: None,
                    confidence: None,
                    updated: None,
                    summary,
                    related: Vec::new(),
                    tags: Vec::new(),
                    can_read: Vec::new(),
                },
                body: content,
            })
        }
        Err(e) => Err(e),
    }
}

/// Recursively walk a vault directory and parse all .md files.
/// Returns a Vec of Results — individual file errors don't stop the walk.
pub fn walk_vault(root: &Path) -> Vec<Result<VaultFile, VaultError>> {
    walk_vault_filtered(root, &[])
}

/// Walk vault with exclusion patterns. Each pattern is matched against
/// directory/file names (e.g., "node_modules", ".obsidian", ".git").
pub fn walk_vault_filtered(root: &Path, exclude: &[String]) -> Vec<Result<VaultFile, VaultError>> {
    let mut results = Vec::new();
    walk_recursive(root, exclude, &mut results);
    results
}

fn walk_recursive(dir: &Path, exclude: &[String], results: &mut Vec<Result<VaultFile, VaultError>>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            results.push(Err(VaultError::Io {
                path: dir.display().to_string(),
                source: e,
            }));
            return;
        }
    };

    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        paths.push(entry.path());
    }
    paths.sort();

    for path in paths {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if exclude.iter().any(|e| e == name) {
            continue;
        }
        if path.is_dir() {
            walk_recursive(&path, exclude, results);
        } else if path.extension().is_some_and(|ext| ext == "md") {
            results.push(read_file(&path));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn create_vault_file(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&path, content).ok();
    }

    #[test]
    fn read_project_file() {
        let dir = tempfile::tempdir().unwrap();
        create_vault_file(
            dir.path(),
            "myapp.md",
            "---\ntype: project\ndomain: myapp\nstatus: active\nsummary: Test project\n---\n## Summary\nBody here.\n",
        );

        let result = read_file(&dir.path().join("myapp.md"));
        assert!(result.is_ok(), "{result:?}");
        let vf = result.unwrap();
        assert_eq!(vf.frontmatter.file_type, crate::vault::types::VaultType::Project);
        assert!(vf.body.contains("## Summary"));
    }

    #[test]
    fn read_decision_file() {
        let dir = tempfile::tempdir().unwrap();
        create_vault_file(
            dir.path(),
            "auth.md",
            "---\ntype: decision\nstatus: resolved\n---\n## Context\n",
        );

        let result = read_file(&dir.path().join("auth.md"));
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn read_file_without_frontmatter_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        create_vault_file(dir.path(), "plain.md", "# My Notes\nSome content here.");

        let result = read_file(&dir.path().join("plain.md"));
        assert!(result.is_ok(), "{result:?}");
        let vf = result.unwrap();
        assert_eq!(vf.frontmatter.file_type, crate::vault::types::VaultType::Reference);
        assert_eq!(vf.frontmatter.summary.as_deref(), Some("My Notes"));
        assert!(vf.body.contains("Some content here."));
    }

    #[test]
    fn read_nonexistent_file_returns_error() {
        let result = read_file(Path::new("/nonexistent/file.md"));
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn walk_vault_finds_all_md_files() {
        let dir = tempfile::tempdir().unwrap();
        create_vault_file(
            dir.path(),
            "project.md",
            "---\ntype: project\n---\nbody\n",
        );
        create_vault_file(
            dir.path(),
            "sub/decision.md",
            "---\ntype: decision\n---\nbody\n",
        );
        create_vault_file(
            dir.path(),
            "not-markdown.txt",
            "ignored",
        );

        let results = walk_vault(dir.path());
        let ok_count = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(ok_count, 2);
    }

    #[test]
    fn walk_vault_indexes_files_without_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        create_vault_file(
            dir.path(),
            "good.md",
            "---\ntype: insight\n---\nbody\n",
        );
        create_vault_file(
            dir.path(),
            "plain.md",
            "Just some notes without frontmatter",
        );

        let results = walk_vault(dir.path());
        assert_eq!(results.len(), 2);
        let ok_count = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(ok_count, 2);
    }
}
