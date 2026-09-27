//! Cheap, local friction scoring for Claude Code / Codex / Grok JSONL transcripts.
//!
//! Used by the Stop-hook distiller gate. No LLM. Signals are taken only from
//! markers the session formats actually emit (interrupts, permission denials,
//! user corrections, tool failures, same-approach retries). Quiet happy-path
//! sessions should score near zero.

use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::OnceLock;

/// Default minimum score before the Stop hook may spawn an LLM distiller.
pub const DEFAULT_MIN_FRICTION: u32 = 3;

pub const WEIGHT_INTERRUPT: u32 = 3;
pub const WEIGHT_REJECTION: u32 = 3;
pub const WEIGHT_USER_CORRECTION: u32 = 2;
pub const WEIGHT_RETRY: u32 = 2;
pub const WEIGHT_TOOL_FAILURE: u32 = 1;

/// Per-signal counts observed in one transcript.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrictionCounts {
    pub interrupt: u32,
    pub rejection: u32,
    pub user_correction: u32,
    pub tool_failure: u32,
    pub retry: u32,
}

impl FrictionCounts {
    /// Compact `key:count` list, or `none`. Stable key order for grep/tests.
    pub fn reasons_compact(&self) -> String {
        let parts: Vec<String> = [
            ("interrupt", self.interrupt),
            ("rejection", self.rejection),
            ("user_correction", self.user_correction),
            ("tool_failure", self.tool_failure),
            ("retry", self.retry),
        ]
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .map(|(k, n)| format!("{k}:{n}"))
        .collect();
        if parts.is_empty() {
            "none".to_string()
        } else {
            parts.join(",")
        }
    }

