#!/usr/bin/env python3
"""Report skill retrieval quality from Fieldcraft eval traces.

This is a retrieval-only evaluator. It deliberately does not score whether an
agent can solve a downstream task; it scores whether the retrieval layer returns
the expected compact skill card under a small top-k budget, and estimates the
context avoided compared with front-loading the skill corpus.
"""
from __future__ import annotations

import argparse
import json
import statistics
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any


def read_jsonl(path: Path) -> list[dict[str, Any]]:
    rows = []
    if not path.exists():
        return rows
    for line in path.read_text(errors="ignore").splitlines():
        if line.strip():
            rows.append(json.loads(line))
    return rows


def merge_query_metadata(traces: list[dict[str, Any]], queries_path: Path | None) -> list[dict[str, Any]]:
    if not queries_path:
        return traces
    queries = {row.get("id"): row for row in read_jsonl(queries_path)}
    merged = []
    for trace in traces:
        query = queries.get(trace.get("id")) or {}
        row = dict(trace)
        for key in ["label_source", "review_status", "should_abstain", "expected", "acceptable", "query"]:
            if key in query:
                row[key] = query[key]
        merged.append(row)
    return merged


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


def wilson(successes: int, total: int, z: float = 1.96) -> tuple[float, float] | None:
    if total <= 0:
        return None
    p = successes / total
    denom = 1 + z * z / total
    center = (p + z * z / (2 * total)) / denom
    margin = z * ((p * (1 - p) / total + z * z / (4 * total * total)) ** 0.5) / denom
    return max(0.0, center - margin), min(1.0, center + margin)


def fmt_ci(successes: int, total: int) -> str:
    interval = wilson(successes, total)
    if interval is None:
        return "n/a"
    lo, hi = interval
    return f"{successes}/{total} [{lo:.3f}, {hi:.3f}]"


def token_estimate(text: str) -> int:
    # Coarse but stable enough for context-budget comparisons.
    return max(1, round(len(text) / 4))


def corpus_stats(corpus: Path) -> dict[str, Any]:
    rows = read_jsonl(corpus)
    body_chars = []
    card_chars = []
    for row in rows:
        body_chars.append(len(str(row.get("body") or "")))
        compact = "\n".join(
            str(row.get(key) or "")
            for key in ["skill_name", "description", "triggers"]
        )
        card_chars.append(len(compact))
    total_body_chars = sum(body_chars)
    return {
        "skill_count": len(rows),
        "total_body_chars": total_body_chars,
        "total_body_tokens_est": token_estimate("x" * total_body_chars) if total_body_chars else 0,
        "median_body_chars": median([float(v) for v in body_chars]),
        "avg_body_chars": mean([float(v) for v in body_chars]),
        "avg_compact_chars": mean([float(v) for v in card_chars]),
    }


def expected_names(row: dict[str, Any]) -> set[str]:
    names = set()
    for key in ["expected", "acceptable"]:
        value = row.get(key) or []
        if isinstance(value, str):
            value = [value]
        for item in value:
            if str(item).strip():
                names.add(str(item).strip())
    return names


def normalize_skill_name(name: str) -> str:
    normalized = name.strip()
    if ":" in normalized:
        normalized = normalized.split(":", 1)[1]
    return normalized.lower()


def names_match(expected: str, returned: str, normalized: bool) -> bool:
    if normalized:
        return normalize_skill_name(expected) == normalize_skill_name(returned)
    return expected == returned


def rank_of_expected(row: dict[str, Any], normalized: bool = False) -> int | None:
    expected = expected_names(row)
    if not expected:
        return None
    for idx, card in enumerate(row.get("cards") or [], 1):
        got = str(card.get("skill_name") or "")
        if any(names_match(exp, got, normalized) for exp in expected):
            return idx
    return None


def is_relevant(row: dict[str, Any], skill_name: str, normalized: bool = False) -> bool:
    expected = expected_names(row)
    return any(names_match(exp, skill_name, normalized) for exp in expected)


