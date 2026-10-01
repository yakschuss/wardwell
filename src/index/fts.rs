use crate::index::store::{IndexError, IndexStore};
use crate::vault::types::{Confidence, Frontmatter, Status, VaultType};
use serde::{Deserialize, Serialize};

/// Search query parameters.
#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    pub query: String,
    /// Filter by domain(s). None = all domains. Some(vec) = only these domains.
    pub domains: Option<Vec<String>>,
    pub types: Vec<VaultType>,
    pub status: Option<Status>,
    pub limit: usize,
}

/// A single search result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub path: String,
    pub frontmatter: Frontmatter,
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_lines: Option<usize>,
}

/// Search response with results and total count.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResults {
    pub results: Vec<SearchResult>,
    pub total: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<String>,
}

impl IndexStore {
    /// Full-text search the vault index.
    pub fn search(&self, q: &SearchQuery) -> Result<SearchResults, IndexError> {
        let limit = if q.limit == 0 { 5 } else { q.limit };

        // Build the FTS5 query with filters
        let mut sql = String::from(
            "SELECT m.path, m.type, m.domain, m.status, m.confidence, m.updated,
                    m.summary, m.related, m.tags,
                    snippet(vault_search, 7, '', '', '...', 40) as snip,
                    s.body
             FROM vault_search s
             JOIN vault_meta m ON s.path = m.path
             WHERE vault_search MATCH ?1"
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        params.push(Box::new(match_expression(&q.query)));

        let mut param_idx = 2;

        if let Some(ref domains) = q.domains {
            if domains.len() == 1 {
                sql.push_str(&format!(" AND m.domain = ?{param_idx}"));
                params.push(Box::new(domains[0].clone()));
                param_idx += 1;
            } else if !domains.is_empty() {
                let placeholders: Vec<String> = domains.iter().enumerate().map(|(i, _)| {
                    format!("?{}", param_idx + i)
                }).collect();
                sql.push_str(&format!(" AND m.domain IN ({})", placeholders.join(", ")));
                for d in domains {
                    params.push(Box::new(d.clone()));
                }
                param_idx += domains.len();
            }
        }

        if !q.types.is_empty() {
            let placeholders: Vec<String> = q.types.iter().enumerate().map(|(i, _)| {
                format!("?{}", param_idx + i)
            }).collect();
            sql.push_str(&format!(" AND m.type IN ({})", placeholders.join(", ")));
            for t in &q.types {
                params.push(Box::new(t.to_string()));
            }
            param_idx += q.types.len();
        }

        if let Some(ref status) = q.status {
            sql.push_str(&format!(" AND m.status = ?{param_idx}"));
            params.push(Box::new(status.to_string()));
        }

        sql.push_str(&format!(" ORDER BY rank LIMIT {}", limit * 3));

        // Scope the lock so it's dropped before fuzzy_suggestions
        let mut results = Vec::new();
        {
            let conn = self.lock()?;
            let mut stmt = conn.prepare(&sql)?;

            let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(param_refs.as_slice(), |row| {
                let path: String = row.get(0)?;
                let file_type: String = row.get(1)?;
                let domain: Option<String> = row.get(2)?;
                let status: Option<String> = row.get(3)?;
                let confidence: Option<String> = row.get(4)?;
                let updated: Option<String> = row.get(5)?;
                let summary: Option<String> = row.get(6)?;
                let related: Option<String> = row.get(7)?;
                let tags: Option<String> = row.get(8)?;
                let snippet: String = row.get(9)?;
                let body: Option<String> = row.get(10)?;

                Ok((path, file_type, domain, status, confidence, updated, summary, related, tags, snippet, body))
            })?;

            for row in rows {
                let (path, file_type, domain, status, confidence, updated, summary, related, tags, snippet, body) = row?;

                let frontmatter = Frontmatter {
                    file_type: parse_vault_type(&file_type),
                    domain,
                    status: status.as_deref().and_then(parse_status),
                    confidence: confidence.as_deref().and_then(parse_confidence),
                    updated: updated.and_then(|s| chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok()),
                    summary,
                    related: related.map(|s| s.split(", ").filter(|s| !s.is_empty()).map(String::from).collect()).unwrap_or_default(),
                    tags: tags.map(|s| s.split(", ").filter(|s| !s.is_empty()).map(String::from).collect()).unwrap_or_default(),
                    can_read: Vec::new(),
                };

                let total_lines = body.as_ref().map(|b| b.lines().count());
                results.push(SearchResult { path, frontmatter, snippet, total_lines });
            }
        }

        // Dedup by path — FTS5 can return multiple rows per document
        let mut seen = std::collections::HashSet::new();
        results.retain(|r| seen.insert(r.path.clone()));
        results.truncate(limit);

        let total = results.len();

        if results.is_empty() {
            let suggestions = self.fuzzy_suggestions(&q.query)?;
            return Ok(SearchResults { results, total: 0, suggestions });
        }

        Ok(SearchResults { results, total, suggestions: Vec::new() })
    }

    fn fuzzy_suggestions(&self, query: &str) -> Result<Vec<String>, IndexError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT path, summary FROM vault_meta WHERE summary IS NOT NULL")?;
        let rows = stmt.query_map([], |row| {
            let path: String = row.get(0)?;
            let summary: String = row.get(1)?;
            Ok((path, summary))
        })?;

        let mut scored: Vec<(f64, String, String)> = Vec::new();
        for row in rows {
            let (path, summary) = row?;
            let similarity = strsim::jaro_winkler(query, &summary);
            if similarity > 0.6 {
                scored.push((similarity, path, summary));
            }
        }

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let suggestions: Vec<String> = scored.into_iter().take(3)
            .map(|(_, path, summary)| format!("{path} — {summary}"))
            .collect();
        Ok(suggestions)
    }
}

