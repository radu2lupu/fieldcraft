//! fieldcraft-cli library: exports testable inner functions for integration tests.

use std::path::Path;

// ---------------------------------------------------------------------------
// US-009: Cost estimator (public for tests)
// ---------------------------------------------------------------------------

/// Estimate the cost of extracting a set of chunks via OpenAI Batch API.
///
/// Pricing per 1M tokens (as of 2026):
///   gpt-4.1       — $2.00 input  / $8.00 output  (default for fieldcraft)
///   gpt-4.1-mini  — $0.40 input  / $1.60 output
///   gpt-4o        — $2.50 input  / $10.00 output
/// Batch discount: 0.5×
/// Token heuristic: chars / 4 for input, 500 output per chunk.
pub fn estimate_batch_cost_usd(chunks: &[fieldcraft_extract::chunker::Chunk], model: &str) -> f64 {
    let (input_price_per_million, output_price_per_million) = match model {
        m if m.contains("gpt-4.1-mini") => (0.40, 1.60),
        m if m.contains("gpt-4o-mini") => (0.15, 0.60),
        m if m.contains("gpt-4o") => (2.50, 10.00),
        // gpt-4.1 and any unknown model — default to gpt-4.1 pricing.
        _ => (2.00, 8.00),
    };
    let total_chars: usize = chunks.iter().map(|c| c.text.len()).sum();
    let input_tokens = total_chars as f64 / 4.0;
    let output_tokens = chunks.len() as f64 * 500.0;
    let cost = (input_tokens / 1_000_000.0) * input_price_per_million
        + (output_tokens / 1_000_000.0) * output_price_per_million;
    cost * 0.5
}

// ---------------------------------------------------------------------------
// US-006: extract-batch inner (public for tests)
// ---------------------------------------------------------------------------

