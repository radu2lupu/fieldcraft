# Status — Fieldcraft

Orientation layer for anyone (human or agent) starting work here. This file holds the
**volatile, judgment** part of project context: where we're headed, where it runs, and the
conventions. For a **live** snapshot of in-flight work and infra, run `scripts/status.sh`.

> Keep this short. Durable structure lives in [ARCHITECTURE.md](ARCHITECTURE.md); planned
> direction in [ROADMAP.md](ROADMAP.md); what changed in [change_history/](change_history/);
> cross-session coding lessons in Fieldcraft itself. This file is only the "right now" orientation —
> not a changelog, not a lesson store.

_Last updated: 2026-09-27 · on `chore/public-release-prep`._

---

## Current direction

**The closed-loop feedback redesign shipped.** The June 2026 audit that found the loop dead (0
outcome events, `dynamic_confidence` NULL on every record, store only accreting) is resolved:
CH-0002 through CH-0004 shipped cue-first retrieval and the trace-analyzer closed loop, and the
memory lifecycle track (`docs/plans/memory-lifecycle.md`) shipped the "store only grows" half of
the problem. `dynamic_confidence` is computed and recorded (still shadow mode — see ROADMAP.md).

**Memory lifecycle (drop, consolidate, dream) is LIVE on the reference shared-server deployment.**
Protection, scheduled reversible eviction, and query-triggered resurrection all run in production;
the first production cull archived 41 records. `fieldcraft dream` (generative consolidation, L5)
passed its G1 usefulness gate on 2026-07-02 (n=19) and runs nightly; derived claims land
`provisional` until confirmed. Full
detail: [docs/plans/memory-lifecycle.md](docs/plans/memory-lifecycle.md) and
[docs/OPERATIONS.md](docs/OPERATIONS.md).

- North Star (unchanged): **session lift** ≥50% @ 30d / ≥70% @ 60d — see [ROADMAP.md](ROADMAP.md).
  The lifecycle work's own north star is the G1 warm/cold usefulness delta
  (`eval/usefulness_harness/`), not session-lift coverage.
- Open work: take `dynamic_confidence` out of shadow mode after a precision audit; make cue-aware
  ranking the ranking authority; harden the usefulness evidence (repeated cold draws, a second repo
  family, distilled-quality lessons).
- Public release prep: tree scrubbed, `/enroll` hardened, public README and [docs/COMPARISON.md](docs/COMPARISON.md) written (CH-0008). Before flipping public: fresh history, distiller default, Linux path. See [docs/plans/public-release.md](docs/plans/public-release.md) and the ROADMAP "Public launch track". See ROADMAP.md for the full shipped/stubbed/planned table.

## Where it runs (topology)

Two modes (details in [docs/OPERATIONS.md](docs/OPERATIONS.md)):

| Node | Role | Notes |
|---|---|---|
| Shared server (port 7777) | **Authoritative server** (`fieldcraft-server`, launchd `com.fieldcraft.server` on macOS) | Holds the real catalogue. Lifecycle apply (`FIELDCRAFT_LIFECYCLE_APPLY=1`) and resurrection overrides are set in its launchd environment. |
| Client machines | **Clients**, optionally with a local **gateway** (`fieldcraft-server --upstream-url <server-url>`) | A client's local `~/.fieldcraft/memory.sqlite3` is stale in shared-server mode — do not trust it. Gateways do not run the lifecycle loop. |

Owner-specific hostnames and ssh aliases live in the gitignored `docs/OPERATIONS.local.md`.

**Known gotchas:**
- `query` / `status` / `write` proxy to the server; always use `fieldcraft stats` (not a client's
  local DB) to judge system health — see [docs/OPERATIONS.md](docs/OPERATIONS.md) for the audit trap.
- `~/.fieldcraft/state/distill.log` is unrotated and can balloon (multi-GB observed) — the
  session-end distiller's stdout.
- Restarting the server can hit the port-release race ("Address already in use"); recovery
  is `launchctl bootout` + `pkill fieldcraft-server` + `launchctl bootstrap` — see
  [docs/OPERATIONS.md](docs/OPERATIONS.md).

## Parallel work

**Source of truth is git** — branches and worktrees ARE the parallel-work log, so don't track it by
hand. Run `scripts/status.sh` (or `git worktree list` / `gh pr list`) for the live view. As of last
update: no active concurrent Fieldcraft work; several merged `codex/*` branches in history, no open PRs.

## Conventions

- **Durable facts** (how it's built, the topology above) belong in `ARCHITECTURE.md` §0 — this file just mirrors the volatile slice.
- **Branch + PR is the delivery path.** Never push to `main` unless asked.
- **Cross-session lessons → Fieldcraft**, not this file. Project orientation → this file. Two different layers; don't conflate.
