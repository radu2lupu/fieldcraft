# CH-0015: Items store per source, batch ingest, source deletion and item search

**Date**: 2026-11-16
**Type**: implementation
**Commits**: 5254cd7 (refactor: shared vector-to-bytes helper), 8ce6a04 (feature)
**Plan**: [docs/plans/personal-index.md](../docs/plans/personal-index.md), stage 1 and the items part of stage 3

## What changed

- Items, a second record family next to lessons, for things apps push (first: I Want Things saved
  items and visited-page content). One SQLite file per source, `items-<source>.sqlite3` next to
  `memory.sqlite3`, with items keyed by `(source, external_id)`, passages, FTS5 over passages and
  passage vectors per model tag (`fieldcraft_storage::items`).
- Passages use IWT's rule (`LocalSearchContent.passages`: 160-character title line, 800 body
  characters, 160 overlap, first 16,000 body characters), so a vector made here and one made by IWT
  for the same text and model are comparable.
- `POST /items:batch`: ordered upserts and deletes in one transaction; per-op status `created`,
  `updated` (title or body changed: passages re-cut, vectors dropped), `metadata_updated` (vectors
  kept), `unchanged`, `deleted`, `not_found`, `error`; stores the connector's cursor and returns it
  with the source's change sequence.
- `DELETE /sources/{source}`: deletes every item, passage, FTS row and vector, closes the writer and
  removes the file (and its WAL and shared-memory files).
- `GET /search?q=&source=&kinds=&limit=`: BM25 over passages and dense cosine over passage vectors
  of the query's tag, fused with RRF k=60, plus a recency bonus of at most a tenth of a first place
  (half-life 180 days). Each result carries source, external_id, kind, title, url, snippet, score,
  timestamps and its rank in each list.
- `CosineThresholds.item_dense_min`: a passage must reach it to be a dense candidate. Gemma 0.6
  (IWT's calibrated `denseFloor` for embeddinggemma2 with the same prompts); Gemini 0.0 (not
  calibrated).
- A background indexer per source embeds pending passages after each batch and at server start;
  nothing in BM25-only mode, and a provider failure leaves passages pending, not the batch failed.

## Why

IWT's local index is full (10,000-document cap) and runs its own embedding worker; the owner
decided Fieldcraft becomes the store and index for things (plan, 2026-11-04). This slice lets IWT's
Command-T search through Fieldcraft before any cutover.

## Not changed

Lesson storage, `GET /lessons` ranking and lesson maintenance. Items never enter `knowledge`, and
no cruft, dream or forgetting path opens an item file. Not built: blobs (attachments, images), tokens bound to a source, ACL and
display contexts (stage 2), lessons in `GET /search`, the things API (stage 4), CLI commands (the CLI
does not mirror the server's non-lesson endpoints).

## Verification

- `cargo test --workspace`: 508 passed, 0 failed, 1 ignored (16 new: 7 storage, 6 retrieval, 3
  server). Covered: same batch twice gives identical rows and statuses `unchanged`; a content update
  re-cuts passages and drops vectors while a metadata update keeps them; deleting an item removes
  its passages, vectors and FTS rows; `DELETE /sources/iwt` removes that file and leaves the other
  source and the lessons untouched; keyword search with no embedder; dense + BM25 fusion with a
  deterministic embedder; the Gemma dense floor; vectors of another tag ignored.
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check` clean.
- End to end on this Mac with the real server and a local EmbeddingGemma 2 sidecar (Q8_0): batch
  ingest, re-ingest unchanged, vectors stored under `gemma:embeddinggemma-2-Q8_0:768`, hybrid
  search ("coffee machine" finds the espresso post), provider down at ingest leaves passages
  pending and the restarted server embeds them, `DELETE /sources/iwt` removes the files, `GET
  /lessons` returns no items.

## Vector dimension (plan stage 0 question)

There is no mismatch. IWT's search model is `embeddinggemma2` with `"dimensions": 768`
(`iwantthings_ios/EmbeddingRuntime/Resources/models.json`: `searchModel` line 13, profile line 54,
`dimensions` line 62), the same as Fieldcraft's default (`FIELDCRAFT_EMBED_DIM` 768). IWT stores a passage as a
4-byte float norm plus one int8 per dimension (`BrowserCore/LocalSearchPolicy.swift:122-131`), so
772 bytes at 768 dimensions. The "1024d, 1028 bytes" text is the doc comment at
`LocalSearchPolicy.swift:119-120` and the test vector at
`IWantThingsMacTests/LocalSemanticSearchTests.swift:173-178`, written for the earlier 1024-dimension
Octen profile still listed in models.json. Prompts also match (`task: search result | query: `,
`title: none | text: `). Fieldcraft stores float32 vectors, not IWT's int8 form.
