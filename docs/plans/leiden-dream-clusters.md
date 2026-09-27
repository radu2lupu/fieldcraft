# Plan: Leiden Community Detection for Dream-Tier Clustering

## Status
- Owner: Radu (orchestrated via Claude Code, implemented via Codex)
- Last updated: 2026-07-02
- Current phase: Implemented and verified; PR open
- Decision log:
  - 2026-07-02: Rejected adopting graphrag-rs as a library (lesson:557bfb33);
    lifting Leiden as an in-house ~250-line module instead.
  - 2026-07-02: CPM resolution fixed at 0.40, derived from the constraint
    window below (not a magic number).

## Problem

`KnowledgeRepository::dream_clusters` (crates/fieldcraft-storage/src/repository.rs,
~line 1349) feeds the nightly L5 dreaming pass. It clusters active lessons by
union-find over pairwise dense-cosine edges in `[0.80, 0.92)`. Union-find over
similarity edges is single-linkage clustering, which suffers *chaining*: if
A~B, B~C, C~D are each in the band but A·D is topical noise, all four land in
one cluster. The nightly dream then synthesizes one derived Claim spanning
unrelated lessons — a mushy abstraction with misleading provenance. The defect
is currently latent (small corpus, few clusters) and will surface as lesson
density grows around hot topics. Nightly dreaming shipped 2026-06 (commits
5429fab, 25d3b5b), so the window to fix this before bad derived Claims
accumulate is now.

## Scenarios

1. Nightly `fieldcraft dream --apply` runs over a corpus where a dense group
   of 4+ genuinely-related lessons has a "tail" of 1-2 lessons attached by a
   single bridge edge. The dense group is dreamed; the tail is excluded.
2. A semantic chain of 5 lessons (each adjacent pair in-band, non-adjacent
   pairs out-of-band) produces NO single 5-member cluster.
3. A minimal coherent triple (the existing test fixture: adjacent cosines
   0.866) still clusters and still gets dreamed — no regression on the
   smallest legitimate unit.

## Scope

- Replace the union-find partition step inside `dream_clusters` with Leiden
  community detection (CPM quality function) over the same weighted edge
  graph (weights = cosine similarity, edges only where cosine ∈ [0.80, 0.92)).
- New module `crates/fieldcraft-storage/src/leiden.rs`, pure function, unit
  tested in isolation plus through `dream_clusters`.
- Keep `dream_clusters`' signature, grouping by `(project_id, kind)`,
  `min_size` / `min_confirmed` / `max_clusters` filters, signal ordering, and
  the READ-ONLY property exactly as they are.

## Non-Goals

- No change to `insert_derived_claim`, the dream CLI (`cmd_dream`), the
  merge queue, RIF demotion, or any retrieval path.
- No new workspace dependencies (no petgraph, no graphrag-rs, no rand usage —
  the algorithm must be deterministic).
- No change to the [0.80, 0.92) edge band or the O(n²) per-group edge
  construction (groups are small; not the bottleneck).
- No hierarchy of communities (single-level partition only).

## Current Evidence

- Implementation: `dream_clusters` at repository.rs:1349-1453; union-find at
  1406-1432.
- Existing test: `dream_clusters_finds_confirmed_related_group`
  (repository.rs:~4683) — a 3-chain at angles 0°/30°/60°; the comment itself
  notes "union-find chains all three into one cluster".
- Chaining math: on a 5-path with edge weights 0.85, single-linkage returns
  one 5-cluster; the path has no dense subcommunity, so any modularity/CPM
  partition splits it.
- Provenance guardrail (`insert_derived_claim` requires ≥3 sources) does NOT
  protect against chaining — a chained 6-cluster passes it trivially.

## Requirements

R1. Partition quality: communities must be well-connected; no community may
    be held together by a single bridge edge (the Leiden guarantee).
R2. Determinism: identical DB state → identical clusters, byte-for-byte, on
    every run and platform. No RNG. Iterate nodes in stable index order;
    break quality ties by lowest community index.
R3. Backward compatibility: the existing triple test passes unchanged.
R4. The K4+tail scenario (below) returns exactly the K4.
R5. The 5-path scenario returns no cluster containing both endpoints.
R6. No new dependencies; `cargo clippy` clean (`-D warnings` per repo CI);
    `cargo fmt` clean.
R7. Performance: no worse than 2x current wall time on a 500-lesson group
    (informal bound; algorithm is near-linear in edges after the existing
    O(n²) edge build, so this is a non-issue in practice).

