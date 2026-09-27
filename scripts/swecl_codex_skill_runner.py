#!/usr/bin/env python3
"""Run Codex on SWE-Bench-CL instances with optional Fieldcraft memory+skills.

This is intentionally small and local-first: it prepares a clean checkout for
each instance, builds a prompt with optional memory and skill-card context, runs
`codex exec`, and writes SWE-Bench prediction JSONL plus per-task telemetry.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import time
from pathlib import Path

from swecl_skill_ablation import (
    ROOT,
    as_list,
    build_merged_corpus,
    read_jsonl,
    render_skill_context,
    retrieval_query,
    run_retrieval,
    write_jsonl,
)


DEFAULT_IDS = [
    "django__django-9296",
    "django__django-10097",
    "django__django-10880",
    "django__django-10914",
    "django__django-10999",
    "django__django-11066",
    "django__django-11099",
]


def load_instances(dataset: Path, ids: list[str]) -> list[dict]:
    wanted = set(ids)
    rows = [row for row in read_jsonl(dataset) if row.get("instance_id") in wanted]
    by_id = {row["instance_id"]: row for row in rows}
    missing = [iid for iid in ids if iid not in by_id]
    if missing:
        raise SystemExit(f"missing instances in dataset: {missing}")
    return [by_id[iid] for iid in ids]


def repo_cache_path(repo: str) -> Path:
    return ROOT / ".swecl-cache" / "repos" / repo.replace("/", "__")


def ensure_repo_cache(repo: str) -> Path:
    path = repo_cache_path(repo)
    if path.exists():
        subprocess.run(["git", "fetch", "--all", "--tags", "--prune"], cwd=path, check=True)
        return path
    path.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(["git", "clone", f"https://github.com/{repo}.git", str(path)], check=True)
    return path


def prepare_worktree(instance: dict, work_root: Path) -> Path:
    cache = ensure_repo_cache(instance["repo"])
    worktree = work_root / instance["instance_id"]
    if worktree.exists():
        shutil.rmtree(worktree)
    subprocess.run(["git", "clone", "--shared", str(cache), str(worktree)], check=True)
    subprocess.run(["git", "checkout", instance["base_commit"]], cwd=worktree, check=True)
    subprocess.run(["git", "clean", "-fdx"], cwd=worktree, check=True)
    return worktree


def build_skill_traces(args: argparse.Namespace, instances: list[dict], out_dir: Path) -> dict[str, dict]:
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
    queries_path = out_dir / "skill_queries.jsonl"
    traces_path = out_dir / "skill_retrieval_traces.jsonl"
    write_jsonl(queries_path, queries)
    corpus_path = build_merged_corpus(args.corpus, out_dir)
    run_retrieval(args, corpus_path, queries_path, traces_path)
    return {row["id"]: row for row in read_jsonl(traces_path)}


def memory_context(instance: dict, enabled: bool, limit: int, fieldcraft_home: Path | None = None) -> str:
    if not enabled:
        return ""
    query = retrieval_query(instance)
    cmd = [
        "cargo",
        "run",
        "-q",
        "-p",
        "fieldcraft-cli",
        "--",
        "query",
        query,
        "--project",
        instance["repo"].replace("/", "-"),
        "--limit",
        str(limit),
    ]
    try:
        env = os.environ.copy()
        if fieldcraft_home is not None:
            env["FIELDCRAFT_HOME"] = str(fieldcraft_home)
        out = subprocess.run(
            cmd,
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
            env=env,
        )
    except subprocess.TimeoutExpired:
        return "Fieldcraft memory retrieval timed out."
    text = (out.stdout or "").strip()
    return text[:6000]


def memory_has_context(text: str) -> bool:
    stripped = (text or "").strip()
    if not stripped:
        return False
    low = stripped.lower()
    empty_markers = [
        "no memories found",
        "no lessons found",
        "no memory",
        "0 memories",
        "retrieval timed out",
        "error:",
    ]
    return not any(marker in low for marker in empty_markers)


def prompt_for(
    instance: dict,
    memory: str,
    skills: str,
    memory_enabled: bool,
    skills_enabled: bool,
) -> str:
    context_sections = []
    if memory_enabled:
        memory_block = memory or "No Fieldcraft memory context was supplied."
        context_sections.append(
            "Fieldcraft memory lane:\n"
            "The following memories may or may not be useful. Use them only if they match the concrete repo, files, and failure.\n"
            f"{memory_block}"
        )
    if skills_enabled:
        skill_block = skills or "No skill card was supplied."
        context_sections.append(f"Fieldcraft skill lane:\n{skill_block}")
    context_block = "\n\n".join(context_sections)
    if context_block:
        context_block = f"\n{context_block}\n"
    return f"""You are solving a SWE-Bench-CL task in a clean checkout.

