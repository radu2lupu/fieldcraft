# Dream Guardrails And Server Write Path Plan

## Status
- Owner: Codex
- Last updated: 2026-07-06
- Current phase: Implementation and verification
- Decision log: Keep dream idempotence in storage; keep maintenance endpoints read-only; index server writes best-effort after durable insert.

## Problem

Pre-Leiden dream synthesis produced oversized derived claims with 44 to 135 sources. Those provisional blobs could shadow every later tighter dream cluster and, if confirmed, demote every cited source into archive in one transaction.

The server also became the authoritative write path, but it inserted knowledge without dense indexing. Most servable records therefore had `dense_vector = NULL`.

Remote clients could not run read-only `analyze` or lifecycle dry-runs against the authoritative server catalog.

## Scenarios

- Derived claim creation accepts 3 to 24 sources and rejects larger source sets before writing.
- A dream cluster is considered already dreamed only when one live derived claim cites more than half of that cluster.
- Archived or superseded derived claims do not shadow future dream clusters.
- Server-created lessons and derived claims attempt the same dense-vector indexing as local CLI writes.
- Embedding failure after a server write is logged and does not fail the write.
- Remote `fieldcraft analyze` and lifecycle dry-run call authenticated server endpoints and render the usual output.
- Remote lifecycle `--apply` remains refused.

## Scope

- Storage guardrails and shared dream idempotence.
- Server write-path best-effort indexing.
- Authenticated read-only `/analyze` and `/lifecycle` endpoints.
- CLI proxying for remote analyze and lifecycle dry-run.
- Focused storage and server tests.

## Non-Goals

- No one-time production reindex.
- No mutation endpoints for lifecycle apply or dream apply beyond existing `/dream`.
- No changes to `encode_dense` internals.
- No changes to Leiden clustering parameters.

## Current Evidence

- 2026-07-02 production dream blobs cited 44 to 135 sources.
- Existing guard counted any `derived_from` row across any member.
- Existing promotion path demotes derived sources on confirmation.
- Prior Fieldcraft lesson shows the server write path skipped embedding, causing dense-vector NULL records: lesson:b17e6fbe880460f5bb741caef6b9fdce6956840fb65d7af02c8663bba0dccfaa.

## Requirements

- Source count bound is `3..=24` in storage and HTTP validation.
- Majority overlap is per live derived claim, not pooled across claims.
- Live derived claim means `state != 'superseded'` and `tier != 'archive'`.
- Indexing must not hold the server repository mutex while embedding.
- Indexing failure must not roll back a successful write.
- Maintenance endpoints require the same bearer middleware as `/lessons`.
- Lifecycle apply remains local-only.

## Architecture

- `fieldcraft-storage` owns `dream_cluster_already_dreamed`, analysis reports, lifecycle dry-run reports, and text renderers.
- `fieldcraft-server` locks the repository only for DB reads/writes, then opens a fresh repository in a blocking indexing task.
- `fieldcraft-cli` renders shared report structs locally or after remote JSON fetches.

## Implementation Stages

- Add storage constants, source bound, and majority overlap query.
- Move analyze/lifecycle dry-run computation and rendering into storage.
- Wire server endpoints and best-effort indexing.
- Wire CLI remote proxying and local shared report calls.
- Add storage and server tests.
- Run `cargo fmt`, build, tests, and clippy.

## Testing Plan

- Storage unit tests for 24-source acceptance, 25-source rejection, archived derived non-shadowing, minority non-shadowing, and one live majority shadowing.
- Server tests for maintenance endpoint auth/JSON, source cap BadRequest, and write success with optional dense-vector result.
- Full workspace verification: build, test, clippy.

## Review Plan

- Check wrong-problem risk: oversized blobs are blocked and old archived blobs no longer shadow.
- Check regression risk: CLI local and remote output paths use one renderer.
- Check performance risk: server embeddings run outside the mutex.
- Check safety risk: no lifecycle mutating endpoint exists.

## Definition Of Done

- Branch contains a focused commit and no push.
- `cargo build --workspace` passes.
- `cargo test --workspace` passes.
- `cargo clippy --workspace --all-targets -- -D warnings` passes.

## Agent Handoff Packet

Implement the scoped guardrail, indexing, and read-only maintenance endpoint changes in this repo. Do not push. Preserve local-only lifecycle apply. Verify with full workspace build, test, and clippy. Return touched files and residual risks.

## Change Log

- 2026-07-06: Plan created with implementation scope and verification gates.
