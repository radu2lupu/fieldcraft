# CH-0006: First-class Grok agent support

Date: 2026-07-29

## What changed

Fieldcraft install now wires Grok Build the same way as Claude Code and Codex:

- Instructions: `install/instructions/grok.md` → `~/.grok/rules/fieldcraft.md`
- Hooks: same SessionStart/Stop fragments merged into `~/.grok/hooks/fieldcraft.json`
- Hook scripts accept Grok camelCase payload fields (`sessionId`, `stopHookActive`, …)
- Stop hook recovers transcripts under `~/.grok/sessions/**/chat_history.jsonl`
- Distiller option `grok` (`grok -p --always-approve`); auto prefers codex, then grok, then claude
- Nested distillers set `FIELDCRAFT_HOOK_SKIP=1` so hooks do not re-enter
- Dotfiles install re-injects Grok rules/hooks so daily sync cannot drop them

## Why

Grok was only partially covered via Claude compatibility. SessionStart stdout is ignored on Grok, so a durable home rule is required for the agent contract.

## How to verify

1. `bash install.sh` (or agent-only reinstall of hooks/instructions)
2. Confirm `~/.grok/rules/fieldcraft.md` and `~/.grok/hooks/fieldcraft.json` exist
3. Simulate SessionStart with Grok payload; hook exits 0 and writes `~/.fieldcraft/state/current-session.json`
4. New Grok session should load the fieldcraft rule and run SessionStart/Stop hooks
