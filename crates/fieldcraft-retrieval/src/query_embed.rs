//! Query embedding under a time budget, for the server's `GET /lessons`.
//!
//! Many agents query every turn, so one slow or dead embedding provider must
//! not make every answer slow. [`embed_within`] waits at most `budget` for
//! the query vector; past that, retrieval runs BM25-only with the record
//! vectors already cached. The embedding call keeps running in the
//! background and stores its result in a small query-vector cache, so the
//! same query (hooks often repeat one) gets the dense path next time. At most
//! [`MAX_IN_FLIGHT`] embeddings run at once; beyond that a query skips
//! embedding immediately instead of queueing behind a stuck provider.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::retrieval::{QueryVector, try_embed_query_text};

const CACHE_CAPACITY: usize = 4096;
/// Time kept back from the deadline for ranking (measured ~100-200 ms per
/// query on the 2026-11 production copy).
pub const RETRIEVAL_RESERVE: Duration = Duration::from_millis(300);
const MAX_IN_FLIGHT: usize = 32;

/// How a query got (or did not get) its vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedOutcome {
    /// Embedded within the budget.
    Fresh,
    /// Served from the query-vector cache.
    Cached,
    /// Provider slower than the budget: BM25 fallback.
    TimedOut,
    /// Provider error: BM25 fallback.
    Failed,
    /// Too many embeddings in flight: BM25 fallback without trying.
    Shed,
    /// No provider configured: BM25-only by configuration.
    Bm25Mode,
}

impl EmbedOutcome {
    pub fn is_fallback(self) -> bool {
        matches!(self, Self::TimedOut | Self::Failed | Self::Shed)
    }
}

/// Counters since process start, for `/status`.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct QueryPathStats {
    pub queries: u64,
    pub embedded: u64,
    pub cache_hits: u64,
    pub fallback_timeout: u64,
    pub fallback_error: u64,
    pub fallback_shed: u64,
    pub bm25_mode: u64,
    pub late_embeddings_cached: u64,
}

static QUERIES: AtomicU64 = AtomicU64::new(0);
static EMBEDDED: AtomicU64 = AtomicU64::new(0);
static CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static FALLBACK_TIMEOUT: AtomicU64 = AtomicU64::new(0);
static FALLBACK_ERROR: AtomicU64 = AtomicU64::new(0);
static FALLBACK_SHED: AtomicU64 = AtomicU64::new(0);
static BM25_MODE: AtomicU64 = AtomicU64::new(0);
static LATE_CACHED: AtomicU64 = AtomicU64::new(0);
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

pub fn query_path_stats() -> QueryPathStats {
    QueryPathStats {
        queries: QUERIES.load(Ordering::Relaxed),
        embedded: EMBEDDED.load(Ordering::Relaxed),
        cache_hits: CACHE_HITS.load(Ordering::Relaxed),
        fallback_timeout: FALLBACK_TIMEOUT.load(Ordering::Relaxed),
        fallback_error: FALLBACK_ERROR.load(Ordering::Relaxed),
        fallback_shed: FALLBACK_SHED.load(Ordering::Relaxed),
        bm25_mode: BM25_MODE.load(Ordering::Relaxed),
        late_embeddings_cached: LATE_CACHED.load(Ordering::Relaxed),
    }
}

/// Bounded first-in-first-out map from query text to its vector. Entries
/// carry their tag; a hit under another tag is a miss.
#[derive(Default)]
struct QueryCache {
    map: HashMap<String, QueryVector>,
    order: VecDeque<String>,
}

impl QueryCache {
    fn get(&self, query: &str) -> Option<QueryVector> {
        let hit = self.map.get(query)?;
        let current = fieldcraft_core::embedding::active_tag();
        (hit.tag == current).then(|| hit.clone())
    }

    fn put(&mut self, query: String, vector: QueryVector) {
        if vector.is_empty() {
            return;
        }
        if self.map.insert(query.clone(), vector).is_none() {
            self.order.push_back(query);
            while self.order.len() > CACHE_CAPACITY {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
    }
}

fn cache() -> &'static Mutex<QueryCache> {
    static CACHE: OnceLock<Mutex<QueryCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(QueryCache::default()))
}

