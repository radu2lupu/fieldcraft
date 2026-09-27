# CH-0004: Cue-first retrieval replaces project-scoped recall

**Date**: 2026-06-04
**Type**: implementation

## What changed

Fieldcraft retrieval now treats project/app identity as cue evidence instead of as a hard `project_id` boundary.

1. **`fieldcraft-core`** — added first-class typed cues:
   - `CueDimension`
   - `CueValue { raw, canonical, aliases, kind }`
   - `Cue { dimension, value, confidence, source }`
   - `CueBundle.cues`
2. **`fieldcraft-core::cue_extract`** — deterministic extraction now mirrors legacy cue fields into typed cues (`domain`, `technical`, `file`, `symbol`) and can attach normalized identity cues with aliases.
3. **`fieldcraft-storage`** — insert and backfill paths add `project_id` as an `identity` cue and index it in `cue_index`.
4. **`fieldcraft-storage`** — identity candidate admission now searches `cue_index(dim_name='identity')`; stored `project_id` is only a legacy fallback for unbackfilled rows.
5. **`fieldcraft-retrieval`** — hybrid search is unscoped across the active tier, carries each record's actual `project_id` through to returned lessons, and no longer labels unscoped hits with the request project.
6. **`fieldcraft-retrieval`** — query construction uses the request project as an identity hint, not a task token flood. Synthetic compact phrase expansion was removed from task/context text.
7. **`fieldcraft-retrieval`** — final ranking adds weighted task grounding and filters weakly grounded same-identity records before MMR.

## Why

The motivating failure was an Oh Potato query about inconsistent ingredient display strings. Relevant memory `9577588004301f8c25103d0368c103c8dcc7c3a5357ff4d0acccbcb824c8c165` existed, but retrieval returned generic UI/plugin/app-icon memories because identity matching and project scoping were doing the wrong job.

The architecture decision is:

- identity cues admit candidates;
- task/domain/symbol/file cues rank candidates;
- `project_id` remains storage/provenance metadata only;
- MMR runs after relevance filtering, not before.

## Verification

- `cargo test -p fieldcraft-core -p fieldcraft-storage -p fieldcraft-retrieval`
- `cargo clippy -p fieldcraft-core -p fieldcraft-storage -p fieldcraft-retrieval --locked -- -D warnings`
- `git diff --check`
- Local copied-DB regression: the Oh Potato ingredient-display query returned lesson `9577588004301f8c25103d0368c103c8dcc7c3a5357ff4d0acccbcb824c8c165` first during PR verification.
- Deployed follow-up: after Mac Mini deploy and full cue backfill, the same query returned the relevant lesson second behind an unrelated app-icon memory. Candidate admission is fixed, but ranking quality still needs a golden retrieval eval and tighter task-grounding.

## Affects

- ARCHITECTURE: §2 Cue model, §3 cue bundle column, §4 retrieval pipeline, §8 active direction.
- ROADMAP: near-term cue-aware retrieval phase marked shipped.
