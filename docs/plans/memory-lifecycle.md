# Memory Lifecycle Plan — drop, consolidate, dream

## Status
- Owner: Radu
- Last updated: 2026-07-02
- Current phase: **L1-L4 LIVE on the authoritative Mac Mini server** (scheduled eviction applying,
  resurrection proven end-to-end in production). L5 (`fieldcraft dream`) is built and deployed but
  the G1 gate PASSED 2026-07-02 (n=19; steps -3.47, CI [-6.58,-0.53]); first supervised `--apply` ran: 3 provisional derived claims. See Change Log.
- Parent: this is the lifecycle track of the larger Fieldcraft Usefulness Plan (delivery / shrink /
  sharpen / learn). It owns the "forget + consolidate + dream" half.
- Decision log: see Change Log.

## Problem

The store only grows (16,348 active lessons, ~8,955 near-duplicate merges, nothing ever culled). It
cannot drop dead weight, cannot consolidate beyond exact near-duplicates, and cannot generate new
abstracted knowledge from what it has seen. Three reasons, each with an existing primitive that is
inert or missing:

1. **Drop.** The only cull signal, `dynamic_confidence`, is monotonically inflated (confirms
   asymptote up, floor 0.05), so any confidence-keyed kill list is permanently empty. The registrar
   already diagnoses ghosts but nothing archives. There is no protection flag, so nothing can be
   safely excluded from a cull.
2. **Consolidate.** The merge worker handles only near-identical pairs (cosine >= 0.92). Related-but-
   not-identical clusters, and "A keeps beating its near-twin B" competition, are never resolved.
3. **Dream.** No offline pass abstracts new lessons from clusters or folds contradictions into
   preconditioned rules. This is the highest-value and highest-risk capability (hallucinated
   generalizations, the DRM false-memory failure mode), so it must be gated, not skipped.

## Scope

Drop (usage-grounded, reversible, protected): served-activation signal, retention/protection,
conservative auto-archive with dry-run, resurrection. Consolidate beyond merge: retrieval-induced
forgetting (demote the loser of an observed retrieval competition). Dream: gated generative
consolidation that abstracts new provisional lessons with provenance.

## Non-Goals / guardrails (hard rules for every packet)

- Non-destructive only. Plain archive is a reversible `tier='archive'` + `knowledge_events` audit
  row. Supersession is a provenance tombstone (`state='superseded'`, `superseded_by=<canonical>`)
  that never returns to the serving pool. No DELETE anywhere.
- Do not touch the merge worker (`repository.rs:2014`); it is correct and conservative. No more
  aggressive merge (Mem0 reverted aggressive reconcile for destroying context).
- The generative dream tier ships ONLY behind the outcome harness (G1), with provenance (a derived
  lesson cites >= 3 source episodes), lands `provisional`, activates only after earning a real
  confirm. No exceptions. Originals are retained in the DB (never DELETEd) but are demoted
  (`state='superseded'`, `tier='archive'`, `superseded_by=<derived id>`, audited, non-serving) at
  the moment the derived claim is promoted to active — never at synthesis time. Rationale
  (Baseten Compile-26, iterative-compaction collapse): compressed memory must prove itself on
  SUBSEQUENT use before it replaces its sources. Synthesis is the primary operation of dreaming;
  dedupe of near-twins falls out of it as a side effect.
- Protected classes (explicit pin, `kind in {directive, context}` that are rare, static confidence
  >= 0.9) are never auto-archived.
- Eviction keys on usage and a conjunction, never on confidence alone.
- Agents do not commit. They implement, run their own tests, and leave the diff for the orchestrator
  to validate (build + tests + diff) before any commit.

## Architecture

Five loops already exist in the code as primitives: `tier`/`state` lifecycle, the
`knowledge_events` audit log, the merge worker's archival mechanism, `retrieval_log.result_ids` (the
served history), and `lesson_outcome_events` (confirm/contradict). This track connects them. The
offline maintenance pass reuses the existing 60s worker cadence (`lib.rs:359`); do not add a second
scheduler. The dream tier is the only genuinely new machinery, and it is gated.

