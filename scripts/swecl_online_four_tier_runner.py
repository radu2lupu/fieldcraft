#!/usr/bin/env python3
"""Online SWE-Bench-CL four-tier runner with isolated memory learning.

This runner answers a different question from `swecl_four_tier_runner.py`:
does learning from earlier solved tasks help later tasks? Memory-enabled tiers
run tasks in order, evaluate each patch, and write a compact Fieldcraft memory
only when the task resolves. Later tasks in that same tier can retrieve those
memories from an isolated benchmark DB.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import sqlite3
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

from swecl_codex_skill_runner import ROOT, DEFAULT_IDS, load_instances


PRED_ROOT = ROOT / "benchmarks/swe-bench-cl/predictions/codex-pilot"
RESULT_ROOT = ROOT / "benchmarks/swe-bench-cl/results"
FOUR_TIER_ROOT = ROOT / "benchmarks/swe-bench-cl/four-tier"

TIERS: dict[str, dict[str, bool]] = {
    "cold": {"memory": False, "skills": False, "learns": False},
    "memory": {"memory": True, "skills": False, "learns": True},
    "skill": {"memory": False, "skills": True, "learns": False},
    "memory_skill": {"memory": True, "skills": True, "learns": True},
}


def project_slug(repo: str) -> str:
    return repo.replace("/", "-")


def read_jsonl(path: Path) -> list[dict]:
    if not path.exists():
        return []
    rows = []
    for line in path.read_text(errors="ignore").splitlines():
        if line.strip():
            rows.append(json.loads(line))
    return rows


def write_json(path: Path, data: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n")


def run_id(run_prefix: str, tier: str) -> str:
    return f"{run_prefix}-{tier.replace('_', '-')}"


def run_child(args: argparse.Namespace, tier: str, instance_id: str, fc_home: Path) -> None:
    spec = TIERS[tier]
    rid = run_id(args.run_prefix, tier)
    cmd = [
        sys.executable,
        str(ROOT / "scripts/swecl_codex_skill_runner.py"),
        "--dataset",
        str(args.dataset),
        "--run-id",
        rid,
        "--instance-ids",
        instance_id,
        "--model",
        args.model,
        "--reasoning",
        args.reasoning,
        "--memory-limit",
        str(args.memory_limit),
        "--skill-limit",
        str(args.skill_limit),
        "--skill-budget-chars",
        str(args.skill_budget_chars),
        "--corpus",
        *[str(path) for path in args.corpus],
        "--out-root",
        str(args.out_root),
        "--work-root",
        str(args.work_root),
        "--fieldcraft-home",
        str(fc_home),
    ]
    if spec["memory"]:
        cmd.append("--memory")
    if spec["skills"]:
        cmd.append("--skills")
    if args.resume:
        cmd.append("--resume")
    subprocess.run(cmd, cwd=ROOT, check=True)


def evaluate_one(args: argparse.Namespace, tier: str, instance_id: str) -> dict:
    rid = run_id(args.run_prefix, tier)
    pred_path = args.out_root / rid / f"{rid}.jsonl"
    report_dir = args.result_root / args.run_prefix / rid
    cmd = [
        sys.executable,
        str(ROOT / "scripts/local_swe_eval.py"),
        "--dataset_name",
        str(args.dataset),
        "--predictions_path",
        str(pred_path),
        "--run_id",
        rid,
        "--report_dir",
        str(report_dir),
        "--instance_ids",
        instance_id,
        "--max_workers",
        "1",
        "--timeout",
        str(args.eval_timeout),
    ]
    if args.resume:
        cmd.append("--resume")
    subprocess.run(cmd, cwd=ROOT, check=True)
    result_path = report_dir / instance_id / "result.json"
    if not result_path.exists():
        raise RuntimeError(f"missing eval result: {result_path}")
    return json.loads(result_path.read_text())


def touched_files(patch: str) -> list[str]:
    files = []
    for match in re.finditer(r"^\+\+\+ b/(.+)$", patch, re.M):
        path = match.group(1).strip()
        if path != "/dev/null":
            files.append(path)
    return sorted(set(files))


def prediction_for(args: argparse.Namespace, tier: str, instance_id: str) -> dict | None:
    rid = run_id(args.run_prefix, tier)
    pred_path = args.out_root / rid / f"{rid}.jsonl"
    for row in read_jsonl(pred_path):
        if row.get("instance_id") == instance_id:
            return row
    return None


def summarize_memory(instance: dict, prediction: dict, eval_result: dict) -> str:
    patch = prediction.get("model_patch") or ""
    files = touched_files(patch)
    f2p_pass = eval_result.get("fail_to_pass_pass") or []
    p2p_pass = eval_result.get("pass_to_pass_pass") or []
    problem = " ".join((instance.get("problem_statement") or "").split())[:900]
    file_text = ", ".join(files[:8]) if files else "the relevant source files"
    test_text = ", ".join((f2p_pass + p2p_pass)[:8]) or "the targeted regression tests"
    return (
        f"SWE-Bench-CL solved task {instance['instance_id']} in {instance['repo']}. "
        f"Problem pattern: {problem}. "
        f"Useful fix pattern: inspect {file_text}, make the smallest semantic source change, "
        f"and verify with {test_text}. "
        "Prefer this memory only for later tasks in the same repository that touch similar "
        "modules, APIs, parsing behavior, test selection, or regression shape."
    )


def write_memory(args: argparse.Namespace, fc_home: Path, instance: dict, prediction: dict, eval_result: dict) -> None:
    summary = summarize_memory(instance, prediction, eval_result)
    env = os.environ.copy()
    env["FIELDCRAFT_HOME"] = str(fc_home)
    subprocess.run(
        [
            "cargo",
            "run",
            "-q",
            "-p",
            "fieldcraft-cli",
            "--",
            "write",
            summary,
            "--project",
            project_slug(instance["repo"]),
            "--kind",
            "procedure",
        ],
        cwd=ROOT,
        env=env,
        check=True,
    )


def eval_summary(report_dir: Path, instance_ids: list[str]) -> dict:
    resolved_ids = []
    unresolved_ids = []
    empty_patch_ids = []
    error_ids = []
    for iid in instance_ids:
        path = report_dir / iid / "result.json"
        if not path.exists():
            continue
        row = json.loads(path.read_text())
        if row.get("empty_patch"):
            empty_patch_ids.append(iid)
        elif row.get("error_msg") and not row.get("apply_failed"):
            error_ids.append(iid)
        elif row.get("resolved"):
            resolved_ids.append(iid)
        else:
            unresolved_ids.append(iid)
    return {
        "total_instances": len(resolved_ids) + len(unresolved_ids),
        "submitted_instances": len(instance_ids),
        "completed_instances": len(resolved_ids) + len(unresolved_ids),
        "resolved_instances": len(resolved_ids),
        "unresolved_instances": len(unresolved_ids),
        "empty_patch_instances": len(empty_patch_ids),
        "error_instances": len(error_ids),
        "resolved_ids": sorted(resolved_ids),
        "unresolved_ids": sorted(unresolved_ids),
        "empty_patch_ids": sorted(empty_patch_ids),
        "error_ids": sorted(error_ids),
        "schema_version": 2,
    }


def memory_count(fc_home: Path, project_ids: list[str]) -> dict[str, int]:
    db = fc_home / "memory.sqlite3"
    counts = {project: 0 for project in project_ids}
    if not db.exists():
        return counts
    placeholders = ",".join("?" for _ in project_ids)
    with sqlite3.connect(db) as conn:
        for project, count in conn.execute(
            f"select project_id, count(*) from knowledge where project_id in ({placeholders}) and state != 'rejected' group by project_id",
            project_ids,
        ):
            counts[str(project)] = int(count)
    return counts


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dataset", type=Path, default=ROOT / "benchmarks/swe-bench-cl/data/swe-bench-cl-instances.jsonl")
    parser.add_argument("--run-prefix", required=True)
    parser.add_argument("--instance-ids", nargs="*", default=DEFAULT_IDS)
    parser.add_argument("--model", default="gpt-5.4-mini")
    parser.add_argument("--reasoning", default="low")
    parser.add_argument("--memory-limit", type=int, default=3)
    parser.add_argument("--skill-limit", type=int, default=3)
    parser.add_argument("--skill-budget-chars", type=int, default=5000)
    parser.add_argument("--eval-timeout", type=int, default=300)
    parser.add_argument(
        "--corpus",
        type=Path,
        nargs="+",
        default=[ROOT / "eval/skills/corpus/swecl_normalized_benchmark_skills.jsonl"],
    )
    parser.add_argument("--tiers", nargs="+", choices=sorted(TIERS), default=["cold", "memory", "skill", "memory_skill"])
    parser.add_argument("--out-root", type=Path, default=PRED_ROOT)
    parser.add_argument("--result-root", type=Path, default=RESULT_ROOT)
    parser.add_argument("--work-root", type=Path, default=ROOT / ".swecl-cache/work")
    parser.add_argument("--manifest-root", type=Path, default=FOUR_TIER_ROOT)
    parser.add_argument("--resume", action="store_true")
    parser.add_argument(
        "--allow-cross-repo-memory",
        action="store_true",
        help="Allow one online memory DB to span multiple repositories. Disabled by default for benchmark isolation.",
    )
    args = parser.parse_args()

    instances = load_instances(args.dataset, args.instance_ids)
    by_id = {inst["instance_id"]: inst for inst in instances}
    projects = sorted({project_slug(inst["repo"]) for inst in instances})
    repos = sorted({inst["repo"] for inst in instances})
    if len(repos) != 1 and not args.allow_cross_repo_memory:
        raise SystemExit(
            "online memory learning must run one repo sequence at a time. "
            f"Got repos={repos}. Use one repo per run-prefix, or pass "
            "--allow-cross-repo-memory only for an intentional cross-repo experiment."
        )
    run_root = args.manifest_root / args.run_prefix
    manifest = {
        "run_prefix": args.run_prefix,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "dataset": str(args.dataset),
        "instance_ids": args.instance_ids,
        "model": args.model,
        "reasoning": args.reasoning,
        "mode": "online_memory_learning",
        "repo_isolation": "single_repo_required",
        "repos": repos,
        "skill_corpus": [str(path) for path in args.corpus],
        "tiers": {},
    }

    for tier in args.tiers:
        rid = run_id(args.run_prefix, tier)
        fc_home = run_root / "fieldcraft-home" / tier
        report_dir = args.result_root / args.run_prefix / rid
        manifest["tiers"][tier] = {
            "run_id": rid,
            "prediction_path": str(args.out_root / rid / f"{rid}.jsonl"),
            "telemetry_path": str(args.out_root / rid / "telemetry.jsonl"),
            "eval_report_dir": str(report_dir),
            "fieldcraft_home": str(fc_home),
            "memory_enabled": TIERS[tier]["memory"],
            "memory_learning_enabled": TIERS[tier]["learns"],
            "skills_enabled": TIERS[tier]["skills"],
        }

    write_json(run_root / "manifest.json", manifest)
    learned_log = run_root / "learned_memories.jsonl"
    learned = {
        (row.get("tier"), row.get("instance_id"))
        for row in read_jsonl(learned_log)
    }

    for tier in args.tiers:
        fc_home = Path(manifest["tiers"][tier]["fieldcraft_home"])
        fc_home.mkdir(parents=True, exist_ok=True)
        print(f"\n== tier {tier} ==")
        for iid in args.instance_ids:
            print(f"-- {iid}")
            run_child(args, tier, iid, fc_home)
            result = evaluate_one(args, tier, iid)
            print(f"{iid}: resolved={result.get('resolved')} error={result.get('error_msg')}")
            if TIERS[tier]["learns"] and result.get("resolved") and (tier, iid) not in learned:
                prediction = prediction_for(args, tier, iid)
                if prediction:
                    write_memory(args, fc_home, by_id[iid], prediction, result)
                    learned.add((tier, iid))
                    with learned_log.open("a") as f:
                        f.write(json.dumps({"tier": tier, "instance_id": iid, "fieldcraft_home": str(fc_home)}) + "\n")

        report_dir = Path(manifest["tiers"][tier]["eval_report_dir"])
        write_json(report_dir / "summary.json", eval_summary(report_dir, args.instance_ids))
        manifest["tiers"][tier]["final_memory_counts"] = memory_count(fc_home, projects)
        write_json(run_root / "manifest.json", manifest)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
