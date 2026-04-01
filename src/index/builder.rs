use crate::index::chunk::{chunk_file, chunk_jsonl};
use crate::index::embed::Embedder;
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
    pub chunks_embedded: usize,
    pub error_details: Vec<String>,
}

/// Build the full index from a vault directory.
pub struct IndexBuilder;

impl IndexBuilder {
    /// Incremental build: upsert changed files, remove stale entries.
    /// Pass `embedder` to generate vector embeddings for hybrid search.
    pub fn full_build(
        store: &IndexStore,
        vault_root: &Path,
        embedder: Option<&mut Embedder>,
    ) -> Result<BuildStats, IndexError> {
        Self::build_filtered(store, vault_root, &[], embedder)
    }

    /// Incremental build with exclusion patterns.
    pub fn build_filtered(
        store: &IndexStore,
        vault_root: &Path,
        exclude: &[String],
        mut embedder: Option<&mut Embedder>,
    ) -> Result<BuildStats, IndexError> {
        let results = walk_vault_filtered(vault_root, exclude);
        let mut indexed = 0;
        let mut skipped = 0;
        let mut errors = 0;
        let mut chunks_embedded = 0;
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

                    let is_jsonl = vf.path.extension().and_then(|e| e.to_str()) == Some("jsonl");

                    if is_jsonl {
                        // Watermark-based incremental indexing for append-only JSONL files
                        match index_jsonl_incremental(store, &vf, &rel_path, vault_root, &mut embedder, &mut error_details) {
                            Ok(new_chunks) => {
                                if new_chunks > 0 {
                                    indexed += 1;
                                    chunks_embedded += new_chunks;
                                } else {
                                    skipped += 1;
                                }
                            }
                            Err(e) => {
                                error_details.push(format!("{rel_path}: {e}"));
                                errors += 1;
                            }
                        }
                    } else {
                        match store.upsert(&vf, vault_root) {
                            Ok(true) => {
                                indexed += 1;

                                // Chunk the file and upsert chunks
                                let chunks = chunk_file(&vf.path, &vf.body);
                                if !chunks.is_empty() {
                                    match store.upsert_chunks(&rel_path, &chunks) {
                                        Ok(changed_ids) => {
                                            // Embed changed chunks if embedder available
                                            if let Some(ref mut emb) = embedder
                                                && !changed_ids.is_empty() {
                                                    // Collect texts for changed chunks
                                                    let texts: Vec<String> = changed_ids.iter()
                                                        .filter_map(|id| {
                                                            chunks.iter()
                                                                .find(|c| format!("{rel_path}::{}", c.index) == *id)
                                                                .map(|c| c.body.clone())
                                                        })
                                                        .collect();

                                                    match emb.embed_batch(&texts) {
                                                        Ok(vecs) => {
                                                            if let Err(e) = store.upsert_embeddings(&changed_ids, &vecs) {
                                                                error_details.push(format!("{rel_path} embeddings: {e}"));
                                                            } else {
                                                                chunks_embedded += vecs.len();
                                                            }
                                                        }
                                                        Err(e) => {
                                                            error_details.push(format!("{rel_path} embed: {e}"));
                                                        }
                                                    }
                                                }
                                        }
                                        Err(e) => {
                                            error_details.push(format!("{rel_path} chunks: {e}"));
                                        }
                                    }
                                }
                            }
                            Ok(false) => skipped += 1,
                            Err(e) => {
                                error_details.push(format!("{rel_path}: {e}"));
                                errors += 1;
                            }
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

        Ok(BuildStats { indexed, skipped, removed, errors, chunks_embedded, error_details })
    }
}

pub(crate) fn compute_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Public convenience: incremental JSONL indexing without embedder (used by watcher).
pub fn index_jsonl_incremental_public(
    store: &IndexStore,
    vf: &crate::vault::types::VaultFile,
    rel_path: &str,
    vault_root: &Path,
) -> Result<usize, IndexError> {
    let mut errors = Vec::new();
    let result = index_jsonl_incremental(store, vf, rel_path, vault_root, &mut None, &mut errors);
    if !errors.is_empty() {
        eprintln!("wardwell: jsonl index errors: {}", errors.join(", "));
    }
    result
}

/// Incremental indexing for append-only JSONL files using line-count watermarks.
/// Only processes lines beyond the stored watermark. Returns the number of new chunks indexed.
fn index_jsonl_incremental(
    store: &IndexStore,
    vf: &crate::vault::types::VaultFile,
    rel_path: &str,
    vault_root: &Path,
    embedder: &mut Option<&mut Embedder>,
    error_details: &mut Vec<String>,
) -> Result<usize, IndexError> {
    let watermark = store.get_watermark(rel_path)?;

    // Count content lines (skip schema headers and empty lines, matching chunk_jsonl behavior)
    let content_lines: Vec<&str> = vf.body.lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with("{\"_schema\"")
        })
        .collect();

