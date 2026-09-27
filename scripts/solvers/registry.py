"""Solver registry — maps solver name strings to Solver instances."""
from __future__ import annotations

from .base import Solver


def get_solver(name: str, **kwargs) -> Solver:
    """Return a :class:`~solvers.base.Solver` instance for *name*.

    Supported names:
        ``"claude"`` — :class:`~solvers.claude_agent.ClaudeAgentSolver`
        ``"codex"``  — :class:`~solvers.codex.CodexSolver` (stub, raises)

    Raises:
        ValueError: if *name* is not a recognised solver.
    """
    if name == "claude":
        from .claude_agent import ClaudeAgentSolver  # lazy import

        return ClaudeAgentSolver()
    if name == "codex":
        from .codex import CodexSolver  # lazy import

        return CodexSolver(reasoning=kwargs.get("reasoning"))
    if name == "anthropic":
        from .anthropic_api import AnthropicAPISolver  # lazy import

        return AnthropicAPISolver()
    raise ValueError(
        f"Unknown solver {name!r}.  Available solvers: 'claude', 'codex', 'anthropic'."
    )
