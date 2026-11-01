# CH-0012: Pluggable embedding providers, model-tagged vectors, explicit BM25-only mode

**Date**: 2026-11-01
**Type**: implementation
**Commit**: bdf6502
**Plan**: [docs/plans/local-embeddings-and-query-load.md](../docs/plans/local-embeddings-and-query-load.md), stage 2

## What changed

- `fieldcraft-core::embedding` owns the provider spec: `gemma` (EmbeddingGemma 2 on a local
  llama.cpp `llama-server --embeddings`), `gemini` (API, still the default), `none` (BM25-only);
  model, dims, the vector tag `provider:model:dims`, and per-tag cosine thresholds. Configured by
  `FIELDCRAFT_EMBED_PROVIDER` and friends, from the environment or `secrets.env`.
- Vectors moved to `knowledge_vectors(knowledge_id, tag, dims, vector, created_at)`, one row per
  record and model. On first open a migration copies `knowledge.dense_vector` under the Gemini tag
  (768-dim) or `legacy:unknown:<dims>`; the old column stays for rollback and is no longer used.
  Deleting or re-inserting a record drops its vectors.
- Every vector read uses the connection's tag. `QueryVector` carries its tag, and a query whose
  tag differs from the store's runs BM25-only. The record cache is per (catalog, tag).
- Gemma client (`fieldcraft-retrieval/src/gemma.rs`): model-card prompts, `/v1/models` alias check
  before first use, shrink-and-retry for texts longer than one 2,048-token slot, normalization.
- `/status` gains an `embedding` block; the server refuses to start on an invalid provider and logs
  BM25-only or not-ready loudly; `fieldcraft doctor` reports the provider, makes a live call in
  local mode and reads the server's block in remote mode.
- `fieldcraft reindex --provider gemma|gemini [--limit N]` embeds records missing a vector under
  that tag (serving records first), one short write transaction per batch: resumable, and readers
  are never blocked. Other tags are untouched; `--all` re-embeds the selected records.

## Why

The owner chose local EmbeddingGemma 2 so no lesson or query text leaves the machine. Vectors of
two models are not comparable, so each vector needs its model recorded, and the switch must keep
the old vectors until the new ones exist. In September 2026 a missing key silently degraded
retrieval to BM25 for three weeks; BM25-only must now be a stated mode.

## Verification

- Tests: tag isolation (storage and retrieval), migration of untagged vectors (once, both tag
  kinds), BM25-only (no reads, writes refused), resumable missing-vector order, Gemma client
  against a stand-in server (alias check, prompts, normalization), `/status` embedding block.
- On a copy of the production store: the migration tagged 30,950 Gemini vectors; `fieldcraft
  reindex --provider gemma` embedded 32,946 records in 1,245 s (~26/s, this Mac under load, sidecar
  b12032 Metal), 0 skipped; Gemini vectors untouched. A server on the same file kept answering
  during the reindex with 0 errors (latency in the plan's Results).
- Model file checksum matches Hugging Face (`2188ac1d…`). Database grew from 500 MB to 745 MB with
  both tags present (WAL not yet checkpointed).
