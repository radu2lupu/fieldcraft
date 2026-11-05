# Tree navigation with a decision model vs Fieldcraft hybrid retrieval

Plan: `docs/plans/navigation-arm.md`. Code: `eval/memvid_compare` (`src/bin/nav_eval.rs`, `src/{tree,jev,nav,meta,report}.rs`).
Run 2026-11-01, Jev `jev-latest`, 865-query label window, aggregates only (no lesson text).
Raw run report: `/Volumes/Development SSD/fieldcraft-nav-eval/out/jev/stdout.md`.

## Verdict

Navigation does not beat `fc-hybrid` on this corpus on any metric. Every pure nav arm and every Jev
arm (including Jev reranking the hybrid top 100) is significantly worse than `fc-hybrid`. The one
non-loss is a heavily weighted blend (nav weight 0.1, hybrid 0.9), which is level with hybrid on
hit@1/3/5/MRR and slightly better on hit@10 and Recall@10 (+0.014 and +0.022; CIs exclude 0).

Caveats that matter: queries are terse keyword strings (median ~90 chars), which the paper never
tested; the labels are lessons agents cited after being served Fieldcraft's own top results, so
they favour retrievers close to the served order (`fc-served` is context only; the primary
comparison is each arm against `fc-hybrid`). The full-history set (2,708 queries) was not run: the
pilot extrapolates to about $42.7, over the $15 limit.

## How fc-hybrid treats project_id

It does not use it. The harness `fc-hybrid` (RRF k=60 of FTS5 BM25 and dense cosine, top 80 each)
searches the whole corpus, filtered only by the time guard. Production does the same by design:
`fieldcraft-retrieval/src/retrieval.rs` (around line 1314) runs hybrid search "without treating
`project_id` as a recall boundary"; project identity enters only as a query/cue term and a
task-grounding score adjustment. So `nav-project` searches a narrower pool than its baseline (it
cannot reach a cited lesson from another project), while `nav-global` does not.

## Results (jev-window, n = 865)

| arm | hit@1 | hit@3 | hit@5 | hit@10 | MRR | Recall@10 |
|---|---|---|---|---|---|---|
| fc-hybrid | 0.452 | 0.689 | 0.793 | 0.886 | 0.599 | 0.841 |
| fc-served (context, favoured by labels) | 0.651 | 0.892 | 1.000 | 1.000 | 0.781 | 1.000 |
| jev-rerank-hybrid100 | 0.238 | 0.421 | 0.499 | 0.602 | 0.365 | 0.534 |
| nav-project | 0.182 | 0.308 | 0.378 | 0.443 | 0.270 | 0.385 |
| nav-project+mc | 0.246 | 0.384 | 0.435 | 0.497 | 0.331 | 0.436 |
| nav-global | 0.047 | 0.101 | 0.133 | 0.169 | 0.086 | 0.149 |
| nav-global+mc | 0.088 | 0.148 | 0.169 | 0.192 | 0.123 | 0.173 |
| rrf50-nav-project (plain RRF k=60) | 0.323 | 0.543 | 0.644 | 0.754 | 0.472 | 0.704 |
| rrf-tuned-nav-project (cross-fit weight) | 0.445 | 0.711 | 0.807 | 0.899 | 0.602 | 0.863 |
| rrf50-nav-global | 0.132 | 0.391 | 0.550 | 0.724 | 0.326 | 0.664 |
| rrf-tuned-nav-global (cross-fit weight) | 0.452 | 0.689 | 0.793 | 0.886 | 0.599 | 0.841 |
| nav-project+dense20 | 0.258 | 0.427 | 0.519 | 0.672 | 0.391 | 0.604 |
| nav-global+dense20 | 0.281 | 0.472 | 0.583 | 0.732 | 0.421 | 0.671 |

Paired bootstrap, delta vs fc-hybrid (2,000 draws, seed 20261027), 95% CI:

