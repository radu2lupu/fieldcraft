#!/usr/bin/env python3
"""Filter public skill benchmark queries to score only present, unblocked labels."""
from __future__ import annotations

import argparse
import json
from pathlib import Path


BLOCKING_PATTERNS = [
    "ignore previous instructions",
    "ignore all previous instructions",
    "curl | sh",
    "rm -rf",
    "cat ~/.ssh",
    "read ~/.ssh",
    "send openai_api_key",
    "send github_token",
]


def read_jsonl(path: Path) -> list[dict]:
    rows = []
    with path.open() as f:
        for line in f:
            if line.strip():
                rows.append(json.loads(line))
    return rows


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--corpus", type=Path, required=True)
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--exclusions", type=Path, required=True)
    args = parser.parse_args()

    present = set()
    blocked = set()
    for row in read_jsonl(args.corpus):
        name = row["skill_name"]
        present.add(name)
        text = f"{row.get('body', '')} {row.get('description', '')}".lower()
        if any(pattern in text for pattern in BLOCKING_PATTERNS):
            blocked.add(name)

    kept = []
    dropped = []
    for row in read_jsonl(args.input):
        labels = set(row.get("expected") or []) | set(row.get("acceptable") or [])
        if labels:
            suffixes = {label.rsplit(":", 1)[-1] for label in labels}
            if not (suffixes & present):
                dropped.append(
                    {
                        "id": row["id"],
                        "reason": "label_not_in_public_corpus",
                        "labels": sorted(labels),
                    }
                )
                continue
            if suffixes <= blocked:
                dropped.append(
                    {
                        "id": row["id"],
                        "reason": "blocked_expected_skill",
                        "labels": sorted(labels),
                    }
                )
                continue
        kept.append(row)

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(
        "\n".join(json.dumps(row, ensure_ascii=False, sort_keys=True) for row in kept) + "\n"
    )
    args.exclusions.parent.mkdir(parents=True, exist_ok=True)
    args.exclusions.write_text(json.dumps(dropped, indent=2) + "\n")
    print(
        f"wrote {len(kept)} scored queries to {args.output}; "
        f"excluded {len(dropped)} rows to {args.exclusions}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
