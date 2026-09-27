#!/usr/bin/env python3
"""Collect patch.diff files written by Agent sub-agents, run local_swe_eval, write runs.jsonl + report.

Usage:
    python3 scripts/collect_agent_results.py \\
      --queue-dir /tmp/oracle_v6_queue \\
      --out-dir eval/oracle_validation_v1/runs/v6_claude_grounded \\
      --dataset eval/patch_planning/heldout_50/dataset.jsonl \\
      --conditions cold whole_skill oracle_section \\
      --task-ids eval/oracle_validation_v1/task_ids.json \\
      [--eval-timeout 900] \\
      [--skip-eval]
"""
from __future__ import annotations

import argparse
import json
import math
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

SCRIPT_DIR = Path(__file__).parent.resolve()
REPO_ROOT = SCRIPT_DIR.parent


def wilson_ci(k: int, n: int, z: float = 1.96) -> tuple[float, float]:
    if n == 0:
        return (0.0, 0.0)
    p_hat = k / n
    denom = 1 + z * z / n
    center = (p_hat + z * z / (2 * n)) / denom
    spread = z * math.sqrt(p_hat * (1 - p_hat) / n + z * z / (4 * n * n)) / denom
    return (max(0.0, center - spread), min(1.0, center + spread))


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


def run_eval(
    instance_id: str,
    patch_file: Path,
    dataset_path: Path,
    out_dir: Path,
    eval_timeout: int,
) -> tuple[bool, str | None]:
    """Run local_swe_eval.py for a single task. Returns (solved, error_msg)."""
    result_json = out_dir / "result.json"
    if result_json.exists():
        try:
            data = json.loads(result_json.read_text())
            return bool(data.get("solved", False)), data.get("error")
        except Exception:
            pass

    run_id = f"oracle_{instance_id}_{int(time.time())}"
    cmd = [
        sys.executable,
        str(SCRIPT_DIR / "local_swe_eval.py"),
        "--instance-id", instance_id,
        "--patch-file", str(patch_file.resolve()),
        "--dataset", str(dataset_path.resolve()),
        "--out-dir", str(out_dir.resolve()),
        "--timeout", str(eval_timeout),
        "--run-id", run_id,
    ]
    try:
        subprocess.run(
            cmd,
            cwd=str(REPO_ROOT),
            timeout=eval_timeout + 300,
            check=False,
        )
    except subprocess.TimeoutExpired:
        return False, "eval_timeout"
    except Exception as exc:
        return False, f"eval_subprocess_error: {exc}"

    if result_json.exists():
        try:
            data = json.loads(result_json.read_text())
            return bool(data.get("solved", False)), data.get("error")
        except Exception as exc:
            return False, f"result_parse_error: {exc}"
    return False, "result.json not found after eval"


