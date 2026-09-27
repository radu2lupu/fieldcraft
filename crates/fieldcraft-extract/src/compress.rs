use serde_json::Value;

use crate::patterns::{self, OutputType, detect_output_type, strip_ansi};
use crate::transcript::{ContentBlock, TranscriptMessage};

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum CompressionRule {
    ToolOutputStripping,
    ThinkingBlockRemoval,
    CycleCollapsing,
    NarrationStripping,
    CodeBlockTruncation,
    BoilerplateStripping,
}

#[derive(Debug, Clone)]
pub struct RuleApplication {
    pub rule: CompressionRule,
    pub tokens_removed: usize,
    pub instances: usize,
}

#[derive(Debug)]
pub struct CompressedTranscript {
    pub text: String,
    pub original_tokens: usize,
    pub compressed_tokens: usize,
    pub compression_ratio: f32,
    pub rules_applied: Vec<RuleApplication>,
}

#[derive(Debug, Clone)]
pub struct TranscriptCompressor {
    pub tool_output_max_lines: usize,
    pub code_block_max_lines: usize,
    pub collapse_cycles: bool,
    pub strip_narration: bool,
}

impl Default for TranscriptCompressor {
    fn default() -> Self {
        Self {
            tool_output_max_lines: 20,
            code_block_max_lines: 10,
            collapse_cycles: true,
            strip_narration: true,
        }
    }
}

// ── Token estimation (1 token ≈ 4 chars) ─────────────────────────────────────

fn estimate_tokens(s: &str) -> usize {
    s.len().div_ceil(4)
}

// ── Line helpers ──────────────────────────────────────────────────────────────

fn head_tail(lines: &[&str], head: usize, tail: usize) -> String {
    if lines.len() <= head + tail {
        return lines.join("\n");
    }
    let mut out = lines[..head].join("\n");
    out.push_str(&format!(
        "\n[... {} lines omitted ...]\n",
        lines.len() - head - tail
    ));
    out.push_str(&lines[lines.len() - tail..].join("\n"));
    out
}

// ── Tool result compression (Rule 1) ─────────────────────────────────────────

