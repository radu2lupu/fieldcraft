# Public launch review (2026-09)

> Independent technical and positioning review run before the repository went public, at commit
> `14ef497`. Several pre-launch items were fixed right after it; the live checklist is the
> "Public launch track" in [ROADMAP.md](../../ROADMAP.md). Competitor facts are as documented by each
> project in September 2026; the public summary is [docs/COMPARISON.md](../COMPARISON.md).

## 1. What Fieldcraft actually does (mechanism, end to end)

Six-crate Rust workspace (edition 2024): `fieldcraft-core` (types, deterministic cue extraction), `fieldcraft-storage` (SQLite schema + repository, lifecycle, dream clustering, Leiden), `fieldcraft-retrieval` (Gemini embeddings, hybrid search, scoring, MMR, cue match), `fieldcraft-extract` (transcript compression + LLM extraction, citation matcher), `fieldcraft-server` (axum HTTP server + gateway proxy + a separate "memories/people/actions" API), `fieldcraft-cli` (all subcommands; HTTP client when `FIELDCRAFT_SERVER_URL` is set). No MCP server (deleted in CH-0003 because MCP clients wrote to a local catalog regardless of server URL).

### 1.1 Write path
- `fieldcraft write --kind <claim|procedure|episode|directive|context> "<text>"` builds a `KnowledgeRecord` (SHA256 id, `project_id` derived from cwd/git, static `confidence` 0.8 manual / 0.5 auto-extracted, `state` provisional or active, `tier` active). `insert_knowledge` (`crates/fieldcraft-storage/src/repository.rs`) derives a deterministic cue bundle (domain from file paths, known tools, distinctive terms, files, identity cue from `project_id`) and writes `cue_index` rows. FTS5 mirror is trigger-driven.
- Server path (`write_lesson_handler`, `crates/fieldcraft-server/src/lib.rs:347`): canonicalizes project id, stamps `owner` from the bearer token label, inserts, then embeds in a `spawn_blocking` task (`index_record_best_effort`) and enqueues a merge candidate. Note: ARCHITECTURE.md/OPERATIONS.md still say "HTTP-written records are not embedded server-side" — that is stale; they are.
- Bulk ingest (`fieldcraft ingest` / `extract-batch`): Stage A (Haiku-class chunk classification) → Stage B (Sonnet-class structured extraction, parallel) → Stage C (Haiku-class self-critique) → word-Jaccard dedup (`crates/fieldcraft-extract/src/extract.rs`). Backend chosen by env: `ANTHROPIC_API_KEY`, else `OPENAI_API_KEY` (default model gpt-4o, or OpenAI Batch API), else `claude -p`.

### 1.2 Background distillation (Stop hook)
`install/bin-hooks/stop-feedback.sh` always exits 0. It (a) appends a session-end marker, (b) detaches `fieldcraft trace-analyze --session --transcript --cwd` (regex only, no LLM), and (c) if the transcript is ≥10 KB and a distiller CLI is present, detaches one of: `codex exec --dangerously-bypass-approvals-and-sandbox --disable hooks --ephemeral`, `grok -p --always-approve --disable-memory`, or `claude --print --dangerously-skip-permissions --max-budget-usd 0.10`, with a 600 s watchdog and a prompt to write at most ONE lesson via `fieldcraft write` or nothing. `FIELDCRAFT_DISTILLER` selects (`codex|grok|claude|none|auto`); `install.sh` defaults it to `codex`.

### 1.3 Retrieval and scoring (`crates/fieldcraft-retrieval/src/retrieval.rs::retrieve_from_repository`)
1. `build_query`: identity hint (project id + alias variants) + context + task + file basenames, stop-words stripped.
2. Intent detection (ActionOriented / KnowledgeOriented / General) selects a 12-feature weight vector; agent `preferred_types` can override kind weights.
3. Dense query vector from **Google Gemini `gemini-embedding-001`, 768-dim** (`crates/fieldcraft-retrieval/src/lib.rs`), L2-normalized, 6 retries with RetryInfo-aware backoff. Requires `GEMINI_API_KEY` or `GOOGLE_API_KEY`. On failure it prints a warning and **falls back to BM25-only**. There is no local embedding model in the dependency tree (no fastembed/ort/candle in `Cargo.lock`); ARCHITECTURE.md §4/§5 ("BGE-Small 384-dim, fastembed") is stale.
4. `hybrid_search_for_query_unscoped` (`sqlite_search.rs:352`): three ranked lists — **exact brute-force cosine** over every active-tier record's stored vector (loads all rows, not ANN), FTS5 BM25, and `cue_index` identity-cue hits — fused by RRF (k=60), top 80, enriched with cue bundles and `effective_confidence = COALESCE(dynamic_confidence, confidence)`.
5. Top 30 scored: `weighted_sum` of 12 features (`P0_WEIGHTS`: rrf 0.30, dense 0.15, bm25 0.10, kind slots 0.05/0.10/0.02, confidence 0.05, active 0.06, provisional −0.03, recency exp(−age/90d) 0.08, title match 0.10, scope/file overlap 0.25) + phase boost (+0.25 same / −0.15 cross, query phase from keyword heuristics) + task grounding (`min(overlapping non-identity terms, 6) × 0.12`) + cue-match adjustment (`(cue_match_factor − 1) × 0.30`, geometric mean over phase/domain/user_state/tools/terms with a 0.3 floor) → optional multiplicative `effective_confidence` gated by `FIELDCRAFT_DYNAMIC_CONF` (default off).
6. Skill-bundle expansion, `keep_grounded_candidates` (drops candidates with <2 overlapping terms when at least one has ≥2 and ≥3 survive), MMR rerank (λ=0.7, dense cosine), token budget (60k chars/4; top-3 full, 4-8 summary, rest title-only) emitted as `<remembered_note id="lesson:<id>" kind= title=>` blocks. CLI `--limit` default 5; server default 10.
7. Every retrieval is logged (`retrieval_log`: query, result ids/scores, raw candidates, `query_cue_bundle`, `session_id`, latency). In server mode a background task then scans `tier='archive'` for resurrection (§1.6).

