"""Pluggable solver abstraction for SWE-Bench solve-rate evaluation."""
from .base import Solver, SolverResult
from .registry import get_solver

__all__ = ["Solver", "SolverResult", "get_solver"]
