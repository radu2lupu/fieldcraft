#!/usr/bin/env python3
"""Evaluate whether retrieved skill previews support correct reveal decisions.

This is the measurement rung between retrieval quality and task completion. It
does not judge code patches. It asks: under a simple runtime reveal policy, does
the system reveal useful skill cards for positive tasks and abstain for no-skill
tasks, while keeping revealed context small?
"""
from __future__ import annotations

import argparse
import json
import math
import statistics
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any


SELECTED_POLICY = "top1_any"


def read_jsonl(path: Path) -> list[dict[str, Any]]:
    rows = []
    for line in path.read_text(errors="ignore").splitlines():
        if line.strip():
            rows.append(json.loads(line))
    return rows


def normalize(name: str) -> str:
    value = str(name).strip().lower()
    if ":" in value:
        value = value.split(":", 1)[1]
    return value


def expected_names(row: dict[str, Any]) -> set[str]:
    out: set[str] = set()
    for key in ["expected", "acceptable"]:
        value = row.get(key) or []
        if isinstance(value, str):
            value = [value]
        for item in value:
            if str(item).strip():
                out.add(str(item).strip())
    return out


def is_relevant(row: dict[str, Any], skill_name: str) -> bool:
    expected = expected_names(row)
    return any(exp == skill_name or normalize(exp) == normalize(skill_name) for exp in expected)


def merge_metadata(traces: list[dict[str, Any]], queries: list[dict[str, Any]]) -> list[dict[str, Any]]:
    query_by_id = {row.get("id"): row for row in queries}
    merged = []
    for trace in traces:
        row = dict(trace)
        query = query_by_id.get(trace.get("id")) or {}
        for key in ["label_source", "review_status", "should_abstain", "expected", "acceptable", "query"]:
            if key in query:
                row[key] = query[key]
        merged.append(row)
    return merged


def token_estimate(value: Any) -> int:
    return max(1, round(len(json.dumps(value, sort_keys=True)) / 4))


def wilson(successes: int, total: int, z: float = 1.96) -> tuple[float, float] | None:
    if total <= 0:
        return None
    p = successes / total
    denom = 1 + z * z / total
    center = (p + z * z / (2 * total)) / denom
    margin = z * math.sqrt(p * (1 - p) / total + z * z / (4 * total * total)) / denom
    return max(0.0, center - margin), min(1.0, center + margin)


def fmt_rate(successes: int, total: int) -> str:
    if total <= 0:
        return "n/a"
    lo, hi = wilson(successes, total) or (0.0, 0.0)
    return f"{successes / total:.3f} ({successes}/{total} [{lo:.3f}, {hi:.3f}])"


def should_reveal(card: dict[str, Any], policy: str) -> bool:
    confidence = str(card.get("confidence") or "").lower()
    decision = str(card.get("routing_decision") or "").lower()
    if policy == "top1_any":
        return True
    if policy == "high_or_use":
        return confidence == "high" or decision == "use_this_skill_card"
    if policy == "use_only":
        return decision == "use_this_skill_card"
    raise ValueError(f"unknown policy: {policy}")


def evaluate_policy(rows: list[dict[str, Any]], policy: str) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    details = []
    for row in rows:
        cards = row.get("cards") or []
        top = cards[0] if cards else None
        revealed = bool(top and should_reveal(top, policy))
        relevant_reveal = bool(revealed and is_relevant(row, str(top.get("skill_name") or "")))
        positive = bool(expected_names(row))
        abstain = bool(row.get("should_abstain"))
        expected_in_top3 = any(is_relevant(row, str(card.get("skill_name") or "")) for card in cards[:3])
        details.append(
            {
                "id": row.get("id"),
                "label_source": row.get("label_source") or "unknown",
                "positive": positive,
                "should_abstain": abstain,
                "revealed": revealed,
                "relevant_reveal": relevant_reveal,
                "expected_in_top3": expected_in_top3,
                "top_skill": top.get("skill_name") if top else None,
                "top_confidence": top.get("confidence") if top else "none",
                "top_decision": top.get("routing_decision") if top else "none",
                "expected": sorted(expected_names(row)),
                "preview_tokens": token_estimate(cards[:3]),
                "revealed_tokens": token_estimate(top) if revealed else 0,
                "query_excerpt": " ".join(str(row.get("query") or "").split())[:220],
            }
        )
    positives = [d for d in details if d["positive"]]
    abstains = [d for d in details if d["should_abstain"]]
    revealed = [d for d in details if d["revealed"]]
    relevant_reveals = [d for d in details if d["relevant_reveal"]]
    positive_relevant_revealed = [d for d in positives if d["relevant_reveal"]]
    false_reveals = [d for d in revealed if not d["relevant_reveal"]]
    missed_reveals = [d for d in positives if not d["relevant_reveal"]]
    top3_recoverable_misses = [d for d in missed_reveals if d["expected_in_top3"]]
    stats = {
        "policy": policy,
        "rows": len(details),
        "positive_rows": len(positives),
        "abstain_rows": len(abstains),
        "reveals": len(revealed),
        "relevant_reveals": len(relevant_reveals),
        "false_reveals": len(false_reveals),
        "missed_positive_reveals": len(missed_reveals),
        "top3_recoverable_misses": len(top3_recoverable_misses),
        "reveal_precision": len(relevant_reveals) / len(revealed) if revealed else None,
        "reveal_recall": len(positive_relevant_revealed) / len(positives) if positives else None,
        "abstain_accuracy": sum(1 for d in abstains if not d["revealed"]) / len(abstains) if abstains else None,
        "false_reveal_rate_on_abstain": sum(1 for d in abstains if d["revealed"]) / len(abstains) if abstains else None,
        "avg_preview_tokens": statistics.fmean(d["preview_tokens"] for d in details) if details else 0,
        "avg_revealed_tokens": statistics.fmean(d["revealed_tokens"] for d in details) if details else 0,
    }
    return stats, details