    pub fn score(&self) -> u32 {
        self.interrupt * WEIGHT_INTERRUPT
            + self.rejection * WEIGHT_REJECTION
            + self.user_correction * WEIGHT_USER_CORRECTION
            + self.retry * WEIGHT_RETRY
            + self.tool_failure * WEIGHT_TOOL_FAILURE
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrictionReport {
    pub score: u32,
    pub counts: FrictionCounts,
}

impl FrictionReport {
    pub fn from_counts(counts: FrictionCounts) -> Self {
        Self {
            score: counts.score(),
            counts,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistillDecision {
    Run,
    Skip,
}

impl DistillDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            DistillDecision::Run => "run",
            DistillDecision::Skip => "skip",
        }
    }
}

/// Decide whether the LLM distiller should run.
///
/// `min_friction == 0` disables the score gate (always run, unless the caller
/// already decided not to). `force` always runs.
pub fn distill_decision(score: u32, min_friction: u32, force: bool) -> DistillDecision {
    if force || min_friction == 0 || score >= min_friction {
        DistillDecision::Run
    } else {
        DistillDecision::Skip
    }
}

/// One-line, machine-grepable gate record.
///
/// Example: `decision=skip score=1 threshold=3 reasons=tool_failure:1`
pub fn format_gate_line(report: &FrictionReport, min_friction: u32, force: bool) -> String {
    let decision = distill_decision(report.score, min_friction, force);
    let (score_label, reasons) = if force {
        ("forced".to_string(), "FIELDCRAFT_DISTILL_FORCE".to_string())
    } else {
        (report.score.to_string(), report.counts.reasons_compact())
    };
    format!(
        "decision={} score={} threshold={} reasons={}",
        decision.as_str(),
        score_label,
        min_friction,
        reasons
    )
}

pub fn env_force_enabled() -> bool {
    matches!(
        std::env::var("FIELDCRAFT_DISTILL_FORCE").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
    )
}

/// Score a JSONL transcript in memory.
pub fn score_transcript(input: &str) -> FrictionReport {
    let mut counts = FrictionCounts::default();
    let mut last_failed_sig: Option<String> = None;
    let mut tool_ids: HashMap<String, String> = HashMap::new();

    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        score_value(&v, &mut counts, &mut last_failed_sig, &mut tool_ids);
    }

    FrictionReport::from_counts(counts)
}

/// Score a JSONL transcript file.
pub fn score_transcript_path(path: &Path) -> io::Result<FrictionReport> {
    let input = std::fs::read_to_string(path)?;
    Ok(score_transcript(&input))
}

fn score_value(
    v: &Value,
    counts: &mut FrictionCounts,
    last_failed_sig: &mut Option<String>,
    tool_ids: &mut HashMap<String, String>,
) {
    let event_type = first_str(v, &["/type", "/payload/type", "/event"]).unwrap_or("");

    if is_interrupt_event(event_type, v) {
        counts.interrupt += 1;
    }

    if is_rejection_event(event_type, v) {
        counts.rejection += 1;
    }

    // Codex / Claude tool-use records.
    if let Some(tool) = extract_tool_use(v, tool_ids)
        && let Some(failed) = last_failed_sig.as_ref()
        && signatures_match(failed, &tool.signature)
    {
        counts.retry += 1;
        last_failed_sig.take();
    }

    if let Some(result) = extract_tool_result(v, tool_ids) {
        if result.is_failure {
            counts.tool_failure += 1;
            *last_failed_sig = Some(result.signature);
        }
        if result.is_rejection {
            counts.rejection += 1;
        }
        if result.is_interrupt {
            counts.interrupt += 1;
        }
    }

    if let Some(user_text) = extract_human_user_text(v) {
        if interrupt_text(&user_text) {
            counts.interrupt += 1;
        } else if rejection_text(&user_text) {
            counts.rejection += 1;
        } else if user_correction_text(&user_text) {
            counts.user_correction += 1;
        }
    }
}

fn is_interrupt_event(event_type: &str, v: &Value) -> bool {
    let t = event_type.to_ascii_lowercase();
    if matches!(
        t.as_str(),
        "turn_aborted" | "turn_interrupted" | "interrupted" | "user_interrupt"
    ) {
        return true;
    }
    if let Some(reason) = first_str(v, &["/payload/reason", "/reason", "/payload/abort_reason"]) {
        let r = reason.to_ascii_lowercase();
        if r.contains("interrupt") || r == "user" {
            return true;
        }
    }
    false
}

fn is_rejection_event(event_type: &str, v: &Value) -> bool {
    let t = event_type.to_ascii_lowercase();
    if t.contains("approval")
        && let Some(decision) = first_str(
            v,
            &[
                "/payload/decision",
                "/decision",
                "/payload/approved",
                "/approved",
            ],
        )
    {
        let d = decision.to_ascii_lowercase();
        return matches!(
            d.as_str(),
            "deny" | "denied" | "reject" | "rejected" | "false"
        );
    }
    false
}

struct ToolUse {
    signature: String,
}

struct ToolResult {
    signature: String,
    is_failure: bool,
    is_rejection: bool,
    is_interrupt: bool,
}

fn extract_tool_use(v: &Value, tool_ids: &mut HashMap<String, String>) -> Option<ToolUse> {
    // Codex function_call
    if v.pointer("/payload/type").and_then(|x| x.as_str()) == Some("function_call") {
        let name = first_str(v, &["/payload/name", "/name"]).unwrap_or("unknown");
        let args = v
            .pointer("/payload/arguments")
            .or_else(|| v.get("arguments"));
        let sig = tool_signature(name, args);
        if let Some(id) = first_str(v, &["/payload/call_id", "/call_id", "/payload/id"]) {
            tool_ids.insert(id.to_string(), sig.clone());
        }
        return Some(ToolUse { signature: sig });
    }

    // Claude tool_use blocks (assistant content array)
    for pointer in ["/message/content", "/content", "/payload/message/content"] {
        if let Some(arr) = v.pointer(pointer).and_then(|c| c.as_array()) {
            for block in arr {
                if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    let name = block
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("unknown");
                    let sig = tool_signature(name, block.get("input"));
                    if let Some(id) = block
                        .get("id")
                        .or_else(|| block.get("tool_use_id"))
                        .and_then(|x| x.as_str())
                    {
                        tool_ids.insert(id.to_string(), sig.clone());
                    }
                    return Some(ToolUse { signature: sig });
                }
            }
        }
    }
    None
}

