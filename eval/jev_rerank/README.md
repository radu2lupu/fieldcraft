# Jev as a reranker over Fieldcraft's served lessons

Question (2026-09-27): could a fast judgment model (TypeSafe Jev, System One) replace or improve
Fieldcraft's retrieval ranking? Answer on this data: no. Jev cannot replace embeddings (it has no
embeddings endpoint, and merge, near-twin retirement, resurrection and dream all need vectors), and
as a reranker over the served list it ranks the lessons agents later cited *lower* than
Fieldcraft's own order does.

## Method

- **Data:** 60 days of the production `retrieval_log`, joined to `lesson_outcome_events` rows
  where the session later cited a served lesson inline (`lesson:<id>`, the strongest confirm
  signal). One row per (session, cited lesson): the earliest retrieval in that session that served
  it. 865 queries, 424 sessions, 2,370 distinct candidate lessons; 863 queries had 5 candidates.
- **Replay:** for each query, Jev sees the query text and the served candidates (kind, title, up
  to 1,200 chars of retrieval text) and scores them. Two framings:
  - `build.jq`: one Noul per candidate ("would an engineer working on `task` act on this?").
  - `build_choice.jq`: one Choice across the candidates plus `none`.
- **Score:** rank of the best cited lesson under the served order vs the Jev order (ties keep the
  served order). Paired bootstrap (2,000 draws) for the deltas. `score.sh`.
- **Cost / latency:** ~1,330 input tokens per query; $0.056 per 865 queries per framing;
  median 1.3–1.4 s per call, p90 2.5 s, 8 in parallel.

Run: `JEV_API_KEY=… ./replay.sh queries.jsonl lessons.jsonl out/ [limit] [parallel]` then
`./score.sh queries.jsonl out/`. The export SQL is in the session notes; the data is not committed
(it contains private lesson text).

## Results (n = 865)

Noul per candidate:

| metric | served order | Jev order | delta | 95% CI |
|---|---|---|---|---|
| hit@1 | 0.651 | 0.542 | −0.109 | [−0.148, −0.068] |
| hit@3 | 0.894 | 0.880 | −0.014 | |
| MRR | 0.781 | 0.720 | −0.061 | [−0.085, −0.036] |

Choice across candidates:

| metric | served order | Jev order | delta | 95% CI |
|---|---|---|---|---|
| hit@1 | 0.651 | 0.576 | −0.075 | [−0.113, −0.036] |
| hit@3 | 0.894 | 0.910 | +0.016 | |
| MRR | 0.781 | 0.744 | −0.037 | [−0.061, −0.014] |

Choice, split by Jev's own confidence: when confident (≥ 0.6, n = 289) Jev hit@1 0.706 vs served
0.685; when unsure (n = 576) 0.510 vs 0.634. A confidence-gated hybrid would therefore be roughly
neutral, for extra latency and a second API dependency.

By query length, Jev only came out ahead on long descriptive queries (≥ 140 chars, n = 29:
0.655 vs 0.621) and lost on the mid-length keyword queries that dominate the log (n = 788:
0.532 vs 0.650). Agents mostly query with terse keyword strings, which gives a judgment model
little to reason about.

## Caveats

- **Label bias toward the served order.** Agents saw full text for the top 3 and summaries for
  ranks 4–5, so citations favour what was already ranked high. This handicaps any reranker; it
  does not explain Jev demoting cited lessons more often than promoting them (Noul 278 down vs
  175 up; Choice 252 vs 183).
- Citations are attributed per session, not per query; a lesson cited because of one query counts
  for the earliest query in the session that served it.
- Only the five served candidates were reranked, so this measures ordering, not recall.
- One prompt per framing; no prompt tuning.

## What Jev might still be good for here

Not ranking, but judging: "did this session apply this lesson?" for outcome attribution (today's
weakest signal is a filename match), or "does this derived lesson follow from its sources?" for
the dream tier. Those are yes/no questions over specific evidence, which is the model's shape.
Untested.
