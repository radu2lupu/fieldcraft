# Experiments

Fieldcraft has no maintained benchmark suite and makes no performance claims. This directory
holds two harnesses from one-off experiments; the numbers in their write-ups are dated snapshots of
those runs, not statements about the current system.

| Path | What it is |
|---|---|
| [`usefulness_harness/`](usefulness_harness/) | Runs a coding agent on real tasks twice, once with a lesson served and once with memory isolated, and scores both with the tasks' own tests. Shell scripts plus a small Rust bootstrap tool. |
| [`jev_rerank/`](jev_rerank/) | Replays logged queries through an external judgment model as a reranker over the served lessons and compares the order against what sessions later cited. jq, curl and awk. |

Raw data from earlier research (run logs, corpora, third-party skill text) is not in the public
tree: it contained local paths, private session content, and material whose licenses do not
clearly allow redistribution.
