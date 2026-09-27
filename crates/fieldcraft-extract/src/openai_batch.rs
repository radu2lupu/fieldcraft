//! OpenAI Batch API client and mockable trait.
//!
//! `OpenAiBatchApi` is the trait that both the real `OpenAiBatchClient` and the
//! test `MockOpenAiBatchClient` implement.  Callers depend on `dyn OpenAiBatchApi`
//! (or a generic `T: OpenAiBatchApi`) so the real HTTP round-trips can be swapped
//! out in tests without hitting the network.

use async_trait::async_trait;
use fieldcraft_core::TrError;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/// Status of an OpenAI Batch job, mirroring the API's `status` field exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchStatus {
    Validating,
    InProgress,
    Finalizing,
    Completed,
    Expired,
    Cancelling,
    Cancelled,
    Failed,
}

/// Per-request counters returned by the Batch API.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BatchRequestCounts {
    pub total: u32,
    pub completed: u32,
    pub failed: u32,
}

/// Envelope returned by `POST /batches` and `GET /batches/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchResponse {
    pub id: String,
    pub status: BatchStatus,
    pub input_file_id: Option<String>,
    pub output_file_id: Option<String>,
    pub error_file_id: Option<String>,
    pub request_counts: BatchRequestCounts,
    /// Unix seconds when the batch was created.
    pub created_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub failed_at: Option<i64>,
    pub expired_at: Option<i64>,
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Abstraction over the OpenAI Batch API for testability.
///
/// The real implementation (`OpenAiBatchClient`) hits the live API.
/// The mock (`MockOpenAiBatchClient`, `#[cfg(test)]`) records all interactions
/// in memory so tests can run without any network access.
#[async_trait]
pub trait OpenAiBatchApi: Send + Sync {
    /// Upload a JSONL file (`purpose=batch`) and return the resulting `file_id`.
    async fn upload_batch_file(&self, jsonl_bytes: Vec<u8>) -> Result<String, TrError>;

    /// Submit a new batch job and return the initial `BatchResponse`.
    async fn create_batch(
        &self,
        input_file_id: &str,
        endpoint: &str,
        window: &str,
    ) -> Result<BatchResponse, TrError>;

    /// Poll the status of a batch job.
    async fn get_batch(&self, batch_id: &str) -> Result<BatchResponse, TrError>;

    /// Download the raw bytes of a file (output or error JSONL).
    async fn download_file(&self, file_id: &str) -> Result<Vec<u8>, TrError>;
}

// ---------------------------------------------------------------------------
// Real implementation
// ---------------------------------------------------------------------------

/// Minimal response shape for `POST /files`.
#[derive(Deserialize)]
struct FileUploadResponse {
    id: String,
}

/// Body sent to `POST /batches`.
#[derive(Serialize)]
struct CreateBatchBody<'a> {
    input_file_id: &'a str,
    endpoint: &'a str,
    completion_window: &'a str,
}

/// Real HTTP client that calls the OpenAI Batch API.
pub struct OpenAiBatchClient {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
}

impl OpenAiBatchClient {
    /// Create a client using the default `https://api.openai.com/v1` base URL.
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            base_url: "https://api.openai.com/v1".to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// Create a client with a custom base URL (useful for proxies / testing).
    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self {
            api_key,
            base_url,
            client: reqwest::Client::new(),
        }
    }

    /// Map a non-2xx HTTP response to `TrError::Storage`.
    async fn map_error(resp: reqwest::Response) -> TrError {
        let status = resp.status();
        let body = resp
            .text()
            .await
            .unwrap_or_else(|_| "<unreadable body>".to_string());
        let truncated = &body[..body.len().min(500)];
        TrError::Storage {
            message: format!("OpenAI Batch API returned {status}: {truncated}"),
        }
    }
}

