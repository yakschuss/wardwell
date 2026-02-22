use crate::index::store::{IndexError, IndexStore};
use crate::vault::reader::walk_vault_filtered;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;

/// Stats from an index build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildStats {
    pub indexed: usize,
    pub skipped: usize,
    pub removed: usize,
    pub errors: usize,
    pub error_details: Vec<String>,
}

/// Build the full index from a vault directory.
pub struct IndexBuilder;

impl IndexBuilder {
    /// Incremental build: upsert changed files, remove stale entries.
    pub fn full_build(store: &IndexStore, vault_root: &Path) -> Result<BuildStats, IndexError> {
        Self::build_filtered(store, vault_root, &[])
    }

    /// Incremental build with exclusion patterns.
    pub fn build_filtered(store: &IndexStore, vault_root: &Path, exclude: &[String]) -> Result<BuildStats, IndexError> {
        let results = walk_vault_filtered(vault_root, exclude);
        let mut indexed = 0;
        let mut skipped = 0;
        let mut errors = 0;
        let mut error_details = Vec::new();
        let mut seen_paths = HashSet::new();

        for result in results {
            match result {
                Ok(vf) => {
                    let rel_path = vf.path
                        .strip_prefix(vault_root)
                        .unwrap_or(&vf.path)
                        .to_string_lossy()
                        .to_string();
                    seen_paths.insert(rel_path.clone());
                    match store.upsert(&vf, vault_root) {
                        Ok(true) => indexed += 1,
                        Ok(false) => skipped += 1,
                        Err(e) => {
                            error_details.push(format!("{rel_path}: {e}"));
                            errors += 1;
                        }
                    }
                }
                Err(e) => {
                    error_details.push(format!("{e}"));
                    errors += 1;
                }
            }
        }

        // Remove stale entries (files that no longer exist on disk)
        let removed = store.remove_stale(&seen_paths)?;

        Ok(BuildStats { indexed, skipped, removed, errors, error_details })
    }
}

pub(crate) fn compute_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn create_test_vault(dir: &Path) {
        let write = |name: &str, content: &str| {
            let path = dir.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&path, content).ok();
        };

        write(
            "myapp.md",
            "---\ntype: project\ndomain: myapp\nstatus: active\nsummary: Task tracker\ntags: [security]\n---\n## Summary\nTest body.\n",
        );
        write(
            "myapp/auth.md",
            "---\ntype: decision\ndomain: myapp\nstatus: resolved\nsummary: Auth approach\n---\n## Context\nDecision body.\n",
        );
        write(
            "insights/debugging.md",
            "---\ntype: insight\nconfidence: inferred\nsummary: Check clippy first\ntags: [rust, debugging]\n---\n## Pattern\nAlways check clippy.\n",
        );
    }

    #[test]
    fn full_build_populates_index() {
        let dir = tempfile::tempdir().unwrap();
        create_test_vault(dir.path());

        let store = IndexStore::in_memory().unwrap();
        let stats = IndexBuilder::full_build(&store, dir.path());
        assert!(stats.is_ok(), "{stats:?}");
        let stats = stats.unwrap();
        assert_eq!(stats.indexed, 3);
        assert_eq!(stats.errors, 0);
    }

    #[test]
    fn full_build_is_incremental() {
        let dir = tempfile::tempdir().unwrap();
        create_test_vault(dir.path());

        let store = IndexStore::in_memory().unwrap();
        let stats = IndexBuilder::full_build(&store, dir.path()).unwrap();
        assert_eq!(stats.indexed, 3);

        // Second build should skip all unchanged files
        let stats2 = IndexBuilder::full_build(&store, dir.path()).unwrap();
        assert_eq!(stats2.indexed, 0);
        assert_eq!(stats2.skipped, 3);
    }

    #[test]
    fn build_filtered_excludes_dirs() {
        let dir = tempfile::tempdir().unwrap();
        create_test_vault(dir.path());
        // Add a file in an excluded directory
        let nm = dir.path().join("node_modules");
        std::fs::create_dir_all(&nm).ok();
        std::fs::write(nm.join("junk.md"), "---\ntype: reference\n---\njunk\n").ok();

        let store = IndexStore::in_memory().unwrap();
        let exclude = vec!["node_modules".to_string()];
        let stats = IndexBuilder::build_filtered(&store, dir.path(), &exclude).unwrap();
        assert_eq!(stats.indexed, 3); // node_modules/junk.md excluded
    }

    #[test]
    fn build_removes_stale_entries() {
        let dir = tempfile::tempdir().unwrap();
        create_test_vault(dir.path());

        let store = IndexStore::in_memory().unwrap();
        IndexBuilder::full_build(&store, dir.path()).ok();

        // Remove a file from disk
        std::fs::remove_file(dir.path().join("myapp.md")).ok();

        let stats = IndexBuilder::full_build(&store, dir.path()).unwrap();
        assert_eq!(stats.removed, 1);
    }

    #[test]
    fn full_build_indexes_files_without_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("good.md"), "---\ntype: project\n---\nbody\n").ok();
        std::fs::write(dir.path().join("plain.md"), "no frontmatter").ok();

        let store = IndexStore::in_memory().unwrap();
        let stats = IndexBuilder::full_build(&store, dir.path());
        assert!(stats.is_ok(), "{stats:?}");
        let stats = stats.unwrap();
        assert_eq!(stats.indexed, 2);
        assert_eq!(stats.errors, 0);
    }

    #[test]
    fn meta_table_populated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.md"),
            "---\ntype: project\ndomain: test\nsummary: Test\n---\nbody\n",
        ).ok();

        let store = IndexStore::in_memory().unwrap();
        IndexBuilder::full_build(&store, dir.path()).ok();

        let conn = store.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM vault_meta", [], |row| row.get(0))
            .unwrap_or(0);
        assert_eq!(count, 1);
    }
}
