//! EmbeddingGemma 2 on a local llama.cpp server (`llama-server --embeddings`).
//!
//! Text goes only to `FIELDCRAFT_EMBED_GEMMA_URL` (default
//! `http://127.0.0.1:7778`), never off the machine. The server must serve the
//! model under the alias in the spec (`--alias embeddinggemma-2-Q8_0`); this
//! client checks `/v1/models` before its first request, so vectors from a
//! different model are never stored under this tag.
//!
//! Prompts follow the model card: queries `task: search result | query: …`,
//! documents `title: none | text: …`.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use reqwest::blocking::Client;
use serde_json::{Value, json};

use crate::{EmbeddingSpec, TextRole, body_snippet, clip_text, l2_normalize, off_runtime};

pub(crate) const DEFAULT_URL: &str = "http://127.0.0.1:7778";
const QUERY_PREFIX: &str = "task: search result | query: ";
const DOCUMENT_PREFIX: &str = "title: none | text: ";
/// Character cap per text. The sidecar holds 2,048 tokens per slot; prose is
/// ~4 chars/token, so 6,000 chars fits nearly everything, and the rare text
/// that does not is retried shorter (see `encode_one_shrinking`).
const DEFAULT_MAX_CHARS: usize = 6000;
const BATCH: usize = 32;

