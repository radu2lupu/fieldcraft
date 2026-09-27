#!/usr/bin/env python3
"""Pull-based SWE-Bench-CL four-tier runner.

This runner evaluates the product shape Fieldcraft is supposed to support:
Codex is given access to retrieval tools, but memory and skill content are not
front-loaded into the prompt. The agent decides if and when to call
`lesson_retrieve` and/or `skill_retrieve`.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from swecl_codex_skill_runner import (
    DEFAULT_IDS,
    ROOT,
    as_list,
    git_diff,
    load_instances,
    prepare_worktree,
)
from swecl_online_four_tier_runner import (
    eval_summary,
    evaluate_one,
    memory_count,
    project_slug,
    read_jsonl,
    run_id,
    summarize_memory,
    write_json,
)


PRED_ROOT = ROOT / "benchmarks/swe-bench-cl/predictions/codex-pilot"
RESULT_ROOT = ROOT / "benchmarks/swe-bench-cl/results"
FOUR_TIER_ROOT = ROOT / "benchmarks/swe-bench-cl/four-tier"

TIERS: dict[str, dict[str, bool]] = {
    "cold": {"memory": False, "skills": False, "learns": False, "suppressed": False, "tool_budget": False},
    "cold_budget": {"memory": False, "skills": False, "learns": False, "suppressed": False, "tool_budget": True},
    "cold_soft_budget": {"memory": False, "skills": False, "learns": False, "suppressed": False, "tool_budget": False, "soft_tool_budget": True},
    "cold_soft_budget_v2": {"memory": False, "skills": False, "learns": False, "suppressed": False, "tool_budget": False, "soft_tool_budget_v2": True},
    "cold_soft_budget_v21": {"memory": False, "skills": False, "learns": False, "suppressed": False, "tool_budget": False, "soft_tool_budget_v21": True},
    "memory": {"memory": True, "skills": False, "learns": True, "suppressed": False, "tool_budget": False, "soft_tool_budget": False},
    "skill": {"memory": False, "skills": True, "learns": False, "suppressed": False, "tool_budget": False, "soft_tool_budget": False},
    "skill_prefetch": {"memory": False, "skills": True, "learns": False, "suppressed": False, "tool_budget": False, "soft_tool_budget": False, "skill_prefetch": True},
    "memory_skill": {"memory": True, "skills": True, "learns": True, "suppressed": False, "tool_budget": False, "soft_tool_budget": False},
    # Diagnostic counterfactuals: expose the same tools and prompts but make
    # Fieldcraft return empty retrieval results. This separates retrieval value
    # from tool/schema/prompt overhead.
    "cold_with_tools": {"memory": True, "skills": False, "learns": False, "suppressed": True, "tool_budget": False, "soft_tool_budget": False},
    "memory_suppressed": {"memory": True, "skills": False, "learns": True, "suppressed": True, "tool_budget": False, "soft_tool_budget": False},
    "skill_suppressed": {"memory": False, "skills": True, "learns": False, "suppressed": True, "tool_budget": False, "soft_tool_budget": False},
    "skill_prefetch_suppressed": {"memory": False, "skills": True, "learns": False, "suppressed": True, "tool_budget": False, "soft_tool_budget": False, "skill_prefetch": True},
    "memory_skill_suppressed": {"memory": True, "skills": True, "learns": True, "suppressed": True, "tool_budget": False, "soft_tool_budget": False},
}


def write_jsonl(path: Path, rows: list[dict[str, Any]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w") as f:
        for row in rows:
            f.write(json.dumps(row, sort_keys=True) + "\n")


def sanitize_path_segment(value: str) -> str:
    return re.sub(r"[^A-Za-z0-9._-]+", "-", value).strip("-")[:96] or "skill"


def copy_codex_auth(codex_home: Path) -> None:
    codex_home.mkdir(parents=True, exist_ok=True)
    src = Path.home() / ".codex" / "auth.json"
    if src.exists():
        shutil.copy2(src, codex_home / "auth.json")


def write_codex_config(
    codex_home: Path,
    worktree: Path,
    fieldcraft_home: Path,
    tools_enabled: bool,
    retrieval_suppressed: bool,
) -> None:
    copy_codex_auth(codex_home)
    lines = [
        'model = "gpt-5.4-mini"',
        'model_reasoning_effort = "low"',
        "",
        f'[projects."{worktree}"]',
        'trust_level = "trusted"',
        "",
    ]
    if tools_enabled:
        lines.extend(
            [
                "[mcp_servers.fieldcraft]",
                'command = "cargo"',
                f'args = ["run", "-q", "-p", "fieldcraft-cli", "--", "serve"]',
                "",
                "[mcp_servers.fieldcraft.env]",
                f'FIELDCRAFT_HOME = "{fieldcraft_home}"',
                f'FIELDCRAFT_RETRIEVAL_SUPPRESSED = "{1 if retrieval_suppressed else 0}"',
                "",
            ]
        )
    (codex_home / "config.toml").write_text("\n".join(lines), encoding="utf-8")


def import_skill_corpus(fieldcraft_home: Path, corpus_paths: list[Path]) -> dict[str, int]:
    """Load JSONL skill records into the persistent DB used by MCP skill_retrieve."""
    fieldcraft_home.mkdir(parents=True, exist_ok=True)
    imported = 0
    blocked = 0
    fixture_root = fieldcraft_home / "skill-fixtures"
    if fixture_root.exists():
        shutil.rmtree(fixture_root)
    fixture_root.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env["FIELDCRAFT_HOME"] = str(fieldcraft_home)
    for corpus in corpus_paths:
        for row in read_jsonl(corpus):
            name = str(row.get("skill_name") or row.get("name") or row.get("id") or "").strip()
            if not name:
                continue
            description = str(row.get("description") or "").strip()
            triggers = row.get("triggers") or row.get("activation_cues") or []
            body = str(row.get("body") or row.get("procedure") or row.get("content") or "").strip()
            skill_dir = fixture_root / sanitize_path_segment(name)
            skill_dir.mkdir(parents=True, exist_ok=True)
            trigger_section = ""
            if isinstance(triggers, list) and triggers:
                trigger_section = "\n## When to use\n" + "\n".join(f"- {t}" for t in triggers[:20]) + "\n"
            skill_md = (
                f"---\nname: {name}\ndescription: {description}\n---\n"
                f"# {name}\n{trigger_section}\n{body}\n"
            )
            (skill_dir / "SKILL.md").write_text(skill_md, encoding="utf-8")
            proc = subprocess.run(
                ["cargo", "run", "-q", "-p", "fieldcraft-cli", "--", "skills", "ingest", "--source", str(skill_dir)],
                cwd=ROOT,
                env=env,
                capture_output=True,
                text=True,
                check=False,
                timeout=90,
            )
            if proc.returncode != 0:
                blocked += 1
            else:
                imported += 1
    return {"imported": imported, "blocked_or_failed": blocked}


def build_pull_prompt(
    instance: dict,
    tier: str,
    require_retrieval: bool,
    require_reveal: bool,
    skill_prefetch_text: str = "",
) -> str:
    spec = TIERS[tier]
    project_id = project_slug(instance["repo"])
    pull_rules = []
    if spec["memory"]:
        rule = (
            "- You have Fieldcraft memory tools. `lesson_retrieve` returns routing-only preview cards, not solution guidance. "
            f"Use it with project_id `{project_id}` when prior solved tasks in this same repository might help. "
            "If a preview title matches the files, APIs, or failure shape you observe, call `lesson_reveal` for that one lesson id with a short reason to access the full procedure. "
            "Ignore previews that do not match; use revealed content only when it matches concrete code you inspect."
        )
        if require_retrieval:
            rule += f" For this benchmark tier, after identifying the likely relevant files, call `lesson_retrieve` once with project_id `{project_id}`, the task, and file context before finalizing the fix plan."
        if require_reveal:
            rule += " For this benchmark tier, if `lesson_retrieve` returns any lessons, call `lesson_reveal` for the top returned lesson before deciding whether to use or ignore it."
        pull_rules.append(rule)
    if spec["skills"]:
        rule = (
            "- You have a Fieldcraft `skill_retrieve` tool. Use it when a compact workflow card for this framework or failure mode might help. "
            "Returned skill cards may be irrelevant; select or ignore them based on the concrete code you inspect."
        )
        if require_retrieval:
            rule += " For this benchmark tier, after identifying the likely failure mode, call `skill_retrieve` once with a focused query before finalizing the fix plan."
        pull_rules.append(rule)
    if not pull_rules:
        pull_rules.append("- No Fieldcraft retrieval tools are available in this tier. Solve from the repository only.")
    retrieval_text = "\n".join(pull_rules)
    budget_text = ""
    if spec.get("tool_budget"):
        budget_text = "\nTool budget:\nComplete this task in at most 7 non-retrieval tool calls. Think before each call; do not probe speculatively.\n"
    if spec.get("soft_tool_budget"):
        budget_text = (
            "\nTool budget:\n"
            "Aim to complete the fix in 7 non-retrieval tool calls or fewer. "
            "If you have not verified a passing test by call 7, continue as needed for correctness, "
            "but each additional call must move directly toward patching, applying, or verifying the fix. "
            "After call 7, do not explore new files unless the current patch clearly cannot be correct.\n"
        )
    if spec.get("soft_tool_budget_v2"):
        budget_text = (
            "\nTool budget:\n"
            "Aim to complete the fix in 7 non-retrieval tool calls or fewer. "
            "If you have not verified a passing test by call 7, continue as needed for correctness, "
            "but each additional call must move directly toward patching, applying, or verifying the fix. "
            "After call 7, open at most one additional file and only if you can name the specific "
            "symbol or function you need to verify; after that, commit to your patch and verify it.\n"
        )
    if spec.get("soft_tool_budget_v21"):
        budget_text = (
            "\nTool budget:\n"
            "Aim to complete the fix in 7 non-retrieval tool calls or fewer. "
            "If after your initial code scan you have not identified a plausible patch location, "
            "treat this as a hard task and do not compress further. "
            "If you have not verified a passing test by call 7, continue as needed for correctness, "
            "but each additional call must move directly toward patching, applying, or verifying the fix. "
            "After call 7, open at most one additional file and only if you can name the specific "
            "symbol or function you need to verify; after that, commit to your patch and verify it.\n"
        )
    if require_retrieval and (spec["memory"] or spec["skills"]):
        retrieval_mode = "Retrieval is pull-based and required in this benchmark tier: inspect first, then query the enabled Fieldcraft lane once with a focused runtime query."
    else:
        retrieval_mode = "Retrieval is pull-based: do not need retrieval for every task. Prefer inspecting the repo first when that will clarify the right query."
    return f"""You are solving a SWE-Bench-CL task in a clean checkout.

