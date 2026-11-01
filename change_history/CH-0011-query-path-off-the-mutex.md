# CH-0011: Query path off the write mutex, in-memory record cache, deterministic RRF ties

**Date**: 2026-11-01
**Type**: implementation (refactor + one fix)
**Commits**: 857a453 (refactor), e354be6 (fix), 5df1833 (eval tool), plus 81dc1a7 / 396a568 (clippy and rustfmt on main, no code change)
**Plan**: [docs/plans/local-embeddings-and-query-load.md](../docs/plans/local-embeddings-and-query-load.md), stage 1

## What changed

- `GET /lessons` no longer runs under `AppState::repo`'s mutex. It embeds the query in
  `spawn_blocking` holding no database handle, then ranks on a pooled read-only connection
  (`readers::ReaderPool`, WAL readers, no migrations on open) in `spawn_blocking`. Only the
  retrieval-log insert and resurrection notes take the write mutex. `retrieve_from_repository` is
  split into `embed_query` and `retrieve_with_query_vector`.
- `fieldcraft_retrieval::record_cache` keeps, per active/provisional record, what the dense and
  legacy-identity scans decide on (tier, state, superseded, source origin, project, identity-cue
  flag, vector) in the old scan order (state, rowid). Dense search ranks from it and reads full rows
  only for the top 80; legacy identity admission (about 9,000 `field_notes` rows with no identity
  cue) filters it and reads text only for rows whose project matches; the L3 resurrection scan
  reads archived vectors from it instead of decoding every archive vector per query.
- Freshness: triggers append record ids to `knowledge_vector_changes` on every insert, delete and
  update of a cached column, and on identity `cue_index` rows. Each query applies the ids logged
  since the last one it saw; the merge loop prunes the log to 100,000 entries and a cache further
  behind rebuilds (about 2 s on the production copy).
- Fix: `rrf_fuse` broke score ties by `HashMap` iteration order, random per process. Two servers
  running the old binary on the same copy disagreed on 55 of 60 logged queries. Ties now break by
  id.
- `eval/load_bench` (Go): stub Gemini endpoint, concurrent load runner, A/B answer comparison.

## Why

Fieldcraft is becoming the per-turn memory of every running agent. Per query the server held its
single mutex while it called Gemini and decoded all ~27,000 active vectors with their text
(~285 ms of database work), so concurrent agents queued behind each other: at 16 concurrent, p50
was 8-10 s.

## Verification

- `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo fmt --all --check`: green.
- Equivalence on a copy of the production store (2026-11-01, 33,202 records), deterministic stub
  embedder, 500 logged queries, top 30: pre-refactor binary plus the RRF fix vs refactored binary
  plus the fix: 500/500 identical ids, order and scores (`load_bench compare`).
- Per-stage timing on the copy (single thread, release, Mac under load ~20): identity admission
  ~220 ms -> ~52 ms, whole retrieval ~300-460 ms -> ~100-190 ms CPU.
- Latency, stub embedder with a fixed 150 ms delay standing in for Gemini (simulated, not measured
  against Gemini), fresh copy per run, interleaved, this Mac (14 cores, shared with other work):
  see the table in the plan, section "Results".
