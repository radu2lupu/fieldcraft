# CH-0003: CLI-only migration + background distiller + HTTP retrieval logging

**Date**: 2026-05-18
**Type**: implementation

## What changed

Four interlocking changes shipped as one architectural shift toward a single-source, non-blocking learning loop:

1. **CLI-only migration** (commit 058c968). Deleted the entire `crates/fieldcraft-mcp/` crate (~2k lines) and the `fieldcraft serve` subcommand. Added `fieldcraft feedback <id> --outcome ...` which calls `POST /lessons/{id}/feedback`. Agents now interact with fieldcraft exclusively through the `fieldcraft` CLI; the CLI honors `FIELDCRAFT_SERVER_URL` and proxies every command over HTTP when set. `install.sh` step 5 (MCP registration) was removed; `install/bin/merge-mcp.sh` deleted; instruction files (`install/instructions/claude.md`, `codex.md`) rewritten for CLI commands. Workspace `Cargo.toml` and `crates/fieldcraft-cli/Cargo.toml` updated.
2. **Stop hook converted from blocking to non-blocking** (commits cd7a67f, c157369, f7a3bc8). The old hook emitted `decision: block` JSON every 30 minutes, eating a turn each time. The new `install/bin-hooks/stop-feedback.sh` always writes a session-end marker to `~/.fieldcraft/state/session-ends.jsonl` and exits 0. If a coding-agent CLI is on PATH and the transcript is ≥10KB, the hook spawns a detached background distiller (`codex exec --dangerously-bypass-approvals-and-sandbox` or `claude --print --dangerously-skip-permissions --max-budget-usd 0.10 --no-session-persistence`) that reviews the transcript and runs `fieldcraft write` only when a non-obvious, durable finding is present.
3. **Configurable distiller** (commit f7a3bc8). New `FIELDCRAFT_DISTILLER` env var (sourced from `~/.fieldcraft/env`) accepts `codex | claude | none | unset(auto)`. Auto-detect order: codex preferred, then claude, then marker-only.
4. **Server-side HTTP retrieval logging + route merge** (commit 0db2e6a). `crates/fieldcraft-server/src/lib.rs` had two separate `.route("/lessons", post(...))` and `.route("/lessons", get(...))` calls that silently overrode each other in axum 0.8 — fixed by combining to `.route("/lessons", post(...).get(...))`. Also wired a fire-and-forget `log_retrieval()` call inside `search_lessons_handler` so HTTP retrievals are now visible to `retrieval_log` on the server — previously only local-mode CLI queries were logged.

End-to-end verified: a client laptop connects via Tailscale Serve to the shared server (host redacted for the public release), `fieldcraft write` lands on the server with correct token label, `fieldcraft query` returns ranked results and appears in `retrieval_log` with latency, per-client token labels are visible in `tr_tokens`. 230 workspace tests pass; `clippy -D warnings` clean. The background distiller produced real lessons during the debugging session itself.

## Why

The MCP stdio server on client machines was silently writing to a *local* SQLite catalog regardless of `FIELDCRAFT_SERVER_URL`, producing a split-brain between client and server. Deleting the MCP path made the CLI the single source of truth: one client, one transport (HTTP), one catalog. The route-merge bug in axum 0.8 meant the silent split-brain was masked further — half the operations succeeded against the server, half hit a 405 — and was only diagnosable once we added retrieval logging to the server-side handler.

The Stop-hook rewrite addresses a different class of problem: the blocking design was paying a per-session turn cost for *prompting* the agent to write a lesson, regardless of whether anything worth writing happened. Pushing the decision into a detached agent CLI lets the lesson-write loop close itself when there's signal and stay silent otherwise — with zero cost to the foreground agent's turn budget. `FIELDCRAFT_DISTILLER` exists because the auto-detect logic needs an explicit escape hatch (disable on shared machines; force one tool when both are installed).

## Affects

- ARCHITECTURE: §1 Workspace Layout (drop `fieldcraft-mcp` row); §6 Server (note HTTP retrieval logging now active for remote clients); new short subsection on the session-end distiller and `~/.fieldcraft/state/session-ends.jsonl` marker
- ROADMAP: Near-term Phase 5 (trace analyzer / auto-confirm) note — background auto-distillation is now shipped as a sibling capability; citation-based auto-confirmation of retrievals remains pending
- README: yes — extend "Agent setup" with the non-blocking Stop hook, the background distiller, and `FIELDCRAFT_DISTILLER`

## Lesson (optional)

—
