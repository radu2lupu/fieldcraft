//! Query embeddings: Gemini `gemini-embedding-001`, mirroring `fieldcraft-retrieval`
//! (same model, `outputDimensionality`, no task type, 8000-char clip, L2 normalisation),
//! cached on disk keyed by retrieval_log id so reruns cost nothing.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

const MODEL: &str = "models/gemini-embedding-001";
const BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
const MAX_CHARS: usize = 8000;
const BATCH: usize = 100;
const MAX_ATTEMPTS: usize = 6;

pub fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v {
            *x /= norm;
        }
    }
}

fn clip(text: &str) -> String {
    let clipped: String = text.chars().take(MAX_CHARS).collect();
    if clipped.is_empty() { " ".to_string() } else { clipped }
}

pub fn load_cache(path: &Path) -> HashMap<i64, Vec<f32>> {
    let Ok(text) = std::fs::read_to_string(path) else { return HashMap::new() };
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| {
            let rid = v.get("rid")?.as_i64()?;
            let vec = v.get("v")?.as_array()?.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect();
            Some((rid, vec))
        })
        .collect()
}

fn post_batch(client: &reqwest::blocking::Client, key: &str, dim: usize, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
    let requests: Vec<Value> = texts
        .iter()
        .map(|t| json!({"model": MODEL, "content": {"parts": [{"text": clip(t)}]}, "outputDimensionality": dim}))
        .collect();
    let body = json!({ "requests": requests });
    let url = format!("{BASE_URL}/{MODEL}:batchEmbedContents");
    for attempt in 0..MAX_ATTEMPTS {
        match client.post(&url).header("x-goog-api-key", key).json(&body).send() {
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().map_err(|e| format!("body read: {e}"))?;
                if status.is_success() {
                    let data: Value = serde_json::from_str(&text).map_err(|e| format!("not JSON: {e}"))?;
                    let embs = data.get("embeddings").and_then(Value::as_array).ok_or("no embeddings array")?;
                    if embs.len() != texts.len() {
                        return Err(format!("{} embeddings for {} requests", embs.len(), texts.len()));
                    }
                    return embs
                        .iter()
                        .map(|e| {
                            let vals = e.get("values").and_then(Value::as_array).ok_or("missing values")?;
                            let mut v: Vec<f32> = vals.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect();
                            if v.len() != dim {
                                return Err(format!("embedding has {} dims, expected {dim}", v.len()));
                            }
                            l2_normalize(&mut v);
                            Ok(v)
                        })
                        .collect();
                }
                let retryable = status.as_u16() == 429 || status.is_server_error();
                if !retryable || attempt + 1 == MAX_ATTEMPTS {
                    // Status only; the response body can echo request content.
                    return Err(format!("Gemini HTTP {status}"));
                }
            }
            Err(e) => {
                if attempt + 1 == MAX_ATTEMPTS {
                    return Err(format!("Gemini transport failed: {}", e.without_url()));
                }
            }
        }
        std::thread::sleep(Duration::from_secs(2u64.pow(attempt as u32).min(30)));
    }
    Err("retries exhausted".to_string())
}

/// Embed every `(rid, text)` missing from the cache, appending to the cache file as it goes.
/// Returns the full map. The key is read from `GEMINI_API_KEY` / `GOOGLE_API_KEY` and never logged.
pub fn embed_missing(cache_path: &Path, items: &[(i64, String)], dim: usize) -> Result<HashMap<i64, Vec<f32>>, String> {
    let mut cache = load_cache(cache_path);
    cache.retain(|_, v| v.len() == dim);
    let todo: Vec<&(i64, String)> = items.iter().filter(|(rid, _)| !cache.contains_key(rid)).collect();
    if todo.is_empty() {
        return Ok(cache);
    }
    let key = ["GEMINI_API_KEY", "GOOGLE_API_KEY"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
        .ok_or("GEMINI_API_KEY / GOOGLE_API_KEY not set (source ~/.fieldcraft/secrets.env)")?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| e.to_string())?;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(cache_path).map_err(|e| e.to_string())?;
    for chunk in todo.chunks(BATCH) {
        let texts: Vec<&str> = chunk.iter().map(|(_, t)| t.as_str()).collect();
        let vecs = post_batch(&client, &key, dim, &texts)?;
        for ((rid, _), v) in chunk.iter().zip(vecs) {
            writeln!(file, "{}", json!({"rid": rid, "v": v})).map_err(|e| e.to_string())?;
            cache.insert(*rid, v);
        }
        eprintln!("embedded {}/{}", cache.len().min(items.len()), items.len());
    }
    Ok(cache)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_to_unit_length() {
        let mut v = vec![3.0, 4.0];
        l2_normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn empty_text_is_sent_as_a_space_and_long_text_is_clipped() {
        assert_eq!(clip(""), " ");
        assert_eq!(clip(&"x".repeat(9000)).chars().count(), MAX_CHARS);
    }
}
