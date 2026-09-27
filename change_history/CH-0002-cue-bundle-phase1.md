# CH-0002: Cue-bundle retrieval Phase 1 shipped

**Date**: 2026-05-17
**Type**: implementation

## What changed

Phase 1 of the cue-bundle retrieval migration ([docs/plans/cue-bundle-retrieval.md](../docs/plans/cue-bundle-retrieval.md)) landed end-to-end. Seven deliverables across three crates:

1. **`fieldcraft-core`** — new `CueBundle`, `UserState`, `SessionArc` types (`src/lib.rs`) plus a deterministic `cue_extract` module with 5 extractors: `domain`, `tools_involved`, `active_files`, `distinctive_terms`, `session_arc` (`src/cue_extract.rs`).
2. **`fieldcraft-storage`** — schema migration adds `cue_bundle TEXT` column on `knowledge`, a new `cue_index` table (inverted lookup, FK to `knowledge`), and `query_cue_bundle TEXT` on `retrieval_log`. All migrations idempotent via `run_migration()`.
3. **`fieldcraft-storage`** — `cue_backfill` module with `backfill_all()` for legacy records (`src/cue_backfill.rs`).
4. **`fieldcraft-cli`** — new thin binary `fieldcraft-backfill-cues` (`src/bin/`).
5. **`fieldcraft-retrieval`** — new `cue_match` module with conjunctive `cue_match_factor` scorer: weighted geometric mean, 0.3 mismatch floor, weights 0.40 phase / 0.25 domain / 0.15 user_state / 0.10 tools / 0.10 terms (`src/cue_match.rs`). **Not yet wired into `retrieve()`** — Phase 3 does that.
6. **`fieldcraft-storage`** — `KnowledgeRepository::insert_knowledge` now auto-derives the deterministic cue bundle from `title + retrieval_text + affected_files` and rebuilds `cue_index` rows on every insert.
7. **Tests** — 137+ workspace tests pass: core `+20`, storage `+7`, retrieval `+9`.

## Why

Implements Tulving & Thomson (1973) encoding specificity: the same situational features (cues) must be present at encoding and retrieval for the record to be recoverable. The conjunctive scorer enforces this multiplicatively, so a record with a hard cue mismatch cannot dominate purely on dense similarity — closing a failure mode the existing linear scorer was blind to.

Phase 1 is the schema + write-time + scorer foundation. New records get cue bundles automatically; legacy records can be backfilled via the new CLI binary. Phase 3 will wire the scorer into the retrieval hot path; Phase 4+ adds the closed-loop telemetry that needs `query_cue_bundle` on `retrieval_log` (already migrated here).

## Affects

- ARCHITECTURE: §3 Storage (new column `cue_bundle`, new table `cue_index`, new `retrieval_log.query_cue_bundle`); §4 Retrieval (mention `cue_match` module, note not yet wired); §8 Active Design Direction (Phase 1 status update)
- ROADMAP: Near-term phase 1 line marked shipped
- README: no (no user-facing surface change beyond the new `fieldcraft-backfill-cues` binary, which is an internal/migration tool)

## Lesson (optional)

—
