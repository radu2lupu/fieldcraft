# Plan: Cue-Bundle Retrieval + Closed-Loop Feedback

**Status:** Draft v1, 2026-05-15
**Supersedes:** `docs/plans/closed-loop-learning.md` (folded into Phases 4-7 below)
**Source:** Research synthesis at `docs/research/human-memory-recall.md`
**Mental model:** Encoding specificity (Tulving & Thomson 1973) + testing effect (Roediger & Karpicke 2006) operationalized as one coordinated redesign.

## The One-Liner

Redesign fieldcraft's retrieval to score memories conjunctively against a *cue bundle* representing the current situation, and close the loop by tagging every retrieval event with its cue bundle and outcome so the system learns context-conditional effectiveness over time.

## Why One Migration, Not Two

These were initially scoped as separate efforts (cue bundle redesign, then closed-loop feedback). They share a single schema migration and produce a useless intermediate state if shipped separately:

- The closed loop tracks tuples. Without cue_bundle in those tuples, you lose context-conditional efficacy (the "useful for problem 2, useless for problem 1" property the product depends on).
- The cue bundle without the closed loop is improved one-shot retrieval but never gets better over time — same open-loop problem we have today, just with sharper initial recall.

Shipping them together as a single schema migration + progressive feature rollout means:
- One ALTER TABLE migration, designed to support all phases up front
- No re-migration when later phases land
- Telemetry attaches to the redesigned retrieval from day one — no need to retroactively rewrite outcome data
- The research findings compose cleanly (conjunctive cues + outcome feedback = context-conditional learning)

## Mental Model (Three Sentences)

A memory is `(content, cue_bundle)` where the bundle captures situational features present at encoding (phase, domain, active files, distinctive terms, user state, etc.). At retrieval, extract the *current* cue bundle from the live session and score records by conjunctive overlap — multiplicative across hard cues, additive across soft adjustments. Every retrieval logs `(record, cue_bundle, outcome)`; over time the system learns `P(useful | record, cue_bundle)`, which is the actual quantity that determines whether memory A is gold for problem 2 and useless for problem 1.

## Architecture

### Data model changes

Single migration. All new columns and tables ship together.

```rust
pub struct KnowledgeRecord {
    // ... existing fields ...

    // Cue bundle (Phase 1)
    pub cue_bundle: serde_json::Value,

    // Retrieval telemetry (Phase 4)
    pub retrieval_count: u32,
    pub confirmed_count: u32,
    pub ignored_count: u32,
    pub contradicted_count: u32,
    pub last_retrieved_at: Option<DateTime<Utc>>,

    // Dynamic scoring (Phase 6)
    pub dynamic_confidence: f32,  // diverges from static confidence over time
}
```

New tables:

```sql
-- Cue inverted index (Phase 1)
CREATE TABLE cue_index (
    record_id TEXT NOT NULL,
    dim_name  TEXT NOT NULL,
    dim_value TEXT NOT NULL,
    FOREIGN KEY (record_id) REFERENCES knowledge(id) ON DELETE CASCADE
);
CREATE INDEX idx_cue_dim_value ON cue_index(dim_name, dim_value);
CREATE INDEX idx_cue_record    ON cue_index(record_id);

-- Retrieval events (Phase 4)
CREATE TABLE retrieval_events (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    record_id      TEXT NOT NULL,
    session_id     TEXT,
    query_hash     TEXT NOT NULL,
    query_cue_bundle TEXT,         -- JSON of the cue bundle present at retrieval time
    retrieved_at   TEXT NOT NULL,
    rank           INTEGER NOT NULL,
    score          REAL NOT NULL,
    outcome        TEXT,           -- null | confirmed | ignored | contradicted
    outcome_at     TEXT,
    outcome_source TEXT,           -- agent_trace | explicit_api | inferred
    FOREIGN KEY (record_id) REFERENCES knowledge(id)
);
CREATE INDEX idx_re_record     ON retrieval_events(record_id);
CREATE INDEX idx_re_session    ON retrieval_events(session_id);
CREATE INDEX idx_re_outcome_at ON retrieval_events(outcome_at) WHERE outcome IS NOT NULL;
```

### Cue bundle schema

Seven dimensions, mix of deterministic and LLM-classified:

| Dimension | Type | Source | Required |
|---|---|---|---|
| `phase` | enum | LLM (Haiku) | Soft (null tolerated) |
| `domain` | string | Path prefix of active files | Soft |
| `tools_involved` | set<string> | Tool call names | Soft |
| `active_files` | set<string> | Direct from tool calls | Soft |
| `distinctive_terms` | set<string> | TF-IDF over session text | Soft |
| `user_state` | enum | LLM (Haiku) | Soft |
| `session_arc` | enum | Turn count bucket | Soft |

