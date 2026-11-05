# Memvid vs Fieldcraft: results

Run 2026-10-27 on the Mac (Apple silicon), snapshot of 32,234 lessons (30,633 with stored
768-dim Gemini vectors), `memvid-core` 2.0.140 (features `lex`, `vec`).

## Verdict

On this corpus Memvid's retrieval is clearly worse than Fieldcraft's like-for-like `fc-hybrid`
on every metric (all 95% CIs exclude zero), and costs far more to ingest and store. Memvid's own
RRF hybrid beats either of its legs but stays 0.12-0.14 below `fc-hybrid`. **Fieldcraft better**
on every quality and operational metric except `mv-vec` query latency. No reason to adopt Memvid;
stage 2 (porting its query widening) is not justified by a quality gap in Memvid's favour.

Part of the gap is harness-imposed, not purely Memvid's engine (see Deviations and caveats):
the vector leg is capped at 50 pre-guard hits, and Memvid's lexical index (Tantivy) is not
configured identically to FTS5.

## Labels

Rule: a `lesson_outcome_events` row with `outcome='confirmed'`, `source='trace_analyzer'`,
`note LIKE '%inline%'` (the inline `lesson:<id>` citation), joined to `retrieval_log` on
`session_id`; one row per retrieval that was the earliest in its session to serve a cited lesson,
restricted to retrievals in `[2026-07-29, 2026-09-25)`. This recovers **865 queries, 424 sessions**,
exactly matching the Jev README (865 / 424). Later end dates add rows the Jev export did not have
(870 until 09-27, 872 until 09-28). The window end was chosen to match, which is a fit to the
README rather than an independent derivation. A second set, full history (2,708 queries), is in the
run output as a robustness check; conclusions are the same.

`fc-served` on the recovered set: hit@1 **0.651**, MRR **0.781**, matching the Jev baseline exactly.

## Retrieval quality (865 label-window queries)

| arm | hit@1 | hit@3 | hit@5 | hit@10 | MRR | Recall@10 |
|---|---|---|---|---|---|---|
| fc-bm25 | 0.386 | 0.634 | 0.740 | 0.836 | 0.539 | 0.785 |
| fc-dense | 0.320 | 0.514 | 0.631 | 0.723 | 0.456 | 0.643 |
| **fc-hybrid** | 0.452 | 0.689 | 0.793 | 0.886 | 0.599 | 0.841 |
| fc-served (reference, biased) | 0.651 | 0.892 | 1.000 | 1.000 | 0.781 | 1.000 |
| mv-lex | 0.239 | 0.383 | 0.461 | 0.572 | 0.346 | 0.504 |
| mv-vec | 0.302 | 0.495 | 0.599 | 0.691 | 0.430 | 0.616 |
| mv-hybrid | 0.332 | 0.570 | 0.655 | 0.756 | 0.477 | 0.703 |

Paired bootstrap vs `fc-hybrid` (2,000 draws, seed 20261027), delta [95% CI]:

| arm | hit@1 | hit@10 | MRR | Recall@10 |
|---|---|---|---|---|
| fc-bm25 | -0.066 [-0.098, -0.032] | -0.050 [-0.073, -0.028] | -0.059 [-0.082, -0.036] | -0.056 [-0.080, -0.033] |
| fc-dense | -0.132 [-0.160, -0.102] | -0.163 [-0.190, -0.136] | -0.143 [-0.162, -0.122] | -0.198 [-0.225, -0.173] |
| mv-lex | -0.213 [-0.250, -0.176] | -0.313 [-0.348, -0.277] | -0.252 [-0.281, -0.224] | -0.337 [-0.370, -0.302] |
| mv-vec | -0.150 [-0.180, -0.120] | -0.194 [-0.224, -0.166] | -0.168 [-0.189, -0.146] | -0.225 [-0.253, -0.198] |
| mv-hybrid | -0.120 [-0.153, -0.089] | -0.129 [-0.155, -0.105] | -0.121 [-0.143, -0.099] | -0.138 [-0.163, -0.113] |
| fc-served | +0.199 [+0.165, +0.235] | +0.114 [+0.094, +0.136] | +0.183 [+0.159, +0.208] | +0.159 [+0.137, +0.181] |

(hit@3/hit@5 deltas are in the run output, same sign and significance.) Per-metric verdict: every
Memvid arm is worse than `fc-hybrid` on every metric. Primary comparison is `fc-hybrid` vs
`mv-hybrid`: Fieldcraft better.