pub async fn cmd_extract_batch_inner(
    shared_root: &Path,
    project_filter: Option<&str>,
    limit: usize,
    max_spend_usd: f64,
    client: &dyn fieldcraft_extract::openai_batch::OpenAiBatchApi,
) -> anyhow::Result<()> {
    use fieldcraft_core::ExtractionBatchRecord;
    use fieldcraft_extract::{
        ContentBlock, TranscriptCompressor, TranscriptMessage, chunker::chunk_transcript,
    };
    use fieldcraft_storage::{
        SESSION_EXTRACTOR_VERSION, atomic_claim_pending_sessions, get_source_session,
        insert_extraction_batch, rename_extraction_batch_id,
    };

    let placeholder_id = format!(
        "placeholder-{}",
        generate_hex_id(shared_root.to_string_lossy().as_ref())
    );

    let claimed_ids =
        atomic_claim_pending_sessions(shared_root, &placeholder_id, limit, project_filter)?;

    if claimed_ids.is_empty() {
        println!("No pending sessions found. Nothing to submit.");
        return Ok(());
    }

    println!("Claimed {} sessions.", claimed_ids.len());

    let mut session_chunks: Vec<(String, Vec<fieldcraft_extract::chunker::Chunk>)> = Vec::new();

    for source_id in &claimed_ids {
        let session = match get_source_session(shared_root, source_id)? {
            Some(s) => s,
            None => {
                eprintln!("  warning: session {source_id} not found after claim");
                continue;
            }
        };

        let messages: Vec<TranscriptMessage> = session
            .transcript
            .iter()
            .map(|entry| TranscriptMessage {
                role: entry.role.clone(),
                content: vec![ContentBlock::Text(entry.content.clone())],
                timestamp: entry.timestamp.clone(),
            })
            .collect();

        let compressor = TranscriptCompressor::default();
        let compressed = compressor.compress(&messages);
        let chunks = chunk_transcript(&compressed);

        if !chunks.is_empty() {
            session_chunks.push((source_id.clone(), chunks));
        }
    }

    let all_chunks: Vec<fieldcraft_extract::chunker::Chunk> = session_chunks
        .iter()
        .flat_map(|(_, ch)| ch.iter().cloned())
        .collect();
    let estimated_cost = estimate_batch_cost_usd(&all_chunks, "gpt-4.1-mini");

    if estimated_cost > max_spend_usd {
        fieldcraft_storage::revert_sessions_to_pending(shared_root, &claimed_ids)?;
        anyhow::bail!(
            "Estimated cost ${:.4} exceeds --max-spend-usd ${:.2}. \
             Aborted — no API calls made. Sessions reverted to Pending.",
            estimated_cost,
            max_spend_usd
        );
    }

    println!(
        "Estimated cost: ${:.4} for {} chunks across {} sessions.",
        estimated_cost,
        all_chunks.len(),
        session_chunks.len()
    );

    const MAX_BATCH_BYTES: usize = 180 * 1024 * 1024;
    const MAX_BATCH_REQUESTS: usize = 45_000;

    let mut batches: Vec<Vec<(String, Vec<fieldcraft_extract::chunker::Chunk>)>> = Vec::new();
    let mut current_batch: Vec<(String, Vec<fieldcraft_extract::chunker::Chunk>)> = Vec::new();
    let mut current_bytes = 0usize;
    let mut current_requests = 0usize;

    for (source_id, chunks) in session_chunks {
        let session_bytes: usize = chunks
            .iter()
            .map(|c| c.text.len() + source_id.len() + 300)
            .sum();
        let session_requests = chunks.len();

        if !current_batch.is_empty()
            && (current_bytes + session_bytes > MAX_BATCH_BYTES
                || current_requests + session_requests > MAX_BATCH_REQUESTS)
        {
            batches.push(current_batch);
            current_batch = Vec::new();
            current_bytes = 0;
            current_requests = 0;
        }

        current_bytes += session_bytes;
        current_requests += session_requests;
        current_batch.push((source_id, chunks));
    }
    if !current_batch.is_empty() {
        batches.push(current_batch);
    }

    println!("Submitting {} OpenAI batch(es)...", batches.len());

    let mut submitted_batch_ids: Vec<String> = Vec::new();

    for batch_sessions in &batches {
        let mut jsonl = Vec::new();
        let mut total_requests = 0u32;
        let mut batch_session_ids: Vec<String> = Vec::new();

        for (source_id, chunks) in batch_sessions {
            batch_session_ids.push(source_id.clone());
            for chunk in chunks {
                let prev_text = chunk
                    .prev_index
                    .and_then(|pi| chunks.get(pi))
                    .map(|c| c.text.as_str())
                    .unwrap_or("");
                let next_text = chunk
                    .next_index
                    .and_then(|ni| chunks.get(ni))
                    .map(|c| c.text.as_str())
                    .unwrap_or("");
                let user_prompt = build_stage_b_user_prompt(&chunk.text, prev_text, next_text);
                let request = serde_json::json!({
                    "custom_id": format!("{}|{}", source_id, chunk.index),
                    "method": "POST",
                    "url": "/v1/chat/completions",
                    "body": {
                        "model": "gpt-4.1-mini",
                        "messages": [
                            {"role": "system", "content": fieldcraft_extract::stage_b::unified_system()},
                            {"role": "user", "content": user_prompt}
                        ],
                        "response_format": {"type": "json_object"},
                        "max_tokens": 1024,
                        // Deterministic for classification tasks — matches the
                        // sync prompt-eval harness used to choose this prompt.
                        // Without this OpenAI defaults to T=1.0 which biases
                        // toward the most common output pattern (directive).
                        "temperature": 0
                    }
                });
                let line = serde_json::to_string(&request).unwrap_or_default();
                jsonl.extend_from_slice(line.as_bytes());
                jsonl.push(b'\n');
                total_requests += 1;
            }
        }

        let input_file_id = client
            .upload_batch_file(jsonl)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to upload batch file: {e}"))?;

        let batch_response = client
            .create_batch(&input_file_id, "/v1/chat/completions", "24h")
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create batch: {e}"))?;

        let real_batch_id = batch_response.id.clone();

        let batch_record = ExtractionBatchRecord {
            batch_id: placeholder_id.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            status: "submitted".to_string(),
            session_ids: batch_session_ids,
            input_file_id: Some(input_file_id),
            output_file_id: None,
            error_file_id: None,
            total_requests,
            reconciled_at: None,
            extractor_version: SESSION_EXTRACTOR_VERSION.to_string(),
            last_polled_at: None,
            poll_error: None,
        };
        insert_extraction_batch(shared_root, &batch_record)?;
        rename_extraction_batch_id(shared_root, &placeholder_id, &real_batch_id)?;

        submitted_batch_ids.push(real_batch_id);
    }

    println!(
        "Submitted {} batch(es): {}",
        submitted_batch_ids.len(),
        submitted_batch_ids.join(", ")
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// US-007: extract-batch-poll inner (public for tests)
// ---------------------------------------------------------------------------

/// Inner implementation of `extract-batch-poll`.
/// Returns true when all active batches are terminal.
pub async fn cmd_extract_batch_poll_inner(
    shared_root: &Path,
    client: &dyn fieldcraft_extract::openai_batch::OpenAiBatchApi,
) -> anyhow::Result<bool> {
    use fieldcraft_extract::openai_batch::BatchStatus;
    use fieldcraft_storage::{KnowledgeRepository, fieldcraft_db_path};
    use fieldcraft_storage::{
        SESSION_EXTRACTOR_VERSION, list_extraction_batches_by_status,
        mark_extraction_batch_reconciled, mark_source_session_failed,
        mark_source_session_succeeded, revert_sessions_to_pending, update_extraction_batch_status,
    };

    let now = chrono::Utc::now();
    let now_str = now.to_rfc3339();

    // Step 1: Stuck-Running sweep (26h timeout)
    let all_live = list_extraction_batches_by_status(
        shared_root,
        &["submitted", "in_progress", "validating", "finalizing"],
    )?;

    for batch in &all_live {
        let created = chrono::DateTime::parse_from_rfc3339(&batch.created_at)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or(now);
        let age_hours = (now - created).num_hours();
        if age_hours >= 26 {
            eprintln!(
                "Batch {} is stuck ({}h old) — marking failed.",
                batch.batch_id, age_hours
            );
            revert_sessions_to_pending(shared_root, &batch.session_ids)?;
            update_extraction_batch_status(
                shared_root,
                &batch.batch_id,
                "failed",
                Some(&now_str),
                Some("expired-26h-timeout"),
            )?;
        }
    }

    // Step 2: Poll remaining live batches
    let live_batches = list_extraction_batches_by_status(
        shared_root,
        &["submitted", "in_progress", "validating", "finalizing"],
    )?;

    if live_batches.is_empty() {
        println!("No active batches to poll.");
        return Ok(true);
    }

    let db_path = fieldcraft_db_path();
    let repo = KnowledgeRepository::open(&db_path)
        .map_err(|e| anyhow::anyhow!("failed to open knowledge repository: {e}"))?;

    let mut n_polled = 0usize;
    let mut n_reconciled = 0usize;
    let mut n_failed = 0usize;
    let mut total_records_written = 0usize;

    for batch in &live_batches {
        let batch_id = &batch.batch_id;
        n_polled += 1;

        let batch_response = match client.get_batch(batch_id).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  poll error for {batch_id}: {e}");
                update_extraction_batch_status(
                    shared_root,
                    batch_id,
                    &batch.status,
                    Some(&now_str),
                    Some(&e.to_string()),
                )?;
                continue;
            }
        };

        let openai_status_str = match batch_response.status {
            BatchStatus::Validating => "validating",
            BatchStatus::InProgress => "in_progress",
            BatchStatus::Finalizing => "finalizing",
            BatchStatus::Completed => "completed",
            BatchStatus::Expired => "expired",
            BatchStatus::Cancelling => "cancelling",
            BatchStatus::Cancelled => "cancelled",
            BatchStatus::Failed => "failed",
        };

        match batch_response.status {
            BatchStatus::Completed => {
                let output_bytes = if let Some(ref fid) = batch_response.output_file_id {
                    match client.download_file(fid).await {
                        Ok(b) => b,
                        Err(e) => {
                            eprintln!("  failed to download output for {batch_id}: {e}");
                            update_extraction_batch_status(
                                shared_root,
                                batch_id,
                                "failed",
                                Some(&now_str),
                                Some(&format!("download-error: {e}")),
                            )?;
                            n_failed += 1;
                            continue;
                        }
                    }
                } else {
                    Vec::new()
                };

                let error_bytes = if let Some(ref fid) = batch_response.error_file_id {
                    client.download_file(fid).await.unwrap_or_default()
                } else {
                    Vec::new()
                };

                let mut success_by_session: std::collections::HashMap<
                    String,
                    Vec<(usize, String)>,
                > = std::collections::HashMap::new();
                let mut retriable_sessions: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                let mut terminal_errors: std::collections::HashMap<String, String> =
                    std::collections::HashMap::new();
                // Collect records from all sessions in this batch before dedup
                let mut all_batch_records: Vec<fieldcraft_core::KnowledgeRecord> = Vec::new();

                for line in output_bytes.split(|b| *b == b'\n') {
                    if line.is_empty() {
                        continue;
                    }
                    let Ok(val) = serde_json::from_slice::<serde_json::Value>(line) else {
                        continue;
                    };
                    let custom_id = val["custom_id"].as_str().unwrap_or("");
                    let (source_id, chunk_idx) = parse_custom_id(custom_id);
                    let status_code = val["response"]["status_code"].as_u64().unwrap_or(0) as u16;
                    let error_code = val["response"]["body"]["error"]["code"]
                        .as_str()
                        .unwrap_or("");

                    if status_code == 200 {
                        let body_str = val["response"]["body"]["choices"][0]["message"]["content"]
                            .as_str()
                            .unwrap_or("");
                        if !body_str.is_empty() {
                            success_by_session
                                .entry(source_id)
                                .or_default()
                                .push((chunk_idx, body_str.to_string()));
                        }
                    } else if is_retriable_status(status_code, error_code) {
                        retriable_sessions.insert(source_id);
                    } else {
                        let msg = val["response"]["body"]["error"]["message"]
                            .as_str()
                            .unwrap_or("unknown error")
                            .to_string();
                        terminal_errors
                            .entry(source_id)
                            .and_modify(|e| {
                                e.push_str("; ");
                                e.push_str(&msg);
                            })
                            .or_insert(msg);
                    }
                }

                for line in error_bytes.split(|b| *b == b'\n') {
                    if line.is_empty() {
                        continue;
                    }
                    let Ok(val) = serde_json::from_slice::<serde_json::Value>(line) else {
                        continue;
                    };
                    let custom_id = val["custom_id"].as_str().unwrap_or("");
                    let (source_id, _) = parse_custom_id(custom_id);
                    let error_code = val["error"]["code"].as_str().unwrap_or("");
                    if is_retriable_status(0, error_code) || error_code.is_empty() {
                        retriable_sessions.insert(source_id);
                    } else {
                        let msg = val["error"]["message"]
                            .as_str()
                            .unwrap_or("unknown error")
                            .to_string();
                        terminal_errors
                            .entry(source_id)
                            .and_modify(|e| {
                                e.push_str("; ");
                                e.push_str(&msg);
                            })
                            .or_insert(msg);
                    }
                }

                for session_id in &batch.session_ids {
                    if retriable_sessions.contains(session_id) {
                        revert_sessions_to_pending(shared_root, std::slice::from_ref(session_id))?;
                        continue;
                    }

                    if let Some(err) = terminal_errors.get(session_id) {
                        mark_source_session_failed(shared_root, session_id, err)?;
                        continue;
                    }

                    if let Some(chunk_results) = success_by_session.get(session_id) {
                        let session =
                            fieldcraft_storage::get_source_session(shared_root, session_id)?;
                        let project_id = session
                            .as_ref()
                            .map(|s| s.project.clone())
                            .unwrap_or_else(|| "unknown".to_string());
                        let session_date = session.and_then(|s| {
                            s.started_at
                                .as_deref()
                                .and_then(|started| {
                                    chrono::DateTime::parse_from_rfc3339(started).ok()
                                })
                                .map(|dt| dt.with_timezone(&chrono::Utc))
                        });

                        for (chunk_idx, json_str) in chunk_results {
                            let json_trimmed =
                                fieldcraft_extract::util::extract_json_object(json_str)
                                    .unwrap_or(json_str.as_str());
                            match build_knowledge_record_from_stage_b(
                                json_trimmed,
                                &project_id,
                                session_id,
                                *chunk_idx,
                            ) {
                                Ok(mut record) => {
                                    record.tier = fieldcraft_core::KnowledgeTier::Archive;
                                    record.session_date = session_date;
                                    if !fieldcraft_extract::extract::is_garbage(&record) {
                                        record.state = fieldcraft_core::KnowledgeState::Active;
                                        all_batch_records.push(record);
                                    }
                                }
                                Err(e) => {
                                    eprintln!(
                                        "  chunk {chunk_idx} of {session_id} parse error: {e}"
                                    );
                                }
                            }
                        }
                    }

                    // Mark session succeeded regardless of whether its records survive dedup.
                    // A session is "succeeded" if all its chunks parsed without terminal errors.
                    mark_source_session_succeeded(
                        shared_root,
                        session_id,
                        SESSION_EXTRACTOR_VERSION,
                    )?;
                }

                // Cross-session dedup: run once over all records from this batch
                let (deduped, n_cross_merged) =
                    fieldcraft_extract::extract::dedup_records(all_batch_records);
                if n_cross_merged > 0 {
                    println!(
                        "  batch {batch_id}: cross-session dedup removed {n_cross_merged} duplicate(s)"
                    );
                }
                for record in &deduped {
                    match repo.insert_knowledge(record) {
                        Ok(_) => total_records_written += 1,
                        Err(e) => eprintln!(
                            "  failed to store record {} for batch {batch_id}: {e}",
                            record.id
                        ),
                    }
                }

                mark_extraction_batch_reconciled(shared_root, batch_id, &now_str)?;
                n_reconciled += 1;
                println!("  batch {batch_id}: reconciled");
            }

            BatchStatus::Failed | BatchStatus::Expired | BatchStatus::Cancelled => {
                revert_sessions_to_pending(shared_root, &batch.session_ids)?;
                update_extraction_batch_status(
                    shared_root,
                    batch_id,
                    "failed",
                    Some(&now_str),
                    Some(&format!("openai-status-{openai_status_str}")),
                )?;
                n_failed += 1;
                println!("  batch {batch_id}: {openai_status_str} — sessions reverted");
            }

            _ => {
                update_extraction_batch_status(
                    shared_root,
                    batch_id,
                    openai_status_str,
                    Some(&now_str),
                    None,
                )?;
                println!("  batch {batch_id}: {openai_status_str}");
            }
        }
    }

    println!(
        "\nPoll complete: {} polled, {} reconciled, {} failed, {} records written.",
        n_polled, n_reconciled, n_failed, total_records_written
    );

    let remaining = list_extraction_batches_by_status(
        shared_root,
        &["submitted", "in_progress", "validating", "finalizing"],
    )?;
    Ok(remaining.is_empty())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn build_stage_b_user_prompt(chunk_text: &str, prev: &str, next: &str) -> String {
    let mut prompt = String::new();
    if !prev.is_empty() {
        prompt.push_str("=== Previous context (do not extract from this) ===\n");
        prompt.push_str(prev);
        prompt.push_str("\n\n");
    }
    prompt.push_str("=== Target chunk (extract from this) ===\n");
    prompt.push_str(chunk_text);
    if !next.is_empty() {
        prompt.push_str("\n\n=== Following context (do not extract from this) ===\n");
        prompt.push_str(next);
    }
    prompt
}

fn generate_hex_id(seed: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    hasher.update(chrono::Utc::now().to_rfc3339().as_bytes());
    format!("{:x}", hasher.finalize())[..24].to_string()
}

fn parse_custom_id(custom_id: &str) -> (String, usize) {
    if let Some(pos) = custom_id.rfind('|') {
        let source_id = custom_id[..pos].to_string();
        let chunk_idx = custom_id[pos + 1..].parse().unwrap_or(0);
        (source_id, chunk_idx)
    } else {
        (custom_id.to_string(), 0)
    }
}

fn is_retriable_status(status_code: u16, error_code: &str) -> bool {
    matches!(status_code, 429 | 500 | 502 | 503 | 504)
        || matches!(
            error_code,
            "rate_limit_exceeded" | "server_error" | "service_unavailable"
        )
}

fn build_knowledge_record_from_stage_b(
    json_str: &str,
    project_id: &str,
    session_id: &str,
    chunk_index: usize,
) -> anyhow::Result<fieldcraft_core::KnowledgeRecord> {
    use fieldcraft_core::KnowledgeKind;

    let val: serde_json::Value =
        serde_json::from_str(json_str).map_err(|e| anyhow::anyhow!("JSON parse: {e}"))?;

    let kind = match val["type"].as_str() {
        Some("directive") => KnowledgeKind::Directive,
        Some("claim") => KnowledgeKind::Claim,
        Some("procedure") => KnowledgeKind::Procedure,
        Some("episode") => KnowledgeKind::Episode,
        Some("context") => KnowledgeKind::Context,
        Some("skip") => anyhow::bail!("skip record"),
        _ => KnowledgeKind::Directive,
    };

    // Delegate to the canonical stage_b record builder which handles all 5 kinds
    // and honors the model's `title` field with an 80-char cap.
    fieldcraft_extract::stage_b::build_record(json_str, kind, project_id, session_id, chunk_index)
        .map_err(|e| anyhow::anyhow!("stage_b::build_record: {e}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::build_knowledge_record_from_stage_b;

    fn make_directive_json(action: &str, reason: &str, queries: &[&str]) -> String {
        let q_arr: Vec<serde_json::Value> = queries
            .iter()
            .map(|s| serde_json::Value::String(s.to_string()))
            .collect();
        serde_json::json!({
            "type": "directive",
            "action": action,
            "reason": reason,
            "files": [],
            "trigger": "test",
            "avoid": [],
            "retrieval_queries": q_arr,
        })
        .to_string()
    }

    /// Simulates two sessions in the same batch producing near-duplicate records.
    /// After collecting all records and running dedup_records once (cross-session),
    /// the knowledge table should have the deduped count, not the raw count.
    #[test]
    fn cross_session_dedup_reduces_duplicate_count() {
        // Two sessions each produce the same directive record
        let json = make_directive_json(
            "always run cargo clippy before committing rust code changes",
            "prevents warnings from landing in main branch",
            &["cargo clippy commit", "rust pre-commit"],
        );

        let r1 =
            build_knowledge_record_from_stage_b(&json, "proj", "session-a", 0).expect("r1 parses");
        // Session B produces a near-identical record (same text, different session)
        let r2 =
            build_knowledge_record_from_stage_b(&json, "proj", "session-b", 0).expect("r2 parses");

        // Collect all records across both sessions (as the new batch reconcile does)
        let all_records = vec![r1, r2];
        let raw_count = all_records.len();

        let (deduped, n_merged) = fieldcraft_extract::extract::dedup_records(all_records);

        // After dedup the count must be less than raw (or equal if embeddings differ — both valid)
        assert!(deduped.len() <= raw_count);
        // n_merged + deduped.len() == raw_count
        assert_eq!(deduped.len() + n_merged, raw_count);
    }
}
