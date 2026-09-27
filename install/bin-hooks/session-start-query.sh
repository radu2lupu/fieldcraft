#!/usr/bin/env bash
# Fieldcraft SessionStart hook (standalone install version).
#
# Reminds the agent to query fieldcraft before doing anything else.
# Works for Claude Code, Codex, and Grok (Claude/Grok hook JSON on stdin).
# Silent if fieldcraft isn't installed (so a stale hook doesn't block sessions).
#
# Note: Grok ignores SessionStart stdout (cannot inject additionalContext).
# Grok agents must follow ~/.grok/rules/fieldcraft.md and call fieldcraft query.

set -e

# Skip nested distiller sessions. For disable, honor durable on-disk markers only
# (ambient FIELDCRAFT_DISABLED from a prior shell can linger and would no-op hooks).
if [ "${FIELDCRAFT_HOOK_SKIP:-}" = "1" ]; then
  exit 0
fi
if [ -f "$HOME/.fieldcraft/DISABLED" ]; then
  exit 0
fi
if [ -f "$HOME/.fieldcraft/env" ]; then
  # shellcheck disable=SC1090
  unset FIELDCRAFT_DISABLED
  set -a
  . "$HOME/.fieldcraft/env" 2>/dev/null || true
  set +a
fi
if [ "${FIELDCRAFT_DISABLED:-}" = "1" ]; then
  exit 0
fi

INPUT=$(cat)

# Claude/Codex use snake_case; Grok uses camelCase.
SESSION_ID="$(echo "$INPUT" | jq -r '.session_id // .sessionId // ""' 2>/dev/null)"
CWD="$(echo "$INPUT" | jq -r '.cwd // .workspaceRoot // ""' 2>/dev/null)"

STATE_DIR="$HOME/.fieldcraft/state"
mkdir -p "$STATE_DIR"
if [ -n "$SESSION_ID" ]; then
  TMP="$(mktemp "$STATE_DIR/.current-session.XXXXXX" 2>/dev/null || true)"
  if [ -n "$TMP" ]; then
    jq -nc \
    --arg session_id "$SESSION_ID" \
    --arg cwd "$CWD" \
    --arg ts "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    '{session_id:$session_id, cwd:$cwd, ts:$ts}' \
      > "$TMP" 2>/dev/null \
      && mv "$TMP" "$STATE_DIR/current-session.json" \
      || rm -f "$TMP"
  fi
fi

# Ensure common bin paths are visible
for p in "$HOME/.cargo/bin" "$HOME/.local/bin" "/opt/homebrew/bin" "/usr/local/bin"; do
  [ -d "$p" ] && export PATH="$p:$PATH"
done

# Source client credentials when running in client mode (no-op otherwise).
if [ -f "$HOME/.fieldcraft/env" ]; then
  set -a
  # shellcheck disable=SC1090
  . "$HOME/.fieldcraft/env"
  set +a
fi

if ! command -v fieldcraft >/dev/null 2>&1; then
  exit 0
fi

# Build context text via heredoc so apostrophes / parens in the body never
# collide with jq filter quoting.
read -r -d '' MSG <<'EOF' || true
FIELDCRAFT MEMORY SYSTEM ACTIVE.

Before reading files, before planning — run `fieldcraft query "<topic>"` from a shell to remember related things that happened in prior sessions.

When you act on a retrieved lesson, mention its id inline as `lesson:<id>` so the trace analyzer can auto-detect the outcome. At session end, write any non-obvious lessons via `fieldcraft write --kind <kind> "<summary>"`, and mark retrieved lessons that were wrong via `fieldcraft feedback <id> --outcome contradicted`.
EOF

# Claude Code / Codex honor SessionStart additionalContext. Grok ignores stdout
# for SessionStart; the durable rule in ~/.grok/rules/fieldcraft.md covers Grok.
jq -n --arg msg "$MSG" '{
  "hookSpecificOutput": {
    "hookEventName": "SessionStart",
    "additionalContext": $msg
  }
}'