/// The query deadline for `GET /lessons` (`FIELDCRAFT_QUERY_DEADLINE_MS`,
/// default 800 ms) and the share of it embedding may use: what is left after
/// reserving `retrieval_reserve` for ranking, at least 50 ms.
pub fn embed_budget(retrieval_reserve: Duration) -> Duration {
    let deadline = std::env::var("FIELDCRAFT_QUERY_DEADLINE_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_millis(800));
    deadline
        .saturating_sub(retrieval_reserve)
        .max(Duration::from_millis(50))
}

/// Embed `query` (built with `build_query`) within `budget`. Never fails:
/// a missing vector means BM25-only retrieval, and the outcome says why.
pub async fn embed_within(query: String, budget: Duration) -> (QueryVector, EmbedOutcome) {
    embed_within_using(query, budget, |q: &str| try_embed_query_text(q)).await
}

async fn embed_within_using(
    query: String,
    budget: Duration,
    embed: impl FnOnce(&str) -> Result<QueryVector, String> + Send + 'static,
) -> (QueryVector, EmbedOutcome) {
    QUERIES.fetch_add(1, Ordering::Relaxed);
    if fieldcraft_core::embedding::active_tag().is_none() {
        BM25_MODE.fetch_add(1, Ordering::Relaxed);
        return (QueryVector::none(), EmbedOutcome::Bm25Mode);
    }
    if let Some(hit) = cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&query)
    {
        CACHE_HITS.fetch_add(1, Ordering::Relaxed);
        return (hit, EmbedOutcome::Cached);
    }
    if IN_FLIGHT.fetch_add(1, Ordering::AcqRel) >= MAX_IN_FLIGHT {
        IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
        FALLBACK_SHED.fetch_add(1, Ordering::Relaxed);
        return (QueryVector::none(), EmbedOutcome::Shed);
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let key = query.clone();
    tokio::task::spawn_blocking(move || {
        let result = embed(&query);
        IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
        let ok = result.is_ok();
        if let Ok(vector) = &result {
            cache()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .put(key, vector.clone());
        }
        // The receiver is gone when the caller stopped waiting.
        if tx.send(result).is_err() && ok {
            LATE_CACHED.fetch_add(1, Ordering::Relaxed);
        }
    });
    match tokio::time::timeout(budget, rx).await {
        Ok(Ok(Ok(vector))) => {
            EMBEDDED.fetch_add(1, Ordering::Relaxed);
            (vector, EmbedOutcome::Fresh)
        }
        Ok(Ok(Err(err))) => {
            eprintln!("WARNING: query embedding failed; BM25 fallback: {err}");
            FALLBACK_ERROR.fetch_add(1, Ordering::Relaxed);
            (QueryVector::none(), EmbedOutcome::Failed)
        }
        Ok(Err(_)) => {
            FALLBACK_ERROR.fetch_add(1, Ordering::Relaxed);
            (QueryVector::none(), EmbedOutcome::Failed)
        }
        Err(_) => {
            FALLBACK_TIMEOUT.fetch_add(1, Ordering::Relaxed);
            (QueryVector::none(), EmbedOutcome::TimedOut)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_is_bounded_and_tag_checked() {
        let mut cache = QueryCache::default();
        let tag = fieldcraft_core::embedding::active_tag();
        for i in 0..(CACHE_CAPACITY + 10) {
            cache.put(
                format!("q{i}"),
                QueryVector {
                    tag: tag.clone(),
                    values: vec![1.0],
                },
            );
        }
        assert_eq!(cache.map.len(), CACHE_CAPACITY);
        assert!(cache.get("q0").is_none(), "oldest entries are evicted");
        assert_eq!(
            cache.get(&format!("q{}", CACHE_CAPACITY + 9)).is_some(),
            tag.is_some()
        );
        cache.put(
            "other".into(),
            QueryVector {
                tag: Some("some-other-model:x:1".into()),
                values: vec![1.0],
            },
        );
        assert!(cache.get("other").is_none(), "another tag is a miss");
        cache.put("empty".into(), QueryVector::none());
        assert!(!cache.map.contains_key("empty"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn slow_provider_falls_back_within_budget_and_caches_late_result() {
        let Some(tag) = fieldcraft_core::embedding::active_tag() else {
            return; // BM25-only configured on this machine: nothing to time out.
        };
        let query = format!("deadline test {}", std::process::id());
        let slow_tag = tag.clone();
        let started = std::time::Instant::now();
        let (vector, outcome) =
            embed_within_using(query.clone(), Duration::from_millis(50), move |_q: &str| {
                std::thread::sleep(Duration::from_millis(400));
                Ok(QueryVector {
                    tag: Some(slow_tag),
                    values: vec![1.0, 0.0],
                })
            })
            .await;
        assert_eq!(outcome, EmbedOutcome::TimedOut);
        assert!(vector.is_empty());
        assert!(started.elapsed() < Duration::from_millis(300));

        tokio::time::sleep(Duration::from_millis(600)).await;
        let (vector, outcome) = embed_within_using(query, Duration::from_millis(50), |_q: &str| {
            Err("must not be called: cached".to_string())
        })
        .await;
        assert_eq!(outcome, EmbedOutcome::Cached);
        assert_eq!(vector.values, vec![1.0, 0.0]);

        let (_, outcome) = embed_within_using(
            "failing provider".into(),
            Duration::from_millis(500),
            |_q: &str| Err("down".to_string()),
        )
        .await;
        assert_eq!(outcome, EmbedOutcome::Failed);
    }

    #[test]
    fn budget_leaves_room_for_ranking() {
        assert_eq!(
            embed_budget(Duration::from_millis(300)),
            Duration::from_millis(500)
        );
        assert_eq!(
            embed_budget(Duration::from_secs(5)),
            Duration::from_millis(50)
        );
    }
}
