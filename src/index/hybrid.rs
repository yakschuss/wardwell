use crate::index::embed::Embedder;
use crate::index::store::{IndexError, IndexStore};
use crate::vault::types::Frontmatter;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A single chunk result from hybrid search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkResult {
    pub path: String,
    pub chunk_index: usize,
    pub heading: Option<String>,
    pub body: String,
    pub score: f64,
    pub frontmatter: Frontmatter,
}

/// Hybrid search response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HybridResults {
    pub chunks: Vec<ChunkResult>,
    pub total: usize,
}

const RRF_K: f64 = 60.0;

/// Reciprocal Rank Fusion: score(doc) = sum(1 / (k + rank_i)) across all result sets.
fn rrf_fuse(result_sets: &[Vec<String>]) -> Vec<(String, f64)> {
    let mut scores: HashMap<String, f64> = HashMap::new();

    for result_set in result_sets {
        for (rank, id) in result_set.iter().enumerate() {
            *scores.entry(id.clone()).or_insert(0.0) += 1.0 / (RRF_K + rank as f64);
        }
    }

    let mut sorted: Vec<(String, f64)> = scores.into_iter().collect();
    sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    sorted
}

/// Run hybrid search: FTS5 on chunk_search + KNN on chunk_vec, fuse with RRF.
/// Returns chunk-level results with full text bodies and parent file frontmatter.
pub fn hybrid_search(
    store: &IndexStore,
    embedder: &mut Embedder,
    query: &str,
    limit: usize,
    domains: Option<&[String]>,
) -> Result<HybridResults, IndexError> {
    let fetch_count = limit * 3;

    // 1. FTS5 search on chunk_search
    let fts_results = store.chunk_fts_search(query, fetch_count, domains)?;
    let fts_ids: Vec<String> = fts_results.into_iter().map(|(id, _)| id).collect();

    // 2. Embed query and KNN search on chunk_vec
    let query_vec = embedder.embed_query(query)?;
    let vec_results = store.vector_search(&query_vec, fetch_count, domains)?;
    let vec_ids: Vec<String> = vec_results.into_iter().map(|(id, _)| id).collect();

    // 3. RRF fusion
    let fused = rrf_fuse(&[fts_ids, vec_ids]);

    // 4. Fetch chunk bodies + parent frontmatter for top N
    let mut chunks = Vec::new();
    for (chunk_id, score) in fused.into_iter().take(limit) {
        let (path, chunk_index, heading, body) = match store.get_chunk(&chunk_id) {
            Ok(c) => c,
            Err(_) => continue, // chunk may have been removed
        };

        // Get parent file frontmatter from vault_meta
        let frontmatter = store.get_frontmatter(&path).unwrap_or_default();

        chunks.push(ChunkResult {
            path,
            chunk_index,
            heading,
            body,
            score,
            frontmatter,
        });
    }

    let total = chunks.len();
    Ok(HybridResults { chunks, total })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_fuse_combines_rankings() {
        let set_a = vec!["doc1".to_string(), "doc2".to_string(), "doc3".to_string()];
        let set_b = vec!["doc2".to_string(), "doc3".to_string(), "doc1".to_string()];

        let fused = rrf_fuse(&[set_a, set_b]);

        // All three docs should appear
        assert_eq!(fused.len(), 3);

        // doc2 should rank highest: rank 1 in set_a (1/61) + rank 0 in set_b (1/60)
        assert_eq!(fused[0].0, "doc2");

        // All scores should be positive
        for (_, score) in &fused {
            assert!(*score > 0.0);
        }
    }

    #[test]
    fn rrf_fuse_single_set() {
        let set = vec!["a".to_string(), "b".to_string()];
        let fused = rrf_fuse(&[set]);

        assert_eq!(fused.len(), 2);
        assert!(fused[0].1 > fused[1].1); // higher rank = higher score
    }

    #[test]
    fn rrf_fuse_disjoint_sets() {
        let set_a = vec!["doc1".to_string()];
        let set_b = vec!["doc2".to_string()];

        let fused = rrf_fuse(&[set_a, set_b]);

        // Both docs, equal scores (both rank 0 in their respective set)
        assert_eq!(fused.len(), 2);
        let scores: Vec<f64> = fused.iter().map(|(_, s)| *s).collect();
        assert!((scores[0] - scores[1]).abs() < f64::EPSILON);
    }

    #[test]
    fn rrf_fuse_empty_sets() {
        let fused = rrf_fuse(&[Vec::new(), Vec::new()]);
        assert!(fused.is_empty());
    }
}
