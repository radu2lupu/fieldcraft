#!/usr/bin/env python3
"""Create a public-facing retrieval benchmark report with baselines.

The report is intentionally conservative: it separates hand-authored routing
queries from corpus-derived smoke queries, compares Fieldcraft against lexical
baselines, and states which claims are supported by the data.
"""
from __future__ import annotations

import argparse
import json
import math
import random
import re
import statistics
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any


TOKEN_RE = re.compile(r"[a-zA-Z0-9_+#.-]+")
STOP = {
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "has", "have", "i",
    "in", "into", "is", "it", "of", "on", "or", "that", "the", "this", "to", "use",
    "when", "with", "work", "user", "task", "need", "help", "real", "project", "pick",
    "most", "relevant", "compact", "workflow", "card",
}


def read_jsonl(path: Path) -> list[dict[str, Any]]:
    rows = []
    for line in path.read_text(errors="ignore").splitlines():
        if line.strip():
            rows.append(json.loads(line))
    return rows


def tokenize(text: str) -> list[str]:
    return [t.lower() for t in TOKEN_RE.findall(text) if len(t) > 1 and t.lower() not in STOP]


def normalize_name(name: str) -> str:
    value = str(name).strip().lower()
    if ":" in value:
        value = value.split(":", 1)[1]
    return value


def expected_set(row: dict[str, Any]) -> set[str]:
    out: set[str] = set()
    for key in ["expected", "acceptable"]:
        value = row.get(key) or []
        if isinstance(value, str):
            value = [value]
        for item in value:
            item = str(item).strip()
            if item:
                out.add(item)
    return out


def is_hit(expected: set[str], got: str) -> bool:
    return any(exp == got or normalize_name(exp) == normalize_name(got) for exp in expected)


def rank_of(expected: set[str], names: list[str]) -> int | None:
    for idx, name in enumerate(names, 1):
        if is_hit(expected, name):
            return idx
    return None


def wilson(successes: int, total: int, z: float = 1.96) -> tuple[float, float]:
    if total <= 0:
        return 0.0, 0.0
    p = successes / total
    denom = 1 + z * z / total
    center = (p + z * z / (2 * total)) / denom
    margin = z * math.sqrt(p * (1 - p) / total + z * z / (4 * total * total)) / denom
    return max(0.0, center - margin), min(1.0, center + margin)


def fmt_ci(successes: int, total: int) -> str:
    lo, hi = wilson(successes, total)
    return f"{successes}/{total} [{lo:.3f}, {hi:.3f}]"


def token_estimate_chars(chars: int) -> int:
    return max(1, round(chars / 4))


def compact_text(skill: dict[str, Any]) -> str:
    parts = [
        skill.get("skill_name") or "",
        skill.get("category") or "",
        skill.get("description") or "",
        " ".join(skill.get("triggers") or []),
    ]
    return " ".join(str(p) for p in parts if p)


def body_text(skill: dict[str, Any]) -> str:
    return f"{compact_text(skill)} {skill.get('body') or ''}"


class LexicalIndex:
    def __init__(self, skills: list[dict[str, Any]], *, include_body: bool):
        self.skills = skills
        self.docs = [tokenize(body_text(s) if include_body else compact_text(s)) for s in skills]
        self.doc_counts = [Counter(doc) for doc in self.docs]
        df: Counter[str] = Counter()
        for doc in self.docs:
            df.update(set(doc))
        total = len(self.docs)
        self.idf = {term: math.log(1 + (total - freq + 0.5) / (freq + 0.5)) for term, freq in df.items()}
        self.avg_len = statistics.fmean([len(doc) for doc in self.docs]) if self.docs else 1.0

    def search(self, query: str, k: int) -> list[str]:
        q_terms = tokenize(query)
        scores = []
        for idx, counts in enumerate(self.doc_counts):
            score = 0.0
            doc_len = max(1, len(self.docs[idx]))
            for term in q_terms:
                tf = counts.get(term, 0)
                if not tf:
                    continue
                idf = self.idf.get(term, 0.0)
                denom = tf + 1.2 * (1 - 0.75 + 0.75 * doc_len / self.avg_len)
                score += idf * (tf * 2.2 / denom)
            if score > 0:
                scores.append((score, idx))
        scores.sort(key=lambda item: (-item[0], self.skills[item[1]].get("skill_name") or ""))
        return [str(self.skills[idx].get("skill_name") or "") for _, idx in scores[:k]]


