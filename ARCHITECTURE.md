# Architecture

This document describes how fieldcraft is currently built. For *what it is*, see [README.md](README.md). For *where it's going*, see [ROADMAP.md](ROADMAP.md). For *how it's deployed and observed*, see [docs/OPERATIONS.md](docs/OPERATIONS.md). For *what has changed*, see [change_history/](change_history/).

This is the authoritative source for module boundaries, data flow, schema, and current invariants. When something here disagrees with the code, the code wins — but raise an issue and submit a fix to this doc.

> **Deployment note (read before auditing):** in shared-server mode the authoritative store is the server's `~/.fieldcraft/memory.sqlite3`. `FIELDCRAFT_SERVER_URL` is the routing authority: non-local means use HTTP, localhost means local catalog. A client's local SQLite is **stale and must not be used to judge the system** unless local mode is explicitly configured. See §0 below and [docs/OPERATIONS.md](docs/OPERATIONS.md).

---

## 0. Deployment Topology

Fieldcraft runs either locally (CLI and SQLite store on one machine) or as a client/server system where one always-on machine runs `fieldcraft-server` and clients reach it over a private network (the reference deployment uses Tailscale).

| Node | Role | Process | Store |
|---|---|---|---|
| Shared server (port 7777) | **Authoritative server** | `fieldcraft-server` (launchd `com.fieldcraft.server` on macOS) | `~/.fieldcraft/memory.sqlite3` — source of truth |
| Client machines | **Client** | `fieldcraft` CLI; an optional local gateway (`--upstream-url`) may proxy to the server but must not define the routing contract | `~/.fieldcraft/memory.sqlite3` — **stale, do not use unless localhost mode is intentional** |

Clients (`fieldcraft` CLI) read `FIELDCRAFT_SERVER_URL` from `~/.fieldcraft/env`; that persisted value beats stale inherited shell environment. A non-local URL routes commands over HTTP to the server, which owns the catalogue and runs reads, writes, `/stats`, and server-backed maintenance paths. A localhost URL means local catalog mode. **Observe the system via `fieldcraft stats` (authoritative `/stats`), never via a client's local DB when configured for a shared server.** Full deploy/restart/recovery procedure is in [docs/OPERATIONS.md](docs/OPERATIONS.md).

---

## 1. Workspace Layout

Rust workspace, edition 2024. Six crates, all under `crates/`:

| Crate | Purpose | Key files |
|---|---|---|
| `fieldcraft-core` | Shared domain types — `KnowledgeRecord`, payload structs, enums, `TrError` | `src/lib.rs` |
| `fieldcraft-storage` | SQLite schema, repositories, session ingestion state | `src/schema.rs`, `src/lib.rs` |
| `fieldcraft-retrieval` | Embeddings (Gemini `gemini-embedding-001` via HTTP), hybrid search, fusion, scoring, MMR | `src/retrieval.rs`, `src/sqlite_search.rs` |
| `fieldcraft-extract` | Transcript parsing, compression, LLM extraction (Stage A/B) | `src/lib.rs` |
| `fieldcraft-cli` | CLI binary; HTTP client when `FIELDCRAFT_SERVER_URL` is set | `src/lib.rs` |
| `fieldcraft-server` | HTTP server for multi-machine sharing (axum 0.8) | `src/lib.rs` |

The `fieldcraft-mcp` crate was deleted in CH-0003; agents now interact with fieldcraft exclusively through the `fieldcraft` CLI, which proxies to `fieldcraft-server` over HTTP when `FIELDCRAFT_SERVER_URL` is set.

Storage location: `~/.fieldcraft/memory.sqlite3` (configurable via `FIELDCRAFT_HOME` for isolated test runs).

---

## 2. Core Types (`fieldcraft-core`)

### KnowledgeRecord

The single record envelope. Persistent fields:

