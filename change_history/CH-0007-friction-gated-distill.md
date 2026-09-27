# CH-0007: Friction-gated Stop-hook distill

**Date**: 2026-09-07
**Type**: implementation

## What changed

The Stop-hook background distiller no longer spends an LLM turn on every
quiet session. A cheap local scorer (`fieldcraft-extract::friction` +
`fieldcraft distill-gate`) reads Claude Code / Codex / Grok JSONL and
counts real pain markers the formats actually emit:

- user interrupts / `turn_aborted`
- permission denials / "user rejected" / tool-use rejected
- short human corrections (`that's wrong`, `I said`, `try again`, `undo`)
- exec tool failures (`is_error`, non-zero `exit_code`, compiler/test fail)
- retry of the same failed command/tool

Quiet happy-path sessions score near zero. Default threshold is 3
(`FIELDCRAFT_DISTILL_MIN_FRICTION`). Isolated one-off test failures stay
below the gate; interrupts, rejections, or correction+retry cross it.

The Stop hook (`install/bin-hooks/stop-feedback.sh`) still always:

1. appends the session-end marker
2. runs `fieldcraft trace-analyze` (feedback loop does not depend on distill)

Then, before spawning `codex` / `grok` / `claude`, it calls
`fieldcraft distill-gate` and logs a grepable line to
`~/.fieldcraft/state/distill.log`:

```
[fieldcraft-stop-hook] decision=skip score=1 threshold=3 reasons=tool_failure:1
[fieldcraft-stop-hook] decision=run score=8 threshold=3 reasons=interrupt:1,user_correction:1,tool_failure:1,retry:1
```

Escape hatches:

- `FIELDCRAFT_DISTILL_FORCE=1` — always spawn (if a distiller is selected)
- `FIELDCRAFT_DISTILL_MIN_FRICTION=0` — disable the score gate
- missing / old CLI with no `distill-gate` subcommand — fail-open (old behavior)

## Why

The distiller prompt already asks the LLM to write at most one lesson, but
quiet sessions still burn budget and produce junk claims that later waste
feedback bandwidth. The gate is local and deterministic so the feedback
loop stays intact.

## How to verify

```bash
# Scoring (offline fixtures; no network)
cargo test -p fieldcraft-extract friction -- --nocapture

# Gate helper on the same fixtures
cargo run -q -p fieldcraft-cli -- distill-gate \
  --transcript crates/fieldcraft-extract/tests/fixtures/quiet_claude.jsonl
# expect: decision=skip score=0 threshold=3 reasons=none

cargo run -q -p fieldcraft-cli -- distill-gate \
  --transcript crates/fieldcraft-extract/tests/fixtures/painful_claude.jsonl
# expect: decision=run  and reasons= containing interrupt/rejection/user_correction

cargo run -q -p fieldcraft-cli -- distill-gate \
  --transcript crates/fieldcraft-extract/tests/fixtures/quiet_claude.jsonl \
  --force
# expect: decision=run score=forced reasons=FIELDCRAFT_DISTILL_FORCE

# After a real Stop, grep the hook log
grep -E 'decision=(run|skip)' ~/.fieldcraft/state/distill.log | tail
```

## Affects

- ARCHITECTURE: §6 Session-end distiller — friction gate + env knobs
- ROADMAP: Phase 5 note — write-side distill is now gated
- README: CLI + code map
- docs/OPERATIONS.md: Stop-hook step
- install.sh `~/.fieldcraft/env` comments (`FIELDCRAFT_DISTILL_MIN_FRICTION`, `FIELDCRAFT_DISTILL_FORCE`)
