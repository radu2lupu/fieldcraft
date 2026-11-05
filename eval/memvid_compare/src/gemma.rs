//! Local EmbeddingGemma 2 vectors for the G4 quality gate (docs/plans/local-embeddings-and-query-load.md).
//!
//! Prompt format, character cap and the "text too large" shrink rule mirror
//! `crates/fieldcraft-retrieval/src/gemma.rs`. That module's items are crate-private and
//! it pulls in the whole retrieval stack, so the constants are restated here and a test
//! (`production_constants_match`) fails if the production source stops containing them.
//!
//! Vectors are cached as raw little-endian f32 records under
//! `<gemma dir>/vectors/<fingerprint>/{docs,queries}.rec`, appended as batches finish, so a
//! killed run resumes and a rerun costs nothing. The fingerprint covers the model file hash
//! and every formatting choice, so a changed prompt can never reuse stale vectors.

use crate::embed::l2_normalize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

pub const DIM: usize = 768;
pub const MODEL_ALIAS: &str = "embeddinggemma-2-Q8_0";
pub const QUERY_PREFIX: &str = "task: search result | query: ";
pub const DOCUMENT_PREFIX: &str = "title: none | text: ";
/// Production default `DEFAULT_MAX_CHARS`.
pub const MAX_CHARS: usize = 6000;
pub const GEMMA_DIR: &str = "/Volumes/Development SSD/fieldcraft-gemma-eval";

/// Record flag: the text was cut by the character cap.
pub const FLAG_CLIPPED: u8 = 1;
/// Record flag: the server refused the text as too large and it was embedded at a shorter cap.
pub const FLAG_SHRUNK: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Query,
    Document,
}

pub fn gemma_dir() -> PathBuf {
    PathBuf::from(std::env::var("GEMMA_EVAL_DIR").unwrap_or_else(|_| GEMMA_DIR.to_string()))
}

pub fn default_model_path() -> PathBuf {
    gemma_dir().join("model/embeddinggemma-2-Q8_0.gguf")
}

/// Production `clip_text`: first `max_chars` characters, a single space when empty.
pub fn clip_text(text: &str, max_chars: usize) -> String {
    let clipped: String = text.chars().take(max_chars).collect();
    if clipped.is_empty() { " ".to_string() } else { clipped }
}

/// Production `prompt`: role prefix followed by the clipped text.
pub fn prompt(role: Role, text: &str, max_chars: usize) -> String {
    let prefix = match role {
        Role::Query => QUERY_PREFIX,
        Role::Document => DOCUMENT_PREFIX,
    };
    format!("{prefix}{}", clip_text(text, max_chars))
}

pub fn file_sha256(path: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Cache key: model file hash plus every choice that changes a vector.
pub fn fingerprint(model_sha256: &str) -> String {
    let mut h = Sha256::new();
    for part in [model_sha256, QUERY_PREFIX, DOCUMENT_PREFIX, &MAX_CHARS.to_string(), "pooling=mean", "l2=true", "dtype=f32"] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    hex(&h.finalize())[..16].to_string()
}

pub struct Record {
    pub vector: Vec<f32>,
    pub flags: u8,
    /// Norm of the vector as the server returned it, before our L2 normalisation.
    pub raw_norm: f32,
}

/// Append-only record file keyed by string id.
pub struct VectorCache {
    path: PathBuf,
    pub map: HashMap<String, Record>,
}

impl VectorCache {
    /// Loads every complete record; a torn trailing record (killed mid-write) is ignored and
    /// overwritten by the next append.
    pub fn open(dir: &Path, fp: &str, kind: &str) -> Result<Self, String> {
        let d = dir.join("vectors").join(fp);
        std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
        let path = d.join(format!("{kind}.rec"));
        let bytes = std::fs::read(&path).unwrap_or_default();
        let (map, good) = parse_records(&bytes);
        if good < bytes.len() {
            let f = std::fs::OpenOptions::new().write(true).open(&path).map_err(|e| e.to_string())?;
            f.set_len(good as u64).map_err(|e| e.to_string())?;
        }
        Ok(Self { path, map })
    }

    pub fn append(&mut self, id: &str, rec: Record) -> Result<(), String> {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.path).map_err(|e| e.to_string())?;
        f.write_all(&encode_record(id, &rec)).map_err(|e| e.to_string())?;
        self.map.insert(id.to_string(), rec);
        Ok(())
    }
}