def generate_report(
    out_dir: Path,
    runs_jsonl: Path,
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
        mean_wall = sum(r.get("solver_latency_s", 0) + r.get("eval_latency_s", 0) for r in crows) / n if n > 0 else 0
        table_lines.append(
            f"| {cond} | {k} | {n} | {rate:.1%} | [{lo:.1%}, {hi:.1%}] "
            f"| {mean_pt:.0f} | {mean_ot:.0f} | {mean_wall:.0f} |"
        )
        cond_stats[cond] = {"n": n, "k": k, "rate": rate, "mean_total_tokens": mean_pt + mean_ot, "mean_wall": mean_wall}

    cold_stats = cond_stats.get("cold", {})
    whole_stats = cond_stats.get("whole_skill", {})
    cold_tokens = cold_stats.get("mean_total_tokens", 0) or 1
    cold_wall = cold_stats.get("mean_wall", 0) or 1
    whole_rate = whole_stats.get("rate", 0.0)

    oracle_variants = [c for c in conditions if c.startswith("oracle_")]
    headline_lines = []
    any_win = False
    for ov in oracle_variants:
        ov_stats = cond_stats.get(ov, {})
        token_ratio = ov_stats.get("mean_total_tokens", 0) / cold_tokens
        time_ratio = ov_stats.get("mean_wall", 0) / cold_wall
        acc_delta = ov_stats.get("rate", 0.0) - whole_rate
        win1 = token_ratio <= 0.9
        win2 = time_ratio <= 0.9
        win3 = acc_delta > 0
        if win1 and win2 and win3:
            any_win = True
        headline_lines.append(
            f"### {ov}\n"
            f"- token_ratio_vs_cold = {token_ratio:.3f} ({'WIN' if win1 else 'FAIL'} ≤0.9)\n"
            f"- time_ratio_vs_cold  = {time_ratio:.3f} ({'WIN' if win2 else 'FAIL'} ≤0.9)\n"
            f"- accuracy_delta_vs_whole_skill = {acc_delta:+.1%} ({'WIN' if win3 else 'FAIL'} >0)\n"
            f"- **All 3 criteria met: {'YES — DECOMPOSITION WINS' if (win1 and win2 and win3) else 'NO'}**\n"
        )

    verdict = "DECOMPOSITION WINS" if any_win else "NO BUILD"
    total_completed = len(rows)
    total_expected = len(conditions) * len(task_ids)
    now = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")

    report = f"""# Oracle Validation Eval Run

- Date: {now}
- Solver: claude-code-agent (model: claude-sonnet-4-6)
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
    print(f"\n[report] Written to {report_path}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--queue-dir", default="/tmp/oracle_v6_queue")
    parser.add_argument("--out-dir", default="eval/oracle_validation_v1/runs/v6_claude_grounded")
    parser.add_argument("--dataset", default="eval/patch_planning/heldout_50/dataset.jsonl")
    parser.add_argument("--task-ids", default="eval/oracle_validation_v1/task_ids.json")
    parser.add_argument("--conditions", nargs="+",
                        default=["cold", "whole_skill", "oracle_section"])
    parser.add_argument("--eval-timeout", type=int, default=900)
    parser.add_argument("--skip-eval", action="store_true",
                        help="Don't run eval; just count patch.diff presence as solved")
    args = parser.parse_args()

    def resolve(p: str) -> Path:
        path = Path(p)
        return (REPO_ROOT / path).resolve() if not path.is_absolute() else path.resolve()

    queue_dir = Path(args.queue_dir)
    out_dir = resolve(args.out_dir)
    dataset_path = resolve(args.dataset)
    task_ids_path = resolve(args.task_ids)

    out_dir.mkdir(parents=True, exist_ok=True)
    dataset = load_dataset(dataset_path)
    task_ids: list[str] = json.loads(task_ids_path.read_text())
    runs_jsonl = out_dir / "runs.jsonl"

    # Collect already-logged pairs to avoid re-eval
    logged: set[tuple[str, str]] = set()
    if runs_jsonl.exists():
        for line in runs_jsonl.read_text().splitlines():
            if not line.strip():
                continue
            try:
                r = json.loads(line)
                logged.add((r["condition"], r["instance_id"]))
            except Exception:
                pass

    task_files = sorted(queue_dir.glob("*.json"))
    print(f"Found {len(task_files)} task files in queue")

    processed = 0
    for task_file in task_files:
        task_obj = json.loads(task_file.read_text())
        condition = task_obj["condition"]
        instance_id = task_obj["instance_id"]

        if (condition, instance_id) in logged:
            print(f"  SKIP (already logged): {condition}/{instance_id}")
            continue

        patch_file = Path(task_obj["patch_file"])
        output_dir = Path(task_obj["output_dir"])

        if not patch_file.exists() or patch_file.stat().st_size == 0:
            print(f"  MISSING patch: {condition}/{instance_id}")
            continue

        patch_lines = len(patch_file.read_text().splitlines())
        print(f"  Evaluating: {condition}/{instance_id} ({patch_lines} patch lines)...", end=" ", flush=True)

        started_at = datetime.now(timezone.utc).isoformat()
        eval_start = time.monotonic()

        if args.skip_eval:
            solved = patch_lines > 0
            eval_error = None
        else:
            solved, eval_error = run_eval(
                instance_id=instance_id,
                patch_file=patch_file,
                dataset_path=dataset_path,
                out_dir=output_dir,
                eval_timeout=args.eval_timeout,
            )

        eval_latency = time.monotonic() - eval_start
        print(f"solved={solved} ({eval_latency:.0f}s)")

        row = {
            "instance_id": instance_id,
            "condition": condition,
            "solver_status": "ok" if patch_lines > 0 else "empty_patch",
            "solved": solved,
            "prompt_tokens": 0,
            "output_tokens": 0,
            "solver_latency_s": 0.0,
            "eval_latency_s": round(eval_latency, 2),
            "patch_size_lines": patch_lines,
            "started_at": started_at,
            "ended_at": datetime.now(timezone.utc).isoformat(),
            "error": eval_error,
        }
        with open(runs_jsonl, "a") as f:
            f.write(json.dumps(row) + "\n")
        logged.add((condition, instance_id))
        processed += 1

    print(f"\nProcessed {processed} new results.")

    generate_report(
        out_dir=out_dir,
        runs_jsonl=runs_jsonl,
        conditions=args.conditions,
        task_ids=task_ids,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
