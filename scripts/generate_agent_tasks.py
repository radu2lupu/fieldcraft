#!/usr/bin/env python3
"""Generate self-contained task JSON files for the Claude Code Agent orchestrator.

Each task file contains everything a sub-agent needs: the full prompt with
repo setup instructions, the test patch, and the output directory to write
patch.diff into.

Usage:
    python3 scripts/generate_agent_tasks.py \\
      --task-ids eval/oracle_validation_v1/task_ids.json \\
      --dataset eval/patch_planning/heldout_50/dataset.jsonl \\
      --conditions-dir eval/oracle_validation_v1/conditions \\
      --out-dir eval/oracle_validation_v1/runs/v6_claude_grounded \\
      --conditions cold whole_skill oracle_section \\
      --queue-dir /tmp/oracle_v6_queue \\
      [--resume]
"""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

SCRIPT_DIR = Path(__file__).parent.resolve()
REPO_ROOT = SCRIPT_DIR.parent


AGENT_PROMPT_TEMPLATE = """\
You are a software engineer solving a SWE-Bench bug. This is an isolated task — do NOT \
query memory, total-recall, prior sessions, or any external knowledge source. \
Solve using only this prompt and what you find in the repository.

=== SETUP (run these Bash commands first) ===

WORK_DIR=/tmp/swe_{instance_id}
mkdir -p "$WORK_DIR"
git clone --quiet https://github.com/{repo}.git "$WORK_DIR/repo"
cd "$WORK_DIR/repo" && git checkout {base_commit}

Then apply this test patch (write it to $WORK_DIR/test.patch and git apply it,
then commit it so git diff later only shows your changes):

--- BEGIN TEST PATCH ---
{test_patch}
--- END TEST PATCH ---

Apply it with:
  cd "$WORK_DIR/repo"
  cat > "$WORK_DIR/test.patch" << 'ENDOFPATCH'
{test_patch}
ENDOFPATCH
  git apply "$WORK_DIR/test.patch" && git -c user.name=fc -c user.email=fc@local commit -am "apply test_patch"

=== TASK ===

Instance: {instance_id}
Repository: {repo}

Problem statement:
{problem_statement}

Failing tests that MUST pass after your fix:
{fail_to_pass_list}

Tests that must NOT regress: {pass_to_pass_count} existing tests.
{knowledge_block}
=== INSTRUCTIONS ===

1. Read the relevant source files in $WORK_DIR/repo to understand the bug.
2. Write a minimal, correct fix. Do NOT modify the failing tests.
3. Optionally run the failing tests to verify your fix works.
4. When done, run:
     cd "$WORK_DIR/repo" && git diff --no-color HEAD
   and write the complete output to:
     {output_dir}/patch.diff

Create the output directory first: mkdir -p {output_dir}

The patch.diff file must be the raw `git diff --no-color HEAD` output.
"""

KNOWLEDGE_BLOCK_TEMPLATE = """
=== DOMAIN KNOWLEDGE ===

Domain knowledge for this task (use as a guide for your investigation — read the actual \
source code to verify before applying any pattern):
{condition_content}

=== END DOMAIN KNOWLEDGE ===
"""


