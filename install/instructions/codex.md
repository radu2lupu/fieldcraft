<!-- fieldcraft:start -->
## Fieldcraft — Memory & Learning System

Fieldcraft is installed on this machine and provides cross-session memory for
coding work. Retrieved lessons are things you remember from prior sessions.
Retrievals are logged, and each session's outcomes (cited, contradicted,
ignored) are recorded against the lessons it was served. **Use it.**

Fieldcraft is **CLI-only**. There is no MCP server. All commands are issued
via the `fieldcraft` binary in your shell (Bash tool). When
`FIELDCRAFT_SERVER_URL` is set (sourced from `~/.fieldcraft/env`), every
command transparently proxies to the shared remote server.

### MANDATORY behaviors

1. **Query at session start.** For non-trivial coding work, your first action
   should be `fieldcraft query "<topic>"` — or rely on the SessionStart hook
   that prompts you.

2. **Cite lesson IDs inline.** When you act on a retrieved lesson — applying
   its advice, following its procedure, quoting it — include the id in your
   reasoning text. Format: `lesson:<id>` (e.g. `lesson:abc12345`). This is
   the cheapest possible signal: it lets the trace analyzer auto-detect
   confirmed retrievals without manual feedback calls.

3. **Write lessons at the end of non-trivial work.** Call
   `fieldcraft write --kind <kind> "<summary>"` when you make a non-obvious
   design decision, fix a tricky bug, surface a constraint a future agent
   would benefit from, or learn something surprising. **Skip** routine work
   — typo fixes, dependency bumps, formatting, anything a future agent
   doesn't need to remember.

4. **Provide feedback on bad lessons.** If a retrieved lesson was wrong or
   misleading, call `fieldcraft feedback <id> --outcome contradicted
   --note "<why>"`. Don't let stale or incorrect lessons silently pollute
   future retrievals.

### CLI commands

| Command | Purpose |
|---|---|
| `fieldcraft query "<topic>"` | Remember top relevant prior lessons for current task |
| `fieldcraft write --kind <claim\|procedure\|episode\|directive\|context> "<summary>"` | Record a new lesson |
| `fieldcraft feedback <id> --outcome <confirmed\|ignored\|contradicted> [--note "<why>"]` | Record outcome on a previously retrieved lesson |
| `fieldcraft stats` | Workhorses, noise candidates, session lift (humans) |
| `fieldcraft promote [--id] [--target skill\|agents-md] [--apply]` | Preview or write a workhorse lesson as SKILL.md / AGENTS.md (humans; dry-run default) |
| `fieldcraft session <id>` | Full per-session log with confidence deltas (humans) |

Run `fieldcraft --help` for the full list. All commands honor
`FIELDCRAFT_SERVER_URL` from `~/.fieldcraft/env` — no special "remote" flag
needed.

### Quality bar for `fieldcraft write`

A good lesson is: project-specific, non-obvious, durable. If a future agent
would already figure it out from the code or compiler output, **don't write
it**. If you'd want a future agent to know this even after the surrounding
code is gone, **do**.
<!-- fieldcraft:end -->
