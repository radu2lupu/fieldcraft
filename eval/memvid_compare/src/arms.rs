//! Ranking arms shared by Fieldcraft and Memvid: the common guard/trim step, RRF, and the
//! four Fieldcraft arms (`fc-bm25`, `fc-dense`, `fc-hybrid`, `fc-served`).

use crate::data::{Corpus, Query};
use crate::guard::visible_prefix;
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use std::collections::HashMap;

/// Raw hits requested from a first-stage index before the time guard removes future lessons.
pub const OVERFETCH: usize = 1000;
/// Per-list depth fed into RRF (production fuses the top 80 of each list).
pub const FUSE_DEPTH: usize = 80;
/// Depth of every arm's final ranking; a cited lesson below it scores 0 for MRR.
pub const RANK_DEPTH: usize = 100;
pub const RRF_K: f32 = 60.0;

const STOP_WORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "are", "was", "have", "has", "had", "can",
    "will", "would", "could", "should", "not", "but", "its", "all", "any", "our", "your", "their",
    "also", "how", "does", "what", "when", "where", "which", "who", "why", "been", "being", "did",
    "does", "doing", "into", "more", "most", "other", "some", "such", "than", "them", "then",
    "these", "those", "very", "just", "about", "each", "use", "using", "used",
];

/// Production FTS query rule (`sparse_search_inner`): alphanumeric/underscore tokens of 3+ bytes,
/// lower-cased, stop words dropped, joined with OR. Empty when nothing survives.
pub fn fts_terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| t.len() >= 3)
        .map(str::to_lowercase)
        .filter(|t| !STOP_WORDS.contains(&t.as_str()))
        .collect()
}

pub fn fts_query(query: &str) -> String {
    fts_terms(query).join(" OR ")
}

/// Keep corpus indices visible at `at` (time guard), in order, then cut to `depth`.
/// `corpus.created` is sorted ascending, so visible lessons are exactly indices `< prefix`.
pub fn guard_and_trim(hits: &[usize], corpus: &Corpus, at: DateTime<Utc>, depth: usize) -> Vec<usize> {
    let prefix = visible_prefix(&corpus.created, at);
    hits.iter().copied().filter(|&i| i < prefix).take(depth).collect()
}

/// Reciprocal-rank fusion over ranked lists of corpus indices. Ties break by best original
/// rank, then index, so the result is deterministic.
pub fn rrf(lists: &[Vec<usize>], k: f32) -> Vec<usize> {
    rrf_weighted(lists, &vec![1.0; lists.len()], k)
}

/// `rrf` with a weight per list (weight 1.0 everywhere is plain RRF).
pub fn rrf_weighted(lists: &[Vec<usize>], weights: &[f32], k: f32) -> Vec<usize> {
    let mut score: HashMap<usize, (f32, usize)> = HashMap::new();
    for (list, w) in lists.iter().zip(weights) {
        for (rank, &i) in list.iter().enumerate() {
            let e = score.entry(i).or_insert((0.0, usize::MAX));
            e.0 += w / (k + rank as f32 + 1.0);
            e.1 = e.1.min(rank);
        }
    }
    let mut v: Vec<(usize, f32, usize)> = score.into_iter().map(|(i, (s, r))| (i, s, r)).collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.2.cmp(&b.2)).then(a.0.cmp(&b.0)));
    v.into_iter().map(|(i, _, _)| i).collect()
}

/// A set of row-major document vectors over the corpus order, with the rows a dense arm may rank.
pub struct VectorSet<'a> {
    pub dim: usize,
    pub vectors: &'a [f32],
    pub candidates: &'a [bool],
}

impl<'a> VectorSet<'a> {
    /// The stored Gemini vectors; candidates are the lessons that have one.
    pub fn gemini(corpus: &'a Corpus) -> Self {
        Self { dim: corpus.dim, vectors: &corpus.vectors, candidates: &corpus.has_vec }
    }

    fn row(&self, i: usize) -> &[f32] {
        &self.vectors[i * self.dim..(i + 1) * self.dim]
    }
}

/// Top `limit` visible lessons by cosine similarity (vectors are unit length, so a dot product).
pub fn dense_top(corpus: &Corpus, qvec: &[f32], at: DateTime<Utc>, limit: usize) -> Vec<usize> {
    dense_top_in(corpus, &VectorSet::gemini(corpus), qvec, at, limit)
}