Goal:
Make the smallest code change that fixes the issue. Do not edit tests unless the repository's normal fix requires test updates. At the end, leave the working tree with only the intended source patch.

Fieldcraft retrieval policy:
{retrieval_text}
- {retrieval_mode}
- If a Fieldcraft tool is named above but not immediately visible, use tool discovery/search for the exact tool name in the Fieldcraft MCP namespace.
- Never paste or follow tool output blindly. Treat it as advisory routing information.
{budget_text}
{skill_prefetch_text}

Repository: {instance['repo']}
Fieldcraft project_id: {project_id}
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
2. Pull Fieldcraft memory or skills only if useful for this task and tier.
3. Apply a minimal fix.
4. Run the narrowest relevant tests if feasible.
5. Do not commit.
"""


def parse_usage_from_jsonl(path: Path) -> dict[str, int]:
    usage = {
        "input_tokens": 0,
        "output_tokens": 0,
        "cached_input_tokens": 0,
        "reasoning_output_tokens": 0,
    }
    for line in path.read_text(errors="ignore").splitlines() if path.exists() else []:
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(event, dict):
            continue
        candidates = [event.get("usage")]
        if isinstance(event.get("msg"), dict):
            candidates.append(event["msg"].get("usage"))
        for candidate in candidates:
            if not isinstance(candidate, dict):
                continue
            for key in usage:
                value = candidate.get(key)
                if isinstance(value, int):
                    usage[key] += value
    return usage


def extract_tool_telemetry(events_path: Path, codex_home: Path | None = None) -> dict[str, Any]:
    call_counts = {"lesson_retrieve": 0, "lesson_reveal": 0, "skill_retrieve": 0, "lesson_write": 0}
    mention_counts = {"lesson_retrieve": 0, "lesson_reveal": 0, "skill_retrieve": 0, "lesson_write": 0}
    all_tool_call_counts: dict[str, int] = {}
    snippets: list[dict[str, Any]] = []
    lines = list(events_path.read_text(errors="ignore").splitlines()) if events_path.exists() else []
    if codex_home is not None:
        sessions_dir = codex_home / "sessions"
        if sessions_dir.exists():
            for session_path in sessions_dir.rglob("*.jsonl"):
                lines.extend(session_path.read_text(errors="ignore").splitlines())
    for line in lines:
        low = line.lower()
        matched = [name for name in mention_counts if name in low]
        for name in matched:
            mention_counts[name] += 1
        is_call = False
        parsed: Any
        try:
            parsed = json.loads(line)
            payload = parsed.get("payload") if isinstance(parsed, dict) else None
            item_type = payload.get("type") if isinstance(payload, dict) else None
            if item_type is None and isinstance(parsed, dict):
                item_type = parsed.get("type")
            call_name = ""
            if isinstance(payload, dict):
                call_name = str(payload.get("name") or payload.get("tool_name") or "")
            if isinstance(parsed, dict):
                call_name = call_name or str(parsed.get("name") or parsed.get("tool_name") or "")
            is_call = item_type == "function_call"
            if is_call:
                normalized_call_name = call_name.removeprefix("mcp__fieldcraft__")
                if normalized_call_name:
                    all_tool_call_counts[normalized_call_name] = (
                        all_tool_call_counts.get(normalized_call_name, 0) + 1
                    )
                for name in matched:
                    if normalized_call_name == name:
                        call_counts[name] += 1
        except json.JSONDecodeError:
            parsed = line[:1000]
        if matched and len(snippets) < 30:
            snippets.append({"tools": matched, "event": parsed})
    retrieval_tool_names = {"lesson_retrieve", "lesson_reveal", "skill_retrieve", "lesson_write"}
    non_retrieval_tool_call_counts = {
        name: count
        for name, count in sorted(all_tool_call_counts.items())
        if name not in retrieval_tool_names
    }
    return {
        "tool_call_counts": call_counts,
        "tool_mention_counts": mention_counts,
        "all_tool_call_counts": dict(sorted(all_tool_call_counts.items())),
        "non_retrieval_tool_call_counts": non_retrieval_tool_call_counts,
        "non_retrieval_tool_call_total": sum(non_retrieval_tool_call_counts.values()),
        "tool_event_snippets": snippets,
    }


def retrieval_log_watermarks(fieldcraft_home: Path) -> dict[str, int]:
    db = fieldcraft_home / "memory.sqlite3"
    if not db.exists():
        return {"memory": 0, "skill": 0}
    with sqlite3.connect(db) as conn:
        return {
            "memory": int(conn.execute("select coalesce(max(id), 0) from retrieval_log").fetchone()[0]),
            "skill": int(conn.execute("select coalesce(max(id), 0) from skill_retrieval_log").fetchone()[0]),
        }


def fetch_retrieval_events(fieldcraft_home: Path, since: dict[str, int], suppressed: bool) -> dict[str, Any]:
    db = fieldcraft_home / "memory.sqlite3"
    if not db.exists():
        return {"memory_retrieval_events": [], "skill_retrieval_events": []}
    with sqlite3.connect(db) as conn:
        memory_columns = {
            row[1]
            for row in conn.execute("pragma table_info(retrieval_log)").fetchall()
        }
        raw_candidate_select = (
            "raw_candidate_ids, raw_candidate_scores, raw_candidate_titles"
            if {"raw_candidate_ids", "raw_candidate_scores", "raw_candidate_titles"}.issubset(memory_columns)
            else "'[]' as raw_candidate_ids, '[]' as raw_candidate_scores, '[]' as raw_candidate_titles"
        )
        memory_rows = conn.execute(
            f"""
            select id, project_id, query_text, result_ids, result_scores, result_count, latency_ms, created_at,
                   {raw_candidate_select}
            from retrieval_log
            where id > ?
            order by id
            """,
            (since.get("memory", 0),),
        ).fetchall()
        skill_rows = conn.execute(
            """
            select id, query_text, result_card_ids, result_scores, result_count, created_at
            from skill_retrieval_log
            where id > ?
            order by id
            """,
            (since.get("skill", 0),),
        ).fetchall()
    return {
        "memory_retrieval_events": [
            {
                "id": row[0],
                "project_id": row[1],
                "query_text": row[2],
                "result_ids": parse_json_field(row[3]),
                "result_scores": parse_json_field(row[4]),
                "result_count": row[5],
                "latency_ms": row[6],
                "created_at": row[7],
                "raw_candidate_ids": parse_json_field(row[8]) if len(row) > 8 else [],
                "raw_candidate_scores": parse_json_field(row[9]) if len(row) > 9 else [],
                "raw_candidate_titles": parse_json_field(row[10]) if len(row) > 10 else [],
                "retrieval_suppressed": suppressed,
            }
            for row in memory_rows
        ],
        "skill_retrieval_events": [
            {
                "id": row[0],
                "query_text": row[1],
                "result_card_ids": parse_json_field(row[2]),
                "result_scores": parse_json_field(row[3]),
                "result_count": row[4],
                "created_at": row[5],
                "retrieval_suppressed": suppressed,
            }
            for row in skill_rows
        ],
    }


def parse_json_field(value: Any) -> Any:
    if not isinstance(value, str):
        return value
    try:
        return json.loads(value)
    except json.JSONDecodeError:
        return value


def skill_prefetch_query(instance: dict) -> str:
    parts = [
        f"Repository: {instance.get('repo', '')}",
        f"Instance: {instance.get('instance_id', '')}",
        "Task: SWE-Bench bugfix in an existing repository.",
        str(instance.get("problem_statement") or "").strip(),
        "Failing tests: " + " ".join(as_list(instance.get("FAIL_TO_PASS"))),
    ]
    return "\n".join(part for part in parts if part.strip())


def run_skill_prefetch_cli(fieldcraft_home: Path, instance: dict, suppressed: bool, limit: int = 3) -> dict[str, Any]:
    query = skill_prefetch_query(instance)
    if suppressed:
        return {"query": query, "cards": [], "suppressed": True}
    env = os.environ.copy()
    env["FIELDCRAFT_HOME"] = str(fieldcraft_home)
    proc = subprocess.run(
        [
            "cargo",
            "run",
            "-q",
            "-p",
            "fieldcraft-cli",
            "--",
            "skills",
            "query",
            query,
            "--limit",
            str(limit),
        ],
        cwd=ROOT,
        env=env,
        capture_output=True,
        text=True,
        check=False,
        timeout=90,
    )
    cards: list[dict[str, Any]] = []
    current: dict[str, Any] | None = None
    for line in proc.stdout.splitlines():
        if line.startswith("SKILL CARD: "):
            if current:
                cards.append(current)
            current = {"title": line.removeprefix("SKILL CARD: ").strip(), "body": []}
        elif current is not None:
            current["body"].append(line)
    if current:
        cards.append(current)
    return {
        "query": query,
        "cards": cards[:limit],
        "suppressed": False,
        "returncode": proc.returncode,
        "stderr": proc.stderr[-2000:],
    }


def format_skill_prefetch(prefetch: dict[str, Any]) -> str:
    if prefetch.get("suppressed"):
        return (
            "\nFieldcraft skill prefetch:\n"
            "- Skill prefetch ran for this tier, but no skill cards are available in this suppressed control. "
            "Proceed without skill guidance.\n"
        )
    cards = prefetch.get("cards") or []
    if not cards:
        return (
            "\nFieldcraft skill prefetch:\n"
            "- No skill card met the retrieval threshold for this task. Proceed without skill guidance.\n"
        )
    lines = [
        "\nFieldcraft skill prefetch:",
        "These compact workflow cards were retrieved before solving. Use only cards that match the concrete code and failure mode you inspect.",
    ]
    for idx, card in enumerate(cards[:3], 1):
        body = "\n".join(str(part) for part in card.get("body", []) if str(part).strip())
        body = "\n".join(body.splitlines()[:18])
        lines.append(f"\nCard {idx}: {card.get('title', '').strip()}\n{body}")
    return "\n".join(lines) + "\n"


def run_codex_pull(
    args: argparse.Namespace,
    tier: str,
    instance: dict,
    worktree: Path,
    task_dir: Path,
    fieldcraft_home: Path,
) -> tuple[int, float, dict[str, Any]]:
    prefetch = {"query": "", "cards": [], "suppressed": False}
    if TIERS[tier].get("skill_prefetch"):
        prefetch = run_skill_prefetch_cli(fieldcraft_home, instance, TIERS[tier]["suppressed"])
    prompt = build_pull_prompt(
        instance,
        tier,
        args.require_retrieval,
        args.require_reveal,
        format_skill_prefetch(prefetch) if TIERS[tier].get("skill_prefetch") else "",
    )
    prompt_path = task_dir / "prompt.md"
    events_path = task_dir / "codex_events.jsonl"
    final_path = task_dir / "final.txt"
    codex_home = task_dir / "codex-home"
    write_codex_config(
        codex_home,
        worktree,
        fieldcraft_home,
        TIERS[tier]["memory"] or TIERS[tier]["skills"],
        TIERS[tier]["suppressed"],
    )
    prompt_path.write_text(prompt, encoding="utf-8")
    env = os.environ.copy()
    env["CODEX_HOME"] = str(codex_home)
    env["FIELDCRAFT_HOME"] = str(fieldcraft_home)
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
    retrieval_before = retrieval_log_watermarks(fieldcraft_home)
    start = time.time()
    with prompt_path.open() as stdin, events_path.open("w") as stdout:
        proc = subprocess.run(cmd, stdin=stdin, stdout=stdout, stderr=subprocess.STDOUT, text=True, env=env)
    wall = time.time() - start
    telemetry = parse_usage_from_jsonl(events_path)
    telemetry.update(extract_tool_telemetry(events_path, codex_home))
    telemetry.update(fetch_retrieval_events(fieldcraft_home, retrieval_before, TIERS[tier]["suppressed"]))
    if TIERS[tier].get("skill_prefetch"):
        telemetry["skill_prefetch"] = prefetch
    return proc.returncode, wall, telemetry


def append_prediction(pred_path: Path, instance_id: str, patch: str) -> None:
    pred_path.parent.mkdir(parents=True, exist_ok=True)
    rows = [row for row in read_jsonl(pred_path) if row.get("instance_id") != instance_id]
    rows.append({"instance_id": instance_id, "model_patch": patch, "model_name_or_path": "codex-pull-fieldcraft"})
    write_jsonl(pred_path, rows)


def append_telemetry(path: Path, row: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a") as f:
        f.write(json.dumps(row, sort_keys=True) + "\n")


def upsert_canonical_result(path: Path, row: dict[str, Any]) -> None:
    rows = [
        existing
        for existing in read_jsonl(path)
        if not (
            existing.get("tier_key") == row.get("tier_key")
            and existing.get("instance_id") == row.get("instance_id")
        )
    ]
    rows.append(row)
    write_jsonl(path, rows)


def prediction_for(pred_path: Path, instance_id: str) -> dict[str, Any] | None:
    for row in read_jsonl(pred_path):
        if row.get("instance_id") == instance_id:
            return row
    return None


def classify_eval_result(result: dict[str, Any], patch: str) -> str:
    if result.get("resolved"):
        return "resolved"
    if result.get("empty_patch") or not patch.strip():
        return "empty_patch"
    if result.get("apply_failed"):
        return "patch_apply_failed"
    error = str(result.get("error_msg") or "").lower()
    if "could not apply" in error or "patch" in error and "fail" in error:
        return "patch_apply_failed"
    if result.get("error_msg"):
        return "eval_error"
    return "tests_failed"


def canonical_result_row(
    args: argparse.Namespace,
    run_prefix: str,
    tier: str,
    tier_key: str,
    repeat_index: int,
    instance: dict[str, Any],
    prediction: dict[str, Any] | None,
    telemetry: dict[str, Any] | None,
    eval_result: dict[str, Any],
) -> dict[str, Any]:
    telemetry = telemetry or {}
    patch = str((prediction or {}).get("model_patch") or "")
    tool_counts = telemetry.get("tool_call_counts") or {}
    return {
        "schema_version": 1,
        "run_prefix": run_prefix,
        "tier": tier,
        "tier_key": tier_key,
        "repeat_index": repeat_index,
        "run_id": run_id(run_prefix, tier),
        "instance_id": instance["instance_id"],
        "repo": instance["repo"],
        "model": args.model,
        "reasoning": args.reasoning,
        "resolved": bool(eval_result.get("resolved")),
        "failure_class": classify_eval_result(eval_result, patch),
        "patch_chars": len(patch),
        "nonempty_patch": bool(patch.strip()),
        "input_tokens": telemetry.get("input_tokens"),
        "output_tokens": telemetry.get("output_tokens"),
        "cached_input_tokens": telemetry.get("cached_input_tokens"),
        "reasoning_output_tokens": telemetry.get("reasoning_output_tokens"),
        "wall_s": telemetry.get("wall_s"),
        "non_retrieval_tool_call_total": telemetry.get("non_retrieval_tool_call_total"),
        "lesson_retrieve_calls": int(tool_counts.get("lesson_retrieve") or 0),
        "lesson_reveal_calls": int(tool_counts.get("lesson_reveal") or 0),
        "skill_retrieve_calls": int(tool_counts.get("skill_retrieve") or 0),
        "memory_retrieval_events": len(telemetry.get("memory_retrieval_events") or []),
        "skill_retrieval_events": len(telemetry.get("skill_retrieval_events") or []),
        "skill_prefetch_cards": len((telemetry.get("skill_prefetch") or {}).get("cards") or []),
        "retrieval_suppressed": TIERS[tier]["suppressed"],
        "memory_tool_enabled": TIERS[tier]["memory"],
        "skill_tool_enabled": TIERS[tier]["skills"],
        "memory_learning_enabled": TIERS[tier]["learns"],
        "eval_error": eval_result.get("error_msg"),
        "created_at": datetime.now(timezone.utc).isoformat(),
    }


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


def skill_card_count(fc_home: Path) -> int:
    db = fc_home / "memory.sqlite3"
    if not db.exists():
        return 0
    with sqlite3.connect(db) as conn:
        return int(conn.execute("select count(*) from skill_cards").fetchone()[0])


def seed_fieldcraft_home(target: Path, seed: Path) -> None:
    seed_db = seed / "memory.sqlite3"
    if not seed_db.exists():
        raise FileNotFoundError(f"seed FIELDCRAFT_HOME has no memory.sqlite3: {seed_db}")
    target.mkdir(parents=True, exist_ok=True)
    target_db = target / "memory.sqlite3"
    if target_db.exists():
        return
    shutil.copy2(seed_db, target_db)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dataset", type=Path, default=ROOT / "benchmarks/swe-bench-cl/data/swe-bench-cl-instances.jsonl")
    parser.add_argument("--run-prefix", required=True)
    parser.add_argument("--instance-ids", nargs="*", default=DEFAULT_IDS)
    parser.add_argument("--model", default="gpt-5.4-mini")
    parser.add_argument("--reasoning", default="low")
    parser.add_argument("--eval-timeout", type=int, default=300)
    parser.add_argument("--corpus", type=Path, nargs="+", default=[ROOT / "eval/skills/corpus/swecl_normalized_benchmark_skills.jsonl"])
    parser.add_argument("--tiers", nargs="+", choices=sorted(TIERS), default=["cold", "memory", "skill", "memory_skill"])
    parser.add_argument(
        "--require-retrieval",
        action="store_true",
        help="Require one runtime pull from each enabled Fieldcraft lane after initial repo inspection.",
    )
    parser.add_argument(
        "--require-reveal",
        action="store_true",
        help="For memory tiers, require revealing the top returned lesson when lesson_retrieve returns a non-empty result.",
    )
    parser.add_argument("--out-root", type=Path, default=PRED_ROOT)
    parser.add_argument("--result-root", type=Path, default=RESULT_ROOT)
    parser.add_argument("--work-root", type=Path, default=ROOT / ".swecl-cache/work-pull")
    parser.add_argument("--manifest-root", type=Path, default=FOUR_TIER_ROOT)
    parser.add_argument("--resume", action="store_true")
    parser.add_argument(
        "--repeats",
        type=int,
        default=1,
        help="Run each requested tier N times with isolated run IDs and Fieldcraft homes.",
    )
    parser.add_argument("--allow-cross-repo-memory", action="store_true")
    parser.add_argument(
        "--seed-fieldcraft-home",
        type=Path,
        help="Optional FIELDCRAFT_HOME whose memory.sqlite3 seeds memory-enabled tiers before the run.",
    )
    args = parser.parse_args()
    if args.repeats < 1:
        raise SystemExit("--repeats must be >= 1")

    instances = load_instances(args.dataset, args.instance_ids)
    by_id = {inst["instance_id"]: inst for inst in instances}
    projects = sorted({project_slug(inst["repo"]) for inst in instances})
    repos = sorted({inst["repo"] for inst in instances})
    if len(repos) != 1 and not args.allow_cross_repo_memory:
        raise SystemExit(f"pull benchmark must run one repo sequence at a time; got repos={repos}")

    run_root = args.manifest_root / args.run_prefix
    manifest = {
        "run_prefix": args.run_prefix,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "dataset": str(args.dataset),
        "instance_ids": args.instance_ids,
        "model": args.model,
        "reasoning": args.reasoning,
        "mode": "pull_based_online_memory_and_skill_learning",
        "require_retrieval": args.require_retrieval,
        "require_reveal": args.require_reveal,
        "repo_isolation": "single_repo_required",
        "repos": repos,
        "skill_corpus": [str(path) for path in args.corpus],
        "seed_fieldcraft_home": str(args.seed_fieldcraft_home) if args.seed_fieldcraft_home else None,
        "repeats": args.repeats,
        "tiers": {},
    }

    scheduled_tiers: list[dict[str, Any]] = []
    for repeat_index in range(1, args.repeats + 1):
        effective_prefix = args.run_prefix if args.repeats == 1 else f"{args.run_prefix}-r{repeat_index:02d}"
        for tier in args.tiers:
            tier_key = tier if args.repeats == 1 else f"{tier}__r{repeat_index:02d}"
            scheduled_tiers.append(
                {
                    "tier": tier,
                    "tier_key": tier_key,
                    "run_prefix": effective_prefix,
                    "repeat_index": repeat_index,
                }
            )

    for scheduled in scheduled_tiers:
        tier = scheduled["tier"]
        tier_key = scheduled["tier_key"]
        rid = run_id(scheduled["run_prefix"], tier)
        fc_home = run_root / "fieldcraft-home" / tier_key
        report_dir = args.result_root / args.run_prefix / rid
        manifest["tiers"][tier_key] = {
            "run_id": rid,
            "tier": tier,
            "repeat_index": scheduled["repeat_index"],
            "prediction_path": str(args.out_root / rid / f"{rid}.jsonl"),
            "telemetry_path": str(args.out_root / rid / "telemetry.jsonl"),
            "eval_report_dir": str(report_dir),
            "fieldcraft_home": str(fc_home),
            "memory_tool_enabled": TIERS[tier]["memory"],
            "memory_learning_enabled": TIERS[tier]["learns"],
            "skill_tool_enabled": TIERS[tier]["skills"],
            "retrieval_suppressed": TIERS[tier]["suppressed"],
        }
    write_json(run_root / "manifest.json", manifest)

    learned_log = run_root / "learned_memories.jsonl"
    learned = {(row.get("tier_key") or row.get("tier"), row.get("instance_id")) for row in read_jsonl(learned_log)}
    canonical_path = run_root / "canonical_results.jsonl"

    for scheduled in scheduled_tiers:
        tier = scheduled["tier"]
        tier_key = scheduled["tier_key"]
        repeat_index = scheduled["repeat_index"]
        effective_prefix = scheduled["run_prefix"]
        effective_args = argparse.Namespace(**vars(args))
        effective_args.run_prefix = effective_prefix
        rid = run_id(effective_prefix, tier)
        fc_home = Path(manifest["tiers"][tier_key]["fieldcraft_home"])
        fc_home.mkdir(parents=True, exist_ok=True)
        if args.seed_fieldcraft_home and TIERS[tier]["memory"]:
            seed_fieldcraft_home(fc_home, args.seed_fieldcraft_home)
            manifest["tiers"][tier_key]["seeded_from"] = str(args.seed_fieldcraft_home)
            write_json(run_root / "manifest.json", manifest)
        if TIERS[tier]["skills"] and skill_card_count(fc_home) == 0:
            manifest["tiers"][tier_key]["skill_import"] = import_skill_corpus(fc_home, args.corpus)
            manifest["tiers"][tier_key]["skill_card_count"] = skill_card_count(fc_home)
            write_json(run_root / "manifest.json", manifest)
        print(f"\n== tier {tier_key} ({rid}) ==")
        pred_path = Path(manifest["tiers"][tier_key]["prediction_path"])
        telemetry_path = Path(manifest["tiers"][tier_key]["telemetry_path"])
        for iid in args.instance_ids:
            print(f"-- {iid}")
            inst = by_id[iid]
            task_dir = args.out_root / rid / iid
            telemetry: dict[str, Any] | None = None
            if args.resume and prediction_for(pred_path, iid):
                print(f"{iid}: prediction exists, skipping codex")
            else:
                worktree = prepare_worktree(inst, args.work_root / rid)
                task_dir.mkdir(parents=True, exist_ok=True)
                rc, wall, telemetry = run_codex_pull(args, tier, inst, worktree, task_dir, fc_home)
                patch = git_diff(worktree)
                append_prediction(pred_path, iid, patch)
                telemetry.update(
                    {
                        "tier": tier,
                        "tier_key": tier_key,
                        "repeat_index": repeat_index,
                        "instance_id": iid,
                        "returncode": rc,
                        "wall_s": round(wall, 3),
                        "patch_chars": len(patch),
                        "nonempty_patch": bool(patch.strip()),
                    }
                )
                append_telemetry(telemetry_path, telemetry)
            result = evaluate_one(effective_args, tier, iid)
            print(f"{iid}: resolved={result.get('resolved')} error={result.get('error_msg')}")
            prediction = prediction_for(pred_path, iid)
            if telemetry is None:
                for row in reversed(read_jsonl(telemetry_path)):
                    if row.get("instance_id") == iid:
                        telemetry = row
                        break
            upsert_canonical_result(
                canonical_path,
                canonical_result_row(
                    effective_args,
                    effective_prefix,
                    tier,
                    tier_key,
                    repeat_index,
                    inst,
                    prediction,
                    telemetry,
                    result,
                ),
            )
            if TIERS[tier]["learns"] and result.get("resolved") and (tier_key, iid) not in learned:
                prediction = prediction_for(pred_path, iid)
                if prediction:
                    write_memory(args, fc_home, inst, prediction, result)
                    learned.add((tier_key, iid))
                    with learned_log.open("a") as f:
                        f.write(
                            json.dumps(
                                {
                                    "tier": tier,
                                    "tier_key": tier_key,
                                    "repeat_index": repeat_index,
                                    "instance_id": iid,
                                    "fieldcraft_home": str(fc_home),
                                }
                            )
                            + "\n"
                        )

        report_dir = Path(manifest["tiers"][tier_key]["eval_report_dir"])
        write_json(report_dir / "summary.json", eval_summary(report_dir, args.instance_ids))
        manifest["tiers"][tier_key]["final_memory_counts"] = memory_count(fc_home, projects)
        manifest["tiers"][tier_key]["final_skill_card_count"] = skill_card_count(fc_home)
        write_json(run_root / "manifest.json", manifest)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