/// Turn a user query into an FTS5 MATCH expression. An ordinary query is one
/// quoted phrase, so hyphens and words like OR stay literal. A query holding a
/// ticket key (COR-12, CM-317, PROJ2-9) quotes each key as its own phrase,
/// keeps OR, AND and NOT between operands as operators, and quotes each run of
/// other words as a phrase.
pub fn match_expression(query: &str) -> String {
    let tokens: Vec<&str> = query.split_whitespace().collect();
    if !tokens.iter().any(|t| is_ticket_key(t)) {
        return quote(query);
    }
    let mut parts: Vec<String> = Vec::new();
    let mut words: Vec<&str> = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        let follows_operand = !words.is_empty() || parts.last().is_some_and(|p| p.starts_with('"'));
        let precedes_operand = tokens.get(i + 1).is_some_and(|next| !is_operator(next));
        if is_operator(token) && follows_operand && precedes_operand {
            flush_phrase(&mut words, &mut parts);
            parts.push((*token).to_string());
        } else if is_ticket_key(token) {
            flush_phrase(&mut words, &mut parts);
            parts.push(quote(token));
        } else {
            words.push(token);
        }
    }
    flush_phrase(&mut words, &mut parts);
    parts.join(" ")
}

fn flush_phrase(words: &mut Vec<&str>, parts: &mut Vec<String>) {
    if !words.is_empty() {
        parts.push(quote(&words.join(" ")));
        words.clear();
    }
}

fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}

fn is_operator(token: &str) -> bool {
    matches!(token, "OR" | "AND" | "NOT")
}

/// The ticket keys in `text`, upper-cased, in first-seen order, each once.
/// The same pattern `match_expression` quotes, so a key found here is a
/// key a search finds.
pub fn ticket_keys(text: &str) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for token in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')).map(|t| t.trim_matches('-')) {
        let key = token.to_ascii_uppercase();
        if is_ticket_key(token) && !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

/// Letters, optional digits, a hyphen, digits: COR-12, CM-317, PROJ2-9.
fn is_ticket_key(token: &str) -> bool {
    let Some((prefix, number)) = token.split_once('-') else {
        return false;
    };
    let letters = prefix.trim_end_matches(|c: char| c.is_ascii_digit());
    !letters.is_empty()
        && letters.chars().all(|c| c.is_ascii_alphabetic())
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
}

