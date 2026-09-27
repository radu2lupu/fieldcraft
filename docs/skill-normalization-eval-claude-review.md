# Claude Review: Skill Normalization Eval Framework

Reviewed file: `docs/skill-normalization-eval-framework.md`

## Blocking Flaws Identified

1. Gold checklist generation risked circularity if it used the same path as normalization.
2. The framework scanned source packages but did not explicitly re-scan normalized cards and adapter payloads.
3. There was no metric for preserving the non-obvious expertise that makes a skill valuable.
4. Runtime regression checks listed metrics but did not define rollout gates.
5. Over-split detection relied too much on co-retrieval instead of task success with isolated cards.
6. Under-split detection lacked a topic breadth probe.
7. Compression ratio was listed as a metric without a decision rule.

## Fixes Applied

The framework now includes:

- independent/adversarial source checklist extraction
- post-normalization scanner pass over cards, references, edge cues, and adapter payloads
- differentiating-content probe for non-obvious expertise preservation
- runtime regression gates for solve rate, tokens, wall time, safety leakage, and graph expansion
- standalone task probes for atomized card self-sufficiency
- topic breadth probe for under-splitting
- minimum sample sizes for retrieval metrics
- metric hygiene rules so diagnostic-only metrics do not become headline quality claims
- structured reviewer acceptance rubric

