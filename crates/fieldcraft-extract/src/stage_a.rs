//! Stage A: Haiku-based chunk classifier.
//!
//! For each chunk, asks Haiku whether it contains durable knowledge worth extracting.
//! Returns HIGH or LOW. HIGH chunks are passed to Stage B.
//!
//! Error handling: malformed JSON → retry once. If retry fails, ALL chunks are
//! marked HIGH (expensive but safe — no knowledge loss).

use crate::anthropic::LlmClient;
use crate::chunker::Chunk;
use crate::util::extract_json_object;
use fieldcraft_core::TrError;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::task::JoinSet;

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ClassificationResult {
    pub worth_remembering: bool,
    #[serde(rename = "type")]
    pub knowledge_type: Option<String>,
    pub summary: String,
}

#[derive(Debug, Clone)]
pub struct StageAResult {
    pub chunk_index: usize,
    pub classification: ClassificationResult,
    pub is_high: bool,
}

// ---------------------------------------------------------------------------
// System prompt
// ---------------------------------------------------------------------------

const STAGE_A_SYSTEM: &str = "\
You are a knowledge classifier for a developer AI lesson system. \
Your job is to determine whether a conversation chunk contains durable knowledge worth remembering.

Classify as HIGH (worth_remembering: true) if the chunk contains ANY of:
- A decision with reasoning (\"we chose X because Y\")
- A gotcha or failure mode (\"X breaks if you do Y\")
- A working procedure with steps (\"to do X, do A then B then C\")
- A debugging discovery (\"the root cause was X\")
- A configuration insight or environment-specific finding

Choose the correct type:
- \"claim\": A factual statement, decision, or gotcha (\"PostgreSQL advisory locks deadlock when pool > 20\")
- \"procedure\": A sequence of steps to accomplish something (\"To deploy: build, test, tag, push, verify\")
- \"episode\": A debugging narrative or problem-solving journey (\"Tried approach A, failed because B, switched to C\")

Examples of PROCEDURE (type \"procedure\"):
- Setting up a development environment or tool
- A deploy or release workflow
- Steps to reproduce or fix a bug
- Configuration or migration instructions

Examples of EPISODE (type \"episode\"):
- Root cause analysis of a crash or bug
- An investigation that tried multiple approaches before finding a solution
- A performance debugging session with measurements

Classify as LOW (worth_remembering: false) if the chunk ONLY contains:
- Routine file reading or editing with no insight
- Boilerplate code generation
- Simple Q&A with no durable insight
- Navigation or exploration with no conclusion

Respond with ONLY a JSON object, no other text:
{\"worth_remembering\": <bool>, \"type\": \"claim\"|\"procedure\"|\"episode\"|null, \"summary\": \"<one sentence>\"}";

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Classify a single chunk using Haiku. Returns the classification result.
///
/// On malformed JSON: retries once. On second failure: treats as HIGH.
pub async fn classify_chunk(
    client: &LlmClient,
    model: &str,
    chunk: &Chunk,
) -> Result<StageAResult, TrError> {
    let user_prompt = build_user_prompt(chunk);

    let raw = client
        .message(model, STAGE_A_SYSTEM, &user_prompt, 256)
        .await?;

    match parse_classification(&raw) {
        Ok(classification) => {
            let is_high = classification.worth_remembering;
            Ok(StageAResult {
                chunk_index: chunk.index,
                classification,
                is_high,
            })
        }
        Err(_first_parse_err) => {
            // Retry once with an explicit reminder about JSON format
            let retry_prompt = format!(
                "{}\n\nRemember: respond with ONLY valid JSON, nothing else.",
                user_prompt
            );
            let raw2 = client
                .message(model, STAGE_A_SYSTEM, &retry_prompt, 256)
                .await?;

            match parse_classification(&raw2) {
                Ok(classification) => {
                    let is_high = classification.worth_remembering;
                    Ok(StageAResult {
                        chunk_index: chunk.index,
                        classification,
                        is_high,
                    })
                }
                Err(_) => {
                    // Both attempts failed: mark HIGH (safe fallback — no knowledge loss)
                    Ok(StageAResult {
                        chunk_index: chunk.index,
                        classification: ClassificationResult {
                            worth_remembering: true,
                            knowledge_type: None,
                            summary: "Classification failed; marked HIGH for safety.".to_string(),
                        },
                        is_high: true,
                    })
                }
            }
        }
    }
}

/// Classify all chunks. Uses batched prompts for CLI backend (5 chunks per call)
/// or parallel individual calls for API backend (10 concurrent).
///
/// If parsing fails for all retries on a chunk, that chunk is marked HIGH.
pub async fn run_stage_a(
    client: Arc<LlmClient>,
    model: &str,
    chunks: &[Chunk],
) -> Vec<StageAResult> {
    if client.is_cli() {
        run_stage_a_batched(&client, model, chunks).await
    } else {
        run_stage_a_parallel(client, model, chunks).await
    }
}

/// CLI backend: pack multiple chunks into one prompt to amortize CLI startup cost.
async fn run_stage_a_batched(
    client: &LlmClient,
    model: &str,
    chunks: &[Chunk],
) -> Vec<StageAResult> {
    const BATCH_SIZE: usize = 5;

    let mut results: Vec<StageAResult> = Vec::with_capacity(chunks.len());

    for batch_start in (0..chunks.len()).step_by(BATCH_SIZE) {
        let batch_end = (batch_start + BATCH_SIZE).min(chunks.len());
        let batch = &chunks[batch_start..batch_end];

        let batch_results = classify_chunk_batch(client, model, batch).await;
        results.extend(batch_results);
    }

    results.sort_by_key(|r| r.chunk_index);
    results
}

/// API backend: parallel individual calls (already fast, no batching needed).
async fn run_stage_a_parallel(
    client: Arc<LlmClient>,
    model: &str,
    chunks: &[Chunk],
) -> Vec<StageAResult> {
    const MAX_CONCURRENT: usize = 10;

    let model = model.to_string();

    let mut results: Vec<StageAResult> = Vec::with_capacity(chunks.len());
    let mut cursor = 0usize;

    while cursor < chunks.len() {
        let batch_end = (cursor + MAX_CONCURRENT).min(chunks.len());
        let batch = &chunks[cursor..batch_end];

        let mut join_set: JoinSet<StageAResult> = JoinSet::new();

        for chunk in batch {
            let chunk = chunk.clone();
            let client = Arc::clone(&client);
            let model = model.clone();

            join_set.spawn(async move {
                match classify_chunk(&client, &model, &chunk).await {
                    Ok(result) => result,
                    Err(_) => StageAResult {
                        chunk_index: chunk.index,
                        classification: ClassificationResult {
                            worth_remembering: true,
                            knowledge_type: None,
                            summary: "API error; marked HIGH for safety.".to_string(),
                        },
                        is_high: true,
                    },
                }
            });
        }

        while let Some(res) = join_set.join_next().await {
            if let Ok(result) = res {
                results.push(result);
            }
        }

        cursor = batch_end;
    }

    results.sort_by_key(|r| r.chunk_index);
    results
}

/// Classify multiple chunks in a single LLM call. Parse the response as
/// a JSON array of classification objects.
async fn classify_chunk_batch(
    client: &LlmClient,
    model: &str,
    chunks: &[Chunk],
) -> Vec<StageAResult> {
    let prompt = build_batch_prompt(chunks);

    let raw = match client.message(model, STAGE_A_SYSTEM, &prompt, 1024).await {
        Ok(r) => r,
        Err(_) => {
            // API error: mark all HIGH for safety
            return chunks
                .iter()
                .map(|c| StageAResult {
                    chunk_index: c.index,
                    classification: ClassificationResult {
                        worth_remembering: true,
                        knowledge_type: None,
                        summary: "Batch API error; marked HIGH for safety.".to_string(),
                    },
                    is_high: true,
                })
                .collect();
        }
    };

    // Try to parse as JSON array
    match parse_batch_response(&raw, chunks) {
        Ok(results) => results,
        Err(_) => {
            // Parse failed: mark all HIGH for safety
            chunks
                .iter()
                .map(|c| StageAResult {
                    chunk_index: c.index,
                    classification: ClassificationResult {
                        worth_remembering: true,
                        knowledge_type: None,
                        summary: "Batch parse failed; marked HIGH for safety.".to_string(),
                    },
                    is_high: true,
                })
                .collect()
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_user_prompt(chunk: &Chunk) -> String {
    format!(
        "Classify this conversation chunk (chunk {} of many):\n\n---\n{}\n---",
        chunk.index + 1,
        chunk.text
    )
}

fn build_batch_prompt(chunks: &[Chunk]) -> String {
    let mut prompt = format!(
        "Classify each of the following {} conversation chunks.\n\
         For EACH chunk, produce one JSON object. Respond with a JSON array containing \
         one object per chunk, in order.\n\n",
        chunks.len()
    );

    for chunk in chunks {
        prompt.push_str(&format!(
            "=== Chunk {} ===\n{}\n\n",
            chunk.index + 1,
            chunk.text
        ));
    }

    prompt.push_str(&format!(
        "Respond with ONLY a JSON array of {} objects, one per chunk, in order:\n\
         [{{\"worth_remembering\": <bool>, \"type\": \"claim\"|\"procedure\"|\"episode\"|null, \"summary\": \"...\"}}, ...]",
        chunks.len()
    ));

    prompt
}

fn parse_batch_response(raw: &str, chunks: &[Chunk]) -> Result<Vec<StageAResult>, TrError> {
    // Find the JSON array in the response
    let start = raw.find('[').ok_or_else(|| TrError::Parse {
        message: "No JSON array found in batch response".to_string(),
    })?;
    let end = raw.rfind(']').ok_or_else(|| TrError::Parse {
        message: "No closing bracket found in batch response".to_string(),
    })?;
    let json_str = &raw[start..=end];

    let classifications: Vec<ClassificationResult> =
        serde_json::from_str(json_str).map_err(|e| TrError::Parse {
            message: format!("Stage A batch: failed to parse JSON array: {e}\nRaw: {json_str}"),
        })?;

    // If we got fewer results than chunks, parse what we can
    let results: Vec<StageAResult> = chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            if let Some(classification) = classifications.get(i) {
                let is_high = classification.worth_remembering;
                StageAResult {
                    chunk_index: chunk.index,
                    classification: classification.clone(),
                    is_high,
                }
            } else {
                // Missing result: mark HIGH for safety
                StageAResult {
                    chunk_index: chunk.index,
                    classification: ClassificationResult {
                        worth_remembering: true,
                        knowledge_type: None,
                        summary: "Missing from batch response; marked HIGH.".to_string(),
                    },
                    is_high: true,
                }
            }
        })
        .collect();

    Ok(results)
}

fn parse_classification(raw: &str) -> Result<ClassificationResult, TrError> {
    // Try to extract JSON from the response (model may include surrounding text)
    let json_str = extract_json_object(raw).unwrap_or(raw);
    serde_json::from_str::<ClassificationResult>(json_str).map_err(|e| TrError::Parse {
        message: format!("Stage A: failed to parse classification JSON: {e}\nRaw: {raw}"),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_chunk(index: usize, text: &str) -> Chunk {
        Chunk {
            index,
            text: text.to_string(),
            token_estimate: text.len() / 4,
            prev_index: None,
            next_index: None,
        }
    }

    #[test]
    fn test_parse_high_classification() {
        let raw = r#"{"worth_remembering": true, "type": "claim", "summary": "PostgreSQL advisory locks deadlock when pool > 20."}"#;
        let result = parse_classification(raw).unwrap();
        assert!(result.worth_remembering);
        assert_eq!(result.knowledge_type.as_deref(), Some("claim"));
        assert!(!result.summary.is_empty());
    }

    #[test]
    fn test_parse_low_classification() {
        let raw =
            r#"{"worth_remembering": false, "type": null, "summary": "Routine file reading."}"#;
        let result = parse_classification(raw).unwrap();
        assert!(!result.worth_remembering);
        assert!(result.knowledge_type.is_none());
    }

    #[test]
    fn test_parse_with_surrounding_text() {
        let raw = r#"Here is my answer: {"worth_remembering": true, "type": "procedure", "summary": "Deploy procedure."} Hope that helps!"#;
        let result = parse_classification(raw).unwrap();
        assert!(result.worth_remembering);
    }

    #[test]
    fn test_parse_malformed_returns_error() {
        let raw = "not json at all";
        assert!(parse_classification(raw).is_err());
    }

    #[test]
    fn test_build_user_prompt_includes_chunk_index() {
        let chunk = make_chunk(2, "some content");
        let prompt = build_user_prompt(&chunk);
        assert!(prompt.contains("chunk 3"));
        assert!(prompt.contains("some content"));
    }

    #[test]
    fn test_extract_json_object() {
        let s = "Some text {\"key\": \"value\"} more text";
        let extracted = extract_json_object(s).unwrap();
        assert_eq!(extracted, r#"{"key": "value"}"#);
    }

    #[test]
    fn test_extract_json_object_none_if_no_braces() {
        assert!(extract_json_object("no json here").is_none());
    }

    #[test]
    fn test_extract_json_object_nested() {
        let s = r#"Sure! {"outer": {"inner": "value"}} done"#;
        let extracted = extract_json_object(s).unwrap();
        assert_eq!(extracted, r#"{"outer": {"inner": "value"}}"#);
    }

    #[test]
    fn test_extract_json_object_stops_at_first_complete_object() {
        // With rfind approach, this would return the wrong span spanning both objects.
        // With depth-counting, it correctly stops at the first complete object.
        let s = r#"{"a": 1} {"b": 2}"#;
        let extracted = extract_json_object(s).unwrap();
        assert_eq!(extracted, r#"{"a": 1}"#);
    }

    #[test]
    fn test_parse_batch_response() {
        let chunks = vec![
            make_chunk(0, "chunk 0"),
            make_chunk(1, "chunk 1"),
            make_chunk(2, "chunk 2"),
        ];
        let raw = r#"[
            {"worth_remembering": true, "type": "claim", "summary": "First"},
            {"worth_remembering": false, "type": null, "summary": "Second"},
            {"worth_remembering": true, "type": "procedure", "summary": "Third"}
        ]"#;
        let results = parse_batch_response(raw, &chunks).unwrap();
        assert_eq!(results.len(), 3);
        assert!(results[0].is_high);
        assert!(!results[1].is_high);
        assert!(results[2].is_high);
        assert_eq!(
            results[2].classification.knowledge_type.as_deref(),
            Some("procedure")
        );
    }

    #[test]
    fn test_parse_batch_response_with_surrounding_text() {
        let chunks = vec![make_chunk(0, "chunk 0")];
        let raw = r#"Here are the results: [{"worth_remembering": true, "type": "claim", "summary": "Test"}] Done!"#;
        let results = parse_batch_response(raw, &chunks).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].is_high);
    }

    #[test]
    fn test_parse_batch_response_fewer_results_marks_remaining_high() {
        let chunks = vec![make_chunk(0, "chunk 0"), make_chunk(1, "chunk 1")];
        let raw = r#"[{"worth_remembering": false, "type": null, "summary": "Only one"}]"#;
        let results = parse_batch_response(raw, &chunks).unwrap();
        assert_eq!(results.len(), 2);
        assert!(!results[0].is_high);
        assert!(results[1].is_high); // missing → HIGH for safety
    }

    #[test]
    fn test_build_batch_prompt_format() {
        let chunks = vec![make_chunk(0, "content A"), make_chunk(1, "content B")];
        let prompt = build_batch_prompt(&chunks);
        assert!(prompt.contains("=== Chunk 1 ==="));
        assert!(prompt.contains("=== Chunk 2 ==="));
        assert!(prompt.contains("content A"));
        assert!(prompt.contains("content B"));
        assert!(prompt.contains("JSON array of 2 objects"));
    }

    #[test]
    fn test_stage_a_system_prompt_contains_required_categories() {
        // Verify the system prompt covers all required classification cases
        assert!(STAGE_A_SYSTEM.contains("decision with reasoning"));
        assert!(STAGE_A_SYSTEM.contains("failure mode"));
        assert!(STAGE_A_SYSTEM.contains("working procedure"));
        assert!(STAGE_A_SYSTEM.contains("debugging discovery"));
        assert!(STAGE_A_SYSTEM.contains("worth_remembering"));
        assert!(STAGE_A_SYSTEM.contains("claim"));
        assert!(STAGE_A_SYSTEM.contains("procedure"));
        assert!(STAGE_A_SYSTEM.contains("episode"));
    }
}