Subtle point for public copy: `dynamic_confidence` already influences ranking a little in "shadow mode" — the additive confidence feature (weight 0.05) reads the COALESCEd effective confidence. What is gated is the multiplicative use. Docs currently say "not used in retrieval ranking"; say "not the ranking authority" instead.

### 1.4 Outcome attribution (`fieldcraft trace-analyze` → `POST /sessions/trace-analyze`)
Client (`crates/fieldcraft-cli/src/main.rs:4754-5010`): scans assistant-only turns of the JSONL transcript for `lesson:([a-f0-9]{16,})` (CONFIRMED candidates) and a contradiction regex (`lesson:<id> is wrong/outdated/…`, `contradicts lesson:<id>`, `doesn't apply anymore`) (CONTRADICTED); collects session time bounds; runs `git diff --name-only` and `git diff --staged --name-only` in cwd (uncommitted changes only).
Server (`trace_analyze_handler`, `lib.rs:1198`):
- CONFIRMED for each cited id (note "inline lesson:<id> citation").
- CONTRADICTED for each regex hit (cited-and-contradicted counts as contradicted only).
- CONFIRMED (`file_overlap`) for every lesson served in this session's `retrieval_log` rows whose `affected_files` **basenames** intersect the changed-file basenames.
- IGNORED for served-but-uncited lessons whose max `cue_match_factor` against the logged query cue bundles ≥ 0.35 (floor is 0.30, so almost any phase or term overlap crosses it).
- All idempotent on `(knowledge_id, session_id, source)`.
The distinctive-token matcher in `crates/fieldcraft-extract/src/citation_match.rs` (word-boundary matching of record terms in agent output) is **not wired anywhere** — plumbing only.

### 1.5 Confidence update math (`repository.rs::compute_outcome_update`)
`c' = c + 0.05·(1−c)` (confirmed), `c' = 0.92·c` (ignored), `c' = 0.88·c` (contradicted), clamped to [0.05, 0.95]; chained off `dynamic_confidence` if present else static `confidence`; one `lesson_outcome_events` row per update. A CONFIRMED event also promotes a `provisional` record to `active` (`promoted_active` audit event) and then **demotes that record's `derived_from` sources** (superseded, archived) — this is the dream-tier activation path, and it fires for any confirm source including the weak `file_overlap` signal.

### 1.6 Memory lifecycle (live on the owner's server; `FIELDCRAFT_LIFECYCLE_APPLY=1`, 6 h cadence)
- Protection: explicit pin, `kind ∈ {directive, context}`, or static confidence ≥ 0.9 are never auto-archived.
- Eviction (`archive_candidates`): ghosts = age > 60 d AND never served AND 0 confirms AND unprotected; contradicted losers = contradicted ≥ 3 AND contradicted > confirmed. Archive = `tier='archive'` + audit row, no DELETE.
- Resurrection: per-query background scan of archived records against the query vector; cosine ≥ 0.85 logs `archive_match`; 3 matches auto-restore. (0.85 was calibrated live because identical text only reaches ~0.927 against cue-enriched record embeddings.)
- Retrieval-induced forgetting (`rif_demotion_candidates`): near-twin pairs (cosine [0.85, 0.92)) co-served ≥ 3 times where one side has strictly more confirms → loser archived reversibly.
- Merge: cosine ≥ 0.92 + cue overlap ≥ 0.35 → loser `superseded` (tombstone), byte-identical pairs merge without vectors.

### 1.7 Dream (generative consolidation)
`dream_clusters`: active embedded records grouped by (project, kind); edges at cosine [0.80, 0.92); Leiden/CPM communities; keep communities with ≥ 3 members and ≥ 2 confirms; skip clusters already sourcing a derived claim; max 3 clusters per run. With `--apply`, an LLM (`codex exec --sandbox read-only` by default) writes ONE derived claim citing ≥ 3 sources; it lands `provisional`; a real CONFIRMED promotes it and demotes its sources. Per the plan changelog it runs nightly on the reference server (launchd). The gate that "unlocked" it (G1) measured gold lessons, not derived-claim quality (§2).

### 1.8 Server and topology
axum 0.8; one `Mutex<KnowledgeRepository>` — retrieval (including the blocking Gemini HTTP call) runs while holding the lock, so queries serialize. Bearer tokens (sha256 at rest, max 64, LRU). `POST /enroll` is **open** (rate-limited 10/min/IP) and the authoritative server binds **0.0.0.0**; a gateway mode binds 127.0.0.1 and proxies to an upstream. `install.sh` always installs a local launchd service, even for single-machine use. There is also a separate "memories" API (`/memories`, `/people`, `/projects`, `/actions`, `/shapes`, `/views/today|inbox|…`) with `audio_ref` fields — a personal-assistant memory product living in the same binary and DB, undocumented in README.