fn extract_tool_result(v: &Value, tool_ids: &HashMap<String, String>) -> Option<ToolResult> {
    // Codex function_call_output
    if v.pointer("/payload/type").and_then(|x| x.as_str()) == Some("function_call_output") {
        let id = first_str(v, &["/payload/call_id", "/call_id"]).unwrap_or("");
        let signature = tool_ids
            .get(id)
            .cloned()
            .unwrap_or_else(|| "unknown:".to_string());
        let output = extract_output_text(v);
        let exit_code = first_i64(
            v,
            &[
                "/payload/metadata/exit_code",
                "/payload/exit_code",
                "/metadata/exit_code",
                "/exit_code",
            ],
        )
        .or_else(|| nested_output_exit_code(v));
        let is_error_flag = bool_at(v, &["/payload/is_error", "/is_error"]).unwrap_or(false);
        let is_rejection = rejection_text(&output);
        let is_interrupt = interrupt_text(&output);
        let is_exec = signature_is_exec(&signature);
        let is_failure = !is_rejection
            && !is_interrupt
            && (is_error_flag
                || exit_code.is_some_and(|c| c != 0)
                || (is_exec && exec_failure_text(&output)));
        return Some(ToolResult {
            signature,
            is_failure,
            is_rejection,
            is_interrupt,
        });
    }

    // Claude tool_result blocks (usually wrapped in a user turn)
    for pointer in ["/message/content", "/content", "/payload/message/content"] {
        if let Some(arr) = v.pointer(pointer).and_then(|c| c.as_array()) {
            for block in arr {
                if block.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                    continue;
                }
                let id = block
                    .get("tool_use_id")
                    .or_else(|| block.get("toolUseId"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let signature = tool_ids
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| "unknown:".to_string());
                let output = block_text(block);
                let is_error_flag = block
                    .get("is_error")
                    .or_else(|| block.get("isError"))
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false);
                let is_rejection = rejection_text(&output);
                let is_interrupt = interrupt_text(&output);
                let is_exec = signature_is_exec(&signature);
                let is_failure = !is_rejection
                    && !is_interrupt
                    && (is_error_flag || (is_exec && exec_failure_text(&output)));
                return Some(ToolResult {
                    signature,
                    is_failure,
                    is_rejection,
                    is_interrupt,
                });
            }
        }
    }
    None
}

fn extract_human_user_text(v: &Value) -> Option<String> {
    if !is_user_turn(v) {
        return None;
    }
    // Skip turns that are only tool_result wrappers.
    if content_is_tool_results_only(v) {
        return None;
    }
    let text = collect_user_text(v);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if is_injected_or_wrapper(trimmed) {
        return None;
    }
    Some(trimmed.to_string())
}

fn is_user_turn(v: &Value) -> bool {
    if v.get("type").and_then(|s| s.as_str()) == Some("user") {
        return true;
    }
    let role = first_str(
        v,
        &[
            "/role",
            "/message/role",
            "/payload/role",
            "/payload/message/role",
        ],
    );
    if role == Some("user") {
        return true;
    }
    // Codex event_msg user_message
    v.pointer("/payload/type").and_then(|s| s.as_str()) == Some("user_message")
}

fn content_is_tool_results_only(v: &Value) -> bool {
    for pointer in ["/message/content", "/content", "/payload/message/content"] {
        if let Some(arr) = v.pointer(pointer).and_then(|c| c.as_array()) {
            if arr.is_empty() {
                continue;
            }
            return arr.iter().all(|b| {
                matches!(
                    b.get("type").and_then(|t| t.as_str()),
                    Some("tool_result") | Some("tool_use")
                )
            });
        }
    }
    false
}

