# CH-0010: Per-prompt UserPromptSubmit hook with a Jev recall gate

**Date**: 2026-11-01
**Type**: implementation

## What changed

New subcommand `fieldcraft hook prompt-submit` (`crates/fieldcraft-cli/src/prompt_hook.rs`),
meant for a Claude Code UserPromptSubmit hook. It reads the hook JSON on stdin and either prints
`{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":...}}` with at most
three lessons (each as `lesson:<id>` plus its text) or prints nothing. It always exits 0.

- Gate: asks Jev (TypeSafe System One, `jev-latest`, one `noul` question "would earlier-session
  notes change what the engineer does") for a probability. `FIELDCRAFT_PROMPT_GATE` selects
  `shadow` (default: always retrieve, record the probability), `enforce` (skip retrieval when
  p < `FIELDCRAFT_PROMPT_GATE_THRESHOLD`, default 0.4) or `off` (never call Jev). Mode and
  threshold are parsed in `GateConfig::parse`.
- Key: macOS Keychain service `fieldcraft.jev-api-key`, else `FIELDCRAFT_JEV_API_KEY`. No key,
  timeout (1.5 s) or any error fails open: retrieve as if p were high.
- Retrieval: the prompt plus extracted paths and identifiers, through the same code path as
  `fieldcraft query` (remote proxy, `retrieval_log` row tagged with the session id). To share it,
  `cmd_query` was split into `query_lessons` (returns lessons) and a printing wrapper; output of
  `fieldcraft query` is unchanged.
- Dedupe: lessons already injected in this session are skipped (state file
  `~/.fieldcraft/state/prompt-hook-<session>.json`, files older than 7 days pruned).
- Budget: 3 s wall time for the whole hook; past that nothing is emitted.
- Log: one line per invocation in `~/.fieldcraft/state/prompt-gate.jsonl` (ts, session_id,
  project, mode, jev_p, jev_ms, decision, injected_ids, retrieval_ms, prompt_chars, error), for
  joining to `lesson_outcome_events` by session id. The prompt text is not logged.
- Skipped without a log line: `~/.fieldcraft/DISABLED`, `FIELDCRAFT_HOOK_SKIP=1`,
  `FIELDCRAFT_DISABLED=1` in the env file, empty prompt, slash command.

## Not done

The hook is not installed into `~/.claude/settings.json` and `install/` does not register it yet.
Test-only overrides: `FIELDCRAFT_JEV_API_URL`, `FIELDCRAFT_JEV_KEYCHAIN_SERVICE`.

## Verified

Unit tests cover mode parsing, Jev response parsing (including malformed), dedupe, output shape,
and the fail-open paths; the HTTP client is tested against a local socket server. A smoke run of
the built binary retrieved three lessons with Jev p=0.72, wrote a `retrieval_log` row on the
server, and failed open with a bogus key (401) and with no key.
