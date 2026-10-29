# CH-0009: Claude distiller fix and re-enable

**Date**: 2026-10-29
**Type**: implementation

## What changed

`install/bin-hooks/stop-feedback.sh`, `claude` distiller branch:

- The prompt moved before `--add-dir`. `--add-dir` is variadic and swallowed a trailing
  prompt, so every run exited with "Input must be provided" and no lesson was ever written.
- The hard-coded `--max-budget-usd 0.10` is now `FIELDCRAFT_DISTILL_BUDGET_USD`, default 0.30.
  A ~3 MB transcript still exceeds it; typical sessions fit.

`FIELDCRAFT_DISTILLER` in `~/.fieldcraft/env` went from `none` (off since at least 2026-08-03,
Codex subscription ended 2026-10-22) to `claude`.

## Verified

A forced run through the real hook on a live transcript wrote a lesson to the server.
Large transcripts are not distilled; that limit is known, not fixed.
