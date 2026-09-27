"""CodexSolver — uses `codex exec` to solve SWE-Bench tasks."""
from __future__ import annotations

import json
import shutil
import subprocess
import tempfile
import time
from pathlib import Path

from .base import Solver, SolverResult
from .claude_agent import ClaudeAgentSolver

DEFAULT_TIMEOUT_S: int = 600


class CodexSolver(Solver):
    """Solver that uses ``codex exec`` to produce patches.

    The solver:
    1. Clones the repo at ``base_commit`` into ``cwd/repo``.
    2. Applies ``test_patch`` so the failing tests exist.
    3. Pipes the prompt to ``codex exec`` via stdin with ``--cd`` pointing at the repo.
    4. Extracts the patch via ``git diff``.
    5. Parses token usage from the events JSONL written to stdout.
    """

    def __init__(self, timeout_s: int = DEFAULT_TIMEOUT_S, keep_workdir: bool = False, reasoning: str | None = None) -> None:
        self.timeout_s = timeout_s
        self.keep_workdir = keep_workdir
        self.reasoning = reasoning

    def solve(self, task: dict, prompt: str, cwd: Path, model: str) -> SolverResult:
        if not shutil.which("codex"):
            return SolverResult(
                patch="", status="error",
                error_msg="codex CLI not found in PATH.",
            )

        cwd.mkdir(parents=True, exist_ok=True)
        repo_dir = cwd / "repo"
        start = time.monotonic()

        try:
            ClaudeAgentSolver._clone_and_checkout(task, repo_dir)
            ClaudeAgentSolver._apply_test_patch(task, repo_dir)

            raw_log, prompt_tokens, output_tokens = self._run_codex(
                prompt=prompt, repo_dir=repo_dir, model=model, reasoning=self.reasoning,
            )

            patch = ClaudeAgentSolver._capture_patch(repo_dir)
            latency_s = time.monotonic() - start

            if patch.strip():
                return SolverResult(
                    patch=patch, status="ok",
                    prompt_tokens=prompt_tokens, output_tokens=output_tokens,
                    latency_s=latency_s, raw_log=raw_log,
                )
            return SolverResult(
                patch="", status="empty_patch",
                prompt_tokens=prompt_tokens, output_tokens=output_tokens,
                latency_s=latency_s, raw_log=raw_log,
                error_msg="git diff returned no output",
            )

        except subprocess.TimeoutExpired as exc:
            return SolverResult(
                patch="", status="timeout", latency_s=time.monotonic() - start,
                error_msg=str(exc),
            )
        except Exception as exc:  # noqa: BLE001
            return SolverResult(
                patch="", status="error", latency_s=time.monotonic() - start,
                error_msg=str(exc),
            )
        finally:
            if not self.keep_workdir and repo_dir.exists():
                shutil.rmtree(repo_dir, ignore_errors=True)

    def _run_codex(
        self, prompt: str, repo_dir: Path, model: str, reasoning: str | None = None,
    ) -> tuple[str, int, int]:
        with tempfile.TemporaryDirectory() as tmp:
            events_path = Path(tmp) / "events.jsonl"
            final_path = Path(tmp) / "final.txt"

            cmd = [
                "codex", "exec",
                "--json",
                "--output-last-message", str(final_path),
                "-m", model,
                "--cd", str(repo_dir),
                "--sandbox", "danger-full-access",
            ]
            if reasoning:
                cmd.extend(["-c", f'reasoning_effort="{reasoning}"'])
            cmd.append("-")

            with events_path.open("w") as events_out:
                result = subprocess.run(
                    cmd,
                    input=prompt,
                    stdout=events_out,
                    stderr=subprocess.STDOUT,
                    text=True,
                    timeout=self.timeout_s,
                    check=False,
                )

            raw_log = events_path.read_text(errors="replace")
            if final_path.exists():
                raw_log += "\n[final]\n" + final_path.read_text(errors="replace")

            prompt_tokens, output_tokens = _parse_codex_usage(raw_log)
            return raw_log, prompt_tokens, output_tokens


def _parse_codex_usage(events_jsonl: str) -> tuple[int, int]:
    input_tokens = 0
    output_tokens = 0
    for line in events_jsonl.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(event, dict):
            continue
        usage = event.get("usage") or {}
        if isinstance(usage, dict):
            input_tokens += usage.get("input_tokens", 0) + usage.get("cached_input_tokens", 0)
            output_tokens += usage.get("output_tokens", 0)
            continue
        input_tokens += event.get("input_tokens", 0) + event.get("cached_input_tokens", 0)
        output_tokens += event.get("output_tokens", 0)
    return input_tokens, output_tokens
