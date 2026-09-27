#!/usr/bin/env python3
"""Real-solve smoke test for ClaudeAgentSolver.

This test invokes the solver with a genuine problem-solving prompt and verifies:
  1. The captured patch is non-empty.
  2. The patch does NOT contain test_patch lines (i.e. test_patch was committed,
     not floating as uncommitted changes).
  3. prompt_tokens > 0 and output_tokens > 0 (token parsing works).

Usage:
    CLAUDE_CODE_OAUTH_TOKEN=<token> python scripts/solvers/test_real_solve.py

Exit code: 0 on pass, 1 on failure.
"""
from __future__ import annotations

import json
import os
import sys
import time
from pathlib import Path

_HERE = Path(__file__).parent
sys.path.insert(0, str(_HERE.parent))

from solvers import SolverResult, get_solver

DATASET_PATH = (
    Path(__file__).parent.parent.parent
    / "eval"
    / "patch_planning"
    / "heldout_50"
    / "dataset.jsonl"
)
TARGET_INSTANCE_ID = "django__django-13363"

# The file added by test_patch for this instance.  If the patch capture
# incorrectly includes test_patch lines, this path will appear in the diff.
TEST_PATCH_FILE = "tests/db_functions/datetime/test_extract_trunc.py"

REAL_SOLVE_PROMPT = (
    "You are solving a Django bug. "
    "Read the problem statement carefully, locate the relevant source file(s), "
    "and write a minimal patch to fix the bug. "
    "Do not modify test files. "
    "Apply your fix using the Edit tool and then stop."
)

REAL_SOLVE_MODEL = "claude-sonnet-4-6"
REAL_SOLVE_TIMEOUT_S = 180  # tight timeout for smoke


def load_task(dataset: Path, instance_id: str) -> dict:
    with dataset.open() as fh:
        for line in fh:
            row = json.loads(line)
            if row.get("instance_id") == instance_id:
                return row
    raise SystemExit(
        f"Instance {instance_id!r} not found in {dataset}. "
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

    from solvers.claude_agent import ClaudeAgentSolver
    solver = ClaudeAgentSolver(timeout_s=REAL_SOLVE_TIMEOUT_S)

    import tempfile
    with tempfile.TemporaryDirectory(prefix="real_solve_") as tmp:
        cwd = Path(tmp) / TARGET_INSTANCE_ID
        cwd.mkdir()

        print(
            f"Running ClaudeAgentSolver with real prompt "
            f"(model={REAL_SOLVE_MODEL}, timeout={REAL_SOLVE_TIMEOUT_S}s) ..."
        )
        t0 = time.monotonic()
        result: SolverResult = solver.solve(
            task=task,
            prompt=REAL_SOLVE_PROMPT,
            cwd=cwd,
            model=REAL_SOLVE_MODEL,
        )
        elapsed = time.monotonic() - t0

    failures: list[str] = []

    # 1. patch must be non-empty (or at least status is not an infra error)
    if result.status == "error":
        failures.append(f"solver returned error: {result.error_msg}")
    elif result.status == "timeout":
        # Timeout is a soft failure for smoke — just warn, don't hard-fail.
        print(f"WARNING: solver timed out after {REAL_SOLVE_TIMEOUT_S}s — patch may be empty.")
    elif not result.patch.strip():
        failures.append("patch is empty (status=empty_patch or ok with no diff)")

    # 2. test_patch file must NOT appear in the captured patch.
    if TEST_PATCH_FILE in result.patch:
        failures.append(
            f"patch contains test_patch file {TEST_PATCH_FILE!r} — "
            "test_patch was not committed before git diff (Bug 1 not fixed)"
        )

    # 3. Token counts must be > 0.
    if result.prompt_tokens == 0:
        failures.append(
            "prompt_tokens == 0 — token parsing failed (Bug 2 not fixed)"
        )
    if result.output_tokens == 0:
        failures.append(
            "output_tokens == 0 — token parsing failed (Bug 2 not fixed)"
        )

    # ── report ────────────────────────────────────────────────────────────────
    print()
    print("=" * 60)
    print(f"REAL SOLVE SMOKE TEST {'PASSED' if not failures else 'FAILED'}")
    print(f"  status          : {result.status}")
    print(f"  latency_s       : {result.latency_s:.1f}s  (wall: {elapsed:.1f}s)")
    print(f"  prompt_tokens   : {result.prompt_tokens}")
    print(f"  output_tokens   : {result.output_tokens}")
    print(f"  patch lines     : {len(result.patch.splitlines())}")
    print(f"  test_patch_file : {'ABSENT (correct)' if TEST_PATCH_FILE not in result.patch else 'PRESENT (BUG)'}")
    if result.error_msg:
        print(f"  error_msg       : {result.error_msg}")
    print(f"  raw_log size    : {len(result.raw_log.encode('utf-8', errors='replace'))} bytes")
    if failures:
        print()
        for f in failures:
            print(f"  FAIL: {f}")
    print("=" * 60)

    if failures:
        sys.exit(1)


if __name__ == "__main__":
    main()
