//! Deterministic citation-based outcome detection for retrieved lessons.
//!
//! Phase 5A of cue-bundle retrieval — the pure core of the trace analyzer.
//! Given a [`KnowledgeRecord`] and a chunk of agent output text (subsequent
//! reasoning, tool calls, edits), decides whether the record's distinctive
//! content appears in the output.
//!
//! Algorithm:
//! 1. Extract a list of distinctive tokens from the record (title +
//!    kind-specific payload fields) via the same heuristic
//!    [`fieldcraft_core::cue_extract::extract_distinctive_terms`] uses for
//!    cue extraction — CamelCase, SCREAMING_SNAKE, acronyms, digit-bearing
//!    identifiers, file paths.
//! 2. Substring-match each token in the agent output with **word-boundary
//!    enforcement** so e.g. `auth` doesn't match inside `author`.
//! 3. Return [`CitationVerdict::Cited`] on first match,
//!    [`CitationVerdict::Ignored`] if none match.
//!
//! Design choices, justified:
//! - **Pure functions, no I/O.** Phase 5B will plug these into a transcript
//!   scanner; keeping them isolated makes precision auditable.
//! - **Conservative.** False negatives (missed citations) leave a record
//!   with stale confidence; false positives corrupt scoring permanently.
//!   The token-not-phrase approach biases toward false negatives.
//! - **Case-insensitive matching.** Agents often re-case identifiers.
//! - **Word-boundary enforcement.** Without it, common substrings produce
//!   constant false positives.

use std::collections::HashSet;

use fieldcraft_core::{
    ClaimPayload, ContextPayload, DirectivePayload, EpisodePayload, KnowledgeKind, KnowledgeRecord,
    ProcedurePayload, cue_extract,
};

/// Verdict for a single (record, output) pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CitationVerdict {
    /// At least one distinctive token from the record appears in the output
    /// with word-boundary enforcement.
    Cited,
    /// No distinctive token matched. May be a false negative if all
    /// distinctive content was paraphrased.
    Ignored,
}

/// Extract distinctive tokens from a record. Uses
/// [`cue_extract::extract_distinctive_terms`] over the title plus
/// kind-specific payload fields. File paths are included verbatim since
/// they're distinctive on their own and don't always survive the term
/// heuristic.
///
/// The returned list is deduplicated case-insensitively and ordered by
/// first appearance.
pub fn extract_distinctive_content(record: &KnowledgeRecord) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let push = |s: String, out: &mut Vec<String>, seen: &mut HashSet<String>| {
        let key = s.to_lowercase();
        if seen.insert(key) {
            out.push(s);
        }
    };

    // Title tokens always included.
    for t in cue_extract::extract_distinctive_terms(&record.title, 8) {
        push(t, &mut out, &mut seen);
    }

    // Kind-specific payload extraction.
    match record.kind {
        KnowledgeKind::Procedure => {
            if let Ok(p) = serde_json::from_str::<ProcedurePayload>(&record.payload_json) {
                let combined = format!(
                    "{} {} {}",
                    p.goal,
                    p.steps.join(" "),
                    p.affected_files.join(" ")
                );
                for t in cue_extract::extract_distinctive_terms(&combined, 16) {
                    push(t, &mut out, &mut seen);
                }
                for f in p.affected_files {
                    push(f, &mut out, &mut seen);
                }
            }
        }
        KnowledgeKind::Directive => {
            if let Ok(p) = serde_json::from_str::<DirectivePayload>(&record.payload_json) {
                let combined = format!("{} {} {}", p.action, p.trigger, p.files.join(" "));
                for t in cue_extract::extract_distinctive_terms(&combined, 16) {
                    push(t, &mut out, &mut seen);
                }
                for f in p.files {
                    push(f, &mut out, &mut seen);
                }
            }
        }
        KnowledgeKind::Episode => {
            if let Ok(p) = serde_json::from_str::<EpisodePayload>(&record.payload_json) {
                let mut combined = format!("{} {}", p.summary, p.affected_files.join(" "));
                if let Some(t) = &p.trigger {
                    combined.push(' ');
                    combined.push_str(t);
                }
                if let Some(o) = &p.outcome {
                    combined.push(' ');
                    combined.push_str(o);
                }
                for t in cue_extract::extract_distinctive_terms(&combined, 16) {
                    push(t, &mut out, &mut seen);
                }
                for f in p.affected_files {
                    push(f, &mut out, &mut seen);
                }
            }
        }
        KnowledgeKind::Claim => {
            if let Ok(p) = serde_json::from_str::<ClaimPayload>(&record.payload_json) {
                let mut combined = format!("{} {}", p.statement, p.affected_files.join(" "));
                if let Some(m) = &p.mechanism {
                    combined.push(' ');
                    combined.push_str(m);
                }
                if let Some(s) = &p.scope {
                    combined.push(' ');
                    combined.push_str(s);
                }
                for t in cue_extract::extract_distinctive_terms(&combined, 16) {
                    push(t, &mut out, &mut seen);
                }
                for f in p.affected_files {
                    push(f, &mut out, &mut seen);
                }
            }
        }
        KnowledgeKind::Context => {
            if let Ok(p) = serde_json::from_str::<ContextPayload>(&record.payload_json) {
                let combined = format!("{} {} {}", p.statement, p.scope, p.files.join(" "));
                for t in cue_extract::extract_distinctive_terms(&combined, 16) {
                    push(t, &mut out, &mut seen);
                }
                for f in p.files {
                    push(f, &mut out, &mut seen);
                }
            }
        }
    }

    out
}

