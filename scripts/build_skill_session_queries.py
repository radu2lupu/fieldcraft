#!/usr/bin/env python3
"""Mine local Codex/Claude sessions for explicit skill mentions and redact them.

This creates candidate replay queries where the expected skill was mentioned in a
real session, but the query text no longer contains the exact skill name.
Manual review is still expected before treating labels as ground truth.
"""
from __future__ import annotations

import argparse
import json
import os
import re
from pathlib import Path


def load_skill_names(corpus: Path) -> list[str]:
    names = []
    with corpus.open() as f:
        for line in f:
            if not line.strip():
                continue
            record = json.loads(line)
            name = record.get("skill_name")
            if name:
                names.append(name)
    return sorted(set(names), key=len, reverse=True)


SKIP_FILE_NAMES = {
    "session_index.jsonl",
    "sessions-index.json",
    "transcription-history.jsonl",
    "skills-curated-cache.json",
    "installed_plugins.json",
}

SKIP_PATH_PARTS = {
    "cache",
    "vendor_imports",
    "skills",
    "ambient-suggestions",
    "pocketdex",
    "plugins",
}


def should_skip_path(path: Path) -> bool:
    if path.name in SKIP_FILE_NAMES:
        return True
    parts = set(path.parts)
    if parts.intersection(SKIP_PATH_PARTS):
        return True
    return False


def iter_session_texts(source_dirs: list[Path]):
    for source in source_dirs:
        expanded = Path(os.path.expanduser(str(source)))
        if not expanded.exists():
            continue
        for path in expanded.rglob("*"):
            if not (path.is_file() and path.suffix.lower() in {".json", ".jsonl", ".md", ".txt"}):
                continue
            if should_skip_path(path):
                continue
            try:
                text = path.read_text(errors="ignore")
            except OSError:
                continue
            user_text = extract_user_text_from_jsonl(text) if path.suffix.lower() == ".jsonl" else None
            yield path, user_text or text


def extract_user_text_from_jsonl(text: str) -> str:
    messages: list[str] = []
    for line in text.splitlines():
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        payload = record.get("payload") if isinstance(record, dict) else None
        if not isinstance(payload, dict):
            continue
        if payload.get("type") == "user_message" and isinstance(payload.get("message"), str):
            messages.append(payload["message"])
            continue
        if payload.get("type") == "message" and payload.get("role") == "user":
            content = payload.get("content")
            if isinstance(content, list):
                for item in content:
                    if isinstance(item, dict) and item.get("type") in {"input_text", "text"}:
                        value = item.get("text")
                        if isinstance(value, str):
                            messages.append(value)
            elif isinstance(content, str):
                messages.append(content)
    return "\n".join(messages)


def extract_snippet(text: str, skill: str) -> str | None:
    pattern = re.compile(re.escape(skill), re.IGNORECASE)
    match = pattern.search(text)
    if not match:
        return None
    start = max(0, match.start() - 350)
    end = min(len(text), match.end() + 350)
    snippet = text[start:end]
    snippet = re.sub(r"\s+", " ", snippet).strip()
    lowered = snippet.lower()
    if any(marker in lowered for marker in ("skill.md", "available skills", "repository guidelines", "<skills_instructions", "file: /users/")):
        return None
    redacted = pattern.sub("the relevant skill", snippet)
    redacted = re.sub(r"\$[a-zA-Z0-9_-]+", "the relevant skill", redacted)
    return redacted[:700]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--corpus", type=Path, required=True)
    parser.add_argument("--source-dir", action="append", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--max", type=int, default=200)
    args = parser.parse_args()

    skills = load_skill_names(args.corpus)
    records = []
    seen = set()
    for path, text in iter_session_texts(args.source_dir):
        lower = text.lower()
        for skill in skills:
            if skill.lower() not in lower:
                continue
            snippet = extract_snippet(text, skill)
            if not snippet:
                continue
            key = (skill, snippet)
            if key in seen:
                continue
            seen.add(key)
            records.append({
                "id": f"session-{len(records)+1:04d}",
                "query": snippet,
                "expected": [skill],
                "source": str(path),
                "needs_review": True,
            })
            if len(records) >= args.max:
                break
        if len(records) >= args.max:
            break

    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("w") as f:
        for record in records:
            f.write(json.dumps(record, ensure_ascii=False) + "\n")
    print(f"wrote {len(records)} redacted session query candidates to {args.output}")


if __name__ == "__main__":
    main()