def score_system(rows: list[dict[str, Any]], predictions: dict[str, list[str]], *, include_abstain: bool = True) -> dict[str, Any]:
    selected = rows if include_abstain else [row for row in rows if expected_set(row)]
    labeled = [row for row in selected if expected_set(row)]
    abstain = [row for row in selected if row.get("should_abstain")]
    ranks = [rank_of(expected_set(row), predictions.get(row["id"], [])) for row in labeled]
    top1 = sum(1 for r in ranks if r == 1)
    top3 = sum(1 for r in ranks if r is not None and r <= 3)
    mrr = statistics.fmean([1.0 / r if r else 0.0 for r in ranks]) if ranks else None
    false_positive_abstain = sum(1 for row in abstain if predictions.get(row["id"], []))
    return {
        "rows": len(selected),
        "labeled": len(labeled),
        "abstain": len(abstain),
        "top1": top1,
        "top3": top3,
        "top1_rate": top1 / len(labeled) if labeled else None,
        "top3_rate": top3 / len(labeled) if labeled else None,
        "mrr": mrr,
        "abstain_false_positives": false_positive_abstain,
    }


def fieldcraft_predictions(traces: list[dict[str, Any]]) -> dict[str, list[str]]:
    return {
        str(row.get("id")): [str(card.get("skill_name") or "") for card in (row.get("cards") or [])[:3]]
        for row in traces
    }


def baseline_predictions(rows: list[dict[str, Any]], index: LexicalIndex, *, k: int, abstain: bool) -> dict[str, list[str]]:
    predictions = {}
    for row in rows:
        if abstain and row.get("should_abstain"):
            predictions[row["id"]] = []
        else:
            predictions[row["id"]] = index.search(str(row.get("query") or ""), k)
    return predictions


def random_predictions(rows: list[dict[str, Any]], skills: list[dict[str, Any]], *, k: int) -> dict[str, list[str]]:
    rng = random.Random(20260509)
    names = [str(skill.get("skill_name") or "") for skill in skills]
    return {
        row["id"]: rng.sample(names, min(k, len(names)))
        for row in rows
    }


def subset(rows: list[dict[str, Any]], sources: set[str]) -> list[dict[str, Any]]:
    return [row for row in rows if row.get("label_source") in sources]


def fmt_rate(stats: dict[str, Any], key: str) -> str:
    value = stats.get(f"{key}_rate")
    if value is None:
        return "n/a"
    return f"{value:.3f} ({fmt_ci(int(stats[key]), int(stats['labeled']))})"