## Work packets and dependency order

Files are concentrated, so waves are dependency-ordered. Parallelism only where file-disjoint.

### Wave 1 (foundation, parallel, file-disjoint, no harness dependency)

- **L1 — storage foundation.** Owns `fieldcraft-storage` (schema.rs, repository.rs) and
  `fieldcraft-core` (lib.rs). Does NOT touch server, cli, or retrieval.
  - Retention/protection: add a `protected` boolean (idempotent migration, follow the pattern at
    `schema.rs:151-177`) to the `knowledge` table and `KnowledgeRecord`; an `is_protected(id)` that
    returns true for explicit pin, rare `directive`/`context`, or static `confidence >= 0.9`; a
    `set_protected(id, bool, reason)` that writes an audit row.
  - Served-activation signal: `served_activation(id) = ln(Σ over each appearance of id in
    retrieval_log.result_ids of max(1, days_since(that retrieval))^-0.5)`; never-served returns
    `None` (the eviction sentinel). Name it served_activation, not usage: served is not proof of use.
  - `archive_record(id, reason)` / `restore_record(id, reason)`: reuse the archival mechanism
    (`repository.rs:773`, set `tier='archive'`, write a `knowledge_events` audit row); restore flips
    plain archives back, audited. Superseded rows are canonical tombstones and restore fails loud.
    Never DELETE.
  - Unit tests for every function. Run `cargo build -p fieldcraft-storage` and
    `cargo test -p fieldcraft-storage`; report output. No commit.

- **G1 — the outcome harness (the gate for the dream tier).** Self-contained under `eval/`. Does NOT
  touch `crates/*`. NO PYTHON (bash + a small Rust binary).
  - Warm-vs-cold held-out outcome harness on the existing `eval/oracle_validation_v1` scaffold: run
    each held-out task memory-served vs memory-suppressed, diff resolved/steps/tokens with a paired
    bootstrap 95% CI; difficulty-calibrate (drop tasks where cold success is 0% or 100%); hold out
    >= 10 tasks. Fix the documented Gauntlet failures (ID-format mismatch, tasks too easy) in
    `eval/RESULTS.md`.
  - Real runs, no mocks. Report a real preliminary number with its CI and state coverage honestly
    (what actually ran). No commit.

### Wave 2 (depends on L1; dispatch after L1 is validated) — SHIPPED, LIVE

- **L2 — eviction loop.** SHIPPED. `archive_candidates` (`repository.rs:1029`) implements the
  conjunctive predicate: `age > 60d AND never served AND confirmed_events == 0 AND NOT is_protected`
  (ghost candidates), plus `contradicted >= 3 AND contradicted > confirmed AND NOT is_protected`
  (contradicted-losers). The `fieldcraft lifecycle` CLI (`--apply` gated, dry-run default) and the
  server's `lifecycle_loop` (`fieldcraft-server/src/lib.rs:380`, 6h cadence via
  `FIELDCRAFT_LIFECYCLE_INTERVAL_SECS`) both call it. The server loop only archives when
  `FIELDCRAFT_LIFECYCLE_APPLY=1` — set in the Mac Mini launchd plist. First production pass archived
  41 records, reversible and audited.
- **L3 — resurrection guard.** SHIPPED. After every live retrieval the server spawns a background
  task (`tokio::spawn`, never on the response path) that scans plain `tier='archive'` records
  against the query embedding, excluding superseded tombstones. Originally targeted cosine >= 0.93; live measurement on the production store
  showed identical-text queries only reach ~0.927 against cue-enriched record embeddings, so the
  shipped default is `FIELDCRAFT_RESURRECTION_COSINE=0.85` (env-tunable). 3 hits (env-tunable via
  `FIELDCRAFT_RESURRECTION_THRESHOLD`) auto-restore via `restore_record`. `FIELDCRAFT_RESURRECTION=0`
  disables the whole feature. Proven end-to-end on production (3 `archive_match` events -> auto
  restore fired for real).

