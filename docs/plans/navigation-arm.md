# Tree navigation with a decision model vs Fieldcraft hybrid retrieval

## Problem

DecisionRAG (arXiv 2610.35868) retrieves with no retriever: a decision model (TypeSafe Jev) walks
a document tree, judging every candidate node of a level with one yes/no (Noul) question each in a
shared context, keeping the top-k and descending. It beat reranking a dense top-100 on two of its
three benchmarks. Open question here: on Fieldcraft's own lessons, does navigation (and navigation
blended with `fc-hybrid`) rank the lesson an agent later cited higher than `fc-hybrid`?

## Scope

In: an offline eval extending `eval/memvid_compare` (one crate, one owner of snapshot loading,
labels, time guard, hybrid arm, metrics, bootstrap). Out: any change to production retrieval,
answer generation, Python. Queries are terse keyword strings (median ~90 chars), which the paper
never tested. Data and caches stay on the Dev SSD (`/Volumes/Development SSD/fieldcraft-nav-eval`);
no lesson text is written to the repo, only aggregates.

`fc-hybrid` in this harness searches the whole corpus and ignores `project_id` (production also
treats project only as a cue/grounding signal, never as a recall boundary,
`fieldcraft-retrieval/src/retrieval.rs`). `nav-project` therefore searches a narrower pool than
the baseline; `nav-global` does not.

## Arms (same corpus, same time guard, same 865 / 2,708 queries)

- A `nav-project`: pool = lessons visible at query time in the query's project plus
  `project_specific = 0` lessons. Groups by (project, first domain cue, else `misc`), split by
  creation month above 150. Judge all groups (requests of at most 100, balanced), keep 10; judge
  all lessons in them, keep 20. `+mc`: one Choice over the top 20.
- B `nav-global`: a project level on top (id, lesson count, 8 commonest domain cues), keep 3, then A.
- C `jev-rerank-hybrid100`: Jev scores the `fc-hybrid` top 100 (shared-context requests).
- D blends: RRF (k=60) of nav and hybrid (plain, and weight tuned on odd queries / scored on even,
  and reversed); and `+dense20`: nav top 20 unioned with hybrid top 20, rescored by Jev.

Descriptions are deterministic (no LLM). Every request and response is cached on disk by request
hash; parallelism 8; transient errors retried, failures counted.

## Metrics

hit@1/3/5/10, MRR, Recall@10; paired bootstrap 95% CI (2,000 draws) against `fc-hybrid`, per arm
and label set. Pruning diagnostics (cited lesson in kept group / final top 20 / hybrid top 20 /
union; where it is lost). Cost (input tokens, dollars at $0.042/M), calls and latency per query.
Bias: labels are lessons cited after Fieldcraft served them, so `fc-served` is favoured; the
primary comparison is each arm against `fc-hybrid`.

## Completion gates

1. Re-running the existing arms reproduces `fc-hybrid` hit@1 0.452, MRR 0.599, Recall@10 0.841 (n=865).
2. Pilot (50 queries): extrapolated cost for 865 queries under $15, else shrink and say so.
3. Full 865 run (2,708 only if its extrapolation is under $15); results in
   `eval/nav_compare_RESULTS.md` with a verdict per metric.
4. `cargo test` and `cargo clippy -- -D warnings` clean for the crate.
