# Fieldcraft as the personal nexus: the things backend, the index and the launcher

## Status

- Owner: Radu
- Last updated: 2026-11-16
- Current phase: stage 1 (item model, per-source files, `POST /items:batch`, `DELETE /sources/<id>`)
  and item search (`GET /search`, the items part of stage 3) built on lane
  `t/d970ce23-fieldcraft-things-index` (CH-0015) on main, not on the local feat branches below; not
  deployed. Not built: blobs (stage 1), stage 2 (source-bound
  tokens, ACL, display contexts), lessons in `GET /search`, stages 4-8. The vector dimension
  question of stage 0 is settled (both 768, see Current Evidence). Owner direction 2026-11-04: Fieldcraft is the
  storage backend for IWT things (IWT moves its things stack onto it), the nexus that collects all
  content, and gets its own Spotlight-style launcher. One decision remains open (offline on iPhone).
- Extends [shared-memory-platform.md](shared-memory-platform.md) (one server on the Mac mini,
  EmbeddingGemma 2, access filter, graph). Builds on the local branches `feat/forgetting`,
  `feat/author-provenance` and `feat/memory-scope` (clone `/Volumes/SSD 1/fieldcraft-merge`). Move
  this file into that line when the branches are consolidated.

## Problem

Fieldcraft today stores agent lessons. Its purpose, per the owner, is to return the relevant, important
piece of information to the user or to an agent, on whatever surface they are working, for the question
they ask: one nexus where all content is collected. That means it is the store (not a copy) of IWT's
My Things (the store behind the Command-T gallery and the iOS tab), the index of mail from the email
client being built into IWT and of every document HiComputer agents create, and the engine behind a
Fieldcraft launcher that replaces Spotlight. Today each of those lives in its own store with its own
search, the IWT index is full (10,000-document cap, about 1,895 My Things items not indexed on this
Mac), and nothing calls Fieldcraft.

## Scenarios

1. In IWT, Command-T finds a saved X post, a YouTube like and an email about the same product in one
   result list, on Mac and iPhone.
2. A HiComputer agent asks "what did we decide about the pricing deck" and gets the deck, the email
   thread that led to it and the lesson an agent wrote afterwards, limited to what that agent may see.
3. Deleting a mail account, an X bookmark or a HiComputer room removes its items, chunks, vectors and
   edges from the index within one sync cycle.
4. With the screen shared, search shows only items marked safe for display.
5. Coding agents keep getting lessons exactly as today; things never appear as lessons.
6. From any app, the Fieldcraft launcher hotkey finds an app, a saved thing, an email, an agent
   document or a lesson in one panel, previews it and opens it in its own app.

## Scope

In: a source-owned item model next to lessons, a connector ingestion API, chunking and embedding of
items, per-source deletion, access from the source's own rules, one search API across kinds, the IWT,
mail and HiComputer connectors, offline behaviour on iPhone.
Later: images and audio (stage 5 of the platform plan), the Spotlight-style device index, graph
expansion in ranking.

## Non-Goals

- Turning items into lessons automatically. Learning from things stays curated: IWT's rule that only
  the curator may promote ("no direct active Fieldcraft writes") stands.
- Replacing IWT's capture code or UI, the mail client's fetching, or the HiComputer relay. IWT keeps
  capture and the gallery UI and writes My Things to Fieldcraft, which owns them. Mail and HiComputer
  documents stay owned by their apps; Fieldcraft holds indexed copies they push.
- Hosting anything outside the owner's own machines.

## Current Evidence

From a read of both repos on 2026-11-04 (paths are in those repos):

- **My Things.** `SavedItem` records, URL-centred, kinds save, bookmark, star, like, cart, listen,
  visit, open; identity is site plus object id (`BrowserCore/SavedItemKind.swift`). Store
  `saved-items.sqlite3` with FTS5; 3,760 items on this Mac (like 2,307, bookmark 971, save 459).
  Semantic index `content.sqlite`: EmbeddingGemma 2 Q8 via llama.cpp, int8 vectors, 800-character
  passages with 160 overlap, RRF k=60, capped at 10,000 documents and about 1 GiB, and at the cap.
  Cross-device sync is end-to-end encrypted through a Cloudflare Worker that sees only ciphertext.
  Private windows are never captured or synced.
