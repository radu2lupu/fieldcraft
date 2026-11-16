//! Search over items (things apps push), apart from lesson retrieval.
//!
//! Profile: BM25 over passages (best passage per item) and dense cosine over
//! passage vectors of the query's tag (best passage per item), fused with
//! reciprocal rank fusion (k = 60), plus a mild recency bonus. Lessons keep
//! their own pipeline (`retrieval.rs`); nothing here reads `knowledge`.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use fieldcraft_core::TrError;
use fieldcraft_core::embedding::EmbeddingSpec;
use fieldcraft_storage::ItemStore;
use fieldcraft_storage::repository::cosine_similarity;
use serde::Serialize;

use crate::retrieval::{QueryVector, rrf_fuse};
use crate::sqlite_search::query_terms;
use crate::{TextRole, encode_with_spec};

/// RRF constant for items (same as IWT's local index).
pub const ITEM_RRF_K: f32 = 60.0;
/// Age at which the recency bonus halves.
pub const ITEM_RECENCY_HALF_LIFE_DAYS: f64 = 180.0;
/// Largest recency bonus: a tenth of what a first place in one list earns,
/// so recency only reorders items whose fused ranks are close.
pub const ITEM_RECENCY_WEIGHT: f32 = 0.1 / (ITEM_RRF_K + 1.0);
/// Candidates taken from each list before fusion.
const MIN_CANDIDATES: usize = 50;
const SNIPPET_CHARS: usize = 200;

