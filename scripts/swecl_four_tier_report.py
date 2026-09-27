#!/usr/bin/env python3
"""Summarize cold, memory, skill, and memory+skill SWE-Bench-CL runs."""
from __future__ import annotations

import argparse
import json
import statistics
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def read_json(path: Path) -> dict:
    return json.loads(path.read_text())


def read_jsonl(path: Path) -> list[dict]:
    if not path.exists():
        return []
    rows = []
    for line in path.read_text(errors="ignore").splitlines():
        if line.strip():
            rows.append(json.loads(line))
    return rows


def mean(values: list[float]) -> float | None:
    return statistics.fmean(values) if values else None


def median(values: list[float]) -> float | None:
    return statistics.median(values) if values else None


def fmt(value: float | int | None, digits: int = 1) -> str:
    if value is None:
        return "n/a"
    if isinstance(value, int):
        return str(value)
    return f"{value:.{digits}f}"


def load_local_eval(report_dir: Path, run_id: str) -> dict:
    candidates = [
        report_dir / run_id / "summary.json",
        report_dir / f"{run_id}.summary.json",
        report_dir / f"{run_id}/results.json",
        report_dir / f"codex-unknown.{run_id}.json",
        ROOT / f"codex-unknown.{run_id}.json",
    ]
    for path in candidates:
        if path.exists():
            return read_json(path)
    return {}


def solved_count(eval_summary: dict) -> tuple[int | None, int | None]:
    if not eval_summary:
        return None, None
    if isinstance(eval_summary.get("resolved_instances"), int):
        total = eval_summary.get("submitted_instances") or eval_summary.get("total_instances")
        return int(eval_summary["resolved_instances"]), int(total) if isinstance(total, int) else None
    for resolved_key in ["resolved", "num_resolved", "strict_resolved", "passed"]:
        if isinstance(eval_summary.get(resolved_key), int):
            total = eval_summary.get("total") or eval_summary.get("total_submitted") or eval_summary.get("n")
            return int(eval_summary[resolved_key]), int(total) if isinstance(total, int) else None
    return None, None


