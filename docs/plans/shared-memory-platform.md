# Fieldcraft as the shared memory and artifact backend

## Status
- Owner: Radu
- Last updated: 2026-11-01
- Phase: planning. Nothing here is built. It extends
  [local-embeddings-and-query-load.md](local-embeddings-and-query-load.md). Another agent has since
  committed that plan's stages 1-4 (CH-0011 to CH-0014, main, up to 8d58772); its G4 (Gemma not below
  Gemini) was waived by the owner on 2026-11-04 ("Gemma is the one we keep"); it was never measured.
  Gemini stays the served provider until the owner approves the production reindex and switch.

## Problem

Fieldcraft stores coding lessons for agents. The owner's direction (2026-11-01) is wider: one
memory engine that coding agents, the I Want Things (IWT) browser, HiComputer and project
artifacts all read and write; that keeps what the user saves, cares about and keeps doing; that
holds images and audio too; that knows how concepts relate; that decides what may be shown where;
and that eventually indexes everything on a device the way Spotlight does, replacing IWT's own
embedding and search engine. Today it cannot do any of that safely: the relation and entity tables
are nearly empty, there is no visibility or origin field, and the retrieval profile is tuned for
curated lessons, not pages or files.

## Scenarios

- A coding agent and the browser both save something about the same project; each finds the
  other's item when relevant, and neither shows up in a lesson query unless asked for.
- The user shares their screen; nothing marked private, personal or sensitive appears in any
  suggestion, launcher result or lesson, whatever its rank.
- "What was that page about X I saved last week" and "what did we decide about X" are answered from
  one index, each from its own kind of record.
- Deleting a source (one browser profile, one app) removes everything it wrote and its vectors.
- If the mini is unreachable, IWT answers keyword-only from its own bounded index and says so;
  nothing is lost on reconnect. Nothing leaves a device for the mini unless the source is enabled.

## Scope and non-goals

