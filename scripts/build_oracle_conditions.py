#!/usr/bin/env python3
"""
Build per-task corpus for oracle_validation_v1 eval.
Writes 100 condition files (5 conditions x 20 tasks) + manifest.json.
"""

import json
import os
import statistics
import sys
from pathlib import Path

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------

REPO_ROOT = Path(__file__).parent.parent
EVAL_DIR = REPO_ROOT / "eval" / "oracle_validation_v1"
SKILLS_DIR = EVAL_DIR / "curated_skills"
ORACLES_DIR = EVAL_DIR / "oracles"
CONDITIONS_DIR = EVAL_DIR / "conditions"
TASK_IDS_FILE = EVAL_DIR / "task_ids.json"

TASK_TO_SKILLS = {
    "django__django-13363": ["S1"],
    "django__django-14373": ["S1"],
    "django__django-15741": ["S1"],
    "django__django-16569": ["S6", "S7"],
    "pytest-dev__pytest-5262": ["S2"],
    "pytest-dev__pytest-5809": [],  # no skill, trivial fix
    "pytest-dev__pytest-7571": ["S2"],
    "pytest-dev__pytest-8399": ["S2"],
    "scikit-learn__scikit-learn-10908": ["S3"],
    "scikit-learn__scikit-learn-12682": ["S3"],
    "scikit-learn__scikit-learn-14053": ["S6", "S3"],
    "scikit-learn__scikit-learn-14087": ["S3"],
    "matplotlib__matplotlib-23299": ["S4"],
    "matplotlib__matplotlib-24149": ["S6"],
    "matplotlib__matplotlib-25287": ["S4"],
    "matplotlib__matplotlib-25311": ["S9"],
    "sphinx-doc__sphinx-8056": ["S8"],
    "sphinx-doc__sphinx-8475": ["S5"],
    "sphinx-doc__sphinx-8721": ["S5"],
    "sphinx-doc__sphinx-9367": ["S6"],
}

# Map skill id prefix to filename
SKILL_FILE_MAP = {
    "S1": "S1_django_datetime_formatting.md",
    "S2": "S2_pytest_fixture_capture_internals.md",
    "S3": "S3_sklearn_estimator_lifecycle.md",
    "S4": "S4_matplotlib_rcparam_backend.md",
    "S5": "S5_sphinx_extension_events.md",
    "S6": "S6_guard_clause_degenerate_inputs.md",
    "S7": "S7_django_formsets_add_fields.md",
    "S8": "S8_sphinx_napoleon_multiparam.md",
    "S9": "S9_matplotlib_draggable_pickle.md",
}

CONDITIONS = ["cold", "whole_skill", "oracle_section", "oracle_rewritten", "oracle_multi"]


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def load_skill(skill_id: str) -> str:
    fname = SKILL_FILE_MAP[skill_id]
    return (SKILLS_DIR / fname).read_text(encoding="utf-8")


def load_oracle(instance_id: str, variant: str) -> tuple[str, str]:
    """Return (content, status). variant in {section, rewritten, multi}."""
    path = ORACLES_DIR / instance_id / f"{variant}.md"
    if not path.exists():
        return "", "missing_source"
    content = path.read_text(encoding="utf-8")
    return content, "ok"