| arm | hit@1 | hit@3 | hit@5 | hit@10 | MRR | Recall@10 |
|---|---|---|---|---|---|---|
| jev-rerank-hybrid100 | -0.214 [-0.255, -0.173] | -0.268 [-0.308, -0.231] | -0.294 [-0.329, -0.255] | -0.283 [-0.317, -0.249] | -0.234 [-0.265, -0.203] | -0.307 [-0.339, -0.274] |
| nav-project | -0.271 [-0.310, -0.230] | -0.382 [-0.423, -0.341] | -0.415 [-0.454, -0.379] | -0.443 [-0.481, -0.406] | -0.328 [-0.362, -0.295] | -0.456 [-0.493, -0.420] |
| nav-project+mc | -0.206 [-0.246, -0.164] | -0.305 [-0.345, -0.265] | -0.358 [-0.397, -0.321] | -0.388 [-0.425, -0.353] | -0.267 [-0.301, -0.233] | -0.405 [-0.442, -0.369] |
| nav-global | -0.405 [-0.440, -0.370] | -0.588 [-0.624, -0.554] | -0.660 [-0.692, -0.628] | -0.717 [-0.746, -0.688] | -0.513 [-0.541, -0.485] | -0.692 [-0.722, -0.662] |
| nav-global+mc | -0.364 [-0.401, -0.327] | -0.541 [-0.577, -0.504] | -0.624 [-0.658, -0.590] | -0.694 [-0.725, -0.662] | -0.475 [-0.504, -0.445] | -0.668 [-0.699, -0.637] |
| rrf50-nav-project | -0.129 [-0.171, -0.090] | -0.146 [-0.185, -0.108] | -0.149 [-0.184, -0.114] | -0.132 [-0.162, -0.102] | -0.127 [-0.159, -0.097] | -0.137 [-0.168, -0.106] |
| rrf-tuned-nav-project | -0.007 [-0.035, +0.020] | +0.022 [-0.001, +0.044] | +0.014 [-0.002, +0.029] | +0.014 [+0.003, +0.024] | +0.004 [-0.015, +0.021] | +0.022 [+0.012, +0.034] |
| rrf50-nav-global | -0.320 [-0.358, -0.284] | -0.298 [-0.334, -0.262] | -0.243 [-0.275, -0.213] | -0.162 [-0.186, -0.136] | -0.273 [-0.297, -0.249] | -0.177 [-0.202, -0.152] |
| rrf-tuned-nav-global | +0.000 (identical to hybrid: tuned weight 0.0) | | | | | |
| nav-project+dense20 | -0.194 [-0.232, -0.155] | -0.262 [-0.302, -0.224] | -0.274 [-0.310, -0.237] | -0.214 [-0.247, -0.182] | -0.207 [-0.238, -0.176] | -0.237 [-0.270, -0.205] |
| nav-global+dense20 | -0.171 [-0.209, -0.132] | -0.217 [-0.254, -0.180] | -0.210 [-0.244, -0.177] | -0.154 [-0.182, -0.126] | -0.178 [-0.208, -0.149] | -0.170 [-0.198, -0.141] |

Verdict per metric against fc-hybrid (CI of the paired delta excludes 0, else "no difference"):

| arm | hit@1 | hit@3 | hit@5 | hit@10 | MRR | Recall@10 |
|---|---|---|---|---|---|---|
| jev-rerank-hybrid100, nav-project(+mc), nav-global(+mc), rrf50-*, *+dense20 | hybrid better | hybrid better | hybrid better | hybrid better | hybrid better | hybrid better |
| rrf-tuned-nav-project | no difference | no difference | no difference | nav better | no difference | nav better |
| rrf-tuned-nav-global | no difference | no difference | no difference | no difference | no difference | no difference |

Blend weights (on the nav list, MRR-tuned): nav-project 0.1 on odd and 0.1 on even queries;
nav-global 0.0 on both (the blend collapses to hybrid). Halves (hit@1 / MRR / Recall@10):

| arm | odd half (n=432) | even half (n=433) |
|---|---|---|
| fc-hybrid | 0.465 / 0.606 / 0.846 | 0.439 / 0.591 / 0.836 |
| nav-project | 0.167 / 0.256 / 0.375 | 0.196 / 0.284 / 0.395 |
| nav-global | 0.044 / 0.081 / 0.134 | 0.051 / 0.091 / 0.165 |
| rrf50-nav-project | 0.333 / 0.476 / 0.705 | 0.312 / 0.468 / 0.704 |
| rrf-tuned-nav-project (w from other half) | 0.444 / 0.607 / 0.866 | 0.446 / 0.597 / 0.861 |
| nav-project+dense20 | 0.220 / 0.367 / 0.604 | 0.296 / 0.415 / 0.604 |
| nav-global+dense20 | 0.280 / 0.412 / 0.647 | 0.282 / 0.430 / 0.696 |