fn collect_user_text(v: &Value) -> String {
    let mut out = String::new();
    for pointer in [
        "/message/content",
        "/content",
        "/payload/message/content",
        "/payload/content",
    ] {
        if let Some(content) = v.pointer(pointer) {
            push_content(content, &mut out);
        }
    }
    for pointer in [
        "/text",
        "/message/text",
        "/payload/text",
        "/payload/message",
        "/payload/message/text",
    ] {
        if let Some(text) = v.pointer(pointer).and_then(|t| t.as_str())
            && !out.contains(text)
        {
            out.push_str(text);
            out.push('\n');
        }
    }
    out
}

fn push_content(content: &Value, out: &mut String) {
    match content {
        Value::String(s) => {
            out.push_str(s);
            out.push('\n');
        }
        Value::Array(arr) => {
            for block in arr {
                // Skip tool_result blocks mixed into a user turn.
                if matches!(
                    block.get("type").and_then(|t| t.as_str()),
                    Some("tool_result") | Some("tool_use")
                ) {
                    continue;
                }
                let text = block_text(block);
                if !text.is_empty() {
                    out.push_str(&text);
                    out.push('\n');
                }
            }
        }
        _ => {}
    }
}

fn block_text(block: &Value) -> String {
    if let Some(s) = block.get("text").and_then(|t| t.as_str()) {
        return s.to_string();
    }
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn extract_output_text(v: &Value) -> String {
    if let Some(s) = first_str(v, &["/payload/output", "/output", "/payload/content"]) {
        // Codex sometimes stores a JSON blob in `output`.
        if let Ok(inner) = serde_json::from_str::<Value>(s)
            && let Some(inner_s) = first_str(&inner, &["/output", "/content", "/text"])
        {
            return inner_s.to_string();
        }
        return s.to_string();
    }
    String::new()
}

fn nested_output_exit_code(v: &Value) -> Option<i64> {
    let raw = first_str(v, &["/payload/output", "/output"])?;
    let inner: Value = serde_json::from_str(raw).ok()?;
    first_i64(&inner, &["/metadata/exit_code", "/exit_code"])
}

fn tool_signature(name: &str, input: Option<&Value>) -> String {
    let parsed_owned;
    let input = match input {
        Some(Value::String(s)) => {
            parsed_owned = serde_json::from_str::<Value>(s).ok();
            parsed_owned.as_ref()
        }
        other => other,
    };
    let primary = input
        .and_then(|inp| {
            first_str(
                inp,
                &[
                    "/command",
                    "/cmd",
                    "/file_path",
                    "/path",
                    "/target_file",
                    "/script",
                ],
            )
        })
        .unwrap_or("");
    let primary = if signature_is_exec(&format!("{}:", name)) {
        primary.split_whitespace().next().unwrap_or(primary)
    } else {
        primary
    };
    format!(
        "{}:{}",
        name.to_ascii_lowercase(),
        primary.to_ascii_lowercase()
    )
}

fn signature_is_exec(signature: &str) -> bool {
    signature.starts_with("bash:")
        || signature.starts_with("shell:")
        || signature.starts_with("bashoutput:")
}

fn signatures_match(failed: &str, next: &str) -> bool {
    if failed.is_empty() || next.is_empty() {
        return false;
    }
    if failed == next {
        return true;
    }
    // Same tool + same primary token (already in the signature).
    let (f_name, f_prim) = failed.split_once(':').unwrap_or((failed, ""));
    let (n_name, n_prim) = next.split_once(':').unwrap_or((next, ""));
    f_name == n_name && !f_prim.is_empty() && f_prim == n_prim
}

fn is_injected_or_wrapper(text: &str) -> bool {
    text.contains("FIELDCRAFT LEARNING SYSTEM")
        || text.contains("<command-message>")
        || text.contains("<command-name>")
        || text.contains("<local-command-stdout>")
        || text.contains("BEFORE STOPPING:")
}

fn interrupt_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("[request interrupted")
        || lower.contains("request interrupted by user")
        || lower.contains("interrupted by the user")
}

