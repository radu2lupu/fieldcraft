//! Stage B: Sonnet-based structured extraction.
//!
//! For each HIGH chunk from Stage A (plus 2 surrounding chunks for context),
//! calls Sonnet to extract a structured KnowledgeRecord.
//!
//! Runs up to `max_concurrent` chunks in parallel using tokio::JoinSet.

use crate::anthropic::LlmClient;
use crate::chunker::Chunk;
use crate::stage_a::StageAResult;
use crate::util::extract_json_object;
use fieldcraft_core::{
    AgentTool, ClaimPayload, ContextPayload, DirectivePayload, EpisodePayload, KnowledgeKind,
    KnowledgePhase, KnowledgeRecord, KnowledgeSource, KnowledgeState, ProcedurePayload,
    SourceReference, TrError,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::task::JoinSet;

// ---------------------------------------------------------------------------
// Extraction response types (JSON returned by Sonnet)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
struct ClaimExtraction {
    statement: String,
    mechanism: Option<String>,
    scope: Option<String>,
    exceptions: Option<Vec<String>>,
    affected_files: Option<Vec<String>>,
    retrieval_questions: Option<Vec<String>>,
    trigger: Option<String>,
    invalid_if: Option<String>,
    #[serde(default)]
    do_action: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ProcedureExtraction {
    goal: String,
    preconditions: Option<Vec<String>>,
    steps: Option<Vec<String>>,
    failure_modes: Option<Vec<String>>,
    affected_files: Option<Vec<String>>,
    retrieval_questions: Option<Vec<String>>,
    trigger: Option<String>,
    invalid_if: Option<String>,
    #[serde(default)]
    do_action: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct EpisodeExtraction {
    summary: String,
    trigger: Option<String>,
    outcome: Option<String>,
    observations: Option<Vec<String>>,
    affected_files: Option<Vec<String>>,
    #[serde(default)]
    do_action: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct DirectiveExtraction {
    action: String,
    reason: String,
    files: Option<Vec<String>>,
    trigger: Option<String>,
    avoid: Option<Vec<String>>,
    verify: Option<String>,
    invalid_if: Option<String>,
    retrieval_queries: Option<Vec<String>>,
    #[serde(default)]
    phase: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ContextExtraction {
    statement: String,
    scope: Option<String>,
    files: Option<Vec<String>>,
    implications: Option<Vec<String>>,
    invalid_if: Option<String>,
    retrieval_queries: Option<Vec<String>>,
    #[serde(default)]
    phase: Option<String>,
}

// ---------------------------------------------------------------------------
// System prompts
// ---------------------------------------------------------------------------
//
// TITLE PATTERN GUIDE (≤80 chars, action-shaped, NOT a transcript excerpt):
//   directive  → "Fix <issue> in <file>" or "Add <thing> to <module>"
//   context    → "<system> <behavior>", e.g. "WAL serializes writes but allows concurrent reads"
//
// KIND TAXONOMY:
//   directive  — bugs fixed, decisions made, gotchas, patterns established
//                Use when the session contains: a specific change that was made, a decision
//                about which approach to take, a discovered bug and its fix, a constraint
//                to enforce going forward.
//   context    — code patterns, invariants, traps, structural rules in a module
//                Use when the session surfaces: how a system behaves, an architectural
//                invariant, a non-obvious dependency, a fact about a third-party API.
//                Example sessions that should produce context, NOT directive:
//                  - discovering that SQLite WAL mode allows concurrent reads
//                  - learning that a config key must match an env var name exactly
//                  - observing that a library silently truncates values > N bytes
// ---------------------------------------------------------------------------

pub const STAGE_B_SYSTEM: &str = "\
You are extracting transferable knowledge from a developer coding session.

Your output will be injected into a FUTURE agent's context to help it work \
faster. The future agent is in EXECUTION mode — it needs instructions, not \
stories. Write as if you are leaving a note for your replacement.

EMIT one of these types: directive | procedure | context | episode | claim | skip. \
EVERY record MUST include a \"title\" field (≤80 chars, action-shaped summary — \
NOT a transcript excerpt) and a \"phase\" field. \
Title patterns by type: \
directive → \"Fix <issue> in <file>\" or \"Add <thing> to <module>\"; \
procedure → \"How to <verb> <object>\", e.g. \"How to deploy via launchd plist\"; \
context → \"<system> <behavior>\", e.g. \"WAL allows concurrent reads\"; \
episode → \"<discovery>: <fix>\", e.g. \"Race in fetch_queue caused message amplification: fixed via atomic UPDATE\"; \
claim → \"<system> <factual property>\", e.g. \"DuckDB warehouse holds 1.59M observations across 10 sources\". \
Phase values: \"analyze\" (understanding code/invariants), \"reproduce\" \
(reproducing a bug), \"edit\" (making changes), \"verify\" (confirming a fix).

## DIRECTIVE (use for: bugs fixed, decisions made, gotchas discovered, patterns established)
A directive tells a future agent WHAT TO DO and WHAT TO AVOID.
{\"type\": \"directive\", \
\"title\": \"<≤80 chars: Fix X in Y or Add Z to W — action-shaped, not a transcript excerpt>\", \
\"phase\": \"<analyze|reproduce|edit|verify — most directives are edit>\", \
\"action\": \"<file path + function name + SPECIFIC code-level change. Include variable names, method calls, conditions. BAD: 'update server start to check Qdrant'. GOOD: 'In scripts/fieldcraft start_server(), add subprocess.run([docker, ps]) check before uvicorn.run() and exit(1) if returncode != 0'>\", \
\"reason\": \"<1-2 sentences: the causal mechanism, not the narrative>\", \
\"files\": [\"<most-important-first>\"], \
\"trigger\": \"<SITUATION that makes this relevant — must include: (1) the specific file or module, AND (2) the task or condition. Format: 'When [doing X] in [file/module Y]'. BAD: 'when working with orders'. GOOD: 'When modifying query logic in src/db/orders.rs or adding new transaction types'>\", \
\"avoid\": [\"<anti-patterns — what NOT to do>\"], \
\"verify\": \"<how to check correctness>\", \
\"invalid_if\": \"<machine-checkable: reference files, imports, config, versions>\", \
\"retrieval_queries\": [\"<2-3 natural language queries that should find this>\"]}

## PROCEDURE (use for: ordered HOW-TO sequences with ≥3 distinct steps to accomplish a goal)
Use procedure — NOT directive — when the chunk lays out a multi-step recipe. Triggers: \
the user/assistant numbered the steps (1, 2, 3, …) OR the chunk has phrases like \
\"first … then … finally\" with ≥3 distinct actions toward one goal.
{\"type\": \"procedure\", \
\"title\": \"<≤80 chars: How to <verb> <object> in <file/module>>\", \
\"phase\": \"<analyze|reproduce|edit|verify>\", \
\"goal\": \"<the outcome the procedure achieves, with module/file specificity>\", \
\"preconditions\": [\"<conditions that must hold before starting — env, version, prior state>\"], \
\"steps\": [\"<step 1: SPECIFIC action with file path, function, command>\", \"<step 2 …>\", \"<step 3 …>\"], \
\"failure_modes\": [\"<known ways this procedure can fail, with the symptom>\"], \
\"affected_files\": [\"<files touched by this procedure>\"], \
\"trigger\": \"<When [doing X] in [module/repo Y]>\", \
\"invalid_if\": \"<machine-checkable staleness>\", \
\"retrieval_questions\": [\"<2-3 queries>\"]}

## EPISODE (use for: a problem discovered + its resolution, as narrative)
Use episode — NOT directive — when the chunk describes (a) a problem that was discovered \
during the session, AND (b) the specific fix or workaround applied. Triggers: words like \
\"discovered\", \"turned out\", \"root cause\", \"the issue was\", followed by a fix.
{\"type\": \"episode\", \
\"title\": \"<≤80 chars: discovery + fix, e.g. 'Race in fetch_queue caused dupe messages: atomic UPDATE'>\", \
\"phase\": \"<analyze|reproduce|edit|verify>\", \
\"summary\": \"<1-3 sentences: what happened, with file/function specificity>\", \
\"trigger\": \"<the symptom/condition that surfaced this — file or behavior pattern>\", \
\"outcome\": \"<the fix applied, with the SPECIFIC change>\", \
\"observations\": [\"<concrete diagnostic facts that pointed to the cause>\"], \
\"affected_files\": [\"<files involved>\"]}

## CLAIM (use for: factual observations about how a system behaves — testable assertions)
Use claim — NOT context — when the chunk states a verifiable fact about a system, \
typically with metrics or specifics. Triggers: numbers, version constraints, capacity \
statements, observed throughput/latency, schema cardinality.
{\"type\": \"claim\", \
\"title\": \"<≤80 chars: system + factual property, e.g. 'DuckDB warehouse: 1.59M observations from 10 sources'>\", \
\"phase\": \"<analyze|reproduce|edit|verify>\", \
\"statement\": \"<the assertion, with measured/observed specifics>\", \
\"mechanism\": \"<why it is so, if known>\", \
\"scope\": \"<file/module where this applies>\", \
\"affected_files\": [\"<files>\"], \
\"retrieval_questions\": [\"<2-3 queries>\"]}

## CONTEXT (use for: code patterns, invariants, traps, structural rules in a module)
A context record describes a PATTERN or INVARIANT a future agent must understand. \
Use context — not directive — when the session surfaces how a system behaves, \
an architectural invariant, a non-obvious API behavior, or a factual observation \
(not a change to make). Examples: SQLite WAL allows concurrent reads; \
config key must match env var exactly; library silently truncates at N bytes.
{\"type\": \"context\", \
\"title\": \"<≤80 chars: system + behavior — e.g. 'SQLite WAL allows concurrent reads'>\", \
\"phase\": \"<analyze|reproduce|edit|verify — most contexts are analyze>\", \
\"statement\": \"<the pattern/invariant, with function names and variable names. BAD: 'the module has some shared state'. GOOD: 'In compiler.py get_combinator_sql(), compiler.query is a shared mutable reference — calling set_values() without clone() mutates all references'>\", \
\"scope\": \"<file path + function/class where this applies>\", \
\"files\": [\"<related files>\"], \
\"implications\": [\"<specific consequence: what breaks if this invariant is violated, with function/variable names>\"], \
\"invalid_if\": \"<machine-checkable staleness>\", \
\"retrieval_queries\": [\"<2-3 queries>\"]}

## DECISION TRAPS — common misclassifications to AVOID

These are real chunks and the CORRECT classification. Study them.

1. User says: \"Create file X with: 1) export A, 2) export B, 3) export C\"
   → procedure (NOT directive). Why: 3 ordered steps toward one goal (creating X).

2. User says: \"we noticed throughput drops to 200 req/s above 1000 connections\"
   → claim (NOT context). Why: factual METRIC with a specific number.

3. Assistant says: \"Bug X turned out to be Y; fixed by changing Z\"
   → episode (NOT directive). Why: discovery + fix narrative.

4. Assistant says: \"SQLite WAL allows concurrent reads but serializes writes\"
   → context (NOT claim). Why: structural pattern, no metric.

5. User says: \"Update orders.rs to validate amount before insert\"
   → directive (correct). Why: SINGLE specific action.

6. Assistant says: \"First, run npm install. Then update tsconfig. Finally restart the dev server.\"
   → procedure (NOT directive). Why: 3 ordered steps.

7. Assistant says: \"The pipeline holds 1.59M observations across 10 sources via DuckDB\"
   → claim (NOT context). Why: cardinality metric.

8. Assistant says: \"Investigated slow query, root cause was missing index on user_id, added it\"
   → episode (NOT directive). Why: investigation + root cause + fix.

RULES:
1. title field: ≤80 chars, action-shaped, summarises the insight. BAD: first sentence \
of the action field truncated. GOOD: \"Fix deadlock in orders.rs pool size\".
2. NEVER write in past tense. Not \"we discovered that X caused Y\" but \"X causes Y; do Z\".
3. action field: file paths first, then the change. \"In src/foo.rs, change X to Y\" not \
\"Y should be used instead of X in the foo module\".
4. avoid field: ACTIVELY SCAN for failed approaches. Look for these patterns: \
developer tried X then switched to Y (avoid X), code was reverted or deleted (avoid that approach), \
explicit \"that didn't work\" or \"wrong approach\" language, workarounds that mask a deeper issue. \
Each entry MUST follow: \"<what not to do> because <specific failure reason>\". \
Example: \"Avoid str.split() for CSV parsing because it breaks on quoted commas\". \
If NO failed approaches exist, leave avoid as an empty array — do not fabricate.
5. invalid_if: must reference something grep-able — a file path, an import, a config key, \
a version string.
6. If the session contained a debugging investigation, extract the FIX as a directive, \
not the INVESTIGATION as a narrative.
7. If nothing in this chunk is worth transferring to a future agent, respond with \
{\"type\": \"skip\"}.
8. Be SPECIFIC. \"Use round() for currency\" beats \"be careful with number formatting\".
9. trigger field: write triggers as IF-THEN conditions a future agent can match against \
its current task. The trigger must be specific enough that it would NOT match unrelated \
work in the same repo. If the trigger would fire on >30% of sessions, it is too broad.
10. KIND CHOICE: if the session chunk describes a factual observation about how a system \
behaves (not a change to make), use context. Reserve directive for actionable changes.
11. PROCEDURE vs DIRECTIVE: if the chunk has ≥3 ordered/numbered steps toward one goal \
(\"1. … 2. … 3. …\" or \"first … then … finally\"), emit PROCEDURE — NOT directive. \
Directive is for SINGLE-action changes. Procedure is for multi-step recipes.
12. EPISODE vs DIRECTIVE: if the chunk describes a PROBLEM that was discovered AND its \
RESOLUTION (root-cause + fix), emit EPISODE — NOT directive. Triggers: \"root cause was X\", \
\"turned out\", \"the bug was\", \"discovered that\". Directive is for changes you decided to make; \
episode is for diagnoses you completed.
13. CLAIM vs CONTEXT: if the chunk states a verifiable factual property (numbers, throughput, \
cardinality, version compatibility), emit CLAIM. If it describes a structural pattern or \
invariant (no metric), emit CONTEXT.

Do NOT extract:
- Descriptions of existing code the assistant merely read (grep can find that)
- Facts obvious from reading the referenced file
- Environment/toolchain issues specific to one machine
- Routine git operations unless they revealed a non-obvious issue
- Routine actions, boilerplate, or conversation narration

Respond with ONLY a JSON object.";

/// Return the production Stage B extraction system prompt.
pub fn unified_system() -> &'static str {
    STAGE_B_SYSTEM
}
// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Result from Stage B for a single chunk.
#[derive(Debug)]
pub struct StageBResult {
    pub chunk_index: usize,
    pub record: KnowledgeRecord,
}

/// Run Stage B on all HIGH chunks. Uses batched prompts for CLI backend
/// or parallel individual calls for API backend.
pub async fn run_stage_b(
    client: Arc<LlmClient>,
    model: &str,
    chunks: &[Chunk],
    stage_a_results: &[StageAResult],
    project_id: &str,
    session_id: &str,
    max_concurrent: usize,
) -> Vec<StageBResult> {
    // Build a map from chunk index → knowledge_type from Stage A
    let type_map: HashMap<usize, Option<String>> = stage_a_results
        .iter()
        .map(|r| (r.chunk_index, r.classification.knowledge_type.clone()))
        .collect();

    // Build a map from chunk index → chunk for context window lookups
    let chunk_map: HashMap<usize, &Chunk> = chunks.iter().map(|c| (c.index, c)).collect();

    // Collect HIGH chunk indices in order
    let high_indices: Vec<usize> = stage_a_results
        .iter()
        .filter(|r| r.is_high)
        .map(|r| r.chunk_index)
        .collect();

    // Prepare extraction tasks with context
    let tasks: Vec<ExtractionTask> = high_indices
        .iter()
        .filter_map(|&chunk_idx| {
            let chunk = (*chunk_map.get(&chunk_idx)?).clone();
            let prev_text = chunk
                .prev_index
                .and_then(|i| chunk_map.get(&i))
                .map(|c| c.text.clone())
                .unwrap_or_default();
            let next_text = chunk
                .next_index
                .and_then(|i| chunk_map.get(&i))
                .map(|c| c.text.clone())
                .unwrap_or_default();
            let knowledge_type = type_map
                .get(&chunk_idx)
                .cloned()
                .flatten()
                .unwrap_or_else(|| "directive".to_string());
            Some(ExtractionTask {
                chunk: chunk.clone(),
                prev_text,
                next_text,
                knowledge_type,
            })
        })
        .collect();

    if client.is_cli() {
        run_stage_b_batched(&client, model, &tasks, project_id, session_id).await
    } else {
        run_stage_b_parallel(
            client,
            model,
            &tasks,
            project_id,
            session_id,
            max_concurrent,
        )
        .await
    }
}

struct ExtractionTask {
    chunk: Chunk,
    prev_text: String,
    next_text: String,
    knowledge_type: String,
}

/// CLI backend: batch 5 chunks per call with unified prompt. No type splitting.
/// Passes already-extracted titles between batches to prevent redundant extraction.
async fn run_stage_b_batched(
    client: &LlmClient,
    model: &str,
    tasks: &[ExtractionTask],
    project_id: &str,
    session_id: &str,
) -> Vec<StageBResult> {
    const BATCH_SIZE: usize = 5;
    let mut results = Vec::new();
    let mut extracted_titles: Vec<String> = Vec::new();

    for batch in tasks.chunks(BATCH_SIZE) {
        let titles_context = build_titles_context(&extracted_titles);

        if batch.len() == 1 {
            // Single chunk: regular prompt
            let task = &batch[0];
            let mut user_prompt =
                build_extraction_prompt(&task.chunk.text, &task.prev_text, &task.next_text);
            if !titles_context.is_empty() {
                user_prompt.push_str(&titles_context);
            }
            if let Ok(raw) = client
                .message(model, unified_system(), &user_prompt, 1024)
                .await
            {
                let json_str = extract_json_object(&raw).unwrap_or(&raw);
                if let Some(kind) = infer_kind_from_json(json_str)
                    && let Ok(record) =
                        build_record(json_str, kind, project_id, session_id, task.chunk.index)
                {
                    extracted_titles.push(record.title.clone());
                    results.push(StageBResult {
                        chunk_index: task.chunk.index,
                        record,
                    });
                }
            }
        } else {
            // Multi-chunk: batch prompt, one CLI call
            let mut prompt = format!(
                "Extract {} separate knowledge records, one from each chunk below.\n\
                 For each, decide the type (directive or context) and include a \"type\" field.\n\
                 Use {{\"type\": \"skip\"}} for chunks with nothing transferable.\n\
                 Respond with a JSON array of {} objects, in order.\n\n",
                batch.len(),
                batch.len()
            );
            for (i, task) in batch.iter().enumerate() {
                prompt.push_str(&format!("=== Chunk {} ===\n", i + 1));
                prompt.push_str(&build_extraction_prompt(
                    &task.chunk.text,
                    &task.prev_text,
                    &task.next_text,
                ));
                prompt.push_str("\n\n");
            }
            if !titles_context.is_empty() {
                prompt.push_str(&titles_context);
                prompt.push('\n');
            }
            prompt.push_str(&format!(
                "Respond with ONLY a JSON array of {} objects, one per chunk, in order.",
                batch.len()
            ));

            if let Ok(raw) = client.message(model, unified_system(), &prompt, 4096).await
                && let Some(start) = raw.find('[')
                && let Some(end) = raw.rfind(']')
            {
                let arr_str = &raw[start..=end];
                if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(arr_str) {
                    for (i, val) in arr.iter().enumerate() {
                        if let Some(task) = batch.get(i) {
                            let json_str = val.to_string();
                            if let Some(kind) = infer_kind_from_json(&json_str)
                                && let Ok(record) = build_record(
                                    &json_str,
                                    kind,
                                    project_id,
                                    session_id,
                                    task.chunk.index,
                                )
                            {
                                extracted_titles.push(record.title.clone());
                                results.push(StageBResult {
                                    chunk_index: task.chunk.index,
                                    record,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    results
}

/// API backend: parallel individual calls (already fast).
async fn run_stage_b_parallel(
    client: Arc<LlmClient>,
    model: &str,
    tasks: &[ExtractionTask],
    project_id: &str,
    session_id: &str,
    max_concurrent: usize,
) -> Vec<StageBResult> {
    let model = model.to_string();
    let project_id = project_id.to_string();
    let session_id = session_id.to_string();

    let mut results: Vec<StageBResult> = Vec::new();
    let mut cursor = 0usize;

    while cursor < tasks.len() {
        let batch_end = (cursor + max_concurrent).min(tasks.len());
        let mut join_set: JoinSet<Option<StageBResult>> = JoinSet::new();

        for task in &tasks[cursor..batch_end] {
            let chunk = task.chunk.clone();
            let prev_text = task.prev_text.clone();
            let next_text = task.next_text.clone();
            let knowledge_type = task.knowledge_type.clone();
            let client = Arc::clone(&client);
            let model = model.clone();
            let project_id = project_id.clone();
            let session_id = session_id.clone();

            join_set.spawn(async move {
                extract_chunk(
                    &client,
                    &model,
                    &chunk,
                    &prev_text,
                    &next_text,
                    &knowledge_type,
                    &project_id,
                    &session_id,
                )
                .await
                .ok()
            });
        }

        while let Some(res) = join_set.join_next().await {
            if let Ok(Some(stage_b_result)) = res {
                results.push(stage_b_result);
            }
        }

        cursor = batch_end;
    }

    results
}

// ---------------------------------------------------------------------------
// Per-chunk extraction
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn extract_chunk(
    client: &LlmClient,
    model: &str,
    chunk: &Chunk,
    prev_context: &str,
    next_context: &str,
    _knowledge_type: &str,
    project_id: &str,
    session_id: &str,
) -> Result<StageBResult, TrError> {
    let user_prompt = build_extraction_prompt(&chunk.text, prev_context, next_context);

    let raw = client
        .message(model, unified_system(), &user_prompt, 1024)
        .await?;

    let json_str = extract_json_object(&raw).unwrap_or(&raw);

    // Determine type from the response JSON — skip if model says nothing transferable
    let kind = infer_kind_from_json(json_str).ok_or_else(|| TrError::Parse {
        message: "Stage B: model returned skip — nothing transferable".to_string(),
    })?;
    let record = build_record(json_str, kind, project_id, session_id, chunk.index)?;

    Ok(StageBResult {
        chunk_index: chunk.index,
        record,
    })
}

/// Infer KnowledgeKind from the JSON response's "type" field.
fn infer_kind_from_json(json_str: &str) -> Option<KnowledgeKind> {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
        match val.get("type").and_then(|t| t.as_str()) {
            Some("directive") => Some(KnowledgeKind::Directive),
            Some("context") => Some(KnowledgeKind::Context),
            Some("claim") => Some(KnowledgeKind::Directive), // remap legacy
            Some("procedure") => Some(KnowledgeKind::Directive), // remap legacy
            Some("episode") => Some(KnowledgeKind::Directive), // remap legacy
            Some("skip") => None,
            _ => Some(KnowledgeKind::Directive),
        }
    } else {
        Some(KnowledgeKind::Directive)
    }
}

/// Build a context string listing already-extracted titles so the model avoids redundancy.
fn build_titles_context(titles: &[String]) -> String {
    if titles.is_empty() {
        return String::new();
    }
    let mut ctx = String::from(
        "\nAlready extracted from earlier chunks (do NOT extract the same fact again):\n",
    );
    for title in titles.iter().rev().take(30) {
        ctx.push_str("- ");
        ctx.push_str(title);
        ctx.push('\n');
    }
    ctx
}

fn build_extraction_prompt(chunk_text: &str, prev: &str, next: &str) -> String {
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

/// Extracts an explicit `title` field from the Stage B JSON if present (≤80 chars).
/// Falls back to truncating `fallback` to 80 chars.
fn extract_title_or_truncate(json_str: &str, fallback: &str) -> String {
    serde_json::from_str::<serde_json::Value>(json_str)
        .ok()
        .and_then(|v| {
            v.get("title")
                .and_then(|t| t.as_str())
                .map(|s| s.chars().take(80).collect::<String>())
        })
        .unwrap_or_else(|| fallback.chars().take(80).collect::<String>())
}

pub fn build_record(
    json_str: &str,
    kind: KnowledgeKind,
    project_id: &str,
    session_id: &str,
    chunk_index: usize,
) -> Result<KnowledgeRecord, TrError> {
    // Default phase by kind: Directive → Edit, Context → Analyze.
    // Overridden if the extraction JSON explicitly specifies a phase.
    let default_phase = match kind {
        KnowledgeKind::Directive => Some(KnowledgePhase::Edit),
        KnowledgeKind::Context => Some(KnowledgePhase::Analyze),
        _ => None,
    };
    let explicit_phase: Option<KnowledgePhase> =
        serde_json::from_str::<serde_json::Value>(json_str)
            .ok()
            .and_then(|v| v.get("phase").and_then(|p| p.as_str()).map(String::from))
            .and_then(|s| KnowledgePhase::parse(&s));
    let phase = explicit_phase.or(default_phase);

    let (payload_json, title, retrieval_text) = match kind {
        KnowledgeKind::Claim => {
            let extraction: ClaimExtraction =
                serde_json::from_str(json_str).map_err(|e| TrError::Parse {
                    message: format!("Stage B: failed to parse claim JSON: {e}\nRaw: {json_str}"),
                })?;

            let title = extract_title_or_truncate(json_str, &extraction.statement);
            let retrieval_text = build_claim_retrieval_text(&extraction);
            let payload = ClaimPayload {
                statement: extraction.statement,
                mechanism: extraction.mechanism,
                scope: extraction.scope,
                exceptions: extraction.exceptions.unwrap_or_default(),
                affected_files: extraction.affected_files.unwrap_or_default(),
                retrieval_questions: extraction.retrieval_questions.unwrap_or_default(),
                trigger: extraction.trigger,
                invalid_if: extraction.invalid_if,
            };
            let payload_json = serde_json::to_string(&payload).map_err(|e| TrError::Internal {
                message: format!("failed to serialize ClaimPayload: {e}"),
            })?;
            (payload_json, title, retrieval_text)
        }
        KnowledgeKind::Procedure => {
            let extraction: ProcedureExtraction =
                serde_json::from_str(json_str).map_err(|e| TrError::Parse {
                    message: format!(
                        "Stage B: failed to parse procedure JSON: {e}\nRaw: {json_str}"
                    ),
                })?;

            let title = extract_title_or_truncate(json_str, &extraction.goal);
            let retrieval_text = build_procedure_retrieval_text(&extraction);
            let payload = ProcedurePayload {
                goal: extraction.goal,
                preconditions: extraction.preconditions.unwrap_or_default(),
                steps: extraction.steps.unwrap_or_default(),
                failure_modes: extraction.failure_modes.unwrap_or_default(),
                affected_files: extraction.affected_files.unwrap_or_default(),
                retrieval_questions: extraction.retrieval_questions.unwrap_or_default(),
                trigger: extraction.trigger,
                invalid_if: extraction.invalid_if,
            };
            let payload_json = serde_json::to_string(&payload).map_err(|e| TrError::Internal {
                message: format!("failed to serialize ProcedurePayload: {e}"),
            })?;
            (payload_json, title, retrieval_text)
        }
        KnowledgeKind::Episode => {
            let extraction: EpisodeExtraction =
                serde_json::from_str(json_str).map_err(|e| TrError::Parse {
                    message: format!("Stage B: failed to parse episode JSON: {e}\nRaw: {json_str}"),
                })?;

            let title = extract_title_or_truncate(json_str, &extraction.summary);
            let retrieval_text = {
                let mut parts = Vec::new();
                if let Some(ref da) = extraction.do_action {
                    parts.push(format!("DO: {da}"));
                }
                parts.push(format!("BECAUSE: {}", extraction.summary));
                if let Some(ref o) = extraction.outcome {
                    parts.push(o.clone());
                }
                if let Some(ref af) = extraction.affected_files
                    && !af.is_empty()
                {
                    parts.push(format!("Files: {}", af.join(", ")));
                }
                parts.join(" ")
            };
            let payload = EpisodePayload {
                summary: extraction.summary,
                trigger: extraction.trigger,
                outcome: extraction.outcome,
                observations: extraction.observations.unwrap_or_default(),
                affected_files: extraction.affected_files.unwrap_or_default(),
                derived_claim_ids: vec![],
                derived_procedure_ids: vec![],
            };
            let payload_json = serde_json::to_string(&payload).map_err(|e| TrError::Internal {
                message: format!("failed to serialize EpisodePayload: {e}"),
            })?;
            (payload_json, title, retrieval_text)
        }
        KnowledgeKind::Directive => {
            let extraction: DirectiveExtraction =
                serde_json::from_str(json_str).map_err(|e| TrError::Parse {
                    message: format!(
                        "Stage B: failed to parse directive JSON: {e}\nRaw: {json_str}"
                    ),
                })?;

            let title = extract_title_or_truncate(json_str, &extraction.action);
            let mut parts = Vec::new();
            parts.push(format!("DO: {}", extraction.action));
            if let Some(ref avoid) = extraction.avoid
                && !avoid.is_empty()
            {
                parts.push(format!("AVOID: {}", avoid.join("; ")));
            }
            parts.push(format!("WHY: {}", extraction.reason));
            if let Some(ref rqs) = extraction.retrieval_queries {
                parts.extend(rqs.clone());
            }
            let retrieval_text = parts.join(" ");
            let payload = DirectivePayload {
                action: extraction.action,
                reason: extraction.reason,
                files: extraction.files.unwrap_or_default(),
                trigger: extraction.trigger.unwrap_or_default(),
                avoid: extraction.avoid.unwrap_or_default(),
                verify: extraction.verify,
                invalid_if: extraction.invalid_if,
                retrieval_queries: extraction.retrieval_queries.unwrap_or_default(),
            };
            let payload_json = serde_json::to_string(&payload).map_err(|e| TrError::Internal {
                message: format!("failed to serialize DirectivePayload: {e}"),
            })?;
            (payload_json, title, retrieval_text)
        }
        KnowledgeKind::Context => {
            let extraction: ContextExtraction =
                serde_json::from_str(json_str).map_err(|e| TrError::Parse {
                    message: format!("Stage B: failed to parse context JSON: {e}\nRaw: {json_str}"),
                })?;

            let title = extract_title_or_truncate(json_str, &extraction.statement);
            let mut parts = Vec::new();
            parts.push(extraction.statement.clone());
            if let Some(ref scope) = extraction.scope {
                parts.push(format!("SCOPE: {scope}"));
            }
            if let Some(ref impls) = extraction.implications {
                parts.extend(impls.clone());
            }
            if let Some(ref rqs) = extraction.retrieval_queries {
                parts.extend(rqs.clone());
            }
            let retrieval_text = parts.join(" ");
            let payload = ContextPayload {
                statement: extraction.statement,
                scope: extraction.scope.unwrap_or_default(),
                files: extraction.files.unwrap_or_default(),
                implications: extraction.implications.unwrap_or_default(),
                invalid_if: extraction.invalid_if,
                retrieval_queries: extraction.retrieval_queries.unwrap_or_default(),
            };
            let payload_json = serde_json::to_string(&payload).map_err(|e| TrError::Internal {
                message: format!("failed to serialize ContextPayload: {e}"),
            })?;
            (payload_json, title, retrieval_text)
        }
    };

    // Generate a stable ID based on content hash
    let id = generate_id(project_id, session_id, chunk_index, &payload_json);

    Ok(KnowledgeRecord {
        protected: false,
        id,
        project_id: project_id.to_string(),
        kind,
        title,
        payload_json,
        state: KnowledgeState::Provisional,
        confidence: 0.5,
        source: KnowledgeSource::Session {
            session_id: session_id.to_string(),
            tool: AgentTool::ClaudeCode,
        },
        superseded_by: None,
        project_specific: true,
        created_at: chrono::Utc::now(),
        valid_until: None,
        retrieval_text,
        schema_version: 4,
        phase,
        tier: fieldcraft_core::KnowledgeTier::Active,
        owner: None,
        session_date: None,
        source_refs: vec![SourceReference {
            source_id: session_id.to_string(),
            chunk_index: Some(chunk_index),
        }],
    })
}

fn build_claim_retrieval_text(e: &ClaimExtraction) -> String {
    let mut parts = Vec::new();
    if let Some(ref da) = e.do_action {
        parts.push(format!("DO: {da}"));
    }
    parts.push(format!("BECAUSE: {}", e.statement));
    if let Some(ref m) = e.mechanism {
        parts.push(m.clone());
    }
    if let Some(ref inv) = e.invalid_if {
        parts.push(format!("CHECK: {inv}"));
    }
    if let Some(ref rqs) = e.retrieval_questions {
        parts.extend(rqs.clone());
    }
    parts.join(" ")
}

fn build_procedure_retrieval_text(e: &ProcedureExtraction) -> String {
    let mut parts = Vec::new();
    if let Some(ref da) = e.do_action {
        parts.push(format!("DO: {da}"));
    }
    parts.push(format!("BECAUSE: {}", e.goal));
    if let Some(ref rqs) = e.retrieval_questions {
        parts.extend(rqs.clone());
    }
    if let Some(ref inv) = e.invalid_if {
        parts.push(format!("CHECK: {inv}"));
    }
    parts.join(" ")
}

fn generate_id(project_id: &str, session_id: &str, chunk_index: usize, payload: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(project_id.as_bytes());
    hasher.update(b"|");
    hasher.update(session_id.as_bytes());
    hasher.update(b"|");
    hasher.update(chunk_index.to_string().as_bytes());
    hasher.update(b"|");
    hasher.update(payload.as_bytes());
    format!("kr_{}", &format!("{:x}", hasher.finalize())[..16])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_claim_extraction() {
        let json = r#"{
            "statement": "PostgreSQL advisory locks on orders deadlock when pool > 20.",
            "mechanism": "Lock acquisition order is non-deterministic under high concurrency.",
            "scope": "orders table under SERIALIZABLE isolation",
            "exceptions": ["not applicable with READ COMMITTED"],
            "affected_files": ["src/db/orders.rs"],
            "retrieval_questions": ["Why does orders deadlock?", "PostgreSQL pool size limit?"],
            "trigger": "High concurrency order processing",
            "invalid_if": "if src/db/orders.rs no longer uses advisory locks"
        }"#;

        let extraction: ClaimExtraction = serde_json::from_str(json).unwrap();
        assert!(extraction.statement.contains("deadlock"));
        assert_eq!(
            extraction.affected_files.as_ref().unwrap()[0],
            "src/db/orders.rs"
        );
        assert_eq!(extraction.retrieval_questions.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn test_parse_procedure_extraction() {
        let json = r#"{
            "goal": "Deploy to staging environment",
            "preconditions": ["All tests pass", "CI green"],
            "steps": ["Run cargo build --release", "Push to staging branch", "Wait for CD"],
            "failure_modes": ["CD timeout: check Heroku logs"],
            "affected_files": [".github/workflows/deploy.yml"],
            "retrieval_questions": ["How to deploy to staging?"],
            "trigger": "When feature is ready for QA",
            "invalid_if": "if .github/workflows/deploy.yml is removed"
        }"#;

        let extraction: ProcedureExtraction = serde_json::from_str(json).unwrap();
        assert_eq!(extraction.goal, "Deploy to staging environment");
        assert_eq!(extraction.steps.as_ref().unwrap().len(), 3);
    }

    #[test]
    fn test_parse_episode_extraction() {
        let json = r#"{
            "summary": "Fixed deadlock in order processing by reducing pool size.",
            "trigger": "Production alert: orders timing out",
            "outcome": "Pool size reduced from 25 to 15, deadlocks eliminated",
            "observations": ["Deadlock only occurs under SERIALIZABLE isolation"],
            "affected_files": ["src/db/pool.rs"]
        }"#;

        let extraction: EpisodeExtraction = serde_json::from_str(json).unwrap();
        assert!(extraction.summary.contains("deadlock"));
        assert_eq!(
            extraction.affected_files.as_ref().unwrap()[0],
            "src/db/pool.rs"
        );
    }

    #[test]
    fn test_build_record_from_claim_json() {
        let json = r#"{
            "statement": "Test statement",
            "mechanism": "Test mechanism",
            "scope": null,
            "exceptions": [],
            "affected_files": ["src/foo.rs"],
            "retrieval_questions": ["How does foo work?"],
            "trigger": "When foo is called",
            "invalid_if": "if src/foo.rs is deleted"
        }"#;

        let record = build_record(json, KnowledgeKind::Claim, "project1", "session1", 0).unwrap();
        assert_eq!(record.kind, KnowledgeKind::Claim);
        assert_eq!(record.state, KnowledgeState::Provisional);
        assert!((record.confidence - 0.5).abs() < f32::EPSILON);
        assert!(!record.id.is_empty());
        assert!(record.retrieval_text.contains("Test statement"));
        assert_eq!(record.source_refs.len(), 1);
        assert_eq!(record.source_refs[0].source_id, "session1");
        assert_eq!(record.source_refs[0].chunk_index, Some(0));
    }

    #[test]
    fn test_extract_json_object_nested() {
        let s = r#"Sure! {"outer": {"inner": "value"}} done"#;
        let extracted = extract_json_object(s).unwrap();
        assert_eq!(extracted, r#"{"outer": {"inner": "value"}}"#);
    }

    #[test]
    fn test_generate_id_deterministic() {
        let id1 = generate_id("proj", "sess", 0, "payload");
        let id2 = generate_id("proj", "sess", 0, "payload");
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_generate_id_different_for_different_inputs() {
        let id1 = generate_id("proj", "sess", 0, "payload1");
        let id2 = generate_id("proj", "sess", 0, "payload2");
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_build_extraction_prompt_includes_context() {
        let prompt = build_extraction_prompt("main chunk", "prev content", "next content");
        assert!(prompt.contains("prev content"));
        assert!(prompt.contains("main chunk"));
        assert!(prompt.contains("next content"));
        assert!(prompt.contains("Target chunk"));
        assert!(prompt.contains("Previous context"));
    }

    #[test]
    fn unified_system_returns_production_prompt() {
        assert_eq!(unified_system(), STAGE_B_SYSTEM);
        assert!(unified_system().contains("DECISION TRAPS"));
    }
}
