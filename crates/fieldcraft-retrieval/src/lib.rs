pub mod cue_match;
mod gemma;
pub mod record_cache;
pub mod retrieval;
pub mod sqlite_search;
pub use cue_match::{MISMATCH_FLOOR, cue_match_factor};
pub use fieldcraft_core::embedding::{EmbeddingProvider, EmbeddingSpec};
pub use record_cache::archive_matches;
pub use retrieval::{
    LessonPreference, QueryVector, RetrievalRequest, RetrievalResponse, RetrievedLesson,
    TokenReceipt, build_embedding_text, build_query, index_knowledge_sqlite, retrieve,
};

use fieldcraft_core::TrError;
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::env;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime};

const DEFAULT_GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
const DEFAULT_MAX_CHARS: usize = 8000;
const MAX_GEMINI_BATCH: usize = 100;
const MAX_ATTEMPTS: usize = 6;
const MAX_RETRY_DELAY_SECS: f64 = 65.0;

/// What a text is for. Gemma prefixes queries and documents with different
/// task prompts (model card); Gemini embeds both the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextRole {
    Query,
    Document,
}

/// Encode `texts` with `spec`'s provider. Vectors are L2-normalized and have
/// `spec.dims` dimensions. BM25-only mode (`provider = none`) is an error:
/// callers decide what "no vector" means for them.
pub fn encode_with_spec(
    spec: &EmbeddingSpec,
    role: TextRole,
    texts: &[String],
) -> Result<Vec<Vec<f32>>, String> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let result = match spec.provider {
        EmbeddingProvider::None => {
            return Err(
                "BM25-only mode: no embedding provider configured (FIELDCRAFT_EMBED_PROVIDER=none)"
                    .to_string(),
            );
        }
        EmbeddingProvider::Gemini => encode_texts_gemini(spec, texts, MAX_GEMINI_BATCH),
        EmbeddingProvider::Gemma => gemma::encode(spec, role, texts),
    };
    record_health(&result);
    result
}

/// Documents with the configured provider (see `fieldcraft_core::embedding`).
pub fn encode_dense(texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    let spec = EmbeddingSpec::from_env()?;
    encode_with_spec(&spec, TextRole::Document, texts)
}

/// Outcome of the most recent embedding calls in this process, for `/status`.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct EmbeddingHealth {
    pub calls: u64,
    pub failures: u64,
    pub last_ok_unix: Option<u64>,
    pub last_error_unix: Option<u64>,
    pub last_error: Option<String>,
}

static HEALTH: Mutex<EmbeddingHealth> = Mutex::new(EmbeddingHealth {
    calls: 0,
    failures: 0,
    last_ok_unix: None,
    last_error_unix: None,
    last_error: None,
});

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn record_health(result: &Result<Vec<Vec<f32>>, String>) {
    let mut health = HEALTH.lock().unwrap_or_else(|e| e.into_inner());
    health.calls += 1;
    match result {
        Ok(_) => health.last_ok_unix = Some(unix_now()),
        Err(err) => {
            health.failures += 1;
            health.last_error_unix = Some(unix_now());
            health.last_error = Some(body_snippet(err));
        }
    }
}

pub fn embedding_health() -> EmbeddingHealth {
    HEALTH.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Whether this process can embed right now, and with what. The one place
/// `/status`, server start-up and `fieldcraft doctor` get that answer from,
/// so BM25-only operation is always stated, never silent.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EmbeddingReadiness {
    pub provider: String,
    pub model: String,
    pub dims: usize,
    pub tag: Option<String>,
    /// `dense` (hybrid retrieval) or `bm25-only`.
    pub mode: &'static str,
    /// The provider answers now (always false in BM25-only mode).
    pub ready: bool,
    /// Why not ready, or why BM25-only.
    pub problem: Option<String>,
}