In: the record model (kind, origin, modality, visibility), typed relations between records and
entities, display contexts and the access filter, one embedding contract shared with IWT, a
single-server deployment on the Mac mini with a measured capacity, and the IWT cutover path. Single owner: this is a private server for the owner and their own apps and agents over Tailscale;
multi-user tenancy is a non-goal, and sizing assumes at most about 16 concurrent clients. Out for now: a standalone Spotlight-replacement
app, general file crawling (the DigUp-style file layer, `lesson:d6690604`), ANN indexes before the
corpus needs them, and any iOS in-process engine (iOS keeps IWT's current path until measured).

## Current evidence

- Fieldcraft snapshot (2026-10-27): 32,234 lessons; `knowledge_relations` 10 rows, `mentions` 4,
  `people` 4, `projects` 4; `origin_class` is `unknown` on all rows; `protected` is 0 on all rows;
  9,416 `mem_*` rows from dictation sit in the lesson corpus and match short chat prompts.
- IWT (`/Users/radulupu/Code/iwantthings_ios`, branch `feat/iphone-duo`, mapped 2026-11-01): the
  embedder is already EmbeddingGemma 2 Q8 through llama.cpp b11476, a macOS subprocess worker
  (JSON lines over pipes, 30 s idle exit) and an in-process path on iOS, with an on-demand vision
  projector and measured parity (min cosine 0.9999 vs fp32). The store is `content.sqlite` with FTS5
  plus int8-compressed 768-dim passage and picture vectors, brute-force SIMD cosine, 10,000 documents
  and 1 GiB caps, RRF k=60 over lexical, dense and visual lists, dense floor 0.60 and window 0.06
  from the model profile, a content-hash dedupe, per-visit rows and encrypted cross-device sync that
  shares compatible vectors. Private windows are excluded entirely. The seam
  `protocol LocalTextEmbedding` isolates the embedder; the store, calibration and sync snapshot are
  tightly coupled to the vector layout. No code in IWT calls Fieldcraft; the Gate-3 curator that
  would write to it is fail-closed (`localChoirAvailable` is never true in production).
- IWT's `NOTICE.txt` promises that inference and search stay on the device and that history, page
  contents and queries reach no hosted service. A server-side Fieldcraft on the Mac mini would break
  that promise for page content.
- Model fact (page read 2026-11-01): EmbeddingGemma 2 embeds text, code, image, video and audio into
  one 768-dim space; text 270M, +image 440M, +audio 570M; 280 tokens per image, 25 per second of
  audio; never float16.
- Measured elsewhere: Fieldcraft's own hybrid retrieval beats Jev reranking and tree navigation on
  lessons (`eval/nav_compare_RESULTS.md`), so no judge-model layer is assumed anywhere below.

## Decisions (proposed; each needs the owner's yes before its stage starts)

1. One embedding contract, one owner. Model identity, prompts, dimensions, pooling, llama.cpp pin
   and cosine calibration live in `fieldcraft-core::embedding`. IWT's manifest becomes a consumer of
   that contract (today the pins differ: IWT b11476, Fieldcraft plan b12032). Swapping IWT's
   embedder buys nothing by itself: it is the same model. The gain is a shared store, shared
   retrieval and one place for admission and access.
2. One server. The Mac mini runs the single embedding and memory service for every client (agents,
   IWT, HiComputer, later apps); clients hold no model and no index of their own beyond what they
   need to degrade offline (IWT keeps keyword search if the mini is unreachable). Owner decision
   2026-11-01, replacing the earlier "local engine per device" proposal. Consequences to settle in
   stage 4: IWT's `NOTICE.txt` promise ("inference and search stay on this device") is rewritten to
   "on your own server" and gated by a per-source switch; every client authenticates with its own
   key and source id; the access filter runs server-side; private-window content is still never
   sent. Capacity is unproven until the benchmark in "Capacity" below reports.
3. Records, not pages, are the unit that crosses into Fieldcraft. Whole-page text stays in IWT's
   bounded local index at first; saves, likes, interests and events (what the user acted on) are
   written to Fieldcraft with a source id. This also matches IWT's existing rule that saving a page
   must not directly create active memory without a curator.
4. A graph as a layer, not an engine: records stay atomic; typed edges (part_of, about,
   derived_from, supersedes, related_to with a stated reason, plus confidence and source) go in the
   existing `knowledge_relations`, entities in `mentions` and the entity tables, seeded from the
   347k canonical cue rows. No graph database (graphrag-rs was rejected for dissolving atomic,
   feedback-weighted lessons).
5. Access is decided before ranking. Every record carries a visibility label set at write time
   (private, project, shareable) and an origin; a caller passes a display context (for example
   shared screen, demo, agent-only) and the filter drops what that context may not show before any
   scoring. Default is hide for every non-lesson kind. Enforced by a test, not an instruction.
6. Retrieval profiles per kind: lessons keep the current scoring; pages and files get their own
   profile (per-query z-score fusion and keyword coverage, never embed numeric-heavy text,
   per `lesson:d6690604`), chosen by the kind filter.

## Capacity (measured on the Mac mini 2026-11-01; eval/embed_capacity/RESULTS.md)

Mini: Apple M4, 10 cores, 16 GB, shared with other services (probe 2026-11-01: 77% memory free,
load 1.5, fieldcraft-server 180 MB RSS, no llama.cpp or GGUF installed, 74 GB disk free). Open:
resident memory per footprint (text 270M, +image 440M, +audio 570M), passages per second at 1, 4,
8 and 16 concurrent clients, query-embedding latency under that load, time to re-embed 33k lessons,
image embed time, whether audio runs in llama.cpp at all, and the effect on `GET /lessons` latency
while embedding runs. Gate: query-embedding p95 within the deadline at 16 concurrent clients while
a bulk re-embed runs at lower priority, and memory pressure stays normal.

Result: the mini is enough for text. llama.cpp b12032, `-np 4 -c 2560 -b 1024 -ub 1024 -t 4`: 485 MB
resident, about 57 passages/s (GPU-bound, 91% Metal), re-embedding 33,000 lessons about 9.6 min.
Queries alone p50 12 ms; with bulk running on the same server p95 203 ms at 1 client and 319 ms at 16
(inside the 800 ms deadline). A second query-only server does not help beyond about 4 clients (shared
GPU), so run one server and cap bulk jobs at 2-4 concurrent. Image route works (RSS 1.09 GB with the
projector, p50 297 ms, p95 751 ms); audio works but is experimental in this build (30 s clip 569 ms).
The projector cannot unload alone; `--sleep-idle-seconds` frees model and projector together (1,091 to
276 MB, first request after sleep 0.55 s), so image and audio run in a separate on-demand server.
Not measured: embedding quality for any modality, load above 64 clients, bulk concurrency 2-4.

## Requirements

- Privacy: private-window and incognito content is never written; a per-source switch controls
  whether anything leaves the device; vectors are as sensitive as text and are encrypted at rest and
  in transit wherever the text is.
- Deletion: delete by source, by record and by time range removes text, vectors, edges and caches.
- Junk control: per-source admission rule and tier at write time; per-source quota; a way to list
  what a source wrote.
- Performance: query p95 within the deadline set in the embeddings plan; brute-force scan stays
  below the ceiling measured on the device (re-measure at 100k vectors before adding ANN).
- Compatibility: vector tags per `(record, model)`; vector layout shared with IWT sync or an
  explicit one-time re-embed.

## Architecture

- `fieldcraft-core`: record model (kind, origin, modality, visibility), the embedding contract, the
  display-context type, the access filter. One owner for each.
- `fieldcraft-storage`: records, vectors by tag, edges, entities; no policy logic.
- `fieldcraft-retrieval`: encode, per-kind profiles, graph-aware expansion; calls the access filter
  first.
- One service: the existing server binary on the Mac mini; every client reaches it over Tailscale
  with its own key and source id; one llama.cpp sidecar next to it loads the vision and audio
  projectors on demand.
- IWT keeps capture, UI and its encrypted sync until a cutover gate passes; it calls Fieldcraft
  through the existing `LocalTextEmbedding` seam plus a new store protocol.

## Implementation stages

0. Prerequisite: embeddings plan stages 1-4 (G4, Gemma vs Gemini, waived by the owner 2026-11-04).
1. Record model and access filter (refactor-safe): add kind, origin, visibility, modality; backfill
   existing rows (`lesson`, origin from `source`, dictation rows tiered out); filter before ranking;
   test that a shared-screen context returns no non-lesson, no private record.
2. Typed edges and entities: populate from cue and mention data; edge-aware expansion behind a
   flag; measure on the 865-query set that expansion does not lower hit@1 (G: no loss beyond CI).
3. Embedding contract moved to `fieldcraft-core`; IWT manifest reads it; one llama.cpp pin; parity
   check against IWT's fp32 fixtures.
4. Embedding sidecar and per-client keys on the mini; IWT writes saves and events with a source id; admission rules and
   per-source deletion.
5. Multimodal records (images first): on-demand projector, RAM measured on the 16 GB server and a
   laptop; text rendering stored with the vector.
6. IWT cutover: IWT queries Fieldcraft for saved-item and event search; gate on IWT's own Cmd-T
   queries (build a 100-query labeled set first; none exists today) and on RAM and latency parity.
7. Device-wide index (Spotlight-style): a separate plan once stages 1-6 hold. The launcher is an
   AppKit shell that copies nanolight's behavior (see "Launcher UI baseline" below).

## Launcher UI baseline (owner, 2026-11-04)

Decision: `justrach/nanolight` (MIT, Zig 0.17) is the behavioral reference for the launcher; the UI is
built in AppKit (`NSPanel`, `NSTextField`, `NSTableView`), not forked. Reasons: drawing cost is not the
bottleneck (at most 16 rows; the server round trip dominates); a custom Metal/CoreText UI must
reimplement input methods, VoiceOver, selection and copy, drag and drop of results, Quick Look and
system appearance; and nanolight sits on a one-day-old author framework (nanoui) plus a Zig toolchain.
Not measured: no benchmark of either approach exists.

- Port as behavior: Carbon global hotkey (no Accessibility permission), floating panel over
  `NSVisualEffectView`, menu-bar item, login item (`SMAppService`), first-run welcome, the tiered
  fuzzy matcher with typo tolerance (whole name, prefix, word start, initials, substring, subsequence,
  edit distance 1 for 4-6 letters and 2 for 7+), launch-count ranking boost, debounced async results.
- Add: a result kind per record kind (file, save, lesson, artifact) from the Fieldcraft server; the
  display context from the access filter goes with every query; app list refreshed on change (nanolight
  builds it once at launch).
- Leave out: Google suggest egress, Google search in a WKWebView, the "sync across Macs through a
  Codegraff account" feature, the Python tools. Nothing typed leaves the owner's own server.
- Built on the Fieldcraft side: file crawler with FSEvents, extractors, `file` record kind, Full Disk
  Access handling, exclusion rules.
- Read at v0.1.1 (b38d802): 23 unit blocks (not run); performance claims are the author's; diffs for
  v0.1.2-v0.1.5 and the nanoui source were not read.

## Testing plan

- Unit: access filter (every kind x every context), deletion by source removes vectors and edges,
  vector tags never compared across models, admission rules.
- Equivalence: lessons retrieval identical before and after stage 1 on replayed logged queries.
- Quality: 865-query lesson set for stage 2; a new labeled set of at least 100 IWT queries for
  stage 6.
- Privacy: egress test that confidential canaries from private windows and private records never
  appear in any outbound request, any result in a restricted context, or any log.
- Ops: RAM and latency on the Mac mini, a laptop and (before any iOS work) a physical iPhone.

## Review plan

Adversarial angles: wrong problem (is a shared engine needed or only a shared embedder), regression
(IWT Cmd-T quality and RAM), moved cost (re-embedding, server load from many writers), complexity
(graph and access layers beyond what a scenario needs), evidence (no IWT quality set exists yet).

## Definition of done

- G1: access filter tests pass; a shared-screen context returns zero private or non-lesson records
  on the replay set.
- G2: edge expansion changes hit@1 by no more than the bootstrap CI on the 865-query set, or is left
  off.
- G3: one embedding contract file read by both repos; one pinned runtime.
- G4: IWT saved-item search through Fieldcraft is not worse than today's on the 100-query set, with
  p95 latency and RAM within IWT's stated targets.
- G5: deleting a source removes its text, vectors and edges (tested).
- Nothing on the production server or any user device changed before the owner approves each
  deploy.

## Open questions for the owner

- Which sources may ever leave the device, and which kinds default to hidden in a shared-screen
  context.
- Whether IWT's own cross-device sync stays once the mini holds the index.
- Whether iOS gets the engine in-process later or stays on IWT's own store.

## Change log

- 2026-11-01: plan written from the owner's direction and the IWT stack map.
- 2026-11-01: decision 2 changed from per-device engines to one server on the Mac mini; capacity
  section added.
- 2026-11-02: launcher UI baseline set to nanolight (fork as shell, Fieldcraft behind it).
- 2026-11-04: launcher decision changed: nanolight is the behavioral reference, UI built in AppKit.
- 2026-11-04: G4 waived by the owner (Gemma is kept); Gemma vs Gemini never measured.
- 2026-11-04: item indexing for My Things, mail and HiComputer documents split out into personal-index.md.