- **Mail.** `ROADMAP/mail-plan.md`: Gmail (REST, `gmail.readonly`) and iCloud/IMAP, local
  `mail.sqlite` with FTS5, stages 0-1 built against a fake Gmail; planned `MailIndexer` feeding the
  local index with `mail://<account>/<message-id>`. Its privacy rule: mail bodies stay on the device.
  Whether Fieldcraft receives mail is listed there as an open decision.
- **HiComputer.** Stable agent ids (`agent_<hex>`, members `member_agent_<hex>`, every agent has an
  owner). Agents create HTML documents (document, deck, design) with versions and comments, to-dos and
  delivery tasks with artifacts, messages, attachments up to 64 MiB, and files in per-agent nest
  folders. All in the Go relay's SQLite with its own FTS5, scoped to rooms the caller can read;
  non-members get 404. Its store-authority table gives lessons to Fieldcraft and browser memory to
  IWT. No product code calls Fieldcraft.
- **Promises that conflict with a central index.** IWT's `EmbeddingRuntime/NOTICE.txt`: "Model
  inference and search stay on this device. These components receive no history, page contents or
  queries through a hosted embedding service." The semantic-search plan: private readable content,
  embeddings and queries must never reach a backend. The mail plan: bodies stay on the device.
- **Capacity on the mini** (eval/embed_capacity/RESULTS.md): about 57 passages per second for
  EmbeddingGemma 2 text, 485 MB resident, query p95 319 ms at 16 clients during bulk embedding.
- **Dimension: no mismatch (checked 2026-11-16).** IWT's search model `embeddinggemma2` is 768-dimensional
  (`EmbeddingRuntime/Resources/models.json` lines 13, 54, 62), as is Fieldcraft's Gemma provider, with
  the same query and document prompts. IWT stores 4 bytes of norm plus one int8 per dimension (772
  bytes); the "1024d, 1028 bytes" text (`BrowserCore/LocalSearchPolicy.swift:119-120`, test at
  `IWantThingsMacTests/LocalSemanticSearchTests.swift:173-178`) dates from the 1024-dimension Octen
  profile. Fieldcraft keeps float32 vectors.

## Requirements

- **Decided 2026-11-04 (owner): content may live on the owner's own server.** All content is collected
  on the Mac mini over Tailscale. IWT's NOTICE ("search stays on this device") and the mail plan's
  "bodies stay on the device" are reworded to "your devices and your own server" when stage 4 ships;
  no third party ever sees content, queries or vectors. Private windows are still never captured.
  Per-source switches stay (a mail account can be headers-only), default on.
- **Owner decision, offline on iPhone (open).** The mini is not always reachable, and IWT now writes
  My Things to it. Recommended: the IWT client keeps a local cache of recent items and an outbox for
  writes made offline (replayed on reconnect, idempotent by `(source, external_id)`), with basic
  local text search over the cache; full search needs the mini. Alternative: keep IWT's whole local
  index as a fallback, which keeps two engines alive.
- **Owner decision, launcher home (open).** Recommended: a separate private repo
  (`fieldcraft-launcher`, AppKit, Swift), because the Fieldcraft repo is public and Rust-only.
- Every item has `(source, external_id)` and writes are idempotent upserts. For My Things Fieldcraft is
  the authority: IWT creates, updates, lists and deletes through the API, and the server is what other
  devices sync from. For mail and HiComputer the app is the authority and pushes changes; deleting
  there deletes text, chunks, vectors, edges and blobs here (tested).
- Access is decided before ranking, from the source's rules: HiComputer room membership, mail account
  owner, IWT private windows never present, display contexts from the platform plan, private/shared
  scope from `feat/memory-scope`.
- Lessons keep their lifecycle (outcomes, forgetting, dream). Items do not: cruft sweeps and dream
  never touch items; items leave only when the source deletes them or the owner forgets a source.
- Ranking and quality of lesson retrieval unchanged (replay of the 865 logged queries, no loss
  beyond the bootstrap CI).
- No new SwiftUI in any client; AppKit and UIKit only.

## Architecture

**Two record families, one engine.**

