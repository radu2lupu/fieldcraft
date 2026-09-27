# Roadmap

This directory is the required durable-state roadmap entrypoint.

The current roadmap document is [../ROADMAP.md](../ROADMAP.md), which now leads with a **Shipped vs Stubbed vs Planned** status table reconciling the cue-bundle plan against the deployed code. It remains at the repo root for compatibility with existing links.

## Current Next Step

The feedback loop is shipped and recording outcomes in **shadow mode** — `dynamic_confidence` is computed and stored but gated OFF (`FIELDCRAFT_DYNAMIC_CONF`), so it does not yet change retrieval. The memory lifecycle (drop, consolidate, dream) is **live**: eviction, protection, and resurrection run on the reference shared-server deployment; `fieldcraft dream` (generative consolidation) passed its G1 usefulness gate on 2026-07-02 and runs nightly with provisional-until-confirmed guardrails. See [docs/plans/memory-lifecycle.md](../docs/plans/memory-lifecycle.md).

The next moves:

1. Audit trace-analyzer outcome precision, then take `dynamic_confidence` out of shadow mode.
2. Make cue-aware ranking authoritative (today `cue_match_factor` is only summed into the score).
3. Build a small golden retrieval eval harness and tighten task-grounding so identity matches admit candidates without dominating rank.
4. Harden the usefulness evidence: repeated cold draws per task, a second repo family, and distilled-quality (not gold) lessons.

Success criteria for the ranking pass:

- the ingredient-display memory ranks first for the known regression query;
- generic same-project memories are demoted when task overlap is weak;
- retrieval changes are validated with recall@30, MRR@10, precision@5, noise@5, and abstain accuracy.

> Observe progress via `fieldcraft stats` (authoritative). Never judge the loop from a client's stale local DB. See [../docs/OPERATIONS.md](../docs/OPERATIONS.md).