/// `dense_top` over any vector set aligned to the corpus.
pub fn dense_top_in(corpus: &Corpus, set: &VectorSet, qvec: &[f32], at: DateTime<Utc>, limit: usize) -> Vec<usize> {
    let prefix = visible_prefix(&corpus.created, at);
    let mut scored: Vec<(f32, usize)> = (0..prefix)
        .filter(|&i| set.candidates[i])
        .map(|i| (cosine(qvec, set.row(i)), i))
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.truncate(limit);
    scored.into_iter().map(|(_, i)| i).collect()
}

/// Production `cosine_similarity` (guards zero norms and dimension mismatch).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) }
}

/// FTS5 `bm25` over the snapshot's `knowledge_fts` (read-only), best first, as corpus indices.
pub struct Fts<'a> {
    conn: &'a Connection,
    index_of: HashMap<&'a str, usize>,
}

impl<'a> Fts<'a> {
    pub fn new(conn: &'a Connection, corpus: &'a Corpus) -> Self {
        Self { conn, index_of: corpus.index_of() }
    }

    pub fn top(&self, query: &str, limit: usize) -> Result<Vec<usize>, String> {
        let q = fts_query(query);
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT id FROM knowledge_fts WHERE knowledge_fts MATCH ?1 ORDER BY bm25(knowledge_fts) LIMIT ?2",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params![q, limit as i64], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for id in rows {
            let id = id.map_err(|e| e.to_string())?;
            if let Some(&i) = self.index_of.get(id.as_str()) {
                out.push(i);
            }
        }
        Ok(out)
    }
}

/// Per-query rankings (lesson ids, best first) for the four Fieldcraft arms.
pub struct FcRanks {
    pub bm25: Vec<String>,
    pub dense: Vec<String>,
    pub hybrid: Vec<String>,
    pub served: Vec<String>,
    /// Visible lessons at query time (diagnostic).
    pub visible: usize,
    /// Unguarded first-stage lists, reused by the Gemma arms.
    pub lex_raw: Vec<usize>,
    pub dense_raw: Vec<usize>,
}

pub fn ids_of(corpus: &Corpus, idx: &[usize]) -> Vec<String> {
    idx.iter().map(|&i| corpus.ids[i].clone()).collect()
}

/// The hybrid recipe shared by `fc-hybrid` and `mv-hybrid`: guard each list, cut to the fuse
/// depth, RRF with k=60, cut to the rank depth.
pub fn hybrid(corpus: &Corpus, at: DateTime<Utc>, lex: &[usize], dense: &[usize]) -> Vec<usize> {
    hybrid_of(corpus, at, &[lex, dense])
}

/// The same recipe over any number of first-stage lists.
pub fn hybrid_of(corpus: &Corpus, at: DateTime<Utc>, raw_lists: &[&[usize]]) -> Vec<usize> {
    let lists: Vec<Vec<usize>> = raw_lists.iter().map(|l| guard_and_trim(l, corpus, at, FUSE_DEPTH)).collect();
    let mut fused = rrf(&lists, RRF_K);
    fused.truncate(RANK_DEPTH);
    fused
}

pub fn fc_ranks(corpus: &Corpus, fts: &Fts, q: &Query, qvec: &[f32]) -> Result<FcRanks, String> {
    let lex_raw = fts.top(&q.text, OVERFETCH)?;
    let dense_raw = dense_top(corpus, qvec, q.at, OVERFETCH);
    let bm25 = guard_and_trim(&lex_raw, corpus, q.at, RANK_DEPTH);
    let dense = guard_and_trim(&dense_raw, corpus, q.at, RANK_DEPTH);
    let hyb = hybrid(corpus, q.at, &lex_raw, &dense_raw);
    Ok(FcRanks {
        bm25: ids_of(corpus, &bm25),
        dense: ids_of(corpus, &dense),
        hybrid: ids_of(corpus, &hyb),
        served: q.served.clone(),
        visible: visible_prefix(&corpus.created, q.at),
        lex_raw,
        dense_raw,
    })
}

/// Rankings for the Gemma arms. Dense arms rank only lessons that also have a stored Gemini
/// vector (`gem.candidates`), so each arm sees the same candidate set as its Gemini twin.
pub struct GemmaRanks {
    pub dense: Vec<String>,
    pub hybrid: Vec<String>,
    /// Exploratory: BM25 + Gemini dense + Gemma dense.
    pub both: Vec<String>,
}