    let total_lines = content_lines.len();
    if total_lines <= watermark {
        return Ok(0); // no new lines
    }

    // Chunk only the new lines
    let new_lines: String = content_lines[watermark..].join("\n");
    let new_chunks = chunk_jsonl(&new_lines);

    if new_chunks.is_empty() {
        store.set_watermark(rel_path, total_lines)?;
        return Ok(0);
    }

    // Re-number chunks to continue from the watermark offset
    let offset_chunks: Vec<crate::index::chunk::Chunk> = new_chunks
        .into_iter()
        .map(|mut c| {
            c.index += watermark;
            c
        })
        .collect();

    // Upsert vault_search/vault_meta for the whole file (needed for FTS5 search on the full body)
    // We always re-insert because the body has grown.
    store.upsert(vf, vault_root)?;

    // Append new chunks (upsert_chunks handles dedup via chunk_id hash)
    let mut new_embedded = 0;
    for chunk in &offset_chunks {
        let chunk_id = format!("{rel_path}::{}", chunk.index);
        let new_hash = compute_hash(&chunk.body);

        // Insert into vault_chunks
        {
            let conn = store.lock()?;
            conn.execute(
                "INSERT OR REPLACE INTO vault_chunks (chunk_id, path, chunk_index, heading, body, body_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![chunk_id, rel_path, chunk.index as i64, chunk.heading, chunk.body, new_hash],
            )?;
            conn.execute(
                "INSERT INTO chunk_search (chunk_id, path, heading, body)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![chunk_id, rel_path, chunk.heading.as_deref().unwrap_or(""), chunk.body],
            )?;
        }

        // Embed if embedder available
        if let Some(emb) = embedder {
            match emb.embed_batch(std::slice::from_ref(&chunk.body)) {
                Ok(vecs) => {
                    if let Err(e) = store.upsert_embeddings(&[chunk_id], &vecs) {
                        error_details.push(format!("{rel_path} embedding: {e}"));
                    } else {
                        new_embedded += vecs.len();
                    }
                }
                Err(e) => {
                    error_details.push(format!("{rel_path} embed: {e}"));
                }
            }
        }
    }

    store.set_watermark(rel_path, total_lines)?;
    Ok(if new_embedded > 0 { new_embedded } else { offset_chunks.len() })
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
        let stats = IndexBuilder::full_build(&store, dir.path(), None);
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
        let stats = IndexBuilder::full_build(&store, dir.path(), None).unwrap();
        assert_eq!(stats.indexed, 3);