pub fn embedding_readiness() -> EmbeddingReadiness {
    let spec = match EmbeddingSpec::from_env() {
        Ok(spec) => spec,
        Err(err) => {
            return EmbeddingReadiness {
                provider: "invalid".into(),
                model: String::new(),
                dims: 0,
                tag: None,
                mode: "bm25-only",
                ready: false,
                problem: Some(err),
            };
        }
    };
    let problem = match spec.provider {
        EmbeddingProvider::None => Some(
            "FIELDCRAFT_EMBED_PROVIDER=none: BM25-only retrieval; merge, resurrection and dream \
             stay inert"
                .to_string(),
        ),
        EmbeddingProvider::Gemini => env_key(&["GEMINI_API_KEY", "GOOGLE_API_KEY"])
            .is_none()
            .then(|| "no GEMINI_API_KEY in env or secrets.env".to_string()),
        EmbeddingProvider::Gemma => gemma::probe(&spec).err(),
    };
    EmbeddingReadiness {
        provider: spec.provider.as_str().to_string(),
        model: spec.model.clone(),
        dims: spec.dims,
        tag: spec.tag(),
        mode: if spec.is_dense() {
            "dense"
        } else {
            "bm25-only"
        },
        ready: spec.is_dense() && problem.is_none(),
        problem,
    }
}

#[derive(Debug, Clone)]
struct GeminiConfig {
    model: String,
    dim: usize,
    max_chars: usize,
    sleep_ms: u64,
    /// API root; overridable (`FIELDCRAFT_EMBED_GEMINI_BASE_URL`) for tests and proxies.
    base_url: String,
}

impl GeminiConfig {
    fn from_spec(spec: &EmbeddingSpec) -> Result<Self, String> {
        Self::from_values(
            Some(&spec.model),
            Some(&spec.dims.to_string()),
            env_non_empty("FIELDCRAFT_EMBED_MAX_CHARS").as_deref(),
            env_non_empty("FIELDCRAFT_EMBED_SLEEP_MS").as_deref(),
            env_non_empty("FIELDCRAFT_EMBED_GEMINI_BASE_URL").as_deref(),
        )
    }

    fn from_values(
        model: Option<&str>,
        dim: Option<&str>,
        max_chars: Option<&str>,
        sleep_ms: Option<&str>,
        base_url: Option<&str>,
    ) -> Result<Self, String> {
        Ok(Self {
            model: model
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(fieldcraft_core::embedding::GEMINI_DEFAULT_MODEL)
                .to_string(),
            dim: parse_positive_usize(
                "FIELDCRAFT_EMBED_DIM",
                dim,
                fieldcraft_core::embedding::DEFAULT_DIMS,
            )?,
            max_chars: parse_positive_usize(
                "FIELDCRAFT_EMBED_MAX_CHARS",
                max_chars,
                DEFAULT_MAX_CHARS,
            )?,
            sleep_ms: parse_u64("FIELDCRAFT_EMBED_SLEEP_MS", sleep_ms, 0)?,
            base_url: base_url
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(DEFAULT_GEMINI_BASE_URL)
                .trim_end_matches('/')
                .to_string(),
        })
    }

    fn batch_url(&self) -> String {
        format!("{}/{}:batchEmbedContents", self.base_url, self.model)
    }
}

/// One blocking client per process. Building a `reqwest::blocking::Client`
/// spawns a runtime thread and parks the caller until it answers; doing that
/// once per batch multiplied the chances of the hang described on
/// `encode_with_config`.
static HTTP_CLIENT: OnceLock<Result<Client, String>> = OnceLock::new();