def summarize_tier(tier: str, spec: dict, eval_report_dir: Path | None) -> dict:
    telemetry = read_jsonl(Path(spec["telemetry_path"]))
    predictions = read_jsonl(Path(spec["prediction_path"]))
    eval_summary = load_local_eval(eval_report_dir, spec["run_id"]) if eval_report_dir else {}
    solved, eval_total = solved_count(eval_summary)
    error_instances = eval_summary.get("error_instances")
    empty_patch_instances = eval_summary.get("empty_patch_instances")
    unresolved_instances = eval_summary.get("unresolved_instances")
    wall = [float(row["wall_s"]) for row in telemetry if isinstance(row.get("wall_s"), (int, float))]
    input_tokens = [int(row["input_tokens"]) for row in telemetry if isinstance(row.get("input_tokens"), int)]
    output_tokens = [int(row["output_tokens"]) for row in telemetry if isinstance(row.get("output_tokens"), int)]
    reasoning_tokens = [
        int(row["reasoning_output_tokens"])
        for row in telemetry
        if isinstance(row.get("reasoning_output_tokens"), int)
    ]
    non_retrieval_tool_calls = [
        int(row["non_retrieval_tool_call_total"])
        for row in telemetry
        if isinstance(row.get("non_retrieval_tool_call_total"), int)
    ]
    memory_hits = sum(
        1
        for row in telemetry
        if row.get("memory_has_context")
        or int((row.get("tool_call_counts") or {}).get("lesson_retrieve") or 0) > 0
        or int((row.get("tool_call_counts") or {}).get("lesson_reveal") or 0) > 0
    )
    skill_hits = sum(
        1
        for row in telemetry
        if row.get("skill_names")
        or int((row.get("tool_call_counts") or {}).get("skill_retrieve") or 0) > 0
    )
    nonempty_patches = sum(1 for row in predictions if (row.get("model_patch") or "").strip())
    return {
        "tier": tier,
        "run_id": spec["run_id"],
        "attempted": len(predictions),
        "telemetry_rows": len(telemetry),
        "nonempty_patches": nonempty_patches,
        "resolved": solved,
        "eval_total": eval_total,
        "unresolved": int(unresolved_instances) if isinstance(unresolved_instances, int) else None,
        "errors": int(error_instances) if isinstance(error_instances, int) else None,
        "empty_patches_eval": int(empty_patch_instances) if isinstance(empty_patch_instances, int) else None,
        "memory_hits": memory_hits,
        "skill_hits": skill_hits,
        "avg_wall_s": mean(wall),
        "median_wall_s": median(wall),
        "avg_input_tokens": mean(input_tokens),
        "avg_output_tokens": mean(output_tokens),
        "avg_reasoning_tokens": mean(reasoning_tokens),
        "avg_non_retrieval_tool_calls": mean(non_retrieval_tool_calls),
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument(
        "--eval-report-dir",
        type=Path,
        help="Optional directory containing per-run local/official eval summaries.",
    )
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()

    manifest = read_json(args.manifest)
    rows = [
        summarize_tier(tier, spec, args.eval_report_dir)
        for tier, spec in manifest["tiers"].items()
    ]

    memory_counts = manifest.get("memory_counts")
    if not memory_counts:
        memory_counts = {
            tier: spec.get("final_memory_counts", {})
            for tier, spec in manifest.get("tiers", {}).items()
            if "final_memory_counts" in spec
        }
    memory_status = manifest.get("memory_status")
    if memory_status is None:
        any_memories = any(
            count
            for counts in memory_counts.values()
            for count in (counts.values() if isinstance(counts, dict) else [])
        ) if isinstance(memory_counts, dict) else False
        memory_status = "populated" if any_memories else "empty"

    lines = [
        f"# SWE-Bench-CL Four-Tier Report: {manifest['run_prefix']}",
        "",
        f"- Dataset: `{manifest['dataset']}`",
        f"- Model: `{manifest['model']}`",
        f"- Reasoning: `{manifest['reasoning']}`",
        f"- Task count: {len(manifest['instance_ids'])}",
        f"- Memory status: `{memory_status}`",
        f"- Memory counts: `{json.dumps(memory_counts, sort_keys=True)}`",
        "",
        "| tier | attempted | evaluated | resolved | errors | memory hits | skill hits | avg wall s | median wall s | avg input tok | avg output tok | avg reasoning tok | avg non-retrieval tool calls | nonempty patches |",
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for row in rows:
        # Prefer actually graded instances (resolved + unresolved) over the
        # evaluator's submitted/non-empty denominator, which can include
        # infrastructure/parser errors that shouldn't be counted as task
        # failures.
        evaluated = (
            None
            if row["resolved"] is None
            else row["resolved"] + (row["unresolved"] or 0)
        )
        if evaluated == 0:
            evaluated = row["eval_total"]
        evaluated_text = "n/a" if evaluated is None else str(evaluated)
        resolved = "n/a" if row["resolved"] is None else f"{row['resolved']}/{evaluated or row['attempted']}"
        lines.append(
            "| {tier} | {attempted} | {evaluated} | {resolved} | {errors} | {memory_hits} | {skill_hits} | {avg_wall_s} | {median_wall_s} | {avg_input_tokens} | {avg_output_tokens} | {avg_reasoning_tokens} | {avg_non_retrieval_tool_calls} | {nonempty_patches} |".format(
                tier=row["tier"],
                attempted=row["attempted"],
                evaluated=evaluated_text,
                resolved=resolved,
                errors=fmt(row["errors"]),
                memory_hits=row["memory_hits"],
                skill_hits=row["skill_hits"],
                avg_wall_s=fmt(row["avg_wall_s"]),
                median_wall_s=fmt(row["median_wall_s"]),
                avg_input_tokens=fmt(row["avg_input_tokens"], 0),
                avg_output_tokens=fmt(row["avg_output_tokens"], 0),
                avg_reasoning_tokens=fmt(row["avg_reasoning_tokens"], 0),
                avg_non_retrieval_tool_calls=fmt(row["avg_non_retrieval_tool_calls"], 1),
                nonempty_patches=row["nonempty_patches"],
            )
        )

    lines.extend(
        [
            "",
            "## Claimability Notes",
            "",
            "- A memory tier is not claimable unless `memory hits` is non-zero and the memory corpus was frozen before the run.",
            "- Solve-rate claims require local or official SWE-Bench evaluation summaries passed through `--eval-report-dir`.",
            "- Speed/token comparisons are descriptive until the same frozen task set has all four completed tiers.",
        ]
    )

    text = "\n".join(lines) + "\n"
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(text)
        print(args.out)
    else:
        print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
