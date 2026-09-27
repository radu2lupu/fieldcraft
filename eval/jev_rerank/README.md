# Jev as a reranker over Fieldcraft's served lessons

Question (2026-09-27): could a fast judgment model (TypeSafe Jev, System One) replace or improve
Fieldcraft's retrieval ranking?

Answer on this data: not as a replacement. Jev has no embeddings endpoint, and merge, near-twin
retirement, resurrection and dream all need vectors. As a reranker **on its own** it ranks the
lessons agents later cited lower than Fieldcraft's own order, in every framing we tried. **Blended
into** Fieldcraft's order it adds two to three points of hit@1 and MRR, consistently, and the gain
holds on a held-out half. That matches what others found in public (see below): judgment models
rarely beat a tuned first-stage ranker alone, and help as one more signal.

## Method

- **Data:** 60 days of the production `retrieval_log`, joined to `lesson_outcome_events` rows
  where the session later cited a served lesson inline (`lesson:<id>`, the strongest confirm
  signal). One row per (session, cited lesson): the earliest retrieval in that session that served
  it. 865 queries, 424 sessions, 2,370 distinct candidate lessons; 863 queries had 5 candidates.
- **Replay:** for each query Jev sees the query text and the served candidates and scores them.
  Four framings, one request per query:
  - `build.jq`: one Noul per candidate, "would an engineer working on `task` act on this?"
  - `build_choice.jq`: one Choice across the candidates plus `none`.
  - `build_context.jq`: one Noul per candidate, "contains information that helps", plus the
    context Fieldcraft's ranker sees (same project, lesson age at query time, files, phase, kind).
  - `build_action.jq`: same context, the wording that works best in public write-ups: "knowing
    this would change what the engineer does, avoids, checks, or mentions"; no: "only shares a
    topic, tool name, or project name".
- **Score:** rank of the best cited lesson under the served order vs the Jev order (ties keep the
  served order). Paired bootstrap (2,000 draws) for the deltas. `score.sh`.
- **Blend:** `score = 1 / served_rank + λ · P(Jev)`, λ swept; λ chosen on odd-numbered rows and
  evaluated on even-numbered rows for the split-half check.
- **Cost / latency:** 1,330–1,420 input tokens per query, $0.056–0.060 per 865 queries per
  framing; median 0.4–1.4 s per call, 8 in parallel.

Run: `JEV_API_KEY=… ./replay.sh queries.jsonl lessons.jsonl out/ [limit] [parallel]` with
`JEV_BUILD=<build file>`, then `./score.sh queries.jsonl out/`. The context variants need
`query_at` on each query row and `project_id`, `created_at`, `phase`, `files` on each lesson row.
The data is not committed (it contains private lesson text).

## Results (n = 865)

Standalone (Jev order replaces the served order):

| framing | hit@1 | Δ vs served (95% CI) | MRR | Δ vs served (95% CI) |
|---|---|---|---|---|
| served order (baseline) | 0.651 | | 0.781 | |
| Noul per candidate | 0.542 | −0.109 [−0.148, −0.068] | 0.720 | −0.061 [−0.085, −0.036] |
| Choice across candidates | 0.576 | −0.075 [−0.113, −0.036] | 0.744 | −0.037 [−0.061, −0.014] |
| Noul + context | 0.561 | −0.090 [−0.128, −0.050] | 0.732 | −0.049 [−0.072, −0.024] |
| Noul + context, change-the-action wording | 0.582 | −0.069 [−0.108, −0.030] | 0.750 | −0.031 [−0.053, −0.006] |

Blended (`1/served_rank + λ·P`), best λ per framing:

| framing | λ | hit@1 | MRR | ΔMRR 95% CI (λ as shown) |
|---|---|---|---|---|
| Choice | 1 | 0.669 | 0.802 | [+0.010, +0.032] |
| Noul | 2 | 0.682 | 0.805 | λ=1: [+0.006, +0.016] |
| Noul + context | 4 | 0.675 | 0.802 | λ=2: [+0.007, +0.023] |
| Noul + context, change-the-action | 4 | 0.674 | 0.805 | λ=2: [+0.012, +0.027] |

Split-half (Choice): λ chosen on odd rows (best λ=1, MRR 0.799); on the held-out even rows
λ=1 gives MRR 0.805 vs 0.777 and hit@1 0.676 vs 0.650.

## Why standalone Jev loses here

- **The served order already encodes context Jev cannot see.** Fieldcraft ranks with file
  overlap, project identity cues, phase, recency and confidence. Several misses are same-stage
  lessons from a different run, where the agent cited the one for its current run; text alone
  cannot tell them apart. Giving Jev that metadata (`build_context.jq`) recovered only part of it.
- **Wording moves standalone results by a few points, never past the baseline.** The relevance
  wording ("contains information that helps") saturates: mean P(cited) 0.855 vs P(best other)
  0.845. The change-the-action wording separates better (0.786 vs 0.765) and gives the best
  standalone numbers, still below the served order.
- **Labels favour the served order.** Agents saw full text for the top 3 and summaries for ranks
  4–5, so citations lean toward what was ranked high. That handicaps any reranker; it does not
  explain Jev demoting cited lessons more often than promoting them (Noul: 278 down vs 175 up).
- **Queries are terse.** Most logged queries are keyword strings, not task descriptions (median
  ~90 characters). Jev did better on the few long descriptive queries (n=29) and worse on the
  keyword ones (n=788).
- Near-twin confusion by title is rare (2% of misses), and the "guidance" framing costs claims
  about 8 points of hit rate relative to procedures; neither is the main effect.

## What others found (details and URLs in `landscape-2026-09.md`)

- Smriti (local SQLite memory layer, LongMemEval, unbiased labels): Jev alone put the right memory
  first 72.5% vs 63.8% for its own order; a 35% Jev / 65% own-score blend was better than either;
  $0.37 per 1,000 questions, 1.4 s per query; judge shipped off by default because memories leave
  the machine.
- zhuyansen (skills-catalog rerank): Jev alone vs a bge-m3 ranker +0.012 nDCG@10 with a CI
  crossing zero; RRF blend +0.090.
- SuperMemory: Noul-as-sort and Score-10 work as rerankers; Noul as a keep/drop gate "kept
  nothing"; Jev loses to jina-turbo on quality and is not "a very good compactor for memory".
- Post-hoc "was this memory applied in this session?": no published recipe. Retrospective
  judgments are the documented weak spot (2 of 12 real contradictions found in one bakeoff; quality
  rubrics with many false rejects). What works is grounding a Noul in observable artifacts (diff,
  test output, trace) rather than "did the model seem to use it".

## Caveats

- One prompt per framing, no tuning on held-out data beyond the λ split.
- Citations are attributed per session, not per query.
- Only the five served candidates were reranked, so this measures ordering, not recall.
- Every candidate lesson is sent to a hosted API.

## What this means for Fieldcraft

- Not a replacement for embeddings or for the current ranker.
- A blended Jev term is worth about +2–3 points of hit@1/MRR for ~$0.00007 and ~0.4–1.4 s per
  query, with lesson text leaving the machine. If pursued, ship it as an optional feature behind
  a flag, off by default, and validate with online interleaving.
- Do not use it for post-hoc outcome attribution. If a judgment model enters that path, ground
  the question in artifacts (files changed, test output) the way the confirm signal already does.
- The cheaper wins are elsewhere: give the agent's query a real task description (the hooks can
  include the first user message), and expose recency and project identity, which is where the
  served order gets its edge.