fn http_client() -> Result<&'static Client, String> {
    HTTP_CLIENT
        .get_or_init(|| {
            Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .map_err(|err| format!("embedding HTTP client build failed: {err}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}

fn encode_texts_gemini(
    spec: &EmbeddingSpec,
    texts: &[String],
    batch: usize,
) -> Result<Vec<Vec<f32>>, String> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let config = GeminiConfig::from_spec(spec)?;
    let key = env_key(&["GEMINI_API_KEY", "GOOGLE_API_KEY"]).ok_or_else(|| {
        "Gemini embedding API key missing; set GEMINI_API_KEY or GOOGLE_API_KEY".to_string()
    })?;
    encode_with_config(&config, &key, texts, batch)
}

/// Run blocking HTTP work on a dedicated OS thread, never on the caller's
/// thread (see `encode_with_config` for why).
pub(crate) fn off_runtime<T: Send>(
    work: impl FnOnce() -> Result<T, String> + Send,
) -> Result<T, String> {
    thread::scope(|scope| {
        scope
            .spawn(work)
            .join()
            .unwrap_or_else(|_| Err("embedding worker thread panicked".to_string()))
    })
}

/// Run the blocking Gemini calls on a dedicated OS thread, never on the
/// caller's thread.
///
/// Callers sit inside tokio runtimes (the CLI's current-thread runtime, the
/// server's workers). `reqwest::blocking` hands each request to its own
/// runtime thread and parks the calling thread until it answers. On a tokio
/// current-thread runtime that park/unpark pair can be lost and the caller
/// sleeps forever at 0% CPU — `fieldcraft reindex` froze that way mid-backfill
/// on 2026-07-06 and again on 2026-09-27 (stack: `cmd_reindex` →
/// `encode_dense` → `ClientBuilder::build` → `thread::park`). A scoped thread
/// has no such interaction, and the retry sleeps no longer stall a runtime
/// worker either.
fn encode_with_config(
    config: &GeminiConfig,
    key: &str,
    texts: &[String],
    batch: usize,
) -> Result<Vec<Vec<f32>>, String> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    off_runtime(|| encode_blocking(config, key, texts, batch))
}

fn encode_blocking(
    config: &GeminiConfig,
    key: &str,
    texts: &[String],
    batch: usize,
) -> Result<Vec<Vec<f32>>, String> {
    let client = http_client()?;
    let url = config.batch_url();
    let chunk_size = batch.clamp(1, MAX_GEMINI_BATCH);
    let mut out = Vec::with_capacity(texts.len());
    for (batch_idx, part) in texts.chunks(chunk_size).enumerate() {
        if config.sleep_ms > 0 && batch_idx > 0 {
            thread::sleep(Duration::from_millis(config.sleep_ms));
        }
        let body = build_batch_request(config, part);
        let vectors = send_batch(client, &url, key, &body, part.len())?;
        out.extend(vectors);
    }
    Ok(out)
}

fn send_batch(
    client: &Client,
    url: &str,
    key: &str,
    body: &Value,
    expected_embeddings: usize,
) -> Result<Vec<Vec<f32>>, String> {
    for attempt in 0..MAX_ATTEMPTS {
        let result = client
            .post(url)
            .header("x-goog-api-key", key)
            .json(body)
            .send();
        match result {
            Ok(response) => {
                let status = response.status();
                let response_body = response
                    .text()
                    .map_err(|err| format!("Gemini embedding response body read failed: {err}"))?;
                if status.is_success() {
                    let data: Value = serde_json::from_str(&response_body)
                        .map_err(|err| format!("Gemini embedding response was not JSON: {err}"))?;
                    return parse_embedding_response(&data, expected_embeddings);
                }
                if is_retryable_status(status.as_u16()) && attempt + 1 < MAX_ATTEMPTS {
                    let delay = if status.as_u16() == 429 {
                        retry_delay_from_error_json(&response_body)
                    } else {
                        None
                    }
                    .unwrap_or_else(|| retry_backoff(attempt));
                    thread::sleep(delay);
                    continue;
                }
                return Err(format!(
                    "Gemini embedding request failed with HTTP {status}: {}",
                    body_snippet(&response_body)
                ));
            }
            Err(err) => {
                if attempt + 1 < MAX_ATTEMPTS {
                    thread::sleep(retry_backoff(attempt));
                    continue;
                }
                return Err(format!(
                    "Gemini embedding transport failed after {MAX_ATTEMPTS} attempts: {err}"
                ));
            }
        }
    }
    Err("Gemini embedding request exhausted retries".to_string())
}

fn build_batch_request(config: &GeminiConfig, texts: &[String]) -> Value {
    let requests: Vec<Value> = texts
        .iter()
        .map(|text| {
            let clipped = clip_text(text, config.max_chars);
            json!({
                "model": config.model,
                "content": {
                    "parts": [{
                        "text": clipped,
                    }],
                },
                "outputDimensionality": config.dim,
            })
        })
        .collect();
    json!({ "requests": requests })
}

fn clip_text(text: &str, max_chars: usize) -> String {
    let clipped: String = text.chars().take(max_chars).collect();
    if clipped.is_empty() {
        " ".to_string()
    } else {
        clipped
    }
}

fn parse_embedding_response(
    data: &Value,
    expected_embeddings: usize,
) -> Result<Vec<Vec<f32>>, String> {
    let embeddings = data
        .get("embeddings")
        .and_then(Value::as_array)
        .ok_or_else(|| "Gemini embedding response missing embeddings array".to_string())?;
    if embeddings.len() != expected_embeddings {
        return Err(format!(
            "Gemini embedding response returned {} embeddings for {} requests",
            embeddings.len(),
            expected_embeddings
        ));
    }

    let mut out = Vec::with_capacity(embeddings.len());
    for (idx, embedding) in embeddings.iter().enumerate() {
        let values = embedding
            .get("values")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("Gemini embedding {idx} missing values array"))?;
        if values.is_empty() {
            return Err(format!("Gemini embedding {idx} has empty values array"));
        }
        let mut vector = Vec::with_capacity(values.len());
        for (value_idx, value) in values.iter().enumerate() {
            let number = value.as_f64().ok_or_else(|| {
                format!("Gemini embedding {idx} value {value_idx} is not numeric")
            })?;
            vector.push(number as f32);
        }
        l2_normalize(&mut vector);
        out.push(vector);
    }
    Ok(out)
}

fn l2_normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in vector {
            *value /= norm;
        }
    }
}

