#!/usr/bin/env python3
"""Run a frozen SWE-Bench-CL task set across cold, memory, skill, memory+skill.

This wrapper does not evaluate correctness by itself. It creates a manifest,
checks whether the memory arm is meaningful, and launches the existing Codex
runner with identical task/model settings for each tier.
"""
from __future__ import annotations

import argparse
import json
import os
import sqlite3
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

from swecl_codex_skill_runner import DEFAULT_IDS, ROOT, load_instances


PRED_ROOT = ROOT / "benchmarks/swe-bench-cl/predictions/codex-pilot"
FOUR_TIER_ROOT = ROOT / "benchmarks/swe-bench-cl/four-tier"
FIELDCRAFT_HOME = Path(os.environ["FIELDCRAFT_HOME"]) if "FIELDCRAFT_HOME" in os.environ else Path.home() / ".fieldcraft"
MEMORY_DB = FIELDCRAFT_HOME / "memory.sqlite3"

TIERS: dict[str, list[str]] = {
    "cold": [],
    "memory": ["--memory"],
    "skill": ["--skills"],
    "memory_skill": ["--memory", "--skills"],
}


def project_slug(repo: str) -> str:
    return repo.replace("/", "-")


def read_memory_counts(db_path: Path, projects: list[str]) -> dict[str, int]:
    counts = {project: 0 for project in projects}
    if not db_path.exists():
        return counts
    placeholders = ",".join("?" for _ in projects)
    if not placeholders:
        return counts
    query = f"""
        select project_id, count(*)
        from knowledge
        where project_id in ({placeholders})
          and state != 'rejected'
        group by project_id
    """
    with sqlite3.connect(db_path) as conn:
        for project_id, count in conn.execute(query, projects):
            counts[str(project_id)] = int(count)
    return counts


def write_json(path: Path, data: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n")


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
    parser.add_argument(
        "--corpus",
        type=Path,
        nargs="+",
        default=[
            ROOT / "eval/skills/corpus/local_skills.jsonl",
            ROOT / "eval/skills/corpus/swe_bench_cl_seed_skills.jsonl",
        ],
    )
    parser.add_argument("--tiers", nargs="+", choices=sorted(TIERS), default=["cold", "memory", "skill", "memory_skill"])
    parser.add_argument("--out-root", type=Path, default=PRED_ROOT)
    parser.add_argument("--work-root", type=Path, default=ROOT / ".swecl-cache/work")
    parser.add_argument("--manifest-root", type=Path, default=FOUR_TIER_ROOT)
    parser.add_argument("--resume", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument(
        "--allow-empty-memory",
        action="store_true",
        help="Run memory tiers even when the frozen project set has no stored memories.",
    )
    args = parser.parse_args()

    instances = load_instances(args.dataset, args.instance_ids)
    projects = sorted({project_slug(inst["repo"]) for inst in instances})
    memory_counts = read_memory_counts(MEMORY_DB, projects)
    memory_total = sum(memory_counts.values())
    requested_memory = any("memory" in tier for tier in args.tiers)
    memory_status = "populated" if memory_total else "empty"

    if requested_memory and memory_total == 0 and not args.allow_empty_memory:
        print(
            "Refusing to run memory tiers because ~/.fieldcraft/memory.sqlite3 has "
            "no memories for the selected projects. Populate/freeze memory first, "
            "or pass --allow-empty-memory for a plumbing-only run.",
            file=sys.stderr,
        )
        write_json(
            args.manifest_root / args.run_prefix / "manifest.json",
            {
                "run_prefix": args.run_prefix,
                "created_at": datetime.now(timezone.utc).isoformat(),
                "status": "blocked_empty_memory",
                "memory_db": str(MEMORY_DB),
                "memory_counts": memory_counts,
                "instance_ids": args.instance_ids,
                "tiers": args.tiers,
            },
        )
        return 2

    manifest = {
        "run_prefix": args.run_prefix,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "dataset": str(args.dataset),
        "instance_ids": args.instance_ids,
        "model": args.model,
        "reasoning": args.reasoning,
        "memory_db": str(MEMORY_DB),
        "memory_counts": memory_counts,
        "memory_status": memory_status,
        "skill_corpus": [str(path) for path in args.corpus],
        "tiers": {},
    }

    for tier in args.tiers:
        run_id = f"{args.run_prefix}-{tier.replace('_', '-')}"
        manifest["tiers"][tier] = {
            "run_id": run_id,
            "prediction_path": str(args.out_root / run_id / f"{run_id}.jsonl"),
            "telemetry_path": str(args.out_root / run_id / "telemetry.jsonl"),
            "memory_enabled": "--memory" in TIERS[tier],
            "skills_enabled": "--skills" in TIERS[tier],
        }

    manifest_path = args.manifest_root / args.run_prefix / "manifest.json"
    write_json(manifest_path, manifest)
    print(f"Wrote manifest: {manifest_path}")

    for tier in args.tiers:
        run_id = manifest["tiers"][tier]["run_id"]
        cmd = [
            sys.executable,
            str(ROOT / "scripts/swecl_codex_skill_runner.py"),
            "--dataset",
            str(args.dataset),
            "--run-id",
            run_id,
            "--instance-ids",
            *args.instance_ids,
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
            *TIERS[tier],
        ]
        if args.resume:
            cmd.append("--resume")
        print("\n" + " ".join(cmd))
        if args.dry_run:
            continue
        subprocess.run(cmd, cwd=ROOT, check=True)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
