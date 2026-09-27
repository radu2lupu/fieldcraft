# CH-0005 — Document the memory lifecycle as shipped and live

**Date:** 2026-07-02
**Author:** radu (via Claude)
**Type:** Documentation update
**Updates:** README.md, ARCHITECTURE.md, ROADMAP.md, STATUS.md, docs/OPERATIONS.md, docs/plans/memory-lifecycle.md, ARCHITECTURE/INDEX.md, ROADMAP/INDEX.md

## What changed

No code changed in this pass — durable-state docs were reconciled against the memory lifecycle
work that shipped over 2026-06-30 to 2026-07-02 (commits `65891b9`, `803cade`, `8068716`,
`1814e1c`) and is now live on the authoritative Mac Mini server.

1. **`docs/plans/memory-lifecycle.md`** — Status block and Wave 2-4 sections updated from
   "dispatched, in progress" to reflect the actual shipped state: L1-L4 live in production
   (scheduled eviction, resurrection, RIF demotion), L5 (`fieldcraft dream`) built and
   dry-run-proven but `--apply` still gated on the G1 usefulness sweep.
2. **`docs/OPERATIONS.md`** — new "Memory lifecycle" section: what each of L1-L5 does, the exact
   env vars (`FIELDCRAFT_LIFECYCLE_APPLY`, `FIELDCRAFT_LIFECYCLE_INTERVAL_SECS`,
   `FIELDCRAFT_RESURRECTION`, `FIELDCRAFT_RESURRECTION_COSINE`, `FIELDCRAFT_RESURRECTION_THRESHOLD`)
   and their defaults, the full deploy sequence (git pull + cargo install + launchd
   bootout/bootstrap), and the pre-cull backup location. Also documents the `duplicate_score` fix
   for merging byte-identical HTTP-written records that lack embeddings.
3. **`ARCHITECTURE.md`** — new §9 "Memory Lifecycle" describing L1-L5 with file:line citations;
   Cross-References renumbered to §10.
4. **`README.md`** — added a paragraph noting the store is no longer append-only, and two CLI
   lines (`fieldcraft lifecycle`, `fieldcraft dream`).
5. **`ROADMAP.md`** — shipped/stubbed table gained rows for lifecycle L1-L4 (shipped, live) and L5
   (built, `--apply` gated); the mid-term "RIF deduplication" and "Episode → Claim consolidation"
   items are marked shipped/built rather than left as open mid-term proposals, since both are now
   implemented.
6. **`STATUS.md`** — the "Current direction" section described a stale, unshipped "v6 redesign"
   plan (dated 2026-06-23, linking a since-deleted `docs/fieldcraft-v6-redesign.html`) as the open
   problem. That redesign is what actually shipped as CH-0002 through CH-0004 plus the lifecycle
   track; the section is rewritten to describe the current live state instead of a stale plan.
7. **`ARCHITECTURE/INDEX.md`**, **`ROADMAP/INDEX.md`** — current-focus/next-step blurbs updated to
   mention the lifecycle is live and where `fieldcraft dream --apply` is gated.

## Why

The docs had drifted behind the code: `STATUS.md` still described the closed loop as dead and
pointed at a deleted plan file that predated the very redesign it was describing. `README.md` and
`ARCHITECTURE.md` had zero mentions of the lifecycle system despite it running in production and
having already archived real records and fired a real resurrection. Per the durable-state
convention, code that materially changes system behavior — here, the store gaining eviction,
protection, resurrection, and gated generative consolidation — must land with docs, and the docs
in this repo track shipped-vs-stubbed status explicitly enough that letting them go stale actively
misleads anyone (human or agent) auditing the system.

## Verification

- Every lifecycle claim in this pass was checked against the code before being written, primarily
  in `crates/fieldcraft-storage/src/repository.rs` (`protected`, `is_protected`,
  `served_activation`, `archive_record`/`restore_record`, `archive_candidates`,
  `rif_demotion_candidates`, `dream_clusters`, `duplicate_score`) and
  `crates/fieldcraft-server/src/lib.rs` (`lifecycle_loop`, the resurrection background task in
  `search_lessons_handler`), plus the `Lifecycle`/`Dream` CLI subcommands in
  `crates/fieldcraft-cli/src/main.rs`.
- No `.rs`, `.sh`, `.toml`, or `eval/` files were touched — documentation only, working tree not
  committed.

## Affects

- ARCHITECTURE: new §9 Memory Lifecycle; §10 Cross-References (renumbered from §9).
- ROADMAP: shipped/stubbed table; mid-term section (RIF dedup, Episode→Claim marked shipped/built).
- STATUS: "Current direction" and topology sections rewritten to current state.
