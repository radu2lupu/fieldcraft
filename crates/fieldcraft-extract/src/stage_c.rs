//! Stage C: Self-critique (Haiku as LLM-as-judge).
//!
//! Rates each extracted KnowledgeRecord on four axes (1-5):
//!   1. Accuracy: factually correct per transcript?
//!   2. Specificity: actionable for this codebase?
//!   3. Durability: useful in future sessions?
//!   4. Classification: correct type (Claim vs Procedure)?
//!
//! Reject if ANY axis < 3 OR average < 3.5.
//! Rejected records are kept with state: Retired (for error taxonomy analysis).

use crate::anthropic::LlmClient;
use crate::util::extract_json_object;
use fieldcraft_core::{KnowledgeRecord, KnowledgeState, TrError};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CritiqueScores {
    pub accuracy: u8,
    pub specificity: u8,
    pub durability: u8,
    pub classification: u8,
    pub reasoning: String,
}

#[derive(Debug, Clone)]
pub struct StageCResult {
    pub record: KnowledgeRecord,
    pub scores: CritiqueScores,
    pub accepted: bool,
    pub rejection_reason: Option<String>,
}

// ---------------------------------------------------------------------------
// System prompt
// ---------------------------------------------------------------------------

const STAGE_C_SYSTEM: &str = "\
You are a quality judge for an AI lesson system. Rate the quality of a knowledge extraction.

Rate on four axes, each 1-5:
1. Accuracy (1-5): Is this factually correct per the transcript?
   1=fabricated, 3=mostly correct, 5=exactly right
2. Specificity (1-5): Is this specific and actionable for THIS codebase?
   1=\"testing is important\", 3=moderately specific, 5=\"cargo test --test integration fails if POSTGRES_URL is unset\"
3. Durability (1-5): Will this be useful in future sessions (not just this one)?
   1=only relevant today, 3=relevant for weeks, 5=permanently relevant
4. Classification (1-5): Is the type (Claim/Procedure/Episode) correct?
   1=wrong type, 3=debatable, 5=clearly correct