/// Search `agent_output` (case-insensitively, with word-boundary enforcement)
/// for any token in `distinctive_content`.
///
/// Returns [`CitationVerdict::Cited`] on first match, [`CitationVerdict::Ignored`]
/// if none match or if either input is empty.
pub fn check_citation(distinctive_content: &[String], agent_output: &str) -> CitationVerdict {
    if distinctive_content.is_empty() || agent_output.is_empty() {
        return CitationVerdict::Ignored;
    }
    let haystack = agent_output.to_lowercase();
    for token in distinctive_content {
        if token.is_empty() {
            continue;
        }
        let needle = token.to_lowercase();
        if substring_with_word_boundary(&haystack, &needle) {
            return CitationVerdict::Cited;
        }
    }
    CitationVerdict::Ignored
}

/// Case-aware substring match with alphanumeric/underscore word-boundary
/// enforcement on both ends. Both `haystack` and `needle` should already be
/// in the same case (the caller lowercases).
fn substring_with_word_boundary(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    let mut i = 0;
    while i + n.len() <= h.len() {
        if &h[i..i + n.len()] == n {
            let before_ok = i == 0 || !is_word_byte(h[i - 1]);
            let after_ok = i + n.len() == h.len() || !is_word_byte(h[i + n.len()]);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use fieldcraft_core::{
        AgentTool, KnowledgeKind, KnowledgeRecord, KnowledgeSource, KnowledgeState, KnowledgeTier,
    };

    fn record_with(kind: KnowledgeKind, title: &str, payload_json: &str) -> KnowledgeRecord {
        KnowledgeRecord {
            protected: false,
            id: "test-id".to_string(),
            project_id: "p".to_string(),
            kind,
            title: title.to_string(),
            payload_json: payload_json.to_string(),
            state: KnowledgeState::Active,
            confidence: 0.5,
            source: KnowledgeSource::Session {
                session_id: "s".to_string(),
                tool: AgentTool::ClaudeCode,
            },
            superseded_by: None,
            project_specific: true,
            created_at: Utc::now(),
            valid_until: None,
            retrieval_text: String::new(),
            schema_version: 4,
            phase: None,
            tier: KnowledgeTier::Active,
            owner: None,
            session_date: None,
            source_refs: vec![],
        }
    }

    // ---- Word-boundary substring match -------------------------------------

    #[test]
    fn word_boundary_rejects_internal_match() {
        // "auth" should not match inside "author"
        assert!(!substring_with_word_boundary("the author writes", "auth"));
    }

    #[test]
    fn word_boundary_accepts_standalone() {
        assert!(substring_with_word_boundary(
            "checking auth middleware",
            "auth"
        ));
    }

    #[test]
    fn word_boundary_accepts_at_start_and_end() {
        assert!(substring_with_word_boundary("auth", "auth"));
        assert!(substring_with_word_boundary("auth happens", "auth"));
        assert!(substring_with_word_boundary("happens auth", "auth"));
    }

    #[test]
    fn word_boundary_accepts_punctuation_neighbors() {
        assert!(substring_with_word_boundary("call auth() now", "auth"));
        assert!(substring_with_word_boundary("/auth/login", "auth"));
        assert!(substring_with_word_boundary("file: src/auth.rs", "auth.rs"));
    }

    #[test]
    fn word_boundary_underscore_blocks_match() {
        // "auth" should not match inside "auth_token" because _ is a word byte
        assert!(!substring_with_word_boundary("auth_token check", "auth"));
    }

    // ---- check_citation ----------------------------------------------------

    #[test]
    fn check_citation_returns_ignored_for_empty_content() {
        assert_eq!(check_citation(&[], "some output"), CitationVerdict::Ignored);
    }

    #[test]
    fn check_citation_returns_ignored_for_empty_output() {
        assert_eq!(
            check_citation(&["KnowledgeRecord".to_string()], ""),
            CitationVerdict::Ignored
        );
    }

    #[test]
    fn check_citation_returns_cited_on_distinctive_match() {
        let content = vec!["KnowledgeRecord".to_string(), "P0_WEIGHTS".to_string()];
        let output = "I updated the P0_WEIGHTS array.";
        assert_eq!(check_citation(&content, output), CitationVerdict::Cited);
    }

    #[test]
    fn check_citation_is_case_insensitive() {
        let content = vec!["KnowledgeRecord".to_string()];
        let output = "the knowledgerecord struct";
        assert_eq!(check_citation(&content, output), CitationVerdict::Cited);
    }

    #[test]
    fn check_citation_respects_word_boundary() {
        let content = vec!["auth".to_string()];
        // No standalone "auth" — only inside "author"
        let output = "The author of this section";
        assert_eq!(check_citation(&content, output), CitationVerdict::Ignored);
    }

    #[test]
    fn check_citation_skips_empty_tokens() {
        let content = vec!["".to_string(), "P0_WEIGHTS".to_string()];
        let output = "P0_WEIGHTS array";
        assert_eq!(check_citation(&content, output), CitationVerdict::Cited);
    }

    // ---- extract_distinctive_content (per kind) ----------------------------

    #[test]
    fn extract_distinctive_content_directive_includes_files() {
        let payload = r#"{
            "action": "Use cargo test before commit",
            "reason": "catch regressions",
            "files": ["crates/fieldcraft-core/src/lib.rs"],
            "trigger": "pre-commit",
            "avoid": [],
            "verify": null,
            "invalid_if": null,
            "retrieval_queries": []
        }"#;
        let r = record_with(KnowledgeKind::Directive, "Use cargo test", payload);
        let tokens = extract_distinctive_content(&r);
        assert!(
            tokens
                .iter()
                .any(|t| t == "crates/fieldcraft-core/src/lib.rs"),
            "expected file path in tokens, got {tokens:?}"
        );
    }

    #[test]
    fn extract_distinctive_content_procedure_includes_code_idents() {
        let payload = r#"{
            "goal": "Refresh RetrievalSignals after RRF",
            "preconditions": [],
            "steps": ["Call load_retrieval_signals_for after hybrid_search"],
            "failure_modes": [],
            "affected_files": ["crates/fieldcraft-retrieval/src/sqlite_search.rs"],
            "retrieval_questions": [],
            "trigger": null,
            "invalid_if": null
        }"#;
        let r = record_with(KnowledgeKind::Procedure, "Refresh signals", payload);
        let tokens = extract_distinctive_content(&r);
        // CamelCase / snake_case identifiers should surface
        assert!(
            tokens
                .iter()
                .any(|t| t == "RetrievalSignals" || t == "load_retrieval_signals_for"),
            "expected code identifier, got {tokens:?}"
        );
        assert!(
            tokens
                .iter()
                .any(|t| t == "crates/fieldcraft-retrieval/src/sqlite_search.rs"),
            "expected file path, got {tokens:?}"
        );
    }

    #[test]
    fn extract_distinctive_content_episode_includes_paths_and_distinctive_terms() {
        let payload = r#"{
            "summary": "Hit database is locked when running extract_batch_e2e",
            "trigger": "cargo test parallel",
            "outcome": "Added HomeRedirect Mutex",
            "observations": [],
            "affected_files": ["crates/fieldcraft-cli/tests/extract_batch_e2e.rs"],
            "derived_claim_ids": [],
            "derived_procedure_ids": []
        }"#;
        let r = record_with(KnowledgeKind::Episode, "Parallel HOME race", payload);
        let tokens = extract_distinctive_content(&r);
        assert!(
            tokens
                .iter()
                .any(|t| t == "crates/fieldcraft-cli/tests/extract_batch_e2e.rs"),
            "expected file path, got {tokens:?}"
        );
        assert!(
            tokens.iter().any(|t| t == "HomeRedirect"),
            "expected CamelCase identifier, got {tokens:?}"
        );
    }

    #[test]
    fn extract_distinctive_content_claim_uses_statement_and_mechanism() {
        let payload = r#"{
            "statement": "P0_WEIGHTS is a 12-feature linear scorer",
            "mechanism": "weighted sum via ScoringFeatures",
            "scope": "crates/fieldcraft-retrieval/src/retrieval.rs",
            "exceptions": [],
            "affected_files": [],
            "retrieval_questions": [],
            "trigger": null,
            "invalid_if": null
        }"#;
        let r = record_with(KnowledgeKind::Claim, "Scoring weights", payload);
        let tokens = extract_distinctive_content(&r);
        assert!(
            tokens.iter().any(|t| t == "P0_WEIGHTS"),
            "expected SCREAMING_SNAKE, got {tokens:?}"
        );
        assert!(
            tokens.iter().any(|t| t == "ScoringFeatures"),
            "expected CamelCase, got {tokens:?}"
        );
    }

    #[test]
    fn extract_distinctive_content_context_includes_scope_files() {
        let payload = r#"{
            "statement": "fieldcraft uses BGE-Small embeddings",
            "scope": "retrieval pipeline",
            "files": ["crates/fieldcraft-retrieval/src/lib.rs"],
            "implications": [],
            "invalid_if": null,
            "retrieval_queries": []
        }"#;
        let r = record_with(KnowledgeKind::Context, "Embedding model", payload);
        let tokens = extract_distinctive_content(&r);
        assert!(
            tokens
                .iter()
                .any(|t| t == "crates/fieldcraft-retrieval/src/lib.rs"),
            "expected file path, got {tokens:?}"
        );
    }

    #[test]
    fn extract_distinctive_content_is_deduplicated_case_insensitively() {
        // Same token in title and payload should appear only once.
        let payload = r#"{
            "statement": "P0_WEIGHTS controls scoring; p0_weights is the constant",
            "exceptions": [],
            "affected_files": [],
            "retrieval_questions": []
        }"#;
        let r = record_with(KnowledgeKind::Claim, "P0_WEIGHTS overview", payload);
        let tokens = extract_distinctive_content(&r);
        let lower: Vec<String> = tokens.iter().map(|t| t.to_lowercase()).collect();
        let count = lower.iter().filter(|t| *t == "p0_weights").count();
        assert_eq!(
            count, 1,
            "p0_weights should appear once after dedup, got {tokens:?}"
        );
    }

    #[test]
    fn extract_distinctive_content_returns_empty_on_garbage_payload() {
        let r = record_with(
            KnowledgeKind::Directive,
            "the and for with",
            r#"{"action":"the and for with","reason":"","files":[],"trigger":"","avoid":[],"verify":null,"invalid_if":null,"retrieval_queries":[]}"#,
        );
        let tokens = extract_distinctive_content(&r);
        assert!(
            tokens.is_empty(),
            "stop-word-only content should yield nothing, got {tokens:?}"
        );
    }

    // ---- End-to-end: extract + check ---------------------------------------

    #[test]
    fn full_pipeline_citation_detected_when_agent_uses_file_path() {
        let payload = r#"{
            "action": "Edit cue_match_factor",
            "reason": "...",
            "files": ["crates/fieldcraft-retrieval/src/cue_match.rs"],
            "trigger": "...",
            "avoid": [],
            "verify": null,
            "invalid_if": null,
            "retrieval_queries": []
        }"#;
        let r = record_with(KnowledgeKind::Directive, "Edit cue_match_factor", payload);
        let content = extract_distinctive_content(&r);
        let agent_output =
            "Opening crates/fieldcraft-retrieval/src/cue_match.rs to review the function";
        assert_eq!(
            check_citation(&content, agent_output),
            CitationVerdict::Cited
        );
    }

    #[test]
    fn full_pipeline_no_citation_when_agent_paraphrases() {
        let payload = r#"{
            "action": "Use cargo test before commit",
            "reason": "catch regressions",
            "files": ["crates/fieldcraft-core/src/lib.rs"],
            "trigger": "pre-commit",
            "avoid": [],
            "verify": null,
            "invalid_if": null,
            "retrieval_queries": []
        }"#;
        let r = record_with(
            KnowledgeKind::Directive,
            "Run tests before committing",
            payload,
        );
        let content = extract_distinctive_content(&r);
        // Agent agrees in spirit but uses paraphrase — no distinctive tokens
        let agent_output = "Let me verify the build is healthy before merging.";
        assert_eq!(
            check_citation(&content, agent_output),
            CitationVerdict::Ignored
        );
    }
}