| Field | Type | Notes |
|---|---|---|
| `id` | `String` | SHA256-based stable ID |
| `project_id` | `String` | Write-time/source namespace. Retrieval treats project/app identity as cue evidence, not a hard boundary (history: CH-0004). |
| `kind` | `KnowledgeKind` | `claim` \| `procedure` \| `episode` \| `directive` \| `context` |
| `title` | `String` | Short summary for display |
| `payload_json` | `String` | Serialized `ClaimPayload` \| `ProcedurePayload` \| etc. |
| `state` | `KnowledgeState` | `active` \| `provisional` \| `superseded` \| `retired` |
| `confidence` | `f32` | 0.0–1.0; starts 0.5 auto-extracted, 0.8 manual |
| `source` | `KnowledgeSource` | `Session{session_id, tool}` \| `External` \| `Imported` \| `Manual` |
| `superseded_by` | `Option<String>` | Pointer to replacement record |
| `project_specific` | `bool` | True if references project-internal entities |
| `created_at` | `DateTime<Utc>` | |
| `valid_until` | `Option<DateTime<Utc>>` | |
| `retrieval_text` | `String` | Pre-computed embedding text |
| `schema_version` | `u32` | 2=legacy, 3=directive/context, 4=phase tagging |
| `phase` | `Option<KnowledgePhase>` | `analyze` \| `reproduce` \| `edit` \| `verify`; None for legacy |
| `tier` | `KnowledgeTier` | `active` (auto-injected) \| `archive` (manual fetch only) |
| `owner` | `Option<String>` | Token label of writer; None for legacy |
| `session_date` | `Option<DateTime<Utc>>` | Source session start time; preferred over `created_at` for recency |

### Payload variants

Five payload types map to the five `KnowledgeKind` values:

- **`ClaimPayload`** — `statement`, `mechanism`, `scope`, `exceptions`, `affected_files`, `retrieval_questions`, `trigger`, `invalid_if`
- **`ProcedurePayload`** — `goal`, `preconditions`, `steps`, `failure_modes`, `affected_files`, `retrieval_questions`, `trigger`, `invalid_if`
- **`EpisodePayload`** — `summary`, `trigger`, `outcome`, `observations`, `affected_files`, `derived_claim_ids`, `derived_procedure_ids`
- **`DirectivePayload`** — `action`, `reason`, `files`, `trigger`, `avoid`, `verify`, `invalid_if`, `retrieval_queries`
- **`ContextPayload`** — `statement`, `scope`, `files`, `implications`, `invalid_if`, `retrieval_queries`

### Phase semantics

Phase tagging is inspired by **Shen et al. 2026 (arXiv:2602.21611)** — *"Structurally Aligned Subtask-Level Memory for Software Engineering Agents"*. Fieldcraft's phase comes from a keyword heuristic, not the paper's method. Fieldcraft has not measured the effect of its own phase boost.

The four phases:
- **Analyze** — understanding code, invariants, module structure ("what/why")
- **Reproduce** — reproducing a bug, setting up failing tests
- **Edit** — making changes — the fix, the patch
- **Verify** — confirming a fix works — tests, regression guards

### Cue model

`CueBundle` has two compatible layers (history: CH-0002, CH-0004):

- Legacy fixed fields: `phase`, `domain`, `tools_involved`, `active_files`, `distinctive_terms`, `user_state`, `session_arc`.
- First-class typed cues: `cues: Vec<Cue>`, where each cue has a stable `CueDimension`, flexible `CueValue { raw, canonical, aliases, kind }`, `confidence`, and `source`.

Typed cue dimensions are `identity`, `task`, `domain`, `technical`, `file`, `entity`, `symbol`, `temporal`, and `outcome`. Identity cues admit candidates and resolve aliases; task/domain/symbol/file cues rank candidates. No cue dimension is a storage partition.

### Relations

`RelationKind`: `Supersedes` (record replaces another) | `DerivedFrom` (claim/procedure was extracted from an episode).

---

## 3. Storage (`fieldcraft-storage`)

### Database

Single SQLite file: `~/.fieldcraft/memory.sqlite3`. WAL mode + `busy_timeout=5000` on every open. Schema version tracked in `schema_meta` table; currently **v3**.

Migrations are **idempotent** — `run_migration()` handles `duplicate column` errors gracefully via ROLLBACK-then-return-Ok. New columns can be re-added safely across deploys.

### Tables (current schema)