#[async_trait]
impl OpenAiBatchApi for OpenAiBatchClient {
    async fn upload_batch_file(&self, jsonl_bytes: Vec<u8>) -> Result<String, TrError> {
        // SAFETY: never log self.api_key value — only first 8 chars for debug.
        tracing::debug!(
            key_prefix = %&self.api_key[..self.api_key.len().min(8)],
            bytes = jsonl_bytes.len(),
            "uploading batch file"
        );

        let part = reqwest::multipart::Part::bytes(jsonl_bytes)
            .file_name("batch.jsonl")
            .mime_str("application/jsonl")
            .map_err(|e| TrError::Internal {
                message: format!("invalid mime type: {e}"),
            })?;

        let form = reqwest::multipart::Form::new()
            .text("purpose", "batch")
            .part("file", part);

        let url = format!("{}/files", self.base_url);
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .multipart(form)
            .send()
            .await
            .map_err(|e| TrError::Storage {
                message: format!("HTTP request failed: {e}"),
            })?;

        if !resp.status().is_success() {
            return Err(Self::map_error(resp).await);
        }

        let parsed: FileUploadResponse = resp.json().await.map_err(|e| TrError::Parse {
            message: format!("failed to parse file upload response: {e}"),
        })?;

        Ok(parsed.id)
    }

    async fn create_batch(
        &self,
        input_file_id: &str,
        endpoint: &str,
        window: &str,
    ) -> Result<BatchResponse, TrError> {
        tracing::debug!(
            key_prefix = %&self.api_key[..self.api_key.len().min(8)],
            input_file_id,
            "creating batch"
        );

        let body = CreateBatchBody {
            input_file_id,
            endpoint,
            completion_window: window,
        };

        let url = format!("{}/batches", self.base_url);
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| TrError::Storage {
                message: format!("HTTP request failed: {e}"),
            })?;

        if !resp.status().is_success() {
            return Err(Self::map_error(resp).await);
        }

        resp.json::<BatchResponse>()
            .await
            .map_err(|e| TrError::Parse {
                message: format!("failed to parse create_batch response: {e}"),
            })
    }

    async fn get_batch(&self, batch_id: &str) -> Result<BatchResponse, TrError> {
        tracing::debug!(
            key_prefix = %&self.api_key[..self.api_key.len().min(8)],
            batch_id,
            "polling batch"
        );

        let url = format!("{}/batches/{}", self.base_url, batch_id);
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await
            .map_err(|e| TrError::Storage {
                message: format!("HTTP request failed: {e}"),
            })?;

        if !resp.status().is_success() {
            return Err(Self::map_error(resp).await);
        }

        resp.json::<BatchResponse>()
            .await
            .map_err(|e| TrError::Parse {
                message: format!("failed to parse get_batch response: {e}"),
            })
    }

    async fn download_file(&self, file_id: &str) -> Result<Vec<u8>, TrError> {
        tracing::debug!(
            key_prefix = %&self.api_key[..self.api_key.len().min(8)],
            file_id,
            "downloading file"
        );

        let url = format!("{}/files/{}/content", self.base_url, file_id);
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await
            .map_err(|e| TrError::Storage {
                message: format!("HTTP request failed: {e}"),
            })?;

        if !resp.status().is_success() {
            return Err(Self::map_error(resp).await);
        }

        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| TrError::Storage {
                message: format!("failed to read file content: {e}"),
            })
    }
}

// ---------------------------------------------------------------------------
// Mock implementation (test only)
// ---------------------------------------------------------------------------

#[cfg(any(test, feature = "test-utils"))]
pub use mock::MockOpenAiBatchClient;

#[cfg(any(test, feature = "test-utils"))]
mod mock {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    struct MockState {
        /// file_id -> raw bytes
        files: HashMap<String, Vec<u8>>,
        /// batch_id -> current BatchResponse
        batches: HashMap<String, BatchResponse>,
        next_file_id: u64,
        next_batch_id: u64,
        /// Status assigned to freshly created batches.
        create_batch_initial_status: BatchStatus,
    }

    impl Default for MockState {
        fn default() -> Self {
            Self {
                files: HashMap::new(),
                batches: HashMap::new(),
                next_file_id: 1,
                next_batch_id: 1,
                create_batch_initial_status: BatchStatus::InProgress,
            }
        }
    }

    /// In-memory mock of the OpenAI Batch API.  All operations are synchronous
    /// under the lock; no network calls are made.
    pub struct MockOpenAiBatchClient {
        state: Arc<Mutex<MockState>>,
    }

    impl Default for MockOpenAiBatchClient {
        fn default() -> Self {
            Self::new()
        }
    }

    impl MockOpenAiBatchClient {
        pub fn new() -> Self {
            Self {
                state: Arc::new(Mutex::new(MockState::default())),
            }
        }

        /// Override the status that `create_batch` gives to new batches.
        pub fn set_initial_batch_status(&self, status: BatchStatus) {
            self.state.lock().unwrap().create_batch_initial_status = status;
        }

