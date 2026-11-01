# CH-0013: Query deadline with BM25 fallback; identity admission off SQLite's global locks

**Date**: 2026-11-01
**Type**: implementation
**Commits**: 46bb717 (deadline), b3fce9e (fix), e8e544d (perf refactor)
**Plan**: [docs/plans/local-embeddings-and-query-load.md](../docs/plans/local-embeddings-and-query-load.md), stage 3

## What changed

- `fieldcraft_retrieval::query_embed::embed_within` gives query embedding a budget: the query
  deadline (`FIELDCRAFT_QUERY_DEADLINE_MS`, default 800 ms) minus 300 ms kept for ranking. Past it,
  or on a provider error, the query runs BM25 plus the cached record vectors; the embedding call keeps
  running and stores its vector in a 4,096-entry query-vector cache (tag-checked), so a repeated
  query takes the dense path next time. At most 32 embeddings run at once; beyond that a query skips
  embedding. `/status` → `embedding.query_path` counts embedded, cache hits and fallbacks by cause.
- `fieldcraft query` against a remote server has an HTTP timeout (`FIELDCRAFT_QUERY_TIMEOUT_MS`,
  default 5 s); it had none. The prompt hook keeps its 3 s fail-open budget.
- Fix: identity admission depended on SQLite's row order (a record matched through several alias
  rows took the first row's score; tied records survived truncation by row order). A record now
  scores by its best identity row and ties break by id.
- Perf (behavior-preserving): after CH-0011 the query path ran in parallel, and at 8 concurrent it
  was slower than the old serialized server. Sampling put almost all time in `identity_cue_search`'s
  SQL and on two process-wide SQLite locks. Its predicates now run once per distinct identity value
  (~3.2k) instead of per cue row (~57k), the query no longer joins `knowledge` (admissibility and the
  lowercased `title text` come from the record cache, which now holds that text and is invalidated on
  title/text updates), and SQLite's memory statistics are off (`SQLITE_CONFIG_MEMSTATUS=0`), which
  removes the global mutex every SQLite allocation took.

## Why

Many agents query every turn: a slow provider must cost recall on the affected queries, not latency
on all of them, and parallel readers only help if they do not serialize inside SQLite.

## Verification

- Equivalence: 500/500 logged queries identical (ids, order, scores) between b3fce9e and e8e544d.
- Unit test: a provider slower than the budget returns `TimedOut` within it, the late vector is
  then served from the cache, a failing provider returns `Failed`; cache bounded and tag-checked;
  the record cache's text follows title/text updates and tier changes.
- Latency and fallback numbers: plan, section Results.
