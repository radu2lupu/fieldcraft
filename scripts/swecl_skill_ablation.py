#!/usr/bin/env python3
"""Prepare a SWE-Bench-CL skill-intelligence ablation.

This script builds the four experiment arms needed to test whether retrieved
skill cards improve SWE-Bench-CL outcomes:

  cold, memory, skills, memory_skills

It selects instances from the local SWE-Bench-CL JSONL, asks Fieldcraft's real
skill retrieval evaluator for cards, writes per-arm prompt packs, and emits a
manifest that can be consumed by a Codex generation runner.
"""
from __future__ import annotations

import argparse
import json
import math
import subprocess
import textwrap
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
DEFAULT_REPOS = [
    "django/django",
    "sympy/sympy",
    "sphinx-doc/sphinx",
    "scikit-learn/scikit-learn",
    "pytest-dev/pytest",
]
ARMS = ("cold", "memory", "skills", "memory_skills")


def read_jsonl(path: Path) -> list[dict]:
    rows = []
    with path.open() as f:
        for line in f:
            line = line.strip()
            if line:
                rows.append(json.loads(line))
    return rows


def write_jsonl(path: Path, rows: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w") as f:
        for row in rows:
            f.write(json.dumps(row, sort_keys=True) + "\n")


def repo_slug(repo: str) -> str:
    return repo.replace("/", "-")


def as_list(value: object) -> list[str]:
    if value is None:
        return []
    if isinstance(value, list):
        return [str(item) for item in value]
    if isinstance(value, str):
        stripped = value.strip()
        if stripped.startswith("["):
            try:
                parsed = json.loads(stripped)
            except json.JSONDecodeError:
                return [value]
            if isinstance(parsed, list):
                return [str(item) for item in parsed]
    return [str(value)]


def select_instances(rows: list[dict], repos: list[str], per_repo: int) -> list[dict]:
    selected = []
    for repo in repos:
        repo_rows = [r for r in rows if r.get("repo") == repo]
        repo_rows.sort(key=lambda r: r["instance_id"])
        selected.extend(repo_rows[:per_repo])
    return selected


def retrieval_query(instance: dict) -> str:
    tests = " ".join(as_list(instance.get("FAIL_TO_PASS")))
    hints = instance.get("hints_text") or ""
    return "\n".join(
        part
        for part in [
            f"Repository: {instance['repo']}",
            f"Instance: {instance['instance_id']}",
            "Task: fix this SWE-Bench issue efficiently and verify with focused tests.",
            instance.get("problem_statement", ""),
            f"Failing tests: {tests}" if tests else "",
            f"Hints: {hints}" if hints else "",
        ]
        if part
    )


def render_skill_context(trace: dict, token_budget_chars: int) -> str:
    cards = trace.get("cards") or []
    if not cards:
        return ""
    parts = [
        "Potentially relevant skill cards. They may or may not apply to this issue.",
        "Use a card only if it matches the concrete failure, files, and tests you observe. Ignore irrelevant cards.",
    ]
    selected = [cards[0]]
    top_score = float(cards[0].get("score") or 0.0)
    for card in cards[1:3]:
        score = float(card.get("score") or 0.0)
        if math.isclose(top_score, score, abs_tol=0.15) or (top_score - score) < 0.15:
            selected.append(card)
    for idx, card in enumerate(selected, 1):
        procedure_value = card.get("procedure") or ""
        procedure = " ".join(procedure_value) if isinstance(procedure_value, list) else str(procedure_value)
        verification = " ".join(card.get("verification_steps") or [])
        constraints = " ".join(card.get("constraints") or [])
        parts.append(
            textwrap.dedent(
                f"""
                {idx}. {card.get('skill_name')}: {card.get('title') or card.get('description') or ''}
                   Procedure: {procedure}
                   Constraints: {constraints}
                   Verify: {verification}
                   Risk: {card.get('risk_summary') or ''}
                """
            ).strip()
        )
    text = "\n".join(parts)
    return text[:token_budget_chars].rstrip()


def render_prompt(instance: dict, arm: str, skill_context: str) -> str:
    memory_placeholder = (
        "Use the normal Fieldcraft memory retrieval lane for this task before editing."
        if arm in {"memory", "memory_skills"}
        else "Do not use Fieldcraft memory retrieval for this ablation arm."
    )
    skill_block = skill_context if arm in {"skills", "memory_skills"} and skill_context else ""
    skill_placeholder = (
        skill_block
        if skill_block
        else "No skill card is supplied for this ablation arm."
    )
    return textwrap.dedent(
        f"""
        You are solving a SWE-Bench-CL task.

        Ablation arm: {arm}

        Memory lane:
        {memory_placeholder}

        Skill lane:
        {skill_placeholder}

        Repository: {instance['repo']}
        Instance: {instance['instance_id']}
        Base commit: {instance.get('base_commit', '')}

        Problem statement:
        {instance.get('problem_statement', '').strip()}

        Hints:
        {(instance.get('hints_text') or '').strip()}

        Failing tests to make pass:
        {json.dumps(as_list(instance.get('FAIL_TO_PASS')))}

        Passing tests that should stay passing:
        {json.dumps(as_list(instance.get('PASS_TO_PASS')))}
        """
    ).strip() + "\n"


def build_merged_corpus(corpus_paths: list[Path], out_dir: Path) -> Path:
    merged = []
    seen = set()
    for path in corpus_paths:
        for row in read_jsonl(path):
            name = row.get("skill_name")
            if not name or name in seen:
                continue
            seen.add(name)
            merged.append(row)
    merged_path = out_dir / "merged_skill_corpus.jsonl"
    write_jsonl(merged_path, merged)
    return merged_path


def run_retrieval(args: argparse.Namespace, corpus_path: Path, queries_path: Path, traces_path: Path) -> None:
    cmd = [
        "cargo",
        "run",
        "-q",
        "-p",
        "fieldcraft-cli",
        "--",
        "eval",
        "skills",
        "--corpus",
        str(corpus_path),
        "--queries",
        str(queries_path),
        "--output-jsonl",
        str(traces_path),
        "--limit",
        str(args.skill_limit),
    ]
    subprocess.run(cmd, cwd=ROOT, check=True)


def load_traces(path: Path) -> dict[str, dict]:
    return {row["id"]: row for row in read_jsonl(path)}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dataset", type=Path, default=ROOT / "benchmarks/swe-bench-cl/data/swe-bench-cl-instances.jsonl")
    parser.add_argument(
        "--corpus",
        type=Path,
        nargs="+",
        default=[
            ROOT / "eval/skills/corpus/local_skills.jsonl",
            ROOT / "eval/skills/corpus/swe_bench_cl_seed_skills.jsonl",
        ],
    )
    parser.add_argument("--out-dir", type=Path, default=ROOT / "benchmarks/swe-bench-cl/skill_experiment")
    parser.add_argument("--repos", nargs="*", default=DEFAULT_REPOS)
    parser.add_argument("--per-repo", type=int, default=10)
    parser.add_argument("--skill-limit", type=int, default=3)
    parser.add_argument("--skill-budget-chars", type=int, default=5000)
    parser.add_argument("--skip-retrieval", action="store_true")
    args = parser.parse_args()
    if not args.out_dir.is_absolute():
        args.out_dir = ROOT / args.out_dir

    instances = select_instances(read_jsonl(args.dataset), args.repos, args.per_repo)
    if not instances:
        raise SystemExit("No matching SWE-Bench-CL instances selected")

    queries = [
        {
            "id": inst["instance_id"],
            "query": retrieval_query(inst),
            "expected": [],
            "source": "swe-bench-cl",
            "label_source": "unlabeled_runtime",
        }
        for inst in instances
    ]
    queries_path = args.out_dir / "skill_queries.jsonl"
    traces_path = args.out_dir / "skill_retrieval_traces.jsonl"
    write_jsonl(queries_path, queries)
    corpus_path = build_merged_corpus(args.corpus, args.out_dir)

    if not args.skip_retrieval:
        run_retrieval(args, corpus_path, queries_path, traces_path)
    elif not traces_path.exists():
        write_jsonl(traces_path, [{"id": q["id"], "cards": [], "confidence": 0.0, "mode": "none"} for q in queries])

    traces = load_traces(traces_path)
    manifest = []
    for inst in instances:
        trace = traces.get(inst["instance_id"], {"cards": []})
        skill_context = render_skill_context(trace, args.skill_budget_chars)
        for arm in ARMS:
            prompt_path = args.out_dir / "prompts" / arm / f"{inst['instance_id']}.md"
            prompt_path.parent.mkdir(parents=True, exist_ok=True)
            prompt_path.write_text(render_prompt(inst, arm, skill_context))
            cards = trace.get("cards") or []
            manifest.append(
                {
                    "instance_id": inst["instance_id"],
                    "repo": inst["repo"],
                    "repo_slug": repo_slug(inst["repo"]),
                    "arm": arm,
                    "prompt_path": str(prompt_path.relative_to(ROOT)),
                    "skill_confidence": trace.get("confidence", 0.0),
                    "skill_mode": trace.get("mode", "none"),
                    "skill_names": [c.get("skill_name") for c in cards],
                    "skill_context_chars": len(skill_context) if arm in {"skills", "memory_skills"} else 0,
                }
            )
    write_jsonl(args.out_dir / "manifest.jsonl", manifest)
    print(f"selected_instances={len(instances)} arms={len(ARMS)} manifest={args.out_dir / 'manifest.jsonl'}")
    print(f"retrieval_traces={traces_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