Enum values:
- `phase`: `troubleshoot` | `kickoff` | `design` | `implement` | `debug` | `verify` | `wrap_up`
- `user_state`: `committed` | `exploratory` | `skeptical` | `blocked` | `decisive`
- `session_arc`: `early` (1-5 turns) | `mid` (6-20) | `late` (20+)

All dimensions are optional. Records with sparse content (e.g., general directives) legitimately have null cues — they're meant to be broadly applicable. The conjunctive scorer treats missing dimensions as neutral (mismatch floor = 0.3), neither boosting nor punishing.

### Pipeline (after migration)

```
query + current_cue_bundle
  ↓
┌─────────────┬─────────────┬──────────────────┐
│ Dense ANN   │ FTS5 BM25   │ Cue exact-match  │
│ (BGE-Small) │ (lexical)   │ (situational)    │
└─────────────┴─────────────┴──────────────────┘
  ↓ ↓ ↓
3-way RRF fusion → candidate set (~50-150 records)
  ↓
Conjunctive cue match × (1 + soft_features_sum) × dynamic_confidence
  ↓
MMR diversity rerank
  ↓
Top-K → returned to agent, logged as retrieval_events
  ↓
[post-session, async] trace analyzer → outcome events → confidence updates
```

### Scoring math

```rust
final_score = cue_match_factor(record_bundle, query_bundle)
            × dynamic_confidence
            × (1.0 + soft_features_sum)

fn cue_match_factor(rb: &Bundle, qb: &Bundle) -> f32 {
    // Weighted geometric mean across hard cues with mismatch floor
    let cues    = ["phase", "domain", "user_state", "failure_class"];
    let weights = [0.4,     0.3,      0.2,          0.1];
    let mut log_score = 0.0;
    for (cue, w) in cues.iter().zip(weights.iter()) {
        let m = match_dimension(rb, qb, cue);
        let floored = m.max(0.3);   // missing or mismatched is neutral, not punitive
        log_score += w * floored.ln();
    }
    log_score.exp()
}

fn match_dimension(rb: &Bundle, qb: &Bundle, dim: &str) -> f32 {
    if !rb.contains_key(dim) || !qb.contains_key(dim) {
        return 0.3;  // neutral
    }
    match dim {
        "phase" | "domain" | "user_state" =>
            if rb.get(dim) == qb.get(dim) { 1.0 } else { 0.0 },
        "tools_involved" | "distinctive_terms" =>
            jaccard(rb.get_set(dim), qb.get_set(dim)),
        _ => 0.0,
    }
}
```

Hard cues multiplicative, soft features additive — implements the encoding-specificity research finding (conjunctive cue overlap) without making the system brittle.

### Dynamic confidence updates (Phase 6)

Replaces static `confidence` weight in scoring. Updated by outcome events from the trace analyzer:

```rust
fn apply_outcome(record: &mut KnowledgeRecord, outcome: Outcome) {
    match outcome {
        Outcome::Confirmed => {
            record.dynamic_confidence += 0.05 * (1.0 - record.dynamic_confidence);
            record.confirmed_count += 1;
        }
        Outcome::Ignored => {
            record.dynamic_confidence -= 0.08 * record.dynamic_confidence;
            record.ignored_count += 1;
        }
        Outcome::Contradicted => {
            record.dynamic_confidence -= 0.12 * record.dynamic_confidence;
            record.contradicted_count += 1;
            if record.dynamic_confidence < 0.5 {
                record.state = State::Provisional;
            }
        }
    }
    record.dynamic_confidence = record.dynamic_confidence.clamp(0.05, 0.95);
}
```

Floor 0.05 (never delete, only suppress). Cap 0.95 (no record is ever certain). Confidence beyond 0.90 requires ≥3 distinct confirmations.

### Trace analyzer (Phase 5)

Deterministic v1 — citation match per record type:

| Record type | Distinctive content extraction |
|---|---|
| Procedure | Function/API names mentioned in payload |
| Directive | Imperative noun phrase (e.g., "use rebase not merge" → ["rebase", "merge"]) |
| Episode | File paths + error class names + distinctive identifiers |
| Claim | Distinctive noun phrases via TF-IDF against corpus |
| Context | Verbatim distinctive terms |

