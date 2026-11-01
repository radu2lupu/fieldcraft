# CH-0014: Gemma retrieval quality on replayed queries; Gemma cosine thresholds

**Date**: 2026-11-01
**Type**: evaluation + implementation
**Commit**: c8d0f4f
**Plan**: [docs/plans/local-embeddings-and-query-load.md](../docs/plans/local-embeddings-and-query-load.md), stage 4

## What changed

- `GEMMA_THRESHOLDS` (merge 0.935, near-twin 0.875, dream 0.835, resurrection 0.875), calibrated by
  quantile matching against the Gemini thresholds on a copy of the production store.
- Tools: `fieldcraft-retrieval/examples/calibrate_cosine.rs` and `eval/embed_compare` (Go).
- `install/launchd/com.fieldcraft.embedder.plist` for the sidecar.
- Gemini stays the default provider.

## Results (copy of the production store, 2026-11-01)

Replay of 694 distinct logged queries, 400 of them with a lesson that was served and then cited in
the same session (cited lessons still active):

| Arm | MRR@10 | hit@1 | recall@10 |
|---|---|---|---|
| BM25 only | 0.402 | 0.230 | 0.800 |
| Gemma (Q8_0, local) | 0.429 | 0.230 | 0.845 |

Gemma − BM25 MRR@10 +0.027, paired bootstrap 95% CI [−0.002, +0.056]. overlap@5 Gemma vs BM25
0.645. Against the top 5 the Gemini-era server logged for the same queries (corpus has changed
since, so a confounded reference only): Gemma 0.395, BM25 0.361.

**Not measured: Gemma vs Gemini.** The Gemini arm needs live Gemini query embeddings; no key was
available on this machine and the mini's key was not used. Run `eval/embed_compare` with a Gemini
arm before making Gemma the default.

Calibration (2,000 sampled serving records × 26,681 others, both models): nearest-neighbour cosine
p10/p50/p90 Gemini 0.757/0.865/0.948, Gemma 0.821/0.888/0.955. Threshold mapping and the share of
above-threshold pairs both models agree on: 0.92 → 0.936 (89%), 0.85 → 0.874 (72%), 0.80 → 0.833
(62%). 425 merged duplicate pairs: cosine p10/p50 Gemini 0.900/0.945, Gemma 0.895/0.952. A record's
own text embedded as a query vs its document vector under Gemma: p10/p50/p90 0.962/0.971/0.979
(Gemini measured ~0.927 in July 2026).