## Architecture

### Quality function: CPM (Constant Potts Model)

For partition P over weighted graph (V, E, w):

    Q(P) = Σ_{c ∈ P} [ W_internal(c) − γ · n_c(n_c−1)/2 ]

where W_internal(c) is the sum of edge weights inside community c, n_c is the
community size, and γ is the resolution. CPM is preferred over modularity:
no resolution-limit pathology, and the γ threshold has a direct reading —
a group merges only if its internal weight density exceeds γ per node pair.

### Resolution γ = 0.40, derived

Edge weights live in [0.80, 0.92). Constraints:

- Existing triple test (path of 3, w≈0.866) must merge:
  merge iff 2w − 3γ > w − γ  ⟺  γ < w/2 ≈ 0.433.
- Tail must NOT attach to a K4 (one edge w=0.85 into a community of 4, which
  adds 4 penalty pairs): reject iff 0.85 − 4γ < 0  ⟺  γ > 0.2125.
- Whole 5-path (4 edges, 10 pairs) must not beat its splits: satisfied
  everywhere in the window above (3.4 − 10γ < best split for γ > ~0.28).

Window: γ ∈ (0.2825, 0.433). Chosen: `const DREAM_CPM_RESOLUTION: f32 = 0.40`
with this derivation in the const's doc comment.

### Algorithm (single-level Leiden, deterministic)

Module `crates/fieldcraft-storage/src/leiden.rs`:

```rust
/// Partition a small weighted undirected graph into well-connected
/// communities maximizing CPM quality at the given resolution.
/// Deterministic: stable iteration order, index-based tie-breaking.
/// nodes are 0..n; edges are (u, v, weight) with u < v.
pub fn leiden_cpm(n: usize, edges: &[(usize, usize, f32)], resolution: f32) -> Vec<Vec<usize>>
```

Phases, iterated until no quality gain (cap 10 outer iterations):
1. Local moving: visit nodes 0..n in order; move each node to the adjacent
   community with the highest positive CPM gain (ties → lowest community id).
   Repeat sweeps until a full sweep makes no move.
2. Refinement: within each community, re-run constrained local moving
   starting from singletons, only merging within the community. This is the
   step that breaks bridge-connected communities and gives R1.
3. Aggregation: collapse each refined community to a super-node (edge weights
   summed, intra-community weight tracked) and repeat from phase 1 on the
   aggregate graph. Stop when aggregation is a no-op.

Given per-group sizes (tens to low hundreds), no priority queues or
optimization tricks — straightforward loops, plainly written.

### Integration

In `dream_clusters`, replace the union-find block (repository.rs:1406-1432)
with: build the same edge list (indices into `members`, weight = cosine),
call `leiden_cpm(n, &edges, DREAM_CPM_RESOLUTION)`, map communities back to
record ids. Everything before (grouping, confirmed counts) and after
(min_size, min_confirmed signal sum, sort by signal desc, max_clusters cap)
is untouched. Update the function's doc comment (it currently says
"union-find") and the L5 section of docs/plans/memory-lifecycle.md.

### Rollback

Single self-contained commit on a branch; revert restores union-find. The
signature is unchanged, so callers (cmd_dream, tests) never know.

## Implementation Stages

1. `leiden.rs` with `leiden_cpm` + in-module unit tests (triangle merges,
   two cliques joined by one bridge edge split, empty/singleton graphs,
   determinism across repeated calls).
2. Wire into `dream_clusters`; delete union-find block; update doc comments
   and memory-lifecycle.md L5 note.
3. Repository-level tests (below).
4. `cargo fmt`, `cargo clippy --all-targets -- -D warnings`,
   `cargo test -p fieldcraft-storage`, then full `cargo test`.

## Testing Plan

All vectors hand-constructed so pairwise cosines are exact; all inserted via
the existing `make_record` + `store_dense_vector` test helpers, mirroring
`dream_clusters_finds_confirmed_related_group`. Seed confirmed outcomes the
same way that test does (2 'confirmed' rows on one member).

T1 (regression, exists): `dream_clusters_finds_confirmed_related_group`
    passes UNCHANGED. This pins the lower bound of γ.