For each retrieval event in a session, check if the record's distinctive content appears in subsequent agent reasoning or tool calls within the same session. Match → `confirmed`. No match → `ignored`. `Contradicted` is deferred to v2 (needs LLM classifier).

The trace analyzer runs post-session as a background worker. Idempotency: keyed on `(record_id, session_id)`; second extraction overwrites.

### Feedback API (Phase 4)

```
POST /lessons/{id}/feedback
{
  "outcome": "confirmed" | "ignored" | "contradicted",
  "session_id": "string",
  "cue_bundle": { ... },
  "reason": "optional string"
}

POST /retrievals/batch-feedback
{
  "events": [
    {"record_id": "...", "session_id": "...", "outcome": "confirmed", "cue_bundle": {...}},
    ...
  ]
}
```

Returns updated `dynamic_confidence` per record. Atomic update within a transaction with the event insert.

## Phases

### Phase 1 — Schema migration + deterministic cue backfill (week 1)

Single migration PR. Contains:

- DDL: new columns on `knowledge`, new `cue_index` and `retrieval_events` tables, indexes
- Deterministic cue extractor (5 of 7 cues, no LLM):
  - `domain` (path prefix)
  - `tools_involved` (parsed from payload)
  - `active_files` (extracted paths)
  - `distinctive_terms` (TF-IDF)
  - `session_arc` (turn count bucket, defaults to `mid` for legacy)