#[derive(Debug, Clone, Default)]
pub struct ItemSearchRequest {
    pub query: String,
    /// Item kinds to keep; empty keeps all.
    pub kinds: Vec<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ItemSearchHit {
    pub source: String,
    pub external_id: String,
    pub kind: String,
    pub title: String,
    pub url: Option<String>,
    pub snippet: String,
    pub score: f32,
    pub created_at: String,
    pub source_updated_at: Option<String>,
    /// 1-based rank in the BM25 list, when the item was in it.
    pub bm25_rank: Option<usize>,
    /// 1-based rank in the dense list, when the item was in it.
    pub dense_rank: Option<usize>,
}

/// FTS5 expression for a typed query: each meaningful term as a quoted
/// prefix, any of them matching (BM25 ranks items matching more terms higher).
pub fn item_match_expression(query: &str) -> Option<String> {
    let terms = query_terms(query);
    if terms.is_empty() {
        return None;
    }
    Some(
        terms
            .iter()
            .map(|t| format!("\"{}\"*", t.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" OR "),
    )
}

struct Candidate {
    store: usize,
    item_id: String,
    chunk_id: i64,
    snippet: Option<String>,
}

fn key(store: usize, item_id: &str) -> String {
    format!("{store}\u{0}{item_id}")
}

/// Search `stores` (one per source). `query` may be empty (BM25 only); dense
/// scoring uses only vectors of the query's tag, at or above that model's
/// `item_dense_min` cosine.
pub fn search_items(
    stores: &[ItemStore],
    request: &ItemSearchRequest,
    query: &QueryVector,
    now: DateTime<Utc>,
) -> Result<Vec<ItemSearchHit>, TrError> {
    let limit = request.limit.max(1);
    let candidates = (limit * 3).max(MIN_CANDIDATES);
    let mut found: HashMap<String, Candidate> = HashMap::new();

    // BM25: best passage per item in each source, merged by score.
    let mut text_list: Vec<(String, f64)> = Vec::new();
    if let Some(expr) = item_match_expression(&request.query) {
        for (s, store) in stores.iter().enumerate() {
            for hit in store.text_search(&expr, &request.kinds, candidates)? {
                let k = key(s, &hit.item_id);
                text_list.push((k.clone(), hit.bm25));
                found.insert(
                    k,
                    Candidate {
                        store: s,
                        item_id: hit.item_id,
                        chunk_id: hit.chunk_id,
                        snippet: Some(hit.snippet),
                    },
                );
            }
        }
    }
    text_list.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    text_list.truncate(candidates);

    // Dense: best passage cosine per item, vectors of the query's tag only.
    let mut dense_list: Vec<(String, f32)> = Vec::new();
    if let (false, Some(tag)) = (query.is_empty(), query.tag.as_deref()) {
        let floor = fieldcraft_core::embedding::thresholds_for_tag(Some(tag)).item_dense_min;
        let mut best: HashMap<String, (f32, usize, String, i64)> = HashMap::new();
        for (s, store) in stores.iter().enumerate() {
            for cv in store.chunk_vectors(tag, &request.kinds)? {
                let Some(cos) = cosine_similarity(&query.values, &cv.vector) else {
                    continue;
                };
                if cos < floor {
                    continue;
                }
                let k = key(s, &cv.item_id);
                let entry = best
                    .entry(k)
                    .or_insert((f32::MIN, s, cv.item_id.clone(), cv.chunk_id));
                if cos > entry.0 {
                    *entry = (cos, s, cv.item_id, cv.chunk_id);
                }
            }
        }
        for (k, (cos, s, item_id, chunk_id)) in best {
            dense_list.push((k.clone(), cos));
            found.entry(k).or_insert(Candidate {
                store: s,
                item_id,
                chunk_id,
                snippet: None,
            });
        }
        dense_list.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        dense_list.truncate(candidates);
    }

    let text_ranked: Vec<(String, f32)> = text_list
        .iter()
        .map(|(k, s)| (k.clone(), *s as f32))
        .collect();
    let text_rank: HashMap<&str, usize> = text_list
        .iter()
        .enumerate()
        .map(|(i, (k, _))| (k.as_str(), i + 1))
        .collect();
    let dense_rank: HashMap<&str, usize> = dense_list
        .iter()
        .enumerate()
        .map(|(i, (k, _))| (k.as_str(), i + 1))
        .collect();
    let fused = rrf_fuse(&[text_ranked, dense_list.clone()], ITEM_RRF_K);

    let mut hits = Vec::new();
    for (k, rrf) in fused {
        let Some(c) = found.get(&k) else { continue };
        let store = &stores[c.store];
        let Some(row) = store.get_by_id(&c.item_id)? else {
            continue;
        };
        let snippet = match &c.snippet {
            Some(s) => s.clone(),
            None => passage_snippet(&store.chunk_text(c.chunk_id)?.unwrap_or_default()),
        };
        let when = row
            .source_updated_at
            .as_deref()
            .unwrap_or(&row.created_at)
            .parse::<DateTime<Utc>>()
            .ok();
        hits.push(ItemSearchHit {
            source: row.source,
            external_id: row.external_id,
            kind: row.kind,
            title: row.title,
            url: row.url,
            snippet,
            score: rrf + recency_bonus(when, now),
            created_at: row.created_at,
            source_updated_at: row.source_updated_at,
            bm25_rank: text_rank.get(k.as_str()).copied(),
            dense_rank: dense_rank.get(k.as_str()).copied(),
        });
    }
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.source.cmp(&b.source))
            .then_with(|| a.external_id.cmp(&b.external_id))
    });
    hits.truncate(limit);
    Ok(hits)
}

/// `ITEM_RECENCY_WEIGHT` for an item from now, halving every
/// `ITEM_RECENCY_HALF_LIFE_DAYS`; nothing for an unknown date.
pub fn recency_bonus(when: Option<DateTime<Utc>>, now: DateTime<Utc>) -> f32 {
    let Some(when) = when else { return 0.0 };
    let age_days = (now - when).num_seconds().max(0) as f64 / 86_400.0;
    ITEM_RECENCY_WEIGHT * 0.5_f64.powf(age_days / ITEM_RECENCY_HALF_LIFE_DAYS) as f32
}

/// The body part of a passage (after its title line), cut to a snippet.
fn passage_snippet(passage: &str) -> String {
    let body = passage.split_once('\n').map(|(_, b)| b).unwrap_or(passage);
    let mut out: String = body.chars().take(SNIPPET_CHARS).collect();
    if body.chars().count() > SNIPPET_CHARS {
        out.push('…');
    }
    out
}