pub(crate) fn base_url() -> String {
    fieldcraft_core::secrets::lookup("FIELDCRAFT_EMBED_GEMMA_URL")
        .unwrap_or_else(|| DEFAULT_URL.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn max_chars() -> usize {
    fieldcraft_core::secrets::lookup("FIELDCRAFT_EMBED_GEMMA_MAX_CHARS")
        .and_then(|v| v.parse().ok())
        .filter(|n: &usize| *n > 0)
        .unwrap_or(DEFAULT_MAX_CHARS)
}

fn client() -> Result<&'static Client, String> {
    static CLIENT: OnceLock<Result<Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(60))
                .build()
                .map_err(|err| format!("Gemma embedding HTTP client build failed: {err}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// `(url, model)` pairs whose model check passed in this process.
fn verified() -> &'static Mutex<HashSet<(String, String)>> {
    static VERIFIED: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    VERIFIED.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(crate) fn prompt(role: TextRole, text: &str, max_chars: usize) -> String {
    let prefix = match role {
        TextRole::Query => QUERY_PREFIX,
        TextRole::Document => DOCUMENT_PREFIX,
    };
    format!("{prefix}{}", clip_text(text, max_chars))
}

pub(crate) fn encode(
    spec: &EmbeddingSpec,
    role: TextRole,
    texts: &[String],
) -> Result<Vec<Vec<f32>>, String> {
    let url = base_url();
    let max_chars = max_chars();
    off_runtime(|| {
        let client = client()?;
        verify_model(client, &url, &spec.model)?;
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(BATCH) {
            let inputs: Vec<String> = chunk.iter().map(|t| prompt(role, t, max_chars)).collect();
            match post(client, &url, &spec.model, &inputs) {
                Ok(vectors) => out.extend(vectors),
                Err(err) if err.contains("too large") => {
                    for text in chunk {
                        out.push(encode_one_shrinking(
                            client,
                            &url,
                            &spec.model,
                            role,
                            text,
                            max_chars,
                        )?);
                    }
                }
                Err(err) => return Err(err),
            }
        }
        for vector in &mut out {
            if vector.len() != spec.dims {
                return Err(format!(
                    "Gemma embedding has {} dims, expected {} (model {})",
                    vector.len(),
                    spec.dims,
                    spec.model
                ));
            }
            l2_normalize(vector);
        }
        Ok(out)
    })
}

/// A text too long for one sidecar slot: halve the character cap until it
/// fits (at most four times).
fn encode_one_shrinking(
    client: &Client,
    url: &str,
    model: &str,
    role: TextRole,
    text: &str,
    mut max_chars: usize,
) -> Result<Vec<f32>, String> {
    for _ in 0..4 {
        max_chars /= 2;
        match post(client, url, model, &[prompt(role, text, max_chars)]) {
            Ok(mut vectors) if vectors.len() == 1 => return Ok(vectors.remove(0)),
            Ok(_) => return Err("Gemma embedding returned the wrong number of vectors".into()),
            Err(err) if err.contains("too large") => continue,
            Err(err) => return Err(err),
        }
    }
    Err("Gemma embedding: text too large even at 1/16 of the character cap".into())
}

/// Is the sidecar up and serving `spec.model`? (for `/status` and `doctor`)
pub(crate) fn probe(spec: &EmbeddingSpec) -> Result<(), String> {
    let url = base_url();
    off_runtime(|| verify_model(client()?, &url, &spec.model))
}

fn verify_model(client: &Client, url: &str, model: &str) -> Result<(), String> {
    let key = (url.to_string(), model.to_string());
    if verified()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&key)
    {
        return Ok(());
    }
    let response = client
        .get(format!("{url}/v1/models"))
        .send()
        .map_err(|err| format!("Gemma embedding server unreachable at {url}: {err}"))?;
    let body: Value = response
        .json()
        .map_err(|err| format!("Gemma embedding server {url}/v1/models: {err}"))?;
    let served = served_models(&body);
    if !served.iter().any(|id| id == model) {
        return Err(format!(
            "Gemma embedding server at {url} serves {served:?}, not {model:?}; \
             start llama-server with --alias {model}"
        ));
    }
    verified()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key);
    Ok(())
}

fn served_models(body: &Value) -> Vec<String> {
    body.get("data")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn post(
    client: &Client,
    url: &str,
    model: &str,
    inputs: &[String],
) -> Result<Vec<Vec<f32>>, String> {
    let response = client
        .post(format!("{url}/v1/embeddings"))
        .json(&json!({ "input": inputs, "model": model }))
        .send()
        .map_err(|err| format!("Gemma embedding request to {url} failed: {err}"))?;
    let status = response.status();
    let text = response
        .text()
        .map_err(|err| format!("Gemma embedding response read failed: {err}"))?;
    if !status.is_success() {
        return Err(format!(
            "Gemma embedding HTTP {status}: {}",
            body_snippet(&text)
        ));
    }
    let data: Value = serde_json::from_str(&text)
        .map_err(|err| format!("Gemma embedding response was not JSON: {err}"))?;
    parse_response(&data, inputs.len())
}

fn parse_response(data: &Value, expected: usize) -> Result<Vec<Vec<f32>>, String> {
    let items = data
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| "Gemma embedding response missing data array".to_string())?;
    if items.len() != expected {
        return Err(format!(
            "Gemma embedding returned {} vectors for {expected} inputs",
            items.len()
        ));
    }
    let mut out = vec![Vec::new(); expected];
    for (position, item) in items.iter().enumerate() {
        let index = item
            .get("index")
            .and_then(Value::as_u64)
            .map(|i| i as usize)
            .unwrap_or(position);
        let values = item
            .get("embedding")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("Gemma embedding {index} missing embedding array"))?;
        let vector: Option<Vec<f32>> = values
            .iter()
            .map(|v| v.as_f64().map(|f| f as f32))
            .collect();
        let vector = vector.ok_or_else(|| format!("Gemma embedding {index} has a non-number"))?;
        if vector.is_empty() || index >= expected {
            return Err(format!("Gemma embedding {index} is empty or out of range"));
        }
        out[index] = vector;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn prompts_follow_the_model_card() {
        assert_eq!(
            prompt(TextRole::Query, "restart the server", 100),
            "task: search result | query: restart the server"
        );
        assert_eq!(
            prompt(TextRole::Document, "abcdef", 3),
            "title: none | text: abc"
        );
    }

    #[test]
    fn response_vectors_follow_their_index() {
        let data = json!({"data": [
            {"index": 1, "embedding": [0.0, 1.0]},
            {"index": 0, "embedding": [1.0, 0.0]}
        ]});
        let vectors = parse_response(&data, 2).unwrap();
        assert_eq!(vectors[0], vec![1.0, 0.0]);
        assert!(parse_response(&data, 3).is_err());
    }

    /// A tiny llama-server stand-in: `/v1/models` lists `served`, and
    /// `/v1/embeddings` answers one 3-dim vector per input.
    fn spawn_server(served: &'static str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut buf = vec![0u8; 65536];
                let mut got = 0;
                loop {
                    let n = stream.read(&mut buf[got..]).unwrap();
                    got += n;
                    let text = String::from_utf8_lossy(&buf[..got]).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if got >= end + 4 + len || n == 0 {
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&buf[..got]).to_string();
                let body = if request.starts_with("GET /v1/models") {
                    json!({"data": [{"id": served}]}).to_string()
                } else {
                    let payload: Value =
                        serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
                    let inputs = payload["input"].as_array().unwrap();
                    assert!(
                        inputs
                            .iter()
                            .all(|i| i.as_str().unwrap().starts_with("title: none | text: "))
                    );
                    let data: Vec<Value> = (0..inputs.len())
                        .map(|i| json!({"index": i, "embedding": [3.0, 4.0, 0.0]}))
                        .collect();
                    json!({"data": data}).to_string()
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn checks_the_served_model_and_normalizes() {
        let url = spawn_server("embeddinggemma-2-Q8_0");
        let client = client().unwrap();
        verify_model(client, &url, "embeddinggemma-2-Q8_0").unwrap();
        let err = verify_model(client, &url, "other-model").unwrap_err();
        assert!(err.contains("not \"other-model\""), "{err}");

        let mut vectors = post(
            client,
            &url,
            "embeddinggemma-2-Q8_0",
            &[prompt(TextRole::Document, "a", 10)],
        )
        .unwrap();
        l2_normalize(&mut vectors[0]);
        assert!((vectors[0][0] - 0.6).abs() < 1e-6);
    }
}
