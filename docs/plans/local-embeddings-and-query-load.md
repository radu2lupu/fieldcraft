# Local embeddings and query-path load

## Status
- Owner: Radu
- Last updated: 2026-11-01
- Current phase: stages 1-4 implemented and measured on a copy; nothing deployed. Open: the
  Gemini-vs-Gemma replay (needs a Gemini key) before Gemma becomes the default; G3 at 16 concurrent.
  Deploy is a separate, owner-approved step (see "Deploy and rollback").

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

## Results (2026-11-01, copy of the production store, this laptop: 14 cores, shared with other work)

Latency, `eval/load_bench`, stub Gemini with a fixed 150 ms delay standing in for the API
(simulated, not Gemini), fresh copy per run, 100 requests at 1, 320 at 8 and 16, ms:

| Concurrent | Before (8cf5ba5) p50 / p95 | After (e8e544d) p50 / p95 | Throughput before → after |
|---|---|---|---|
| 1 | 462 / 660 | 245 / 377 | 1.9 → 3.5 q/s |
| 8 | 2,741 / 17,242 | 439 / 992 | 1.5 → 15.6 q/s |
| 16 | 10,078 / 18,939 | 1,347 / 2,809 | 1.5 → 12.2 q/s |

Gemma provider (real sidecar, Q8_0, b12032, Metal), final binary, Gemma-reindexed copy:
1: 118 / 245; 8: 634 / 773; 16: 1,425 / 1,750 (p50 / p95 ms), 0 errors. Fallback at 16: 0 of 320
(310 embedded in budget, 10 query-cache hits). With a provider forced to 1.5 s per call at 16: 225
of 320 fell back (200 deadline, 25 shed), 95 cache hits, p95 1,387 ms. Server RSS 193-318 MB;
sidecar footprint 222 MB, peak 372 MB.

Gates:

- G1 met: 500/500 identical (twice: stage-1 refactor, identity perf refactor), p95 at 16 down
  18.9 s → 2.8 s.
- G2 met: tag isolation and BM25-only tested; full Gemma reindex 32,946 records in 1,245 s, 0
  skipped, resumable; a server on the same file answered throughout with 0 errors.
- G3 not met as written (p95 <= 850 ms at 16): embedding is bounded (fallbacks above), but ranking
  CPU (FTS5 BM25 and identity admission, ~100-200 ms per query) at 16 concurrent on this shared
  laptop gives p95 1.75 s with Gemma. Not measured on the mini (M4, 10 cores).
- G4 partly: Gemma vs BM25-only measured (CH-0014); Gemma vs Gemini not measured (no key), so
  Gemma is not the default. Thresholds recalibrated (CH-0014).

## Deploy and rollback

For the owner to approve. Nothing below has been run on the Mac mini.

Order (each step is safe to stop after; the server keeps answering throughout):

1. **Back up** on the mini: `sqlite3 ~/.fieldcraft/memory.sqlite3 ".backup ~/.fieldcraft/backups/pre-embeddings-$(date +%Y%m%d).sqlite3"`
   (~500 MB; `scripts/install-macos-launchagent.sh` also takes one).
2. **Deploy the new binaries with the provider unchanged** (Gemini stays the default):
   `git pull --ff-only && scripts/install-macos-launchagent.sh` (or `fieldcraft update`). On first
   open the server migrates: creates `knowledge_vectors`, copies the 768-dim `dense_vector` blobs
   under `gemini:models/gemini-embedding-001:768` (one transaction, ~100 MB, seconds), adds the
   change-log triggers. `knowledge.dense_vector` is left untouched. Check: start-up line
   `embeddings: ready`, `curl -s localhost:7777/health`, `fieldcraft doctor` (server-embeddings ok),
   `/status` → `embedding.vectors_by_tag` shows the Gemini tag with ~31k rows.
3. **Start the sidecar**: put `llama-server` (llama.cpp b12032 macOS arm64, or any build >= b11476)
   and `embeddinggemma-2-Q8_0.gguf` (sha256 `2188ac1d…09135`) in `~/.fieldcraft/embedder/`,
   install `install/launchd/com.fieldcraft.embedder.plist` (replace `__HOME__`), bootstrap it.
   Check: `curl -s 127.0.0.1:7778/v1/models` lists `embeddinggemma-2-Q8_0`.
4. **Build Gemma vectors** while Gemini keeps serving:
   `FIELDCRAFT_EMBED_GEMMA_URL=http://127.0.0.1:7778 fieldcraft reindex --provider gemma`
   on the mini (local catalog). ~33k records; ~21 min on the laptop under load, mini speed not
   measured. Resumable: rerun after any interruption. Queries keep working (they read Gemini
   vectors; WAL readers are not blocked).
5. **Only if the stage-4 gate passes** (not yet: the Gemini arm needs a key, see Results):
   add `export FIELDCRAFT_EMBED_PROVIDER=gemma` to `~/.fieldcraft/secrets.env`, rerun step 4 to
   catch records written since, restart (`launchctl kickstart -k gui/$(id -u)/com.fieldcraft.server`).
   Check: start-up line names `gemma:embeddinggemma-2-Q8_0:768`; `/status` → `embedding.ready`
   true and `serving_missing_vector` ~0; `fieldcraft doctor`.
6. Merge, RIF, dream and resurrection use `GEMMA_THRESHOLDS` (calibrated in CH-0014) once the
   provider is gemma. The lifecycle loop keeps running with `FIELDCRAFT_LIFECYCLE_APPLY=1`; if you
   want to watch the first Gemma passes before they archive, set it to 0 for a day.

Rollback:

- From step 5: remove the provider line (or set `gemini`), restart. Gemini vectors were never
  modified; records written while on Gemma have Gemma vectors only, so run
  `fieldcraft reindex --provider gemini` to fill them.
- From step 2 (back to the old binary): reinstall the previous commit. The old binary reads
  `knowledge.dense_vector`, which still holds every vector that existed at migration time; records
  written after it have none there, and `fieldcraft reindex` (old binary, missing only) fills them.
  The new tables and triggers are inert for the old binary, except that the change-log triggers
  keep appending to `knowledge_vector_changes`; drop them if staying on the old binary:
  `DROP TRIGGER knowledge_vector_change_insert; … _delete; … _update; cue_index_identity_change_insert; … _delete; knowledge_vectors_change_insert; … _update; … _delete;`.
- Disk: both tags plus the change log grew the copy from 500 MB to 745 MB. Once Gemma serves and
  Gemini is retired, deleting the Gemini rows and the unused `dense_vector` column, then `VACUUM`,
  recovers it (separate, later step).

## Change log

- 2026-11-01: plan written. Stages 1-4 done (CH-0011..CH-0014). Added during stage 3: the
  identity-admission and SQLite-lock work (load profile showed it), RRF and identity tie fixes.

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
