//! LLM client backends for the extraction pipeline.
//!
//! Three backends:
//! - `ApiClient`: Direct Anthropic Messages API (requires ANTHROPIC_API_KEY)
//! - `OpenAiClient`: OpenAI-compatible Chat Completions API (requires OPENAI_API_KEY)
//! - `CliClient`: Shells out to `claude -p` CLI (uses existing auth)
//!
//! Unified via `LlmClient` enum dispatch.

use fieldcraft_core::TrError;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Unified client (enum dispatch)
// ---------------------------------------------------------------------------

/// LLM client used by the extraction pipeline.
pub enum LlmClient {
    /// Direct Anthropic Messages API.
    Api(ApiClient),
    /// OpenAI-compatible Chat Completions API.
    OpenAi(OpenAiClient),
    /// Claude Code CLI (`claude -p`).
    Cli(CliClient),
}

impl LlmClient {
    /// Create an API-backed client.
    pub fn api(api_key: impl Into<String>) -> Self {
        Self::Api(ApiClient::new(api_key))
    }

    /// Create an OpenAI-compatible API client.
    pub fn openai(api_key: impl Into<String>, base_url: Option<String>) -> Self {
        Self::OpenAi(OpenAiClient::new(api_key, base_url))
    }

    /// Create a Claude Code CLI-backed client.
    pub fn cli() -> Self {
        Self::Cli(CliClient)
    }

    /// Returns true if using the CLI backend (slow per-call, benefits from batching).
    pub fn is_cli(&self) -> bool {
        matches!(self, Self::Cli(_))
    }

    /// Send a single-turn message and return the text response.
    pub async fn message(
        &self,
        model: &str,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> Result<String, TrError> {
        match self {
            Self::Api(c) => c.message(model, system, user, max_tokens).await,
            Self::OpenAi(c) => c.message(model, system, user, max_tokens).await,
            Self::Cli(c) => c.message(model, system, user).await,
        }
    }
}

// ---------------------------------------------------------------------------
// Backend 1: Direct Anthropic API
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    messages: Vec<RequestMessage<'a>>,
}

#[derive(Serialize)]
struct RequestMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize, Debug)]
struct MessagesResponse {
    content: Vec<ResponseContentBlock>,
    #[allow(dead_code)]
    usage: Option<UsageBlock>,
}

#[derive(Deserialize, Debug)]
struct ResponseContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct UsageBlock {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

#[derive(Deserialize, Debug)]
struct ApiErrorEnvelope {
    error: ApiErrorDetail,
}

#[derive(Deserialize, Debug)]
struct ApiErrorDetail {
    #[serde(rename = "type")]
    error_type: String,
    message: String,
}

pub struct ApiClient {
    api_key: String,
    http: reqwest::Client,
}

impl ApiClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            http: reqwest::Client::new(),
        }
    }

    async fn message(
        &self,
        model: &str,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> Result<String, TrError> {
        let request = MessagesRequest {
            model,
            max_tokens,
            system: if system.is_empty() {
                None
            } else {
                Some(system)
            },
            messages: vec![RequestMessage {
                role: "user",
                content: user,
            }],
        };

        let body = serde_json::to_string(&request).map_err(|e| TrError::Internal {
            message: format!("failed to serialize request: {e}"),
        })?;

        let mut attempt = 0u32;
        const MAX_RETRIES: u32 = 3;

        loop {
            let response = self
                .http
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .body(body.clone())
                .send()
                .await
                .map_err(|e| TrError::Internal {
                    message: format!("HTTP request failed: {e}"),
                })?;

            let status = response.status();

            if status == 429 && attempt < MAX_RETRIES {
                let delay_secs = 1u64 << attempt;
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
                continue;
            }

            let response_text = response.text().await.map_err(|e| TrError::Internal {
                message: format!("failed to read response body: {e}"),
            })?;

            if !status.is_success() {
                let detail = serde_json::from_str::<ApiErrorEnvelope>(&response_text)
                    .map(|e| format!("{}: {}", e.error.error_type, e.error.message))
                    .unwrap_or_else(|_| response_text.clone());

                return Err(TrError::Internal {
                    message: format!("Anthropic API error {status}: {detail}"),
                });
            }

            let parsed: MessagesResponse =
                serde_json::from_str(&response_text).map_err(|e| TrError::Parse {
                    message: format!(
                        "failed to parse Anthropic response: {e}\nBody: {response_text}"
                    ),
                })?;

            let text = parsed
                .content
                .into_iter()
                .filter(|b| b.block_type == "text")
                .filter_map(|b| b.text)
                .collect::<Vec<_>>()
                .join("");

            if text.is_empty() {
                return Err(TrError::Internal {
                    message: "Anthropic response contained no text content".to_string(),
                });
            }

            return Ok(text);
        }
    }
}

// ---------------------------------------------------------------------------
// Backend 2: OpenAI-compatible Chat Completions API
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    max_tokens: u32,
    /// Hardcoded to 0 — Stage B is a classification/extraction task, not a
    /// generative one. OpenAI defaults to 1.0 which biases toward common
    /// output patterns (e.g. directive-heavy classification).
    temperature: f32,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

#[derive(Deserialize)]
struct ChatChoiceMessage {
    content: Option<String>,
}

pub struct OpenAiClient {
    api_key: String,
    base_url: String,
    http: reqwest::Client,
}