fn retry_delay_from_error_json(body: &str) -> Option<Duration> {
    let data: Value = serde_json::from_str(body).ok()?;
    data.get("error")
        .and_then(|error| error.get("details"))
        .and_then(Value::as_array)?
        .iter()
        .find_map(|detail| {
            let type_name = detail.get("@type").and_then(Value::as_str)?;
            if !type_name.ends_with("RetryInfo") {
                return None;
            }
            detail
                .get("retryDelay")
                .and_then(Value::as_str)
                .and_then(parse_retry_delay)
        })
}

fn parse_retry_delay(value: &str) -> Option<Duration> {
    let seconds = value.strip_suffix('s')?.parse::<f64>().ok()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(seconds.min(MAX_RETRY_DELAY_SECS)))
}

fn retry_backoff(attempt: usize) -> Duration {
    Duration::from_secs_f64((2.0_f64).powi(attempt as i32).min(MAX_RETRY_DELAY_SECS))
}

fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 503)
}

fn body_snippet(body: &str) -> String {
    body.chars().take(200).collect()
}

/// API keys: process environment first, then `~/.fieldcraft/secrets.env`
/// (see `fieldcraft_core::secrets`), so a reinstall that rewrites launchd
/// plists or shell env cannot silently drop the embedding key.
fn env_key(names: &[&str]) -> Option<String> {
    fieldcraft_core::secrets::lookup_any(names).map(|(value, _)| value)
}

fn env_non_empty(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn parse_positive_usize(name: &str, value: Option<&str>, default: usize) -> Result<usize, String> {
    let Some(value) = value else {
        return Ok(default);
    };
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("{name} must be a positive integer"))?;
    if parsed == 0 {
        return Err(format!("{name} must be greater than 0"));
    }
    Ok(parsed)
}

fn parse_u64(name: &str, value: Option<&str>, default: u64) -> Result<u64, String> {
    let Some(value) = value else {
        return Ok(default);
    };
    value
        .parse::<u64>()
        .map_err(|_| format!("{name} must be a non-negative integer"))
}