def render(args: argparse.Namespace, queries: list[dict[str, Any]], traces: list[dict[str, Any]], skills: list[dict[str, Any]]) -> str:
    fc = fieldcraft_predictions(traces)
    compact_bm25 = baseline_predictions(queries, LexicalIndex(skills, include_body=False), k=3, abstain=False)
    raw_bm25 = baseline_predictions(queries, LexicalIndex(skills, include_body=True), k=3, abstain=False)
    random_base = random_predictions(queries, skills, k=3)

    hand = subset(queries, {"manual_independent_holdout", "manual_near_miss_holdout"})
    all_rows = queries
    deterministic = subset(queries, {"deterministic_corpus_holdout"})
    abstain = [row for row in queries if row.get("should_abstain")]

    systems = {
        "Fieldcraft compact retrieval": fc,
        "BM25 over compact cards": compact_bm25,
        "BM25 over raw skill bodies": raw_bm25,
        "Random eligible skill": random_base,
    }
    hand_scores = {name: score_system(hand, pred, include_abstain=False) for name, pred in systems.items()}
    all_scores = {name: score_system(all_rows, pred) for name, pred in systems.items()}
    det_scores = {name: score_system(deterministic, pred, include_abstain=False) for name, pred in systems.items()}

    raw_chars = sum(len(str(skill.get("body") or "")) for skill in skills)
    compact_returned_chars = []
    for row in traces:
        compact_returned_chars.append(sum(len(json.dumps(card, sort_keys=True)) for card in (row.get("cards") or [])))
    avg_returned_tokens = statistics.fmean(token_estimate_chars(v) for v in compact_returned_chars)
    raw_tokens = token_estimate_chars(raw_chars)
    reduction = raw_tokens / avg_returned_tokens if avg_returned_tokens else 0.0
    raw_body_violations = sum(1 for row in traces if row.get("raw_skill_body_returned"))
    avg_cards = statistics.fmean(len(row.get("cards") or []) for row in traces)

    fc_hand = hand_scores["Fieldcraft compact retrieval"]
    bm25_hand = hand_scores["BM25 over compact cards"]
    claim = (
        f"On a frozen 574-skill corpus and 53 hand-authored positive routing queries, "
        f"Fieldcraft selected a pre-labeled expected or acceptable skill at rank 1 for {fc_hand['top1']}/{fc_hand['labeled']} "
        f"queries ({fc_hand['top1_rate']:.1%}, 95% Wilson CI {wilson(fc_hand['top1'], fc_hand['labeled'])[0]:.1%}-{wilson(fc_hand['top1'], fc_hand['labeled'])[1]:.1%}) "
        f"and by rank 3 for {fc_hand['top3']}/{fc_hand['labeled']} queries. "
        f"In the same benchmark, compact-card BM25 selected a labeled skill at rank 1 for "
        f"{bm25_hand['top1']}/{bm25_hand['labeled']} queries."
    )

    lines = [
        f"# Public Skill Retrieval Benchmark: {args.name}",
        "",
        "## Claim Supported By This Run",
        "",
        claim,
        "",
        "This is a retrieval-only benchmark. It does not claim downstream task-solving lift, solve-rate improvement, or end-to-end coding-agent speedup.",
        "",
        "Publication status: preliminary and publishable only with the exact corpus, query JSONL, trace JSONL, report script, commit hash, and runtime details. The hand-authored labels were produced during Fieldcraft development, not by an independent third-party benchmark provider.",
        "",
        "## Dataset",
        "",
        f"- Corpus: `{args.corpus}`",
        f"- Corpus skills: {len(skills)}",
        f"- Queries: `{args.queries}`",
        f"- Total queries: {len(queries)}",
        f"- Hand-authored positive routing queries: {len(hand)}",
        f"- Corpus-derived smoke positives: {len(deterministic)}",
        f"- Abstention / no-skill queries: {len(abstain)}",
        f"- Fieldcraft traces: `{args.traces}`",
        "- Label semantics: each positive query has a frozen `expected` list and optional `acceptable` aliases. A hit means the returned skill name matches either list after namespace normalization.",
        "- Near-miss definition: positive queries intentionally worded near another plausible skill boundary, used to test whether the intended routing target remains recoverable.",
        "- Query authorship limitation: the hand-authored labels were created as part of this project, so unconscious benchmark/system alignment remains possible.",
        "",
        "## Headline Comparison",
        "",
        "Headline rows use only hand-authored independent and near-miss positive queries.",
        "",
        "| system | top-1 | top-3 | MRR |",
        "| --- | ---: | ---: | ---: |",
    ]
    for name, stats in hand_scores.items():
        lines.append(
            f"| {name} | {fmt_rate(stats, 'top1')} | {fmt_rate(stats, 'top3')} | {stats['mrr']:.3f} |"
        )
    lines.extend(
        [
            "",
            "## Aggregate Diagnostics",
            "",
            "Aggregate rows include deterministic smoke positives and abstention queries. They are diagnostics, not the headline retrieval-quality claim, and should not be cited as retrieval quality.",
            "",
            "| system | labeled | top-1 | top-3 | abstain false positives |",
            "| --- | ---: | ---: | ---: | ---: |",
        ]
    )
    for name, stats in all_scores.items():
        lines.append(
            f"| {name} | {stats['labeled']} | {fmt_rate(stats, 'top1')} | {fmt_rate(stats, 'top3')} | {stats['abstain_false_positives']} |"
        )
    lines.extend(
        [
            "",
            "## Corpus-Derived Smoke Set",
            "",
            "| system | top-1 | top-3 |",
            "| --- | ---: | ---: |",
        ]
    )
    for name, stats in det_scores.items():
        lines.append(f"| {name} | {fmt_rate(stats, 'top1')} | {fmt_rate(stats, 'top3')} |")
    lines.extend(
        [
            "",
            "## Context Budget",
            "",
            f"- Average Fieldcraft returned compact-card context: {avg_returned_tokens:.0f} tokens",
            f"- Estimated raw skill-body corpus: {raw_tokens:,} tokens",
            f"- Naive front-load-all comparison only: the raw corpus is {reduction:.1f}x larger than the average Fieldcraft returned context. This is not a practical retrieval-system baseline.",
            f"- Average cards returned: {avg_cards:.2f}",
            f"- Raw skill bodies returned: {raw_body_violations}",
            "",
            "## Public-Claim Boundaries",
            "",
            "- Supported: compact skill retrieval can select the expected/acceptable card on this frozen retrieval benchmark while avoiding raw skill-body front-loading.",
            "- Supported: retrieval quality is measured separately from deterministic corpus-description smoke tests.",
            "- Baseline limitation: BM25 compact/raw baselines are lexical floor baselines; dense-vector and LLM-reranked baselines are not included and could narrow the gap.",
            "- Abstention comparison limitation: BM25 and random baselines have no calibrated abstention classifier in this report, so their abstention false-positive counts reflect always-return behavior.",
            "- Not supported: downstream coding solve-rate, token spend in full SWE tasks, or wall-clock speedup.",
            "- Not yet ideal: the hand-authored positive set is 53 queries; a stronger public benchmark should add hundreds more independently written real-session queries.",
            "- Publication gate: do not publish the headline number without publishing the corpus, query JSONL, trace JSONL, report script, exact commit, and model/runtime details.",
            "",
        ]
    )
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--corpus", type=Path, required=True)
    parser.add_argument("--queries", type=Path, required=True)
    parser.add_argument("--traces", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--name", default="skill-retrieval-public-v1")
    args = parser.parse_args()

    text = render(args, read_jsonl(args.queries), read_jsonl(args.traces), read_jsonl(args.corpus))
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(text + "\n")
    print(args.out)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
