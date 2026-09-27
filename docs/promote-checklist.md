# Promote checklist — lesson → skill / AGENTS.md

`fieldcraft promote` turns a **high-confirm workhorse lesson** into a formal
local artifact. Default is a dry-run preview. Nothing is written unless you
pass `--apply` with a single `--id`. Hooks, cron, and `lifecycle` must never
call `--apply`.

Lessons stay in Fieldcraft’s own store. Promotion copies them; it
does not migrate to teamai or delete closed-loop history.

## When to promote vs leave as retrieval

Promote when all of these are true:

- The lesson is a repeatable **procedure** (or a durable **directive**).
- It already behaves as a workhorse in `fieldcraft stats`: confirmed often,
  rarely ignored, never contradicted.
- Future agents would benefit from a **named skill** or a **standing
  AGENTS.md rule**, not just fuzzy retrieval.
- A human or agent has read the lesson and agrees the wording is still true.

Leave it as a retrieval lesson when:

- It is a one-off **episode**, a narrow **claim**, or **context** that will
  go stale.
- Confirm signal is thin, noisy, or mixed with contradictions.
- You only want it surfaced for similar tasks (retrieval already does that).
- You have not reviewed the text. There is no silent auto-promote.

## Threshold defaults

Aligned with `fieldcraft stats` workhorses, plus a contradiction gate:

| Rule | Default | Flag / env |
|---|---|---|
| Kind | `procedure`, `directive` | `--kinds` |
| Confirmed count | ≥ 3 | `--min-confirmed` / `FIELDCRAFT_PROMOTE_MIN_CONFIRMED` |
| Confirm rate | ≥ 0.5 | `--min-confirm-rate` / `FIELDCRAFT_PROMOTE_MIN_CONFIRM_RATE` |
| Contradicted | 0 | `--max-contradicted` / `FIELDCRAFT_PROMOTE_MAX_CONTRADICTED` |
| Excluded | ghosts (no outcomes), noise (`total ≥ 5` and rate `< 0.2`), archive / superseded / retired, already promoted | `--include-promoted`, `--force` |

`--force` is a human override after review, not a hook default.

## Review steps before `--apply`

1. `fieldcraft stats` — confirm the lesson is a workhorse, not noise/ghost.
2. `fieldcraft promote` — list eligible candidates (writes nothing).
3. `fieldcraft promote --id <lesson> --target skill` (or `--target agents-md`)
   — read the artifact preview. Check trigger, steps, and “invalid if”.
4. Choose the target:
   - **skill** — reusable workflow. Writes
     `<catalog>/promoted-skills/{project}/{slug}/SKILL.md`
     (override with `--dest`).
   - **agents-md** — standing project rule. Appends a marked block to
     `./AGENTS.md` (override with `--dest`).
5. Only then: add `--apply`. One lesson per invocation. Bulk `--apply`
   is refused.

## What happens to the original lesson

- It is **not deleted**, archived, or superseded.
- Closed-loop history (`lesson_outcome_events`) is unchanged.
- A `knowledge_events` row with `event_type = lesson_promoted` records
  source lesson id, feedback counts at promotion time, target, and
  destination path.
- Default candidate lists skip already-promoted lessons
  (`--include-promoted` to see them).
- Retrieval continues to serve the original lesson.

## Commands

```bash
fieldcraft promote --help
fieldcraft promote --data-dir ~/.fieldcraft          # list (dry-run)
fieldcraft promote --id <lesson> --target skill      # preview SKILL.md
fieldcraft promote --id <lesson> --target skill --apply
fieldcraft promote --id <lesson> --target agents-md --apply --dest ./AGENTS.md
```