fn compress_tool_use_input(name: &str, input: &Value) -> String {
    match name {
        "Edit" | "MultiEdit" => {
            let path = input
                .get("file_path")
                .or_else(|| input.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let old_first = input
                .get("old_string")
                .and_then(|v| v.as_str())
                .and_then(|s| s.lines().next())
                .unwrap_or("");
            let new_first = input
                .get("new_string")
                .and_then(|v| v.as_str())
                .and_then(|s| s.lines().next())
                .unwrap_or("");
            format!("[tool: edit, path: {path}, old: {old_first:?}, new: {new_first:?}]")
        }
        "Write" => {
            let path = input
                .get("file_path")
                .or_else(|| input.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let first5: Vec<&str> = content.lines().take(5).collect();
            format!(
                "[tool: write, path: {path}, first 5 lines: {}]",
                first5.join(" | ")
            )
        }
        _ => {
            // For other tool uses, render as compact JSON
            format!("[tool_use: {name}, input: {}]", input)
        }
    }
}

fn compress_tool_result(tool_name: &str, raw_content: &str) -> (String, usize) {
    let content = strip_ansi(raw_content);
    let original_len = content.len();
    let lines: Vec<&str> = content.lines().collect();
    let max = 20usize;

    if lines.len() <= max {
        return (content.clone(), 0);
    }

    let compressed = match tool_name {
        "Read" => {
            let path_hint = lines
                .first()
                .copied()
                .unwrap_or("")
                .trim_start_matches("```")
                .trim();
            let first3: Vec<&str> = lines.iter().take(3).copied().collect();
            let last3: Vec<&str> = lines.iter().rev().take(3).rev().copied().collect();
            format!(
                "[tool: read_file, {}, {} lines, first 3 + last 3 lines shown]\n{}\n...\n{}",
                path_hint,
                lines.len(),
                first3.join("\n"),
                last3.join("\n")
            )
        }
        "Bash" => {
            let output_type = detect_output_type(&content);
            match output_type {
                OutputType::TestRunner => {
                    // Keep only failure lines + summary
                    let failure_lines: Vec<&str> = lines
                        .iter()
                        .filter(|l| patterns::test_failure_line_re().is_match(l))
                        .copied()
                        .collect();
                    if failure_lines.is_empty() {
                        // All tests passed — just keep the summary line
                        let summary = lines.last().copied().unwrap_or("(no output)");
                        format!("[test output: all passed] {summary}")
                    } else {
                        format!("[test output: failures]\n{}", failure_lines.join("\n"))
                    }
                }
                OutputType::CompilerOutput => {
                    // Keep only errors and warnings
                    let error_lines: Vec<&str> = lines
                        .iter()
                        .filter(|l| patterns::compiler_error_line_re().is_match(l))
                        .copied()
                        .collect();
                    format!(
                        "[compiler output: {} errors/warnings]\n{}",
                        error_lines.len(),
                        error_lines.join("\n")
                    )
                }
                OutputType::BuildProgress => {
                    // Keep first + last lines (summary)
                    format!(
                        "[build output: {} lines]\n{}",
                        lines.len(),
                        head_tail(&lines, 2, 2)
                    )
                }
                OutputType::Generic | OutputType::FileContent => {
                    format!(
                        "[bash output: {} lines]\n{}",
                        lines.len(),
                        head_tail(&lines, 10, 10)
                    )
                }
            }
        }
        "Grep" | "Glob" => {
            let first20: Vec<&str> = lines.iter().take(20).copied().collect();
            format!(
                "[search results: {} matches, showing first 20]\n{}",
                lines.len(),
                first20.join("\n")
            )
        }
        _ => {
            format!(
                "[tool {tool_name} output: {} lines]\n{}",
                lines.len(),
                head_tail(&lines, 10, 10)
            )
        }
    };

    let saved = original_len.saturating_sub(compressed.len());
    (compressed, saved)
}

// ── Code block truncation helper ──────────────────────────────────────────────

fn truncate_code_blocks(text: &str, max_lines: usize) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut saved = 0usize;
    let mut in_code_block = false;
    let mut block_lines: Vec<&str> = Vec::new();
    let mut fence = "";

    for line in text.lines() {
        let trimmed = line.trim_start();
        if !in_code_block && (trimmed.starts_with("```") || trimmed.starts_with("~~~")) {
            in_code_block = true;
            fence = if trimmed.starts_with("```") {
                "```"
            } else {
                "~~~"
            };
            block_lines.clear();
            out.push_str(line);
            out.push('\n');
        } else if in_code_block && (trimmed.starts_with(fence)) {
            in_code_block = false;
            if block_lines.len() > max_lines {
                let half = max_lines / 2;
                let orig: String = block_lines.join("\n");
                let truncated = format!(
                    "{}\n[... {} lines omitted ...]\n{}",
                    block_lines[..half].join("\n"),
                    block_lines.len() - half * 2,
                    block_lines[block_lines.len() - half..].join("\n")
                );
                saved += orig.len().saturating_sub(truncated.len());
                out.push_str(&truncated);
                out.push('\n');
            } else {
                for bl in &block_lines {
                    out.push_str(bl);
                    out.push('\n');
                }
            }
            block_lines.clear();
            out.push_str(line);
            out.push('\n');
        } else if in_code_block {
            block_lines.push(line);
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }

    // Flush any unclosed block
    for bl in &block_lines {
        out.push_str(bl);
        out.push('\n');
    }

    (out, saved)
}

// ── Boilerplate stripping helper ─────────────────────────────────────────────

/// Strip AGENTS.md headers, <INSTRUCTIONS> blocks, and system prompt boilerplate.
fn strip_boilerplate(text: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut saved = 0usize;
    let mut in_instructions_block = false;

    for line in text.lines() {
        let trimmed = line.trim();

        // Detect <INSTRUCTIONS> block start
        if trimmed.starts_with("<INSTRUCTIONS>") || trimmed.starts_with("<instructions>") {
            in_instructions_block = true;
            saved += line.len() + 1;
            continue;
        }

        // Detect </INSTRUCTIONS> block end
        if in_instructions_block {
            saved += line.len() + 1;
            if trimmed.starts_with("</INSTRUCTIONS>") || trimmed.starts_with("</instructions>") {
                in_instructions_block = false;
            }
            continue;
        }

        // Skip AGENTS.md header lines
        if trimmed == "# AGENTS.md"
            || trimmed.starts_with("## Role and Purpose")
            || trimmed.starts_with("## Communication Style")
            || trimmed.starts_with("## Key Behaviors")
            || trimmed.starts_with("## Tool Usage")
        {
            saved += line.len() + 1;
            continue;
        }

        out.push_str(line);
        out.push('\n');
    }

    (out, saved)
}

// ── Narration stripping helper ────────────────────────────────────────────────

fn strip_narration_lines(text: &str) -> (String, usize) {
    let re = patterns::narration_re();
    let decision_re = patterns::decision_re();
    let mut out = String::with_capacity(text.len());
    let mut saved = 0usize;

    for line in text.lines() {
        // Never strip if line contains decision keywords
        if decision_re.is_match(line) {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if re.is_match(line) {
            saved += line.len() + 1;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }

    (out, saved)
}

// ── Cycle detection ───────────────────────────────────────────────────────────

/// Very lightweight cycle detector: looks for alternating edit/test sequences
/// in the rendered text output. Returns (collapsed_text, tokens_removed).
fn collapse_edit_test_cycles(messages: &[RenderedMessage]) -> (Vec<RenderedMessage>, usize) {
    // Mark tool-use messages that are edits and tool-result messages that look like tests
    // We work at message level: find runs of [edit, bash-test, (fail), edit, bash-test, ...]
    // and collapse them.

    #[derive(PartialEq)]
    enum MsgKind {
        Edit(String), // file path
        TestFail,
        TestPass,
        Other,
    }

    fn classify(msg: &RenderedMessage) -> MsgKind {
        if msg.text.contains("[tool: edit,") {
            // Extract path
            let path = msg
                .text
                .split("path: ")
                .nth(1)
                .and_then(|s| s.split(',').next())
                .unwrap_or("?")
                .to_string();
            return MsgKind::Edit(path);
        }
        if msg.text.contains("[test output: failures]") {
            return MsgKind::TestFail;
        }
        if msg.text.contains("[test output: all passed]") {
            return MsgKind::TestPass;
        }
        MsgKind::Other
    }

    let mut result: Vec<RenderedMessage> = Vec::new();
    let mut i = 0;
    let mut total_saved = 0usize;

    while i < messages.len() {
        // Try to detect: Edit, TestFail, [Edit, TestFail]+ ..., Edit, TestPass
        if let MsgKind::Edit(ref file) = classify(&messages[i]) {
            let file = file.clone();
            let cycle_start = i;
            let mut cycle_count = 0usize;
            let mut last_error = String::new();
            let mut j = i;

            loop {
                if j >= messages.len() {
                    break;
                }
                match classify(&messages[j]) {
                    MsgKind::Edit(_) => {
                        j += 1;
                        cycle_count += 1;
                    }
                    MsgKind::TestFail => {
                        last_error = messages[j]
                            .text
                            .lines()
                            .find(|l| patterns::test_failure_line_re().is_match(l))
                            .unwrap_or("")
                            .to_string();
                        j += 1;
                    }
                    MsgKind::TestPass => {
                        // Cycle ends here with success
                        if cycle_count > 1 {
                            let summary = format!(
                                "[cycle collapsed: edited {file} to fix {last_error:?}, tests passed after {cycle_count} iterations]"
                            );
                            let chars_saved: usize = messages[cycle_start..j]
                                .iter()
                                .map(|m| m.text.len())
                                .sum::<usize>()
                                .saturating_sub(summary.len());
                            total_saved += chars_saved;
                            result.push(RenderedMessage {
                                role: "assistant".to_string(),
                                text: summary,
                            });
                            i = j + 1;
                        } else {
                            // Only 1 edit → not worth collapsing
                            result.push(messages[i].clone());
                            i += 1;
                        }
                        break;
                    }
                    MsgKind::Other => break,
                }
            }

            if i == cycle_start {
                result.push(messages[i].clone());
                i += 1;
            }
        } else {
            result.push(messages[i].clone());
            i += 1;
        }
    }

    (result, total_saved / 4)
}

// ── Rendered message (intermediate) ──────────────────────────────────────────

#[derive(Clone)]
struct RenderedMessage {
    role: String,
    text: String,
}

// ── Main compressor ───────────────────────────────────────────────────────────

impl TranscriptCompressor {
    pub fn compress(&self, messages: &[TranscriptMessage]) -> CompressedTranscript {
        let original_text = render_messages_raw(messages);
        let original_tokens = estimate_tokens(&original_text);

        let mut rule_stats: std::collections::HashMap<&str, (usize, usize)> =
            std::collections::HashMap::new();

        // Step 1: Render messages, applying thinking removal + tool output stripping
        let mut rendered: Vec<RenderedMessage> = Vec::new();
        let mut thinking_chars_removed = 0usize;
        let mut thinking_instances = 0usize;
        let mut tool_chars_removed = 0usize;
        let mut tool_instances = 0usize;
        // Track last seen tool name across message boundaries (tool_use and tool_result
        // can appear in separate messages in Claude Code's JSONL format).
        let mut last_tool_name = String::from("Unknown");

        for msg in messages {
            // Skip developer/system messages — Claude Code internals, not meaningful content
            if matches!(msg.role.as_str(), "developer" | "system") {
                continue;
            }
            let role = msg.role.clone();
            let is_user = role == "user";
            let mut parts: Vec<String> = Vec::new();

            for block in &msg.content {
                match block {
                    ContentBlock::Thinking(t) => {
                        // Rule 2: strip thinking blocks entirely
                        thinking_chars_removed += t.len();
                        thinking_instances += 1;
                    }
                    ContentBlock::Text(t) => {
                        parts.push(t.clone());
                    }
                    ContentBlock::ToolUse { name, input } => {
                        last_tool_name = name.clone();
                        let rendered_use = compress_tool_use_input(name, input);
                        tool_instances += 1;
                        parts.push(rendered_use);
                    }
                    ContentBlock::ToolResult { content, .. } => {
                        // Prefer in-message context, fall back to cross-message last tool name.
                        let tool_name = {
                            let inferred = infer_tool_name_from_context(&parts);
                            if inferred == "Unknown" {
                                last_tool_name.clone()
                            } else {
                                inferred
                            }
                        };
                        let (compressed, saved) = if is_user {
                            (content.clone(), 0)
                        } else {
                            compress_tool_result(&tool_name, content)
                        };
                        tool_chars_removed += saved;
                        tool_instances += 1;
                        parts.push(compressed);
                    }
                }
            }

            rendered.push(RenderedMessage {
                role,
                text: parts.join("\n"),
            });
        }

        rule_stats.insert("thinking", (thinking_chars_removed / 4, thinking_instances));
        rule_stats.insert("tool", (tool_chars_removed / 4, tool_instances));

        // Step 2: Cycle collapsing (Rule 3)
        let cycle_tokens_removed;
        let cycle_instances;
        if self.collapse_cycles {
            let (collapsed, saved) = collapse_edit_test_cycles(&rendered);
            cycle_instances = rendered.len() - collapsed.len();
            cycle_tokens_removed = saved;
            rendered = collapsed;
        } else {
            cycle_tokens_removed = 0;
            cycle_instances = 0;
        }
        rule_stats.insert("cycle", (cycle_tokens_removed, cycle_instances));

        // Step 3: Narration stripping + code block truncation + boilerplate removal
        let mut narration_chars_removed = 0usize;
        let mut narration_instances = 0usize;
        let mut code_chars_removed = 0usize;
        let mut code_instances = 0usize;
        let mut boilerplate_chars_removed = 0usize;
        let mut boilerplate_instances = 0usize;

        for msg in rendered.iter_mut() {
            // Never strip user messages
            if msg.role == "user" {
                continue;
            }

            // Strip AGENTS.md and <INSTRUCTIONS> boilerplate
            let (cleaned, saved) = strip_boilerplate(&msg.text);
            if saved > 0 {
                boilerplate_instances += 1;
                boilerplate_chars_removed += saved;
                msg.text = cleaned;
            }

            // Preserve decision-keyword sentences always
            if self.strip_narration {
                let (stripped, saved) = strip_narration_lines(&msg.text);
                if saved > 0 {
                    narration_instances += 1;
                    narration_chars_removed += saved;
                    msg.text = stripped;
                }
            }

            // Truncate code blocks
            let (truncated, saved) = truncate_code_blocks(&msg.text, self.code_block_max_lines);
            if saved > 0 {
                code_instances += 1;
                code_chars_removed += saved;
                msg.text = truncated;
            }
        }

        rule_stats.insert(
            "narration",
            (narration_chars_removed / 4, narration_instances),
        );
        rule_stats.insert("code", (code_chars_removed / 4, code_instances));
        rule_stats.insert(
            "boilerplate",
            (boilerplate_chars_removed / 4, boilerplate_instances),
        );

        // Build final text
        let compressed_text: String = rendered
            .iter()
            .map(|m| format!("[{}]\n{}", m.role.to_uppercase(), m.text))
            .collect::<Vec<_>>()
            .join("\n\n");

        let compressed_tokens = estimate_tokens(&compressed_text);

        let compression_ratio = if original_tokens > 0 {
            1.0 - (compressed_tokens as f32 / original_tokens as f32)
        } else {
            0.0
        };

        let rules_applied = vec![
            RuleApplication {
                rule: CompressionRule::ToolOutputStripping,
                tokens_removed: rule_stats.get("tool").map(|r| r.0).unwrap_or(0),
                instances: rule_stats.get("tool").map(|r| r.1).unwrap_or(0),
            },
            RuleApplication {
                rule: CompressionRule::ThinkingBlockRemoval,
                tokens_removed: rule_stats.get("thinking").map(|r| r.0).unwrap_or(0),
                instances: rule_stats.get("thinking").map(|r| r.1).unwrap_or(0),
            },
            RuleApplication {
                rule: CompressionRule::CycleCollapsing,
                tokens_removed: rule_stats.get("cycle").map(|r| r.0).unwrap_or(0),
                instances: rule_stats.get("cycle").map(|r| r.1).unwrap_or(0),
            },
            RuleApplication {
                rule: CompressionRule::NarrationStripping,
                tokens_removed: rule_stats.get("narration").map(|r| r.0).unwrap_or(0),
                instances: rule_stats.get("narration").map(|r| r.1).unwrap_or(0),
            },
            RuleApplication {
                rule: CompressionRule::CodeBlockTruncation,
                tokens_removed: rule_stats.get("code").map(|r| r.0).unwrap_or(0),
                instances: rule_stats.get("code").map(|r| r.1).unwrap_or(0),
            },
            RuleApplication {
                rule: CompressionRule::BoilerplateStripping,
                tokens_removed: rule_stats.get("boilerplate").map(|r| r.0).unwrap_or(0),
                instances: rule_stats.get("boilerplate").map(|r| r.1).unwrap_or(0),
            },
        ];

        CompressedTranscript {
            text: compressed_text,
            original_tokens,
            compressed_tokens,
            compression_ratio,
            rules_applied,
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Infer the tool name from the most recently rendered [tool: ...] tag in parts.
fn infer_tool_name_from_context(parts: &[String]) -> String {
    for part in parts.iter().rev() {
        if let Some(rest) = part.strip_prefix("[tool: ")
            && let Some(name) = rest.split(',').next()
        {
            return name.to_string();
        }
        if let Some(rest) = part.strip_prefix("[tool_use: ")
            && let Some(name) = rest.split(',').next()
        {
            return name.to_string();
        }
    }
    "Unknown".to_string()
}

fn render_messages_raw(messages: &[TranscriptMessage]) -> String {
    let mut out = String::new();
    for msg in messages {
        // Skip developer/system messages — Claude Code internal prompts, noise for extraction.
        if matches!(msg.role.as_str(), "developer" | "system") {
            continue;
        }
        out.push_str(&format!("[{}]\n", msg.role.to_uppercase()));
        for block in &msg.content {
            match block {
                ContentBlock::Text(t) => out.push_str(t),
                // Skip thinking blocks — internal reasoning, not useful for knowledge extraction.
                ContentBlock::Thinking(_) => {}
                ContentBlock::ToolUse { name, input } => {
                    out.push_str(&compress_tool_use_input(name, input));
                }
                ContentBlock::ToolResult { content, .. } => {
                    out.push_str(content);
                }
            }
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::ContentBlock;

    fn text_msg(role: &str, text: &str) -> TranscriptMessage {
        TranscriptMessage {
            role: role.to_string(),
            content: vec![ContentBlock::Text(text.to_string())],
            timestamp: None,
        }
    }

    fn thinking_msg(role: &str, think: &str, text: &str) -> TranscriptMessage {
        TranscriptMessage {
            role: role.to_string(),
            content: vec![
                ContentBlock::Thinking(think.to_string()),
                ContentBlock::Text(text.to_string()),
            ],
            timestamp: None,
        }
    }

    fn tool_result_msg(role: &str, tool_use_id: &str, content: &str) -> TranscriptMessage {
        TranscriptMessage {
            role: role.to_string(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content: content.to_string(),
            }],
            timestamp: None,
        }
    }

    fn tool_use_msg(role: &str, name: &str, input: serde_json::Value) -> TranscriptMessage {
        TranscriptMessage {
            role: role.to_string(),
            content: vec![ContentBlock::ToolUse {
                name: name.to_string(),
                input,
            }],
            timestamp: None,
        }
    }

    #[test]
    fn test_thinking_blocks_removed() {
        let compressor = TranscriptCompressor::default();
        let messages = vec![thinking_msg(
            "assistant",
            "This is long internal reasoning that should be stripped.",
            "Here is my answer.",
        )];
        let result = compressor.compress(&messages);
        assert!(!result.text.contains("internal reasoning"));
        assert!(result.text.contains("Here is my answer."));
        let thinking_rule = result
            .rules_applied
            .iter()
            .find(|r| r.rule == CompressionRule::ThinkingBlockRemoval)
            .unwrap();
        assert!(thinking_rule.instances > 0);
    }

    #[test]
    fn test_human_messages_never_stripped() {
        let compressor = TranscriptCompressor::default();
        let user_text =
            "Let me read the file and I'll check the configuration. Because this matters.";
        let messages = vec![text_msg("user", user_text)];
        let result = compressor.compress(&messages);
        assert!(result.text.contains(user_text));
    }

    #[test]
    fn test_decision_keyword_text_preserved() {
        let compressor = TranscriptCompressor::default();
        let decision_text =
            "Let me explain why we chose this approach. The root cause was a missing index.";
        let messages = vec![text_msg("assistant", decision_text)];
        let result = compressor.compress(&messages);
        // Lines with "chose" and "root cause" should be preserved
        assert!(result.text.contains("root cause"));
    }

    #[test]
    fn test_tool_output_stripping_long_content() {
        let compressor = TranscriptCompressor::default();
        let long_content: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let messages = vec![
            tool_use_msg(
                "assistant",
                "Read",
                serde_json::json!({"file_path": "src/main.rs"}),
            ),
            tool_result_msg("assistant", "id1", &long_content),
        ];
        let result = compressor.compress(&messages);
        // Should be compressed — not contain all 100 lines verbatim
        assert!(result.compressed_tokens < result.original_tokens);
    }

    #[test]
    fn test_edit_tool_inputs_preserved() {
        let compressor = TranscriptCompressor::default();
        let messages = vec![tool_use_msg(
            "assistant",
            "Edit",
            serde_json::json!({
                "file_path": "src/auth.rs",
                "old_string": "fn old_fn() {}",
                "new_string": "fn new_fn() { /* improved */ }"
            }),
        )];
        let result = compressor.compress(&messages);
        assert!(result.text.contains("src/auth.rs"));
        assert!(result.text.contains("fn old_fn()"));
        assert!(result.text.contains("fn new_fn()"));
    }

    #[test]
    fn test_narration_stripping() {
        let compressor = TranscriptCompressor::default();
        let messages = vec![text_msg(
            "assistant",
            "Let me read the file to understand the structure.\nI'll now check the configuration.\nThe answer is 42.",
        )];
        let result = compressor.compress(&messages);
        // Narration lines should be removed, the actual content preserved
        assert!(result.text.contains("The answer is 42."));
        // These narration lines should not appear
        assert!(!result.text.contains("Let me read the file"));
    }

    #[test]
    fn test_compression_ratio_realistic_session() {
        let compressor = TranscriptCompressor::default();

        // Build a realistic synthetic session
        let long_file: String = (0..200)
            .map(|i| format!("line {i}: some code here\n"))
            .collect();
        let test_output = "running 20 tests\ntest foo ... ok\ntest bar ... FAILED\n\
            failures:\n  bar: assertion failed: left == right\n\
            test result: FAILED. 19 passed; 1 failed";
        let thinking = "a".repeat(2000);
        let narration = "Let me read the file to understand it.\n".repeat(20);
        let build_output: String = (0..50)
            .map(|i| format!("   Compiling dep_{i} v1.0\n"))
            .collect();

        let messages = vec![
            text_msg("user", "Please fix the failing test."),
            thinking_msg("assistant", &thinking, "Let me investigate."),
            tool_use_msg(
                "assistant",
                "Read",
                serde_json::json!({"file_path": "src/lib.rs"}),
            ),
            tool_result_msg("assistant", "id1", &long_file),
            text_msg("assistant", &narration),
            tool_result_msg("assistant", "id2", test_output),
            tool_result_msg("assistant", "id3", &build_output),
            tool_use_msg(
                "assistant",
                "Edit",
                serde_json::json!({
                    "file_path": "src/lib.rs",
                    "old_string": "fn bar() { assert_eq!(1, 2); }",
                    "new_string": "fn bar() { assert_eq!(1, 1); }"
                }),
            ),
            text_msg("assistant", "Fixed. The root cause was a bad assertion."),
        ];

        let result = compressor.compress(&messages);
        // Should achieve >50% compression
        assert!(
            result.compression_ratio > 0.5,
            "Expected >50% compression, got {:.1}%",
            result.compression_ratio * 100.0
        );
        // User message intact
        assert!(result.text.contains("Please fix the failing test."));
        // Edit preserved
        assert!(result.text.contains("src/lib.rs"));
        // Decision text preserved
        assert!(result.text.contains("root cause"));
    }

    #[test]
    fn test_code_block_truncation() {
        let compressor = TranscriptCompressor::default();
        let code: String = (0..30).map(|i| format!("    let x{i} = {i};\n")).collect();
        let text = format!("Here is some code:\n```rust\n{code}```\nEnd of message.");
        let messages = vec![text_msg("assistant", &text)];
        let result = compressor.compress(&messages);
        assert!(result.text.contains("lines omitted"));
        assert!(result.text.contains("End of message."));
    }

    #[test]
    fn test_grep_results_truncated() {
        let compressor = TranscriptCompressor::default();
        let grep_output: String = (0..50)
            .map(|i| format!("src/file{i}.rs:10: match\n"))
            .collect();
        let messages = vec![
            tool_use_msg("assistant", "Grep", serde_json::json!({"pattern": "foo"})),
            tool_result_msg("assistant", "id1", &grep_output),
        ];
        let result = compressor.compress(&messages);
        assert!(result.text.contains("search results"));
        assert!(result.compressed_tokens < result.original_tokens);
    }
}
