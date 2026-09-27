# CH-0001 — Initialize durable project state

**Date:** 2026-05-17
**Author:** radu (via Claude)
**Type:** Documentation scaffolding
**Updates:** ARCHITECTURE.md, ROADMAP.md, change_history/ (all created)

## What changed

Initial scaffolding of the four durable-state artifacts required by the global behavioral rule on durable project state (Rule 14):

1. **`ARCHITECTURE.md`** — created. Authoritative description of current crate layout, core types (`KnowledgeRecord` and 18 fields), schema v3 with current tables (`knowledge`, `knowledge_fts`, `feedback`, `lesson_feedback`, `retrieval_log`, etc.), 8-step retrieval pipeline, the 12 scoring features and their actual weights from `P0_WEIGHTS`, MMR + token-budget enforcement, and current invariants (tier promotion is human-only, migrations are idempotent, FTS5 is trigger-driven).
2. **`ROADMAP.md`** — created. Near-term: cue-bundle + closed-loop migration plan ([docs/plans/cue-bundle-retrieval.md](../docs/plans/cue-bundle-retrieval.md)). Mid-term: co-retrieval graph (HippoRAG analogue), RIF dedup, Episode→Claim consolidation, Reflexion-style failure tagging. Long-term: learned retrieval scorer, reconsolidation-style updates, reconstructive multi-record synthesis. North Star metric and eval bar called out explicitly.
3. **`change_history/`** — created with this CH-0001 entry.

## Why

Rule 14 requires four durable-state artifacts at the project root: README, ARCHITECTURE, ROADMAP, and change_history. Before scaffolding this turn, only `README.md` and `CONTRIBUTING.md` existed.

The cue-bundle + closed-loop migration ([docs/plans/cue-bundle-retrieval.md](../docs/plans/cue-bundle-retrieval.md)) is a material architectural change. Per Rule 14, such changes land with an ARCHITECTURE update and a CH entry. The scaffolding here was therefore a prerequisite for the migration's first PR — `/document-this` needs target docs to update.

## Plan corrections discovered during codebase read

Reading [`crates/fieldcraft-core/src/lib.rs`](../crates/fieldcraft-core/src/lib.rs), [`crates/fieldcraft-storage/src/schema.rs`](../crates/fieldcraft-storage/src/schema.rs), and [`crates/fieldcraft-retrieval/src/retrieval.rs`](../crates/fieldcraft-retrieval/src/retrieval.rs) surfaced several gaps in [docs/plans/cue-bundle-retrieval.md](../docs/plans/cue-bundle-retrieval.md) that need to be fixed before any Phase 1 work begins:

1. **The plan proposed a 7-value `phase` enum.** The actual codebase already has a 4-value enum (`Analyze` | `Reproduce` | `Edit` | `Verify`) in `fieldcraft-core`, with detection logic in `detect_query_phase` and active phase boost (`+0.25` same / `-0.15` cross) in `compute_score`. The plan's expanded enum was speculation; align to the existing 4 values per Rule 3 (surgical changes) and Rule 7 (match conventions).

2. **The plan proposed a new `retrieval_events` table.** A `retrieval_log` table already exists with `raw_candidate_ids`, `raw_candidate_scores`, `link_ids_surfaced`, `link_ids_revealed`. The plan's table is mostly duplicative; the migration should extend `retrieval_log` (add `query_cue_bundle` and `outcome` columns) rather than introduce a parallel table.

3. **The plan proposed a new feedback endpoint.** `feedback` and `lesson_feedback` tables already exist with `knowledge_id`, `signal`, `session_id`, and per-retrieval linkage. Existing infrastructure should be the foundation; the plan's `POST /lessons/{id}/feedback` may already be partially present (verification needed during the spike).

4. **`affected_files_json` already exists** as a column on `knowledge`. Files are also extracted opportunistically from `retrieval_text` by parsing `scope:` / `files:` prefix patterns. Partial cue infrastructure is already encoded; the migration extends rather than replaces.

5. **`scope_match` is already the most heavily weighted feature** at 0.25 (`P0_WEIGHTS[11]`). The plan's narrative that "linear additive scoring is broken" needs softening — scope-based situational matching already dominates the score. The cue-bundle redesign upgrades this from a soft feature to a multiplicative conjunctive layer; it doesn't introduce situational matching from scratch.

These corrections should be folded into the plan before any Phase 1 implementation work begins. The next CH entry should capture that plan update.

## What's verified

- All three new docs exist at expected paths
- Cross-references point to existing files (`docs/plans/cue-bundle-retrieval.md`, `docs/research/human-memory-recall.md`, `docs/plans/closed-loop-learning.md` (never landed), `eval/RESULTS.md`)

## What's not verified

- No tests run (documentation-only change)
- Build not re-run (no Rust code touched)
- The plan corrections listed above are **noted, not yet applied** to [docs/plans/cue-bundle-retrieval.md](../docs/plans/cue-bundle-retrieval.md) — that's the next discrete step

## Related

- Plan: [docs/plans/cue-bundle-retrieval.md](../docs/plans/cue-bundle-retrieval.md)
- Research: docs/research/human-memory-recall.md (not in the public tree)
- Superseded prior plan: docs/plans/closed-loop-learning.md (never landed)
