//! Chunker: split a CompressedTranscript into non-overlapping chunks at message boundaries.
//!
//! Target: ~8K tokens per chunk (1 token ≈ 4 chars).
//! Each chunk carries its index and references to neighbouring chunks.
//! No overlap — Stage B already receives prev/next chunk text as context.

use crate::compress::CompressedTranscript;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single chunk from a compressed transcript.
#[derive(Debug, Clone)]
pub struct Chunk {
    /// Zero-based index in the full chunk sequence.
    pub index: usize,
    /// The text content of this chunk.
    pub text: String,
    /// Estimated token count for this chunk's text.
    pub token_estimate: usize,
    /// Index of the previous chunk (for context windows), if any.
    pub prev_index: Option<usize>,
    /// Index of the next chunk, if any.
    pub next_index: Option<usize>,
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

const TARGET_TOKENS: usize = 8_000;
const CHARS_PER_TOKEN: usize = 4;

const TARGET_CHARS: usize = TARGET_TOKENS * CHARS_PER_TOKEN;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Split a compressed transcript into chunks at message boundaries.
///
/// Chunks target ~4K tokens with no overlap (Stage B provides prev/next context).
pub fn chunk_transcript(transcript: &CompressedTranscript) -> Vec<Chunk> {
    chunk_text(&transcript.text)
}

/// Split arbitrary text into chunks at line boundaries (message boundary heuristic).
///
/// Exposed as `pub` so stage_a / tests can call it directly.
pub fn chunk_text(text: &str) -> Vec<Chunk> {
    if text.is_empty() {
        return vec![];
    }

    // Split text at message boundaries: lines starting with "[USER]" or "[ASSISTANT]"
    // or any line starting with "[" followed by uppercase letters and "]".
    // We collect paragraphs/messages as "units" and then pack them into chunks.
    let units = split_into_units(text);

    if units.is_empty() {
        // Fallback: treat whole text as a single chunk
        return vec![Chunk {
            index: 0,
            text: text.to_string(),
            token_estimate: estimate_tokens(text),
            prev_index: None,
            next_index: None,
        }];
    }

    // Pack units into chunks of ~TARGET_CHARS each
    let raw_chunks = pack_units_into_chunks(&units);

    // Build final Chunk structs with index/prev/next links
    let total = raw_chunks.len();
    raw_chunks
        .into_iter()
        .enumerate()
        .map(|(i, text)| {
            let token_estimate = estimate_tokens(&text);
            Chunk {
                index: i,
                text,
                token_estimate,
                prev_index: if i > 0 { Some(i - 1) } else { None },
                next_index: if i + 1 < total { Some(i + 1) } else { None },
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Implementation helpers
// ---------------------------------------------------------------------------

/// Estimate token count: 1 token ≈ 4 chars.
pub fn estimate_tokens(s: &str) -> usize {
    s.len().div_ceil(4)
}

/// Split text into "units" at message boundaries.
/// A unit is a run of lines belonging to one speaker turn.
fn split_into_units(text: &str) -> Vec<String> {
    let mut units: Vec<String> = Vec::new();
    let mut current = String::new();

    for line in text.lines() {
        // Detect message boundary: line like "[USER]" or "[ASSISTANT]"
        let trimmed = line.trim();
        let is_boundary = is_message_boundary(trimmed);

        if is_boundary && !current.trim().is_empty() {
            units.push(current.trim_end().to_string());
            current = String::new();
        }

        current.push_str(line);
        current.push('\n');
    }

    if !current.trim().is_empty() {
        units.push(current.trim_end().to_string());
    }

    units
}

fn is_message_boundary(line: &str) -> bool {
    // Matches lines like [USER], [ASSISTANT], [HUMAN], etc.
    if line.starts_with('[')
        && let Some(end) = line.find(']')
    {
        let label = &line[1..end];
        return label
            .chars()
            .all(|c| c.is_uppercase() || c == '_' || c == ' ');
    }
    false
}

/// Pack units into raw chunks by greedy bin-packing up to TARGET_CHARS.
fn pack_units_into_chunks(units: &[String]) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0usize;

    for unit in units {
        let unit_chars = unit.len();

        // If a single unit exceeds the target, we still emit it as a standalone chunk
        // (don't split mid-message — message boundaries are respected).
        if current_chars + unit_chars > TARGET_CHARS && !current.is_empty() {
            chunks.push(current.trim_end().to_string());
            current = String::new();
            current_chars = 0;
        }

        current.push_str(unit);
        current.push('\n');
        current_chars += unit_chars + 1;
    }

    if !current.trim().is_empty() {
        chunks.push(current.trim_end().to_string());
    }

    chunks
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_transcript(n_messages: usize, chars_per_message: usize) -> String {
        (0..n_messages)
            .map(|i| {
                let role = if i % 2 == 0 { "USER" } else { "ASSISTANT" };
                let content = "x".repeat(chars_per_message);
                format!("[{role}]\n{content}")
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    #[test]
    fn test_single_small_transcript_one_chunk() {
        let text = "[USER]\nHello world\n\n[ASSISTANT]\nHi there";
        let chunks = chunk_text(text);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].index, 0);
        assert!(chunks[0].prev_index.is_none());
        assert!(chunks[0].next_index.is_none());
    }

    #[test]
    fn test_large_transcript_splits_into_multiple_chunks() {
        // Each message ~2K chars → 2 messages per chunk → need multiple chunks
        let text = make_transcript(20, 2_000);
        let chunks = chunk_text(&text);
        assert!(
            chunks.len() > 1,
            "Expected multiple chunks, got {}",
            chunks.len()
        );
    }

    #[test]
    fn test_chunk_size_within_bounds() {
        let text = make_transcript(20, 2_000);
        let chunks = chunk_text(&text);
        for chunk in &chunks {
            // Allow some slack for boundary headers
            assert!(
                chunk.token_estimate <= TARGET_TOKENS * 2,
                "Chunk {} too large: {} tokens",
                chunk.index,
                chunk.token_estimate
            );
        }
    }

    #[test]
    fn test_no_overlap_between_chunks() {
        // With overlap removed, chunks should not contain overlap markers
        let text = make_transcript(6, 5_000);
        let chunks = chunk_text(&text);
        for chunk in &chunks {
            assert!(
                !chunk.text.contains("overlap from previous chunk"),
                "Chunk {} should not contain overlap marker",
                chunk.index,
            );
        }
    }

    #[test]
    fn test_chunk_indices_and_links() {
        let text = make_transcript(20, 2_000);
        let chunks = chunk_text(&text);
        let n = chunks.len();
        for (i, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.index, i);
            if i == 0 {
                assert!(chunk.prev_index.is_none());
            } else {
                assert_eq!(chunk.prev_index, Some(i - 1));
            }
            if i + 1 == n {
                assert!(chunk.next_index.is_none());
            } else {
                assert_eq!(chunk.next_index, Some(i + 1));
            }
        }
    }

    #[test]
    fn test_empty_transcript_returns_empty_chunks() {
        let chunks = chunk_text("");
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_token_estimate_accuracy() {
        let text = "a".repeat(400); // 400 chars = 100 tokens
        let tokens = estimate_tokens(&text);
        assert_eq!(tokens, 100);
    }
}
