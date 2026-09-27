#!/usr/bin/env python3
"""Report SWE-Bench-CL skill ablation outcomes from summaries and manifests."""
from __future__ import annotations

import argparse
import json
from collections import defaultdict
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent


def read_jsonl(path: Path) -> list[dict]:
    rows = []
    with path.open() as f:
        for line in f:
            line = line.strip()
            if line:
                rows.append(json.loads(line))
    return rows


def load_summary(path: Path) -> dict:
    data = json.loads(path.read_text())
    resolved_ids = set(data.get("resolved_ids") or [])
    return {
        "resolved": int(data.get("resolved_instances") or 0),
        "attempted": int(data.get("total_attempted") or data.get("total_instances") or 0),
        "empty": int(data.get("empty_patch_instances") or 0),
        "error": int(data.get("error_instances") or 0),
        "resolved_ids": resolved_ids,
        "avg_wall_s": data.get("avg_wall_s") or data.get("avg_wall_seconds"),
        "avg_input_tokens": data.get("avg_input_tokens") or data.get("avg_in_tok") or data.get("avg_in_tokens"),
        "avg_output_tokens": data.get("avg_output_tokens") or data.get("avg_out_tok") or data.get("avg_out_tokens"),
    }


def rate(resolved: int, attempted: int) -> float:
    return resolved / attempted if attempted else 0.0


def fmt(x: float | None, suffix: str = "") -> str:
    if x is None:
        return "n/a"
    return f"{x:.1f}{suffix}" if isinstance(x, float) else f"{x}{suffix}"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", type=Path, default=ROOT / "benchmarks/swe-bench-cl/skill_experiment/manifest.jsonl")
    parser.add_argument("--summary", action="append", default=[], help="arm=path/to/summary.json")
    parser.add_argument("--out", type=Path, default=ROOT / "benchmarks/swe-bench-cl/skill_experiment/report.md")
    args = parser.parse_args()

    manifest = read_jsonl(args.manifest)
    by_arm = defaultdict(list)
    for row in manifest:
        by_arm[row["arm"]].append(row)

    summaries: dict[str, dict] = {}
    for spec in args.summary:
        if "=" not in spec:
            raise SystemExit(f"--summary must be arm=path, got {spec!r}")
        arm, path = spec.split("=", 1)
        summaries[arm] = load_summary(Path(path))

    lines = ["# SWE-Bench-CL Skill Ablation Report", ""]
    lines.append("## Retrieval Coverage")
    lines.append("")
    lines.append("| arm | tasks | tasks with skills | avg skill chars | top skills |")
    lines.append("| --- | ---: | ---: | ---: | --- |")
    for arm in ("cold", "memory", "skills", "memory_skills"):
        rows = by_arm.get(arm, [])
        with_skills = sum(1 for r in rows if int(r.get("skill_context_chars") or 0) > 0)
        avg_chars = sum(int(r.get("skill_context_chars") or 0) for r in rows) / len(rows) if rows else 0
        counts = defaultdict(int)
        for r in rows:
            if int(r.get("skill_context_chars") or 0) > 0:
                for name in r.get("skill_names") or []:
                    counts[name] += 1
        top = ", ".join(f"{k} ({v})" for k, v in sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))[:5])
        lines.append(f"| {arm} | {len(rows)} | {with_skills} | {avg_chars:.0f} | {top or 'none'} |")

    if summaries:
        lines.extend(["", "## Outcome Summary", ""])
        lines.append("| arm | attempted | resolved | pass rate | empty | errors | avg wall | avg input tok | avg output tok |")
        lines.append("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")
        for arm in ("cold", "memory", "skills", "memory_skills"):
            s = summaries.get(arm)
            if not s:
                continue
            lines.append(
                f"| {arm} | {s['attempted']} | {s['resolved']} | {100*rate(s['resolved'], s['attempted']):.1f}% | "
                f"{s['empty']} | {s['error']} | {fmt(s['avg_wall_s'], 's')} | {fmt(s['avg_input_tokens'])} | {fmt(s['avg_output_tokens'])} |"
            )

        base = summaries.get("cold")
        if base:
            lines.extend(["", "## Delta vs Cold", ""])
            lines.append("| arm | pass-rate delta | wall delta | input-token delta |")
            lines.append("| --- | ---: | ---: | ---: |")
            base_rate = rate(base["resolved"], base["attempted"])
            for arm in ("memory", "skills", "memory_skills"):
                s = summaries.get(arm)
                if not s:
                    continue
                pass_delta = 100 * (rate(s["resolved"], s["attempted"]) - base_rate)
                wall_delta = None if s["avg_wall_s"] is None or base["avg_wall_s"] is None else s["avg_wall_s"] - base["avg_wall_s"]
                tok_delta = None if s["avg_input_tokens"] is None or base["avg_input_tokens"] is None else s["avg_input_tokens"] - base["avg_input_tokens"]
                lines.append(f"| {arm} | {pass_delta:+.1f}pp | {fmt(wall_delta, 's')} | {fmt(tok_delta)} |")

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text("\n".join(lines) + "\n")
    print(args.out)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
