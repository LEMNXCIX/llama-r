//! Simple character-based text chunker for RAG ingest (MVP).

#[derive(Debug, Clone)]
pub struct ChunkConfig {
    pub max_chars: usize,
    pub overlap_chars: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            max_chars: 1200,
            overlap_chars: 150,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChunk {
    pub id: String,
    pub text: String,
    pub index: usize,
}

/// Split `text` into overlapping windows of at most `max_chars`.
///
/// Chunk ids are stable: `{source_key}#chunk-{index}` so re-ingest upserts
/// replace rather than duplicate.
pub fn chunk_text(source_key: &str, text: &str, cfg: &ChunkConfig) -> Vec<TextChunk> {
    let max_chars = cfg.max_chars.max(1);
    let overlap = cfg.overlap_chars.min(max_chars.saturating_sub(1));

    // Normalize newlines to `\n` and trim outer whitespace for empty check.
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    if normalized.trim().is_empty() {
        return Vec::new();
    }

    let chars: Vec<char> = normalized.chars().collect();
    let mut chunks = Vec::new();
    let mut start = 0usize;
    let mut index = 0usize;

    while start < chars.len() {
        let end = (start + max_chars).min(chars.len());
        let slice: String = chars[start..end].iter().collect();
        let trimmed = slice.trim();
        if !trimmed.is_empty() {
            chunks.push(TextChunk {
                id: format!("{source_key}#chunk-{index}"),
                text: trimmed.to_string(),
                index,
            });
            index += 1;
        }

        if end >= chars.len() {
            break;
        }

        // Advance with overlap; ensure forward progress.
        let next = end.saturating_sub(overlap);
        start = if next <= start { end } else { next };
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_text_respects_max_and_overlap() {
        let text = "abcdefghij".repeat(30); // 300 chars
        let cfg = ChunkConfig {
            max_chars: 100,
            overlap_chars: 20,
        };
        let chunks = chunk_text("doc1", &text, &cfg);
        assert!(chunks.len() >= 3);
        for chunk in &chunks {
            assert!(chunk.text.chars().count() <= 100);
            assert!(chunk.id.starts_with("doc1#chunk-"));
        }
        // Stable ids by index
        assert_eq!(chunks[0].id, "doc1#chunk-0");
        assert_eq!(chunks[1].id, "doc1#chunk-1");
    }

    #[test]
    fn chunk_text_skips_empty() {
        let cfg = ChunkConfig::default();
        assert!(chunk_text("x", "   \n\n  ", &cfg).is_empty());
        assert!(chunk_text("x", "", &cfg).is_empty());
    }

    #[test]
    fn chunk_text_single_small_doc() {
        let cfg = ChunkConfig {
            max_chars: 1200,
            overlap_chars: 150,
        };
        let chunks = chunk_text("memo", "hello world", &cfg);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "hello world");
        assert_eq!(chunks[0].id, "memo#chunk-0");
    }

    #[test]
    fn chunk_ids_stable_across_calls() {
        let cfg = ChunkConfig {
            max_chars: 50,
            overlap_chars: 10,
        };
        let text = "alpha beta gamma delta epsilon zeta eta theta";
        let a = chunk_text("src", text, &cfg);
        let b = chunk_text("src", text, &cfg);
        assert_eq!(a, b);
    }
}