### 1.9 Shipped vs shadow/gated vs planned (verified against code)
| Capability | State |
|---|---|
| Typed cue bundles at write + query, `cue_index`, identity admission | Shipped |
| Hybrid retrieval (exact cosine + BM25 + identity RRF), 12-feature scoring, phase boost, task grounding, MMR, token budget | Shipped |
| Cue-match adjustment summed into score | Shipped (not the ranking authority) |
| Retrieval logging with cue bundle + session id | Shipped |
| Trace analyzer: citation / contradiction regex, file-overlap confirm, cue-gated ignore | Shipped |
| `dynamic_confidence` update + `lesson_outcome_events` | Shipped |
| Multiplicative dynamic confidence in ranking | Gated off (`FIELDCRAFT_DYNAMIC_CONF`) |
| Distinctive-token citation matcher (`citation_match.rs`) | Built, not wired |
| Lifecycle L1-L4 (protect, evict, resurrect, RIF) | Shipped; apply is env-gated |
| Dream L5 | Built; `--apply` in nightly use on the owner's server |
| Background distiller (codex/grok/claude) | Shipped; default `codex`; owner has it off |
| LLM cue classifier (phase/user_state) | Not built (phase is keyword heuristic) |
| Golden retrieval eval harness | Not built |
| Learned ranker, co-retrieval graph, time decay | Planned |
| Session-lift metric (North Star) | Not computed anywhere (`stats` prints raw counts) |

---

## 2. Evidence audit

Legend — **measured**: a number exists and the design supports the claim; **plumbing-only**: the code path exists but no usefulness evidence; **aspirational**: stated as if true, no evidence; **contradicted**: evidence in the repo points the other way or the statement is factually off.

| # | Claim (where) | Evidence (path, numbers) | Status |
|---|---|---|---|
| 1 | "future agents repeat less investigation, make fewer mistakes, and burn fewer tokens" (README pitch) | `eval/usefulness_harness/RESULTS.md`, `results/scorecard.json`: n=19 pytest SWE-bench Verified tasks, codex agent, warm−cold steps −3.47 (95% CI [−6.58, −0.53]); resolved +5.3 pts (CI [0, +15.8], includes 0); tokens −40.7k (CI includes 0). But `tasks/lessons.json` "gold lessons" state the fix itself (e.g. "Fix: add a `mode` property that returns `self.buffer.mode.replace('b','')`"); memory was **push-injected into the prompt** by `src/run_task.sh`, not pulled through hooks/retrieval; single run per arm; one repo. | Measured **upper bound only** — supports "steps" claim under oracle lessons; does not support "fewer mistakes" (n.s.) or production-path claims. |
| 2 | "Gold-standard eval 29/30 (97%), P50 96 ms" (ARCHITECTURE §7, eval/RESULTS.md) | 30 **synthetic** query→result pairs; latency measured with the old local BGE model (now Gemini API, network-bound). | Plumbing-only; latency figure stale. |
| 3 | "Real lesson benchmark 10/15 PASS" | 15 queries, PASS/WEAK self-scored, 5 WEAK. | Plumbing-only, small n. |
| 4 | Gauntlet task-success eval | `eval/RESULTS.md`: 100% in warm, cold **and control**; RESULTS.md itself says results "cannot support a public claim". | Contradicted (no measurable effect). |
| 5 | Retrieval beats no-retrieval on held-out patch planning | ROADMAP "Eval Bar": 20 held-out tasks, format-only tied `lean_exact` retrieval at file hit@1 1.000 / test hit@1 0.900 while retrieval used more tokens; "public benchmark improvement is not currently claimable". | Contradicted at the time; unchanged since. |
| 6 | "dense ANN + FTS5 BM25 fusion" (README, OPERATIONS) | `sqlite_search.rs::dense_search_inner` loads every vector and sorts by exact cosine. | Mislabelled — exact k-NN, not ANN. |
| 7 | "local learning layer" / local-first | Embeddings are Gemini-API only (`retrieval/src/lib.rs`); ingest needs Anthropic/OpenAI key or `claude -p`; distiller needs codex/grok/claude CLI. Lesson text and every query are sent to Google. Store itself is local SQLite. | Contradicted as "local-first"; accurate as "self-hosted store, cloud embeddings". |
| 8 | Closed loop is live and recording | `fieldcraft stats` (2026-09-27, 7-day window): confirmed 61, ignored 8, contradicted 24; 4,143 records with `dynamic_confidence`; 18,835 retrievals with cue bundle. | Measured (plumbing live). Precision of events never audited (ROADMAP Phase 6 required ≥80% precision before scoring changes). `file_overlap` matches basenames only (`lib.rs`), so `main.rs`/`index.ts` collisions confirm unrelated lessons; `git diff` at hook time misses committed work and counts pre-existing dirty files. |
| 9 | "`dynamic_confidence` … not used in retrieval ranking" (README, ARCH, OPERATIONS) | `sqlite_search.rs::enrich_with_cue_bundles` sets `hit.confidence = COALESCE(dynamic, static)`; `compute_score` uses it at weight 0.05. | Slightly inaccurate; say "not the ranking authority". |
| 10 | "lessons get sharper with use" (installed agent instructions) | No ranking effect beyond #9; no precision audit. | Aspirational. |
| 11 | "~16,348 active lessons, ~8,955 merges, first cull archived 41, resurrection proven" | Owner's private store; plan changelog. Not reproducible by a reader. | Anecdotal/plumbing. |
| 12 | Lifecycle improves outcomes | No before/after measurement of retrieval quality or usefulness after eviction/RIF. | Plumbing-only. |
| 13 | "Dream G1 gate passed → dream unlocked" | Gate = harness in #1 (gold lessons, n≥10). It measures whether ideal lessons help, not whether LLM-derived claims are correct. | Weak gating logic; derived-claim quality unmeasured. |
| 14 | North Star "session lift ≥50% @30d" | Not computed anywhere; `stats` prints counts. | Aspirational. |
| 15 | "Phase tagging … +3.9pp on SWE-bench Verified (Shen et al. 2026, arXiv:2602.21611)" (ARCH §2) | Abstract reports "+4.7 pp on average (e.g., +6.8 pp on Gemini 2.5 Pro)" and the title is "…Subtask-Level **Memory**…". Fieldcraft's phase is a keyword heuristic; no LLM classifier. | Citation mismatch; mechanism only loosely analogous. |
| 16 | `eval/retrieval_usefulness/v2` (reveal precision 0.995) | Evaluates **skill-card** reveal policy, not lesson retrieval. | Not applicable to lesson claims. |
| 17 | Multi-agent support (Claude Code, Codex, Grok) | `install.sh`, `install/hooks/*.json`, CH-0006; verified by owner only. | Plumbing (works per docs). |
| 18 | Tests pass | `cargo test --workspace`: 427 passed / 0 failed / 1 ignored. | Measured. |
| 19 | "Supports multi-machine sharing via HTTP server with web UI" (`.claude-plugin/plugin.json`) | No web UI in the tree. Root `hooks/` also ships a legacy **blocking** Stop hook. | Contradicted / stale artifacts. |
| 20 | "No Python" (plan docs, CLAUDE.md rule) | `install.sh` embeds python3 for plist and env parsing; 30+ Python eval scripts in `scripts/`. | Contradicted (cosmetic, but public readers will notice). |
| 21 | ARCH §5 "requires OPENAI_API_KEY", §6 keychain enrollment, §4 "384-dim BGE" | Code: three extraction backends; Gemini 768-dim. | Stale docs. |
| 22 | Encoding-specificity / testing-effect grounding (Tulving & Thomson; Roediger & Karpicke) | Design rationale only; no experiment isolates cue-match contribution. | Aspirational framing; fine as "inspired by". |

