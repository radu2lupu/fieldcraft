#!/usr/bin/env python3
"""Prepare a multi-repo SWE-Bench-CL task set and required skill plan."""
from __future__ import annotations

import argparse
import json
import re
from collections import Counter, defaultdict
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]

DEFAULT_REPOS = [
    "django/django",
    "sympy/sympy",
    "sphinx-doc/sphinx",
    "matplotlib/matplotlib",
    "scikit-learn/scikit-learn",
    "astropy/astropy",
    "pytest-dev/pytest",
]


SKILL_PLAN = {
    "django/django": [
        ("django-orm-query-expressions", ["queryset", "aggregate", "annotate", "distinct", "subquery", "sql", "ordering", "delete"]),
        ("django-validation-and-regex", ["validator", "urlvalidator", "usernamevalidator", "regex", "invalid", "newline"]),
        ("django-files-storage-settings", ["file_upload", "storage", "permission", "filesystemstorage", "uploaded"]),
        ("django-admin-forms-templates", ["admin", "form", "inline", "template", "render_to_string", "autoescape"]),
        ("django-migrations-multidb-contenttypes", ["migration", "contenttype", "database", "router", "renamecontenttype"]),
        ("django-http-utils-dateparse", ["httpresponse", "memoryview", "parse_duration", "duration", "dateparse"]),
    ],
    "sympy/sympy": [
        ("sympy-matrix-array-operations", ["matrix", "hstack", "vstack", "identity", "col_insert", "array"]),
        ("sympy-evalf-numeric-evaluation", ["evalf", "precision", "_imp_", "float", "numeric"]),
        ("sympy-combinatorics-permutation", ["permutation", "cycle", "combinatorics", "subclass"]),
        ("sympy-sets-assumptions-logic", ["set", "complement", "symbol", "assumption", "notimplemented"]),
        ("sympy-simplification-subs-products", ["simplify", "subs", "product", "coth", "tan", "log"]),
    ],
    "sphinx-doc/sphinx": [
        ("sphinx-autodoc-typehints", ["autodoc", "typehint", "autoclass", "annotation", "return type"]),
        ("sphinx-rst-directives-includes", ["literalinclude", "include", "rst_prolog", "directive", "prepend", "heading"]),
        ("sphinx-latex-html-builders", ["latex", "html", "svg", "pdf", "builder", "highlighting"]),
        ("sphinx-toctree-index-domain", ["toctree", "genindex", "modindex", "domain", "cpp", "index"]),
        ("sphinx-warning-location-events", ["warning", "location", "source-read", "message", "event"]),
    ],
    "matplotlib/matplotlib": [
        ("matplotlib-axis-scale-limits", ["axis", "limits", "log", "range", "ticks", "shared-axis"]),
        ("matplotlib-legend-colorbar-subfigure", ["legend", "colorbar", "subfigure", "drawedges", "extend"]),
        ("matplotlib-units-dates-formatters", ["datetime", "dateformatter", "units", "category", "usetex"]),
        ("matplotlib-widgets-selectors", ["spanselector", "interactive", "axes limits", "selector"]),
        ("matplotlib-backend-figure-state", ["backend", "gcf", "figures", "rc_context", "projection"]),
    ],
    "scikit-learn/scikit-learn": [
        ("sklearn-estimator-api-validation", ["estimator", "fit", "predict", "notfittederror", "validation", "clone"]),
        ("sklearn-model-selection-cv", ["cross", "cv", "stratifiedkfold", "gridsearch", "refit", "score"]),
        ("sklearn-linear-models", ["ridge", "lasso", "logisticregression", "huber", "linear_model"]),
        ("sklearn-metrics-numerical-stability", ["metric", "score", "runtimewarning", "overflow", "fowlkes"]),
        ("sklearn-preprocessing-feature-extraction", ["countvectorizer", "kbinsdiscretizer", "sparsecoder", "feature"]),
    ],
    "astropy/astropy": [
        ("astropy-table-mixin-io", ["table", "mixin", "column", "ascii", "format", "qdp", "mrt"]),
        ("astropy-coordinates-wcs", ["skycoord", "itrs", "wcs", "world_to_pixel", "coordinate", "observed"]),
        ("astropy-units-quantity-ufunc", ["quantity", "unit", "ufunc", "notimplemented", "valueerror"]),
        ("astropy-modeling-separability", ["modeling", "compoundmodel", "separable", "separability_matrix"]),
        ("astropy-timeseries-required-columns", ["timeseries", "required column", "time", "flux"]),
        ("astropy-io-registry-identify", ["registry", "identify_format", "io", "indexerror"]),
    ],
    "pytest-dev/pytest": [
        ("pytest-collection-import-hooks", ["collection", "collect", "conftest", "importerror", "__init__.py"]),
        ("pytest-unittest-skip-pdb", ["unittest", "skip", "teardown", "pdb", "testcase"]),
        ("pytest-caplog-logging", ["caplog", "logging", "get_records", "clear"]),
        ("pytest-reporting-serialization", ["exception", "serialization", "chained", "headline", "report"]),
        ("pytest-capture-pastebin-bytes", ["capture", "encodedfile", "byteswarning", "pastebin", "lexer"]),
        ("pytest-marks-mro", ["mark", "mro", "class", "inheritance"]),
    ],
}


def read_jsonl(path: Path) -> list[dict]:
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def as_list(value) -> list[str]:
    if isinstance(value, list):
        return value
    if isinstance(value, str):
        try:
            loaded = json.loads(value)
            return loaded if isinstance(loaded, list) else []
        except json.JSONDecodeError:
            return []
    return []


