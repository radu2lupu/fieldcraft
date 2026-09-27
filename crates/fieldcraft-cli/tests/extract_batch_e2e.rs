//! End-to-end integration tests for the batch extraction lifecycle.
//!
//! Uses MockOpenAiBatchClient to avoid any real network calls.
//! Each test creates an isolated temp directory with its own SQLite databases.

use fieldcraft_core::SourceSessionStatus;
use fieldcraft_extract::openai_batch::MockOpenAiBatchClient;
use fieldcraft_storage::{
    KnowledgeRepository, fieldcraft_db_path, get_source_session, list_extraction_batches_by_status,
    open_source_sessions_catalog,
};
use std::path::Path;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// HOME-redirection helper
// ---------------------------------------------------------------------------
// Several tests redirect process-global `HOME` so `fieldcraft_db_path()`
// resolves into the test's tempdir. Without serialization, parallel tests
// clobber each other's env var and racing repository opens hit SQLite WAL
// lock or disk-I/O errors. This Mutex serializes those tests; the
// `HomeRedirect` Drop impl restores the original HOME even on panic.

static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct HomeRedirect {
    _guard: std::sync::MutexGuard<'static, ()>,
    orig: String,
}

impl Drop for HomeRedirect {
    fn drop(&mut self) {
        unsafe {
            std::env::set_var("HOME", &self.orig);
        }
    }
}