Eval-design weaknesses to state plainly in public copy: synthetic gold sets (#2), tiny n (#1, #3, #5), no control arm or saturated control (#4), oracle lessons and push-mode injection (#1), single repo family, single seed, no precision audit of the automatic outcome signal (#8), no measurement of lifecycle or dream effects (#12, #13).

---

## 3. Competitive landscape (as of September 2026)

All rows are from vendor docs, READMEs, or papers fetched during this review; "not documented" means the fetched source did not describe it, not that it is absent. "Unverified" marks third-party-only claims.

| System | What is stored / who writes | Retrieval | Learns from outcomes? | Forgetting / lifecycle | Integration | Local / privacy | Multi-agent, multi-machine | Published evals | Sources |
|---|---|---|---|---|---|---|---|---|---|
| **Mem0 / OpenMemory** | LLM-extracted facts ("preferences, decisions, plans"), deduped, embedded, entity-tagged; automatic. | Vector + keyword + entity + temporal signals (platform); vector + optional rerank (OSS). | Platform has a per-memory `feedback` API (POSITIVE/NEGATIVE/VERY_NEGATIVE) "used to improve the accuracy of the memories and search results" — mechanism not specified; OSS docs describe none; no automatic attribution. | Explicit update/delete; no documented decay. | SDK, REST, MCP (OpenMemory) for Claude Code/Cursor/Windsurf. | OpenMemory local-first MCP; Platform is hosted; OSS needs your own vector/SQL stores. | Yes via MCP; multi-machine via platform. | LOCOMO-style benchmarks in paper; SWE-ContextBench (third party): FAIL_TO_PASS 20.2%, resolution 24.2%. | [How it works](https://docs.mem0.ai/core-concepts/how-it-works), [Feedback](https://docs.mem0.ai/platform/features/feedback-mechanism), [OpenMemory](https://mem0.ai/openmemory), [paper](https://arxiv.org/html/2504.19413v1), [SWE-ContextBench](https://arxiv.org/html/2602.08316) |
| **Letta (MemGPT)** | In-context "memory blocks" the agent edits via tools; archival/recall stores; "sleep-time" agents rewrite memory offline. | Agent-driven tool calls; archival search. | Not documented — no signal about whether a memory helped. | Agent rewrites; no automatic decay documented. | SDK/API, Context Repositories (git-versioned, Feb 2026). | Self-host or cloud. | Multi-agent shared blocks. | AIME/GSM sleep-time results (reasoning, not memory usefulness). | [Memory blocks](https://www.letta.com/blog/memory-blocks/), [Sleep-time](https://www.letta.com/blog/sleep-time-compute/), [Agent memory](https://www.letta.com/blog/agent-memory/) |
| **Zep / Graphiti** | Bi-temporal knowledge graph of entities/edges with validity intervals; LLM extraction. | Semantic + keyword + graph search. | No outcome loop documented; conflicts resolved by temporal invalidation. | Edges invalidated, not deleted. | SDK/API; Graphiti OSS. | Zep cloud; Graphiti self-host (Neo4j). | Yes. | DMR 94.8% vs MemGPT 93.4%; LongMemEval up to +18.5%. | [arXiv 2501.13956](https://arxiv.org/abs/2501.13956), [Graphiti](https://neo4j.com/blog/developer/graphiti-knowledge-graph-memory/) |
| **LangMem** | Semantic/episodic/procedural memory; **procedural = rewriting the agent's prompt** via metaprompt/gradient optimizers from conversation feedback. | LangGraph store search. | Yes, at prompt level: critiques from conversations rewrite instructions; no per-memory confidence. | Not documented. | Python SDK (LangGraph). | Your infra. | LangGraph agents. | None published. | [Launch post](https://www.langchain.com/blog/langmem-sdk-launch) |
| **cognee** | ECL pipeline → graph + vector; LLM entity/relation extraction; "memify" refinement. | 14 retrieval modes over graph+vector. | `improve` op "apply feedback … into the graph" — mechanism not specified. | `forget` (explicit delete). | Python/TS SDK, REST, MCP, Claude Code plugin. | Local-first, can run without API keys (local models). | Yes. | Vendor blog benchmarks. | [GitHub](https://github.com/topoteretes/cognee), [architecture](https://www.cognee.ai/blog/fundamentals/how-cognee-builds-ai-memory) |
| **Supermemory** | Facts (static/dynamic), user profiles, connectors (Drive, GitHub…); automatic extraction + contradiction resolution. | Hybrid, memory graph, "sub-300 ms". | Not documented. | Selective forgetting/contradiction resolution. | API, MCP, Claude Code + OpenCode plugins. | Hosted. | Yes. | SWE-ContextBench (third party): FAIL_TO_PASS 55.95%, resolution 30.30% (best of tested). | [supermemory.ai](https://supermemory.ai/), [SWE-ContextBench](https://arxiv.org/html/2602.08316) |
| **A-MEM** (research) | Zettelkasten-style atomic notes with LLM keywords/tags; links + "memory evolution" (adding a note revises linked notes). | Similarity over notes/links. | No. | No. | Research code. | — | — | Multi-hop QA gains; 85-93% fewer memory tokens vs baselines. | [arXiv 2502.12110](https://arxiv.org/abs/2502.12110) |
| **MemOS** | "MemCube" units for plaintext/activation/parameter memory with provenance, scheduling layer. | Hybrid retrieval. | Self-evolving claims; no per-memory outcome loop documented. | Scheduling/migration of cubes. | Python; MemOS server. | Self-host. | Yes. | Claims 35.24% token savings (README). | [arXiv 2507.03724](https://arxiv.org/abs/2507.03724), [GitHub](https://github.com/MemTensor/MemOS) |
| **Memori (GibsonAI)** | SQL-native entity/relationship extraction; intercepts LLM calls to inject/log. | SQL + "conscious" working memory promotion. | Not documented. | Not documented. | Python, any SQL DB. | Your DB. | Yes. | None. | [GitHub](https://github.com/GibsonAI/memori/blob/main/README.md) |
| **claude-mem** | Captures tool-use observations via 5 lifecycle hooks; AI-compressed summaries; SQLite + Chroma. | Hybrid semantic+keyword; progressive disclosure at SessionStart. | None documented. | None documented. | Claude Code plugin; also Codex, Gemini, Copilot, OpenCode, Hermes. | Local; hosted "CMEM Pro" optional; can use your Anthropic plan. | Multi-agent yes; multi-machine via hosted tier. | None. | [GitHub](https://github.com/thedotmack/claude-mem) |
| **basic-memory** | Human/AI-written Markdown notes → entities/observations/relations graph; SQLite index. | Note search (FTS/semantic). | No. | Manual. | MCP (Claude, Codex, Cursor, VS Code). | Local files (Obsidian-compatible). | Any MCP client; files sync via your tools. | None. | [GitHub](https://github.com/basicmachines-co/basic-memory), [docs](https://docs.basicmemory.com/start-here/what-is-basic-memory) |
| **ByteRover CLI (formerly Cipher)** | Curated "context tree" (`/curate "…" @file`), review/approve queue; Git-like branch/commit/merge/push/pull. | `/query` over the tree; results tagged local vs shared. | Not documented. | Manual. | CLI/REPL; earlier Cipher via MCP for many IDEs. | Local by default, optional cloud sync. | Team sharing via cloud. | None. | [GitHub](https://github.com/campfirein/byterover-cli), [Cipher npm](https://www.npmjs.com/package/@byterover/cipher) |
| **Claude Code auto memory** (native) | Claude writes `MEMORY.md` index + topic files per repo (`user`, `feedback`, … types); on by default. | First 200 lines / 25 KB of `MEMORY.md` loaded every session; topic files read on demand. | No usefulness signal; Claude is nudged to trim when near limits. | Manual/Claude edits; excluded from transcript cleanup. | Built in; plus CLAUDE.md/AGENTS.md rules. | Machine-local; "Files are not shared across machines or cloud environments." | Claude Code only; subagents get separate memory. | None. | [Claude Code docs](https://code.claude.com/docs/en/memory) |
| **Cursor Memories** | Sentence-sized facts proposed in background or on request; user approval before save (beta). | Auto-included in future chats. | No. | Manual. | Built in (settings → Rules & Memories); also Automations memory. | Per project, per user, not in repo, not versioned. | Cursor only; not shared with teammates. | None. | [localskills guide](https://localskills.sh/blog/cursor-memories-guide), [Cursor Automations](https://cursor.com/docs/cloud-agent/automations), [forum](https://forum.cursor.com/t/about-cursors-memory-record-feature/107355) |
| **Codex Memories + AGENTS.md** | Off by default; background summarization after idle (skips short sessions, redacts secrets) into `~/.codex/memories/`; AGENTS.md is static, ≤32 KiB. | Injected when `memories.use_memories` is on. | No outcome feedback documented (community requested "outcome-aware" selection, Jul 2026). | Not addressed in docs; third-party blogs mention "diff-based forgetting" (unverified). | Built into Codex CLI/app. | Device-local. | Codex only. | None. | [Codex memories doc](https://learn.chatgpt.com/docs/customization/memories.md?surface=app), [discussion #12567](https://github.com/openai/codex/discussions/12567) |
| **Windsurf Cascade Memories** | Auto-generated facts during chat or on request; stored in `~/.codeium/windsurf/memories/`. | Referenced automatically. | No. | Manual. | Built in. | Local, workspace-scoped, not committed. | Windsurf only. | None. | [Windsurf docs](https://docs.windsurf.com/plugins/cascade/memories) |
| **GitHub Copilot Memory** | Repo facts with **code citations** + user preferences; created by agents as they work; public preview, on by default for Pro/Pro+. | Citations re-validated against the current branch before use. | Partial: unused entries expire after 28 days and the timer resets on successful validation+use — usage-aware, not outcome-aware. | 28-day unused expiry; admin management. | Built into coding agent, code review, CLI, autofix. | Hosted by GitHub; repo-scoped. | Shared across Copilot agents; org-owned on business plans. | None. | [Concept doc](https://docs.github.com/en/copilot/concepts/agents/copilot-memory), [changelog](https://github.blog/changelog/2026-03-04-copilot-memory-now-on-by-default-for-pro-and-pro-users-in-public-preview/) |
| **Hindsight (Vectorize)** | LLM-extracted facts → consolidated "observations" with evidence tracking; user-curated "mental models". | TEMPR: semantic+keyword+graph+temporal in parallel, RRF + reranker; `reflect()` reasons over memory. | Not documented. | Consolidation; no decay documented. | Python/TS/Go/CLI/HTTP, MCP. | Cloud or self-host (Docker/Helm/pip). | Yes. | LongMemEval SOTA claims, +44.6 pts over full-context (vendor). | [Overview](https://hindsight.vectorize.io/), [paper](https://arxiv.org/pdf/2512.12818) |
| **agentmemory (rohitg00)** | 4-tier consolidation (working→episodic→semantic→procedural) from 12 auto-capture hooks; LLM compression optional. | BM25 + vector + graph RRF; keyword-only without embeddings. | Not documented. | TTL, importance eviction, "auto-forget", contradiction resolution. | 54 MCP tools, REST, Claude Code/Cursor/Codex/OpenCode plugins. | Local. | Yes. | LongMemEval-S R@5 95.2% (self-reported); in-house coding benchmark. | [GitHub](https://github.com/rohitg00/agentmemory) |
| **Research: Reflexion / ExpeL / AWM / ReasoningBank / ACE / "Learning on the Job"** | Verbal reflections, distilled insights, induced workflows, or evolving "playbooks"; ReasoningBank extracts from successes **and failures** with an LLM judge; "Learning on the Job" distils episodes into rules updated by outcome verdicts/corrections. | Similarity over memory items injected into the prompt. | **Yes** — the defining feature; judgments by LLM-as-judge or environment verdicts. ACE notes methods degrade without reliable feedback. | Curation loops ("insight governance"). | Research code. | — | — | ReasoningBank: WebArena +8.3%, SWE-bench Verified +4.6%, ~3 fewer steps; "Learning on the Job": 1.6×/2.6× on τ-bench. | [ReasoningBank](https://research.google/blog/reasoningbank-enabling-agents-to-learn-from-experience/), [ACE](https://arxiv.org/abs/2510.04618), [Learning on the Job](https://arxiv.org/abs/2607.22157), [Insight governance](https://arxiv.org/abs/2606.17591), [Subtask-level memory](https://arxiv.org/abs/2602.21611) |

Landscape takeaways
- The market splits into (a) hosted memory APIs for chat/product agents (Mem0, Zep, Supermemory, Hindsight, cognee), (b) IDE/agent-native note systems (Claude Code auto memory, Cursor, Codex, Windsurf, Copilot), (c) hook/MCP plugins for coding agents (claude-mem, agentmemory, basic-memory, ByteRover), and (d) research on learning from outcomes (ReasoningBank, ACE, "Learning on the Job").
- Almost nobody in (a)-(c) closes the loop on **whether a served memory was used and whether it helped**. Closest: Mem0 Platform's explicit feedback API (human thumbs), Copilot's usage-reset 28-day expiry and citation revalidation, cognee's `improve`. None documents automatic transcript-derived attribution with per-record confidence and retrieval-induced forgetting. The research track (d) does this but is not packaged for daily coding-agent use.
- SWE-ContextBench (independent) is the sharpest external reference for coding-agent memory: correctly selected context lifts resolution 26.3% → 34.3%, free selection barely helps, and unfiltered context can hurt. This is exactly the retrieval-precision problem Fieldcraft's outcome loop is meant to fix — and exactly the metric Fieldcraft has not yet demonstrated.

---

## 4. Honest differentiation

### Where Fieldcraft is genuinely different (mechanism-grounded)
1. **Outcome instrumentation is built into the write, serve and stop path.** Every retrieval is logged with the query's cue bundle and session id; every session end is scanned for citations, contradictions and touched files; every served record accumulates `lesson_outcome_events` and a `dynamic_confidence`. No competitor in the table does automatic per-record attribution from the agent's own transcript plus `git diff`. Caveat that must stay in the copy: the signal is recorded, only weakly used in ranking, and its precision is unaudited.
2. **Lessons, not transcripts.** Records are typed (`claim | procedure | episode | directive | context`) with structured payloads (`affected_files`, `invalid_if`, `steps`, `retrieval_questions`), a phase, and deterministic cue bundles. Most plugins store compressed observations or free-text facts. This gives Fieldcraft file-scoped and phase-scoped ranking features others lack.
3. **A store that forgets, reversibly, and can un-forget.** Protection classes, usage-grounded eviction, query-triggered resurrection, retrieval-induced-forgetting of losing near-twins, and a provenance-gated generative consolidation tier that lands `provisional` and demotes its sources only after a real confirm. Copilot has a 28-day expiry; Zep invalidates edges; nobody else in the table has resurrection or RIF.
4. **CLI + hooks, no MCP, vendor-neutral, one SQLite file, optional shared server.** Works identically for Claude Code, Codex and Grok because the agent just runs a binary; a shared server on a private network gives every machine and every agent the same store with per-client tokens. No IDE lock-in, no hosted account.

### Where it is weaker
- Retrieval needs a Google Gemini API key; without it the system silently degrades to BM25 and the vector-dependent machinery (merge, RIF, dream, resurrection) is inert. Not local-first; queries leave the machine.
- No public evidence that the closed loop improves retrieval or outcomes; ranking-quality regressions are documented (CH-0004: the motivating memory still ranked second after deploy).
- Installer is macOS-only (launchd, python3 plist, Homebrew paths) and edits shell rc files; no Linux/Windows path.
- Security defaults assume a private network (open `/enroll`, `0.0.0.0` bind); the distiller runs a coding agent with sandbox/approvals bypassed on transcript text.
- Single-maintainer dogfood; benchmarks are small, synthetic or oracle-shaped; docs drift (BGE vs Gemini, MCP mentions, web UI).
- Scope creep in the binary (personal "memories/people/actions" API, skills subsystem) blurs the pitch.

### When not to use Fieldcraft
- You want memory for a chat/product agent (user preferences, conversation facts): use Mem0, Zep, Supermemory or Hindsight.
- You need a hosted, zero-ops service with SLAs: Fieldcraft is self-hosted, single-binary, no auth beyond bearer tokens.
- You cannot send lesson text/queries to a third-party embedding API: wait for a local-embedding option.
- You only use one IDE and its native memory is enough (Claude Code auto memory, Copilot Memory, Cursor Memories) and you do not care about outcome tracking or cross-machine sharing.
- You want human-curated, git-versioned project docs for agents: ByteRover CLI or basic-memory fit better.
- You need Linux/Windows today, or Python/TypeScript SDK integration rather than a CLI.

---

## 5. Improvements (prioritized)

Impact H/M/L, effort S/M/L, evidence = what in the repo motivates it.

### (a) Must-fix before public launch
| # | Item | Impact | Effort | Evidence |
|---|---|---|---|---|
| A1 | Strip private infrastructure from tracked files: tailnet hostname, ssh alias, server username, laptop name, owner's app names in tests (`crates/fieldcraft-cli/src/main.rs:3636-3795`, `README.md`, `ARCHITECTURE.md`, `STATUS.md`, `docs/OPERATIONS.md`, `change_history/CH-0003`). Re-run a grep for the private host, user and machine names after the scrub. Consider whether the eval raw logs (437 `raw_log.txt` files with `/Users/<user>/...` paths and full agent transcripts) may be published at all. | H | S | §2 #19-21; `git grep` in review |
| A2 | Repo hygiene: 3,342 tracked files, 1,803 under `eval/` and 1,406 under `benchmarks/` (~227 MB, tracked SQLite DBs, per-run JSON at repo root, `benchmarks/` deleted in the working tree but still tracked). Move artifacts to a release asset or separate repo; either rewrite history (`git filter-repo`) or publish from a fresh repo, because history carries the private paths regardless of HEAD. | H | M | `git ls-files` counts, `git status` (1,406 D) |
| A3 | Embedding dependency must be explicit: `install.sh` never mentions `GEMINI_API_KEY`; `fieldcraft query` warns and degrades to BM25; writes never get vectors, so merge/RIF/dream/resurrection silently do nothing. Add a preflight check, a `fieldcraft doctor` line, and either (i) a local embedding option (fastembed/ONNX) or (ii) an explicit "BM25-only mode" with the vector features disabled loudly. Update ARCHITECTURE §4/§5/§7 (BGE-384, fastembed, 96 ms). | H | M | `crates/fieldcraft-retrieval/src/lib.rs`, `retrieval.rs:1318` |
| A4 | Installer portability and side effects: launchd + python3 `plistlib` + Homebrew paths (macOS only); appends loaders to `.zshrc/.zprofile/.bashrc/.bash_profile`; always installs a local HTTP service even for one machine; injects into `~/.claude/CLAUDE.md` and `~/.codex/AGENTS.md`. Ship a Linux path (systemd user unit or plain `--data-dir` local mode), make the local service optional, document exactly what is modified, and drop the embedded Python (jq/awk suffice). | H | M | `install.sh` |
| A5 | Security defaults: `POST /enroll` is unauthenticated (rate-limited) and the authoritative server binds `0.0.0.0` — anyone on the network can mint a token and read/write the whole store. Bind `127.0.0.1` by default, require an enrollment secret or one-time code for non-local binds, document the private-network assumption. Distiller default should not be `codex exec --dangerously-bypass-approvals-and-sandbox` on transcript text (prompt-injection surface); default to `none` or a read-only sandbox and make the bypass opt-in. | H | S-M | `crates/fieldcraft-server/src/lib.rs:170-175`, `main.rs:131`, `install/bin-hooks/stop-feedback.sh` |
| A6 | Remove or reconcile stale artifacts: `.claude-plugin/plugin.json` ("web UI"), root `hooks/` (legacy blocking Stop hook, referenced by CI shellcheck and CONTRIBUTING), CONTRIBUTING's `OPENAI_API_KEY`-for-tests note and `hooks/` description, broken link `docs/research/human-memory-recall.md` (directory is empty), ROADMAP "CH-NNNN" placeholder. | M | S | files listed |
| A7 | Fix overclaims in public-facing text: "dense ANN" → exact cosine; "local learning layer" → self-hosted store + cloud embeddings; shadow-mode wording (§2 #9); Shen et al. numbers/title (§2 #15); "lessons get sharper with use" in the installed agent instructions → "outcomes are recorded". | H | S | §2 |
| A8 | Decide product scope: the `/memories`, `/people`, `/actions`, `/shapes`, `/views/*` API (with `audio_ref`) and the skills subsystem are undocumented in README. Either document them as experimental or feature-gate them out of the public build. | M | S-M | `crates/fieldcraft-server/src/memories.rs`, `schema.rs:242-330` |
| A9 | Single-machine observability: `fieldcraft stats` hits `/stats` and the docs tell users never to trust the local DB; a new user has one machine. Make `stats` work against the local catalog and print embedding status, hook status and last trace-analyze result. | M | S | `docs/OPERATIONS.md`, `main.rs::cmd_stats` |
| A10 | Correctness risk: a `file_overlap` CONFIRMED (basename match) can promote a provisional dream-derived claim to active and demote its sources. Restrict promotion to citation-source confirms, or require ≥2 confirms from distinct sessions. | H | S | `repository.rs::apply_feedback_outcome`, `lib.rs::trace_analyze_handler` |

### (b) Make the differentiation provable
| # | Item | Impact | Effort | Evidence |
|---|---|---|---|---|
| B1 | Precision audit of outcome events: hand-label ~200 recent CONFIRMED (split by source: citation vs file_overlap), IGNORED and CONTRADICTED events; publish precision per source. Then weight `file_overlap` down or require full-path match / ≥2 files / non-generic basenames, and include committed files (`git log --since=<session start> --name-only`) rather than only the dirty tree. | H | M | ROADMAP Phase 6 gate (≥80%) never executed; `stats` shows 24 contradicted vs 61 confirmed in 7 days — worth checking what those contradictions are |
| B2 | Take `dynamic_confidence` out of shadow safely with online interleaving: alternate ranking arms (static vs dynamic multiplier) per query, tag the arm in `retrieval_log`, and compare confirm rate per arm over ~2-4 weeks. This yields a defensible number without a benchmark. Keep the env gate as the kill switch. | H | M | `retrieval.rs::apply_dynamic_confidence_multiplier` |
| B3 | A public, reproducible benchmark that matches the product path: pull-mode (agent runs `fieldcraft query` via the real hooks), lessons produced by the real distiller from earlier tasks in the same repo stream (not gold), ≥2 repo families, ≥3 runs per arm, a memory-suppressed control, paired bootstrap CIs, published task manifest and runner with no private paths. Report steps/tokens/resolution and the SWE-ContextBench-style "wrong context hurts" regression count. `docs/fieldcraft-eval-breakthrough-protocol.md` already specifies most of this; it just has not been run. | H | L | §2 #1, #5; protocol doc |
| B4 | Golden retrieval eval (recall@30, MRR@10, precision@5, noise@5, abstain accuracy) with real positive and negative queries, including the CH-0004 ingredient-display regression as a fixture. Gate scoring changes on it in CI. | H | M | ROADMAP "Immediate retrieval-quality follow-up"; not built |
| B5 | Compute and print the North Star (session lift = share of sessions with ≥1 confirmed retrieval, plus mean rank of confirmed items) in `fieldcraft stats`, so the README can show a real number. | M | S | ROADMAP North Star; `stats` prints counts only |
| B6 | Wire `citation_match.rs` as a second CONFIRMED source (`distinctive_token`) with its own audit line; it catches paraphrased use that the `lesson:<id>` regex misses. | M | S | `citation_match.rs` unused |
| B7 | Dream quality gate: a review queue (`fieldcraft dream --review`) that shows derived claims with their sources and lets a human accept/reject before they can be promoted; measure acceptance rate. The current G1 gate measured gold lessons, not dream output. | M | M | §2 #13 |
| B8 | Publish a small "outcome dataset" (anonymized query cue bundle, record cue bundle, outcome) as it accumulates — it is the raw material for the planned learned ranker and a credible artifact no competitor ships. | M | S | ROADMAP long-term |

### (c) Longer-term
| # | Item | Impact | Effort |
|---|---|---|---|
| C1 | Take the Gemini call out from under the repository mutex (`spawn_blocking` before locking) and cache vectors in memory instead of re-reading every row per query; add an ANN index only if stores exceed ~100k records. | M | M |
| C2 | Pluggable embedding providers (local ONNX, OpenAI, Voyage) with model tagging on vectors (dimension check already exists). | H | M |
| C3 | LLM cue classifier for `phase`/`user_state` (currently keyword heuristic, `user_state` always None), then re-evaluate the cue-match weights (0.40 phase / 0.15 user_state assume both are populated). | M | M |
| C4 | Time decay of confidence; co-retrieval graph (outcome-weighted PPR); learned ranker once ≥5k triples. | M | L |
| C5 | Team features: per-project token scopes, read-only tokens, Docker image, systemd unit, backup/restore command. | M | M |
| C6 | Trim the CLI surface for the public build (`eval`, `learn`, `external-skills`, `re-extract`, `backfill-*`, `queue/claim/run/complete/fail`) behind a `--features` flag or a separate binary; 6k-line `main.rs` is a contributor barrier. | M | M |

---

## 6. Overclaim risks to remove from public copy (short list)
1. Any sentence implying Fieldcraft already learns from outcomes or that lessons "get sharper with use". Correct: outcomes are recorded per record; ranking uses them only marginally; this is shadow mode.
2. "Fewer mistakes / fewer tokens" as a general claim. Correct: one oracle-lesson study on 19 pytest tasks showed ~19% fewer steps; resolution and tokens were not significant.
3. "Local-first" / "local learning layer". Correct: local SQLite store; embeddings via Gemini API; distillation via your coding-agent CLI.
4. "Dense ANN". Correct: exact cosine over all vectors.
5. "Dream passed its gate". Correct: the dream tier is built and provenance-gated; its output quality is not yet measured.
6. Latency and eval numbers from the BGE era.
7. Anything citing the owner's store size, merge counts or production cull as evidence.