def norm(text: str) -> str:
    return re.sub(r"\s+", " ", text.lower())


def choose_tasks(rows: list[dict], repos: list[str], per_repo: int) -> list[dict]:
    selected = []
    for repo in repos:
        candidates = [r for r in rows if r["repo"] == repo]
        candidates.sort(key=lambda r: (r.get("created_at", ""), r["instance_id"]))
        if len(candidates) < per_repo:
            raise SystemExit(f"{repo} only has {len(candidates)} instances, need {per_repo}")
        # Spread across the repo's timeline instead of taking a contiguous block.
        if per_repo == 1:
            picks = [candidates[len(candidates) // 2]]
        else:
            picks = [
                candidates[round(i * (len(candidates) - 1) / (per_repo - 1))]
                for i in range(per_repo)
            ]
        deduped = []
        seen = set()
        for row in picks:
            if row["instance_id"] not in seen:
                deduped.append(row)
                seen.add(row["instance_id"])
        for row in candidates:
            if len(deduped) >= per_repo:
                break
            if row["instance_id"] not in seen:
                deduped.append(row)
                seen.add(row["instance_id"])
        selected.extend(deduped[:per_repo])
    return selected


def infer_skills(task: dict) -> list[str]:
    text = norm(
        " ".join(
            [
                task.get("problem_statement", ""),
                task.get("hints_text", ""),
                " ".join(as_list(task.get("FAIL_TO_PASS"))),
                " ".join(as_list(task.get("PASS_TO_PASS"))),
            ]
        )
    )
    repo_plan = SKILL_PLAN.get(task["repo"], [])
    scores = []
    for skill, cues in repo_plan:
        score = sum(1 for cue in cues if cue.lower() in text)
        if score:
            scores.append((score, skill))
    scores.sort(reverse=True)
    if scores:
        return [skill for _, skill in scores[:3]]
    return [repo_plan[0][0]] if repo_plan else ["python-regression-test-minimization"]


def existing_skill_names(paths: list[Path]) -> set[str]:
    names = set()
    for path in paths:
        if not path.exists():
            continue
        for row in read_jsonl(path):
            name = (row.get("name") or row.get("skill_name") or row.get("id") or "").strip()
            if name:
                names.add(name)
    return names


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dataset", type=Path, default=ROOT / "benchmarks/swe-bench-cl/data/swe-bench-cl-instances.jsonl")
    parser.add_argument("--repos", nargs="+", default=DEFAULT_REPOS)
    parser.add_argument("--per-repo", type=int, default=8)
    parser.add_argument("--out-dir", type=Path, default=ROOT / "benchmarks/swe-bench-cl/four-tier/multirepo-56")
    args = parser.parse_args()

    rows = read_jsonl(args.dataset)
    selected = choose_tasks(rows, args.repos, args.per_repo)
    args.out_dir.mkdir(parents=True, exist_ok=True)
    tasks_path = args.out_dir / "tasks.jsonl"
    ids_path = args.out_dir / "instance_ids.txt"
    plan_path = args.out_dir / "skill-needs.md"
    write_rows = []
    skill_counts = Counter()
    by_repo: dict[str, list[dict]] = defaultdict(list)
    for task in selected:
        skills = infer_skills(task)
        skill_counts.update(skills)
        f2p = as_list(task.get("FAIL_TO_PASS"))
        row = {
            "instance_id": task["instance_id"],
            "repo": task["repo"],
            "version": task.get("version"),
            "created_at": task.get("created_at"),
            "title": (task.get("problem_statement") or "").strip().splitlines()[0],
            "fail_to_pass_count": len(f2p),
            "fail_to_pass_sample": f2p[:3],
            "needed_skills": skills,
        }
        write_rows.append(row)
        by_repo[task["repo"]].append(row)
    tasks_path.write_text("\n".join(json.dumps(r, sort_keys=True) for r in write_rows) + "\n")
    ids_path.write_text("\n".join(r["instance_id"] for r in write_rows) + "\n")

    existing = existing_skill_names(
        [
            ROOT / "eval/skills/corpus/swe_bench_cl_seed_skills.jsonl",
            ROOT / "eval/skills/corpus/local_skills.jsonl",
            ROOT / "eval/skills/corpus/public_registry_skills_500.jsonl",
        ]
    )
    lines = [
        "# SWE-Bench-CL Multi-Repo Skill Needs",
        "",
        f"- Repos: {len(args.repos)}",
        f"- Tasks per repo: {args.per_repo}",
        f"- Total tasks: {len(write_rows)}",
        "",
        "## Selected Repos",
        "",
    ]
    for repo in args.repos:
        lines.append(f"- `{repo}`: {len(by_repo[repo])} tasks")
    lines.extend(["", "## Required Skill Cards", ""])
    for skill, count in skill_counts.most_common():
        status = "present" if skill in existing else "missing"
        lines.append(f"- `{skill}`: {count} tasks ({status})")
    lines.extend(["", "## Per-Repo Task Map", ""])
    for repo in args.repos:
        lines.append(f"### {repo}")
        lines.append("")
        for row in by_repo[repo]:
            skills = ", ".join(f"`{s}`" for s in row["needed_skills"])
            lines.append(f"- `{row['instance_id']}`: {row['title'][:140]}  ")
            lines.append(f"  Skills: {skills}")
        lines.append("")
    plan_path.write_text("\n".join(lines))
    print(tasks_path)
    print(ids_path)
    print(plan_path)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