fn rejection_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("doesn't want to proceed with this tool use")
        || lower.contains("does not want to proceed with this tool use")
        || lower.contains("the tool use was rejected")
        || lower.contains("user rejected")
        || lower.contains("user declined")
}

fn user_correction_text(text: &str) -> bool {
    // Long pastes (error logs, diffs) are not corrections.
    if text.len() > 600 {
        return false;
    }
    let trimmed = text.trim();
    let lower = trimmed.to_ascii_lowercase();
    if matches!(
        lower.trim_end_matches('.'),
        "no" | "nope" | "stop" | "undo" | "revert" | "wrong"
    ) {
        return true;
    }
    user_correction_re().is_match(&lower)
}

fn exec_failure_text(text: &str) -> bool {
    exec_failure_re().is_match(text)
}

fn user_correction_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(that'?s (not|wrong|incorrect)|that is (not|wrong|incorrect)|not what i (said|asked|meant)|i (said|asked for|meant|told you)|try again|do it again|undo that|\bundo\b|\brevert\b|stop doing|no,? (not|that'?s|i meant)|don'?t (do|use|change|touch|edit) )",
        )
        .expect("user correction regex")
    })
}

fn exec_failure_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?m)(exit code[: ]+[1-9]|command not found|error\[E\d+\]|panicked at|^error:|^Error:|^fatal:|^FATAL:|[1-9]\d* failed|tests failed|\bFAILED\b)",
        )
        .expect("exec failure regex")
    })
}

fn first_str<'a>(v: &'a Value, pointers: &[&str]) -> Option<&'a str> {
    for p in pointers {
        if let Some(s) = v.pointer(p).and_then(|x| x.as_str()) {
            return Some(s);
        }
    }
    None
}

fn first_i64(v: &Value, pointers: &[&str]) -> Option<i64> {
    for p in pointers {
        if let Some(n) = v.pointer(p).and_then(|x| x.as_i64()) {
            return Some(n);
        }
        if let Some(n) = v.pointer(p).and_then(|x| x.as_u64()) {
            return Some(n as i64);
        }
    }
    None
}