def render(policy_stats: list[dict[str, Any]], selected_details: list[dict[str, Any]], rows: list[dict[str, Any]]) -> str:
    lines = [
        "# Retrieval Usefulness Eval",
        "",
        "This eval measures the progressive-reveal decision layer: given retrieved skill previews, should the runtime reveal the top card or abstain before any code editing?",
        "",
        "It does not score patch correctness. It isolates whether retrieval output is actionable enough to support the next rung: patch-planning quality.",
        "",
        "## Policy Comparison",
        "",
        "| policy | reveals | reveal precision | reveal recall | abstain accuracy | false reveals | avg preview tokens | avg revealed tokens |",
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for stats in policy_stats:
        precision = "n/a" if stats["reveal_precision"] is None else f"{stats['reveal_precision']:.3f}"
        recall = "n/a" if stats["reveal_recall"] is None else f"{stats['reveal_recall']:.3f}"
        abstain = "n/a" if stats["abstain_accuracy"] is None else f"{stats['abstain_accuracy']:.3f}"
        lines.append(
            f"| {stats['policy']} | {stats['reveals']} | {precision} | {recall} | {abstain} | {stats['false_reveals']} | {stats['avg_preview_tokens']:.0f} | {stats['avg_revealed_tokens']:.0f} |"
        )

    chosen_policy = SELECTED_POLICY
    chosen = next(s for s in policy_stats if s["policy"] == chosen_policy)
    lines.extend(
        [
            "",
            "## Selected Runtime Policy",
            "",
            f"`{chosen_policy}`: reveal the top card whenever Fieldcraft returns one. This relies on retrieval-time abstention to suppress no-skill tasks instead of adding a second confidence gate.",
            "",
            f"- Reveal precision: {fmt_rate(chosen['relevant_reveals'], chosen['reveals'])}",
            f"- Reveal recall over positive tasks: {fmt_rate(chosen['positive_rows'] - chosen['missed_positive_reveals'], chosen['positive_rows'])}",
            f"- Abstain accuracy on no-skill tasks: {fmt_rate(round((chosen['abstain_accuracy'] or 0) * chosen['abstain_rows']), chosen['abstain_rows'])}",
            f"- False reveals: {chosen['false_reveals']}",
            f"- Top-3 recoverable misses: {chosen['top3_recoverable_misses']}",
            "",
            "## Label Source Breakdown",
            "",
            "| source | rows | positives | abstain | reveal precision | reveal recall |",
            "| --- | ---: | ---: | ---: | ---: | ---: |",
        ]
    )
    grouped: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for row in rows:
        grouped[str(row.get("label_source") or "unknown")].append(row)
    for source, source_rows in sorted(grouped.items()):
        stats, _ = evaluate_policy(source_rows, chosen_policy)
        precision = "n/a" if stats["reveal_precision"] is None else f"{stats['reveal_precision']:.3f}"
        recall = "n/a" if stats["reveal_recall"] is None else f"{stats['reveal_recall']:.3f}"
        lines.append(
            f"| {source} | {stats['rows']} | {stats['positive_rows']} | {stats['abstain_rows']} | {precision} | {recall} |"
        )
    failures = [d for d in selected_details if d["revealed"] and not d["relevant_reveal"]]
    misses = [d for d in selected_details if d["positive"] and not d["relevant_reveal"]]
    lines.extend(["", "## False Reveals", "", "| id | top skill | expected | query |", "| --- | --- | --- | --- |"])
    for d in failures[:20]:
        lines.append(f"| `{d['id']}` | `{d['top_skill']}` | `{', '.join(d['expected'])}` | {d['query_excerpt']} |")
    lines.extend(["", "## Missed Positive Reveals", "", "| id | top skill | expected | recoverable top-3 | query |", "| --- | --- | --- | ---: | --- |"])
    for d in misses[:20]:
        lines.append(
            f"| `{d['id']}` | `{d['top_skill']}` | `{', '.join(d['expected'])}` | {d['expected_in_top3']} | {d['query_excerpt']} |"
        )
    lines.extend(
        [
            "",
            "## Validity Notes",
            "",
            "- This is a policy/usefulness eval, not a retrieval benchmark and not a code-solving benchmark.",
            "- A positive hit means the revealed top card matches the frozen expected/acceptable label.",
            "- A false reveal means the runtime would spend reveal context on a card not labeled useful for that task.",
            "- The next rung should ask whether revealed guidance improves file localization and patch planning.",
            "",
        ]
    )
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--traces", type=Path, required=True)
    parser.add_argument("--queries", type=Path, required=True)
    parser.add_argument("--out-dir", type=Path, required=True)
    args = parser.parse_args()

    rows = merge_metadata(read_jsonl(args.traces), read_jsonl(args.queries))
    policies = ["top1_any", "high_or_use", "use_only"]
    policy_stats = []
    selected_details = []
    for policy in policies:
        stats, details = evaluate_policy(rows, policy)
        policy_stats.append(stats)
        if policy == SELECTED_POLICY:
            selected_details = details

    args.out_dir.mkdir(parents=True, exist_ok=True)
    (args.out_dir / "retrieval_usefulness_trace.json").write_text(
        json.dumps({"policies": policy_stats, "details": selected_details}, indent=2, sort_keys=True) + "\n"
    )
    (args.out_dir / "retrieval_usefulness_report.md").write_text(render(policy_stats, selected_details, rows) + "\n")
    print(args.out_dir / "retrieval_usefulness_report.md")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