fn redirect_home_to(tempdir: &Path) -> HomeRedirect {
    let guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let orig = std::env::var("HOME").unwrap_or_default();
    unsafe {
        std::env::set_var("HOME", tempdir.to_str().unwrap());
    }
    HomeRedirect {
        _guard: guard,
        orig,
    }
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Create the source_sessions catalog and insert N fake pending sessions.
fn insert_fake_sessions(shared_root: &Path, n: usize, project: &str) -> Vec<String> {
    let conn = open_source_sessions_catalog(shared_root).expect("open catalog");
    let now = chrono::Utc::now().to_rfc3339();
    let mut ids = Vec::new();

    for i in 0..n {
        let source_id = format!("src-{i:04}");
        let transcript_json = serde_json::json!([{
            "timestamp": null,
            "role": "user",
            "kind": "text",
            "content": format!(
                "This is a fake session transcript for session {i}. \
                 It contains important information about how to configure \
                 the build system. Use cargo build --workspace to build all crates. \
                 This is a longer piece of text to make sure it gets chunked properly \
                 by the chunker. The chunker splits on token boundaries so we need \
                 enough text here to produce at least one chunk. \
                 Additional context: the project uses SQLite as its storage backend. \
                 More content to ensure minimum chunk size is met for session {i}. \
                 When modifying the database schema, always run migrations idempotently."
            )
        }])
        .to_string();

        conn.execute(
            "INSERT OR IGNORE INTO source_sessions
             (source_id, source_tool, source_path, session_id, project, machine, cwd,
              title, started_at, updated_at, discovered_at, source_fingerprint,
              transcript_json, status, attempts, extractor_version, last_error, last_extracted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            rusqlite::params![
                source_id,
                "claude_code",
                format!("/fake/path/session_{i}.json"),
                format!("session-{i:04}"),
                project,
                "test-machine",
                Option::<String>::None,
                format!("Test Session {i}"),
                Option::<String>::None,
                Option::<String>::None,
                now,
                format!("fp-{i:04}"),
                transcript_json,
                "pending",
                0i64,
                "test-extractor-v1",
                Option::<String>::None,
                Option::<String>::None,
            ],
        )
        .expect("insert session");

        ids.push(source_id);
    }

    ids
}

/// Build a synthetic output JSONL for the given (source_id, chunk_idx) pairs.
fn build_output_jsonl(entries: &[(String, usize)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (source_id, chunk_idx) in entries {
        let custom_id = format!("{source_id}|{chunk_idx}");
        let body_content = serde_json::json!({
            "type": "directive",
            "phase": "edit",
            "action": format!("In src/main.rs, add logging for session {source_id}"),
            "reason": "Improves debuggability and traceability of session processing",
            "files": ["src/main.rs"],
            "trigger": format!("When modifying session handling code for {source_id}"),
            "avoid": [],
            "verify": "cargo test --workspace",
            "invalid_if": "if src/main.rs is deleted",
            "retrieval_queries": ["how to add logging", "debug session handling"]
        });
        let line = serde_json::json!({
            "custom_id": custom_id,
            "response": {
                "status_code": 200,
                "body": {
                    "choices": [{
                        "message": {
                            "content": body_content.to_string()
                        }
                    }]
                }
            }
        });
        out.extend_from_slice(serde_json::to_string(&line).unwrap().as_bytes());
        out.push(b'\n');
    }
    out
}

// ---------------------------------------------------------------------------
// US-009 cost estimator unit test
// ---------------------------------------------------------------------------

#[test]
fn test_cost_estimator_known_fixture() {
    // 10 chunks of 4000 chars each
    // input_tokens = 10 * 4000 / 4 = 10,000
    // output_tokens = 10 * 500 = 5,000
    // cost pre-discount = 10000/1M * $0.40 + 5000/1M * $1.60
    //                   = $0.004 + $0.008 = $0.012  (wait: /1M not /1000)
    //   actually: 10000/1_000_000 * 0.40 = 0.000004
    //   Let me redo: input=10000 tokens, 10000 * 0.40 / 1_000_000 = 0.004 / 1000 = 0.000004
    //   Hmm no: 10000 / 1000000 = 0.01 * 0.40 = 0.004 ... that's wrong too
    //   Correct: 10000 * (0.40 / 1_000_000) = 10000 * 0.0000004 = 0.004 ... wait that can't be right
    //   10000 * 0.40 = 4000 → 4000 / 1_000_000 = 0.004
    //   5000 * 1.60 = 8000 → 8000 / 1_000_000 = 0.008
    //   Total pre-discount: $0.004 + $0.008 = $0.012 ... but that's in dollars per what?
    //   Actually: price is $0.40 per 1M tokens → 1 token costs $0.40/1_000_000 = $0.0000004
    //   10000 tokens * $0.0000004 = $0.004 ... wait: 10000 * 0.40 / 1_000_000 = 4000/1000000 = 0.004
    //   Hmm that's $0.004 for input, $0.008 for output. Total $0.012, halved = $0.006 USD.
    // Expected: ~$0.006 for gpt-4.1-mini with batch discount

    let chunks: Vec<fieldcraft_extract::chunker::Chunk> = (0..10)
        .map(|i| fieldcraft_extract::chunker::Chunk {
            index: i,
            text: "a".repeat(4000),
            token_estimate: 1000,
            prev_index: if i > 0 { Some(i - 1) } else { None },
            next_index: if i < 9 { Some(i + 1) } else { None },
        })
        .collect();

    let cost = fieldcraft_cli::estimate_batch_cost_usd(&chunks, "gpt-4.1-mini");

    // With the math above:
    // input: 10 chunks * 4000 chars / 4 = 10000 tokens → 10000/1M * $0.40 = $0.004
    // output: 10 chunks * 500 = 5000 tokens → 5000/1M * $1.60 = $0.008
    // total: $0.012 * 0.5 batch discount = $0.006
    let expected = 0.006_f64;
    let tolerance = 0.0001; // small absolute tolerance
    assert!(
        (cost - expected).abs() < tolerance,
        "Expected cost ~${expected:.6}, got ${cost:.6}"
    );
}

// ---------------------------------------------------------------------------
// Test 1: Happy path lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_batch_lifecycle_happy_path() {
    let dir = TempDir::new().unwrap();
    let shared_root = dir.path().join("sessions");
    std::fs::create_dir_all(&shared_root).unwrap();

    let project = "test-project";
    let session_ids = insert_fake_sessions(&shared_root, 3, project);
    assert_eq!(session_ids.len(), 3);

    let mock = MockOpenAiBatchClient::new();

    // Submit
    fieldcraft_cli::cmd_extract_batch_inner(&shared_root, Some(project), 10, 100.0, &mock)
        .await
        .expect("extract-batch-inner should succeed");

    // Verify: 1 extraction_batch row with status='submitted' or 'in_progress'
    let batches =
        list_extraction_batches_by_status(&shared_root, &["submitted", "in_progress"]).unwrap();
    assert_eq!(batches.len(), 1, "Expected exactly 1 batch submitted");
    let batch = &batches[0];
    assert_eq!(batch.status, "submitted");
    assert_eq!(batch.session_ids.len(), 3);

    // Verify: all source_sessions have status='running'
    for sid in &session_ids {
        let sess = get_source_session(&shared_root, sid)
            .unwrap()
            .expect("session should exist");
        assert_eq!(
            sess.status,
            SourceSessionStatus::Running,
            "session {sid} should be Running after claim"
        );
        assert!(
            sess.extraction_batch_id.is_some(),
            "session {sid} should have extraction_batch_id set"
        );
    }

    // Build synthetic output JSONL (one chunk per session at index 0)
    let output_entries: Vec<(String, usize)> =
        session_ids.iter().map(|id| (id.clone(), 0)).collect();
    let output_jsonl = build_output_jsonl(&output_entries);

    // Complete the batch in the mock
    let batch_id = batch.batch_id.clone();
    mock.complete_batch(&batch_id, output_jsonl, None);

    // Redirect HOME so fieldcraft_db_path() resolves to our temp dir.
    // Guard serializes parallel HOME-mutating tests and restores on drop.
    let _home = redirect_home_to(dir.path());

    // Poll
    let done = fieldcraft_cli::cmd_extract_batch_poll_inner(&shared_root, &mock)
        .await
        .expect("poll should succeed");

    // Verify: batch reconciled
    let reconciled = list_extraction_batches_by_status(&shared_root, &["reconciled"]).unwrap();
    assert_eq!(reconciled.len(), 1, "batch should be reconciled");

    // Verify: all sessions Succeeded
    for sid in &session_ids {
        let sess = get_source_session(&shared_root, sid)
            .unwrap()
            .expect("session should exist");
        assert_eq!(
            sess.status,
            SourceSessionStatus::Succeeded,
            "session {sid} should be Succeeded after reconciliation"
        );
        assert!(
            sess.extraction_batch_id.is_none(),
            "session {sid} extraction_batch_id should be cleared"
        );
    }

    assert!(done, "poll should indicate no more live batches");
}

// ---------------------------------------------------------------------------
// Test 2: Failure path — OpenAI reports batch failed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_batch_failure_path() {
    let dir = TempDir::new().unwrap();
    let shared_root = dir.path().join("sessions");
    std::fs::create_dir_all(&shared_root).unwrap();

    let project = "fail-project";
    let session_ids = insert_fake_sessions(&shared_root, 2, project);

    let mock = MockOpenAiBatchClient::new();

    // Submit
    fieldcraft_cli::cmd_extract_batch_inner(&shared_root, Some(project), 10, 100.0, &mock)
        .await
        .expect("extract-batch-inner should succeed");

    let batches =
        list_extraction_batches_by_status(&shared_root, &["submitted", "in_progress"]).unwrap();
    assert_eq!(batches.len(), 1);
    let batch_id = batches[0].batch_id.clone();

    // Fail the batch in the mock
    mock.fail_batch(&batch_id, "batch expired");

    let _home = redirect_home_to(dir.path());

    // Poll
    fieldcraft_cli::cmd_extract_batch_poll_inner(&shared_root, &mock)
        .await
        .expect("poll should succeed even for failed batch");

    // Verify: batch marked failed
    let failed = list_extraction_batches_by_status(&shared_root, &["failed"]).unwrap();
    assert_eq!(failed.len(), 1, "batch should be failed");

    // Verify: all sessions reverted to Pending
    for sid in &session_ids {
        let sess = get_source_session(&shared_root, sid)
            .unwrap()
            .expect("session should exist");
        assert_eq!(
            sess.status,
            SourceSessionStatus::Pending,
            "session {sid} should be Pending after batch failure"
        );
        assert!(
            sess.extraction_batch_id.is_none(),
            "session {sid} extraction_batch_id should be cleared"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 3: Cost cap pre-flight abort
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_cost_cap_abort() {
    let dir = TempDir::new().unwrap();
    let shared_root = dir.path().join("sessions");
    std::fs::create_dir_all(&shared_root).unwrap();

    let project = "cost-cap-project";
    // Insert 5 sessions — cost estimate will be > 0 but we set cap to $0.000001
    let session_ids = insert_fake_sessions(&shared_root, 5, project);

    let mock = MockOpenAiBatchClient::new();

    // Set max_spend_usd to an extremely low value — guaranteed to abort
    let result =
        fieldcraft_cli::cmd_extract_batch_inner(&shared_root, Some(project), 50, 0.000001, &mock)
            .await;

    // Should have errored with cost-cap message
    assert!(result.is_err(), "Should fail with cost cap exceeded");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("Estimated cost") || err.contains("max-spend-usd"),
        "Error should mention cost cap, got: {err}"
    );

    // No API calls made — no extraction_batches rows should exist
    let batches = list_extraction_batches_by_status(&shared_root, &["submitted"]).unwrap();
    assert!(
        batches.is_empty(),
        "No batches should be submitted when cost cap aborts"
    );

    // Sessions should be back to Pending
    for sid in &session_ids {
        let sess = get_source_session(&shared_root, sid)
            .unwrap()
            .expect("session should exist");
        assert_eq!(
            sess.status,
            SourceSessionStatus::Pending,
            "session {sid} should be Pending after cost-cap abort"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 4: Idempotent poll — second poll on reconciled batch is a no-op
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_idempotent_poll() {
    let dir = TempDir::new().unwrap();
    let shared_root = dir.path().join("sessions");
    std::fs::create_dir_all(&shared_root).unwrap();

    let project = "idempotent-project";
    let session_ids = insert_fake_sessions(&shared_root, 2, project);

    let mock = MockOpenAiBatchClient::new();

    // Submit
    fieldcraft_cli::cmd_extract_batch_inner(&shared_root, Some(project), 10, 100.0, &mock)
        .await
        .expect("submit should succeed");

    let batches =
        list_extraction_batches_by_status(&shared_root, &["submitted", "in_progress"]).unwrap();
    let batch_id = batches[0].batch_id.clone();

    // Complete in mock
    let output_entries: Vec<(String, usize)> =
        session_ids.iter().map(|id| (id.clone(), 0)).collect();
    mock.complete_batch(&batch_id, build_output_jsonl(&output_entries), None);

    let _home = redirect_home_to(dir.path());

    // First poll
    fieldcraft_cli::cmd_extract_batch_poll_inner(&shared_root, &mock)
        .await
        .expect("first poll should succeed");

    // Count records after first poll
    let db_path = fieldcraft_db_path();
    let records_after_first = if db_path.exists() {
        let repo = KnowledgeRepository::open(&db_path).expect("open repo");
        repo.list_knowledge(project, None, None)
            .unwrap_or_default()
            .len()
    } else {
        0
    };

    // Second poll — should be a no-op (batch already reconciled, no live batches)
    fieldcraft_cli::cmd_extract_batch_poll_inner(&shared_root, &mock)
        .await
        .expect("second poll should succeed");

    let records_after_second = if db_path.exists() {
        let repo = KnowledgeRepository::open(&db_path).expect("open repo");
        repo.list_knowledge(project, None, None)
            .unwrap_or_default()
            .len()
    } else {
        0
    };

    assert_eq!(
        records_after_first, records_after_second,
        "Second poll should not write duplicate records: first={records_after_first}, second={records_after_second}"
    );
}