fn bool_at(v: &Value, pointers: &[&str]) -> Option<bool> {
    for p in pointers {
        if let Some(b) = v.pointer(p).and_then(|x| x.as_bool()) {
            return Some(b);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUIET_CLAUDE: &str = include_str!("../tests/fixtures/quiet_claude.jsonl");
    const PAINFUL_CLAUDE: &str = include_str!("../tests/fixtures/painful_claude.jsonl");
    const QUIET_CODEX: &str = include_str!("../tests/fixtures/quiet_codex.jsonl");
    const PAINFUL_CODEX: &str = include_str!("../tests/fixtures/painful_codex.jsonl");

    #[test]
    fn quiet_claude_scores_below_default_threshold() {
        let report = score_transcript(QUIET_CLAUDE);
        assert!(
            report.score < DEFAULT_MIN_FRICTION,
            "quiet claude score={} reasons={}",
            report.score,
            report.counts.reasons_compact()
        );
        assert_eq!(report.counts.reasons_compact(), "none");
        assert_eq!(
            distill_decision(report.score, DEFAULT_MIN_FRICTION, false),
            DistillDecision::Skip
        );
    }

    #[test]
    fn painful_claude_scores_at_or_above_default_threshold() {
        let report = score_transcript(PAINFUL_CLAUDE);
        assert!(
            report.score >= DEFAULT_MIN_FRICTION,
            "painful claude score={} reasons={}",
            report.score,
            report.counts.reasons_compact()
        );
        assert!(report.counts.interrupt >= 1);
        assert!(report.counts.rejection >= 1);
        assert!(report.counts.user_correction >= 1);
        assert!(report.counts.tool_failure >= 1);
        assert!(report.counts.retry >= 1);
        let line = format_gate_line(&report, DEFAULT_MIN_FRICTION, false);
        assert!(line.starts_with("decision=run "));
        assert!(line.contains("reasons="));
        assert!(line.contains("interrupt:"));
        assert_eq!(
            distill_decision(report.score, DEFAULT_MIN_FRICTION, false),
            DistillDecision::Run
        );
    }

    #[test]
    fn quiet_codex_scores_below_default_threshold() {
        let report = score_transcript(QUIET_CODEX);
        assert!(
            report.score < DEFAULT_MIN_FRICTION,
            "quiet codex score={} reasons={}",
            report.score,
            report.counts.reasons_compact()
        );
        assert_eq!(
            distill_decision(report.score, DEFAULT_MIN_FRICTION, false),
            DistillDecision::Skip
        );
    }

    #[test]
    fn painful_codex_scores_at_or_above_default_threshold() {
        let report = score_transcript(PAINFUL_CODEX);
        assert!(
            report.score >= DEFAULT_MIN_FRICTION,
            "painful codex score={} reasons={}",
            report.score,
            report.counts.reasons_compact()
        );
        assert!(report.counts.interrupt >= 1);
        assert!(report.counts.user_correction >= 1);
        assert!(report.counts.tool_failure >= 1);
        assert_eq!(
            distill_decision(report.score, DEFAULT_MIN_FRICTION, false),
            DistillDecision::Run
        );
    }

    #[test]
    fn passing_cargo_output_is_not_a_tool_failure() {
        let jsonl = r#"
{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"test result: ok. 2 passed; 0 failed"}]}}
"#;
        let report = score_transcript(jsonl);
        assert_eq!(report.counts.tool_failure, 0);
        assert_eq!(report.score, 0);
    }

    #[test]
    fn isolated_tool_failure_is_not_enough() {
        let jsonl = r#"
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"run the tests"}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"exit code 1"}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"one test failed; fixing it."}]}}
"#;
        let report = score_transcript(jsonl);
        assert_eq!(report.counts.tool_failure, 1);
        assert_eq!(report.score, WEIGHT_TOOL_FAILURE);
        assert_eq!(
            distill_decision(report.score, DEFAULT_MIN_FRICTION, false),
            DistillDecision::Skip
        );
    }

    #[test]
    fn long_user_paste_with_error_is_not_a_correction() {
        let paste = "error: something failed\n".repeat(40);
        let jsonl = format!(
            r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"text","text":"{}"}}]}}}}"#,
            paste.replace('\n', "\\n")
        );
        let report = score_transcript(&jsonl);
        assert_eq!(report.counts.user_correction, 0);
        assert_eq!(report.score, 0);
    }

    #[test]
    fn force_and_zero_threshold_always_run() {
        assert_eq!(
            distill_decision(0, DEFAULT_MIN_FRICTION, true),
            DistillDecision::Run
        );
        assert_eq!(distill_decision(0, 0, false), DistillDecision::Run);
        let report = FrictionReport::from_counts(FrictionCounts::default());
        let line = format_gate_line(&report, DEFAULT_MIN_FRICTION, true);
        assert_eq!(
            line,
            "decision=run score=forced threshold=3 reasons=FIELDCRAFT_DISTILL_FORCE"
        );
    }

    #[test]
    fn empty_transcript_is_quiet() {
        let report = score_transcript("");
        assert_eq!(report.score, 0);
        assert_eq!(report.counts.reasons_compact(), "none");
    }

    #[test]
    fn grok_style_user_correction_counts() {
        let jsonl = r#"
{"role":"user","content":"add a helper for the date formatter"}
{"role":"assistant","content":"I'll add a small helper."}
{"role":"user","content":"that's wrong — I said use ISO-8601, try again"}
"#;
        let report = score_transcript(jsonl);
        assert!(report.counts.user_correction >= 1);
        assert!(report.score >= WEIGHT_USER_CORRECTION);
    }
}
