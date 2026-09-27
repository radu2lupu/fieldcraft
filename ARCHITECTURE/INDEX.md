# Architecture

This directory is the required durable-state architecture entrypoint.

The current architecture document is [../ARCHITECTURE.md](../ARCHITECTURE.md). It remains at the repo root for compatibility with existing links and review prompts. Deployment and observability live in [../docs/OPERATIONS.md](../docs/OPERATIONS.md).

## Read this before auditing

In shared-server mode the authoritative store is the server's `~/.fieldcraft/memory.sqlite3`. Clients (and any local gateway) may hold a stale local SQLite that must never be used to judge the system. Observe via `fieldcraft stats` (authoritative `/stats`). See ARCHITECTURE.md §0 and OPERATIONS.md.

## Current Focus

The feedback loop is closed and recording outcomes, but runs in **shadow mode**: `dynamic_confidence` is computed and stored, gated behind `FIELDCRAFT_DYNAMIC_CONF` (default OFF), so it does not yet steer retrieval. Cue-first admission shipped (CH-0004): identity cues admit candidates, task/domain/symbol/file cues rank them; `project_id` is provenance metadata, not a retrieval partition.

The store is no longer append-only: the memory lifecycle (drop, consolidate, dream) is **live** on the reference shared-server deployment — protection, scheduled eviction, and resurrection all run in production; `fieldcraft dream` (generative consolidation) passed its G1 usefulness gate on 2026-07-02 and runs nightly, landing derived claims as `provisional` until confirmed. See [../ARCHITECTURE.md §9](../ARCHITECTURE.md) and [../docs/plans/memory-lifecycle.md](../docs/plans/memory-lifecycle.md).

The next architecture work:

- take `dynamic_confidence` out of shadow mode after a precision audit of outcome events;
- make cue-aware ranking the ranking authority (currently `cue_match_factor` is only summed in);
- add a golden retrieval eval harness before further scoring changes.
