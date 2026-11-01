# Local embeddings and query-path load

## Status
- Owner: Radu
- Last updated: 2026-11-01
- Current phase: implementation (stages 1-4 below), nothing deployed. Deploy is a separate,
  owner-approved step (see "Deploy and rollback").

## Problem

Fieldcraft is becoming the memory layer for every running agent, called every turn by many agents
at once. Today one `GET /lessons` does all of this while holding the server's single repository
mutex, on a tokio worker thread:

1. a blocking HTTPS call to the Gemini embedding API (query text leaves the machine);
2. a full scan of every active row with a vector (~26,700 rows on the 2026-11-01 production copy),
   decoding each row's text, payload and 3 KB vector, to rank them by cosine;
3. FTS5 BM25, cue enrichment, scoring, MMR.

So concurrent queries serialise behind one lock, a slow or failing Gemini call stalls everyone,
and a missing key silently degraded every query to BM25 for three weeks in September 2026. The
owner decided embeddings move to EmbeddingGemma 2 run locally: no embedding text leaves the
machine.

## Scenarios

- 16 agents query at once: each answer arrives within the deadline (default 800 ms).
- The embedder is slow, down or not configured: queries still answer (BM25), and `/status` and
  `fieldcraft doctor` say so explicitly.
- The owner switches the store from Gemini to Gemma vectors: `fieldcraft reindex` builds Gemma
  vectors while the server keeps serving Gemini ones; nothing is deleted; switch-over is a config
  change plus restart; rollback is the reverse.

## Scope and non-goals

In scope, in this order, one commit (plus a docs commit) per stage:

1. Refactor (behavior identical): embedding call outside the repository mutex; retrieval off the
   async workers on a read-connection pool; in-memory record-vector cache invalidated by triggers
   on write / supersede / archive.
2. Pluggable embedding providers with model tagging: `gemma` (default, local llama.cpp sidecar)
   and `gemini` (opt-in); vectors stored per `(record, tag)`; queries compare only same-tag
   vectors; explicit BM25-only mode surfaced in `/status` and `doctor`; resumable, batched,
   non-blocking `fieldcraft reindex` that keeps old vectors.
3. Query deadline on `GET /lessons` with BM25 + cached-query-vector fallback; fallback counters.
4. Quality gate: replay >= 300 logged queries, Gemini vs Gemma; recalibrate cosine thresholds.

Non-goals: deploying, reindexing or restarting anything on the production server; multimodal (see Direction below: stage 5, after G4)
embeddings; ANN indexes (exact cosine over ~30k x 768 stays well under 10 ms); changing scoring.

## Evidence

- Production copy (2026-11-01): 33,202 records; 28,934 active-tier (26,682 with vectors), 4,268
  archive; all vectors 768-dim Gemini; `retrieval_log` 22,833 rows; 5,801 confirmed outcomes;
  `knowledge_merge_queue` backlog 19,873 (merge loop drains 16/minute).
- Server code: `search_lessons_handler` runs `retrieve_from_repository` under `state.repo.lock()`
  on the async worker; `encode_dense` is called inside it. The resurrection scan also holds the
  lock and decodes every archive vector per query.
- Mac mini (server): Apple M4, 10 cores, 16 GB.

## Model and runtime decision

- Model: `google/embeddinggemma-2` (Google DeepMind, Apache 2.0). Text path 270M params (130M
  transformer + 140M embedder), 768-dim output (MRL 512/256/128), 8,192-token context, mean
  pooling. Prompts: query `task: search result | query: {q}`, document
  `title: {title|none} | text: {text}`. MTEB eng v2 68.46, multilingual v2 61.36, code v1 78.68.
  Sources: <https://huggingface.co/google/embeddinggemma-2>,
  <https://ai.google.dev/gemma/docs/embeddinggemma/model_card_2>.
- Smallest variant: the text-only Q8_0 GGUF from `ggml-org/embeddinggemma-2-GGUF`
  (`embeddinggemma-2-Q8_0.gguf`, 309,855,456 bytes); no vision/audio projector is loaded. The
  only other text file is BF16 (558 MB). No smaller official quant exists.
- Runtime: llama.cpp `llama-server --embeddings` as a local sidecar on 127.0.0.1 (release
  b12032 prebuilt for macOS arm64, Metal). Chosen over in-process bindings because it keeps
  C++/Metal out of the Rust build, isolates crashes, and matches what another local project
  already measured (Q8 vs fp32 cosine >= 0.9999; ~320 MB loaded). Older llama.cpp builds
  (< b11476) cannot load the `gemma-embedding2` architecture.

## Architecture

- `fieldcraft-core::embedding` owns the provider spec: provider (`gemma` | `gemini` | `none`),
  model, dims, the vector tag string, and per-tag cosine calibration. Everything else calls it.
- `fieldcraft-retrieval` owns encoding (`encode_queries`, `encode_documents`) per provider, the
  record-vector cache, and the query-vector LRU.