The two halves agree in direction on every arm. The tuned-blend gain is small and the hit@1
change is a wash in both halves.

## Pruning diagnostics (any cited lesson of the query reaches the stage)

| stage | nav-project | nav-global |
|---|---|---|
| (a0) cited lesson in the pool (own project + global / the 3 kept projects) | 86.2% | 35.3% |
| (a) cited lesson in a kept group | 70.5% | 25.4% |
| (b) cited lesson in the final top-20 | 52.5% | 19.7% |
| (c) cited lesson in fc-hybrid top-20 | 93.3% | 93.3% |
| (d) union of (b) and (c) | 94.9% | 93.6% |
| only nav finds it | 1.6% (14 queries) | 0.3% (3 queries) |
| only hybrid finds it | 42.4% | 74.0% |

Where the cited lesson is lost: nav-project 18.0% ranked below k, 15.7% wrong group, 13.8% wrong
project (cited lesson is in another project and not global); nav-global 5.8% ranked below k, 9.8%
wrong group, 64.7% wrong project. Pruning is the dominant failure: the project level of
`nav-global` discards the right project for two thirds of queries (its description is only the id,
a count and domain cues, and only 4,054 of 32,234 lessons carry a domain cue, so most projects
describe as "domains: none"). Even inside the pool, the judge is the weak step: of the 70.5% of
queries whose cited lesson survives group pruning, 52.5% reach the top 20. Navigation adds almost
no cited lessons the hybrid top 20 misses (14 and 3 queries), so unioning gains little.

## Cost and latency (per query; $0.042 per million input tokens, output free)

| arm | calls | input tokens | $ | latency median / p95 (s) |
|---|---|---|---|---|
| jev-rerank-hybrid100 | 1.0 | 17,770 | 0.00075 | 0.27 / 0.33 |
| nav-project | 12.1 | 193,135 | 0.00811 | 0.49 / 0.63 |
| + mc (adds) | 1.0 | 2,787 | 0.00012 | 0.67 / 0.81 cumulative |
| + dense20 (adds) | 1.0 | 6,696 | 0.00028 | 0.69 / 0.86 cumulative |
| nav-global | 13.3 | 145,620 | 0.00612 | 0.72 / 0.91 |
| + mc (adds) | 1.0 | 2,617 | 0.00011 | 0.89 / 1.10 cumulative |
| + dense20 (adds) | 1.0 | 6,847 | 0.00029 | 0.92 / 1.11 cumulative |

Latency is the sequential critical path (slowest request per level, levels in series), not wall
time under the 8-call cap. All Jev arms together: 375,472 tokens, $0.0158 per query. Full 865 run:
$13.64 of tokens, $12.80 billed in this run (the rest was cached from the pilot); wall 758 s at 8
parallel calls; 0 failed calls, 0 retries, 0 missing answers. For comparison, hybrid retrieval
costs nothing per query and runs in milliseconds.

Pilot (50 evenly spaced queries) before the full run: with 400-character leaf text it extrapolated
to $14.43 for 865 queries ($45.17 for 2,708); leaf text was shrunk to 250 characters to leave
margin, giving $13.23 (865) and $41.42 (2,708). The 2,708-query set was therefore not run.

## Deviations and notes

- Leaf text is 250 characters, not 400 (cost margin, see above); it applies to every arm.
- Groups above 150 lessons split by creation month (then sequential chunks of 150), never by a
  next cue.
- Group descriptions carry the project label (own project, or `global` for other projects'
  global lessons) next to the domain, so the judge can tell the groups apart.
- Candidates are listed with local indices per request; each Noul question points at
  `candidates[i]`. Choice options are `c0..c19`. Noul answers are quantised, so ties are broken by
  parent-group score then recency (nav) or by the prior order (rerank, union).
- Beyond the top 20, nav rankings continue by judged P(yes) to depth 100, so MRR is not truncated.
- `+mc` and `+dense20` were run for both nav arms; the Choice reorders only the top 20.
- The odd/even split indexes jev-window queries in rid order; the tuned-blend arm is cross-fit, so
  no query is scored with a weight it helped choose.
- Existing harness changes (behaviour identical, `fc-bm25/dense/hybrid` digests unchanged):
  `Query` gained `project_id`; the eval window and path helpers moved to `data.rs`; `arms::rrf`
  delegates to a new `rrf_weighted`; the table code moved to `report.rs` (and its Markdown header
  row no longer has a stray trailing `|`).