pub fn gemma_ranks(corpus: &Corpus, gem: &VectorSet, q: &Query, gemma_qvec: &[f32], fc: &FcRanks) -> GemmaRanks {
    let gemma_raw = dense_top_in(corpus, gem, gemma_qvec, q.at, OVERFETCH);
    let dense = guard_and_trim(&gemma_raw, corpus, q.at, RANK_DEPTH);
    GemmaRanks {
        dense: ids_of(corpus, &dense),
        hybrid: ids_of(corpus, &hybrid(corpus, q.at, &fc.lex_raw, &gemma_raw)),
        both: ids_of(corpus, &hybrid_of(corpus, q.at, &[&fc.lex_raw, &fc.dense_raw, &gemma_raw])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::parse_ts;

    fn corpus(times: &[&str]) -> Corpus {
        let n = times.len();
        Corpus {
            ids: (0..n).map(|i| format!("l{i}")).collect(),
            created: times.iter().map(|t| parse_ts(t).unwrap()).collect(),
            texts: vec![String::new(); n],
            vectors: vec![0.0; n * 2],
            has_vec: vec![true; n],
            dim: 2,
        }
    }

    #[test]
    fn guard_drops_future_lessons_before_trimming() {
        let c = corpus(&["2026-01-01T00:00:00+00:00", "2026-02-01T00:00:00+00:00", "2026-03-01T00:00:00+00:00"]);
        let at = parse_ts("2026-02-01T00:00:00+00:00").unwrap();
        // Future lesson 2 is the raw top hit; it must not survive, and depth counts survivors.
        assert_eq!(guard_and_trim(&[2, 1, 0], &c, at, 2), vec![1, 0]);
    }

    #[test]
    fn dense_search_never_returns_a_future_lesson() {
        let mut c = corpus(&["2026-01-01T00:00:00+00:00", "2026-03-01T00:00:00+00:00"]);
        c.vectors = vec![0.0, 1.0, 1.0, 0.0]; // lesson 1 (future) matches the query exactly
        let at = parse_ts("2026-02-01T00:00:00+00:00").unwrap();
        assert_eq!(dense_top(&c, &[1.0, 0.0], at, 10), vec![0]);
    }

    #[test]
    fn dense_arm_ranks_only_candidate_lessons() {
        let c = corpus(&["2026-01-01T00:00:00+00:00", "2026-01-02T00:00:00+00:00", "2026-01-03T00:00:00+00:00"]);
        // Row 1 is the exact match but is not a candidate (no Gemini vector): it must not appear.
        let vectors = [0.0, 1.0, 1.0, 0.0, 0.6, 0.8];
        let set = VectorSet { dim: 2, vectors: &vectors, candidates: &[true, false, true] };
        let at = parse_ts("2026-02-01T00:00:00+00:00").unwrap();
        assert_eq!(dense_top_in(&c, &set, &[1.0, 0.0], at, 10), vec![2, 0]);
    }

    #[test]
    fn hybrid_of_two_lists_equals_hybrid_and_three_lists_fuse() {
        let c = corpus(&["2026-01-01T00:00:00+00:00", "2026-01-02T00:00:00+00:00", "2026-01-03T00:00:00+00:00"]);
        let at = parse_ts("2026-02-01T00:00:00+00:00").unwrap();
        assert_eq!(hybrid(&c, at, &[0, 1], &[1, 2]), hybrid_of(&c, at, &[&[0, 1], &[1, 2]]));
        assert_eq!(hybrid_of(&c, at, &[&[0, 1], &[1, 2], &[1, 0]])[0], 1);
    }

    #[test]
    fn hybrid_never_returns_a_future_lesson() {
        let c = corpus(&["2026-01-01T00:00:00+00:00", "2026-01-02T00:00:00+00:00", "2026-03-01T00:00:00+00:00"]);
        let at = parse_ts("2026-02-01T00:00:00+00:00").unwrap();
        assert!(hybrid(&c, at, &[2, 0], &[2, 1]).iter().all(|&i| i < 2));
    }

    #[test]
    fn rrf_rewards_agreement_and_is_deterministic() {
        let fused = rrf(&[vec![5, 1, 2], vec![1, 9, 5]], 60.0);
        assert_eq!(fused[..2], [1, 5]);
        assert_eq!(fused, rrf(&[vec![5, 1, 2], vec![1, 9, 5]], 60.0));
    }

    #[test]
    fn fts_query_follows_production_rule() {
        assert_eq!(fts_query("How to fix the ab swift_ui-bug"), "fix OR swift_ui OR bug");
        assert_eq!(fts_query("the of"), "");
    }
}