pub fn parse_vault_type(s: &str) -> VaultType {
    match s {
        "project" => VaultType::Project,
        "decision" => VaultType::Decision,
        "insight" => VaultType::Insight,
        "thread" => VaultType::Thread,
        "domain" => VaultType::Domain,
        "history" => VaultType::History,
        "reference" => VaultType::Reference,
        _ => VaultType::Reference, // fallback
    }
}

pub fn parse_status(s: &str) -> Option<Status> {
    match s {
        "active" => Some(Status::Active),
        "resolved" => Some(Status::Resolved),
        "abandoned" => Some(Status::Abandoned),
        "superseded" => Some(Status::Superseded),
        _ => None,
    }
}

pub fn parse_confidence(s: &str) -> Option<Confidence> {
    match s {
        "inferred" => Some(Confidence::Inferred),
        "proposed" => Some(Confidence::Proposed),
        "confirmed" => Some(Confidence::Confirmed),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::index::builder::IndexBuilder;

    fn build_test_index() -> IndexStore {
        let dir = tempfile::tempdir().unwrap();

        let write = |name: &str, content: &str| {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&path, content).ok();
        };

        write(
            "myapp.md",
            "---\ntype: project\ndomain: myapp\nstatus: active\nsummary: Project management tool\ntags: [auth, saas]\n---\n## Summary\nKeepSight is an MyApp is a project management platform.\n",
        );
        write(
            "myapp/auth.md",
            "---\ntype: decision\ndomain: myapp\nstatus: resolved\nconfidence: confirmed\nsummary: Chose JWT over sessions for auth\nrelated: [myapp.md]\ntags: [auth]\n---\n## Context\nAuthentication approach decision.\n",
        );
        write(
            "wardwell.md",
            "---\ntype: project\ndomain: wardwell\nstatus: active\nsummary: Personal AI knowledge vault\ntags: [rust, mcp]\n---\n## Summary\nWardwell is an MCP server for knowledge.\n",
        );
        write(
            "insights/debugging.md",
            "---\ntype: insight\nconfidence: inferred\nsummary: Always check clippy warnings first\ntags: [rust, debugging]\n---\n## Pattern\nCheck clippy before declaring fixed.\n",
        );

        let store = IndexStore::in_memory().unwrap();
        IndexBuilder::full_build(&store, dir.path(), None).ok();
        store
    }

    #[test]
    fn search_returns_ranked_results() {
        let store = build_test_index();
        let q = SearchQuery {
            query: "auth".to_string(),
            limit: 5,
            ..Default::default()
        };
        let results = store.search(&q);
        assert!(results.is_ok(), "{results:?}");
        let results = results.unwrap();
        assert!(results.total > 0);
    }

    #[test]
    fn search_filter_by_domain() {
        let store = build_test_index();
        let q = SearchQuery {
            query: "project management".to_string(),
            domains: Some(vec!["myapp".to_string()]),
            limit: 5,
            ..Default::default()
        };
        let results = store.search(&q);
        assert!(results.is_ok(), "{results:?}");
        let results = results.unwrap();
        for r in &results.results {
            assert_eq!(r.frontmatter.domain.as_deref(), Some("myapp"));
        }
    }

    #[test]
    fn search_filter_by_type() {
        let store = build_test_index();
        let q = SearchQuery {
            query: "auth compliance".to_string(),
            types: vec![VaultType::Decision],
            limit: 5,
            ..Default::default()
        };
        let results = store.search(&q);
        assert!(results.is_ok(), "{results:?}");
        let results = results.unwrap();
        for r in &results.results {
            assert_eq!(r.frontmatter.file_type, VaultType::Decision);
        }
    }

    #[test]
    fn search_zero_results_returns_suggestions() {
        let store = build_test_index();
        let q = SearchQuery {
            query: "xyznonexistent".to_string(),
            limit: 5,
            ..Default::default()
        };
        let results = store.search(&q);
        assert!(results.is_ok(), "{results:?}");
        let results = results.unwrap();
        assert_eq!(results.total, 0);
        // suggestions may or may not be present depending on fuzzy match
    }

    #[test]
    fn search_with_status_filter() {
        let store = build_test_index();
        let q = SearchQuery {
            query: "auth".to_string(),
            status: Some(Status::Resolved),
            limit: 5,
            ..Default::default()
        };
        let results = store.search(&q);
        assert!(results.is_ok(), "{results:?}");
        let results = results.unwrap();
        for r in &results.results {
            assert_eq!(r.frontmatter.status, Some(Status::Resolved));
        }
    }

    #[test]
    fn search_multi_domain_filter() {
        let store = build_test_index();
        // Search across myapp and wardwell domains
        let q = SearchQuery {
            query: "management knowledge".to_string(),
            domains: Some(vec!["myapp".to_string(), "wardwell".to_string()]),
            limit: 10,
            ..Default::default()
        };
        let results = store.search(&q);
        assert!(results.is_ok(), "{results:?}");
        let results = results.unwrap();
        // All results should be from one of the allowed domains
        for r in &results.results {
            if let Some(ref d) = r.frontmatter.domain {
                assert!(d == "myapp" || d == "wardwell",
                    "unexpected domain: {d}");
            }
        }
    }

    #[test]
    fn search_single_domain_in_vec() {
        let store = build_test_index();
        let q = SearchQuery {
            query: "project management".to_string(),
            domains: Some(vec!["myapp".to_string()]),
            limit: 5,
            ..Default::default()
        };
        let results = store.search(&q);
        assert!(results.is_ok(), "{results:?}");
        let results = results.unwrap();
        for r in &results.results {
            assert_eq!(r.frontmatter.domain.as_deref(), Some("myapp"));
        }
    }

    /// A vault whose tracker log holds COR-5 and COR-12 rows, plus a note
    /// that mentions neither key.
    fn build_tracker_index() -> (tempfile::TempDir, IndexStore) {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("work").join("claims");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("tracker.jsonl"),
            "{\"_schema\":\"tracker\",\"_version\":\"1.0\"}\n\
             {\"kind\":\"state_changed\",\"id\":\"a\",\"external_key\":\"COR-5\",\"title\":\"COR-5 Feedback tickets file into Linear: state Todo to Done\"}\n\
             {\"kind\":\"state_changed\",\"id\":\"b\",\"external_key\":\"COR-12\",\"title\":\"COR-12 Claims inbox shows wrong payer: state Todo to In Progress\"}\n",
        ).unwrap();
        std::fs::write(
            dir.path().join("work").join("notes.md"),
            "---\ntype: reference\nsummary: Unrelated notes\n---\nThe claims inbox shows the payer.\n",
        ).unwrap();
        let store = IndexStore::in_memory().unwrap();
        IndexBuilder::full_build(&store, dir.path(), None).unwrap();
        (dir, store)
    }

    fn file_hits(store: &IndexStore, query: &str) -> Vec<SearchResult> {
        store.search(&SearchQuery { query: query.into(), limit: 10, ..Default::default() }).unwrap().results
    }

    fn chunk_headings(store: &IndexStore, query: &str) -> Vec<String> {
        store
            .chunk_fts_search(query, 10, None)
            .unwrap()
            .into_iter()
            .filter_map(|(id, _)| store.get_chunk(&id).ok().and_then(|c| c.2))
            .collect()
    }

    #[test]
    fn ticket_key_finds_its_tracker_row() {
        let (_dir, store) = build_tracker_index();
        for (key, other) in [("COR-5", "COR-12"), ("COR-12", "COR-5")] {
            let hits = file_hits(&store, key);
            assert_eq!(hits.len(), 1, "{key}: {hits:?}");
            assert_eq!(hits[0].path, "work/claims/tracker.jsonl");
            let headings = chunk_headings(&store, key);
            assert_eq!(headings.len(), 1, "{key}: {headings:?}");
            assert!(headings[0].starts_with(&format!("{key} ")), "{key}: {headings:?}");
            assert!(!headings[0].starts_with(other));
        }
    }

    #[test]
    fn ticket_keys_combine_with_boolean_operators() {
        let (_dir, store) = build_tracker_index();
        assert_eq!(file_hits(&store, "COR-12 OR COR-13").len(), 1);
        assert_eq!(chunk_headings(&store, "COR-12 OR COR-13").len(), 1);
        assert_eq!(chunk_headings(&store, "COR-12 OR COR-5").len(), 2);
        assert!(chunk_headings(&store, "COR-12 AND COR-5").is_empty(), "no single row holds both");
        assert!(chunk_headings(&store, "COR-13").is_empty());
    }

    #[test]
    fn ticket_key_mixed_with_words_keeps_the_words_as_a_phrase() {
        let (_dir, store) = build_tracker_index();
        assert_eq!(chunk_headings(&store, "COR-12 wrong payer").len(), 1);
        assert!(chunk_headings(&store, "COR-5 wrong payer").is_empty());
    }

    #[test]
    fn ordinary_query_is_still_one_phrase() {
        let (_dir, store) = build_tracker_index();
        assert_eq!(match_expression("claims inbox"), "\"claims inbox\"");
        assert_eq!(match_expression("payer OR nothing"), "\"payer OR nothing\"");
        assert_eq!(match_expression("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(file_hits(&store, "claims inbox").len(), 2, "phrase matches the note and the log");
        assert!(file_hits(&store, "inbox claims").is_empty(), "word order still matters");
    }

    #[test]
    fn ticket_keys_reads_the_keys_a_search_quotes() {
        assert_eq!(ticket_keys("COR-12: fix inbox (cm-317, COR-12) for PROJ2-9"), vec!["COR-12", "CM-317", "PROJ2-9"]);
        assert_eq!(ticket_keys("[COR-5] Fix -COR-6- and COR-7-fix"), vec!["COR-5", "COR-6"]);
        assert!(ticket_keys("Bump serde to 1.0.200").is_empty());
        for key in ticket_keys("COR-12 and CM-3") {
            assert_eq!(match_expression(&key), format!("\"{key}\""));
        }
    }

    #[test]
    fn key_expression_quotes_keys_and_keeps_operators() {
        assert_eq!(match_expression("COR-12"), "\"COR-12\"");
        assert_eq!(match_expression("cm-317 OR PROJ2-9"), "\"cm-317\" OR \"PROJ2-9\"");
        assert_eq!(match_expression("COR-12 NOT COR-5"), "\"COR-12\" NOT \"COR-5\"");
        assert_eq!(match_expression("COR-12 wrong payer"), "\"COR-12\" \"wrong payer\"");
        assert_eq!(match_expression("OR COR-12 OR"), "\"OR\" \"COR-12\" \"OR\"");
        assert_eq!(match_expression("COR-12 OR OR COR-5"), "\"COR-12\" \"OR\" OR \"COR-5\"");
    }

    #[test]
    fn domain_inferred_from_path_when_frontmatter_empty() {
        let dir = tempfile::tempdir().unwrap();
        // File under work/ with no domain in frontmatter
        let work_dir = dir.path().join("work");
        std::fs::create_dir_all(&work_dir).unwrap();
        std::fs::write(
            work_dir.join("notes.md"),
            "---\ntype: reference\nstatus: active\nsummary: Work notes\n---\nSome work notes.\n",
        ).unwrap();

        let store = IndexStore::in_memory().unwrap();
        IndexBuilder::full_build(&store, dir.path(), None).unwrap();

        // Search with domain filter should find it
        let q = SearchQuery {
            query: "work notes".to_string(),
            domains: Some(vec!["work".to_string()]),
            limit: 5,
            ..Default::default()
        };
        let results = store.search(&q).unwrap();
        assert_eq!(results.total, 1);
        assert_eq!(results.results[0].frontmatter.domain.as_deref(), Some("work"));
    }
}
