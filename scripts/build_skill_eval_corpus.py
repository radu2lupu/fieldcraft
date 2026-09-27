#!/usr/bin/env python3
"""Build a skill-eval corpus JSONL from real SKILL.md directories.

Example:
  scripts/build_skill_eval_corpus.py \
    --source-dir ~/.codex/skills \
    --source-dir ~/.codex/plugins/cache/openai-curated \
    --output eval/skills/corpus/skills.jsonl \
    --max 500
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
from pathlib import Path


def parse_skill_md(path: Path) -> dict | None:
    text = path.read_text(errors="ignore")
    if not text.startswith("---\n"):
        return None
    rest = text[4:]
    end = rest.find("\n---")
    if end < 0:
        return None
    frontmatter = rest[:end]
    body = rest[end + 4 :].lstrip("\n")
    name = None
    description = None
    lines = frontmatter.splitlines()
    for idx, line in enumerate(lines):
        stripped = line.strip()
        if stripped.startswith("name:"):
            name = stripped.split(":", 1)[1].strip().strip('"\'')
        elif stripped.startswith("description:"):
            value = stripped.split(":", 1)[1].strip().strip('"\'')
            if value and value not in {"|", ">"}:
                description = value
            else:
                parts = []
                for next_line in lines[idx + 1 :]:
                    if next_line and not next_line.startswith((" ", "\t")):
                        break
                    cleaned = next_line.strip()
                    if cleaned:
                        parts.append(cleaned)
                if parts:
                    description = " ".join(parts)
    if not name or not description:
        return None
    body = body.strip()
    if is_placeholder_skill(name, description, body):
        return None
    return {
        "skill_name": name,
        "description": description,
        "body": body,
        "triggers": extract_triggers(body),
    }




def extract_triggers(body: str) -> list[str]:
    triggers: list[str] = []
    in_section = False
    for line in body.splitlines():
        stripped = line.strip()
        if re.match(r"^#{1,3}\s+(triggers?|when to use|activation|use when)", stripped, re.I):
            in_section = True
            continue
        if in_section and re.match(r"^#{1,3}\s+", stripped):
            break
        if in_section:
            cleaned = stripped.lstrip("- ").strip()
            if cleaned:
                triggers.append(cleaned[:240])
        if len(triggers) >= 12:
            break
    return triggers

def is_placeholder_skill(name: str, description: str, body: str) -> bool:
    if name in {"skill-creator", "skill-installer", "plugin-creator"}:
        return False
    haystack = f"{name} {description} {body}".lower()
    placeholder_markers = [
        "replace with description",
        "insert instructions below",
        "todo",
        "template-skill",
        "your skill name",
        "describe when this skill should be used",
    ]
    if any(marker in haystack for marker in placeholder_markers):
        return True
    meaningful = [line.strip() for line in body.splitlines() if line.strip() and not line.strip().startswith("#")]
    return len(" ".join(meaningful)) < 80

def infer_category(path: Path) -> str:
    parts = [p.lower() for p in path.parts]
    for candidate in [
        "cloudflare",
        "github",
        "gmail",
        "google-calendar",
        "google-drive",
        "canva",
        "stripe",
        "build-ios-apps",
        "build-macos-apps",
    ]:
        if candidate in parts:
            return candidate
    parent = path.parent.name
    return parent if parent else "uncategorized"


def iter_skill_files(source_dirs: list[Path]):
    seen = set()
    for source in source_dirs:
        expanded = Path(os.path.expanduser(str(source)))
        if not expanded.exists():
            continue
        for skill_md in expanded.rglob("SKILL.md"):
            real = skill_md.resolve()
            if real in seen:
                continue
            seen.add(real)
            yield real



def default_source_dirs() -> list[Path]:
    home = Path.home()
    return [
        home / ".codex" / "skills",
        home / ".codex" / "skills" / ".system",
        home / ".codex" / "plugins" / "cache" / "openai-curated",
        home / ".codex" / "plugins" / "cache" / "openai-primary-runtime",
        home / ".codex" / "plugins" / "cache" / "openai-bundled",
        home / "Code" / "utils" / "Skills",
        Path("eval/skills/registries"),
    ]

def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source-dir", action="append", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--max", type=int, default=500)
    args = parser.parse_args()

    source_dirs = args.source_dir or default_source_dirs()
    records = []
    seen_hashes = set()
    for skill_md in iter_skill_files(source_dirs):
        parsed = parse_skill_md(skill_md)
        if not parsed:
            continue
        package_root = skill_md.parent
        body_hash = hashlib.sha256(skill_md.read_bytes()).hexdigest()
        if body_hash in seen_hashes:
            continue
        seen_hashes.add(body_hash)
        parsed["source_uri"] = str(package_root)
        parsed["category"] = infer_category(skill_md)
        parsed["hash"] = body_hash
        records.append(parsed)
        if len(records) >= args.max:
            break

    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("w") as f:
        for record in records:
            f.write(json.dumps(record, ensure_ascii=False) + "\n")
    print(f"wrote {len(records)} skill records to {args.output}")


if __name__ == "__main__":
    main()