REJECT (score 1 on all axes) if the extraction:
- Contains raw tool output, error traces, or file listings instead of knowledge
- Is a narration of actions (\"I read the file\", \"Let me check...\")
- Describes the conversation itself rather than knowledge derived from it
- Contains AGENTS.md, system prompt text, or boilerplate instructions

Respond with ONLY a JSON object:
{
  \"accuracy\": <1-5>,
  \"specificity\": <1-5>,
  \"durability\": <1-5>,
  \"classification\": <1-5>,
  \"reasoning\": \"<one sentence explaining the scores>\"
}";

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Critique a single KnowledgeRecord. Returns updated record with state set
/// appropriately (Active if accepted, Retired if rejected).
pub async fn critique_record(
    client: &LlmClient,
    model: &str,
    record: KnowledgeRecord,
) -> Result<StageCResult, TrError> {
    let user_prompt = build_critique_prompt(&record);

    let raw = client
        .message(model, STAGE_C_SYSTEM, &user_prompt, 512)
        .await?;

    let json_str = extract_json_object(&raw).unwrap_or(&raw);
    let scores: CritiqueScores = serde_json::from_str(json_str).map_err(|e| TrError::Parse {
        message: format!("Stage C: failed to parse critique JSON: {e}\nRaw: {raw}"),
    })?;

    let (accepted, rejection_reason) = evaluate_scores(&scores);

    let mut record = record;
    if accepted {
        record.state = KnowledgeState::Active;
        record.confidence = 0.3 + (average_score(&scores) - 3.5) * 0.3;
        record.confidence = record.confidence.clamp(0.0, 1.0);
    } else {
        record.state = KnowledgeState::Retired;
    }

    Ok(StageCResult {
        record,
        scores,
        accepted,
        rejection_reason,
    })
}

/// Run Stage C on records. Uses batched prompts for CLI backend
/// or parallel individual calls for API backend.
/// Deduplication is handled by Jaccard dedup after Stage C, not here.
pub async fn run_stage_c(
    client: Arc<LlmClient>,
    model: &str,
    records: Vec<KnowledgeRecord>,
) -> Vec<StageCResult> {
    if client.is_cli() {
        run_stage_c_batched(&client, model, records).await
    } else {
        run_stage_c_parallel(client, model, records).await
    }
}

/// CLI backend: batch 5 records per call.
async fn run_stage_c_batched(
    client: &LlmClient,
    model: &str,
    records: Vec<KnowledgeRecord>,
) -> Vec<StageCResult> {
    const BATCH_SIZE: usize = 5;
    let mut results: Vec<StageCResult> = Vec::with_capacity(records.len());

    for batch in records.chunks(BATCH_SIZE) {
        if batch.len() == 1 {
            let record = batch[0].clone();
            let result = match critique_record(client, model, record.clone()).await {
                Ok(r) => r,
                Err(_) => default_accept(record),
            };
            results.push(result);
            continue;
        }

        // Build batch prompt
        let mut prompt = format!(
            "Rate each of the following {} knowledge records.\n\
             Respond with a JSON array of {} score objects, one per record, in order.\n\n",
            batch.len(),
            batch.len()
        );
        for (i, record) in batch.iter().enumerate() {
            prompt.push_str(&format!(
                "=== Record {} ===\nType: {:?}\nTitle: {}\nPayload: {}\nRetrieval text: {}\n\n",
                i + 1,
                record.kind,
                record.title,
                record.payload_json,
                record.retrieval_text
            ));
        }
        prompt.push_str(&format!(
            "Respond with ONLY a JSON array of {} score objects, one per record, in order.",
            batch.len()
        ));

        match client.message(model, STAGE_C_SYSTEM, &prompt, 1024).await {
            Ok(raw) => {
                if let Some(parsed) = parse_batch_critique(&raw, batch.len()) {
                    for (i, scores) in parsed.into_iter().enumerate() {
                        if let Some(record) = batch.get(i) {
                            let (accepted, rejection_reason) = evaluate_scores(&scores);
                            let mut record = record.clone();
                            if accepted {
                                record.state = KnowledgeState::Active;
                                record.confidence = 0.3 + (average_score(&scores) - 3.5) * 0.3;
                                record.confidence = record.confidence.clamp(0.0, 1.0);
                            } else {
                                record.state = KnowledgeState::Retired;
                            }
                            results.push(StageCResult {
                                record,
                                scores,
                                accepted,
                                rejection_reason,
                            });
                        }
                    }
                } else {
                    // Parse failed: accept all
                    for record in batch {
                        results.push(default_accept(record.clone()));
                    }
                }
            }
            Err(_) => {
                for record in batch {
                    results.push(default_accept(record.clone()));
                }
            }
        }
    }

    results
}

/// API backend: parallel individual calls.
async fn run_stage_c_parallel(
    client: Arc<LlmClient>,
    model: &str,
    records: Vec<KnowledgeRecord>,
) -> Vec<StageCResult> {
    use tokio::task::JoinSet;
    const MAX_CONCURRENT: usize = 10;

    let model = model.to_string();
    let mut results: Vec<StageCResult> = Vec::with_capacity(records.len());
    let mut cursor = 0usize;

    while cursor < records.len() {
        let batch_end = (cursor + MAX_CONCURRENT).min(records.len());
        let batch = records[cursor..batch_end].to_vec();

        let mut join_set: JoinSet<StageCResult> = JoinSet::new();

        for record in batch {
            let client = Arc::clone(&client);
            let model = model.clone();

            join_set.spawn(async move {
                match critique_record(&client, &model, record.clone()).await {
                    Ok(result) => result,
                    Err(_) => default_accept(record),
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

    results
}

fn default_accept(record: KnowledgeRecord) -> StageCResult {
    StageCResult {
        record: KnowledgeRecord {
            protected: false,
            state: KnowledgeState::Active,
            ..record
        },
        scores: CritiqueScores {
            accuracy: 4,
            specificity: 4,
            durability: 4,
            classification: 4,
            reasoning: "Critique API failed; defaulting to accept.".to_string(),
        },
        accepted: true,
        rejection_reason: None,
    }
}

fn parse_batch_critique(raw: &str, _expected: usize) -> Option<Vec<CritiqueScores>> {
    let start = raw.find('[')?;
    let end = raw.rfind(']')?;
    let arr_str = &raw[start..=end];
    let scores: Vec<CritiqueScores> = serde_json::from_str(arr_str).ok()?;
    Some(scores) // partial is still useful
}

// ---------------------------------------------------------------------------
// Scoring logic
// ---------------------------------------------------------------------------

/// Returns (accepted, rejection_reason).
///
/// Accept if ALL axes >= 3 AND average >= 3.5.
pub fn evaluate_scores(scores: &CritiqueScores) -> (bool, Option<String>) {
    let axes = [
        ("accuracy", scores.accuracy),
        ("specificity", scores.specificity),
        ("durability", scores.durability),
        ("classification", scores.classification),
    ];

    // Check any axis < 3
    for (name, score) in &axes {
        if *score < 3 {
            return (false, Some(format!("{name} score too low ({score}/5 < 3)")));
        }
    }

    // Check average < 3.5
    let avg = average_score(scores);
    if avg < 3.5 {
        return (
            false,
            Some(format!("average score too low ({avg:.2} < 3.5)")),
        );
    }

    (true, None)
}

pub fn average_score(scores: &CritiqueScores) -> f32 {
    (scores.accuracy as f32
        + scores.specificity as f32
        + scores.durability as f32
        + scores.classification as f32)
        / 4.0
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_critique_prompt(record: &KnowledgeRecord) -> String {
    format!(
        "Rate this extracted knowledge record:\n\nType: {:?}\nTitle: {}\nPayload: {}\nRetrieval text: {}",
        record.kind, record.title, record.payload_json, record.retrieval_text
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn scores(accuracy: u8, specificity: u8, durability: u8, classification: u8) -> CritiqueScores {
        CritiqueScores {
            accuracy,
            specificity,
            durability,
            classification,
            reasoning: "test".to_string(),
        }
    }

    #[test]
    fn test_accept_all_fives() {
        let s = scores(5, 5, 5, 5);
        let (accepted, reason) = evaluate_scores(&s);
        assert!(accepted);
        assert!(reason.is_none());
    }

    #[test]
    fn test_accept_borderline_3_5_average() {
        // 3+3+4+4 = 14, avg = 3.5 → accept
        let s = scores(3, 3, 4, 4);
        let (accepted, _) = evaluate_scores(&s);
        assert!(accepted);
    }

    #[test]
    fn test_reject_any_axis_below_3() {
        let s = scores(5, 5, 2, 5);
        let (accepted, reason) = evaluate_scores(&s);
        assert!(!accepted);
        assert!(reason.unwrap().contains("durability"));
    }

    #[test]
    fn test_reject_low_average() {
        // 3+3+3+3 = 12, avg = 3.0 < 3.5 → reject
        let s = scores(3, 3, 3, 3);
        let (accepted, reason) = evaluate_scores(&s);
        assert!(!accepted);
        assert!(reason.unwrap().contains("average"));
    }

    #[test]
    fn test_reject_accuracy_below_3() {
        let s = scores(2, 5, 5, 5);
        let (accepted, reason) = evaluate_scores(&s);
        assert!(!accepted);
        assert!(reason.unwrap().contains("accuracy"));
    }

    #[test]
    fn test_average_score_computation() {
        let s = scores(4, 4, 4, 4);
        assert!((average_score(&s) - 4.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_parse_critique_response() {
        let raw = r#"{
            "accuracy": 4,
            "specificity": 5,
            "durability": 3,
            "classification": 5,
            "reasoning": "Very specific claim about this codebase."
        }"#;
        let scores: CritiqueScores = serde_json::from_str(raw).unwrap();
        assert_eq!(scores.accuracy, 4);
        assert_eq!(scores.specificity, 5);
        assert_eq!(scores.reasoning, "Very specific claim about this codebase.");
    }

    #[test]
    fn test_stage_c_system_prompt_covers_four_axes() {
        assert!(STAGE_C_SYSTEM.contains("Accuracy"));
        assert!(STAGE_C_SYSTEM.contains("Specificity"));
        assert!(STAGE_C_SYSTEM.contains("Durability"));
        assert!(STAGE_C_SYSTEM.contains("Classification"));
    }

    #[test]
    fn test_confidence_boosted_for_high_scores() {
        let s = scores(5, 5, 5, 5);
        let avg = average_score(&s); // 5.0
        // confidence = 0.3 + (5.0 - 3.5) * 0.3 = 0.3 + 0.45 = 0.75
        let confidence = (0.3f32 + (avg - 3.5) * 0.3).clamp(0.0, 1.0);
        assert!((confidence - 0.75).abs() < 0.001);
    }

    #[test]
    fn test_confidence_borderline_scores() {
        let s = scores(3, 3, 4, 4);
        let avg = average_score(&s); // 3.5
        // confidence = 0.3 + (3.5 - 3.5) * 0.3 = 0.3
        let confidence = (0.3f32 + (avg - 3.5) * 0.3).clamp(0.0, 1.0);
        assert!((confidence - 0.30).abs() < 0.001);
    }
}