        /// Transition a batch to `Completed`, storing the output (and optional error) JSONL.
        pub fn complete_batch(
            &self,
            batch_id: &str,
            output_jsonl: Vec<u8>,
            error_jsonl: Option<Vec<u8>>,
        ) {
            let mut st = self.state.lock().unwrap();

            // Store the output file.
            let output_file_id = format!("file-out-{}", st.next_file_id);
            st.next_file_id += 1;
            st.files.insert(output_file_id.clone(), output_jsonl);

            // Optionally store an error file.
            let error_file_id = error_jsonl.map(|bytes| {
                let id = format!("file-err-{}", st.next_file_id);
                st.next_file_id += 1;
                st.files.insert(id.clone(), bytes);
                id
            });

            if let Some(batch) = st.batches.get_mut(batch_id) {
                batch.status = BatchStatus::Completed;
                batch.output_file_id = Some(output_file_id);
                batch.error_file_id = error_file_id;
                batch.completed_at = Some(0);
            }
        }

        /// Transition a batch to `Failed`.
        pub fn fail_batch(&self, batch_id: &str, _reason: &str) {
            let mut st = self.state.lock().unwrap();
            if let Some(batch) = st.batches.get_mut(batch_id) {
                batch.status = BatchStatus::Failed;
                batch.failed_at = Some(0);
            }
        }
    }

    #[async_trait]
    impl OpenAiBatchApi for MockOpenAiBatchClient {
        async fn upload_batch_file(&self, jsonl_bytes: Vec<u8>) -> Result<String, TrError> {
            let mut st = self.state.lock().unwrap();
            let file_id = format!("file-{}", st.next_file_id);
            st.next_file_id += 1;
            st.files.insert(file_id.clone(), jsonl_bytes);
            Ok(file_id)
        }

        async fn create_batch(
            &self,
            input_file_id: &str,
            _endpoint: &str,
            _window: &str,
        ) -> Result<BatchResponse, TrError> {
            let mut st = self.state.lock().unwrap();
            let batch_id = format!("batch_{}", st.next_batch_id);
            st.next_batch_id += 1;
            let status = st.create_batch_initial_status;
            let response = BatchResponse {
                id: batch_id.clone(),
                status,
                input_file_id: Some(input_file_id.to_string()),
                output_file_id: None,
                error_file_id: None,
                request_counts: BatchRequestCounts::default(),
                created_at: Some(0),
                completed_at: None,
                failed_at: None,
                expired_at: None,
            };
            st.batches.insert(batch_id, response.clone());
            Ok(response)
        }

        async fn get_batch(&self, batch_id: &str) -> Result<BatchResponse, TrError> {
            let st = self.state.lock().unwrap();
            st.batches
                .get(batch_id)
                .cloned()
                .ok_or_else(|| TrError::NotFound {
                    message: format!("batch {batch_id} not found"),
                })
        }