### Wave 3 (consolidation tier 2; depends on L1/L2) — SHIPPED, LIVE

- **L4 — retrieval-induced forgetting.** SHIPPED. `rif_demotion_candidates` (`repository.rs:1155`)
  finds near-twin pairs (dense cosine `[0.85, 0.92)`, below the merge threshold) that co-occur in
  `retrieval_log` at least 3 times, where one side has strictly more `confirmed` outcomes than the
  other; the loser is proposed for reversible archival. Protected records and pair-winners are never
  proposed. Wired into both the CLI `lifecycle` command and the server's `lifecycle_loop` alongside
  L2 — one conjoined candidate set, deduped, one archival pass.

### Wave 4 (the dream tier; GATED behind G1; dispatch only after G1 is trusted) — BUILT, `--apply` GATED

- **L5 — generative consolidation (dreaming).** BUILT and deployed as `fieldcraft dream`, dry-run by
  default. `dream_clusters` (`repository.rs:1347`) clusters same-project/same-kind lessons via
  deterministic CPM community detection over pairwise cosine `[0.80, 0.92)`, requiring >= 3 members and >= 2 confirms. With
  `--apply`, an LLM (default `codex exec --sandbox read-only`, overridable via `--llm-cmd`) abstracts
  ONE derived Claim per cluster from a prompt built over member titles/retrieval_text. Guardrails:
  the derived claim must cite >= 3 sources (an audited `derived_from` event per source), lands
  `state='provisional'`, and originals are retained untouched at synthesis time; unusable LLM output
  (empty/unparseable JSON) is skipped, not force-written. A real `confirmed` outcome on a provisional
  record promotes it to `active`, writing a `promoted_active` audit event — the only way a derived
  claim earns its place. **Demote-on-promotion (synthesis-first dedupe):** when that promotion fires
  on a derived claim, its `derived_from` sources are demoted via the merge archival mechanism
  (`state='superseded'`, `tier='archive'`, `superseded_by=<derived id>`, one `source_demoted` audit
  event per source) so the proven canonical becomes the sole serving representative. Sources that
  are protected (`is_protected`) or already superseded/archived are skipped. Demotion is a canonical
  replacement, not a reversible archive: the L3 resurrection guard skips superseded rows. If the
  old source becomes true again, write or promote a new current canonical memory instead.
  Dry-run on production found 3 clusters. `--apply` remains gated on the G1 n>=10 usefulness sweep,
  which has not yet run at full scale (infrastructure and cold-arm isolation are proven; the sweep
  itself is in progress).

## Testing Plan

- L1: unit tests per function; migration idempotency test; archive then restore round-trips; assert
  no DELETE.
- G1: real warm/cold run on the held-out set; published delta + bootstrap CI; honest coverage note.
- L2: dry-run set reviewed against `fieldcraft analyze`; protected records never appear; zero
  DELETE; audit row per archive.
- L3: a resurrection fires in a test; active retrieval latency unchanged (separate path).
- L4: demotion only when the competition is real and the loser is reversible.
- L5: derived lesson has >= 3 cited sources, lands provisional, does not go active without a confirm,
  and does not regress the G1 outcome delta. Demote-on-promotion: sources are demoted only when the
  derived claim is promoted (never at synthesis time), protected sources are skipped, a second
  confirm does not re-fire demotion, promotion of a non-derived provisional record demotes nothing,
  and every demotion is audited while staying out of the serving pool.
- No "green" without naming what ran. Thresholds (age, cosine bands, decay exponent) tuned on real
  data, not guessed.

## Definition Of Done

