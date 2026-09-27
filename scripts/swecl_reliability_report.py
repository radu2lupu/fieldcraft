#!/usr/bin/env python3
"""Build a repeat-aware SWE-Bench-CL reliability report.

The report is intentionally based on canonical per-task rows instead of ad hoc
summary files. It is meant to answer whether a retrieval tier beats its paired
suppressed control without hiding regressions behind aggregate averages.
"""
from __future__ import annotations

import argparse
import json
import statistics
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]


def read_json(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text())


def read_jsonl(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    rows = []
    for line in path.read_text(errors="ignore").splitlines():
        if line.strip():
            rows.append(json.loads(line))
    return rows


def mean(values: list[float]) -> float | None:
    return statistics.fmean(values) if values else None


def stdev(values: list[float]) -> float | None:
    return statistics.stdev(values) if len(values) >= 2 else None


def fmt(value: float | int | None, digits: int = 1) -> str:
    if value is None:
        return "n/a"
    if isinstance(value, int):
        return str(value)
    return f"{value:.{digits}f}"


def tier_rows(rows: list[dict[str, Any]], tier: str) -> list[dict[str, Any]]:
    return [row for row in rows if row.get("tier") == tier]


def solve_rate(rows: list[dict[str, Any]]) -> float | None:
    if not rows:
        return None
    return sum(1 for row in rows if row.get("resolved")) / len(rows)


def numeric(rows: list[dict[str, Any]], key: str) -> list[float]:
    values = []
    for row in rows:
        value = row.get(key)
        if isinstance(value, (int, float)):
            values.append(float(value))
    return values


def summarize(rows: list[dict[str, Any]], tier: str) -> dict[str, Any]:
    selected = tier_rows(rows, tier)
    return {
        "tier": tier,
        "attempts": len(selected),
        "resolved": sum(1 for row in selected if row.get("resolved")),
        "solve_rate": solve_rate(selected),
        "avg_input_tokens": mean(numeric(selected, "input_tokens")),
        "sd_input_tokens": stdev(numeric(selected, "input_tokens")),
        "avg_output_tokens": mean(numeric(selected, "output_tokens")),
        "avg_reasoning_tokens": mean(numeric(selected, "reasoning_output_tokens")),
        "avg_wall_s": mean(numeric(selected, "wall_s")),
        "sd_wall_s": stdev(numeric(selected, "wall_s")),
        "avg_non_retrieval_calls": mean(numeric(selected, "non_retrieval_tool_call_total")),
        "prefetch_card_attempts": sum(1 for row in selected if (row.get("skill_prefetch_cards") or 0) > 0),
        "memory_pull_attempts": sum(1 for row in selected if (row.get("lesson_retrieve_calls") or 0) > 0),
        "skill_pull_attempts": sum(1 for row in selected if (row.get("skill_retrieve_calls") or 0) > 0),
        "failures": dict(Counter(str(row.get("failure_class") or "unknown") for row in selected)),
    }


def paired_rows(rows: list[dict[str, Any]], active: str, control: str) -> list[tuple[dict[str, Any], dict[str, Any]]]:
    by_key: dict[tuple[Any, Any], dict[str, Any]] = {}
    for row in rows:
        if row.get("tier") == control:
            by_key[(row.get("repeat_index"), row.get("instance_id"))] = row
    pairs = []
    for row in rows:
        if row.get("tier") != active:
            continue
        other = by_key.get((row.get("repeat_index"), row.get("instance_id")))
        if other:
            pairs.append((row, other))
    return pairs


def compare(rows: list[dict[str, Any]], active: str, control: str) -> dict[str, Any]:
    pairs = paired_rows(rows, active, control)
    active_wins = []
    regressions = []
    both_solved = []
    both_failed = []
    input_deltas = []
    wall_deltas = []
    call_deltas = []
    for active_row, control_row in pairs:
        a = bool(active_row.get("resolved"))
        c = bool(control_row.get("resolved"))
        if a and not c:
            active_wins.append(active_row["instance_id"])
        elif c and not a:
            regressions.append(active_row["instance_id"])
        elif a and c:
            both_solved.append(active_row["instance_id"])
        else:
            both_failed.append(active_row["instance_id"])
        for key, target in [
            ("input_tokens", input_deltas),
            ("wall_s", wall_deltas),
            ("non_retrieval_tool_call_total", call_deltas),
        ]:
            av = active_row.get(key)
            cv = control_row.get(key)
            if isinstance(av, (int, float)) and isinstance(cv, (int, float)):
                target.append(float(av) - float(cv))
    return {
        "active": active,
        "control": control,
        "paired_attempts": len(pairs),
        "active_unique_solves": sorted(active_wins),
        "regressions": sorted(regressions),
        "both_solved": sorted(both_solved),
        "both_failed": sorted(both_failed),
        "net_solve_delta": len(active_wins) - len(regressions),
        "avg_input_token_delta": mean(input_deltas),
        "avg_wall_s_delta": mean(wall_deltas),
        "avg_non_retrieval_call_delta": mean(call_deltas),
    }


def task_matrix(rows: list[dict[str, Any]], tiers: list[str]) -> list[dict[str, Any]]:
    grouped: dict[str, dict[str, list[dict[str, Any]]]] = defaultdict(lambda: defaultdict(list))
    for row in rows:
        if row.get("tier") in tiers:
            grouped[str(row.get("instance_id"))][str(row.get("tier"))].append(row)
    matrix = []
    for iid, by_tier in sorted(grouped.items()):
        item: dict[str, Any] = {"instance_id": iid}
        for tier in tiers:
            attempts = by_tier.get(tier, [])
            item[f"{tier}_solves"] = sum(1 for row in attempts if row.get("resolved"))
            item[f"{tier}_attempts"] = len(attempts)
        matrix.append(item)
    return matrix


def render_report(manifest: dict[str, Any], rows: list[dict[str, Any]], comparisons: list[dict[str, str]]) -> str:
    tiers = sorted({str(row.get("tier")) for row in rows})
    summaries = [summarize(rows, tier) for tier in tiers]
    lines = [
        f"# SWE-Bench-CL Reliability Report: {manifest.get('run_prefix')}",
        "",
        f"- Dataset: `{manifest.get('dataset')}`",
        f"- Model: `{manifest.get('model')}`",
        f"- Reasoning: `{manifest.get('reasoning')}`",
        f"- Repeats: {manifest.get('repeats', 1)}",
        f"- Canonical rows: {len(rows)}",
        "",
        "## Tier Summary",
        "",
        "| tier | attempts | resolved | solve rate | avg input tok | sd input tok | avg wall s | sd wall s | avg non-retrieval calls | prefetch hits | memory pulls | skill pulls | failures |",
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |",
    ]
    for row in summaries:
        lines.append(
            "| {tier} | {attempts} | {resolved} | {solve_rate} | {avg_input_tokens} | {sd_input_tokens} | {avg_wall_s} | {sd_wall_s} | {avg_non_retrieval_calls} | {prefetch_card_attempts} | {memory_pull_attempts} | {skill_pull_attempts} | `{failures}` |".format(
                tier=row["tier"],
                attempts=row["attempts"],
                resolved=row["resolved"],
                solve_rate=fmt(row["solve_rate"], 3),
                avg_input_tokens=fmt(row["avg_input_tokens"], 0),
                sd_input_tokens=fmt(row["sd_input_tokens"], 0),
                avg_wall_s=fmt(row["avg_wall_s"]),
                sd_wall_s=fmt(row["sd_wall_s"]),
                avg_non_retrieval_calls=fmt(row["avg_non_retrieval_calls"]),
                prefetch_card_attempts=row["prefetch_card_attempts"],
                memory_pull_attempts=row["memory_pull_attempts"],
                skill_pull_attempts=row["skill_pull_attempts"],
                failures=json.dumps(row["failures"], sort_keys=True),
            )
        )

    if comparisons:
        lines.extend(["", "## Paired Comparisons", ""])
        for spec in comparisons:
            comp = compare(rows, spec["active"], spec["control"])
            lines.extend(
                [
                    f"### {spec['active']} vs {spec['control']}",
                    "",
                    f"- Paired attempts: {comp['paired_attempts']}",
                    f"- Net solve delta: {comp['net_solve_delta']}",
                    f"- Active-only solves: `{json.dumps(comp['active_unique_solves'])}`",
                    f"- Regressions: `{json.dumps(comp['regressions'])}`",
                    f"- Avg input-token delta: {fmt(comp['avg_input_token_delta'], 0)}",
                    f"- Avg wall-second delta: {fmt(comp['avg_wall_s_delta'])}",
                    f"- Avg non-retrieval-call delta: {fmt(comp['avg_non_retrieval_call_delta'])}",
                    "",
                ]
            )

    lines.extend(
        [
            "## Per-Task Solve Matrix",
            "",
            "| instance | " + " | ".join(tiers) + " |",
            "| --- | " + " | ".join("---:" for _ in tiers) + " |",
        ]
    )
    for row in task_matrix(rows, tiers):
        cells = []
        for tier in tiers:
            cells.append(f"{row.get(f'{tier}_solves', 0)}/{row.get(f'{tier}_attempts', 0)}")
        lines.append(f"| {row['instance_id']} | " + " | ".join(cells) + " |")

    lines.extend(
        [
            "",
            "## Claimability Gate",
            "",
            "- Use active-vs-suppressed comparisons as the primary causal signal.",
            "- A public claim should have zero or explicitly bounded regressions against cold and suppressed controls.",
            "- Solve-rate, token, and wall-time claims should report repeat count, task list, model, reasoning effort, and evaluator version.",
            "- If standard deviations are large relative to deltas, treat the result as inconclusive and increase repeats before tuning retrieval.",
        ]
    )
    return "\n".join(lines) + "\n"


def parse_comparison(value: str) -> dict[str, str]:
    if ":" not in value:
        raise argparse.ArgumentTypeError("comparison must be active:control")
    active, control = value.split(":", 1)
    if not active or not control:
        raise argparse.ArgumentTypeError("comparison must be active:control")
    return {"active": active, "control": control}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--canonical", type=Path)
    parser.add_argument(
        "--compare",
        action="append",
        type=parse_comparison,
        default=[],
        help="Paired comparison as active:control. May be passed multiple times.",
    )
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()

    manifest = read_json(args.manifest)
    canonical = args.canonical or (args.manifest.parent / "canonical_results.jsonl")
    rows = read_jsonl(canonical)
    if not rows:
        raise SystemExit(f"no canonical rows found: {canonical}")
    comparisons = args.compare
    if not comparisons:
        available = {row.get("tier") for row in rows}
        default_pairs = [
            ("memory", "memory_suppressed"),
            ("skill", "skill_suppressed"),
            ("skill_prefetch", "skill_prefetch_suppressed"),
            ("memory_skill", "memory_skill_suppressed"),
        ]
        comparisons = [
            {"active": active, "control": control}
            for active, control in default_pairs
            if active in available and control in available
        ]
    report = render_report(manifest, rows, comparisons)
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(report)
        print(args.out)
    else:
        print(report)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
