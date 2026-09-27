# Fieldcraft Learning Framework

This document is the canonical source for how fieldcraft improves over time.
It defines the durable system pillars and the controlled improvement path.

## 0) Core Objective

Make `fieldcraft` consistently improve agent outcomes under context limits across coding and non-coding tasks while:

- keeping lesson injection token-cost bounded,
- avoiding false recall and cross-task noise,
- improving repeatable warm-start performance versus cold and control baselines.

## 1) Pillar Architecture (End-to-End)

Fieldcraft is optimized as five coupled pillars.

### Pillar 1: Capture
Turn raw session activity into structured, reusable signals.

- Stable contracts:
  - structured extraction of summary + intent + outcome + confidence,
  - durability signal grading (`low` / `medium` / `high`),
  - explicit cue fields (`cue_problem`, `cue_action`, `cue_verify`, `keywords`, `files`, `commands`).
- Improvement space:
  - richer domain inference,
  - less noisy summaries,
  - better long-tail abstraction templates.

### Pillar 2: Storage
Persist lessons in a schema that supports useful retrieval and cleanup.

- Stable contracts:
  - lane-based persistence (`sessions`, `decisions`, `patterns`, `bugs`, imported sessions),
  - deterministic parseable note format used by optimizer/evaluators,
  - project scope default with explicit cross-project transfer path.
- Improvement space:
  - new abstraction lanes,
  - schema extensions for novel task classes,
  - migration-safe metadata additions.

### Pillar 3: Recall
Find the right lessons quickly and safely.

- Stable contracts:
  - specificity gradient (broad → balanced → specific),
  - dense + sparse hybrid retrieval,
  - reranking by utility, alignment, recency, distinctiveness, and actionability,
  - diversity filtering,
  - strict token budget enforcement.
- Improvement space:
  - query synthesis quality,
  - candidate scoring calibration,
  - alignment behavior for mixed-domain sessions.

### Pillar 4: Learning & Hygiene
Improve system behavior from repeated use.

- Stable contracts:
  - every change is benchmarked and recorded,
  - promotion requires measurable evidence and stability checks,
  - periodic cleanup/rewrite is part of lifecycle.
- Improvement space:
  - better consolidation logic,
  - cluster-based abstraction from repeated sessions,
  - lesson retirement/decay rules for stale content.

### Pillar 5: Delivery & Client Integration
Guarantee that lessons are actually usable by agents and tools.

- Stable contracts:
  - client-facing command contract (`write`, `query`, `ingest`, `cleanup`, `status`) stays stable,
  - install path remains "one command" usable for humans,
  - local/server/client modes remain consistent.
- Improvement space:
  - onboarding ergonomics,
  - client hook templates for more LLM environments,
  - stronger end-to-end smoke tests for non-Claude/Codex paths.

## 2) Durable Invariants (Don’t change often)

These are intentionally hard to change. Any break requires a specific architecture milestone.

1. `project-local` is default; cross-project recall is additive and opt-in.
2. Lesson output is always bounded by token budget.
3. The same lesson should not depend on code-specific artifacts to be useful.
4. Progress is validated on both retrieval-level and agent-level outcomes.
5. Benefits must clear warm-vs-cold and warm-vs-control checks.

## 3) Evaluation Backbone (required for every significant change)

### Gates
- `cargo test --workspace` — all unit and integration tests pass
- `scripts/run-pre-push-e2e.sh` — end-to-end smoke test (write + query round-trip)
- results recorded in `eval/RESULTS.md`

### Hard gates
- no regressions in write quality,
- token budget compliance unchanged or improved,
- warm-vs-cold pass rate not degraded relative to stable baseline,
- warm-vs-control uplift remains positive,
- no regression in repeated-run median/variance,
- timeout and failure rates remain bounded.

If any hard gate fails, mark as `iterate` and rollback non-essential changes.

## 4) Pillar-by-Pillar Iteration Playbook

### Capture iterations
- Improve extraction quality and durable classification without changing output schema.
- Validate with unit tests and smoke test round-trip.

### Storage iterations
- Add or refine storage categories while maintaining backward compatibility.
- Validate with schema migration tests and cleanup reruns.

### Recall iterations
- Adjust ranking, query synthesis, or reranking weights.
- Validate with retrieval unit tests and `eval/RESULTS.md` baseline.

### Learning & hygiene iterations
- Add cleanup, rewrite, or consolidation behavior.
- Validate by comparing retrieval quality, token footprint, and warm-vs-cold gains.

### Delivery and integration iterations
- Improve installer, hook reliability, or server/client path behavior.
- Validate with integration smoke tests.

## 5) Improvement Records

Record meaningful decisions and outcomes in `eval/RESULTS.md`.

Minimum fields per entry:
- `hypothesis`
- `implementation changes`
- `evidence` (test deltas, measured outcomes)
- `decision` (`keep`, `iterate`, `rollback`, `replace`)

## 6) Current priority track

Highest-value areas before major expansion:

1. strengthen capture-to-abstraction quality,
2. improve query synthesis for low-signal sessions,
3. expand domain-general scoring for non-coding workflows,
4. harden reusable cleanup/consolidation loop.

Current architecture already gives us:
- mature hybrid retrieval (FTS5 BM25 + cosine similarity + MMR reranking),
- session_date-aware recency scoring,
- server/client operational path via Tailscale,
- cleanup and backfill tooling.
