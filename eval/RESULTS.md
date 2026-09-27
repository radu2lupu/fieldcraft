# Fieldcraft Benchmark Results

## Architecture Comparison: Qdrant → SQLite-only

Migration completed 2026-04-01. Removed Qdrant vector DB, rewrote to SQLite-only
(brute-force cosine + FTS5 BM25 hybrid search, RRF fusion, MMR reranking).
Local embeddings via BGE-small-en-v1.5 (384-dim, CPU inference).

---

## Tier 3: Retrieval Quality (plumbing check)

### Gold Standard Eval (30 synthetic query-result pairs)

| Metric | Result |
|--------|--------|
| Pass rate | 29/30 (97%) |
| exact_keyword | 10/10 |
| semantic | 10/10 |
| cross_topic | 5/6 |
| diagnostic | 4/4 |
| Latency mean | 98ms |
| Latency median | 96ms |
| Latency min | 94ms |
| Latency max | 135ms |

Single failure: #25 (cross_topic) "CSS framework removing styles" — unwanted CSS Grid result.

### Real Lesson Benchmark (15 queries across 4 real projects)

| Metric | Result |
|--------|--------|
| PASS | 10/15 (67%) |
| WEAK | 5/15 (33%) |
| FAIL | 0/15 (0%) |
| Mean score | 0.800 |
| Keyword hit rate | 67% |
| Kind hit rate | 87% |
| Top-3 relevance | 67% |

All 5 WEAK cases are from iwantthings-ios and ohapotato-ios projects where lessons
are primarily raw Codex session imports (low-quality episode records without
structured summaries).

### Latency Profile

| Metric | Value |
|--------|-------|
| P50 (median) | 96ms |
| Mean | 98ms |
| Min | 94ms |
| Max | 135ms |

Note: Includes local CPU embedding time (~60-70ms). Old Qdrant latency (11-17ms)
excluded embedding time, making direct comparison misleading. The search-only
component is estimated at 2-4ms (faster than old Qdrant's 11-17ms HNSW search).

---

## Tier 1: Task Success Under Ambiguity (the Gauntlet)

### Historical Results (Qdrant + Codex, 2026-03-27)

| Metric | Value |
|--------|-------|
| Scenarios | 10 |
| Warm success | 100% |
| Cold success | 100% |
| Control success | 0% |
| Warm median time | 58-76s |
| Cold median time | 118-170s |
| Warm speedup | ~2x |

Note: 100% cold success means these scenarios were too easy — the agent could
solve everything without lesson. This undermines the headline claim about lesson's
value. See "Phase 4" below.

### New Results (SQLite + Claude Code CLI, 2026-04-01)

4 scenarios x 3 repeats x 3 modes (warm/cold/control) = 36 agent runs.

| Metric | Old (Qdrant+Codex) | New (SQLite+Claude) |
|--------|-------------------|---------------------|
| Warm success | 100% | 100% |
| Cold success | 100% | 100% |
| Control success | 0% | 100% |
| Warm win rate | — | 42% |
| Warm median | 58-76s | 32-39s |
| Cold median | 118-170s | 28-42s |

Per-scenario breakdown:

| Scenario | Warm | Cold | Control | Warm med | Cold med | Speedup |
|----------|------|------|---------|----------|----------|---------|
| inventory_allocator | 100% | 100% | 100% | 38.7s | 41.6s | 1.08x |
| pricing_engine | 100% | 100% | 100% | 36.9s | 35.9s | 0.99x |
| token_bucket | 100% | 100% | 100% | 32.1s | 38.9s | 1.34x |
| csv_export_regression | 100% | 100% | 100% | 33.4s | 27.5s | 0.84x |

Key observations:
- **All modes pass 100%** — these 4 scenarios are too easy for Claude Code CLI.
  Even control (irrelevant lesson) passes, meaning the tasks are trivially solvable.
- **No meaningful warm/cold speed difference** — median speedup ranges 0.84-1.34x,
  with warm actually slower on csv_export_regression.
- **Claude Code is 2-4x faster than Codex** across all modes (32-42s vs 58-170s),
  but this is an agent capability improvement, not a lesson system improvement.
- **warm_expected_source_hit = 0%** — retrieval finds lessons but the source IDs
  don't match expected patterns (format mismatch between old tr:// URIs and new
  SQLite record IDs).
- **These results cannot support a public claim about lesson's value.** The scenarios
  need to be harder (cold success 30-70%) for the warm-cold delta to be meaningful.

---

## Phase 4: Publicly-Citable Benchmark (planned)

To produce numbers we can confidently cite:
- Expand to 30 scenarios across 6 categories
- Calibrate difficulty: cold success 30-70%
- Hold out 10+ scenarios from tuning
- Report bootstrapped 95% CIs
- Target: warm-cold delta >= 25 points, warm-control delta >= 20 points