impl OpenAiClient {
    pub fn new(api_key: impl Into<String>, base_url: Option<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            http: reqwest::Client::new(),
        }
    }

    async fn message(
        &self,
        model: &str,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> Result<String, TrError> {
        let mut messages = Vec::new();
        if !system.is_empty() {
            messages.push(ChatMessage {
                role: "system",
                content: system,
            });
        }
        messages.push(ChatMessage {
            role: "user",
            content: user,
        });

        let request = ChatRequest {
            model,
            messages,
            max_tokens,
            temperature: 0.0,
        };

        let body = serde_json::to_string(&request).map_err(|e| TrError::Internal {
            message: format!("failed to serialize request: {e}"),
        })?;

        let url = format!("{}/chat/completions", self.base_url);
        let mut attempt = 0u32;
        const MAX_RETRIES: u32 = 3;

        loop {
            let response = self
                .http
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("content-type", "application/json")
                .body(body.clone())
                .send()
                .await
                .map_err(|e| TrError::Internal {
                    message: format!("HTTP request failed: {e}"),
                })?;

            let status = response.status();

            if status == 429 && attempt < MAX_RETRIES {
                let delay_secs = 1u64 << attempt;
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
                continue;
            }

            let response_text = response.text().await.map_err(|e| TrError::Internal {
                message: format!("failed to read response body: {e}"),
            })?;

            if !status.is_success() {
                return Err(TrError::Internal {
                    message: format!("OpenAI API error {status}: {response_text}"),
                });
            }

            let parsed: ChatResponse =
                serde_json::from_str(&response_text).map_err(|e| TrError::Parse {
                    message: format!("failed to parse OpenAI response: {e}\nBody: {response_text}"),
                })?;

            let text = parsed
                .choices
                .into_iter()
                .filter_map(|c| c.message.content)
                .collect::<Vec<_>>()
                .join("");

            if text.is_empty() {
                return Err(TrError::Internal {
                    message: "OpenAI response contained no text content".to_string(),
                });
            }

            return Ok(text);
        }
    }
}

// ---------------------------------------------------------------------------
// Backend 3: Claude Code CLI
// ---------------------------------------------------------------------------

/// LLM client that shells out to `claude -p` using existing Claude Code auth.
pub struct CliClient;

impl CliClient {
    /// Map extraction model names to claude CLI model aliases.
    fn cli_model(model: &str) -> &str {
        if model.contains("haiku") {
            "haiku"
        } else if model.contains("sonnet") {
            "sonnet"
        } else if model.contains("opus") {
            "opus"
        } else {
            "sonnet"
        }
    }

    async fn message(&self, model: &str, system: &str, user: &str) -> Result<String, TrError> {
        let cli_model = Self::cli_model(model);

        let mut cmd = tokio::process::Command::new("claude");
        cmd.arg("-p")
            .arg("--model")
            .arg(cli_model)
            .arg("--no-session-persistence")
            .current_dir("/tmp");

        if !system.is_empty() {
            cmd.arg("--system-prompt").arg(system);
        }

        cmd.arg(user);

        let output = cmd.output().await.map_err(|e| TrError::Internal {
            message: format!("failed to run claude CLI: {e}"),
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(TrError::Internal {
                message: format!("claude CLI exited with {}: {stderr}", output.status),
            });
        }

        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();

        if text.is_empty() {
            return Err(TrError::Internal {
                message: "claude CLI returned empty output".to_string(),
            });
        }

        Ok(text)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_serializes_correctly() {
        let req = MessagesRequest {
            model: "claude-haiku-4-5-20251001",
            max_tokens: 256,
            system: Some("You are a classifier."),
            messages: vec![RequestMessage {
                role: "user",
                content: "Classify this chunk.",
            }],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["model"], "claude-haiku-4-5-20251001");
        assert_eq!(json["max_tokens"], 256);
        assert_eq!(json["system"], "You are a classifier.");
        assert_eq!(json["messages"][0]["role"], "user");
        assert_eq!(json["messages"][0]["content"], "Classify this chunk.");
    }

    #[test]
    fn test_empty_system_omitted() {
        let req = MessagesRequest {
            model: "claude-haiku-4-5-20251001",
            max_tokens: 256,
            system: None,
            messages: vec![RequestMessage {
                role: "user",
                content: "Hi",
            }],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("system").is_none());
    }

    #[test]
    fn test_response_parsing() {
        let raw = r#"{
            "id": "msg_123",
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "text", "text": "This is the answer."}
            ],
            "model": "claude-haiku-4-5-20251001",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        }"#;
        let parsed: MessagesResponse = serde_json::from_str(raw).unwrap();
        let text: String = parsed
            .content
            .into_iter()
            .filter(|b| b.block_type == "text")
            .filter_map(|b| b.text)
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(text, "This is the answer.");
    }

    #[test]
    fn test_error_response_parsing() {
        let raw = r#"{"error": {"type": "rate_limit_error", "message": "Rate limit exceeded"}}"#;
        let parsed: ApiErrorEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.error.error_type, "rate_limit_error");
        assert_eq!(parsed.error.message, "Rate limit exceeded");
    }

    #[test]
    fn test_cli_model_mapping() {
        assert_eq!(CliClient::cli_model("claude-haiku-4-5-20251001"), "haiku");
        assert_eq!(CliClient::cli_model("claude-sonnet-4-5-20241022"), "sonnet");
        assert_eq!(CliClient::cli_model("claude-opus-4-6"), "opus");
        assert_eq!(CliClient::cli_model("unknown-model"), "sonnet");
    }
}
