use std::path::Path;

/// A chunk extracted from a vault file.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub index: usize,
    pub heading: Option<String>,
    pub body: String,
}

const MIN_CHUNK_CHARS: usize = 50;
const MAX_CHUNK_CHARS: usize = 2000;

/// Dispatch chunking based on file extension.
pub fn chunk_file(path: &Path, body: &str) -> Vec<Chunk> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("jsonl") => chunk_jsonl(body),
        _ => chunk_markdown(body),
    }
}

/// Split markdown body at ## headers.
/// Preamble (before first header) is chunk 0.
/// Tiny sections (<50 chars) merge into previous chunk.
/// Long sections (>2000 chars) split at paragraph boundaries.
pub fn chunk_markdown(body: &str) -> Vec<Chunk> {
    let mut raw_sections: Vec<(Option<String>, String)> = Vec::new();
    let mut current_heading: Option<String> = None;
    let mut current_body = String::new();

    for line in body.lines() {
        if line.starts_with("## ") {
            // Flush previous section
            if !current_body.is_empty() || current_heading.is_some() {
                raw_sections.push((current_heading.take(), current_body.trim().to_string()));
                current_body = String::new();
            }
            current_heading = Some(line.trim_start_matches('#').trim().to_string());
        } else {
            if !current_body.is_empty() {
                current_body.push('\n');
            }
            current_body.push_str(line);
        }
    }
    // Flush final section
    let trimmed = current_body.trim().to_string();
    if !trimmed.is_empty() || current_heading.is_some() {
        raw_sections.push((current_heading, trimmed));
    }

    // Remove empty sections
    raw_sections.retain(|(_, body)| !body.is_empty());

    if raw_sections.is_empty() {
        return Vec::new();
    }

    // Merge tiny sections into previous, split large sections at paragraphs
    let mut merged: Vec<(Option<String>, String)> = Vec::new();

    for (heading, body) in raw_sections {
        if body.len() < MIN_CHUNK_CHARS && !merged.is_empty() {
            // Merge into previous
            let prev = merged.last_mut();
            if let Some((_, prev_body)) = prev {
                prev_body.push_str("\n\n");
                if let Some(ref h) = heading {
                    prev_body.push_str("## ");
                    prev_body.push_str(h);
                    prev_body.push('\n');
                }
                prev_body.push_str(&body);
            }
        } else if body.len() > MAX_CHUNK_CHARS {
            // Split at paragraph boundaries
            let sub_chunks = split_at_paragraphs(&body, MAX_CHUNK_CHARS);
            for (i, sub) in sub_chunks.into_iter().enumerate() {
                let h = if i == 0 { heading.clone() } else { heading.as_ref().map(|h| format!("{h} (cont.)")) };
                merged.push((h, sub));
            }
        } else {
            merged.push((heading, body));
        }
    }

    merged
        .into_iter()
        .enumerate()
        .map(|(i, (heading, body))| Chunk { index: i, heading, body })
        .collect()
}

/// Split JSONL: each non-schema line is a chunk.
pub fn chunk_jsonl(body: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut index = 0;

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Skip schema header lines
        if trimmed.starts_with("{\"_schema\"") {
            continue;
        }
        // Extract title from JSON if present
        let heading = serde_json::from_str::<serde_json::Value>(trimmed)
            .ok()
            .and_then(|v| v["title"].as_str().map(String::from));

        chunks.push(Chunk {
            index,
            heading,
            body: trimmed.to_string(),
        });
        index += 1;
    }

    chunks
}

