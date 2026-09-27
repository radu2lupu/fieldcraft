use fieldcraft_core::AgentTool;
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;

/// Detect the AI tool provider from a session file path.
pub fn detect_provider(path: &Path) -> AgentTool {
    let path_str = path.to_string_lossy();
    if path_str.contains("/.claude/") || path_str.contains("/claude/") {
        AgentTool::ClaudeCode
    } else if path_str.contains("/.codex/") || path_str.contains("/codex/") {
        AgentTool::Codex
    } else if path_str.contains("/.cursor/") || path_str.contains("/cursor/") {
        AgentTool::Cursor
    } else {
        AgentTool::Unknown
    }
}

#[derive(Debug, Clone)]
pub struct TranscriptMessage {
    pub role: String,
    pub content: Vec<ContentBlock>,
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ContentBlock {
    Text(String),
    Thinking(String),
    ToolUse {
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

// Raw serde structures for JSONL parsing

#[derive(Deserialize)]
struct RawContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: Option<String>,
    thinking: Option<String>,
    name: Option<String>,
    input: Option<Value>,
    tool_use_id: Option<String>,
    content: Option<Value>,
}

fn parse_content_value(content: &Value) -> Vec<ContentBlock> {
    match content {
        Value::String(s) => vec![ContentBlock::Text(s.clone())],
        Value::Array(arr) => arr
            .iter()
            .filter_map(|block| {
                let raw: RawContentBlock = serde_json::from_value(block.clone()).ok()?;
                parse_content_block(raw)
            })
            .collect(),
        _ => vec![],
    }
}

fn parse_content_block(raw: RawContentBlock) -> Option<ContentBlock> {
    match raw.block_type.as_str() {
        "text" => Some(ContentBlock::Text(raw.text.unwrap_or_default())),
        "thinking" => Some(ContentBlock::Thinking(raw.thinking.unwrap_or_default())),
        "tool_use" => Some(ContentBlock::ToolUse {
            name: raw.name.unwrap_or_default(),
            input: raw.input.unwrap_or(Value::Null),
        }),
        "tool_result" => {
            let content_str = match raw.content {
                Some(Value::String(s)) => s,
                Some(Value::Array(arr)) => arr
                    .iter()
                    .filter_map(|b| {
                        let rb: RawContentBlock = serde_json::from_value(b.clone()).ok()?;
                        rb.text
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                Some(other) => other.to_string(),
                None => raw.text.unwrap_or_default(),
            };
            Some(ContentBlock::ToolResult {
                tool_use_id: raw.tool_use_id.unwrap_or_default(),
                content: content_str,
            })
        }
        _ => None,
    }
}

/// Parse a Claude Code JSONL session file into messages.
/// Malformed lines are skipped with a warning printed to stderr.
pub fn parse_jsonl(input: &str) -> Vec<TranscriptMessage> {
    let mut messages = Vec::new();

    for (line_num, line) in input.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let parsed: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "[fieldcraft-extract] skipping malformed JSONL line {}: {}",
                    line_num + 1,
                    e
                );
                continue;
            }
        };

        // Try to unwrap nested "message" key (Claude Code wraps assistant turns)
        let msg_obj = if let Some(inner) = parsed.get("message") {
            inner
        } else {
            &parsed
        };

        let role = match msg_obj.get("role").and_then(|v| v.as_str()) {
            Some(r) => r.to_string(),
            None => continue,
        };

        let content = match msg_obj.get("content") {
            Some(c) => parse_content_value(c),
            None => continue,
        };

        let timestamp = parsed
            .get("timestamp")
            .or_else(|| msg_obj.get("timestamp"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        messages.push(TranscriptMessage {
            role,
            content,
            timestamp,
        });
    }

    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_user_message() {
        let jsonl = r#"{"role":"user","content":"Hello world"}"#;
        let msgs = parse_jsonl(jsonl);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
        matches!(&msgs[0].content[0], ContentBlock::Text(t) if t == "Hello world");
    }

    #[test]
    fn test_parse_array_content() {
        let jsonl = r#"{"role":"assistant","content":[{"type":"text","text":"Hi"},{"type":"thinking","thinking":"internal"}]}"#;
        let msgs = parse_jsonl(jsonl);
        assert_eq!(msgs[0].content.len(), 2);
    }

    #[test]
    fn test_skip_malformed_lines() {
        let jsonl = "not json\n{\"role\":\"user\",\"content\":\"ok\"}";
        let msgs = parse_jsonl(jsonl);
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn test_nested_message_key() {
        let jsonl = r#"{"type":"message","message":{"role":"assistant","content":"reply"}}"#;
        let msgs = parse_jsonl(jsonl);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "assistant");
    }
}