| Table | Purpose |
|---|---|
| `knowledge` | The records themselves; one row per `KnowledgeRecord` |
| `knowledge_fts` | FTS5 virtual table mirroring `retrieval_text` for BM25 search |
| `knowledge_relations` | Supersedes / DerivedFrom edges between records |
| `imports` | Tracking external imports (URI, hash, status) |
| `feedback` | Per-record user/agent feedback signals (knowledge_id, session_id, signal, context) |
| `lesson_feedback` | Newer per-retrieval feedback (knowledge_id, signal, retrieval_id, project_id) |
| `retrieval_log` | One row per retrieval event: query, result IDs, scores, raw candidates, links surfaced |
| `skill_packages`, `skill_findings`, `skill_cards`, `skill_cards_fts`, `skill_retrieval_log` | Skill ingestion + retrieval (separate subsystem) |
| `cue_index` | Inverted index from cue value to `knowledge_id` for fast exact-match lookup (history: CH-0002) |
| `schema_meta` | Key-value (currently just `version`) |

### Key columns added across schema versions

The schema has evolved by additive migration:

- `knowledge.dense_vector BLOB` — dense embedding stored per record (Gemini `gemini-embedding-001`, 768-dim by default via `FIELDCRAFT_EMBED_DIM`; `fieldcraft reindex` rebuilds all vectors)
- `knowledge.schema_version INTEGER` — per-record schema marker (2/3/4)
- `knowledge.phase TEXT` — phase classification (nullable for v2 records)
- `knowledge.tier TEXT` — `active` | `archive`
- `knowledge.owner TEXT`, `knowledge.session_date TEXT` — provenance fields
- `retrieval_log` extended with `raw_candidate_*` and `link_ids_*` columns
- `retrieval_log.query_cue_bundle TEXT` — query-time cue bundle for closed-loop telemetry (history: CH-0002)
- `knowledge.cue_bundle TEXT` — JSON CueBundle auto-derived on insert. Includes legacy fixed fields plus typed cues such as identity/domain/technical/file/symbol (history: CH-0002, CH-0004)
- `knowledge_relations.confidence`, `valid_until` — relation strength tracking

### Invariants

1. **Tier promotion is human-only.** Any future distillation pipeline that promotes `source_sessions` content into `knowledge` records MUST set `tier = Archive`. Active promotion requires explicit human decision.
2. **Lesson → skill / AGENTS.md promotion is `--apply`-gated.** `fieldcraft promote` lists high-confirm `procedure`/`directive` workhorses by default and writes nothing. `--apply` requires `--id` (no bulk write, no hook/cron path). The original lesson is retained; provenance is a `knowledge_events` row with `event_type = lesson_promoted`. See [docs/promote-checklist.md](docs/promote-checklist.md).
3. **Schema migrations must be idempotent.** Use `run_migration()`; never raw `execute_batch` on ALTER TABLE.
4. **FTS5 mirroring is trigger-driven.** Don't write to `knowledge_fts` directly — INSERT/DELETE/UPDATE triggers on `knowledge` keep it in sync.
5. **Dense vectors live alongside records.** No separate vector store. Migration from Qdrant happened 2026-04-01.

---

## 4. Retrieval Pipeline (`fieldcraft-retrieval`)

The hot path — `retrieve()` in `crates/fieldcraft-retrieval/src/retrieval.rs`.

### 8-step pipeline

```
RetrievalRequest{ project_id, context, files, task, preferred_types, tier }
  ↓
1. build_query(request)              → single token string from identity hint + context + task + file basenames
2. build_query_cue_bundle(request)   → typed identity cue + deterministic domain/tool/file/symbol cues
3. detect_query_intent(request)      → ActionOriented | KnowledgeOriented | General
4. weights_for_intent(intent)        → 12-feature weight vector, dynamic on intent
5. encode_dense([query])             → Gemini gemini-embedding-001, 768-dim vector (needs GEMINI_API_KEY)
6. hybrid_search(repo, query, vec)   → unscoped dense + unscoped FTS5 BM25 + cue_index identity admission
   ↓ RRF fusion
7. compute_score per candidate       → weighted_sum of 12 features + weighted task grounding + cue-match adjustment
   ↓ phase boost (+0.25 same, -0.15 cross)
   ↓ minimum task-grounding filter when grounded candidates exist
   ↓ MMR rerank (lambda=0.7)
8. apply_token_budget                → top-3 full detail, top-4-8 summary, rest title-only
  ↓
RetrievalResponse{ lessons, token_receipt, query_cue_bundle_json, raw_candidate_* }
```