/// Embed up to `batch` passages of `store` that have no vector under `tag`,
/// with `embed` (documents in, one vector each out), and store the vectors.
/// Returns how many were stored; 0 means nothing is pending.
pub fn embed_pending_item_chunks(
    store: &mut ItemStore,
    tag: &str,
    batch: usize,
    embed: impl FnOnce(&[String]) -> Result<Vec<Vec<f32>>, String>,
) -> Result<usize, String> {
    let pending = store
        .chunks_missing_vector(tag, batch)
        .map_err(|e| e.to_string())?;
    if pending.is_empty() {
        return Ok(0);
    }
    let texts: Vec<String> = pending.iter().map(|(_, t)| t.clone()).collect();
    let vectors = embed(&texts)?;
    if vectors.len() != pending.len() {
        return Err(format!(
            "embedder returned {} vectors for {} passages",
            vectors.len(),
            pending.len()
        ));
    }
    let rows: Vec<(i64, Vec<f32>)> = pending.into_iter().map(|(id, _)| id).zip(vectors).collect();
    store
        .store_chunk_vectors(tag, &rows)
        .map_err(|e| e.to_string())?;
    // Chunks replaced while embedding are skipped by the store, but they
    // still count as progress: they are no longer pending.
    Ok(rows.len())
}

