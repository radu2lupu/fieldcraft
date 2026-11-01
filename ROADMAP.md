# Roadmap

This document describes where fieldcraft is going, and — at the top — exactly what is shipped vs stubbed vs planned right now. For what it *is now*, see [ARCHITECTURE.md](ARCHITECTURE.md). For *how it's deployed*, see [docs/OPERATIONS.md](docs/OPERATIONS.md). For *what just changed*, see [change_history/](change_history/).

The roadmap is grouped by horizon. Near-term items are committed direction; mid-term are planned; long-term are research-grounded but not scheduled.

---

## Status — Shipped vs Stubbed vs Planned

This reconciles [docs/plans/cue-bundle-retrieval.md](docs/plans/cue-bundle-retrieval.md) against the deployed code. The plan document is a design artifact; the table below is ground truth.

| Capability | State | Notes |
|---|---|---|
| Cue backfill + `cue_bundle` column + `cue_index` | **Shipped** | CH-0002. Deterministic cues populated at write and over the corpus. |
| `retrieval_log.query_cue_bundle` logging | **Shipped** | Query-time cue bundle persisted on every retrieval. |
| Cue-first admission (identity admits, task ranks) | **Shipped** | CH-0004. `project_id` is no longer a hard retrieval boundary. |
| Trace analyzer / closed loop (`trace-analyze`) | **Shipped** | Emits CONFIRMED / CONTRADICTED / IGNORED into `lesson_outcome_events`. |
| **File-overlap** confirmed signal | **Shipped** | Served lesson's `affected_files` ∩ session's git-changed files → CONFIRMED. |
| `dynamic_confidence` computed + stored | **Shipped** | `apply_feedback_outcome` updates it asymptotically. |
| Merge / consolidation (cosine ≥ 0.92 + cue overlap) | **Shipped** | Non-destructive supersede + archive. ~8,955 merges done. Also merges byte-identical HTTP-written pairs with no embeddings. |
| Memory lifecycle L1-L4 (protection, eviction, resurrection, RIF demotion) | **Shipped, live** | Live on the reference shared-server deployment (`FIELDCRAFT_LIFECYCLE_APPLY=1`, 6h cadence). First production cull archived 41 records; resurrection proven end-to-end. See [docs/plans/memory-lifecycle.md](docs/plans/memory-lifecycle.md). |
| Memory lifecycle L5 (`fieldcraft dream` generative consolidation) | **Shipped, guarded** | Runs nightly on the reference deployment. Derived claims land `provisional` and cite >= 3 sources; originals retained. |
| Lesson → skill / AGENTS.md promote (`fieldcraft promote`) | **Shipped, `--apply` gated** | Dry-run lists workhorse `procedure`/`directive` lessons. `--apply --id` writes a local SKILL.md or AGENTS.md snippet and a `lesson_promoted` audit row. Never auto-promotes. See [docs/promote-checklist.md](docs/promote-checklist.md). |
| **Cue-aware ranking as the ranking authority** | **Stubbed / gated** | `cue_match_factor` exists and is summed in, but is not yet the primary ranker. |
| **`dynamic_confidence` used in retrieval ranking** | **Stubbed / gated** | Shadow mode — behind `FIELDCRAFT_DYNAMIC_CONF` (default OFF). Loop records, does not steer. |
| Learned retrieval scorer / reranker | **Not built** | Long-term item below; requires ≥5000 outcome triples before training is viable. |
| LLM cue classification (`phase` / `user_state`) | **Stubbed** | Deterministic cues only; the Haiku classifier is not wired. |
| Time-decay of confidence | **Planned** | Not implemented. |