fn encode_record(id: &str, rec: &Record) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + id.len() + 5 + DIM * 4);
    out.extend_from_slice(&(id.len() as u16).to_le_bytes());
    out.extend_from_slice(id.as_bytes());
    out.push(rec.flags);
    out.extend_from_slice(&rec.raw_norm.to_le_bytes());
    for x in &rec.vector {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// Parses complete records; returns them and the byte length they occupy.
fn parse_records(bytes: &[u8]) -> (HashMap<String, Record>, usize) {
    let mut map = HashMap::new();
    let mut pos = 0usize;
    while pos + 2 <= bytes.len() {
        let id_len = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        let end = pos + 2 + id_len + 1 + 4 + DIM * 4;
        if end > bytes.len() {
            break;
        }
        let id = String::from_utf8_lossy(&bytes[pos + 2..pos + 2 + id_len]).into_owned();
        let mut p = pos + 2 + id_len;
        let flags = bytes[p];
        let raw_norm = f32::from_le_bytes(bytes[p + 1..p + 5].try_into().unwrap());
        p += 5;
        let vector = bytes[p..end].as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect();
        map.insert(id, Record { vector, flags, raw_norm });
        pos = end;
    }
    (map, pos)
}

/// Dense candidate set rule: a lesson is searchable by the Gemma arms only when it also has a
/// stored Gemini vector, so Gemma and Gemini twins rank the identical set of lessons.
pub fn candidate_mask(gemini_has: &[bool], gemma_has: &[bool]) -> Vec<bool> {
    gemini_has.iter().zip(gemma_has).map(|(a, b)| *a && *b).collect()
}

/// Gemma document vectors aligned to the corpus order.
pub struct GemmaVectors {
    pub vectors: Vec<f32>,
    /// Has a Gemma vector (before the candidate rule).
    pub has: Vec<bool>,
    /// Candidate set: Gemini vector AND Gemma vector.
    pub candidates: Vec<bool>,
}

/// Aligns cached document vectors to `ids`. `gemini_has` is the corpus' stored-vector mask.
pub fn align(cache: &VectorCache, ids: &[String], gemini_has: &[bool]) -> GemmaVectors {
    let mut vectors = vec![0.0f32; ids.len() * DIM];
    let mut has = vec![false; ids.len()];
    for (i, id) in ids.iter().enumerate() {
        if let Some(r) = cache.map.get(id) {
            vectors[i * DIM..(i + 1) * DIM].copy_from_slice(&r.vector);
            has[i] = true;
        }
    }
    let candidates = candidate_mask(gemini_has, &has);
    GemmaVectors { vectors, has, candidates }
}

/// llama-server `/v1/embeddings` client (local only).
pub struct Embedder {
    url: String,
    client: reqwest::blocking::Client,
}

pub struct EmbedStats {
    pub embedded: usize,
    pub failed: usize,
    pub clipped: usize,
    pub shrunk: usize,
}

impl Embedder {
    pub fn new(url: &str) -> Result<Self, String> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { url: url.trim_end_matches('/').to_string(), client })
    }

    /// The server must serve the production alias, as the production client checks.
    pub fn verify_model(&self) -> Result<(), String> {
        let body: serde_json::Value = self
            .client
            .get(format!("{}/v1/models", self.url))
            .send()
            .map_err(|e| format!("server unreachable at {}: {}", self.url, e.without_url()))?
            .json()
            .map_err(|e| e.to_string())?;
        let served: Vec<&str> = body["data"].as_array().map(|a| a.iter().filter_map(|m| m["id"].as_str()).collect()).unwrap_or_default();
        if served.contains(&MODEL_ALIAS) { Ok(()) } else { Err(format!("server serves {served:?}, expected {MODEL_ALIAS}")) }
    }

    fn post(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let resp = self
            .client
            .post(format!("{}/v1/embeddings", self.url))
            .json(&serde_json::json!({"input": inputs, "model": MODEL_ALIAS}))
            .send()
            .map_err(|e| format!("request failed: {}", e.without_url()))?;
        let status = resp.status();
        let text = resp.text().map_err(|e| e.to_string())?;
        if !status.is_success() {
            // Never echo the body: it can contain request text.
            return Err(if text.contains("too large") { "too large".to_string() } else { format!("HTTP {status}") });
        }
        let data: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let items = data["data"].as_array().ok_or("missing data array")?;
        if items.len() != inputs.len() {
            return Err(format!("{} vectors for {} inputs", items.len(), inputs.len()));
        }
        let mut out = vec![Vec::new(); inputs.len()];
        for (pos, item) in items.iter().enumerate() {
            let idx = item["index"].as_u64().map_or(pos, |i| i as usize);
            let v: Option<Vec<f32>> = item["embedding"].as_array().ok_or("missing embedding")?.iter().map(|x| x.as_f64().map(|f| f as f32)).collect();
            let v = v.ok_or("non-number in embedding")?;
            if idx >= inputs.len() || v.len() != DIM {
                return Err(format!("embedding {idx} has {} dims, expected {DIM}", v.len()));
            }
            out[idx] = v;
        }
        Ok(out)
    }

    /// One text at progressively shorter caps (production `encode_one_shrinking`).
    fn post_shrinking(&self, role: Role, text: &str) -> Result<Vec<f32>, String> {
        let mut cap = MAX_CHARS;
        for _ in 0..4 {
            cap /= 2;
            match self.post(&[prompt(role, text, cap)]) {
                Ok(mut v) => return Ok(v.remove(0)),
                Err(e) if e == "too large" => continue,
                Err(e) => return Err(e),
            }
        }
        Err("text too large even at 1/16 of the cap".into())
    }

    /// Embeds `items` (id, text) missing from `cache` with `workers` parallel requests of
    /// `batch` texts each, appending each result to the cache file as it arrives.
    pub fn embed_missing(&self, cache: &mut VectorCache, role: Role, items: &[(String, String)], workers: usize, batch: usize) -> Result<EmbedStats, String> {
        let todo: Vec<&(String, String)> = items.iter().filter(|(id, _)| !cache.map.contains_key(id)).collect();
        let mut stats = EmbedStats { embedded: 0, failed: 0, clipped: 0, shrunk: 0 };
        if todo.is_empty() {
            return Ok(stats);
        }
        let chunks: Vec<&[&(String, String)]> = todo.chunks(batch).collect();
        let next = AtomicUsize::new(0);
        let (tx, rx) = mpsc::channel::<Vec<(String, Result<(Vec<f32>, u8), String>)>>();
        let total = todo.len();
        let mut done = 0usize;
        let mut write_err: Option<String> = None;
        std::thread::scope(|s| {
            for _ in 0..workers {
                let tx = tx.clone();
                let (next, chunks) = (&next, &chunks);
                s.spawn(move || {
                    loop {
                        let n = next.fetch_add(1, Ordering::Relaxed);
                        let Some(chunk) = chunks.get(n) else { break };
                        let inputs: Vec<String> = chunk.iter().map(|(_, t)| prompt(role, t, MAX_CHARS)).collect();
                        let batched = self.post(&inputs);
                        let mut results = Vec::with_capacity(chunk.len());
                        for (j, (id, text)) in chunk.iter().map(|p| (&p.0, &p.1)).enumerate() {
                            let clipped = if text.chars().count() > MAX_CHARS { FLAG_CLIPPED } else { 0 };
                            let r = match &batched {
                                Ok(vs) => Ok((vs[j].clone(), clipped)),
                                Err(_) => self.post_shrinking(role, text).map(|v| (v, clipped | FLAG_SHRUNK)),
                            };
                            results.push((id.clone(), r));
                        }
                        if tx.send(results).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(tx);
            for results in rx {
                for (id, r) in results {
                    match r {
                        Ok((mut v, flags)) => {
                            let raw_norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                            l2_normalize(&mut v);
                            stats.clipped += usize::from(flags & FLAG_CLIPPED != 0);
                            stats.shrunk += usize::from(flags & FLAG_SHRUNK != 0);
                            stats.embedded += 1;
                            if let Err(e) = cache.append(&id, Record { vector: v, flags, raw_norm }) {
                                write_err.get_or_insert(e);
                            }
                        }
                        Err(_) => stats.failed += 1,
                    }
                    done += 1;
                    if done.is_multiple_of(2000) {
                        eprintln!("embedded {done}/{total}");
                    }
                }
            }
        });
        match write_err {
            Some(e) => Err(e),
            None => Ok(stats),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_follow_the_model_card() {
        assert_eq!(prompt(Role::Query, "restart the server", MAX_CHARS), "task: search result | query: restart the server");
        assert_eq!(prompt(Role::Document, "abc", MAX_CHARS), "title: none | text: abc");
        assert_eq!(prompt(Role::Document, "", MAX_CHARS), "title: none | text:  ");
        let long = prompt(Role::Document, &"é".repeat(MAX_CHARS + 50), MAX_CHARS);
        assert_eq!(long.chars().count(), DOCUMENT_PREFIX.chars().count() + MAX_CHARS);
    }

    #[test]
    fn production_constants_match() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../crates/fieldcraft-retrieval/src/gemma.rs"))
            .expect("production gemma.rs readable from the harness");
        for line in [
            format!("const QUERY_PREFIX: &str = \"{QUERY_PREFIX}\";"),
            format!("const DOCUMENT_PREFIX: &str = \"{DOCUMENT_PREFIX}\";"),
            format!("const DEFAULT_MAX_CHARS: usize = {MAX_CHARS};"),
        ] {
            assert!(src.contains(&line), "production source no longer contains: {line}");
        }
    }

    #[test]
    fn fingerprint_changes_with_the_model_hash() {
        let a = fingerprint("aaaa");
        assert_eq!(a, fingerprint("aaaa"));
        assert_ne!(a, fingerprint("bbbb"));
        assert_eq!(a.len(), 16);
    }

    fn rec(seed: f32, flags: u8) -> Record {
        Record { vector: (0..DIM).map(|i| seed + i as f32).collect(), flags, raw_norm: 1.5 }
    }

    #[test]
    fn cache_round_trips_resumes_and_survives_a_torn_tail() {
        let dir = std::env::temp_dir().join(format!("gemma_cache_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut c = VectorCache::open(&dir, "fp1", "docs").unwrap();
        c.append("lesson-a", rec(1.0, FLAG_CLIPPED)).unwrap();
        c.append("lesson-b", rec(2.0, 0)).unwrap();
        // Torn write: half a record.
        let path = dir.join("vectors/fp1/docs.rec");
        let full = encode_record("lesson-c", &rec(3.0, 0));
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(&full[..full.len() / 2]).unwrap();
        let mut c2 = VectorCache::open(&dir, "fp1", "docs").unwrap();
        assert_eq!(c2.map.len(), 2);
        assert_eq!(c2.map["lesson-a"].flags, FLAG_CLIPPED);
        assert_eq!(c2.map["lesson-b"].vector[5], 7.0);
        c2.append("lesson-c", rec(3.0, 0)).unwrap();
        assert_eq!(VectorCache::open(&dir, "fp1", "docs").unwrap().map.len(), 3);
        // A different fingerprint is a different, empty cache.
        assert!(VectorCache::open(&dir, "fp2", "docs").unwrap().map.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn candidate_set_needs_both_vectors() {
        assert_eq!(candidate_mask(&[true, true, false, false], &[true, false, true, false]), vec![true, false, false, false]);
    }

    #[test]
    fn align_marks_missing_gemma_vectors_as_non_candidates() {
        let dir = std::env::temp_dir().join(format!("gemma_align_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut c = VectorCache::open(&dir, "fp", "docs").unwrap();
        c.append("a", rec(0.0, 0)).unwrap();
        let ids = vec!["a".to_string(), "b".to_string()];
        let g = align(&c, &ids, &[true, true]);
        assert_eq!(g.has, vec![true, false]);
        assert_eq!(g.candidates, vec![true, false]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
