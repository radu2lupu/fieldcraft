# Claude Review: Skill Intelligence Architecture

Reviewed file: `docs/skill-intelligence-architecture.md`

## Blocking Flaws Identified

1. Card schema lacked a per-card content hash and mutation trail.
2. Atomization was underspecified and could silently create bad splits.
3. Graph edges lacked deferred resolution and referential integrity.
4. Quarantine had no explicit promotion protocol.
5. Confidence thresholds were uncalibrated labels.
6. Learning loop mutations could write directly into production retrieval state.
7. Eval ground truth risked circularity if labels came from activation cues.
8. Deduplication across source packages was missing.

## Recommended Fixes

1. Add `content_hash`, `card_schema_version`, card revisions, and mutation records.
2. Add per-ecosystem normalization policies and parse-confidence fallbacks.
3. Add pending edges plus a graph integrity pass.
4. Require explicit quarantine promotion records and successful rescan before normalization.
5. Replace confidence labels with calibrated score bands and add `no_skill_reason`.
6. Make learning-loop changes proposals; gate high-risk changes behind review.
7. Add independent `skill_eval_labels`; exclude heuristic labels from public headline metrics.
8. Add duplicate detection and duplicate-cluster collapse during retrieval.

## Changes Applied

The architecture doc now includes:

- card-level schema/version/hash fields
- `skill_card_revisions`, `skill_card_mutations`, and `skill_mutation_proposals`
- normalization policy enum
- normalization failure behavior
- atomization status enum
- deduplication pass and duplicate cluster handling
- pending edge resolution and graph integrity requirements
- calibrated confidence bands and explicit empty-result reasons
- quarantine promotion protocol
- proposal-gated learning loop
- independent eval label sources and public-claim constraints