        // Second build should skip all unchanged files
        let stats2 = IndexBuilder::full_build(&store, dir.path(), None).unwrap();
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
        let stats = IndexBuilder::build_filtered(&store, dir.path(), &exclude, None).unwrap();
        assert_eq!(stats.indexed, 3); // node_modules/junk.md excluded
    }

    #[test]
    fn build_removes_stale_entries() {
        let dir = tempfile::tempdir().unwrap();
        create_test_vault(dir.path());

        let store = IndexStore::in_memory().unwrap();
        IndexBuilder::full_build(&store, dir.path(), None).ok();

        // Remove a file from disk
        std::fs::remove_file(dir.path().join("myapp.md")).ok();

        let stats = IndexBuilder::full_build(&store, dir.path(), None).unwrap();
        assert_eq!(stats.removed, 1);
    }

    #[test]
    fn full_build_indexes_files_without_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("good.md"), "---\ntype: project\n---\nbody\n").ok();
        std::fs::write(dir.path().join("plain.md"), "no frontmatter").ok();

        let store = IndexStore::in_memory().unwrap();
        let stats = IndexBuilder::full_build(&store, dir.path(), None);
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
        IndexBuilder::full_build(&store, dir.path(), None).ok();

        let conn = store.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM vault_meta", [], |row| row.get(0))
            .unwrap_or(0);
        assert_eq!(count, 1);
    }

    #[test]
    fn full_build_indexes_history_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("work").join("myproject");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("history.jsonl"),
            "{\"_schema\": \"history\", \"_version\": \"1.0\"}\n\
             {\"date\": \"2026-03-15\", \"title\": \"Added shulops bulletin feature\", \"body\": \"Implemented the new bulletin board\", \"source\": \"code\"}\n\
             {\"date\": \"2026-03-16\", \"title\": \"Fixed auth bug\", \"body\": \"Resolved JWT expiration issue\", \"source\": \"code\"}\n",
        ).unwrap();

        let store = IndexStore::in_memory().unwrap();
        let stats = IndexBuilder::full_build(&store, dir.path(), None).unwrap();
        assert!(stats.indexed > 0, "expected history.jsonl to be indexed");
        assert_eq!(stats.errors, 0);

        // Verify watermark was set
        let wm = store.get_watermark("work/myproject/history.jsonl").unwrap();
        assert_eq!(wm, 2); // 2 content lines (schema skipped)

        // Verify chunks were created
        let conn = store.lock().unwrap();
        let chunk_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM vault_chunks WHERE path = 'work/myproject/history.jsonl'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(chunk_count, 2);

        // Verify vault_meta has type=history
        let file_type: String = conn
            .query_row(
                "SELECT type FROM vault_meta WHERE path = 'work/myproject/history.jsonl'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(file_type, "history");
    }

    #[test]
    fn jsonl_incremental_only_indexes_new_lines() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("work").join("myproject");
        std::fs::create_dir_all(&project_dir).unwrap();

        // Initial content: 2 entries
        std::fs::write(
            project_dir.join("history.jsonl"),
            "{\"_schema\": \"history\"}\n\
             {\"date\": \"2026-03-15\", \"title\": \"First entry\", \"body\": \"content one\", \"source\": \"code\"}\n\
             {\"date\": \"2026-03-16\", \"title\": \"Second entry\", \"body\": \"content two\", \"source\": \"code\"}\n",
        ).unwrap();

        let store = IndexStore::in_memory().unwrap();
        let stats1 = IndexBuilder::full_build(&store, dir.path(), None).unwrap();
        assert!(stats1.indexed > 0);
        let wm1 = store.get_watermark("work/myproject/history.jsonl").unwrap();
        assert_eq!(wm1, 2);

        // Append a new entry
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(project_dir.join("history.jsonl"))
            .unwrap();
        writeln!(f, "{{\"date\": \"2026-03-17\", \"title\": \"Third entry\", \"body\": \"content three\", \"source\": \"code\"}}").unwrap();
        drop(f);

        // Second build should pick up only the new entry
        let stats2 = IndexBuilder::full_build(&store, dir.path(), None).unwrap();
        assert!(stats2.indexed > 0, "new entry should be indexed");

        let wm2 = store.get_watermark("work/myproject/history.jsonl").unwrap();
        assert_eq!(wm2, 3);

        // Verify 3 chunks total
        let conn = store.lock().unwrap();
        let chunk_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM vault_chunks WHERE path = 'work/myproject/history.jsonl'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(chunk_count, 3);
    }

    #[test]
    fn jsonl_no_new_lines_skips() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("work").join("proj");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("history.jsonl"),
            "{\"_schema\": \"history\"}\n{\"date\": \"2026-03-15\", \"title\": \"Entry\", \"body\": \"body\", \"source\": \"code\"}\n",
        ).unwrap();

        let store = IndexStore::in_memory().unwrap();
        IndexBuilder::full_build(&store, dir.path(), None).unwrap();

        // Second build with no changes should skip
        let stats2 = IndexBuilder::full_build(&store, dir.path(), None).unwrap();
        assert_eq!(stats2.skipped, 1);
        assert_eq!(stats2.indexed, 0);
    }

    #[test]
    fn history_entries_searchable_via_fts() {
        use crate::index::fts::SearchQuery;

        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("work").join("myproject");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("history.jsonl"),
            "{\"_schema\": \"history\"}\n\
             {\"date\": \"2026-03-15\", \"title\": \"Added shulops bulletin feature\", \"body\": \"Implemented the new bulletin board for daily updates\", \"source\": \"code\"}\n\
             {\"date\": \"2026-03-16\", \"title\": \"Refactored database layer\", \"body\": \"Moved to connection pooling\", \"source\": \"desktop\"}\n",
        ).unwrap();

        let store = IndexStore::in_memory().unwrap();
        IndexBuilder::full_build(&store, dir.path(), None).unwrap();

        // Search for "shulops bulletin" — should find the history entry
        let q = SearchQuery {
            query: "shulops bulletin".to_string(),
            limit: 5,
            ..Default::default()
        };
        let results = store.search(&q).unwrap();
        assert!(results.total > 0, "should find history entry for 'shulops bulletin'");
        assert_eq!(results.results[0].frontmatter.file_type, crate::vault::types::VaultType::History);
    }

    #[test]
    fn history_domain_inferred_from_path() {
        use crate::index::fts::SearchQuery;
        use crate::vault::types::VaultType;

        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("personal").join("journal");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("history.jsonl"),
            "{\"_schema\": \"history\"}\n\
             {\"date\": \"2026-03-15\", \"title\": \"Morning routine update\", \"body\": \"Started journaling habit\", \"source\": \"manual\"}\n",
        ).unwrap();

        let store = IndexStore::in_memory().unwrap();
        IndexBuilder::full_build(&store, dir.path(), None).unwrap();

        // Domain should be inferred as "personal" from the path
        let q = SearchQuery {
            query: "journaling".to_string(),
            domains: Some(vec!["personal".to_string()]),
            limit: 5,
            ..Default::default()
        };
        let results = store.search(&q).unwrap();
        assert_eq!(results.total, 1);
        assert_eq!(results.results[0].frontmatter.domain.as_deref(), Some("personal"));
        assert_eq!(results.results[0].frontmatter.file_type, VaultType::History);
    }
}