`project_id` remains on the request/response for compatibility and provenance, but retrieval no longer uses it as `WHERE project_id = ?`. It is converted into an `identity` cue, indexed through `cue_index`, and excluded from task-overlap scoring so same-identity records do not outrank task-grounded records.

### The 12 scoring features

Defined as `P0_WEIGHTS: [f32; 12]` in `retrieval.rs:98`:

| Idx | Feature | Base weight | Notes |
|---|---|---|---|
| 0 | `rrf_rank` | 0.30 | `1 / (1 + rank)` from RRF fusion position |
| 1 | `dense_similarity` | 0.15 | Cosine similarity of dense vectors |
| 2 | `sparse_bm25_score` | 0.10 | BM25 normalized by max in result set |
| 3 | `node_type_claim` | 0.05 | 1 if kind=`claim` or `directive`; tuned per intent |
| 4 | `node_type_procedure` | 0.10 | 1 if kind=`procedure` or `context`; tuned per intent |
| 5 | `node_type_episode` | 0.02 | 1 if kind=`episode` (legacy) |
| 6 | `confidence` | 0.05 | Record's stored `confidence` clamped [0, 1] |
| 7 | `state_active` | 0.06 | 1 if state=`active` |
| 8 | `state_provisional` | -0.03 | Penalty for state=`provisional` |
| 9 | `recency_decay` | 0.08 | `exp(-age_days / 90)` — 90-day half-life |
| 10 | `title_match_ratio` | 0.10 | Jaccard-ish overlap of query tokens with title + retrieval text |
| 11 | `scope_match` | 0.25 | File basename overlap between request and record |

`scope_match` is the **most heavily weighted feature** (0.25). It compares request `files` against the record's `affected_files` extracted from `retrieval_text` (parsed from `scope:` / `files:` prefixes).

### Intent-based weight blending

`detect_query_intent` classifies the combined `context + task` text:
- **ActionOriented** (`"how do I"`, `"fix"`, `"error"`, `"why"`) → boosts claim/directive weight
- **KnowledgeOriented** (`"what is"`, `"how does"`, `"architecture"`, `"explain"`) → heavily boosts context weight (slot 4 to 0.15)
- **General** → base weights

Agent-supplied `preferred_types` overrides keyword inference, blending directive/context by relative weight.

### Phase boost (post-scoring)

After `compute_score`, if both `query_phase` and `candidate.phase` are `Some`:
- Same phase → `score += 0.25` (strong boost)
- Cross phase → `score -= 0.15` (penalty)

Legacy records (`phase = None`) are neutral — no adjustment. This implements Shen et al. 2026's category isolation.

### MMR diversity reranking

`mmr_rerank(candidates, max_results, lambda=0.7)` selects results that maximize:
```
mmr = lambda * score - (1 - lambda) * max_similarity_to_already_selected
```

Iteratively picks the candidate with highest MMR, removes from pool, repeats until `max_results` filled. Uses dense vector cosine for similarity.

### Token budget

Default `DEFAULT_TOKEN_BUDGET = 60_000` (against `CONTEXT_WINDOW = 200_000`). `max_results = (budget / 500).clamp(3, 30)`. Top-3 records get full detail as XML-like `<remembered_note>` blocks; positions 4-8 get summary blocks (first sentence); rest get title-only note blocks. Truncates when adding next record would exceed budget.

### Embedding text construction

`build_embedding_text(record)` produces a pipe-delimited string per kind, used both for the dense vector input and for BM25 indexing via `retrieval_text`:

- Claim: `"{statement} | {mechanism} | scope: {scope} | {retrieval_questions}"`
- Procedure: `"{goal} | steps: {first 3} | preconditions: {all} | {retrieval_questions}"`
- Episode: `"{trigger} | {outcome} | {first 2 observations}"`
- Directive: `"{action} | {reason} | trigger: {trigger} | avoid: {avoid} | {retrieval_queries}"`
- Context: `"{statement} | scope: {scope} | {implications} | {retrieval_queries}"`

---

## 5. Extraction (`fieldcraft-extract`)

Two-stage extraction from raw transcripts:

- **Stage A** — discover, compress, chunk transcripts from `~/.claude/` and `~/.codex/`
- **Stage B** — submit chunks to OpenAI Batch API for structured record extraction; results are deduplicated before insertion

Requires `OPENAI_API_KEY`. Batch operations are async (poll-based via `extract-batch-poll`). State tracked in `source_sessions` table.

---

## 6. Server

Single network entrypoint:

- **`fieldcraft-server`** — HTTP server (axum 0.8). Used for multi-machine sharing
  via `FIELDCRAFT_SERVER_URL`. Agents on client machines invoke `fieldcraft`
  CLI commands locally; the CLI forwards over HTTP when `FIELDCRAFT_SERVER_URL`
  is set in the environment (typically sourced from `~/.fieldcraft/env`).

Auth via `FIELDCRAFT_AUTH_TOKEN` (HTTP). Tokens stored in `~/.fieldcraft/tokens.sqlite3`, with macOS Keychain enrollment supported.

Earlier versions exposed an MCP stdio server (`fieldcraft serve`); that path
was removed (history: CH-0003) because MCP servers on client machines wrote
to a local SQLite catalog regardless of `FIELDCRAFT_SERVER_URL`, creating a
silent split-brain between client and server. The CLI-only design eliminates
that class of bug.

The server's `search_lessons_handler` issues a fire-and-forget `log_retrieval()`
call so HTTP-mode retrievals from remote clients are visible in `retrieval_log`
on the server — closing the closed-loop telemetry path for multi-machine setups
(history: CH-0003).

### Session-end distiller

The Stop hook (`install/bin-hooks/stop-feedback.sh`) is non-blocking: it always
writes a session-end marker to `~/.fieldcraft/state/session-ends.jsonl` and
exits 0 (history: CH-0003). When a coding-agent CLI is on `PATH` and the
transcript is ≥10KB, the hook may spawn a detached background distiller (`codex
exec` or `claude --print`) that reviews the transcript and runs `fieldcraft
write` only if a non-obvious, durable finding is present. The foreground agent
never pays a turn for this.

LLM distill is **friction-gated** (history: CH-0007). A cheap local
`fieldcraft distill-gate` scores the transcript (interrupts, permission
denials, user corrections, tool failures, same-approach retries) and skips
quiet sessions. Default threshold is 3 (`FIELDCRAFT_DISTILL_MIN_FRICTION`);
`0` disables the gate; `FIELDCRAFT_DISTILL_FORCE=1` always distills. Marker
+ `trace-analyze` still run regardless. Decisions are appended to
`~/.fieldcraft/state/distill.log` as
`[fieldcraft-stop-hook] decision=skip|run score=… threshold=… reasons=…`.

The distiller is selected by `FIELDCRAFT_DISTILLER` (sourced from
`~/.fieldcraft/env`): `codex | grok | claude | none | unset(auto)`. Auto-detect
order: codex preferred, then grok, then claude, then marker-only.

---

## 7. Experiments

There is no maintained benchmark suite. Two harnesses live under `eval/` (see [eval/README.md](eval/README.md)): a warm-vs-cold agent run on real tasks (`usefulness_harness/`) and a logged-query replay through an external reranker (`jev_rerank/`). Numbers inside those directories are dated snapshots of one-off runs, not claims about the current system.

## 8. The Feedback Loop

This is the heart of the system: how a completed session turns into a confidence signal. The loop is **closed and recording** today, but runs in **shadow mode** — it observes outcomes without yet steering retrieval.

### Flow

