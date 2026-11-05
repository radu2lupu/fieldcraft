# Memvid vs Fieldcraft retrieval: head-to-head

## Problem

A tweet and Memvid's own README claim a single-file memory (`.mv2`: Tantivy BM25 + HNSW + RRF in
one append-only file) is a better approach than a RAG stack. We read the source (2026-10-27) and
the public evidence: the benchmark numbers are vendor-run, the baselines are copied from another
paper, the judge is lenient, and LoCoMo itself has a ~6% wrong answer key. No independent
comparison exists. Fieldcraft is already one SQLite file with FTS5 BM25, Gemini dense vectors and
RRF, so the open question is narrow: **does Memvid's index/fusion/container beat ours on our data,
and what do writes and opens cost?**

## Scope

In: retrieval quality (hit@k, MRR, Recall@10) and operational cost (ingest, file size, cold open,
query latency, RSS) over the production lesson corpus. Out: LoCoMo, answer generation, LLM judges,
multi-writer behaviour (stated as a known Memvid limit, not tested), any change to Fieldcraft.

## Data (not committed; private lesson text)

- Snapshot of the production DB on the Mac mini, copied 2026-10-27 to
  `/Volumes/Development SSD/fieldcraft-memvid-eval/snapshot.sqlite3` (32,234 lessons, 30,633 with
  stored Gemini vectors, 22,323 logged retrievals, 7,982 outcome events).
- Queries and labels: the `retrieval_log` x `lesson_outcome_events` join used by
  `eval/jev_rerank` (a session later cited a served lesson inline, `lesson:<id>`). Target about 865
  rows; the harness must say how many it recovered and why any differ.
- Time leakage guard: a query only sees lessons with `created_at <= query created_at`. Archived
  lessons stay in the corpus; the label set was served from the active set.

## Arms (same corpus text, same stored vectors, same query vectors)

1. `fc-bm25`: SQLite FTS5 `bm25` over `retrieval_text`.
2. `fc-dense`: brute-force cosine over the stored vectors.
3. `fc-hybrid`: RRF (k=60) of 1 and 2. The like-for-like reference.
4. `fc-served`: the logged production order (cues, MMR, scoring). Reference only (see bias).
5. `mv-lex`, `mv-vec`, `mv-hybrid`: the real `memvid-core` crate, same texts, same vectors via
   `put_with_embedding`, queries through its own search and its own RRF.
6. Stage 2, only if stage 1 shows a gap: port Memvid's query-type widening (recency, aggregation,
   analytical) onto `fc-hybrid`.

Query vectors: embed each query once with `gemini-embedding-001` at the production dimension and
cache to disk, so every arm sees identical vectors.

## Known bias (state it in the report)

Labels are lessons agents cited after being served Fieldcraft's top results, so `fc-served` is
favoured. The primary comparison is therefore 3 vs 5 (neither was the served system); 4 is context.

## Metrics

hit@1/3/5/10, MRR, Recall@10, with paired-bootstrap 95% CIs (2,000 draws) for each arm against
`fc-hybrid`. Operational: batch ingest time for the full corpus, file size, cold open time, query
latency p50/p95, peak RSS, each measured over 3 runs.

## Stages and completion gates

1. Harness (Rust, separate cargo project under `eval/memvid_compare`, not a workspace member,
   build output on the Dev SSD). Gate: `fc-served` on the recovered label set matches the Jev eval
   baseline (hit@1 about 0.651, MRR about 0.781) within sampling error; arms 1-3 are deterministic
   across two runs.
2. Memvid arms. Gate: ingest completes for all lessons, search returns the label id for at least
   one query, tests for the adapter pass.
3. Report `eval/memvid_compare/RESULTS.md` with the table, CIs, ops numbers, bias note, and a
   verdict that says "no difference", "Memvid better", or "Fieldcraft better" per metric.
4. Only then: stage 2 and a decision on what to adopt. Record it in `change_history/`.