        async fn download_file(&self, file_id: &str) -> Result<Vec<u8>, TrError> {
            let st = self.state.lock().unwrap();
            st.files
                .get(file_id)
                .cloned()
                .ok_or_else(|| TrError::NotFound {
                    message: format!("file {file_id} not found"),
                })
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mock::MockOpenAiBatchClient;

    // 1. upload returns a file-id; download_file returns the same bytes.
    #[tokio::test]
    async fn mock_upload_returns_file_id() {
        let client = MockOpenAiBatchClient::new();
        let payload = b"line1\nline2\n".to_vec();
        let file_id = client
            .upload_batch_file(payload.clone())
            .await
            .expect("upload should succeed");
        assert!(!file_id.is_empty(), "file_id must not be empty");

        let downloaded = client
            .download_file(&file_id)
            .await
            .expect("download should succeed");
        assert_eq!(downloaded, payload);
    }

    // 2. Full lifecycle: upload → create → InProgress → complete → Completed → download.
    #[tokio::test]
    async fn mock_batch_lifecycle() {
        let client = MockOpenAiBatchClient::new();

        // Upload input file.
        let input_bytes = b"{\"custom_id\":\"req-1\"}\n".to_vec();
        let file_id = client.upload_batch_file(input_bytes).await.expect("upload");

        // Create batch — starts as InProgress (default mock status).
        let batch = client
            .create_batch(&file_id, "/v1/chat/completions", "24h")
            .await
            .expect("create_batch");
        let batch_id = batch.id.clone();
        assert_eq!(batch.status, BatchStatus::InProgress);

        // Poll — still InProgress.
        let polled = client.get_batch(&batch_id).await.expect("get_batch");
        assert_eq!(polled.status, BatchStatus::InProgress);
        assert!(polled.output_file_id.is_none());

        // Simulate completion by the mock helper.
        let output_jsonl = b"{\"id\":\"batch_req_1\"}\n".to_vec();
        client.complete_batch(&batch_id, output_jsonl.clone(), None);

        // Poll again — should be Completed with an output_file_id.
        let completed = client
            .get_batch(&batch_id)
            .await
            .expect("get_batch after complete");
        assert_eq!(completed.status, BatchStatus::Completed);
        let out_file_id = completed
            .output_file_id
            .expect("output_file_id must be set after completion");

        // Download output.
        let downloaded = client
            .download_file(&out_file_id)
            .await
            .expect("download output");
        assert_eq!(downloaded, output_jsonl);
    }

    // 3. upload → create → fail → Failed status.
    #[tokio::test]
    async fn mock_batch_failure() {
        let client = MockOpenAiBatchClient::new();

        let file_id = client
            .upload_batch_file(b"input\n".to_vec())
            .await
            .expect("upload");

        let batch = client
            .create_batch(&file_id, "/v1/chat/completions", "24h")
            .await
            .expect("create_batch");
        let batch_id = batch.id.clone();

        client.fail_batch(&batch_id, "internal server error");

        let failed = client.get_batch(&batch_id).await.expect("get_batch");
        assert_eq!(failed.status, BatchStatus::Failed);
    }

    // 4. BatchResponse serializes and deserializes identically.
    #[test]
    fn batch_response_serde_roundtrip() {
        let original = BatchResponse {
            id: "batch_abc123".to_string(),
            status: BatchStatus::Completed,
            input_file_id: Some("file-input".to_string()),
            output_file_id: Some("file-output".to_string()),
            error_file_id: None,
            request_counts: BatchRequestCounts {
                total: 10,
                completed: 9,
                failed: 1,
            },
            created_at: Some(1_700_000_000),
            completed_at: Some(1_700_003_600),
            failed_at: None,
            expired_at: None,
        };

        let json = serde_json::to_string(&original).expect("serialize");
        let roundtripped: BatchResponse = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(roundtripped.id, original.id);
        assert_eq!(roundtripped.status, original.status);
        assert_eq!(roundtripped.input_file_id, original.input_file_id);
        assert_eq!(roundtripped.output_file_id, original.output_file_id);
        assert_eq!(roundtripped.error_file_id, original.error_file_id);
        assert_eq!(
            roundtripped.request_counts.total,
            original.request_counts.total
        );
        assert_eq!(
            roundtripped.request_counts.completed,
            original.request_counts.completed
        );
        assert_eq!(
            roundtripped.request_counts.failed,
            original.request_counts.failed
        );
        assert_eq!(roundtripped.created_at, original.created_at);
        assert_eq!(roundtripped.completed_at, original.completed_at);
        assert_eq!(roundtripped.failed_at, original.failed_at);
        assert_eq!(roundtripped.expired_at, original.expired_at);
    }

    // 5. Each BatchStatus variant serializes to the correct snake_case string.
    #[test]
    fn batch_status_serde_snake_case() {
        let cases = [
            (BatchStatus::Validating, "\"validating\""),
            (BatchStatus::InProgress, "\"in_progress\""),
            (BatchStatus::Finalizing, "\"finalizing\""),
            (BatchStatus::Completed, "\"completed\""),
            (BatchStatus::Expired, "\"expired\""),
            (BatchStatus::Cancelling, "\"cancelling\""),
            (BatchStatus::Cancelled, "\"cancelled\""),
            (BatchStatus::Failed, "\"failed\""),
        ];

        for (status, expected) in cases {
            let serialized = serde_json::to_string(&status).expect("serialize");
            assert_eq!(
                serialized, expected,
                "BatchStatus::{status:?} should serialize as {expected}"
            );

            // Also verify round-trip.
            let deserialized: BatchStatus = serde_json::from_str(&serialized).expect("deserialize");
            assert_eq!(deserialized, status);
        }
    }
}