```
SessionStart hook → fieldcraft query
   → retrieve() (exact dense cosine + FTS5 BM25, cue-aware scoring)
   → query cue bundle logged to retrieval_log.query_cue_bundle
         ↓ (agent works the session)
Stop hook (non-blocking) → fieldcraft trace-analyze --session <id> --transcript <p> --cwd <d>
   → CLI extracts cited lesson:<id> refs, contradictions, git-changed files
   → POST /sessions/trace-analyze
         ↓
Server correlates against retrieval_log over the session window and emits outcome events:
   CONFIRMED    — agent cited `lesson:<id>`  OR  served lesson's affected_files ∩ changed files
   CONTRADICTED — agent explicitly contradicted a served lesson
   IGNORED      — retrieved-but-uncited lesson whose cue_match_factor ≥ threshold (default 0.35)
         ↓
apply_feedback_outcome(repository.rs) updates dynamic_confidence + writes lesson_outcome_events
```

### Outcome → confidence update

`compute_outcome_update` (`crates/fieldcraft-storage/src/repository.rs:67`) is asymptotic, clamped to `[0.05, 0.95]`:

| Outcome | Update | Effect |
|---|---|---|
| Confirmed | `c + 0.05·(1 − c)` | approaches 0.95, never overshoots |
| Ignored | `c − 0.08·c` (`·0.92`) | geometric decay |
| Contradicted | `c − 0.12·c` (`·0.88`) | harder decay |

Each call also appends a row to `lesson_outcome_events`. All emissions are idempotent on `(knowledge_id, session_id, source)`.

### Shadow mode — what is live, what is gated

- **`dynamic_confidence` is computed and stored but is not the ranking authority.** It feeds only the small additive confidence feature (weight 0.05, via `COALESCE(dynamic_confidence, confidence)`); the multiplicative use is gated behind env `FIELDCRAFT_DYNAMIC_CONF` (default OFF; enabled only for `"1"`/`"true"`). `apply_dynamic_confidence_multiplier` in `retrieval.rs:801` is the gate. The loop records truth; it does not yet change what surfaces.
- **`cue_match_factor` exists** (`fieldcraft-retrieval::cue_match`) and a cue-match adjustment is summed into the score in `score_candidate`, but cue-aware ranking is not yet the ranking authority — full cue-first ranking is staged.
- **No time-decay** of confidence yet (planned only).

### Merge / consolidation

Background dedup via `knowledge_merge_queue` → `obvious_merge_candidate` (`repository.rs`). A candidate pair must clear dense-vector cosine **≥ 0.92** (`DUPLICATE_MIN_DENSE_COSINE`) **and** cue overlap **≥ 0.35** before a composite score gates the merge. **Non-destructive:** the loser is set `state = 'superseded'` and archived (a `Supersedes` relation is written), never deleted. ~8,955 merges done to date.

### Cue-first retrieval (shipped, CH-0002 + CH-0004)

`project_id` is compatibility/provenance metadata, not a retrieval partition. Identity is represented as typed cue evidence: identity cues *admit* candidates via `cue_index`; task/domain/symbol/file overlap *ranks* them, so a same-identity but off-task memory cannot outrank a task-grounded one.

For the shipped-vs-stubbed-vs-planned breakdown of every piece, see [ROADMAP.md](ROADMAP.md).

---

## 9. Memory Lifecycle (drop, consolidate, dream)

LIVE on the reference shared-server deployment. The store is no longer append-only: it can evict, self-correct, and (behind a gate) generate new knowledge. Full plan and change log: [docs/plans/memory-lifecycle.md](docs/plans/memory-lifecycle.md).

### L1 — storage foundation

`crates/fieldcraft-storage/src/repository.rs`. Every record has a `protected` column (`repository.rs:824` `set_protected`). `is_protected(id)` (`repository.rs:859`) is true for an explicit pin, `kind in {directive, context}`, or `confidence >= 0.9`. `served_activation(id)` (`repository.rs:912`) is a ln-sum decay over each appearance in `retrieval_log.result_ids`; `None` means never served — the eviction sentinel, distinct from zero. `archive_record` / `restore_record` (`repository.rs:956`/`986`) flip `tier` and write a `knowledge_events` audit row each time; neither issues a SQL DELETE.

### L2 — eviction