def load_dataset(dataset_path: Path) -> dict[str, dict]:
    instances: dict[str, dict] = {}
    with open(dataset_path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            row = json.loads(line)
            instances[row["instance_id"]] = row
    return instances


def load_condition_content(conditions_dir: Path, condition: str, instance_id: str) -> str:
    if condition == "cold":
        return ""
    p = conditions_dir / condition / f"{instance_id}.md"
    return p.read_text().strip() if p.exists() else ""


def build_agent_prompt(task: dict, condition_content: str, output_dir: Path) -> str:
    fail_to_pass = task.get("FAIL_TO_PASS", [])
    if isinstance(fail_to_pass, str):
        try:
            fail_to_pass = json.loads(fail_to_pass)
        except Exception:
            fail_to_pass = [fail_to_pass]
    fail_to_pass_list = "\n".join(f"  - {t}" for t in fail_to_pass) or "  (none listed)"

    pass_to_pass = task.get("PASS_TO_PASS", [])
    if isinstance(pass_to_pass, str):
        try:
            pass_to_pass = json.loads(pass_to_pass)
        except Exception:
            pass_to_pass = []

    knowledge_block = ""
    if condition_content:
        knowledge_block = KNOWLEDGE_BLOCK_TEMPLATE.format(condition_content=condition_content)

    test_patch = task.get("test_patch", "").strip()

    return AGENT_PROMPT_TEMPLATE.format(
        instance_id=task["instance_id"],
        repo=task["repo"],
        base_commit=task["base_commit"],
        test_patch=test_patch,
        problem_statement=task.get("problem_statement", "").strip(),
        fail_to_pass_list=fail_to_pass_list,
        pass_to_pass_count=len(pass_to_pass),
        knowledge_block=knowledge_block,
        output_dir=str(output_dir),
    )


def main() -> int:
    parser = argparse.ArgumentParser(description="Generate agent task JSON files")
    parser.add_argument("--task-ids", default="eval/oracle_validation_v1/task_ids.json")
    parser.add_argument("--dataset", default="eval/patch_planning/heldout_50/dataset.jsonl")
    parser.add_argument("--conditions-dir", default="eval/oracle_validation_v1/conditions")
    parser.add_argument("--out-dir", default="eval/oracle_validation_v1/runs/v6_claude_grounded")
    parser.add_argument("--conditions", nargs="+",
                        default=["cold", "whole_skill", "oracle_section", "oracle_rewritten", "oracle_multi"])
    parser.add_argument("--queue-dir", default="/tmp/oracle_v6_queue")
    parser.add_argument("--resume", action="store_true",
                        help="Skip tasks where patch.diff already exists in output_dir")
    args = parser.parse_args()

    def resolve(p: str) -> Path:
        path = Path(p)
        return (REPO_ROOT / path).resolve() if not path.is_absolute() else path.resolve()

    task_ids_path = resolve(args.task_ids)
    dataset_path = resolve(args.dataset)
    conditions_dir = resolve(args.conditions_dir)
    out_dir = resolve(args.out_dir)
    queue_dir = Path(args.queue_dir)
    queue_dir.mkdir(parents=True, exist_ok=True)

    task_ids: list[str] = json.loads(task_ids_path.read_text())
    dataset = load_dataset(dataset_path)

    generated = 0
    skipped = 0

    for condition in args.conditions:
        for iid in task_ids:
            task = dataset.get(iid)
            if task is None:
                print(f"WARN: {iid} not in dataset — skipping", file=sys.stderr)
                continue

            output_dir = out_dir / condition / iid
            patch_file = output_dir / "patch.diff"

            task_key = f"{condition}__{iid}"
            task_file = queue_dir / f"{task_key}.json"

            if args.resume and patch_file.exists() and patch_file.stat().st_size > 0:
                skipped += 1
                continue

            condition_content = load_condition_content(conditions_dir, condition, iid)
            prompt = build_agent_prompt(task, condition_content, output_dir)

            task_obj = {
                "key": task_key,
                "condition": condition,
                "instance_id": iid,
                "repo": task["repo"],
                "base_commit": task["base_commit"],
                "output_dir": str(output_dir),
                "patch_file": str(patch_file),
                "prompt": prompt,
            }
            task_file.write_text(json.dumps(task_obj, indent=2))
            generated += 1

    print(f"Generated {generated} task files in {queue_dir}")
    if skipped:
        print(f"Skipped {skipped} (patch.diff already exists)")

    # Print the manifest so the orchestrator knows what to run
    manifest = sorted(queue_dir.glob("*.json"))
    print(f"\nQueue manifest ({len(manifest)} tasks):")
    for f in manifest:
        print(f"  {f.name}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
