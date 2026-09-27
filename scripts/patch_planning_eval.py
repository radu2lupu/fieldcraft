#!/usr/bin/env python3
"""Compare cold vs retrieval-assisted pre-edit plans on SWE-Bench CL tasks.

This is the measurement rung after retrieval usefulness. It does not edit code
or run tests. It asks whether retrieved guidance improves the agent's initial
plan: likely files to inspect, failure-mode summary, and verification command.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import random
import re
import subprocess
import tempfile
import textwrap
import time
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
DIFF_RE = re.compile(r"^diff --git a/(.*?) b/(.*?)$", re.MULTILINE)


DEFAULT_IDS = [
    "astropy__astropy-12907",
    "django__django-10880",
    "matplotlib__matplotlib-13989",
    "pytest-dev__pytest-5809",
    "sympy__sympy-12481",
    "pydata__xarray-4075",
]


def wilson(k: int, n: int, z: float = 1.96) -> tuple[float, float]:
    if n == 0:
        return (0.0, 0.0)
    p = k / n
    denom = 1 + z * z / n
    center = (p + z * z / (2 * n)) / denom
    margin = z * ((p * (1 - p) + z * z / (4 * n)) / n) ** 0.5 / denom
    return (max(0.0, center - margin), min(1.0, center + margin))


def read_jsonl(path: Path) -> list[dict[str, Any]]:
    rows = []
    for line in path.read_text(errors="ignore").splitlines():
        if line.strip():
            rows.append(json.loads(line))
    return rows


def write_jsonl(path: Path, rows: list[dict[str, Any]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("".join(json.dumps(row, sort_keys=True) + "\n" for row in rows))


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def changed_files(patch: str) -> list[str]:
    files = []
    for _a, b in DIFF_RE.findall(patch or ""):
        if b not in files:
            files.append(b)
    return files


def repo_family(repo: str | None) -> str:
    text = (repo or "").lower()
    families = [
        ("django", ["django"]),
        ("pytest", ["pytest"]),
        ("sympy", ["sympy"]),
        ("sphinx", ["sphinx"]),
        ("sklearn", ["scikit", "sklearn"]),
        ("xarray", ["xarray"]),
        ("astropy", ["astropy"]),
        ("matplotlib", ["matplotlib"]),
        ("pylint", ["pylint"]),
        ("requests", ["requests"]),
        ("flask", ["flask"]),
        ("seaborn", ["seaborn"]),
    ]
    for family, needles in families:
        if any(needle in text for needle in needles):
            return family
    return text.replace("/", "-")


def build_file_universe(rows: list[dict[str, Any]]) -> dict[str, list[str]]:
    universe: dict[str, set[str]] = {}
    for row in rows:
        repo = row.get("repo") or ""
        bucket = universe.setdefault(repo, set())
        for field in ["patch", "test_patch"]:
            for path in changed_files(row.get(field) or ""):
                bucket.add(path)
        for test in parse_tests(row.get("FAIL_TO_PASS")):
            bucket.add(test.split("::", 1)[0])
    return {repo: sorted(paths) for repo, paths in universe.items()}


def balanced_sample(rows: list[dict[str, Any]], size: int, seed: int = 17) -> list[dict[str, Any]]:
    by_repo: dict[str, list[dict[str, Any]]] = {}
    for row in rows:
        by_repo.setdefault(row.get("repo") or "", []).append(row)
    rng = random.Random(seed)
    for bucket in by_repo.values():
        bucket.sort(key=lambda r: r["instance_id"])
        rng.shuffle(bucket)
    selected: list[dict[str, Any]] = []
    repos = sorted(by_repo)
    while len(selected) < size and any(by_repo.values()):
        for repo in repos:
            if len(selected) >= size:
                break
            if by_repo[repo]:
                selected.append(by_repo[repo].pop())
    return selected


def parse_tests(value: Any) -> list[str]:
    if isinstance(value, list):
        return [str(v) for v in value]
    if isinstance(value, str):
        try:
            parsed = json.loads(value)
            if isinstance(parsed, list):
                return [str(v) for v in parsed]
        except Exception:
            pass
    return []


def cleaned_problem(text: str | None, max_chars: int = 2400) -> str:
    text = text or ""
    text = re.sub(r"<details>.*?</details>", "", text, flags=re.IGNORECASE | re.DOTALL)
    text = re.sub(r"#### Output of .*", "", text, flags=re.IGNORECASE | re.DOTALL)
    text = re.sub(r"INSTALLED VERSIONS.*", "", text, flags=re.IGNORECASE | re.DOTALL)
    lines: list[str] = []
    seen: set[str] = set()
    for raw in text.splitlines():
        line = raw.rstrip()
        key = line.strip()
        if key and key in seen:
            continue
        if key:
            seen.add(key)
        lines.append(line)
    compact = "\n".join(lines).strip()
    if len(compact) > max_chars:
        return compact[:max_chars].rsplit("\n", 1)[0]
    return compact


def task_query(row: dict[str, Any]) -> str:
    failing = " ".join(parse_tests(row.get("FAIL_TO_PASS"))[:8])
    family = repo_family(row.get("repo"))
    return textwrap.dedent(
        f"""
        Repository: {row.get('repo')}
        Repository family: {family}
        Instance: {row.get('instance_id')}
        Task: produce a concise pre-edit fix plan and verification command.
        Failing tests: {failing}
        Problem:
        {cleaned_problem(row.get('problem_statement'))}
        """
    ).strip()


def run_skill_retrieval(corpus: Path, tasks: list[dict[str, Any]], out_dir: Path, limit: int) -> dict[str, dict[str, Any]]:
    queries = [
        {
            "id": row["instance_id"],
            "query": task_query(row),
            "expected": [],
            "acceptable": [],
            "label_source": "patch_planning_unlabeled",
        }
        for row in tasks
    ]
    queries_path = out_dir / "skill_queries.jsonl"
    traces_path = out_dir / "skill_retrieval_traces.jsonl"
    write_jsonl(queries_path, queries)
    subprocess.run(
        [
            "cargo",
            "run",
            "-q",
            "-p",
            "fieldcraft-cli",
            "--",
            "eval",
            "skills",
            "--corpus",
            str(corpus),
            "--queries",
            str(queries_path),
            "--output-jsonl",
            str(traces_path),
            "--limit",
            str(limit),
        ],
        cwd=ROOT,
        check=True,
        timeout=240,
    )
    return {row["id"]: row for row in read_jsonl(traces_path)}


def prepare_corpus(primary: Path, supplemental: list[Path], out_dir: Path) -> tuple[Path, list[dict[str, Any]]]:
    cards = read_jsonl(primary)
    for path in supplemental:
        cards.extend(read_jsonl(path))
    if not supplemental:
        return primary, cards
    merged = out_dir / "merged_skill_corpus.jsonl"
    write_jsonl(merged, cards)
    return merged, cards


def deterministic_null_card(cards: list[dict[str, Any]], repo: str | None, instance_id: str) -> list[dict[str, Any]]:
    family = repo_family(repo)
    pool = [card for card in cards if not skill_relevance(repo, [card])["top_relevant"]]
    if not pool:
        return []
    seed = int(hashlib.sha256((instance_id + family).encode()).hexdigest()[:12], 16)
    return [pool[seed % len(pool)]]


def deterministic_oracle_card(cards: list[dict[str, Any]], repo: str | None, instance_id: str) -> list[dict[str, Any]]:
    family = repo_family(repo)
    def exact_family_match(card: dict[str, Any]) -> bool:
        text = " ".join(str(card.get(k, "")) for k in ["skill_name", "source_uri", "description", "triggers"]).lower()
        aliases = {
            "sklearn": ["sklearn", "scikit-learn", "scikit"],
            "requests": ["requests"],
            "django": ["django"],
            "pytest": ["pytest"],
            "matplotlib": ["matplotlib"],
            "xarray": ["xarray"],
            "astropy": ["astropy"],
            "seaborn": ["seaborn", "matplotlib"],
        }.get(family, [family])
        return any(alias in text for alias in aliases)
    pool = [card for card in cards if exact_family_match(card)]
    if not pool:
        return []
    def priority(card: dict[str, Any]) -> tuple[int, int, str]:
        text = " ".join(str(card.get(k, "")) for k in ["skill_name", "source_uri", "description"]).lower()
        supplemental = "local://supplemental/swe-bench-cl" in text
        exact_family = family in text
        return (0 if supplemental else 1, 0 if exact_family else 1, str(card.get("skill_name") or ""))
    return [sorted(pool, key=priority)[0]]


def format_only_card(instance_id: str) -> list[dict[str, Any]]:
    return [
        {
            "skill_name": f"format-only-control-{instance_id}",
            "title": "Format-only control",
            "description": "No relevant task knowledge is available in this control card.",
            "activation_cues": [],
            "procedure": "No domain guidance. Use the task statement and repository evidence only.",
            "constraints": ["This is an evaluation control, not a real skill."],
        }
    ]


def extract_json(text: str) -> dict[str, Any]:
    text = text.strip()
    if text.startswith("```"):
        text = re.sub(r"^```(?:json)?", "", text).strip()
        text = re.sub(r"```$", "", text).strip()
    try:
        return json.loads(text)
    except Exception:
        start = text.find("{")
        end = text.rfind("}")
        if start >= 0 and end > start:
            return json.loads(text[start : end + 1])
        raise


# Modes that use source-first ranking instructions (same prompt structure for
# both format-control and lean_exact so the only variable is card content).
_SOURCE_FIRST_MODES = {"format", "format-only", "lean_exact"}


def plan_with_claude(row: dict[str, Any], mode: str, cards: list[dict[str, Any]], timeout: int, model: str = "claude-sonnet-4-6") -> dict[str, Any]:
    guidance_block = ""
    card_text = ""
    if cards:
        top = cards[0]
        card_text = json.dumps(
            {
                "skill_name": top.get("skill_name"),
                "title": top.get("title"),
                "description": top.get("description"),
                "activation_cues": top.get("activation_cues"),
                "procedure": top.get("procedure"),
                "constraints": top.get("constraints"),
            },
            indent=2,
        )
        guidance_block = f"Retrieved guidance:\n{card_text}\n\n"
    elif mode != "empty":
        guidance_block = "Retrieved guidance:\nNo retrieved skill card.\n\n"

    # Source-first ranking instructions injected for format and lean_exact modes.
    # This makes the prompt structure identical between format (no card content) and
    # lean_exact (with retrieved card), so the only variable is whether real skill
    # content is present.
    if mode in _SOURCE_FIRST_MODES:
        ranking_instructions = (
            "Ranking instruction: list source files before test files in files_to_inspect. "
            "If the retrieved guidance does not clearly apply to this task, ignore it and "
            "rely on the task statement and your repository knowledge only.\n"
        )
    else:
        ranking_instructions = ""

    prompt = textwrap.dedent(
        f"""
        You are planning a SWE-Bench style code fix. Do not write code.
        Return JSON only with keys:
        - failure_mode: short string
        - files_to_inspect: array of likely source file paths, most likely first
        - verification_commands: array of focused test commands or test node ids
        - use_retrieved_guidance: boolean
        - rationale: short string

        Mode: {mode}
        {ranking_instructions}{guidance_block}
        Task:
        {task_query(row)}
        """
    ).strip()
    started = time.perf_counter()
    try:
        proc = subprocess.run(
            ["claude", "--model", model, "-p", prompt],
            cwd=ROOT,
            text=True,
            capture_output=True,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired as err:
        return {
            "error": f"timeout after {timeout}s",
            "latency_s": time.perf_counter() - started,
            "raw": (err.stdout or b"").decode(errors="ignore") if isinstance(err.stdout, bytes) else (err.stdout or ""),
            "prompt_tokens_est": max(1, round(len(prompt) / 4)),
            "output_tokens_est": max(1, round(len(err.stdout or b"") / 4)) if isinstance(err.stdout, bytes) else 0,
        }
    elapsed = time.perf_counter() - started
    if proc.returncode != 0:
        return {
            "error": proc.stderr[-1000:] or proc.stdout[-1000:],
            "latency_s": elapsed,
            "raw": proc.stdout,
            "model": model,
        }
    try:
        plan = extract_json(proc.stdout)
    except Exception as err:
        plan = {"error": f"parse failed: {err}", "raw": proc.stdout}
    plan["latency_s"] = elapsed
    plan["prompt_tokens_est"] = max(1, round(len(prompt) / 4))
    plan["output_tokens_est"] = max(1, round(len(proc.stdout) / 4))
    plan["model"] = model
    return plan


def file_matches(predicted: str, actual: str) -> bool:
    predicted = predicted.strip()
    actual = actual.strip()
    if not predicted or not actual:
        return False
    return predicted == actual or predicted.endswith(actual) or actual.endswith(predicted) or Path(predicted).name == Path(actual).name


def first_match_rank(predicted: list[str], actuals: list[str]) -> int | None:
    for idx, pred in enumerate(predicted, start=1):
        if any(file_matches(pred, actual) for actual in actuals):
            return idx
    return None


def recall_at(predicted: list[str], actuals: list[str], k: int | None = None) -> float | None:
    if not actuals:
        return None
    window = predicted if k is None else predicted[:k]
    hits = sum(1 for actual in actuals if any(file_matches(pred, actual) for pred in window))
    return hits / len(actuals)


def parse_unittest_label(label: str) -> dict[str, str | None]:
    """Parse a unittest-style label like 'test_foo (mod.ClassName)' or pytest 'path::Class::test_foo'.

    Returns dict with keys: test_name, class_name, module_path, pytest_node.
    """
    result: dict[str, str | None] = {"test_name": None, "class_name": None, "module_path": None, "pytest_node": None}
    # Unittest style: test_name (dotted.module.ClassName)
    ut_match = re.match(r"^(\w+)\s+\(([^)]+)\)$", label.strip())
    if ut_match:
        test_name = ut_match.group(1)
        qualified = ut_match.group(2)  # e.g. "dbshell.test_postgresql.PostgreSqlDbshellCommandTestCase"
        parts = qualified.rsplit(".", 1)
        class_name = parts[-1] if parts else None
        module_path = parts[0].replace(".", "/") if len(parts) > 1 else None
        result["test_name"] = test_name
        result["class_name"] = class_name
        result["module_path"] = module_path
        # Build equivalent pytest node if possible: module/path.py::ClassName::test_name
        if module_path and class_name:
            result["pytest_node"] = f"{module_path}.py::{class_name}::{test_name}"
        return result
    # Pytest style: path/to/file.py::ClassName::test_name or path::test_name
    if "::" in label:
        parts = label.split("::")
        result["module_path"] = parts[0]
        result["test_name"] = parts[-1]
        result["class_name"] = parts[1] if len(parts) >= 3 else None
        result["pytest_node"] = label
    return result


def test_matches(command: str, test: str) -> bool:
    """Return True if command references the given test label.

    Handles both pytest-style (path::Class::test_name) and
    unittest-style (test_name (module.ClassName)) labels.
    """
    if test in command:
        return True
    parsed = parse_unittest_label(test)
    if parsed["test_name"] and parsed["test_name"] in command:
        return True
    if parsed["class_name"] and parsed["class_name"] in command:
        return True
    if parsed["module_path"] and parsed["module_path"] in command:
        return True
    if parsed["pytest_node"] and parsed["pytest_node"] in command:
        return True
    # Also handle plain pytest-style: check test_file and test_name from "::" split
    test_file = test.split("::", 1)[0]
    test_name_plain = test.rsplit("::", 1)[-1]
    if test_file and test_file in command:
        return True
    if test_name_plain and test_name_plain != test and test_name_plain in command:
        return True
    return False


def test_first_match_rank(commands: list[str], tests: list[str]) -> int | None:
    for idx, command in enumerate(commands, start=1):
        if any(test_matches(command, test) for test in tests):
            return idx
    return None


def score_plan(plan: dict[str, Any], patch_files: list[str], fail_tests: list[str]) -> dict[str, Any]:
    predicted_files = [str(v) for v in plan.get("files_to_inspect") or []]
    predicted_commands = [str(v) for v in plan.get("verification_commands") or []]
    file_rank = first_match_rank(predicted_files, patch_files)
    test_rank = test_first_match_rank(predicted_commands, fail_tests)
    test_hits = sum(1 for test in fail_tests if any(test_matches(cmd, test) for cmd in predicted_commands))
    return {
        "patch_file_recall": recall_at(predicted_files, patch_files),
        "patch_file_recall_at_3": recall_at(predicted_files, patch_files, 3),
        "patch_file_hit": file_rank is not None if patch_files else None,
        "patch_file_hit_at_1": file_rank == 1 if patch_files else None,
        "patch_file_mrr": (1 / file_rank) if file_rank else (0.0 if patch_files else None),
        "test_target_recall": test_hits / len(fail_tests) if fail_tests else None,
        "test_target_hit": test_hits > 0 if fail_tests else None,
        "test_target_hit_at_1": test_rank == 1 if fail_tests else None,
        "test_target_mrr": (1 / test_rank) if test_rank else (0.0 if fail_tests else None),
        "predicted_file_count": len(predicted_files),
        "predicted_command_count": len(predicted_commands),
    }


def score_task_plan(row: dict[str, Any], plan: dict[str, Any]) -> dict[str, Any]:
    return score_plan(plan, changed_files(row.get("patch") or ""), parse_tests(row.get("FAIL_TO_PASS")))


def random_plan_score(row: dict[str, Any], universe: list[str], patch_files: list[str], fail_tests: list[str], width: int = 5) -> dict[str, Any]:
    seed = int(hashlib.sha256(row["instance_id"].encode()).hexdigest()[:12], 16)
    rng = random.Random(seed)
    candidates = [p for p in universe if p not in patch_files] + patch_files
    predicted = rng.sample(candidates, min(width, len(candidates))) if candidates else []
    plan = {"files_to_inspect": predicted, "verification_commands": ["pytest"]}
    return score_plan(plan, patch_files, fail_tests)


def skill_relevance(repo: str | None, cards: list[dict[str, Any]]) -> dict[str, Any]:
    if not cards:
        return {"family": repo_family(repo), "top_relevant": False, "any_relevant": False}
    family = repo_family(repo)
    aliases = {
        "sklearn": ["sklearn", "scikit-learn", "scikit"],
        "pytest": ["pytest", "caplog"],
        "django": ["django", "orm", "queryset"],
        "matplotlib": ["matplotlib", "plot"],
    }
    needles = aliases.get(family, [family])
    flags = []
    for card in cards:
        text = " ".join(str(card.get(k, "")) for k in ["skill_name", "title", "description", "activation_cues", "procedure"]).lower()
        flags.append(any(needle in text for needle in needles))
    return {"family": family, "top_relevant": bool(flags and flags[0]), "any_relevant": any(flags)}


_LOG_TRUNCATE_BYTES = 50 * 1024  # 50 KB per stream


def write_condition_log(out_dir: Path, condition: str, instance_id: str, prompt: str, stdout: str, stderr: str) -> None:
    """Write per-(task, condition) log file for human debugging."""
    log_dir = out_dir / "logs" / condition
    log_dir.mkdir(parents=True, exist_ok=True)
    def trunc(s: str, limit: int = _LOG_TRUNCATE_BYTES) -> str:
        if len(s.encode("utf-8", errors="replace")) > limit:
            return s[:limit] + "\n... [truncated]"
        return s
    content = "\n".join([
        f"=== PROMPT ===",
        prompt,
        "",
        f"=== STDOUT ===",
        trunc(stdout),
        "",
        f"=== STDERR ===",
        trunc(stderr),
        "",
    ])
    (log_dir / f"{instance_id}.txt").write_text(content, encoding="utf-8", errors="replace")


def plan_with_claude_logged(
    row: dict[str, Any],
    mode: str,
    cards: list[dict[str, Any]],
    timeout: int,
    model: str,
    out_dir: Path,
    condition_key: str,
) -> dict[str, Any]:
    """Wrapper around plan_with_claude that captures prompt/stdout/stderr and writes a log file."""
    # Rebuild the prompt the same way plan_with_claude does, to capture it for logging.
    guidance_block = ""
    if cards:
        top = cards[0]
        card_text = json.dumps(
            {
                "skill_name": top.get("skill_name"),
                "title": top.get("title"),
                "description": top.get("description"),
                "activation_cues": top.get("activation_cues"),
                "procedure": top.get("procedure"),
                "constraints": top.get("constraints"),
            },
            indent=2,
        )
        guidance_block = f"Retrieved guidance:\n{card_text}\n\n"
    elif mode != "empty":
        guidance_block = "Retrieved guidance:\nNo retrieved skill card.\n\n"
    if mode in _SOURCE_FIRST_MODES:
        ranking_instructions = (
            "Ranking instruction: list source files before test files in files_to_inspect. "
            "If the retrieved guidance does not clearly apply to this task, ignore it and "
            "rely on the task statement and your repository knowledge only.\n"
        )
    else:
        ranking_instructions = ""
    prompt = textwrap.dedent(
        f"""
        You are planning a SWE-Bench style code fix. Do not write code.
        Return JSON only with keys:
        - failure_mode: short string
        - files_to_inspect: array of likely source file paths, most likely first
        - verification_commands: array of focused test commands or test node ids
        - use_retrieved_guidance: boolean
        - rationale: short string

        Mode: {mode}
        {ranking_instructions}{guidance_block}
        Task:
        {task_query(row)}
        """
    ).strip()

    # Run claude directly here to capture stdout/stderr for logging.
    iid = row["instance_id"]
    started = time.perf_counter()
    stdout_raw = ""
    stderr_raw = ""
    try:
        proc = subprocess.run(
            ["claude", "--model", model, "-p", prompt],
            cwd=ROOT,
            text=True,
            capture_output=True,
            timeout=timeout,
        )
        stdout_raw = proc.stdout or ""
        stderr_raw = proc.stderr or ""
        elapsed = time.perf_counter() - started
        if proc.returncode != 0:
            plan: dict[str, Any] = {
                "error": stderr_raw[-1000:] or stdout_raw[-1000:],
                "latency_s": elapsed,
                "raw": stdout_raw,
                "model": model,
                "prompt_tokens_est": max(1, round(len(prompt) / 4)),
                "output_tokens_est": max(1, round(len(stdout_raw) / 4)),
            }
        else:
            try:
                plan = extract_json(stdout_raw)
            except Exception as err:
                plan = {"error": f"parse failed: {err}", "raw": stdout_raw}
            plan["latency_s"] = elapsed
            plan["prompt_tokens_est"] = max(1, round(len(prompt) / 4))
            plan["output_tokens_est"] = max(1, round(len(stdout_raw) / 4))
            plan["model"] = model
    except subprocess.TimeoutExpired as err:
        elapsed = time.perf_counter() - started
        stdout_raw = (err.stdout or b"").decode(errors="ignore") if isinstance(err.stdout, bytes) else (err.stdout or "")
        stderr_raw = (err.stderr or b"").decode(errors="ignore") if isinstance(err.stderr, bytes) else (err.stderr or "")
        plan = {
            "error": f"timeout after {timeout}s",
            "latency_s": elapsed,
            "raw": stdout_raw,
            "prompt_tokens_est": max(1, round(len(prompt) / 4)),
            "output_tokens_est": max(1, round(len(stdout_raw) / 4)),
        }

    write_condition_log(out_dir, condition_key, iid, prompt, stdout_raw, stderr_raw)
    return plan


def _error_score() -> dict[str, Any]:
    """Return a score dict with all fields None for error/timeout cells."""
    return {
        "patch_file_recall": None,
        "patch_file_recall_at_3": None,
        "patch_file_hit": None,
        "patch_file_hit_at_1": None,
        "patch_file_mrr": None,
        "test_target_recall": None,
        "test_target_hit": None,
        "test_target_hit_at_1": None,
        "test_target_mrr": None,
        "predicted_file_count": 0,
        "predicted_command_count": 0,
    }


def safe_score_plan(plan: dict[str, Any], patch_files: list[str], fail_tests: list[str]) -> dict[str, Any]:
    """Score plan, returning error score dict if plan has an error key."""
    if "error" in plan:
        return _error_score()
    return score_plan(plan, patch_files, fail_tests)


def _fmt_score_cell(score: dict[str, Any], key: str, plan: dict[str, Any]) -> str:
    """Format a score cell, showing ERR or TIMEOUT for error/timeout plans."""
    if "error" in plan:
        err = str(plan["error"])
        if "timeout" in err.lower():
            return "TIMEOUT"
        return "ERR"
    val = score.get(key)
    if val is None:
        return "—"
    if isinstance(val, float):
        return f"{val:.3f}"
    return str(val)


def render(rows: list[dict[str, Any]], model: str | None = None) -> str:
    # All modes present in the data (order matters for report columns).
    all_modes = ["empty", "cold", "format", "lean_exact", "retrieval", "oracle", "null", "random_file"]

    def avg(mode: str, key: str) -> float:
        vals = [r[mode]["score"][key] for r in rows if r.get(mode, {}).get("score", {}).get(key) is not None]
        return sum(float(v) for v in vals) / len(vals) if vals else 0.0

    def rate(mode: str, key: str) -> float:
        vals = [r[mode]["score"][key] for r in rows if r.get(mode, {}).get("score", {}).get(key) is not None]
        return sum(1 for v in vals if v) / len(vals) if vals else 0.0

    def mcnemar_line(a: str, b: str, key: str) -> str:
        b_wins = sum(1 for r in rows if not (r.get(a) or {}).get("score", {}).get(key) and (r.get(b) or {}).get("score", {}).get(key))
        a_wins = sum(1 for r in rows if (r.get(a) or {}).get("score", {}).get(key) and not (r.get(b) or {}).get("score", {}).get(key))
        discordant = a_wins + b_wins
        if discordant == 0:
            return f"{a} vs {b} on {key}: no discordant pairs"
        from math import comb
        tail = sum(comb(discordant, i) for i in range(0, min(a_wins, b_wins) + 1)) / (2**discordant)
        p = min(1.0, 2 * tail)
        return f"{a} vs {b} on {key}: {b} wins {b_wins}, {a} wins {a_wins}, exact p={p:.3f}"

    # Only render modes that appear in at least one row.
    present_modes = [m for m in all_modes if any(m in r for r in rows)]

    lines = [
        "# Patch Planning Eval",
        "",
        "This eval compares cold vs retrieval-assisted pre-edit plans on SWE-Bench CL tasks. It scores only planning artifacts, not patches.",
        "",
        f"- Planner model: `{model or 'unspecified'}`",
        f"- Tasks: {len(rows)}",
        "",
        "## Summary",
        "",
        "| mode | file hit@1 | file MRR | file recall@3 | test hit@1 | test MRR | avg prompt tokens | avg output tokens |",
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]

    def ci_cell(mode: str, key: str) -> str:
        vals = [r[mode]["score"][key] for r in rows if r.get(mode, {}).get("score", {}).get(key) is not None]
        k = sum(1 for v in vals if v)
        lo, hi = wilson(k, len(vals))
        return f"{k}/{len(vals)} [{lo:.3f}, {hi:.3f}]"

    for mode in present_modes:
        if mode == "random_file":
            prompt_avg = 0.0
            output_avg = 0.0
        else:
            prompt_avg = sum(r.get(mode, {}).get("plan", {}).get("prompt_tokens_est", 0) for r in rows) / max(1, len(rows))
            output_avg = sum(r.get(mode, {}).get("plan", {}).get("output_tokens_est", 0) for r in rows) / max(1, len(rows))
        lines.append(
            f"| {mode} | {rate(mode, 'patch_file_hit_at_1'):.3f} | {avg(mode, 'patch_file_mrr'):.3f} | {avg(mode, 'patch_file_recall_at_3'):.3f} | {rate(mode, 'test_target_hit_at_1'):.3f} | {avg(mode, 'test_target_mrr'):.3f} | {prompt_avg:.0f} | {output_avg:.0f} |"
        )

    relevant_top = sum(1 for r in rows if r["skill_relevance"]["top_relevant"])
    relevant_any = sum(1 for r in rows if r["skill_relevance"]["any_relevant"])
    relevant_rows = [r for r in rows if r["skill_relevance"]["top_relevant"]]
    irrelevant_rows = [r for r in rows if not r["skill_relevance"]["top_relevant"]]

    def subset_rate(subset: list[dict[str, Any]], mode: str, key: str) -> float:
        vals = [r[mode]["score"][key] for r in subset if r.get(mode, {}).get("score", {}).get(key) is not None]
        return sum(1 for v in vals if v) / len(vals) if vals else 0.0

    lines.extend(
        [
            "",
            "## Retrieval Relevance",
            "",
            f"- Top retrieved skill relevant by repo-family heuristic: {relevant_top}/{len(rows)}.",
            f"- Any retrieved skill relevant by repo-family heuristic: {relevant_any}/{len(rows)}.",
            f"- Empty file hit@1 Wilson CI: {ci_cell('empty', 'patch_file_hit_at_1')}.",
            f"- Cold file hit@1 Wilson CI: {ci_cell('cold', 'patch_file_hit_at_1')}.",
            f"- Format file hit@1 Wilson CI: {ci_cell('format', 'patch_file_hit_at_1')}.",
            f"- Lean_exact file hit@1 Wilson CI: {ci_cell('lean_exact', 'patch_file_hit_at_1')}." if "lean_exact" in present_modes else "",
            f"- Retrieval file hit@1 Wilson CI: {ci_cell('retrieval', 'patch_file_hit_at_1')}.",
            f"- Oracle file hit@1 Wilson CI: {ci_cell('oracle', 'patch_file_hit_at_1')}.",
            f"- Empty test hit@1 Wilson CI: {ci_cell('empty', 'test_target_hit_at_1')}.",
            f"- Cold test hit@1 Wilson CI: {ci_cell('cold', 'test_target_hit_at_1')}.",
            f"- Format test hit@1 Wilson CI: {ci_cell('format', 'test_target_hit_at_1')}.",
            f"- Lean_exact test hit@1 Wilson CI: {ci_cell('lean_exact', 'test_target_hit_at_1')}." if "lean_exact" in present_modes else "",
            f"- Retrieval test hit@1 Wilson CI: {ci_cell('retrieval', 'test_target_hit_at_1')}.",
            f"- Oracle test hit@1 Wilson CI: {ci_cell('oracle', 'test_target_hit_at_1')}.",
            f"- Paired file hit@1 (cold vs retrieval): {mcnemar_line('cold', 'retrieval', 'patch_file_hit_at_1')}.",
            f"- Paired test hit@1 (cold vs retrieval): {mcnemar_line('cold', 'retrieval', 'test_target_hit_at_1')}.",
            f"- Paired file hit@1 (format vs lean_exact): {mcnemar_line('format', 'lean_exact', 'patch_file_hit_at_1')}." if "lean_exact" in present_modes else "",
            f"- Paired test hit@1 (format vs lean_exact): {mcnemar_line('format', 'lean_exact', 'test_target_hit_at_1')}." if "lean_exact" in present_modes else "",
            f"- Relevant subset file hit@1 empty/cold/format/lean_exact/retrieval/oracle/null: "
            f"{subset_rate(relevant_rows, 'empty', 'patch_file_hit_at_1'):.3f}/"
            f"{subset_rate(relevant_rows, 'cold', 'patch_file_hit_at_1'):.3f}/"
            f"{subset_rate(relevant_rows, 'format', 'patch_file_hit_at_1'):.3f}/"
            f"{subset_rate(relevant_rows, 'lean_exact', 'patch_file_hit_at_1'):.3f}/"
            f"{subset_rate(relevant_rows, 'retrieval', 'patch_file_hit_at_1'):.3f}/"
            f"{subset_rate(relevant_rows, 'oracle', 'patch_file_hit_at_1'):.3f}/"
            f"{subset_rate(relevant_rows, 'null', 'patch_file_hit_at_1'):.3f}.",
            f"- Irrelevant subset file hit@1 empty/cold/format/lean_exact/retrieval/oracle/null: "
            f"{subset_rate(irrelevant_rows, 'empty', 'patch_file_hit_at_1'):.3f}/"
            f"{subset_rate(irrelevant_rows, 'cold', 'patch_file_hit_at_1'):.3f}/"
            f"{subset_rate(irrelevant_rows, 'format', 'patch_file_hit_at_1'):.3f}/"
            f"{subset_rate(irrelevant_rows, 'lean_exact', 'patch_file_hit_at_1'):.3f}/"
            f"{subset_rate(irrelevant_rows, 'retrieval', 'patch_file_hit_at_1'):.3f}/"
            f"{subset_rate(irrelevant_rows, 'oracle', 'patch_file_hit_at_1'):.3f}/"
            f"{subset_rate(irrelevant_rows, 'null', 'patch_file_hit_at_1'):.3f}.",
            "",
            "## Per Task",
            "",
            "| instance | repo | skill | relevant | empty f@1 | cold f@1 | format f@1 | lean_exact f@1 | retrieval f@1 | oracle f@1 | null f@1 | retrieval MRR | lean_exact MRR | cold t@1 | format t@1 | lean_exact t@1 | retrieval t@1 | oracle t@1 |",
            "| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
        ]
    )
    for r in rows:
        skill = ((r.get("retrieved_cards") or [{}])[0].get("skill_name") or "none")
        def sc(mode: str, key: str) -> str:
            plan = r.get(mode, {}).get("plan", {})
            score = r.get(mode, {}).get("score", {})
            return _fmt_score_cell(score, key, plan)
        def mrr(mode: str) -> str:
            plan = r.get(mode, {}).get("plan", {})
            score = r.get(mode, {}).get("score", {})
            if "error" in plan:
                return "ERR" if "timeout" not in str(plan.get("error", "")).lower() else "TIMEOUT"
            v = score.get("patch_file_mrr")
            return f"{v:.3f}" if v is not None else "—"
        lines.append(
            f"| `{r['instance_id']}` | `{r['repo']}` | `{skill}` | {r['skill_relevance']['top_relevant']} "
            f"| {sc('empty', 'patch_file_hit_at_1')} | {sc('cold', 'patch_file_hit_at_1')} "
            f"| {sc('format', 'patch_file_hit_at_1')} | {sc('lean_exact', 'patch_file_hit_at_1')} "
            f"| {sc('retrieval', 'patch_file_hit_at_1')} | {sc('oracle', 'patch_file_hit_at_1')} "
            f"| {sc('null', 'patch_file_hit_at_1')} | {mrr('retrieval')} | {mrr('lean_exact')} "
            f"| {sc('cold', 'test_target_hit_at_1')} | {sc('format', 'test_target_hit_at_1')} "
            f"| {sc('lean_exact', 'test_target_hit_at_1')} | {sc('retrieval', 'test_target_hit_at_1')} "
            f"| {sc('oracle', 'test_target_hit_at_1')} |"
        )
    lines.extend(
        [
            "",
            "## Validity Notes",
            "",
            "- Source-file labels come from the gold SWE-Bench patch diff.",
            "- Verification labels come from FAIL_TO_PASS tests (both pytest-style and unittest-style labels matched).",
            "- Retrieval relevance is a heuristic repo-family check, not a human label.",
            "- Null retrieval injects one deterministic non-repo-family card with the same prompt structure.",
            "- Empty omits the retrieval guidance block. Cold includes an explicit no-card block. Oracle injects a deterministic repo-family card as an upper-bound control.",
            "- format: card-shaped no-knowledge control with source-first ranking instructions; no retrieved skill content.",
            "- lean_exact: same prompt structure as format plus the retrieved skill card. The only variable vs format is card content.",
            "- retrieval: retrieved card injected, without source-first ranking instructions (legacy condition kept for backward compatibility).",
            "- The screened subset is selected by cold file/test MRR before retrieval is run; use only as a hard-cohort diagnostic, not an unbiased population estimate.",
            "- Random-file baseline samples repo-local files from all gold patch/test paths in this dataset; it is a label sanity baseline, not a prompted agent.",
            "- ERR/TIMEOUT cells are excluded from aggregate metrics.",
            "- This is still not a solve-rate benchmark; it is an attribution rung before full patch execution.",
            "- Plans are generated by Claude CLI in this first implementation, so model variance should be handled with repeated runs before public claims.",
            "",
        ]
    )
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dataset", type=Path, default=ROOT / "benchmarks/swe-bench-cl/data/swe-bench-cl-instances.jsonl")
    parser.add_argument("--corpus", type=Path, default=ROOT / "eval/skills/retrieval_eval_v1/merged_public_local_1000.jsonl")
    parser.add_argument("--supplemental-corpus", type=Path, action="append", default=[])
    parser.add_argument("--out-dir", type=Path, default=ROOT / "eval/patch_planning/v1")
    parser.add_argument("--ids", nargs="*", default=DEFAULT_IDS)
    parser.add_argument("--limit", type=int, default=6)
    parser.add_argument("--screen-size", type=int, default=0, help="Cold-screen this many balanced tasks and evaluate the hardest subset.")
    parser.add_argument("--screen-trace", type=Path, help="Reuse an existing cold screening JSON trace.")
    parser.add_argument("--claude-timeout", type=int, default=120)
    parser.add_argument("--model", type=str, default="claude-sonnet-4-6", help="Claude CLI model id (e.g. claude-sonnet-4-6).")
    args = parser.parse_args()
    args.out_dir.mkdir(parents=True, exist_ok=True)

    by_id = {row["instance_id"]: row for row in read_jsonl(args.dataset)}
    all_rows = list(by_id.values())
    file_universe = build_file_universe(all_rows)
    screen_rows: list[dict[str, Any]] = []
    cold_by_id: dict[str, dict[str, Any]] = {}
    if args.screen_trace:
        screen_rows = json.loads(args.screen_trace.read_text())
        cold_by_id = {row["instance_id"]: row["cold"]["plan"] for row in screen_rows}
    elif args.screen_size:
        candidates = balanced_sample(all_rows, args.screen_size)
        for row in candidates:
            cold_plan = plan_with_claude(row, "cold-screen", [], args.claude_timeout, args.model)
            score = score_task_plan(row, cold_plan)
            screen_rows.append(
                {
                    "instance_id": row["instance_id"],
                    "repo": row.get("repo"),
                    "patch_files": changed_files(row.get("patch") or ""),
                    "fail_tests": parse_tests(row.get("FAIL_TO_PASS")),
                    "cold": {"plan": cold_plan, "score": score},
                }
            )
            cold_by_id[row["instance_id"]] = cold_plan
            write_json(args.out_dir / "cold_screen_trace.json", screen_rows)
    if screen_rows:
        ranked = sorted(
            screen_rows,
            key=lambda r: (
                float(r["cold"]["score"].get("patch_file_mrr") or 0.0),
                float(r["cold"]["score"].get("test_target_mrr") or 0.0),
            ),
        )
        tasks = [by_id[r["instance_id"]] for r in ranked[: args.limit]]
    else:
        tasks = [by_id[i] for i in args.ids if i in by_id][: args.limit]
    if not tasks:
        raise SystemExit("no tasks selected")
    corpus_path, corpus_cards = prepare_corpus(args.corpus, args.supplemental_corpus, args.out_dir)
    traces = run_skill_retrieval(corpus_path, tasks, args.out_dir, 1)
    results = []
    for row in tasks:
        iid = row["instance_id"]
        patch_files = changed_files(row.get("patch") or "")
        fail_tests = parse_tests(row.get("FAIL_TO_PASS"))
        cards = (traces.get(iid) or {}).get("cards") or []
        null_cards = deterministic_null_card(corpus_cards, row.get("repo"), iid)
        oracle_cards = deterministic_oracle_card(corpus_cards, row.get("repo"), iid)

        # Run all conditions, catching errors per-cell so one failure doesn't abort the task.
        def run_condition(mode: str, cond_cards: list[dict[str, Any]], condition_key: str) -> dict[str, Any]:
            plan = plan_with_claude_logged(row, mode, cond_cards, args.claude_timeout, args.model, args.out_dir, condition_key)
            return {"plan": plan, "score": safe_score_plan(plan, patch_files, fail_tests)}

        empty_cell = run_condition("empty", [], "empty")
        cold_cell = {"plan": cold_by_id[iid], "score": safe_score_plan(cold_by_id[iid], patch_files, fail_tests)} if iid in cold_by_id else run_condition("cold", [], "cold")
        # format: source-first instructions, no real card content (format_only_card is a placeholder)
        format_cell = run_condition("format", format_only_card(iid), "format")
        # lean_exact: source-first instructions + retrieved card content
        lean_exact_cell = run_condition("lean_exact", cards, "lean_exact")
        # retrieval: legacy condition — retrieved card, no source-first instructions
        retrieval_cell = run_condition("retrieval", cards, "retrieval")
        null_cell = run_condition("null-retrieval", null_cards, "null")
        oracle_cell = run_condition("oracle-retrieval", oracle_cards, "oracle")

        result = {
            "instance_id": iid,
            "repo": row.get("repo"),
            "patch_files": patch_files,
            "fail_tests": fail_tests,
            "retrieved_cards": cards,
            "null_cards": null_cards,
            "oracle_cards": oracle_cards,
            "skill_relevance": skill_relevance(row.get("repo"), cards),
            "empty": empty_cell,
            "cold": cold_cell,
            "format": format_cell,
            "lean_exact": lean_exact_cell,
            "retrieval": retrieval_cell,
            "null": null_cell,
            "oracle": oracle_cell,
            "random_file": {"plan": {}, "score": random_plan_score(row, file_universe.get(row.get("repo") or "", []), patch_files, fail_tests)},
        }
        results.append(result)
        # Incremental write after each task so a crash doesn't lose work.
        write_json(args.out_dir / "patch_planning_trace.json", results)

    # Final write (also covers the last task, redundant but explicit).
    write_json(args.out_dir / "patch_planning_trace.json", results)
    write_json(
        args.out_dir / "run_metadata.json",
        {
            "model": args.model,
            "claude_timeout_s": args.claude_timeout,
            "task_count": len(results),
            "instance_ids": [r["instance_id"] for r in results],
            "dataset": str(args.dataset),
            "corpus": str(args.corpus),
            "supplemental_corpus": [str(p) for p in args.supplemental_corpus],
            "screen_size": args.screen_size,
            "limit": args.limit,
        },
    )
    (args.out_dir / "patch_planning_report.md").write_text(render(results, args.model) + "\n")
    print(args.out_dir / "patch_planning_report.md")
    return 0


# ---------------------------------------------------------------------------
# Self-test: run with --selftest to verify test_matches handles both styles.
# ---------------------------------------------------------------------------

def _run_selftest() -> None:
    import sys

    failures: list[str] = []

    def check(label: str, command: str, expected: bool) -> None:
        result = test_matches(command, label)
        if result != expected:
            failures.append(f"FAIL test_matches({label!r}, {command!r}) -> {result}, want {expected}")

    # Unittest-style: test_name (module.ClassName)
    ut_label = "test_ssl_certificate (dbshell.test_postgresql.PostgreSqlDbshellCommandTestCase)"
    check(ut_label, "python -m pytest dbshell/test_postgresql.py", True)           # module_path hit
    check(ut_label, "python -m pytest -k test_ssl_certificate", True)              # test_name hit
    check(ut_label, "python -m pytest -k PostgreSqlDbshellCommandTestCase", True)  # class_name hit
    check(ut_label, "python -m pytest dbshell/test_postgresql.py::PostgreSqlDbshellCommandTestCase::test_ssl_certificate", True)  # pytest_node hit
    check(ut_label, "python -m pytest some_unrelated_test.py", False)              # no hit

    # Pytest-style: path::Class::test
    py_label = "tests/test_foo.py::TestBar::test_baz"
    check(py_label, "pytest tests/test_foo.py::TestBar::test_baz", True)           # exact label hit
    check(py_label, "pytest tests/test_foo.py", True)                              # module_path hit
    check(py_label, "pytest -k test_baz", True)                                    # test_name hit
    check(py_label, "pytest -k TestBar", True)                                     # class_name hit
    check(py_label, "pytest unrelated.py", False)                                  # no hit

    # Unittest-style: test_name (simple.ClassName) — no sub-module
    simple_ut = "test_foo (mymodule.MyClass)"
    check(simple_ut, "python -m pytest mymodule.py::MyClass::test_foo", True)
    check(simple_ut, "pytest -k test_foo", True)
    check(simple_ut, "pytest -k MyClass", True)
    check(simple_ut, "pytest unrelated.py", False)

    if failures:
        for f in failures:
            print(f, file=sys.stderr)
        raise SystemExit(f"selftest: {len(failures)} failure(s)")
    print(f"selftest: all {10 + 4 + 4} checks passed")


if __name__ == "__main__":
    import sys
    if "--selftest" in sys.argv:
        _run_selftest()
    else:
        raise SystemExit(main())