`archive_candidates(min_age_days)` (`repository.rs:1029`) is a single batched pass (no per-record subqueries) returning ghost candidates (`age > 60d AND never served AND 0 confirmed outcomes AND not protected`) and contradicted-losers (`contradicted >= 3 AND contradicted > confirmed AND not protected`). The `fieldcraft lifecycle` CLI defaults to dry-run; `--apply` is required to archive. The server runs the same logic on a scheduled `lifecycle_loop` (`crates/fieldcraft-server/src/lib.rs:380`, default 6h via `FIELDCRAFT_LIFECYCLE_INTERVAL_SECS`), which only writes when `FIELDCRAFT_LIFECYCLE_APPLY=1` — enabled in the server's launchd environment on the reference deployment. First production pass archived 41 records.

### L3 — resurrection

After every live retrieval, `search_lessons_handler` spawns a background task (`tokio::spawn`, `crates/fieldcraft-server/src/lib.rs:713-747`) — never on the response path — that scans `tier='archive'` records against the query's dense embedding (`RetrievalResponse.query_dense`, so no second encode). Matches at or above `FIELDCRAFT_RESURRECTION_COSINE` (default **0.85**, live-calibrated: identical-text queries measure only ~0.927 against cue-enriched record embeddings on the production store, so the originally-planned 0.93 threshold was unreachable) log an `archive_match` event; `FIELDCRAFT_RESURRECTION_THRESHOLD` hits (default 3) auto-restore via `restore_record`. `FIELDCRAFT_RESURRECTION=0` disables the scan. Proven end-to-end on production.

### L4 — retrieval-induced forgetting

`rif_demotion_candidates(min_cooccur)` (`repository.rs:1155`) finds near-twin pairs — dense cosine `[0.85, 0.92)`, below the merge threshold — that are served together at least `min_cooccur` (3) times, where one side has strictly more `confirmed` outcomes; the loser is proposed for reversible archival. Protected records and pair-winners are never proposed. Runs in both `fieldcraft lifecycle` and the server's scheduled loop, merged into one deduped candidate set alongside L2.

### L5 — dreaming (built, `--apply` gated)

`fieldcraft dream` (dry-run by default). `dream_clusters` (`repository.rs:1347`) clusters same-project/same-kind lessons via union-find over pairwise cosine `[0.80, 0.92)`, requiring `>= 3` members and `>= 2` confirms. With `--apply`, an LLM (default `codex exec --sandbox read-only`, overridable via `--llm-cmd`) is invoked once per cluster to abstract ONE derived Claim; unparseable or empty LLM output is skipped rather than force-written. Guardrails: the derived claim must cite `>= 3` sources (each written as an audited `derived_from` event, `repository.rs:1519`), lands `state='provisional'`, and originals are retained untouched. A real `confirmed` outcome on a provisional record promotes it to `active`, writing a `promoted_active` audit event (`repository.rs:2047`) — the only mechanism by which a derived claim earns its place. Dry-run on production found 3 clusters; `--apply` stays off in production until the G1 usefulness sweep (`eval/usefulness_harness/`) reaches its n>=10 target — infrastructure and cold-arm `CODEX_HOME` isolation are proven, the full sweep is still running.

### Merge / duplicate detection fix

`duplicate_score` (`repository.rs:2779`) now merges byte-identical `title`+`retrieval_text` pairs even when neither record has a dense vector — HTTP-written records are embedded best-effort (`index_record_best_effort`) and can stay vectorless when embedding fails, so this closes a gap where identical HTTP-posted lessons never deduplicated. Anything less than byte-identical still requires embeddings to judge.

---

## 10. Cross-References

- Deploy / observe / restart: [docs/OPERATIONS.md](docs/OPERATIONS.md)
- Shipped vs stubbed vs planned: [ROADMAP.md](ROADMAP.md)
- Memory lifecycle plan: [docs/plans/memory-lifecycle.md](docs/plans/memory-lifecycle.md)
- Research backing for design choices: docs/plans/cue-bundle-retrieval.md (encoding specificity, testing effect)
- Migration plan (partly shipped, partly stubbed): [docs/plans/cue-bundle-retrieval.md](docs/plans/cue-bundle-retrieval.md)
- Change log: [change_history/](change_history/)
