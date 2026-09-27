#!/usr/bin/env python3
"""Oracle Validation Eval Orchestrator.

Runs the full Claude-based SWE-Bench solve-rate eval across 5 conditions × 20 tasks.

Usage:
    python3 scripts/oracle_validation_runner.py \\
      --task-ids eval/oracle_validation_v1/task_ids.json \\
      --dataset eval/patch_planning/heldout_50/dataset.jsonl \\
      --conditions-dir eval/oracle_validation_v1/conditions \\
      --out-dir eval/oracle_validation_v1/runs/v1 \\
      --solver claude \\
      --model claude-sonnet-4-6 \\
      --conditions cold whole_skill oracle_section oracle_rewritten oracle_multi \\
      --solver-timeout 600 \\
      --eval-timeout 900 \\
      --parallel 1 \\
      [--resume] \\
      [--smoke]
"""
from __future__ import annotations

import argparse
import concurrent.futures
import importlib.util
import json
import math
import os
import subprocess
import sys
import tempfile
import threading
import time
from datetime import datetime, timezone
from pathlib import Path

# ---------------------------------------------------------------------------
# Paths
# ---------------------------------------------------------------------------
SCRIPT_DIR = Path(__file__).parent.resolve()
REPO_ROOT = SCRIPT_DIR.parent

# ---------------------------------------------------------------------------
# Base agent prompt template
# ---------------------------------------------------------------------------
BASE_PROMPT_TEMPLATE = """\
You are solving a SWE-Bench bug for {instance_id} in {repo}.

Problem statement:
{problem_statement}

Failing tests that must pass after your fix:
{fail_to_pass_list}

Existing tests that must NOT regress:
{pass_to_pass_count} tests

Repository layout: explore as needed. The repo is at {repo_dir}.
{knowledge_block}
Approach: read the relevant code, write a minimal correct patch, optionally run the failing tests to verify. Output your final answer as edits to source files. Do NOT modify the failing tests themselves.
"""

KNOWLEDGE_BLOCK_TEMPLATE = """
Domain knowledge for this task (use as a guide for your investigation — read the actual source code to verify before applying any pattern):
{condition_content}
"""

# ---------------------------------------------------------------------------
# Wilson confidence interval
# ---------------------------------------------------------------------------

def wilson_ci(k: int, n: int, z: float = 1.96) -> tuple[float, float]:
    """Return Wilson 95% CI as (lo, hi) proportions, or (0,0) if n==0."""
    if n == 0:
        return (0.0, 0.0)
    p_hat = k / n
    denom = 1 + z * z / n
    center = (p_hat + z * z / (2 * n)) / denom
    spread = z * math.sqrt(p_hat * (1 - p_hat) / n + z * z / (4 * n * n)) / denom
    return (max(0.0, center - spread), min(1.0, center + spread))


# ---------------------------------------------------------------------------
# Dataset loading
# ---------------------------------------------------------------------------