T2 (chaining, new) `dream_clusters_excludes_bridge_tail_from_clique`:
    7-dimensional vectors. Let α=0.85, a=√α≈0.92195, b=√(1−α)≈0.38730.
    Clique: v_i = a·e1 + b·e_{1+i} for i=1..4 → all clique pairs have
    cosine exactly 0.85 (in band).
    Tail: w1 = 0.85·v1 + √(1−0.7225)·e6 → cos(w1,v1)=0.85 (in band),
    cos(w1,v_j)=0.7225 (out of band, j≠1);
    w2 = 0.85·w1 + √(1−0.7225)·e7 → cos(w2,w1)=0.85 (in band),
    cos(w2,v1)=0.7225, cos(w2,v_j)≈0.614 (out of band).
    Single-linkage would return one 6-cluster. Assert: exactly one cluster,
    sorted ids == [v1, v2, v3, v4]; w1, w2 absent.

T3 (long chain, new) `dream_clusters_does_not_chain_across_path`:
    2D unit vectors at angles 0°, 32°, 64°, 96°, 128° (adjacent cosine
    cos 32° ≈ 0.848 in band; skip-one cos 64° ≈ 0.438 out of band).
    Assert: no returned cluster contains both the 0° and 128° records, and
    no cluster has ≥4 members.

T4 (determinism, new): run `dream_clusters` twice on the T2 fixture; assert
    identical output. Plus a leiden.rs unit test calling `leiden_cpm` 5 times
    on the two-cliques-one-bridge graph asserting identical partitions.

Acceptance evidence to capture in the PR: full `cargo test` output, clippy
output, and the T2/T3 assertions demonstrating the old behavior is gone
(optionally: show the old union-find returns the 6-cluster by running T2
against main).

## Review Plan

- Wrong-problem: does the K4+tail test actually encode the chaining defect?
  (Yes — it is the minimal case where provenance guardrails pass but the
  cluster is semantically broken.)
- Regression: T1 unchanged; `cmd_dream` behavior identical for well-separated
  cliques; no schema or signature change.
- Performance: leiden adds near-linear passes after the existing O(n²) edge
  build; verify no accidental O(n³) in the aggregation loop.
- Complexity: one module, one const, zero deps. If the diff grows past ~400
  lines of non-test code, something went wrong — push back.
- Evidence: reviewer re-runs tests locally; PR review workflow runs
  automatically on open.

## Definition Of Done

- [ ] `leiden_cpm` exists, documented, deterministic, no new deps
- [ ] `dream_clusters` uses it; union-find block deleted; doc comments and
      memory-lifecycle.md updated
- [ ] T1 passes unchanged; T2, T3, T4 pass
- [ ] `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
      full `cargo test` green, run by the orchestrator (not taken on faith)
- [ ] PR opened from feat/leiden-dream-clusters with acceptance evidence;
      automatic Claude review has no unaddressed [blocking] findings

## Agent Handoff Packet

Objective: implement exactly Stages 1-4 of this plan in
the fieldcraft repo on branch feat/leiden-dream-clusters.

Read first: this file; crates/fieldcraft-storage/src/repository.rs lines
1342-1460 (dream_clusters, insert_derived_claim) and the existing test
dream_clusters_finds_confirmed_related_group (~line 4683) whose fixture
style T2-T4 must mirror; docs/plans/memory-lifecycle.md L5 section.

Constraints (forbidden regressions):
- dream_clusters signature, filters, ordering, READ-ONLY property unchanged.
- dream_clusters_finds_confirmed_related_group must pass WITHOUT edits.
- No new workspace dependencies. No RNG anywhere in the new code.
- Comment style: match the crate (sparse, doc-comments on public items,
  constraint-stating inline comments only).

Commands to run before claiming done:
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test

Evidence to return: the diff summary, full test output (including the new
test names), clippy output, and one sentence per DoD checkbox.

## Change Log

- 2026-07-02: Initial plan. Source analysis: graphrag-rs evaluation
  (lesson:557bfb33); chaining defect identified in dream_clusters
  single-linkage union-find; γ window derived from existing test + K4-tail +
  5-path constraints.
- 2026-07-02: Implemented by Codex per the handoff packet; verified
  independently by orchestrator (clippy -D warnings clean, full cargo test
  green, 130 storage tests incl. T1 unchanged + T2/T3/T4). Deviations from
  spec: none material. Codex additionally made the final cluster sort
  deterministic across (project, kind) groups by tie-breaking on member ids
  (strengthens R2 beyond the letter of the spec). Orchestrator reverted
  four fmt-only file reformats that were out of scope.
