# Evaluation

What ships in this directory, and what does not.

| Path | What it is |
|---|---|
| [`usefulness_harness/`](usefulness_harness/) | The G1 warm-vs-cold harness: real SWE-bench Verified tasks run by a coding agent with memory served (warm) and with memory isolated (cold), scored by the tasks' own oracle tests, paired bootstrap for confidence intervals. Scorecard: [`usefulness_harness/RESULTS.md`](usefulness_harness/RESULTS.md). |
| [`RESULTS.md`](RESULTS.md) | Older retrieval plumbing checks (gold-set pass rate, latency) and the Gauntlet ambiguity suite. These measure plumbing, not task outcomes. |
| [`retrieval_usefulness/`](retrieval_usefulness/) | Reports from the skill-retrieval reveal/abstain eval (the skill-intelligence research track). |
| [`jev_rerank/`](jev_rerank/) | Replay of 865 logged queries through a judgment model (TypeSafe Jev) as a reranker, scored against later-cited lessons. Negative result; harness included. |

## Not in the public tree

Raw run logs and corpora from the skill-intelligence research track (`oracle_validation_v1/`,
`skills/`, `patch_planning/`, retrieval trace JSON) are not published. They contained absolute
local paths, private session content, and third-party skill text whose licenses do not clearly
allow redistribution. The reports that summarize them remain where they are cited. Vendored
third-party benchmarks (for example SWE-Bench-CL) are fetched separately rather than committed;
`benchmarks/` is gitignored.

## Reading the numbers

The strongest current evidence is the G1 sweep: n=19 tasks from one repository family, one run
per arm, with idealized (gold) lessons. It shows a significant reduction in agent steps and a
directional, non-significant improvement in resolution rate. Treat it as evidence that the
delivery path works, not as a general benchmark result. See the caveats in the scorecard.
