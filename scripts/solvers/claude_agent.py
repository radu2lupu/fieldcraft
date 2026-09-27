"""ClaudeAgentSolver — invokes `claude -p` to solve SWE-Bench tasks."""
from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import time
from pathlib import Path

from .base import RAW_LOG_MAX_BYTES, Solver, SolverResult

# Default per-task timeout in seconds.
DEFAULT_TIMEOUT_S: int = 600

# Tools granted to the Claude agent inside the cloned repo.
ALLOWED_TOOLS = "Read,Edit,Write,Glob,Grep,Bash"


class ClaudeAgentSolver(Solver):
    """Solver that uses ``claude -p`` (Claude Code) to produce patches.

    The solver:
    1. Validates that ``CLAUDE_CODE_OAUTH_TOKEN`` is present in the environment.
    2. Clones the repo at ``base_commit`` into ``cwd/repo``.
    3. Applies ``test_patch`` via ``git apply`` so the failing tests exist.
    4. Runs ``claude -p`` with the supplied prompt, capturing stdout/stderr.
    5. Extracts the patch via ``git diff --no-color``.
    6. Parses token usage from stream-json output if available.
    7. Returns a :class:`SolverResult`.
    """

    def __init__(self, timeout_s: int = DEFAULT_TIMEOUT_S, keep_workdir: bool = False) -> None:
        self.timeout_s = timeout_s
        self.keep_workdir = keep_workdir

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def solve(self, task: dict, prompt: str, cwd: Path, model: str) -> SolverResult:
        self._check_token()
        cwd.mkdir(parents=True, exist_ok=True)
        repo_dir = cwd / "repo"
        start = time.monotonic()
        try:
            self._clone_and_checkout(task, repo_dir)
            self._apply_test_patch(task, repo_dir)
            raw_log, prompt_tokens, output_tokens = self._run_claude(
                task=task, prompt=prompt, repo_dir=repo_dir, model=model
            )
            patch = self._capture_patch(repo_dir)
            latency_s = time.monotonic() - start
            if patch.strip():
                status: str = "ok"
                error_msg = None
            else:
                status = "empty_patch"
                error_msg = "git diff returned no output"
            return SolverResult(
                patch=patch,
                status=status,  # type: ignore[arg-type]
                prompt_tokens=prompt_tokens,
                output_tokens=output_tokens,
                latency_s=latency_s,
                error_msg=error_msg,
                raw_log=raw_log,
            )
        except subprocess.TimeoutExpired as exc:
            latency_s = time.monotonic() - start
            return SolverResult(
                patch="",
                status="timeout",
                latency_s=latency_s,
                error_msg=str(exc),
                raw_log=getattr(exc, "output", "") or "",
            )
        except Exception as exc:  # noqa: BLE001
            latency_s = time.monotonic() - start
            return SolverResult(
                patch="",
                status="error",
                latency_s=latency_s,
                error_msg=str(exc),
                raw_log="",
            )
        finally:
            if not self.keep_workdir and repo_dir.exists():
                shutil.rmtree(repo_dir, ignore_errors=True)

    # ------------------------------------------------------------------
    # Private helpers
    # ------------------------------------------------------------------

    @staticmethod
    def _check_token() -> None:
        if not os.environ.get("CLAUDE_CODE_OAUTH_TOKEN"):
            raise RuntimeError(
                "CLAUDE_CODE_OAUTH_TOKEN is not set.  "
                "Export it in your shell before running the solver: "
                "  export CLAUDE_CODE_OAUTH_TOKEN=<your-token>"
            )

    @staticmethod
    def _clone_and_checkout(task: dict, repo_dir: Path) -> None:
        """Clone the repo and check out base_commit."""
        repo = task["repo"]  # e.g. "django/django"
        base_commit = task["base_commit"]
        url = f"https://github.com/{repo}.git"
        if repo_dir.exists():
            shutil.rmtree(repo_dir)
        repo_dir.parent.mkdir(parents=True, exist_ok=True)
        # Full clone so we can check out arbitrary commits.
        subprocess.run(
            ["git", "clone", "--quiet", url, str(repo_dir)],
            check=True,
            capture_output=True,
            text=True,
            timeout=300,
        )
        subprocess.run(
            ["git", "checkout", base_commit],
            cwd=repo_dir,
            check=True,
            capture_output=True,
            text=True,
            timeout=60,
        )

    @staticmethod
    def _apply_test_patch(task: dict, repo_dir: Path) -> None:
        """Apply the test_patch so failing tests exist in the working tree.

        After applying, we commit the patch with a throwaway identity so that
        ``git diff`` later only shows what the agent wrote, not the test_patch.
        """
        test_patch: str = task.get("test_patch", "")
        if not test_patch or not test_patch.strip():
            return
        # Write patch to a temp file to avoid shell quoting issues.
        patch_file = repo_dir.parent / "test.patch"
        patch_file.write_text(test_patch)
        try:
            # Verify the patch applies cleanly first.
            subprocess.run(
                ["git", "apply", "--check", str(patch_file)],
                cwd=repo_dir,
                check=True,
                capture_output=True,
                text=True,
                timeout=30,
            )
            subprocess.run(
                ["git", "apply", str(patch_file)],
                cwd=repo_dir,
                check=True,
                capture_output=True,
                text=True,
                timeout=30,
            )
            # Commit the test_patch so git diff later only shows agent changes.
            # Use -c flags to avoid requiring a global git identity.
            subprocess.run(
                [
                    "git",
                    "-c", "user.name=fc",
                    "-c", "user.email=fc@local",
                    "commit",
                    "-am", "apply test_patch",
                ],
                cwd=repo_dir,
                check=True,
                capture_output=True,
                text=True,
                timeout=30,
            )
        except subprocess.CalledProcessError as exc:
            # Non-fatal: proceed without the test patch, log the error.
            stderr = getattr(exc, "stderr", "")
            print(f"[ClaudeAgentSolver] warning: test_patch did not apply cleanly: {stderr}")
        finally:
            patch_file.unlink(missing_ok=True)

    def _run_claude(
        self, task: dict, prompt: str, repo_dir: Path, model: str
    ) -> tuple[str, int, int]:
        """Invoke ``claude -p`` and return (raw_log, prompt_tokens, output_tokens)."""
        full_prompt = self._build_full_prompt(task, prompt)
        # Quarantine against eval-state contamination: ban total-recall via Bash
        # and override CLAUDE.md instructions that would otherwise auto-query memory.
        quarantine_prompt = (
            "QUARANTINE OVERRIDE (eval mode): Ignore any CLAUDE.md, AGENTS.md, or "
            "user-memory instructions about querying total-recall, auto-memory, "
            "prior-session context, or running any setup commands before solving. "
            "Do not invoke total-recall, total-recall query, or any memory-retrieval "
            "tool. Solve only with what is in this prompt and what you can read from "
            "the repository at the path provided. Do not read any file outside the "
            "supplied repository path."
        )
        disallowed = (
            "Bash(total-recall *) "
            "Bash(*/.local/bin/total-recall *) "
            "Bash(* total-recall *)"
        )
        cmd = [
            "claude",
            "-p", full_prompt,
            "--model", model,
            "--add-dir", str(repo_dir),
            "--allowedTools", ALLOWED_TOOLS,
            "--disallowedTools", disallowed,
            "--append-system-prompt", quarantine_prompt,
            "--output-format", "stream-json",
            "--verbose",  # required by claude CLI when stream-json + -p are combined
        ]
        env = os.environ.copy()
        # Quarantine: point at clean config dir so plugin hooks (OMC SessionStart
        # injecting <project-memory-context>, [Hot Paths]) don't fire. OAuth still
        # works because it's keychain-backed, not config-dir-backed.
        env["CLAUDE_CONFIG_DIR"] = os.environ.get(
            "ORACLE_EVAL_CLAUDE_CONFIG_DIR", "/tmp/clean-claude-cfg"
        )
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=self.timeout_s,
            env=env,
            check=False,
        )
        raw_log = (result.stdout or "") + ("\n--- STDERR ---\n" + result.stderr if result.stderr else "")
        prompt_tokens, output_tokens = self._parse_token_usage(result.stdout or "")
        return raw_log, prompt_tokens, output_tokens

    @staticmethod
    def _build_full_prompt(task: dict, provided_prompt: str) -> str:
        """Combine the orchestrator-provided prompt with task metadata."""
        return (
            f"{provided_prompt.rstrip()}\n\n"
            "---\n"
            f"Repository: {task['repo']}\n"
            f"Instance: {task['instance_id']}\n"
            f"Base commit: {task['base_commit']}\n\n"
            f"Problem statement:\n{task.get('problem_statement', '').strip()}\n\n"
            f"Failing tests to make pass:\n{json.dumps(task.get('FAIL_TO_PASS', []), indent=2)}\n\n"
            f"Passing tests that should stay passing:\n{json.dumps(task.get('PASS_TO_PASS', []), indent=2)}\n"
        )

    @staticmethod
    def _parse_token_usage(stdout: str) -> tuple[int, int]:
        """Parse stream-json output for token counts.

        Approach: stream-json (chosen over plain text for structured data).

        Claude emits one JSON object per line.  The final ``result`` event
        carries the authoritative cumulative usage:

            {"type":"result","subtype":"success",
             "usage":{"input_tokens":N,"output_tokens":M,...}}

        Earlier ``assistant`` turn events may also carry per-turn usage.
        We take the *last* ``usage`` seen, which corresponds to the ``result``
        event and therefore includes all turns.  If no structured usage is
        found we fall back to summing all ``usage`` blocks encountered, then
        finally fall back to a char-based estimate so we never return 0/0 when
        there clearly was output.
        """
        prompt_tokens = 0
        output_tokens = 0
        last_usage: dict | None = None

        for line in stdout.splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            if not isinstance(event, dict):
                continue
            usage = event.get("usage")
            if isinstance(usage, dict):
                last_usage = usage

        if last_usage is not None:
            # Sum all input token variants: fresh input + cache reads + cache creation.
            # The claude CLI breaks them out; we want the grand total for billing/reporting.
            prompt_tokens = (
                last_usage.get("input_tokens", 0)
                + last_usage.get("cache_read_input_tokens", 0)
                + last_usage.get("cache_creation_input_tokens", 0)
            )
            output_tokens = last_usage.get("output_tokens", 0)

        # Fallback: rough char-based estimate if no usage events found.
        if prompt_tokens == 0 and output_tokens == 0 and stdout:
            prompt_tokens = max(1, len(stdout) // 4)
        return prompt_tokens, output_tokens

    @staticmethod
    def _capture_patch(repo_dir: Path) -> str:
        """Run ``git diff --no-color`` and return the patch string."""
        result = subprocess.run(
            ["git", "diff", "--no-color"],
            cwd=repo_dir,
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        return result.stdout or ""