def retrieved_names(row: dict[str, Any], k: int) -> list[str]:
    return [str(card.get("skill_name") or "") for card in (row.get("cards") or [])[:k]]


def score_rows(rows: list[dict[str, Any]], normalized: bool = False) -> dict[str, Any]:
    labeled = [row for row in rows if expected_names(row)]
    abstain = [row for row in rows if row.get("should_abstain")]
    non_abstain = [row for row in rows if not row.get("should_abstain")]
    ranks = [rank_of_expected(row, normalized=normalized) for row in labeled]
    present_ranks = [rank for rank in ranks if rank is not None]
    top1 = sum(1 for rank in ranks if rank == 1)
    top2 = sum(1 for rank in ranks if rank is not None and rank <= 2)
    top3 = sum(1 for rank in ranks if rank is not None and rank <= 3)
    reciprocal = [1.0 / rank if rank is not None else 0.0 for rank in ranks]
    false_positive_abstain = sum(1 for row in abstain if row.get("cards"))
    missed_non_abstain = sum(1 for row in non_abstain if not row.get("cards"))
    activation_rows = [row for row in rows if row.get("cards")]
    activation_top1_correct = sum(
        1
        for row in activation_rows
        if retrieved_names(row, 1) and is_relevant(row, retrieved_names(row, 1)[0], normalized=normalized)
    )
    surfaced_total = 0
    surfaced_relevant = 0
    precision_at_k_values = []
    for row in rows:
        cards = row.get("cards") or []
        if not cards:
            continue
        relevant_for_row = 0
        for card in cards:
            surfaced_total += 1
            if is_relevant(row, str(card.get("skill_name") or ""), normalized=normalized):
                surfaced_relevant += 1
                relevant_for_row += 1
        precision_at_k_values.append(relevant_for_row / len(cards))
    raw_returned = sum(1 for row in rows if row.get("raw_skill_body_returned"))
    avg_cards = mean([float(len(row.get("cards") or [])) for row in rows])
    returned_chars = []
    for row in rows:
        total = 0
        for card in row.get("cards") or []:
            total += len(json.dumps(card, sort_keys=True))
        returned_chars.append(float(total))
    return {
        "total": len(rows),
        "labeled": len(labeled),
        "abstain": len(abstain),
        "top1": top1,
        "top2": top2,
        "top3": top3,
        "top1_recall": top1 / len(labeled) if labeled else None,
        "top2_recall": top2 / len(labeled) if labeled else None,
        "top3_recall": top3 / len(labeled) if labeled else None,
        "mrr": mean(reciprocal),
        "retrieval_precision_at_1": top1 / len(non_abstain) if non_abstain else None,
        "activation_precision_at_1": activation_top1_correct / len(activation_rows) if activation_rows else None,
        "activation_top1_correct": activation_top1_correct,
        "activation_total": len(activation_rows),
        "surfaced_card_precision": surfaced_relevant / surfaced_total if surfaced_total else None,
        "surfaced_relevant": surfaced_relevant,
        "surfaced_total": surfaced_total,
        "macro_precision_at_returned_k": mean(precision_at_k_values),
        "false_positive_abstain": false_positive_abstain,
        "missed_non_abstain": missed_non_abstain,
        "raw_skill_body_returned": raw_returned,
        "avg_cards_returned": avg_cards,
        "avg_returned_chars": mean(returned_chars),
        "avg_returned_tokens_est": mean([float(token_estimate("x" * int(v))) for v in returned_chars]),
        "found_rank_counts": dict(Counter(str(rank or "miss") for rank in ranks)),
        "median_found_rank": median([float(rank) for rank in present_ranks]),
    }