Goal:
Make the smallest code change that fixes the issue. Do not edit tests unless the repository's normal fix requires test updates. At the end, leave the working tree with only the intended source patch.
{context_block}

Repository: {instance['repo']}
Instance: {instance['instance_id']}
Base commit: {instance['base_commit']}

Problem statement:
{instance.get('problem_statement', '').strip()}

Hints:
{(instance.get('hints_text') or '').strip()}

Failing tests to make pass:
{json.dumps(as_list(instance.get('FAIL_TO_PASS')), indent=2)}

Passing tests that should stay passing:
{json.dumps(as_list(instance.get('PASS_TO_PASS')), indent=2)}

Instructions:
1. Inspect the relevant files.
2. Apply a minimal fix.
3. Run the narrowest relevant tests if feasible.
4. Do not commit.
"""


def parse_usage_from_jsonl(path: Path) -> dict:
    usage = {
        "input_tokens": 0,
        "output_tokens": 0,
        "cached_input_tokens": 0,
        "reasoning_output_tokens": 0,
    }
    if not path.exists():
        return usage
    for line in path.read_text(errors="ignore").splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        event_usage = event.get("usage")
        if isinstance(event_usage, dict):
            for key in usage:
                value = event_usage.get(key)
                if isinstance(value, int):
                    usage[key] += value
            continue
        for key in usage:
            value = event.get(key)
            if isinstance(value, int):
                usage[key] += value
    return usage


def run_codex(args: argparse.Namespace, worktree: Path, prompt: str, task_dir: Path) -> tuple[int, float, dict]:
    prompt_path = task_dir / "prompt.md"
    events_path = task_dir / "codex_events.jsonl"
    final_path = task_dir / "final.txt"
    prompt_path.write_text(prompt)
    cmd = [
        "codex",
        "exec",
        "--json",
        "--output-last-message",
        str(final_path),
        "-m",
        args.model,
        "--cd",
        str(worktree),
        "--sandbox",
        "danger-full-access",
        "-",
    ]
    if args.reasoning:
        cmd.extend(["-c", f'reasoning_effort="{args.reasoning}"'])
    start = time.time()
    with prompt_path.open() as stdin, events_path.open("w") as stdout:
        proc = subprocess.run(cmd, stdin=stdin, stdout=stdout, stderr=subprocess.STDOUT, text=True)
    wall = time.time() - start
    usage = parse_usage_from_jsonl(events_path)
    return proc.returncode, wall, usage


def git_diff(worktree: Path) -> str:
    return subprocess.run(
        ["git", "diff", "--binary"],
        cwd=worktree,
        capture_output=True,
        text=True,
        check=False,
    ).stdout


def card_attribution(cards: list[dict], patch: str) -> dict:
    patch_lower = patch.lower()
    results = []
    for card in cards:
        terms = set()
        for value in [
            card.get("skill_name", ""),
            card.get("title", ""),
            card.get("description", ""),
            card.get("procedure", ""),
            " ".join(card.get("activation_cues") or []),
            " ".join(card.get("verification_steps") or []),
        ]:
            for token in re.findall(r"[A-Za-z_][A-Za-z0-9_]{3,}", str(value)):
                lower = token.lower()
                if lower not in {"django", "python", "test", "tests", "file", "using", "with"}:
                    terms.add(lower)
        matched = sorted(term for term in terms if term in patch_lower)
        results.append(
            {
                "skill_name": card.get("skill_name"),
                "matched_terms": matched[:20],
                "matched_term_count": len(matched),
                "candidate_term_count": len(terms),
            }
        )
    top = results[0] if results else {}
    return {
        "top_skill_matched_term_count": top.get("matched_term_count", 0),
        "top_skill_candidate_term_count": top.get("candidate_term_count", 0),
        "cards": results,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dataset", type=Path, default=ROOT / "benchmarks/swe-bench-cl/data/swe-bench-cl-instances.jsonl")
    parser.add_argument("--run-id", default="pilot-warm-skills-django-django-minilowv3-7")
    parser.add_argument("--instance-ids", nargs="*", default=DEFAULT_IDS)
    parser.add_argument("--model", default="gpt-5.4-mini")
    parser.add_argument("--reasoning", default="low")
    parser.add_argument("--memory", action="store_true")
    parser.add_argument("--skills", action="store_true")
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
    parser.add_argument("--out-root", type=Path, default=ROOT / "benchmarks/swe-bench-cl/predictions/codex-pilot")
    parser.add_argument("--work-root", type=Path, default=ROOT / ".swecl-cache/work")
    parser.add_argument(
        "--fieldcraft-home",
        type=Path,
        help="Optional isolated Fieldcraft home used for memory retrieval.",
    )
    parser.add_argument("--resume", action="store_true")
    args = parser.parse_args()

    instances = load_instances(args.dataset, args.instance_ids)
    run_dir = args.out_root / args.run_id
    task_root = run_dir / "tasks"
    run_dir.mkdir(parents=True, exist_ok=True)
    task_root.mkdir(parents=True, exist_ok=True)
    pred_path = run_dir / f"{args.run_id}.jsonl"
    telemetry_path = run_dir / "telemetry.jsonl"

    skill_traces = build_skill_traces(args, instances, run_dir) if args.skills else {}
    existing = set()
    if args.resume and pred_path.exists():
        existing = {row["instance_id"] for row in read_jsonl(pred_path)}

    predictions = []
    telemetry = []
    if args.resume and pred_path.exists():
        predictions.extend(read_jsonl(pred_path))
    if args.resume and telemetry_path.exists():
        telemetry.extend(read_jsonl(telemetry_path))

    for position, instance in enumerate(instances, 1):
        iid = instance["instance_id"]
        if iid in existing:
            continue
        task_dir = task_root / iid
        task_dir.mkdir(parents=True, exist_ok=True)
        worktree = prepare_worktree(instance, args.work_root / args.run_id)
        mem = memory_context(instance, args.memory, args.memory_limit, args.fieldcraft_home)
        mem_has_context = memory_has_context(mem)
        skill_context = ""
        if args.skills:
            skill_context = render_skill_context(
                skill_traces.get(iid, {"cards": []}),
                args.skill_budget_chars,
            )
        prompt = prompt_for(instance, mem, skill_context, args.memory, args.skills)
        rc, wall, usage = run_codex(args, worktree, prompt, task_dir)
        patch = git_diff(worktree)
        cards = skill_traces.get(iid, {}).get("cards") or []
        attribution = card_attribution(cards, patch) if args.skills else {}
        prediction = {
            "instance_id": iid,
            "model_name_or_path": f"codex-{args.model}-{args.run_id}",
            "model_patch": patch,
        }
        predictions.append(prediction)
        telemetry.append(
            {
                "instance_id": iid,
                "run_id": args.run_id,
                "position": position,
                "rc": rc,
                "wall_s": round(wall, 2),
                "patch_chars": len(patch),
                "memory_enabled": args.memory,
                "memory_has_context": mem_has_context,
                "memory_context_chars": len(mem or ""),
                "memory_context_snapshot": (mem or "")[:1200],
                "skills_enabled": args.skills,
                "skill_names": [
                    card.get("skill_name")
                    for card in cards
                ],
                "skill_attribution": attribution,
                **usage,
            }
        )
        write_jsonl(pred_path, predictions)
        write_jsonl(telemetry_path, telemetry)
        print(f"{iid}: rc={rc} wall={wall:.1f}s patch_chars={len(patch)}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
