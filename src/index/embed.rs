use crate::index::store::IndexError;
use std::path::Path;

/// Wrapper around fastembed for local text embedding.
pub struct Embedder {
    model: fastembed::TextEmbedding,
}

impl Embedder {
    /// Initialize with BGE-small-en-v1.5 model cached at the given directory.
    /// Model downloads automatically on first use (~33MB).
    pub fn new(cache_dir: &Path) -> Result<Self, IndexError> {
        let options = fastembed::InitOptions::new(fastembed::EmbeddingModel::BGESmallENV15)
            .with_cache_dir(cache_dir.to_path_buf())
            .with_show_download_progress(true);

        let model = fastembed::TextEmbedding::try_new(options)
            .map_err(|e| IndexError::Embedding(format!("Failed to load embedding model: {e}")))?;

        Ok(Self { model })
    }

    /// Embed a batch of texts. Returns one Vec<f32> per input text.
    pub fn embed_batch(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, IndexError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        self.model
            .embed(texts, None)
            .map_err(|e| IndexError::Embedding(format!("Embedding failed: {e}")))
    }

    /// Embed a single query string.
    pub fn embed_query(&mut self, query: &str) -> Result<Vec<f32>, IndexError> {
        let results = self
            .model
            .embed(vec![query.to_string()], None)
            .map_err(|e| IndexError::Embedding(format!("Query embedding failed: {e}")))?;

        results
            .into_iter()
            .next()
            .ok_or_else(|| IndexError::Embedding("No embedding returned".to_string()))
    }

    /// Returns the embedding dimension (384 for BGE-small-en-v1.5).
    pub fn dimension(&self) -> usize {
        384
    }
}