- `fieldcraft-storage` owns `knowledge_vectors(knowledge_id, tag, dims, vector, created_at)` and
  the trigger-fed `knowledge_vector_changes` log the cache reads to invalidate.
- Server: `GET /lessons` = embed (deadline-bounded, no lock) -> retrieve on a pooled read
  connection in `spawn_blocking` -> log under the write mutex. Resurrection reads archive vectors
  from the cache.

## Implementation stages

1. Refactor: split `retrieve_from_repository` into embed + `retrieve_with_query_vector`; read
   pool; vector cache + change-log triggers; resurrection via cache. Gate: identical outputs on
   replayed logged queries; tests green; latency table.
2. Providers + tagging: schema `knowledge_vectors` (migrates existing `dense_vector` rows as the
   Gemini tag, column left in place for rollback); all vector readers take the active tag;
   provider config; BM25-only mode; `/status` + `doctor`; resumable `reindex --provider`.
3. Deadline: `FIELDCRAFT_QUERY_DEADLINE_MS` (default 800); embed timeout -> BM25 (+ query LRU);
   the late embedding still fills the LRU; counters in `/status`.
4. Quality gate + calibration on the copy.

## Testing plan

- `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`, `cargo test --workspace`.
- Equivalence (stage 1): old vs new binary on the same copy, deterministic stub embedder, >= 500
  logged queries, byte-identical result ids and scores.
- Load: `eval/load_bench` (Go) fires N concurrent `GET /lessons` from logged queries at a server
  on the copy; p50/p95 at 1, 8, 16.
- Quality (stage 4): `eval/embed_compare` (Go) replays >= 300 logged queries against a Gemini-arm
  and a Gemma-arm server on copies; overlap@5, MRR@10 on cited lessons, bootstrap CI.

## Definition of done

- G1: stage-1 outputs identical on >= 500 replayed queries; p95 at 16 concurrent improves.
- G2: no vector comparison across tags (tested); `provider=none` reported by `/status` and
  `doctor` as BM25-only (tested); reindex resumable and queries answer while it runs (measured).
- G3: p95 <= deadline + 50 ms at 16 concurrent; fallback rate reported.
- G4: Gemma MRR@10 not below Gemini beyond the bootstrap 95% CI of the paired difference; else
  stop and report before making Gemma the default. Thresholds recalibrated and reported.
- Nothing on the production server changed.

## Deploy and rollback

Written at the end of the work into this section and the final report; owner approves.

## Change log

- 2026-11-01: plan written.

## Direction (owner, 2026-11-01): shared memory and artifact backend

Decision: EmbeddingGemma 2 is the primary embedding model for Fieldcraft and for any embedding work
done on or for it. Fieldcraft is to become the shared backend for every client that remembers
things, not only agents writing lessons: coding agents (read and write), the IWT browser (what the
user saves, what they care about, their patterns), the HiComputer app, and project artifacts and
prior work worth keeping, including images and audio.

Facts that make this one model, not a new one per modality (model page, read 2026-11-01): text,
code, image, video and audio embed into one 768-dim space with the same Matryoshka sizes (512, 256,
128) as today's Gemini vectors, so vector width and the `knowledge_vectors(record, tag)` layout do
not change. Footprints: text-only 270M, +image 440M, +audio 570M, all 740M; unused encoders can be
disabled at load. Costs: 280 tokens per image, 25 per second of audio (about 327 s max), 8,192-token
context shared across modalities. Task prompts apply to text only. Do not run in float16. MMEB v2
image 57.28, audio retrieval MSEB MRR@10 69.54; 128d hurts multimodal badly (59.01 to 45.65), so
keep 768 or 256. Source: <https://huggingface.co/google/embeddinggemma-2>.

Consequences, in order of when they bite:

- The text-only Q8 GGUF in stages 1-4 stays the first deliverable. Multimodal is stage 5, after
  G4 passes: load the vision/audio projectors only when the first non-text record is ingested, and
  report RAM per footprint on the 16 GB server.
- Records need a kind and origin for non-lesson content (artifact, bookmark, preference signal,
  transcript), a modality, and a reference to the blob. Media bytes stay outside SQLite (path or
  content hash); the store holds the text rendering, vector and metadata. One owner for these
  fields: `fieldcraft-core`.
- Junk is the main risk, not storage. 9,416 dictation rows already sit in the lesson corpus and
  surface for short chat prompts. Every source gets an admission rule and a tier at write time
  (what is kept, what is only logged), and retrieval filters by kind so a lesson query does not
  return a bookmark or a dictation. Preferences and patterns are derived views over events (saved,
  revisited, ignored), not hand-written records.
- Clients write through the same HTTP API with a source id; per-source quotas and a way to list
  and delete everything one source wrote.

Not decided here: the admission rules per source, the preference model, and retention. Each needs
its own short plan after stage 4.
