# Usefulness harness — G1 scorecard (DEFINITIVE, full sweep)

Run 2026-07-02: all 19 pytest-dev/pytest SWE-bench Verified tasks, warm (memory-served) vs cold
(no-memory, CODEX_HOME-isolated), real codex runs, real oracle tests, paired bootstrap
(seed 20260701, 10000 draws). Gold lessons authored for all 19 from the real fix patches.

## The number

| metric | warm - cold | 95% CI | verdict |
|---|---|---|---|
| resolved rate | +5.3 pts (16/19 vs 15/19) | [0.0, +15.8] | positive, not significant |
| steps | **-3.47** | **[-6.58, -0.53]** | **significant: memory reduces effort ~19%** |
| tokens | -40.7k | [-124.9k, +42.7k] | directionally positive |

- warm_expected_source_hit: 19/19 (the gold lesson was actually retrieved in every warm run).
- cold_contaminated: 0/19 (the CODEX_HOME isolation held across the whole sweep).
- Headroom exists now: 3 tasks fail both arms (5262, 7324, 7521); pytest-5787 ("1-4 hours" tier)
  flips cold-FAIL -> warm-PASS.

## Verdict

Memory measurably helps: statistically significant on steps (effort), directionally positive on
resolution and tokens at n=19. This satisfies the G1 gate for the dream tier
(docs/plans/memory-lifecycle.md): `fieldcraft dream --apply` is unlocked, still under its own
guardrails (>=3 cited sources, provisional-until-confirmed, originals retained).

Caveats, stated honestly: single run per arm (no within-task variance), one repo family (pytest),
gold lessons are idealized memory (upper bound on lesson quality, not typical distilled quality).
Next hardening: repeated cold draws per task, a second repo family, and distilled-quality lessons.

## Reproduce

    bash src/run_batch.sh "@all" work results tasks/tasks.json tasks/lessons.json
    bash src/make_scorecard.sh results 20260701 10000 10

Cold isolation design + calibration history: see git history of this file (CODEX_HOME scratch,
zsh -lc re-source diagnosis, 401 misattribution).