def load_dataset(dataset_path: Path) -> dict[str, dict]:
    instances: dict[str, dict] = {}
    with open(dataset_path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            row = json.loads(line)
            instances[row["instance_id"]] = row
    return instances


# ---------------------------------------------------------------------------
# Condition content loading
# ---------------------------------------------------------------------------

def load_condition_content(conditions_dir: Path, condition: str, instance_id: str) -> str:
    """Return the content of the condition file, or '' if not found."""
    condition_file = conditions_dir / condition / f"{instance_id}.md"
    if condition_file.exists():
        return condition_file.read_text().strip()
    return ""


# ---------------------------------------------------------------------------
# Prompt builder
# ---------------------------------------------------------------------------

def build_prompt(task: dict, condition_content: str, repo_dir: Path) -> str:
    fail_to_pass = task.get("FAIL_TO_PASS", [])
    if isinstance(fail_to_pass, str):
        try:
            fail_to_pass = json.loads(fail_to_pass)
        except Exception:
            fail_to_pass = [fail_to_pass]
    fail_to_pass_list = "\n".join(f"  - {t}" for t in fail_to_pass)

    pass_to_pass = task.get("PASS_TO_PASS", [])
    if isinstance(pass_to_pass, str):
        try:
            pass_to_pass = json.loads(pass_to_pass)
        except Exception:
            pass_to_pass = []
    pass_to_pass_count = len(pass_to_pass)

    knowledge_block = ""
    if condition_content:
        knowledge_block = KNOWLEDGE_BLOCK_TEMPLATE.format(
            condition_content=condition_content
        )

    return BASE_PROMPT_TEMPLATE.format(
        instance_id=task["instance_id"],
        repo=task["repo"],
        problem_statement=task.get("problem_statement", "").strip(),
        fail_to_pass_list=fail_to_pass_list or "  (none listed)",
        pass_to_pass_count=pass_to_pass_count,
        repo_dir=str(repo_dir),
        knowledge_block=knowledge_block,
    )


# ---------------------------------------------------------------------------
# runs.jsonl helpers
# ---------------------------------------------------------------------------
_runs_lock = threading.Lock()


def append_run_row(runs_jsonl: Path, row: dict) -> None:
    with _runs_lock:
        runs_jsonl.parent.mkdir(parents=True, exist_ok=True)
        with open(runs_jsonl, "a") as f:
            f.write(json.dumps(row) + "\n")


# ---------------------------------------------------------------------------
# Per-run execution
# ---------------------------------------------------------------------------

def run_one(
    *,
    task: dict,
    condition: str,
    conditions_dir: Path,
    out_dir: Path,
    solver,
    model: str,
    solver_timeout: int,
    eval_timeout: int,
    dataset_path: Path,
    total: int,
    idx: int,
    worker_id: int = 0,
) -> dict:
    """Execute one (task, condition) pair. Returns the run row dict."""
    instance_id = task["instance_id"]
    run_out_dir = out_dir / condition / instance_id
    result_json = run_out_dir / "result.json"

    started_at = datetime.now(timezone.utc).isoformat()

    # Build per-worker temp workdir (unique to avoid git clone collisions).
    workdir_base = Path(tempfile.mkdtemp(prefix=f"oracle_w{worker_id}_{condition}_{instance_id}_"))
    cwd = workdir_base / "solve"
    repo_dir = cwd / "repo"

    # Load condition content
    condition_content = load_condition_content(conditions_dir, condition, instance_id)

    # Build prompt
    prompt = build_prompt(task, condition_content, repo_dir)

    # --- Solver ---
    solver_start = time.monotonic()
    try:
        # Override solver timeout if set
        old_timeout = getattr(solver, "timeout_s", solver_timeout)
        solver.timeout_s = solver_timeout
        solver_result = solver.solve(task=task, prompt=prompt, cwd=cwd, model=model)
        solver.timeout_s = old_timeout
    except Exception as exc:
        solver_latency = time.monotonic() - solver_start
        import traceback
        row = {
            "instance_id": instance_id,
            "condition": condition,
            "solver_status": "error",
            "solved": False,
            "prompt_tokens": 0,
            "output_tokens": 0,
            "solver_latency_s": round(solver_latency, 2),
            "eval_latency_s": 0.0,
            "patch_size_lines": 0,
            "started_at": started_at,
            "ended_at": datetime.now(timezone.utc).isoformat(),
            "error": f"solver_exception: {exc}\n{traceback.format_exc()[:2000]}",
        }
        print(
            f"[{idx}/{total}] {condition}/{instance_id}: status=error solved=false "
            f"tokens=0+0 time={solver_latency:.0f}s ERROR={exc}",
            flush=True,
        )
        import shutil
        shutil.rmtree(workdir_base, ignore_errors=True)
        return row

    solver_latency = time.monotonic() - solver_start

    # Write patch
    run_out_dir.mkdir(parents=True, exist_ok=True)
    patch_file = run_out_dir / "patch.diff"
    patch_file.write_text(solver_result.patch or "")
    patch_size_lines = len((solver_result.patch or "").splitlines())

    # Write raw log (already truncated by SolverResult.__post_init__)
    if solver_result.raw_log:
        (run_out_dir / "raw_log.txt").write_text(solver_result.raw_log)

    # --- Eval ---
    eval_start = time.monotonic()
    run_id = f"oracle_{condition}_{instance_id}_{int(time.time())}"
    eval_cmd = [
        sys.executable,  # python3 / python3.13 — inherit interpreter
        str(SCRIPT_DIR / "local_swe_eval.py"),
        "--instance-id", instance_id,
        "--patch-file", str(patch_file.resolve()),
        "--dataset", str(dataset_path.resolve()),
        "--out-dir", str(run_out_dir.resolve()),
        "--timeout", str(eval_timeout),
        "--run-id", run_id,
    ]
    try:
        subprocess.run(
            eval_cmd,
            cwd=str(REPO_ROOT),
            timeout=eval_timeout + 300,
            check=False,
        )
    except subprocess.TimeoutExpired:
        pass
    except Exception as exc:
        print(f"[local_swe_eval] subprocess error: {exc}", flush=True)

    eval_latency = time.monotonic() - eval_start

    # Parse result.json
    solved = False
    eval_error = None
    if result_json.exists():
        try:
            result_data = json.loads(result_json.read_text())
            solved = bool(result_data.get("solved", False))
            eval_error = result_data.get("error")
        except Exception as exc:
            eval_error = f"parse_error: {exc}"
    else:
        eval_error = "result.json not found after eval"

    # Compose error field
    error_parts = []
    if solver_result.error_msg:
        error_parts.append(f"solver: {solver_result.error_msg}")
    if eval_error:
        error_parts.append(f"eval: {eval_error}")
    error = "; ".join(error_parts) if error_parts else None

    row = {
        "instance_id": instance_id,
        "condition": condition,
        "solver_status": solver_result.status,
        "solved": solved,
        "prompt_tokens": solver_result.prompt_tokens,
        "output_tokens": solver_result.output_tokens,
        "solver_latency_s": round(solver_latency, 2),
        "eval_latency_s": round(eval_latency, 2),
        "patch_size_lines": patch_size_lines,
        "started_at": started_at,
        "ended_at": datetime.now(timezone.utc).isoformat(),
        "error": error,
    }

    total_tokens = solver_result.prompt_tokens + solver_result.output_tokens
    wall = solver_latency + eval_latency
    print(
        f"[{idx}/{total}] {condition}/{instance_id}: "
        f"status={solver_result.status} solved={'true' if solved else 'false'} "
        f"tokens={solver_result.prompt_tokens}+{solver_result.output_tokens}={total_tokens} "
        f"time={wall:.0f}s",
        flush=True,
    )

    import shutil
    shutil.rmtree(workdir_base, ignore_errors=True)
    return row


# ---------------------------------------------------------------------------
# Report generation
# ---------------------------------------------------------------------------

def generate_report(
    out_dir: Path,
    runs_jsonl: Path,
    solver_name: str,
    model: str,
    conditions: list[str],
    task_ids: list[str],
) -> None:
    rows: list[dict] = []
    if runs_jsonl.exists():
        for line in runs_jsonl.read_text().splitlines():
            line = line.strip()
            if line:
                try:
                    rows.append(json.loads(line))
                except Exception:
                    pass

    total_expected = len(conditions) * len(task_ids)
    total_completed = len(rows)

    # Per-condition stats
    from collections import defaultdict

    cond_rows: dict[str, list[dict]] = defaultdict(list)
    for r in rows:
        cond_rows[r["condition"]].append(r)

    table_lines = [
        "| condition | solved | total | rate | wilson 95% CI | mean prompt tokens | mean output tokens | mean wall time (s) |",
        "|---|---:|---:|---:|---|---:|---:|---:|",
    ]

    cond_stats: dict[str, dict] = {}
    for cond in conditions:
        crows = cond_rows.get(cond, [])
        n = len(crows)
        k = sum(1 for r in crows if r.get("solved"))
        rate = k / n if n > 0 else 0.0
        lo, hi = wilson_ci(k, n)
        mean_pt = sum(r.get("prompt_tokens", 0) for r in crows) / n if n > 0 else 0
        mean_ot = sum(r.get("output_tokens", 0) for r in crows) / n if n > 0 else 0
        mean_wall = (
            sum(r.get("solver_latency_s", 0) + r.get("eval_latency_s", 0) for r in crows) / n
            if n > 0
            else 0
        )
        table_lines.append(
            f"| {cond} | {k} | {n} | {rate:.1%} | [{lo:.1%}, {hi:.1%}] "
            f"| {mean_pt:.0f} | {mean_ot:.0f} | {mean_wall:.0f} |"
        )
        cond_stats[cond] = {
            "n": n,
            "k": k,
            "rate": rate,
            "lo": lo,
            "hi": hi,
            "mean_pt": mean_pt,
            "mean_ot": mean_ot,
            "mean_wall": mean_wall,
            "mean_total_tokens": mean_pt + mean_ot,
        }

    # Headline verdict
    headline_lines = []
    cold_stats = cond_stats.get("cold", {})
    whole_stats = cond_stats.get("whole_skill", {})
    cold_tokens = cold_stats.get("mean_total_tokens", 0) or 1
    cold_wall = cold_stats.get("mean_wall", 0) or 1
    whole_rate = whole_stats.get("rate", 0.0)

    oracle_variants = [c for c in conditions if c.startswith("oracle_")]
    any_win = False
    for ov in oracle_variants:
        ov_stats = cond_stats.get(ov, {})
        token_ratio = ov_stats.get("mean_total_tokens", 0) / cold_tokens
        time_ratio = ov_stats.get("mean_wall", 0) / cold_wall
        acc_delta = ov_stats.get("rate", 0.0) - whole_rate
        win1 = token_ratio <= 0.9
        win2 = time_ratio <= 0.9
        win3 = acc_delta > 0
        wins_all = win1 and win2 and win3
        if wins_all:
            any_win = True
        headline_lines.append(
            f"### {ov}\n"
            f"- token_ratio_vs_cold = {token_ratio:.3f} ({'WIN' if win1 else 'FAIL'} ≤0.9)\n"
            f"- time_ratio_vs_cold  = {time_ratio:.3f} ({'WIN' if win2 else 'FAIL'} ≤0.9)\n"
            f"- accuracy_delta_vs_whole_skill = {acc_delta:+.1%} ({'WIN' if win3 else 'FAIL'} >0)\n"
            f"- **All 3 criteria met: {'YES — DECOMPOSITION WINS' if wins_all else 'NO'}**\n"
        )

    if oracle_variants:
        verdict = "DECOMPOSITION WINS" if any_win else "NO BUILD"
    else:
        verdict = "(insufficient oracle conditions to decide)"

    now = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
    report = f"""# Oracle Validation Eval Run

- Date: {now}
- Solver: {solver_name} (model: {model})
- Tasks: {len(task_ids)} (frozen list at eval/oracle_validation_v1/task_ids.json)
- Conditions: {" / ".join(conditions)}
- Total runs completed: {total_completed} / {total_expected}

## Solve rate per condition

{chr(10).join(table_lines)}

## Headline (per locked decision rule)

{chr(10).join(headline_lines)}

## Final Verdict

**{verdict}**
"""

    report_path = out_dir / "report.md"
    report_path.write_text(report)
    print(f"\n[report] Written to {report_path}", flush=True)


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(description="Oracle Validation Eval Orchestrator")
    parser.add_argument(
        "--task-ids",
        default="eval/oracle_validation_v1/task_ids.json",
        help="Path to task_ids.json",
    )
    parser.add_argument(
        "--dataset",
        default="eval/patch_planning/heldout_50/dataset.jsonl",
        help="Path to dataset JSONL",
    )
    parser.add_argument(
        "--conditions-dir",
        default="eval/oracle_validation_v1/conditions",
        help="Directory containing per-condition subdirs with <instance_id>.md files",
    )
    parser.add_argument(
        "--out-dir",
        default="eval/oracle_validation_v1/runs/v1",
        help="Output directory for all run artifacts",
    )
    parser.add_argument("--solver", default="claude", help="Solver name (default: claude)")
    parser.add_argument("--reasoning", default=None, help="Reasoning effort for codex solver (e.g. low, medium, high, instant)")
    parser.add_argument(
        "--model",
        default="claude-sonnet-4-6",
        help="Model identifier for the solver",
    )
    parser.add_argument(
        "--conditions",
        nargs="+",
        default=["cold", "whole_skill", "oracle_section", "oracle_rewritten", "oracle_multi"],
        help="Conditions to run (space-separated)",
    )
    parser.add_argument(
        "--solver-timeout",
        type=int,
        default=600,
        help="Per-task solver timeout in seconds (default: 600)",
    )
    parser.add_argument(
        "--eval-timeout",
        type=int,
        default=900,
        help="Per-task eval timeout in seconds (default: 900)",
    )
    parser.add_argument(
        "--parallel",
        type=int,
        default=1,
        help="Number of concurrent workers (default: 1)",
    )
    parser.add_argument(
        "--resume",
        action="store_true",
        help="Skip tasks where result.json already exists",
    )
    parser.add_argument(
        "--smoke",
        action="store_true",
        help="Run only first task × cold condition and exit",
    )
    args = parser.parse_args()

    # Resolve all paths relative to REPO_ROOT so the script can be invoked
    # from any working directory.
    def resolve(p: str) -> Path:
        path = Path(p)
        if not path.is_absolute():
            path = REPO_ROOT / path
        return path.resolve()

    task_ids_path = resolve(args.task_ids)
    dataset_path = resolve(args.dataset)
    conditions_dir = resolve(args.conditions_dir)
    out_dir = resolve(args.out_dir)

    # Validate inputs
    if not task_ids_path.exists():
        print(f"ERROR: task_ids not found: {task_ids_path}", file=sys.stderr)
        return 1
    if not dataset_path.exists():
        print(f"ERROR: dataset not found: {dataset_path}", file=sys.stderr)
        return 1
    if not conditions_dir.exists():
        print(f"ERROR: conditions_dir not found: {conditions_dir}", file=sys.stderr)
        return 1

    task_ids: list[str] = json.loads(task_ids_path.read_text())
    dataset = load_dataset(dataset_path)
    conditions = args.conditions

    # Load solver — use sys.path injection so we can import from scripts/
    scripts_dir = str(SCRIPT_DIR)
    if scripts_dir not in sys.path:
        sys.path.insert(0, scripts_dir)
    from solvers.registry import get_solver
    solver = get_solver(args.solver, reasoning=args.reasoning)
    solver.timeout_s = args.solver_timeout

    out_dir.mkdir(parents=True, exist_ok=True)
    runs_jsonl = out_dir / "runs.jsonl"

    # Build work list
    if args.smoke:
        conditions = ["cold"]
        task_ids = task_ids[:1]
        print(f"[smoke] Running 1 task × 1 condition: cold/{task_ids[0]}", flush=True)

    work_items: list[tuple[str, str]] = []  # (condition, instance_id)
    for condition in conditions:
        for iid in task_ids:
            work_items.append((condition, iid))

    total = len(work_items)

    # Resume: skip only runs whose latest runs.jsonl entry has solver_status="ok".
    # Failed runs (error/no_patch from prior disk-full crashes) get retried even if
    # a result.json exists from a prior partial completion.
    if args.resume:
        latest_status: dict[tuple[str, str], str] = {}
        runs_jsonl = out_dir / "runs.jsonl"
        if runs_jsonl.exists():
            for line in runs_jsonl.read_text().splitlines():
                if not line.strip():
                    continue
                try:
                    row = json.loads(line)
                    key = (row["condition"], row["instance_id"])
                    latest_status[key] = row.get("solver_status", "unknown")
                except (json.JSONDecodeError, KeyError):
                    continue
        filtered = []
        skipped_ok = 0
        retry_failed = 0
        for condition, iid in work_items:
            status = latest_status.get((condition, iid))
            if status == "ok":
                skipped_ok += 1
            else:
                filtered.append((condition, iid))
                if status is not None:
                    retry_failed += 1
        work_items = filtered
        print(
            f"[resume] {skipped_ok} skipped (solver_status=ok), "
            f"{retry_failed} retry (prior status: {sorted({s for s in latest_status.values() if s != 'ok'})}), "
            f"{len(work_items) - retry_failed} new, {len(work_items)} total remaining",
            flush=True,
        )

    total_planned = len(work_items)
    print(
        f"\n[oracle_runner] Starting {total_planned} run(s) "
        f"({len(conditions)} conditions × {len(task_ids)} tasks, parallel={args.parallel})\n",
        flush=True,
    )

    overall_start = time.monotonic()
    completed = 0

    def _run_item(idx: int, condition: str, instance_id: str, worker_id: int) -> dict:
        task = dataset.get(instance_id)
        if task is None:
            print(f"[{idx}/{total_planned}] SKIP: {instance_id} not in dataset", flush=True)
            return {
                "instance_id": instance_id,
                "condition": condition,
                "solver_status": "skip",
                "solved": False,
                "prompt_tokens": 0,
                "output_tokens": 0,
                "solver_latency_s": 0.0,
                "eval_latency_s": 0.0,
                "patch_size_lines": 0,
                "started_at": datetime.now(timezone.utc).isoformat(),
                "ended_at": datetime.now(timezone.utc).isoformat(),
                "error": f"instance_id not found in dataset: {instance_id}",
            }
        return run_one(
            task=task,
            condition=condition,
            conditions_dir=conditions_dir,
            out_dir=out_dir,
            solver=solver,
            model=args.model,
            solver_timeout=args.solver_timeout,
            eval_timeout=args.eval_timeout,
            dataset_path=dataset_path,
            total=total_planned,
            idx=idx,
            worker_id=worker_id,
        )

    if args.parallel <= 1:
        for i, (condition, iid) in enumerate(work_items, start=1):
            row = _run_item(i, condition, iid, 0)
            append_run_row(runs_jsonl, row)
            completed += 1
    else:
        futures_map: dict = {}
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.parallel) as executor:
            for i, (condition, iid) in enumerate(work_items, start=1):
                worker_id = i % args.parallel
                fut = executor.submit(_run_item, i, condition, iid, worker_id)
                futures_map[fut] = (i, condition, iid)
            for fut in concurrent.futures.as_completed(futures_map):
                row = fut.result()
                append_run_row(runs_jsonl, row)
                completed += 1

    total_wall = time.monotonic() - overall_start
    print(
        f"\n[oracle_runner] Done. {completed}/{total_planned} runs in {total_wall:.0f}s "
        f"({total_wall/max(completed,1):.0f}s/run avg)",
        flush=True,
    )

    # Final report
    # Reload all task_ids and conditions for the report (smoke may have sliced them)
    all_task_ids: list[str] = json.loads(task_ids_path.read_text())
    all_conditions: list[str] = args.conditions
    generate_report(
        out_dir=out_dir,
        runs_jsonl=runs_jsonl,
        solver_name=args.solver,
        model=args.model,
        conditions=all_conditions,
        task_ids=all_task_ids,
    )

    return 0


if __name__ == "__main__":
    sys.exit(main())