**The headline:** the feedback loop is closed and recording real outcomes (this is *not* dead — observe via `fieldcraft stats`, never a client's local DB). The store is no longer append-only — lifecycle L1-L4 evict, protect, and self-correct in production. What remains is turning the recorded outcome signal into ranking influence (flip dynamic confidence out of shadow mode, make cue-aware ranking authoritative), hardening the usefulness evidence behind dream, and — much further out — a learned ranker once enough outcome data accumulates.

---

## North Star Metric

**Session lift** — percent of agent coding sessions that contain ≥1 confirmed retrieval (the retrieved record was demonstrably used by the agent in subsequent reasoning or tool calls).

- 0-2 weeks of dogfooding: noise floor; do not draw conclusions
- 30 days: target ≥50% session lift
- 60 days: target ≥70% session lift

Secondary signal: average rank of confirmed retrievals — sharp retrieval pushes confirms to ranks 1-2.

---

## Public launch track (from the 2026-09 review)

Source: an independent technical and positioning review before the repo went public, summarized in
[docs/research/2026-09-public-launch-review.md](docs/research/2026-09-public-launch-review.md).
Done items are marked; the rest are ordered by priority within each group.

### Before launch

- [x] Scrub private infrastructure from the tree; drop private and third-party eval data.
- [x] `/enroll` gated to loopback or a shared secret (it was open on a `0.0.0.0` bind).
- [x] `install.sh` carries `GEMINI_API_KEY` into the server's launchd env (owner-only plist).
- [x] `fieldcraft write` accepts all five documented kinds (was claim/procedure only).
- [x] Public wording fixed: exact cosine not "ANN", "not the ranking authority" not "not used",
      no "lessons get sharper with use", correct Shen et al. citation, no web-UI claim.
- [ ] Publish with fresh git history (history still holds private paths and an old token).
- [ ] Distiller default: `codex exec --dangerously-bypass-approvals-and-sandbox` runs on transcript
      text, which is a prompt-injection surface. Default new installs to `none` or a read-only sandbox
      and make the bypass opt-in.
- [x] Embedding dependency: add a preflight / `fieldcraft doctor` line and either a local embedding
      option or an explicit BM25-only mode, instead of silently degrading (CH-0012: local
      EmbeddingGemma 2 provider, `FIELDCRAFT_EMBED_PROVIDER=none`, `/status` embedding block).
- [ ] Linux install path (systemd user unit or plain local mode); stop requiring `python3`.
- [x] Interactive onboarding: `fieldcraft setup` (detect agents, local vs join-a-server, embedding
      key into `secrets.env`, distiller choice, hooks, then a round-trip check) and a
      non-interactive `fieldcraft doctor`. `install.sh` hands over to it on a terminal.
- [ ] Decide the public scope of the `/memories`, `/people`, `/views/*` API and the skills
      subsystem: document as experimental or feature-gate out.
- [ ] Remove stale artifacts: root `hooks/` legacy Stop hook, broken `docs/research/human-memory-recall.md` links.
- [ ] Only citation-sourced confirms (not basename `file_overlap`) may promote a provisional dream
      claim and demote its sources.

### Make the differentiation provable

- [ ] Precision audit: hand-label ~200 recent outcome events by source (citation, file_overlap,
      ignored, contradicted); tighten `file_overlap` to full paths and include files committed during
      the session, not only the dirty tree.
- [ ] Take `dynamic_confidence` out of shadow with online interleaving (arm-tagged `retrieval_log`,
      compare confirm rate per arm), keeping `FIELDCRAFT_DYNAMIC_CONF` as the kill switch.
- [ ] Measure usefulness on the real product path: distiller-written lessons, retrieval through the
      hooks, several repositories, repeated runs per arm, and a memory-suppressed control.
- [ ] Golden retrieval eval (recall@30, MRR@10, precision@5, noise@5, abstain accuracy) gating scoring
      changes in CI.
- [ ] Compute the North Star (session lift) in `fieldcraft stats`.
- [ ] Wire `citation_match.rs` as a second confirm source for paraphrased use.
- [x] Judgment-model reranker, tested with TypeSafe Jev on 865 logged queries (2026-09-27,
      `eval/jev_rerank/README.md`): on its own it ranks cited lessons lower than the current order in
      all four framings; blended into the served order it adds +2–3 points hit@1 and MRR (CI excludes
      zero, holds on a held-out half). Optional blended feature is the only worthwhile follow-up;
      public evidence says post-hoc "was this memory used?" judgments are its weak spot.
- [ ] Dream review queue with measured acceptance rate.

### Longer term

- [x] Move the Gemini call out from under the repository mutex; cache vectors in memory (CH-0011:
      read-connection pool, trigger-invalidated record cache; RRF ties made deterministic).
- [x] Pluggable embedding providers with model tagging (CH-0012).
- LLM cue classifier for phase / user_state.
- Trim the public CLI surface; split the 6k-line `main.rs`.

---

## Near-term (next 7 weeks)

### Cue-first retrieval + closed-loop feedback (CH-0002, CH-0004)

Unified 7-phase migration plan at [docs/plans/cue-bundle-retrieval.md](docs/plans/cue-bundle-retrieval.md). Implements:

- **Cue bundle** — situational features captured at write time and at retrieval time. The model now includes first-class typed cues (`identity`, `task`, `domain`, `technical`, `file`, `entity`, `symbol`, `temporal`, `outcome`) with canonical/alias payloads. Multiplicative cue scoring plus weighted task grounding implements Tulving & Thomson (1973) encoding specificity.
- **Closed-loop feedback** — every retrieval event tagged with its cue bundle and outcome; outcome events (cited / ignored / contradicted) extracted by a post-session trace analyzer; `confidence` becomes dynamic, updating via testing-effect-style asymptotic Bayesian rule (Roediger & Karpicke 2006).

The two ship as one schema migration because closed-loop telemetry tuples need to be `(record, cue_bundle, outcome)` — without `cue_bundle` the feedback signal loses context-conditional efficacy.

**Phases** (~7 weeks solo):
1. Schema + deterministic cue extraction + backfill (week 1) — **shipped (CH-0002)**
2. Cue-aware retrieval pipeline (weeks 2-3) — **shipped (CH-0004)**: project identity is cue evidence, not a hard retrieval boundary; identity admission uses `cue_index`; same-identity records need task grounding to rank.
3. LLM cue classifier — Haiku for `phase` + `user_state` (week 3, parallel)
4. Telemetry + feedback API (week 4) — note: significant infrastructure already exists in `feedback` / `lesson_feedback` / `retrieval_log` tables
5. Trace analyzer — deterministic citation match (week 5). Note: a sibling capability shipped in CH-0003 — a non-blocking Stop hook may spawn a detached background distiller (`codex` or `claude`) that writes fresh lessons from completed transcripts. CH-0007 friction-gates that LLM spawn (quiet sessions skip). This is *write-side* auto-distillation, not the *read-side* auto-confirmation of cited retrievals that Phase 5 still needs to deliver (history: CH-0003, CH-0007).
6. Closed-loop scoring updates (after Phase 5 audit ≥80% precision)
7. Observability CLI (`fieldcraft stats`) + weekly digest (weeks 6-7)

### Immediate retrieval-quality follow-up

CH-0004 fixed candidate admission, but deployed verification showed the known Oh Potato ingredient-display memory ranked second behind unrelated app-icon memory. The next pass should treat this as a ranking/evaluation problem, not a plumbing problem:

- add a golden retrieval eval harness with real positive and negative queries;
- report recall@30, MRR@10, precision@5, noise@5, and abstain accuracy;
- tighten task-grounding so identity cues admit candidates but same-identity records do not dominate ranking without task/domain/symbol/file overlap;
- keep the ingredient-display regression as a named fixture until the relevant memory ranks first.

### Open questions to resolve during the spike

- Does the existing `feedback`/`lesson_feedback` schema cover what the plan needs, or do new columns need to be added? (Likely: minor additions to capture `cue_bundle` at retrieval time and structured outcome enum.)
- Should `cue_bundle` live as a column on `knowledge` or a separate normalized table? (Plan says column + separate `cue_index` for inverted lookup; verify against existing access patterns.)
- Does the existing 4-phase enum need to be expanded to capture richer cognitive context (e.g., a `kickoff` / `troubleshoot` distinction not currently represented)? Default: **no, align with existing 4 values.** Expansion only if the spike reveals a concrete miss.

---

## Mid-term (3-6 months)

Items grounded in docs/plans/cue-bundle-retrieval.md (encoding specificity, testing effect) but deferred to land after Phase 7 of the cue-bundle plan ships and accumulates data.

### Co-retrieval graph + Personalized PageRank (HippoRAG analogue)

Gutiérrez et al. 2024 (arXiv:2405.14831) shows +20.3% on MuSiQue from PPR over a co-occurrence knowledge graph. For fieldcraft: build edges between records that *co-confirm* in a session (outcome-weighted, stronger signal than HippoRAG's co-occurrence). Requires Phase 5 telemetry to be live and trusted.

### ~~Retrieval-induced forgetting (RIF) deduplication~~ — Shipped

Shipped as L4 of the memory lifecycle track: `rif_demotion_candidates` demotes the loser of a near-twin pair (cosine 0.85..0.92) that is repeatedly co-served and consistently loses on confirms. Live on the authoritative server. See [ARCHITECTURE.md §9](ARCHITECTURE.md#9-memory-lifecycle-drop-consolidate-dream) and [docs/plans/memory-lifecycle.md](docs/plans/memory-lifecycle.md).

### ~~Episode → Claim consolidation~~ — Built, gated (L5 dreaming)

Shipped as `fieldcraft dream`: clusters of ≥3 same-project/kind lessons with ≥2 confirms are abstracted by an LLM into one derived Claim that must cite ≥3 sources, lands `provisional`, and activates only after a real confirm — the provenance and human-review gate this item called for. Built and dry-run-proven on production; `--apply` stays off until the G1 usefulness sweep clears. See [docs/plans/memory-lifecycle.md](docs/plans/memory-lifecycle.md).

### Reflexion-style failure tagging (extension of Phase 5)

Beyond `cited` / `ignored`, detect explicit contradiction in agent reasoning (Shinn et al. 2023, +11pp on HumanEval). Requires LLM classifier (Haiku). Defers to v2 of the trace analyzer.

---

## Long-term (research-grounded, not scheduled)

### Learned retrieval scorer

Once ≥5000 `(query_cue_bundle, record_cue_bundle, outcome)` triples accumulate, train a small model to predict `P(useful | record, cue_bundle)` and replace hand-tuned 12-feature weights with learned ones. This is the "neocortical slow learning" piece of CLS theory — each fieldcraft instance becomes uniquely tuned to its user's work patterns.

### Reconsolidation-style retrieve-and-update

Nader et al. (2000). Reactivated memories enter a labile state; updates within the window become incorporated. Software analogue: post-session, prompt the agent to confirm / update / retract recently retrieved records, with append-only versioning via `superseded_by` pointers. Already partially supported by the existing `Supersedes` relation. **Misinformation effect risk** (Loftus & Palmer 1974) — human approval required for any retraction or substantive modification.

### Reconstructive multi-record synthesis

Schacter & Addis (2007). Memory is construction, not playback. For queries where no single record directly answers, synthesize from top-K with explicit `[inferred]` tagging on unsourced spans. **DRM false memory risk** (Roediger & McDermott 1995) — defer until provenance metadata is solid in production.

---

## Explicitly Out of Scope

These have been considered and rejected (or deferred indefinitely):- **State-dependent memory** (Eich 1980). No analogue of agent "mood"; the closest proxy (phase/task) is already captured by phase tagging.
- **Sleep-staging consolidation timing.** SWS-specific neurochemistry has no software analogue. The offline-batch principle is adopted; the specific biological mechanism is not.

---

## Cross-References

- Current implementation: [ARCHITECTURE.md](ARCHITECTURE.md)
- Memory lifecycle plan (drop, consolidate, dream): [docs/plans/memory-lifecycle.md](docs/plans/memory-lifecycle.md)
- Research foundation: docs/plans/cue-bundle-retrieval.md (encoding specificity, testing effect)
- Active migration plan: [docs/plans/cue-bundle-retrieval.md](docs/plans/cue-bundle-retrieval.md)
- Change log: [change_history/](change_history/)