- *Memories* (today's `knowledge`): lessons, directives, dictation tail, agent notes. Learned,
  outcome-tracked, forgettable.
- *Items* (new): things. My Things rows are owned here; mail and HiComputer rows are copies their
  apps push. Table `items(id, source, external_id, kind,
  title, body, url, author/sender, people, created_at, updated_at, source_updated_at, scope,
  acl_json, display_safe, parent_id, thread_id, blob_refs, content_hash)`. `chunks(item_id, ord,
  text, vector by model tag)`. Blobs (attachments, images) content-addressed on disk next to the
  database.

**Storage split by family.** One SQLite file per family, as the dictation move already does:
`memory.sqlite3` (lessons), `items-iwt.sqlite3`, `items-mail.sqlite3`, `items-hc.sqlite3`. Bulk mail
ingest then cannot stall lesson writes behind one WAL, and forgetting a whole source is dropping its
file. The query layer opens all of them read-only and merges.

**Connectors push; Fieldcraft never crawls.** `POST /items:batch` (upsert or delete, with a per-source
cursor) and `DELETE /sources/<id>`. Each app runs its connector:
- IWT: the `SavedItemStore` change feed (and later history), never private windows.
- Mail: the `MailIndexer` already planned in IWT, pointed at Fieldcraft instead of the local index.
- HiComputer: the relay emits document, version, to-do, message and membership changes; room
  membership becomes the item's `acl_json`, and membership changes update it.
Tokens are bound to an app and, where relevant, an agent (`feat/author-provenance`), so the server
knows which source a batch may write.

**Embedding.** One sidecar on the mini (EmbeddingGemma 2, one dimension after the mismatch above is
settled), a background queue ordered lessons first, then recent items, then backfill. Chunking shared
with IWT's rule (800 characters, 160 overlap) so vectors are comparable.

**Search.** One `GET /search?q=&kinds=&context=` returning results grouped by kind, each with its
retrieval profile (lessons keep today's pipeline; items use BM25 plus dense over chunks with RRF and
recency). `GET /lessons` stays as it is for agents.

**Things API (IWT moves its things stack here).** Beyond batch ingest, IWT needs what its
`SavedItemStore` does today: `PUT /things/<source>/<external_id>` (save, with engagements),
`DELETE` (unsave), `GET /things?kind=&site=&before=` (paged gallery, newest first), `GET
/things/changes?cursor=` (each device's sync feed), and search. The server is then the sync hub
between Mac and iPhone for things, replacing the Cloudflare encrypted sync for this record type
(history and tabs keep it until a later decision). IWT swaps the `SavedItemStore` implementation
behind its existing interface, imports its 3,760 items once (idempotent), and keeps capture and the
gallery UI unchanged. Images in saved items are stored as blobs here.

**Launcher (the Fieldcraft UI).** A Spotlight replacement whose whole content comes from Fieldcraft.
AppKit `NSPanel`; behaviour copied from nanolight (global Carbon hotkey without Accessibility
permission, floating panel, menu-bar item, login item, tiered fuzzy matcher with typo tolerance,
launch-count boost). Shape:
- One field. Results grouped by kind: top hit, apps, things, mail, documents, lessons and notes,
  people, later files. Apps and recents answer locally in the first frame; server results stream in
  after a short debounce, so typing never waits on the network.
- A preview pane: the thing with its image, the mail thread, a snapshot of the HiComputer document,
  the lesson with its author tag, Quick Look for files.
- Actions on each result: open in its own app (IWT, mail, HiComputer), copy link, hand to an agent as
  context, mark private, forget.
- Filters typed as tokens: `mail:`, `from:dalma`, `in:<room>`, `kind:lesson`, `by:<agent>`, dates.
- A display-context switch with a visible badge: in shared-screen mode only display-safe kinds show.
  Automatic detection of screen sharing is a later step after checking what macOS exposes.
- Questions, not only keywords ("what did we decide about pricing") are answered from the top results
  with citations. Later, and only with a local or owner-approved model.
- Nothing typed leaves the owner's machines: no web suggestions, no hosted search.
- Two kinds of provider behind one ranker in the launcher. Device-local providers answer in the first
  frame and never go to the server: installed apps (the roots nanolight scans, kept current with a
  file watcher, plus System Settings panes), open IWT tabs and windows (live state over local IPC from
  IWT), calculator and unit conversion, and recent files. Fieldcraft providers stream in after the
  debounce: My Things, history, bookmarks, mail, HiComputer documents, lessons and notes, people. Every
  result carries a source badge (App, Tab, History, Bookmark, Saved, Mail, HiComputer, Lesson), as the
  owner asked earlier for IWT search. Ranking: an exact or prefix app match is the top hit; then score
  with a per-kind cap so one kind cannot fill the panel; launch and open counts are a local boost.
  Results never reorder under the cursor once shown; late results append below.

**Graph.** People (mail senders, HiComputer members), sites, projects and rooms become entities;
items link to them. Used for filters and display first, ranking only after a replay shows a gain.

**Scale to check.** 100,000 mail messages at about 3 chunks each is about 300,000 chunks; at
57 passages per second that is about 90 minutes of initial embedding, and about 230 MB of int8
vectors, which brute-force search on the mini can still handle. Measure before choosing an ANN index.

## Implementation Stages

0. Settle the offline and launcher-home decisions and the vector dimension. Consolidate the local
   branches.
1. Item model and storage split (refactor-safe for lessons): items, chunks, blobs, per-family files,
   `POST /items:batch`, `DELETE /sources/<id>`, tests for idempotent upsert and complete deletion.
2. Access: `acl_json`, display-safe flag, scope; the access filter applied before ranking for items
   and lessons; tests per source rule.
3. `GET /search` across kinds; lesson replay unchanged.
4. Things API and IWT cutover for My Things: the endpoints above, the one-time import, the device
   change feed, the IWT `SavedItemStore` swap behind a setting, offline cache and outbox on iPhone,
   NOTICE reworded. Build a 100-query labeled set from real Command-T use before switching search.
5. Launcher, first version: AppKit panel, hotkey, apps and things, preview, open-in-app, display
   switch.
6. HiComputer connector (documents, versions, to-dos, messages, membership), shown in the launcher.
7. Mail connector, shown in the launcher; per-account headers-only switch.
8. IWT stops writing its own semantic index once G4 passes.

Rollback: each connector is off by a setting; the item files can be deleted without touching lessons.

## Testing Plan

- Unit: upsert idempotency, delete-from-source removes every derived row and blob, ACL changes take
  effect on the next query, scope and display rules per kind.
- Privacy: canary strings from a private window, an opted-out mail account and a room the reader is
  not in never appear in results, logs or `retrieval_log`.
- Quality: lesson replay (865 queries) unchanged; the new 100-query Command-T set for items; a
  HiComputer query set built from real agent questions.
- Load: bulk mail ingest while agents query lessons on the mini; lesson p95 within today's budget.

## Review Plan

Wrong problem (does the owner need one index, or one search across indexes); privacy regressions
against the written promises; moved cost (mini RAM and disk, embedding backlog); complexity of
keeping two engines during the cutover; evidence that cross-kind results are actually better.

## Definition Of Done

- G1: deleting a source removes all its text, chunks, vectors, edges and blobs (tested per source).
- G2: privacy canaries never leak (tested per source and per display context).
- G3: lesson retrieval replay shows no loss beyond the bootstrap CI.
- G4: Command-T through Fieldcraft is not worse than the local index on the 100-query set, with
  p95 latency within IWT's target, on Mac and iPhone.
- G5: HiComputer agents only ever see items from rooms they belong to (tested).
- G6: My Things round-trips through Fieldcraft with no loss: the import count equals IWT's, a save on
  the Mac appears on the iPhone through the change feed, an offline save on the iPhone lands once on
  reconnect (tested), and unsave deletes everywhere.
- G7: the launcher shows local results in the first frame and server results within IWT's search
  latency target, measured on this Mac; in shared-screen mode no private or non-display-safe item
  appears (tested with canaries).
- Nothing deployed and no promise text changed without the owner's approval.

## Change Log

- 2026-11-04: written from the owner's direction (Fieldcraft as the indexer for My Things, mail and
  HiComputer agent documents) and a read-only survey of the IWT and HiComputer repos.
- 2026-11-04: owner direction: Fieldcraft is the things backend for IWT (not a copy indexer), the
  nexus for all content, and gets its own launcher; content may live on the owner's own server.
  Things API, launcher, stages 4-8 and gates G6-G7 added.
- 2026-11-16: stage 1 and item search built (CH-0015): items in `items-<source>.sqlite3`, passages by
  IWT's rule, `POST /items:batch`, `DELETE /sources/<id>`, `GET /search` (BM25 + dense over passages,
  RRF k=60, mild recency, Gemma dense floor 0.6 from IWT's calibration). Dimension question settled:
  both 768.