/// [`embed_pending_item_chunks`] with the configured provider's document
/// embedding. `Ok(0)` in BM25-only mode.
pub fn embed_pending_item_chunks_with_spec(
    store: &mut ItemStore,
    spec: &EmbeddingSpec,
    batch: usize,
) -> Result<usize, String> {
    let Some(tag) = spec.tag() else {
        return Ok(0);
    };
    embed_pending_item_chunks(store, &tag, batch, |texts| {
        encode_with_spec(spec, TextRole::Document, texts)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fieldcraft_storage::{ItemOp, ItemUpsert};
    use tempfile::tempdir;

    const TAG: &str = "fake:hash:16";

    /// Deterministic bag-of-words embedder: each lowercase word adds to one
    /// of 16 buckets, then L2-normalized. Synonyms are made to share buckets
    /// via `alias`, so dense can find what BM25 cannot.
    fn fake_embed(text: &str) -> Vec<f32> {
        let mut v = [0.0_f32; 16];
        for word in text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
        {
            let w = word.to_lowercase();
            let w = match w.as_str() {
                "espresso" | "coffee" | "barista" => "coffee",
                "sneakers" | "trainers" | "shoes" => "shoes",
                other => other,
            }
            .to_string();
            let bucket = w.bytes().fold(7u32, |h, b| h.wrapping_mul(31) ^ b as u32) % 16;
            v[bucket as usize] += 1.0;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
        v.iter().map(|x| x / norm).collect()
    }

    fn put(store: &mut ItemStore, id: &str, kind: &str, title: &str, body: &str, when: &str) {
        store
            .apply_batch(
                &[ItemOp::Upsert(ItemUpsert {
                    external_id: id.into(),
                    kind: kind.into(),
                    title: title.into(),
                    body: body.into(),
                    url: Some(format!("https://example.com/{id}")),
                    source_updated_at: Some(when.into()),
                    ..Default::default()
                })],
                None,
            )
            .unwrap();
    }

    fn seeded() -> (tempfile::TempDir, ItemStore) {
        let dir = tempdir().unwrap();
        let mut s = ItemStore::open(dir.path(), "iwt").unwrap();
        put(
            &mut s,
            "grinder",
            "save",
            "Burr grinder review",
            "The best espresso grinder for home baristas",
            "2026-11-01T00:00:00Z",
        );
        put(
            &mut s,
            "trainers",
            "like",
            "Running trainers sale",
            "Lightweight trainers for marathon runners",
            "2026-10-01T00:00:00Z",
        );
        put(
            &mut s,
            "rust",
            "bookmark",
            "Rust ownership guide",
            "Borrowing and lifetimes in Rust",
            "2025-01-01T00:00:00Z",
        );
        (dir, s)
    }

    fn now() -> DateTime<Utc> {
        "2026-11-16T00:00:00Z".parse().unwrap()
    }

    fn ids(hits: &[ItemSearchHit]) -> Vec<&str> {
        hits.iter().map(|h| h.external_id.as_str()).collect()
    }

    #[test]
    fn keyword_search_works_without_an_embedder() {
        let (_dir, s) = seeded();
        let req = ItemSearchRequest {
            query: "rust lifetimes".into(),
            limit: 10,
            ..Default::default()
        };
        let hits =
            search_items(std::slice::from_ref(&s), &req, &QueryVector::none(), now()).unwrap();
        assert_eq!(ids(&hits), vec!["rust"]);
        let hit = &hits[0];
        assert_eq!(hit.source, "iwt");
        assert_eq!(hit.kind, "bookmark");
        assert_eq!(hit.title, "Rust ownership guide");
        assert_eq!(hit.url.as_deref(), Some("https://example.com/rust"));
        assert!(hit.snippet.contains("lifetimes"), "{}", hit.snippet);
        assert_eq!(hit.bm25_rank, Some(1));
        assert_eq!(hit.dense_rank, None);
        assert!(hit.score > 0.0);

        // Prefix matching (typing as you go) and the kinds filter.
        let typing = ItemSearchRequest {
            query: "grind".into(),
            limit: 10,
            ..Default::default()
        };
        let hits = search_items(
            std::slice::from_ref(&s),
            &typing,
            &QueryVector::none(),
            now(),
        )
        .unwrap();
        assert_eq!(ids(&hits), vec!["grinder"]);
        let filtered = ItemSearchRequest {
            kinds: vec!["like".into()],
            ..typing
        };
        assert!(
            search_items(
                std::slice::from_ref(&s),
                &filtered,
                &QueryVector::none(),
                now()
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn dense_and_bm25_fuse_with_a_deterministic_embedder() {
        let (_dir, mut s) = seeded();
        let stored = embed_pending_item_chunks(&mut s, TAG, 100, |texts| {
            Ok(texts.iter().map(|t| fake_embed(t)).collect())
        })
        .unwrap();
        assert_eq!(stored, 3);
        assert_eq!(
            embed_pending_item_chunks(&mut s, TAG, 100, |_| panic!("nothing pending")).unwrap(),
            0
        );

        // "coffee shoes": BM25 finds nothing ("coffee" and "shoes" are not
        // in any text); dense finds both synonyms' items.
        let query = QueryVector {
            tag: Some(TAG.into()),
            values: fake_embed("coffee shoes"),
        };
        let req = ItemSearchRequest {
            query: "coffee shoes".into(),
            limit: 2,
            ..Default::default()
        };
        let hits = search_items(std::slice::from_ref(&s), &req, &query, now()).unwrap();
        let mut got = ids(&hits);
        got.sort();
        assert_eq!(got, vec!["grinder", "trainers"]);
        assert!(
            hits.iter()
                .all(|h| h.bm25_rank.is_none() && h.dense_rank.is_some())
        );
        assert!(!hits[0].snippet.is_empty());

        // "espresso trainers": BM25 matches both items by one word each; dense
        // agrees. Items in both lists outrank items in one.
        let query = QueryVector {
            tag: Some(TAG.into()),
            values: fake_embed("espresso trainers"),
        };
        let req = ItemSearchRequest {
            query: "espresso trainers".into(),
            limit: 3,
            ..Default::default()
        };
        let hits = search_items(std::slice::from_ref(&s), &req, &query, now()).unwrap();
        assert_eq!(hits.len(), 3);
        let fused: Vec<&ItemSearchHit> = hits
            .iter()
            .filter(|h| h.bm25_rank.is_some() && h.dense_rank.is_some())
            .collect();
        assert_eq!(fused.len(), 2);
        assert_eq!(hits[2].external_id, "rust");
        assert!(hits[2].bm25_rank.is_none());
        assert!(hits[1].score > hits[2].score);

        // A query vector of another tag is ignored: BM25 only.
        let other = QueryVector {
            tag: Some("other:model:16".into()),
            values: fake_embed("coffee shoes"),
        };
        let req = ItemSearchRequest {
            query: "coffee shoes".into(),
            limit: 5,
            ..Default::default()
        };
        assert!(
            search_items(std::slice::from_ref(&s), &req, &other, now())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn dense_candidates_respect_the_model_floor() {
        let (_dir, mut s) = seeded();
        // Gemma tag: floor 0.6 (IWT's calibration for the same model).
        let tag = "gemma:fake:16";
        embed_pending_item_chunks(&mut s, tag, 100, |texts| {
            Ok(texts.iter().map(|t| fake_embed(t)).collect())
        })
        .unwrap();
        let req = ItemSearchRequest {
            query: "zq".into(),
            limit: 5,
            ..Default::default()
        };
        let unrelated = QueryVector {
            tag: Some(tag.into()),
            values: fake_embed("zq"),
        };
        assert!(
            search_items(std::slice::from_ref(&s), &req, &unrelated, now())
                .unwrap()
                .is_empty()
        );
        let same = QueryVector {
            tag: Some(tag.into()),
            values: fake_embed("Rust ownership guide\nBorrowing and lifetimes in Rust"),
        };
        let hits = search_items(std::slice::from_ref(&s), &req, &same, now()).unwrap();
        assert_eq!(ids(&hits), vec!["rust"]);
        assert_eq!(hits[0].snippet, "Borrowing and lifetimes in Rust");
    }

    #[test]
    fn recency_is_a_mild_tie_breaker() {
        let dir = tempdir().unwrap();
        let mut s = ItemStore::open(dir.path(), "iwt").unwrap();
        put(
            &mut s,
            "old",
            "save",
            "Lamp",
            "desk lamp",
            "2020-01-01T00:00:00Z",
        );
        put(
            &mut s,
            "new",
            "save",
            "Lamp",
            "desk lamp",
            "2026-11-10T00:00:00Z",
        );
        let req = ItemSearchRequest {
            query: "lamp".into(),
            limit: 5,
            ..Default::default()
        };
        let hits =
            search_items(std::slice::from_ref(&s), &req, &QueryVector::none(), now()).unwrap();
        assert_eq!(ids(&hits), vec!["new", "old"]);
        assert!(recency_bonus(Some(now()), now()) <= ITEM_RECENCY_WEIGHT);
        // At most a tenth of what a first place in one list earns.
        let first = 1.0 / (ITEM_RRF_K + 1.0);
        assert!(ITEM_RECENCY_WEIGHT * 10.0 <= first * 1.0001);
        assert_eq!(recency_bonus(None, now()), 0.0);
    }

    #[test]
    fn search_spans_sources() {
        let dir = tempdir().unwrap();
        let mut a = ItemStore::open(dir.path(), "iwt").unwrap();
        let mut b = ItemStore::open(dir.path(), "mail").unwrap();
        put(
            &mut a,
            "x",
            "save",
            "Pricing deck",
            "",
            "2026-11-01T00:00:00Z",
        );
        put(
            &mut b,
            "x",
            "email",
            "Re: pricing",
            "numbers",
            "2026-11-02T00:00:00Z",
        );
        let req = ItemSearchRequest {
            query: "pricing".into(),
            limit: 5,
            ..Default::default()
        };
        let hits = search_items(&[a, b], &req, &QueryVector::none(), now()).unwrap();
        let mut sources: Vec<&str> = hits.iter().map(|h| h.source.as_str()).collect();
        sources.sort();
        assert_eq!(sources, vec!["iwt", "mail"]);
    }

    #[test]
    fn match_expression_quotes_terms() {
        assert_eq!(
            item_match_expression("the Rust \"book\" NEAR").as_deref(),
            Some("\"rust\"* OR \"book\"* OR \"near\"*")
        );
        assert_eq!(item_match_expression("a !!"), None);
    }
}
