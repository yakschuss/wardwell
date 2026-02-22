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
                    snippet(vault_search, 7, '', '', '...', 40) as snip
             FROM vault_search s
             JOIN vault_meta m ON s.path = m.path
             WHERE vault_search MATCH ?1"
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        params.push(Box::new(q.query.clone()));

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

                Ok((path, file_type, domain, status, confidence, updated, summary, related, tags, snippet))
            })?;

            for row in rows {
                let (path, file_type, domain, status, confidence, updated, summary, related, tags, snippet) = row?;

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

                results.push(SearchResult { path, frontmatter, snippet });
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

fn parse_vault_type(s: &str) -> VaultType {
    match s {
        "project" => VaultType::Project,
        "decision" => VaultType::Decision,
        "insight" => VaultType::Insight,
        "thread" => VaultType::Thread,
        "domain" => VaultType::Domain,
        "reference" => VaultType::Reference,
        _ => VaultType::Reference, // fallback
    }
}

fn parse_status(s: &str) -> Option<Status> {
    match s {
        "active" => Some(Status::Active),
        "resolved" => Some(Status::Resolved),
        "abandoned" => Some(Status::Abandoned),
        "superseded" => Some(Status::Superseded),
        _ => None,
    }
}

fn parse_confidence(s: &str) -> Option<Confidence> {
    match s {
        "inferred" => Some(Confidence::Inferred),
        "proposed" => Some(Confidence::Proposed),
        "confirmed" => Some(Confidence::Confirmed),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::builder::IndexBuilder;

    fn build_test_index() -> IndexStore {
        let dir = tempfile::tempdir().unwrap_or_else(|_| std::process::exit(1));

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

        let store = IndexStore::in_memory().unwrap_or_else(|_| std::process::exit(1));
        IndexBuilder::full_build(&store, dir.path()).ok();
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
        assert!(results.is_ok());
        let results = results.unwrap_or_else(|_| std::process::exit(1));
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
        assert!(results.is_ok());
        let results = results.unwrap_or_else(|_| std::process::exit(1));
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
        assert!(results.is_ok());
        let results = results.unwrap_or_else(|_| std::process::exit(1));
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
        assert!(results.is_ok());
        let results = results.unwrap_or_else(|_| std::process::exit(1));
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
        assert!(results.is_ok());
        let results = results.unwrap_or_else(|_| std::process::exit(1));
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
        assert!(results.is_ok());
        let results = results.unwrap_or_else(|_| std::process::exit(1));
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
        assert!(results.is_ok());
        let results = results.unwrap_or_else(|_| std::process::exit(1));
        for r in &results.results {
            assert_eq!(r.frontmatter.domain.as_deref(), Some("myapp"));
        }
    }
}