/// Split text at paragraph boundaries (`\n\n`), keeping chunks under max_chars.
fn split_at_paragraphs(text: &str, max_chars: usize) -> Vec<String> {
    let paragraphs: Vec<&str> = text.split("\n\n").collect();
    let mut result = Vec::new();
    let mut current = String::new();

    for para in paragraphs {
        if current.is_empty() {
            current.push_str(para);
        } else if current.len() + 2 + para.len() > max_chars {
            result.push(current.trim().to_string());
            current = para.to_string();
        } else {
            current.push_str("\n\n");
            current.push_str(para);
        }
    }

    if !current.trim().is_empty() {
        result.push(current.trim().to_string());
    }

    // If no paragraph breaks found, just return the whole text
    if result.is_empty() {
        result.push(text.to_string());
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_markdown_splits_at_headers() {
        let body = "## Introduction\nSome intro text here that is long enough to be a standalone chunk on its own right.\n\n## Architecture\nThe system uses a layered approach with multiple components working together in concert.\n\n## Testing\nAll integration and unit tests pass with full coverage across the entire codebase.";
        let chunks = chunk_markdown(body);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].heading.as_deref(), Some("Introduction"));
        assert_eq!(chunks[1].heading.as_deref(), Some("Architecture"));
        assert_eq!(chunks[2].heading.as_deref(), Some("Testing"));
    }

    #[test]
    fn chunk_markdown_preamble_is_chunk_zero() {
        let body = "This is preamble text before any header, long enough to stand alone as its own chunk.\n\n## First Section\nSection body with enough content to not be merged into the preamble chunk above.";
        let chunks = chunk_markdown(body);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].heading, None);
        assert!(chunks[0].body.contains("preamble"));
        assert_eq!(chunks[1].heading.as_deref(), Some("First Section"));
    }

    #[test]
    fn chunk_markdown_merges_tiny_sections() {
        let body = "## Big Section\nThis is a substantial section with enough content to stand alone as a chunk.\n\n## Tiny\nHi";
        let chunks = chunk_markdown(body);
        // Tiny section (<50 chars) merged into previous
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].body.contains("Tiny"));
        assert!(chunks[0].body.contains("Hi"));
    }

    #[test]
    fn chunk_markdown_splits_large_sections() {
        // Use paragraph breaks so split_at_paragraphs can find boundaries
        let paras: Vec<String> = (0..30).map(|i| format!("Paragraph {i} with enough content to make it meaningful and substantial in the overall document context.")).collect();
        let big_body = format!("## Big\n{}", paras.join("\n\n"));
        assert!(big_body.len() > MAX_CHUNK_CHARS, "test body must exceed max: {}", big_body.len());
        let chunks = chunk_markdown(&big_body);
        assert!(chunks.len() > 1, "should split large section, got {} chunks", chunks.len());
        for chunk in &chunks {
            assert!(chunk.body.len() <= MAX_CHUNK_CHARS + 200, "chunk too large: {}", chunk.body.len());
        }
    }

    #[test]
    fn chunk_markdown_empty_body() {
        let chunks = chunk_markdown("");
        assert!(chunks.is_empty());
    }

    #[test]
    fn chunk_markdown_no_headers() {
        let body = "Just plain text with no headers at all.\nMultiple lines of content.";
        let chunks = chunk_markdown(body);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].heading, None);
    }

    #[test]
    fn chunk_jsonl_skips_schema() {
        let body = "{\"_schema\": \"history\", \"_version\": \"1.0\"}\n{\"title\": \"First entry\", \"body\": \"content\"}\n{\"title\": \"Second entry\", \"body\": \"more\"}";
        let chunks = chunk_jsonl(body);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].heading.as_deref(), Some("First entry"));
        assert_eq!(chunks[1].heading.as_deref(), Some("Second entry"));
    }

    #[test]
    fn chunk_jsonl_skips_empty_lines() {
        let body = "{\"title\": \"A\"}\n\n{\"title\": \"B\"}\n";
        let chunks = chunk_jsonl(body);
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn chunk_file_dispatches_by_extension() {
        let md_chunks = chunk_file(Path::new("test.md"), "## Header\nBody text here.");
        assert_eq!(md_chunks[0].heading.as_deref(), Some("Header"));

        let jsonl_chunks = chunk_file(Path::new("history.jsonl"), "{\"title\": \"entry\"}");
        assert_eq!(jsonl_chunks[0].heading.as_deref(), Some("entry"));
    }

    #[test]
    fn chunk_indexes_are_sequential() {
        let body = "## A\nContent A.\n\n## B\nContent B.\n\n## C\nContent C is long enough to be its own chunk.";
        let chunks = chunk_markdown(body);
        for (i, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.index, i);
        }
    }

    #[test]
    fn split_paragraphs_respects_max() {
        let text = "Para one.\n\nPara two.\n\nPara three.";
        let result = split_at_paragraphs(text, 25);
        assert!(result.len() > 1);
        for part in &result {
            assert!(part.len() <= 25 + 20, "part too large: {}", part.len());
        }
    }
}