Time-leakage check: 0 future lessons in any guarded arm's ranking across all 2,712 queries.
`fc-bm25/dense/hybrid` digests were produced in this one run; determinism across two runs was not
re-checked beyond the unit tests for RRF.

## Operational (3 runs each, `/usr/bin/time -l`)

| measure | Memvid | Fieldcraft/SQLite |
|---|---|---|
| batch ingest, all 32,234 lessons | 908.6 s, 871.5 s, 1019.7 s (put 828-979 s + commit ~40 s) | 4.1 s, 4.1 s, 3.9 s (minimal SQLite + FTS5 file) |
| file size | 462,471,286 / 462,471,002 / 462,471,437 bytes (462 MB) | 141,209,600 bytes (141 MB minimal file); production snapshot 491 MB with all columns and indexes |
| cold open (`open_read_only` + frame map) | 1150 ms, 457 ms, 423 ms (page cache not purged; run 1 is closest to cold) | n/a (SQLite open is near-instant; not separately timed) |
| first lex+vec query after open | 99 ms, 65 ms, 65 ms | n/a |
| query latency p50 / p95, lexical | 69.8 / 106.9 ms, 71.6 / 109.5, 69.6 / 107.6 | fc-bm25 (FTS5): 12.2 / 17.1, 11.3 / 16.3, 11.7 / 17.1 |
| query latency p50 / p95, vector | 0.63 / 1.18 ms, 0.64 / 1.23, 0.63 / 1.13 (HNSW, approximate, top 50) | fc-dense (brute force, 32k vectors): 23.6 / 24.8, 22.8 / 24.2, 22.4 / 24.5 |
| peak RSS | ingest 1.50 / 1.20 / 1.41 GB; query 794-799 MB | ingest 241 MB; query 254 MB; corpus-load baseline 233 MB |

Latencies are over the first 300 label-window queries. Run 1's `build` wall clock was 2,658 s versus
its self-reported 909 s total, which the machine being busy or asleep explains; the self-reported
ingest times are the figures used. The `.mv2` is ~3.3x the minimal SQLite file for the same data
(part of that is the local capacity ticket's pre-sized WAL, see below).

## Label-bias caveat

Labels are lessons agents cited after being served Fieldcraft's top results, so `fc-served` is
favoured by construction (its hit@5 and Recall@10 of 1.000 are an artefact: every labelled lesson was
in the served top 5). The primary comparison is therefore `fc-hybrid` vs `mv-hybrid`, neither of
which was the served system; `fc-served` is context only. A lesson that was never served but would
have been useful is unlabelled for all arms.

## Deviations and caveats

- **`mv-lex` uses `Memvid::search` (Tantivy), not `search_lex`.** `search_lex` returns "Lexical index
  is not enabled" on a reopened `put_many`-written file; `ask` itself calls `search`. The query is the
  production OR-joined term string so it matches `fc-bm25`'s input. Tantivy's tokenisation/stemming and
  scoring differ from FTS5, so `mv-lex` partly measures that, not just fusion.
- **`mv-vec` over-fetch is 50, not 1000.** `search_vec` runs HNSW with a hard-coded `ef_search=50`
  and panics when asked for more. Hits are time-guarded after retrieval, so queries whose top 50
  contain future lessons lose candidates; this handicaps `mv-vec` and `mv-hybrid` (lexical over-fetch
  is 1,000). Not quantified. Vectors are unit length, so Memvid's L2 order equals cosine order.
- **`mv-hybrid` is our RRF (k=60, top 80 per leg) over Memvid's two legs**, not Memvid's `ask`
  call, which needs a model; only retrieval was used, and fusion was done in `arms::hybrid` to
  share the time guard. It is not Memvid's own fusion code.
- **Capacity ticket:** a fresh `.mv2` is free tier (50 MiB); the build applies a local ticket via
  `Memvid::apply_ticket` (as the crate's tests do) with a pre-sized WAL. This is a licensing
  workaround for the experiment, not a production setup, and may inflate file size.
- **Time guard:** one `.mv2` holds all lessons; hits are filtered by `created_at <= query time`
  after over-fetching 1,000 (lex) / 50 (vec).
- **The accuracy eval ran against the `.mv2` built before the three ops rebuilds**; the later rebuilds
  were not re-evaluated, so cross-rebuild ranking stability of the HNSW index is unverified.
- Cold open is not truly cold (no `purge`, needs sudo). Ingest/RSS for Memvid includes the harness's
  in-memory corpus copy (~233 MB baseline, the `load-only` row).
- fc-bm25 latency excludes fusion; `fc-hybrid` latency was not benchmarked separately.
- Not committed or pushed, per the brief.