pub(crate) fn internal(message: impl Into<String>) -> TrError {
    TrError::Internal {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_gemini_batch_request_with_clipping_space_model_and_dim() {
        let config =
            GeminiConfig::from_values(Some("models/custom"), Some("12"), Some("3"), None, None)
                .unwrap();
        let body = build_batch_request(&config, &["abcdef".to_string(), "".to_string()]);
        let requests = body["requests"].as_array().unwrap();

        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["model"], "models/custom");
        assert_eq!(requests[0]["outputDimensionality"], 12);
        assert_eq!(requests[0]["content"]["parts"][0]["text"], "abc");
        assert_eq!(requests[1]["content"]["parts"][0]["text"], " ");
    }

    #[test]
    fn parse_embedding_response_normalizes_vectors() {
        let data = json!({
            "embeddings": [
                { "values": [3.0, 4.0] },
                { "values": [0.0, 2.0] }
            ]
        });
        let vectors = parse_embedding_response(&data, 2).unwrap();

        assert!((vectors[0][0] - 0.6).abs() < 1e-6);
        assert!((vectors[0][1] - 0.8).abs() < 1e-6);
        assert!((vectors[1][0] - 0.0).abs() < 1e-6);
        assert!((vectors[1][1] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn parse_embedding_response_fails_loud_on_count_mismatch() {
        let data = json!({ "embeddings": [{ "values": [1.0] }] });
        let err = parse_embedding_response(&data, 2).unwrap_err();
        assert!(err.contains("returned 1 embeddings for 2 requests"));
    }

    #[test]
    fn parse_embedding_response_fails_loud_on_empty_values() {
        let data = json!({ "embeddings": [{ "values": [] }] });
        let err = parse_embedding_response(&data, 1).unwrap_err();
        assert!(err.contains("empty values"));
    }

    #[test]
    fn l2_normalize_leaves_zero_vector_unchanged() {
        let mut vector = vec![0.0, 0.0];
        l2_normalize(&mut vector);
        assert_eq!(vector, vec![0.0, 0.0]);
    }

    #[test]
    fn retry_delay_parser_honors_retry_info() {
        let body = json!({
            "error": {
                "details": [
                    { "@type": "type.googleapis.com/google.rpc.DebugInfo" },
                    {
                        "@type": "type.googleapis.com/google.rpc.RetryInfo",
                        "retryDelay": "7s"
                    }
                ]
            }
        })
        .to_string();

        assert_eq!(
            retry_delay_from_error_json(&body),
            Some(Duration::from_secs(7))
        );
    }

    #[test]
    fn retry_delay_parser_caps_large_values() {
        assert_eq!(
            parse_retry_delay("100s"),
            Some(Duration::from_secs_f64(MAX_RETRY_DELAY_SECS))
        );
    }

    /// Minimal batchEmbedContents stand-in: answers each request with one
    /// unit vector per text, `Connection: close` so every call reconnects.
    fn serve_mock_embeddings(stream: &mut std::net::TcpStream) {
        use std::io::{Read, Write};
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let (header_end, content_length) = loop {
            let n = stream.read(&mut chunk).unwrap();
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
                let len = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                break (pos + 4, len);
            }
        };
        while buf.len() < header_end + content_length {
            let n = stream.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let body: Value = serde_json::from_slice(&buf[header_end..header_end + content_length])
            .expect("mock received a JSON body");
        let requests = body["requests"].as_array().map(Vec::len).unwrap_or(0);
        let dim = body["requests"][0]["outputDimensionality"]
            .as_u64()
            .unwrap_or(4) as usize;
        let embeddings: Vec<Value> = (0..requests)
            .map(|i| {
                let values: Vec<f64> = (0..dim)
                    .map(|d| if d == i % dim { 1.0 } else { 0.0 })
                    .collect();
                json!({ "values": values })
            })
            .collect();
        let payload = json!({ "embeddings": embeddings }).to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            payload.len(),
            payload
        )
        .unwrap();
        stream.flush().unwrap();
    }

    /// Regression: `fieldcraft reindex` runs `encode_dense` from a tokio
    /// current-thread runtime and used to freeze after a few dozen batches
    /// (lost park/unpark inside `reqwest::blocking`). With the work on a
    /// scoped thread, hundreds of calls from that context must complete.
    #[test]
    fn encode_from_a_current_thread_runtime_does_not_hang() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                thread::spawn(move || serve_mock_embeddings(&mut stream));
            }
        });
        let config = GeminiConfig::from_values(
            Some("models/mock"),
            Some("4"),
            None,
            None,
            Some(&format!("http://{addr}")),
        )
        .unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result = rt.block_on(async {
                for i in 0..200 {
                    let texts = vec![format!("lesson {i}"), "other".to_string()];
                    let vectors = encode_with_config(&config, "test-key", &texts, 100)?;
                    if vectors.len() != 2 || vectors[0].len() != 4 {
                        return Err(format!("unexpected shape on call {i}"));
                    }
                }
                Ok::<(), String>(())
            });
            let _ = tx.send(result);
        });

        match rx.recv_timeout(Duration::from_secs(90)) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => panic!("encode failed: {err}"),
            Err(_) => panic!("encode hung when called from a current-thread tokio runtime"),
        }
    }
}