- One-shot backfill CLI (`bin/fieldcraft-backfill-cues`) that runs deterministic extraction over the entire existing corpus
- Conjunctive scorer with mismatch-floor rule (Phase 3 integrates it; here it's just the function)
- `cue_bundle` extraction at write time (deterministic only — LLM cues null)

Exit: every existing record has a partial `cue_bundle` populated with 4-5 of 7 dimensions; new records get the same on insert; schema is settled for the remaining phases.

### Phase 2 — Cue-aware retrieval pipeline (weeks 2-3)

- Cue exact-match retriever (SQL over `cue_index` with `min_overlap >= 2`)
- 3-way RRF fusion (dense + BM25 + cue-match)
- Wire conjunctive scorer into the existing retrieval flow
- Refactor the 12-feature scorer — phase_match and file/scope migrate into `cue_match_factor`; others stay as `soft_features_sum`

Exit: retrieval returns cue-aware results; eval (existing `retrieval_usefulness` benchmark) shows no regression and ideally a small lift on situational queries.

### Phase 3 — LLM cue classifier (week 3, parallel with Phase 2)

- Haiku call extracting `phase` and `user_state` (one call, two outputs)
- At write time: deferred to background queue (no blocking)
- At retrieval time for current cue bundle: sync, ~100ms
- For legacy records: priority-queue backfill ordered by `retrieval_count DESC`; lazy fill on first retrieval that touches a record missing classified cues

Exit: all retrieval-time cue bundles fully populated; ≥80% of legacy corpus has classified cues within ~1 week of running the priority worker.

### Phase 4 — Retrieval telemetry + feedback API (week 4)

- Instrument retrieval to write `retrieval_events` (record, session, query, **query_cue_bundle**, rank, score)
- `POST /lessons/{id}/feedback` endpoint
- `POST /retrievals/batch-feedback` endpoint (for trace analyzer)
- CLI: `fieldcraft feedback <record-id> --outcome confirmed`
- Manual smoke test: explicit feedback updates `dynamic_confidence` and persists

Exit: every retrieval logged with cue bundle context; manual feedback flow works end to end.

### Phase 5 — Trace analyzer (week 5)

- Per-record-type distinctive-content extractors (the table above)
- Background worker that reads session transcripts (from `.tr-source-sessions.sqlite3` or equivalent)
- For each retrieval event, citation-match against subsequent agent output in the session
- Emit `confirmed` / `ignored` events via `POST /retrievals/batch-feedback`
- Runs as post-session hook, cron job, or one-shot CLI

Exit: extractor over a 1-week backlog of real sessions produces non-null outcomes for ≥60% of retrievals; manual audit of 50 random events shows precision ≥80%.

### Phase 6 — Closed-loop scoring updates (~3 days, after Phase 5 validation)

- `dynamic_confidence` update rule applied on every outcome event
- Wired into `final_score` as a multiplier (between `cue_match_factor` and `(1 + soft_features_sum)`)
- Caps and floors enforced

Exit: scoring uses outcome-derived confidence; observable divergence between `confidence` (static, write-time) and `dynamic_confidence` (live) within 2 weeks of use.

### Phase 7 — Observability (week 6-7)

- `fieldcraft stats` CLI with five canned queries:
  - **Workhorses**: `confirmed_count >= 3 AND confirmed_count / retrieval_count >= 0.5`
  - **Noise**: `retrieval_count >= 5 AND confirmed_count / retrieval_count < 0.2`
  - **Ghosts**: never retrieved, age >30 days
  - **Stale workhorses**: high `confirmed_count`, `last_retrieved_at > 60 days ago`
  - **Cold sessions**: sessions with retrievals but zero confirmations
- **Session lift metric**: `% of sessions with ≥1 confirmed retrieval` — the North Star number
- Weekly digest auto-snapshot to `dogfooding-log/YYYY-MM-DD.md` (markdown report)
- Optional HTML dashboard for trend visualization

Exit: `fieldcraft stats` runs in <2 seconds; session lift number is visible; weekly digests accumulating for trend analysis.

## Success Criteria

**Plan ships (acceptance) if:**
1. Migration runs cleanly on production data without loss
2. New retrieval returns same-or-better results on existing eval (no regression on `retrieval_usefulness`)
3. Trace analyzer precision ≥80% on manual audit
4. Dynamic confidence diverges from static within 2 weeks of real use

**Plan is a win if (after 30 days of dogfooding):**
1. Session lift ≥50% (≥half of coding sessions involve a confirmed retrieval)
2. ≥30% of records' `dynamic_confidence` has moved by ≥0.1 from baseline
3. Subjectively: you notice fieldcraft surfacing relevant memories without prompting

**Plan is a major win if (after 60 days):**
1. Session lift ≥70%
2. Records retrieved 5+ times with `ignored > confirmed` are reliably below rank 5
3. You can point to ≥3 specific moments where fieldcraft retrieved exactly the right thing in exactly the right situation

## Risks and Mitigations

| Risk | Mitigation |
|---|---|
| LLM cue classifier mislabels phase/user_state | Treat as soft features (mismatch floor 0.3, not 0.0). Closed loop self-corrects: persistent confirmation against the tag flags it for re-classification (Phase 5+ improvement). |
| Trace analyzer false confirmations / false ignores | Deterministic citation match is conservative — biases toward missing real cites (false negatives) rather than fabricating cites (false positives). Audit 100 events at end of week 5; if precision <80%, fix extractors before flipping update switch. |
| Conjunctive scoring zeroes out legitimately relevant records | Mismatch floor of 0.3 prevents zero-annihilation. Tune the floor up if records are getting filtered too aggressively. |
| Backfilled cues are noisy / wrong | The closed loop self-corrects. Synthesized cues are not a special case — even native cues are noisy. The feedback signal is what makes any cues trustworthy. |
| Confidence pump from agent self-confirming bad reasoning | Cap 0.95. Require ≥3 *distinct sessions* to cross 0.90. Future: weight confirmations by external verification (test pass, build success, user accept). |
| Migration is large; breaks production | The migration is additive (new columns nullable, new tables empty-but-indexed). No existing fields modified. Rollback is `DROP TABLE` + `ALTER TABLE DROP COLUMN`. |
| Telemetry overhead slows retrieval | Async writes for `retrieval_events`. Hot path unchanged in latency. |
| LLM cost balloons at scale | Haiku at $0.25/M input, ~200 tokens per classification call. 1000 records/day = $0.05/day. Cap and budget alarm in the worker. |
| Initial weeks show no closed-loop signal | Expected — `dynamic_confidence` needs many events to diverge from static. Don't evaluate before week 3. Don't make scoring weight changes before week 4. |

## What This Plan Defers

Explicitly out of scope for this plan (covered in research synthesis, deferred to later phases):

- **HippoRAG-style co-retrieval graph + Personalized PageRank** — depends on accumulated retrieval events; revisit after Phase 7 ships.
- **Retrieval-induced forgetting (RIF) deduplication** — once cluster competition data exists, demote dominated near-duplicates. Layer 2 in the research synthesis.
- **Episode → Claim consolidation** — LLM-driven semanticization. Layer 3. Needs provenance metadata (separate G1 hypothesis) and human-review gate. Defer 6+ months.
- **Learned scoring weights** — train a small model on accumulated `(query_cue, record_cue, outcome)` triples to replace hand-tuned weights. Layer 4. Defer until ≥5000 events.
- **`applied` outcome detection** — beyond `cited`, detect when agent action matches record's prescription. Extension to Phase 5.
- **`contradicted` outcome detection** — needs LLM classifier. Extension to Phase 5.
- **Reconsolidation update workflow** — retrieve-and-update with append-only versioning. Powerful but high failure-mode risk.

Each of these is a future incremental addition that builds on the foundation this plan establishes. None require schema changes — this migration is the last one.

## Sequencing Notes

- **Phase 1 must ship as one PR** (schema + deterministic backfill + new scorer function). Otherwise the system has the new pipeline but no records to score with it.
- **Phases 2 and 3 can run in parallel** if you have any concurrent capacity. Phase 2 is the integration of the conjunctive scorer; Phase 3 is the LLM cue classifier. Different files, no conflict.
- **Phase 4 (telemetry) is safe to ship before Phase 5 (trace analyzer)** — events accumulate as null-outcome until the analyzer fills them in. No urgency.
- **Phase 5 needs Phase 4 deployed for ≥1 week** before it has data to analyze.
- **Phase 6 needs Phase 5 audited and validated** before enabling auto-updates. The single most important discipline in the whole plan: don't auto-update confidence from a trace analyzer you haven't manually verified is accurate. Bad outcome data corrupts the system invisibly for weeks.

## File Layout

```
crates/fieldcraft-storage/
  migrations/202X-XX-XX-cue-bundle-and-feedback/
    up.sql       # all schema changes in one migration
    down.sql

crates/fieldcraft-core/
  src/cue_extract/
    bundle.rs              # CueBundle struct + serialization
    deterministic.rs       # 5 free cues
    llm_classifier.rs      # phase + user_state via Haiku
    backfill.rs            # one-shot extractor over existing corpus
  src/scoring/
    conjunctive.rs         # cue_match_factor with mismatch_floor
    dynamic_confidence.rs  # outcome → score updates

crates/fieldcraft-retrieval/
  src/retrieval.rs         # 3-way RRF, integrated scoring (modified)
  src/cue_match.rs         # cue exact-match retriever (new)

crates/fieldcraft-trace/   # new crate
  src/citation_match.rs    # per-record-type distinctive content extraction
  src/analyzer.rs          # post-session worker

crates/fieldcraft-server/
  src/routes/feedback.rs   # POST /lessons/{id}/feedback (new)
  src/routes/retrieval.rs  # logs to retrieval_events (modified)

bin/
  fieldcraft-backfill-cues       # Phase 1 one-shot
  fieldcraft-trace-analyzer      # Phase 5 worker
  fieldcraft-stats               # Phase 7 CLI
```

## Total Timeline

| Week | Phase | Deliverable |
|---|---|---|
| 1 | Phase 1 | Schema migration + deterministic backfill ships |
| 2 | Phase 2 | 3-way retrieval pipeline live |
| 3 | Phase 2 + 3 | Conjunctive scorer integrated + LLM cue classifier deployed |
| 4 | Phase 4 | Telemetry + feedback API live |
| 5 | Phase 5 | Trace analyzer running, awaiting precision audit |
| 6 | Phase 5 + 6 | Audit complete, closed-loop scoring updates enabled |
| 7 | Phase 7 | Observability shipped, weekly digests accumulating |

**Total: ~7 weeks of focused solo work.** Realistic. The migration is the longest single piece; the rest is incremental on top.

## References

- Tulving, E., & Thomson, D. M. (1973). Encoding specificity and retrieval processes in episodic memory. *Psychological Review*, 80(5), 352-373.
- Roediger, H. L., & Karpicke, J. D. (2006). The power of testing memory. *Psychological Science*, 17(3), 249-255.
- Rubin, D. C., & Wallace, W. T. (1989). Rhyme and reason: Analyses of dual retrieval cues. *JEP: Learning, Memory, and Cognition*, 15(4), 698-709. *(Cue compounding is multiplicative.)*
- Morris, C. D., Bransford, J. D., & Franks, J. J. (1977). Levels of processing versus transfer appropriate processing. *Journal of Verbal Learning and Verbal Behavior*, 16(5), 519-533.
- Bartlett, F. C. (1932). *Remembering*. Cambridge University Press. *(Schemas as automatic situational activation.)*
- Anderson, J. R. (1974). Retrieval of propositional information from long-term memory. *Cognitive Psychology*, 6(4), 451-474. *(Fan effect.)*
- Watkins, M. J. (1979). Engrams as cuegrams and forgetting as cue overload. *Memory & Cognition*.
- Shinn, N., et al. (2023). Reflexion: Language Agents with Verbal Reinforcement Learning. arXiv:2303.11366.
- Park, J. S., et al. (2023). Generative Agents. arXiv:2304.03442. *(Composite scoring inspiration.)*
- Full literature review at `docs/research/human-memory-recall.md`.
