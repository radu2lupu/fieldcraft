#!/usr/bin/env python3
"""Smoke test for ClaudeAgentSolver.

Tests that the end-to-end pipeline (repo clone -> test_patch apply ->
claude invocation -> git diff capture) completes without crashing and
returns a valid SolverResult.

This test does NOT attempt to solve the task — it passes a trivial prompt
that instructs Claude not to edit any code, so it runs fast.

Usage:
    CLAUDE_CODE_OAUTH_TOKEN=<token> python scripts/solvers/test_smoke.py

Exit code: 0 on pass, 1 on failure.
"""
from __future__ import annotations

import json
import os
import sys
import time
from pathlib import Path

# Allow running directly without installing the package.
_HERE = Path(__file__).parent
sys.path.insert(0, str(_HERE.parent))

from solvers import SolverResult, get_solver

# ── hardcoded task ─────────────────────────────────────────────────────────────
DATASET_PATH = (
    Path(__file__).parent.parent.parent
    / "eval"
    / "patch_planning"
    / "heldout_50"
    / "dataset.jsonl"
)
TARGET_INSTANCE_ID = "django__django-13363"

# Trivial prompt: read only, do not edit code.
SMOKE_PROMPT = (
    "Read the problem statement for this task.  "
    "Do NOT edit any source files.  "
    "Output a single sentence describing the issue and then stop."
)

SMOKE_MODEL = "claude-sonnet-4-6"


def load_task(dataset: Path, instance_id: str) -> dict:
    with dataset.open() as fh:
        for line in fh:
            row = json.loads(line)
            if row.get("instance_id") == instance_id:
                return row
    raise SystemExit(
        f"Instance {instance_id!r} not found in {dataset}.  "
        "Ensure eval/patch_planning/heldout_50/dataset.jsonl is present."
    )


def main() -> None:
    if not os.environ.get("CLAUDE_CODE_OAUTH_TOKEN"):
        print("FAIL: CLAUDE_CODE_OAUTH_TOKEN is not set in the environment.", file=sys.stderr)
        sys.exit(1)

    if not DATASET_PATH.exists():
        print(f"FAIL: dataset not found at {DATASET_PATH}", file=sys.stderr)
        sys.exit(1)

    print(f"Loading task {TARGET_INSTANCE_ID} from {DATASET_PATH} ...")
    task = load_task(DATASET_PATH, TARGET_INSTANCE_ID)
    print(f"  repo={task['repo']}  base_commit={task['base_commit'][:12]}")

    solver = get_solver("claude")

    # Use a temp dir under /tmp so we don't litter the repo.
    import tempfile
    with tempfile.TemporaryDirectory(prefix="smoke_test_") as tmp:
        cwd = Path(tmp) / TARGET_INSTANCE_ID
        cwd.mkdir()

        print(f"Running ClaudeAgentSolver (model={SMOKE_MODEL}, cwd={cwd}) ...")
        t0 = time.monotonic()
        result: SolverResult = solver.solve(
            task=task,
            prompt=SMOKE_PROMPT,
            cwd=cwd,
            model=SMOKE_MODEL,
        )
        elapsed = time.monotonic() - t0

    # ── assertions ────────────────────────────────────────────────────────────
    valid_statuses = {"ok", "timeout", "error", "empty_patch"}
    assert result.status in valid_statuses, (
        f"result.status={result.status!r} not in {valid_statuses}"
    )
    assert isinstance(result.patch, str), "result.patch must be str"
    assert isinstance(result.latency_s, float), "result.latency_s must be float"
    assert result.latency_s >= 0, "result.latency_s must be non-negative"
    assert isinstance(result.raw_log, str), "result.raw_log must be str"
    assert len(result.raw_log.encode("utf-8", errors="replace")) <= 50 * 1024 + 100, (
        "raw_log exceeds 50 KB limit"
    )

    # ── report ────────────────────────────────────────────────────────────────
    print()
    print("=" * 60)
    print("SMOKE TEST PASSED")
    print(f"  status       : {result.status}")
    print(f"  latency_s    : {result.latency_s:.1f}s  (wall: {elapsed:.1f}s)")
    print(f"  prompt_tokens: {result.prompt_tokens}")
    print(f"  output_tokens: {result.output_tokens}")
    print(f"  patch lines  : {len(result.patch.splitlines())}")
    if result.error_msg:
        print(f"  error_msg    : {result.error_msg}")
    print(f"  raw_log size : {len(result.raw_log.encode('utf-8', errors='replace'))} bytes")
    print("=" * 60)


if __name__ == "__main__":
    main()