def write_file(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    task_ids: list[str] = json.loads(TASK_IDS_FILE.read_text())

    files_meta: dict[str, dict] = {}
    char_totals: dict[str, int] = {c: 0 for c in CONDITIONS}
    char_lists: dict[str, list[int]] = {c: [] for c in CONDITIONS}
    total_written = 0

    for instance_id in task_ids:
        skills = TASK_TO_SKILLS.get(instance_id, [])
        files_meta[instance_id] = {}

        # ---- cold --------------------------------------------------------
        cond = "cold"
        out_path = CONDITIONS_DIR / cond / f"{instance_id}.md"
        write_file(out_path, "")
        files_meta[instance_id][cond] = {
            "path": str(out_path.relative_to(REPO_ROOT)),
            "char_count": 0,
            "status": "empty",
        }
        char_totals[cond] += 0
        char_lists[cond].append(0)
        total_written += 1

        # ---- whole_skill -------------------------------------------------
        cond = "whole_skill"
        if not skills:
            content = "(no skill matched - this task has trivial fix; testing condition equivalent to cold)"
        else:
            parts = [load_skill(s) for s in skills]
            content = "\n\n---\n\n".join(parts)
        out_path = CONDITIONS_DIR / cond / f"{instance_id}.md"
        write_file(out_path, content)
        status = "ok" if skills else "empty"
        n = len(content)
        files_meta[instance_id][cond] = {
            "path": str(out_path.relative_to(REPO_ROOT)),
            "char_count": n,
            "status": status,
        }
        char_totals[cond] += n
        char_lists[cond].append(n)
        total_written += 1

        # ---- oracle variants ---------------------------------------------
        for variant, cond in [
            ("section", "oracle_section"),
            ("rewritten", "oracle_rewritten"),
            ("multi", "oracle_multi"),
        ]:
            content, status = load_oracle(instance_id, variant)
            out_path = CONDITIONS_DIR / cond / f"{instance_id}.md"
            write_file(out_path, content)
            n = len(content)
            files_meta[instance_id][cond] = {
                "path": str(out_path.relative_to(REPO_ROOT)),
                "char_count": n,
                "status": status,
            }
            char_totals[cond] += n
            char_lists[cond].append(n)
            total_written += 1

    # ---- manifest --------------------------------------------------------
    manifest = {
        "tasks": task_ids,
        "conditions": CONDITIONS,
        "task_to_skills": TASK_TO_SKILLS,
        "files": files_meta,
        "stats": {
            "total_files_written": total_written,
            "char_count_by_condition": char_totals,
            "median_chars_by_condition": {
                c: statistics.median(v) for c, v in char_lists.items()
            },
        },
    }
    manifest_path = CONDITIONS_DIR / "manifest.json"
    manifest_path.parent.mkdir(parents=True, exist_ok=True)
    manifest_path.write_text(json.dumps(manifest, indent=2), encoding="utf-8")

    # ---- validation ------------------------------------------------------
    errors = []

    # 1. file count
    actual_files = sum(
        1
        for cond in CONDITIONS
        for task in task_ids
        if (CONDITIONS_DIR / cond / f"{task}.md").exists()
    )
    if actual_files != 100:
        errors.append(f"Expected 100 condition files, found {actual_files}")

    # 2. cold must be all zeros
    for task in task_ids:
        n = files_meta[task]["cold"]["char_count"]
        if n != 0:
            errors.append(f"cold/{task} has {n} chars (expected 0)")

    # 3. non-cold conditions should have non-zero chars on average
    for cond in ["whole_skill", "oracle_section", "oracle_rewritten", "oracle_multi"]:
        avg = char_totals[cond] / len(task_ids)
        if avg == 0:
            errors.append(f"{cond} avg chars == 0 (expected > 0)")

    # 4. whole_skill > oracle_rewritten on average
    ws_avg = char_totals["whole_skill"] / len(task_ids)
    or_avg = char_totals["oracle_rewritten"] / len(task_ids)
    if ws_avg <= or_avg:
        errors.append(
            f"whole_skill avg ({ws_avg:.0f}) not > oracle_rewritten avg ({or_avg:.0f})"
        )

    # ---- summary table ---------------------------------------------------
    print(f"\n{'Condition':<20} {'Avg chars':>10} {'Median':>10} {'Total':>10}")
    print("-" * 54)
    for cond in CONDITIONS:
        avg = char_totals[cond] / len(task_ids)
        med = statistics.median(char_lists[cond])
        print(f"{cond:<20} {avg:>10.0f} {med:>10.0f} {char_totals[cond]:>10}")

    print(f"\nTotal files written: {total_written}")
    print(f"Manifest: {manifest_path.relative_to(REPO_ROOT)}")

    if errors:
        print("\nVALIDATION FAILURES:")
        for e in errors:
            print(f"  - {e}")
        sys.exit(1)
    else:
        print("\nAll validations passed.")


if __name__ == "__main__":
    main()
