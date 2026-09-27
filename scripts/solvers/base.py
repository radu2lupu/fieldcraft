"""Base solver ABC and SolverResult dataclass."""
from __future__ import annotations

import abc
from dataclasses import dataclass, field
from pathlib import Path
from typing import Literal

RAW_LOG_MAX_BYTES = 50 * 1024  # 50 KB


@dataclass
class SolverResult:
    patch: str
    status: Literal["ok", "timeout", "error", "empty_patch"]
    prompt_tokens: int = 0
    output_tokens: int = 0
    latency_s: float = 0.0
    error_msg: str | None = None
    raw_log: str = ""

    def __post_init__(self) -> None:
        # Truncate raw_log to 50 KB to keep it serialisable.
        if len(self.raw_log.encode("utf-8", errors="replace")) > RAW_LOG_MAX_BYTES:
            truncated = self.raw_log.encode("utf-8", errors="replace")[:RAW_LOG_MAX_BYTES]
            self.raw_log = truncated.decode("utf-8", errors="replace") + "\n[...truncated to 50 KB...]"


class Solver(abc.ABC):
    """Abstract base class for task solvers.

    A solver accepts a SWE-Bench task dict, a prompt string (already including
    any skill/memory context injected by the orchestrator), a working directory,
    and a model identifier.  It returns a SolverResult describing the outcome.
    """

    @abc.abstractmethod
    def solve(
        self,
        task: dict,
        prompt: str,
        cwd: Path,
        model: str,
    ) -> SolverResult:
        """Run the solver against *task* and return a SolverResult.

        Args:
            task: SWE-Bench CL row dict.  Expected keys: ``instance_id``,
                ``repo``, ``base_commit``, ``problem_statement``,
                ``test_patch``, ``FAIL_TO_PASS``, ``PASS_TO_PASS``.
            prompt: Full prompt string (skill/memory context already merged in
                by the orchestrator).
            cwd: Per-task working directory.  The solver should clone the repo
                into a subdirectory of this path, e.g. ``cwd / "repo"``.
            model: Model identifier string to pass to the underlying agent.

        Returns:
            A :class:`SolverResult` describing the patch, token usage, timing,
            and any error.
        """