- Active-lesson count flat or declining over a month, zero data loss (reversible + audited).
- Eviction keys on served-activation + a conjunction, never on confidence; protected classes are
  safe; resurrection self-corrects over-forgetting (fires in a test).
- The outcome harness (G1) produces a real warm/cold delta with a CI and gates the dream tier.
- The dream tier ships provisional-only with provenance and does not regress the outcome delta.

## Agent Handoff Packet (shared)

- Repo: fieldcraft, branch feat/feedback-loop-shadow. Rust + SQLite, NO PYTHON.
- Forbidden regressions: no DELETE; do not touch the merge worker; no generative tier without G1 +
  provenance + provisional; protected classes never archived; do not commit.
- Anchors: knowledge DDL `schema.rs:321`; migration pattern `schema.rs:151-177`; merge archival
  `repository.rs:773`; merge thresholds `repository.rs:2014`; outcome update `repository.rs:67`;
  KnowledgeRecord `crates/fieldcraft-core/src/lib.rs:570-604`; maintenance cadence `lib.rs:359`;
  registrar/ghosts `main.rs:4980,5297`; eval scaffold `eval/oracle_validation_v1/`; Gauntlet
  post-mortem `eval/RESULTS.md`.
- Each agent returns: what changed (files), test command output, deviations from this plan, and any
  blocking discovery. The orchestrator re-runs the workspace build + tests and reads the diff before
  any commit. Never trust a sub-agent's "done" or "green."

## Change Log
- 2026-06-30: Created from the consolidation-and-dreaming discussion; sequenced into 4 waves; Wave 1
  (L1 storage foundation + G1 outcome harness) dispatched to Sonnet 5 sub-agents, file-disjoint.
- 2026-07-01: L2/L3/L4 LIVE on the authoritative server: scheduled eviction enabled
  (FIELDCRAFT_LIFECYCLE_APPLY=1, 6h cadence), first production cull archived 41 records
  (reversible, audited), resurrection wired into live retrieval and proven end-to-end on
  production (identical-text query measures ~0.927 vs cue-enriched record embeddings, so the
  resurrection default is calibrated to 0.85; 3-hit counter guards restores). L5 `fieldcraft
  dream` built + deployed, dry-run proven on production (3 clusters); --apply remains gated on
  the G1 n>=10 usefulness sweep (running).
- 2026-07-02: G1 gate PASSED — full sweep n=19, warm-cold steps delta -3.47 (95% CI
  [-6.58, -0.53], significant), resolved +5.3pts (CI [0, +15.8]), tokens -40.7k (n.s.),
  cold contamination 0/19, warm source-hit 19/19 (eval/usefulness_harness/RESULTS.md).
  First supervised `fieldcraft dream --apply` on production: 3 derived claims (provisional,
  full-cluster provenance). Also fixed: codex exec invocation needed --skip-git-repo-check
  + null stdin (was silently failing as "no usable claim").
- 2026-07-02 (b): Dreaming goes AUTOMATIC: nightly launchd job com.fieldcraft.dream on the
  authoritative server, 00:10 local (Europe/Berlin = least-used window), `fieldcraft dream --apply
  --max-clusters 3`, logs to ~/.fieldcraft/reports/nightly-dream.log. Added the nightly-idempotence
  guard: a cluster any of whose members already sources a derived claim is skipped, so daily runs
  never mint near-duplicate uber-lessons. Installer: scripts/install-nightly-dream.sh.
- 2026-07-02 (c): Design decision (Radu): dreaming is synthesis-first; dedupe falls out of it.
  Softened the "originals retained untouched" guardrail to demote-on-promotion: when a derived
  claim earns promotion to active, its sources are superseded/archived (audited, non-serving) so
  the canonical becomes the sole serving representative. Never at synthesis time — per the
  Baseten Compile-26 iterative-compaction finding, compressed memory is validated on subsequent
  use before replacing its sources. Recorded as fieldcraft lesson e5abeac2.