def top1_miss_rows(rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    misses = []
    for row in rows:
        expected = sorted(expected_names(row))
        if not expected:
            continue
        rank = rank_of_expected(row, normalized=False)
        if rank == 1:
            continue
        misses.append(
            {
                "id": row.get("id"),
                "rank": rank,
                "expected": expected,
                "top": retrieved_names(row, 3),
                "query": " ".join(str(row.get("query") or "").split())[:260],
            }
        )
    return misses


def confusion_rows(rows: list[dict[str, Any]], limit: int, normalized: bool = False) -> list[dict[str, Any]]:
    misses = []
    for row in rows:
        expected = sorted(expected_names(row))
        if not expected:
            continue
        rank = rank_of_expected(row, normalized=normalized)
        if rank is not None and rank <= limit:
            continue
        top = retrieved_names(row, limit)
        misses.append(
            {
                "id": row.get("id"),
                "expected": expected,
                "top": top,
                "query": " ".join(str(row.get("query") or "").split())[:260],
            }
        )
    return misses


def top_wrong_pairs(rows: list[dict[str, Any]], limit: int, normalized: bool = False) -> list[tuple[tuple[str, str], int]]:
    pairs = Counter()
    for row in rows:
        expected = expected_names(row)
        if not expected:
            continue
        rank = rank_of_expected(row, normalized=normalized)
        if rank is not None and rank <= limit:
            continue
        first = retrieved_names(row, 1)
        if first:
            for exp in expected:
                pairs[(exp, first[0])] += 1
    return pairs.most_common(12)


def category_breakdown(rows: list[dict[str, Any]]) -> dict[str, dict[str, int]]:
    breakdown: dict[str, dict[str, int]] = defaultdict(lambda: {"total": 0, "top1": 0, "top3": 0})
    for row in rows:
        expected = sorted(expected_names(row))
        if not expected:
            continue
        prefix = expected[0].split("-", 1)[0]
        breakdown[prefix]["total"] += 1
        rank = rank_of_expected(row, normalized=True)
        if rank == 1:
            breakdown[prefix]["top1"] += 1
        if rank is not None and rank <= 3:
            breakdown[prefix]["top3"] += 1
    return dict(sorted(breakdown.items()))


def label_source_breakdown(rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    grouped: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for row in rows:
        grouped[str(row.get("label_source") or "unknown")].append(row)
    output = []
    for label_source, selected in sorted(grouped.items()):
        stats = score_rows(selected, normalized=False)
        namespace_stats = score_rows(selected, normalized=True)
        output.append(
            {
                "label_source": label_source,
                "total": stats["total"],
                "labeled": stats["labeled"],
                "abstain": stats["abstain"],
                "top1": stats["top1_recall"],
                "top3": stats["top3_recall"],
                "namespace_top1": namespace_stats["top1_recall"],
                "namespace_top3": namespace_stats["top3_recall"],
                "mrr": stats["mrr"],
                "activation_precision_at_1": stats["activation_precision_at_1"],
                "surfaced_card_precision": stats["surfaced_card_precision"],
                "macro_precision_at_returned_k": stats["macro_precision_at_returned_k"],
            }
        )
    return output


def render(args: argparse.Namespace, rows: list[dict[str, Any]]) -> str:
    stats = score_rows(rows, normalized=False)
    namespace_stats = score_rows(rows, normalized=True)
    quality_rows = [
        row
        for row in rows
        if row.get("label_source") in {"manual_independent_holdout", "manual_near_miss_holdout"}
    ]
    quality_stats = score_rows(quality_rows, normalized=False)
    smoke_rows = [row for row in rows if row.get("label_source") == "deterministic_corpus_holdout"]
    smoke_stats = score_rows(smoke_rows, normalized=False)
    corpus = corpus_stats(args.corpus) if args.corpus else {}
    avg_returned = float(stats.get("avg_returned_chars") or 0.0)
    avoided_ratio = None
    if corpus.get("total_body_chars") and avg_returned:
        avoided_ratio = float(corpus["total_body_chars"]) / avg_returned
    lines = [
        f"# Skill Retrieval Reliability Report: {args.name}",
        "",
        "## What This Tests",
        "",
        "This is a retrieval-only test. It checks whether Fieldcraft can select the expected compact skill card from a larger corpus under a top-3 budget. It does not claim downstream task-solving lift.",
        "",
        "## Dataset",
        "",
        f"- Trace file: `{args.traces}`",
        f"- Query count: {stats['total']}",
        f"- Labeled queries: {stats['labeled']}",
        f"- Abstention queries: {stats['abstain']}",
        f"- Retrieval-quality queries: {quality_stats['labeled']} hand-authored positives",
        f"- Smoke/plumbing positives: {smoke_stats['labeled']} corpus-derived positives",
    ]
    if corpus:
        lines.extend(
            [
                f"- Corpus: `{args.corpus}`",
                f"- Corpus skills: {corpus['skill_count']}",
                f"- Estimated full raw corpus context: {fmt(corpus['total_body_tokens_est'], 0)} tokens",
                f"- Average returned compact context: {fmt(stats['avg_returned_tokens_est'], 0)} tokens",
                f"- Estimated context reduction vs raw front-load: {fmt(avoided_ratio, 1)}x",
            ]
        )
    lines.extend(
        [
            "",
            "## Headline Retrieval Quality",
            "",
            "This section excludes deterministic corpus-description queries. It uses only hand-authored independent and near-miss positives.",
            "",
            "| metric | value |",
            "| --- | ---: |",
            f"| hand-authored top-1 | {fmt(quality_stats['top1_recall'], 3)} ({fmt_ci(quality_stats['top1'], quality_stats['labeled'])}) |",
            f"| hand-authored top-3 | {fmt(quality_stats['top3_recall'], 3)} ({fmt_ci(quality_stats['top3'], quality_stats['labeled'])}) |",
            f"| hand-authored MRR | {fmt(quality_stats['mrr'], 3)} |",
            f"| hand-authored top-card precision | {fmt(quality_stats['activation_precision_at_1'], 3)} ({fmt_ci(quality_stats['activation_top1_correct'], quality_stats['activation_total'])}) |",
            f"| hand-authored surfaced-card precision lower bound | {fmt(quality_stats['surfaced_card_precision'], 3)} ({fmt_ci(quality_stats['surfaced_relevant'], quality_stats['surfaced_total'])}) |",
            "",
            "## Retrieval Metrics",
            "",
            "Aggregate metrics include deterministic corpus-derived smoke queries and should not be used as the retrieval-quality headline.",
            "",
            "| metric | value |",
            "| --- | ---: |",
            f"| top-1 recall | {fmt(stats['top1_recall'], 3)} |",
            f"| top-2 recall | {fmt(stats['top2_recall'], 3)} |",
            f"| top-3 recall | {fmt(stats['top3_recall'], 3)} |",
            f"| MRR | {fmt(stats['mrr'], 3)} |",
            f"| precision@1 over non-abstain queries | {fmt(stats['retrieval_precision_at_1'], 3)} |",
            f"| activation precision@1 | {fmt(stats['activation_precision_at_1'], 3)} |",
            f"| surfaced-card precision | {fmt(stats['surfaced_card_precision'], 3)} |",
            f"| macro precision@returned-k | {fmt(stats['macro_precision_at_returned_k'], 3)} |",
            f"| namespace-normalized diagnostic top-1 | {fmt(namespace_stats['top1_recall'], 3)} |",
            f"| namespace-normalized diagnostic top-3 | {fmt(namespace_stats['top3_recall'], 3)} |",
            f"| average cards returned | {fmt(stats['avg_cards_returned'], 2)} |",
            f"| abstention false positives | {stats['false_positive_abstain']} |",
            f"| non-abstain misses with no returned card | {stats['missed_non_abstain']} |",
            f"| raw skill bodies returned | {stats['raw_skill_body_returned']} |",
        ]
    )
    lines.extend(
        [
            "",
            "## Rank Distribution",
            "",
            f"`{json.dumps(stats['found_rank_counts'], sort_keys=True)}`",
            "",
            "## Label Source Breakdown",
            "",
            "| label source | rows | labeled | abstain | top-1 | top-3 | activation P@1 | surfaced precision | macro P@returned-k | namespace top-1 | namespace top-3 | MRR |",
            "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
        ]
    )
    for row in label_source_breakdown(rows):
        lines.append(
                "| {label_source} | {total} | {labeled} | {abstain} | {top1} | {top3} | {activation_precision_at_1} | {surfaced_card_precision} | {macro_precision_at_returned_k} | {strict_top1} | {strict_top3} | {mrr} |".format(
                label_source=row["label_source"],
                total=row["total"],
                labeled=row["labeled"],
                abstain=row["abstain"],
                top1=fmt(row["top1"], 3),
                top3=fmt(row["top3"], 3),
                activation_precision_at_1=fmt(row["activation_precision_at_1"], 3),
                surfaced_card_precision=fmt(row["surfaced_card_precision"], 3),
                macro_precision_at_returned_k=fmt(row["macro_precision_at_returned_k"], 3),
                strict_top1=fmt(row["namespace_top1"], 3),
                strict_top3=fmt(row["namespace_top3"], 3),
                mrr=fmt(row["mrr"], 3),
            )
        )
    lines.extend(
        [
            "",
            "## Top Confusions",
            "",
            "| expected | returned top-1 | count |",
            "| --- | --- | ---: |",
        ]
    )
    for (expected, got), count in top_wrong_pairs(rows, args.limit, normalized=False):
        lines.append(f"| `{expected}` | `{got}` | {count} |")
    misses = confusion_rows(rows, args.limit, normalized=False)
    lines.extend(
        [
            "",
            f"## Sample Misses (first {args.max_misses})",
            "",
            "| id | expected | top returned | query excerpt |",
            "| --- | --- | --- | --- |",
        ]
    )
    for row in misses[: args.max_misses]:
        lines.append(
            f"| `{row['id']}` | `{', '.join(row['expected'])}` | `{', '.join(row['top']) or 'none'}` | {row['query']} |"
        )
    top1_misses = top1_miss_rows(rows)
    lines.extend(
        [
            "",
            f"## Top-1 Misses Recovered By Top-3 (first {args.max_misses})",
            "",
            "| id | rank | expected | top returned | query excerpt |",
            "| --- | ---: | --- | --- | --- |",
        ]
    )
    for row in top1_misses[: args.max_misses]:
        if row["rank"] is None or row["rank"] > args.limit:
            continue
        lines.append(
            f"| `{row['id']}` | {row['rank']} | `{', '.join(row['expected'])}` | `{', '.join(row['top'])}` | {row['query']} |"
        )
    breakdown = category_breakdown(rows)
    if breakdown:
        lines.extend(["", "## Prefix Breakdown", "", "| prefix | total | top-1 | top-3 |", "| --- | ---: | ---: | ---: |"])
        for prefix, values in breakdown.items():
            if values["total"] >= args.min_prefix_total:
                lines.append(
                    f"| `{prefix}` | {values['total']} | {values['top1']} | {values['top3']} |"
                )
    lines.extend(
        [
            "",
            "## Validity Notes",
            "",
            "- Strong evidence: compact retrieval can avoid raw corpus front-loading when hand-authored top-k routing remains high and raw bodies returned is zero.",
            "- Weakness: deterministic holdout queries echo skill descriptions; they are smoke/plumbing tests, not retrieval-quality evidence.",
            "- Precision framing: top-card precision is claimable with current labels; surfaced-card precision is only a lower bound until every runner-up card is independently labeled relevant, partial, or irrelevant.",
            "- Required next step: run the same report on redacted real task/session queries and use human/Claude labels for expected or acceptable skills.",
            "- Public benchmark claims should report this retrieval-only result separately from downstream SWE task-solving lift.",
        ]
    )
    return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--traces", type=Path, required=True)
    parser.add_argument("--queries", type=Path)
    parser.add_argument("--corpus", type=Path)
    parser.add_argument("--name", default="skill-retrieval")
    parser.add_argument("--limit", type=int, default=3)
    parser.add_argument("--max-misses", type=int, default=20)
    parser.add_argument("--min-prefix-total", type=int, default=4)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    rows = merge_query_metadata(read_jsonl(args.traces), args.queries)
    if not rows:
        raise SystemExit(f"no trace rows: {args.traces}")
    text = render(args, rows)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(text)
    print(args.out)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
