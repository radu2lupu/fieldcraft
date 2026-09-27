#!/bin/bash
# SessionStart hook — tells Claude to query Fieldcraft before doing anything else.
# Activates for any git repo automatically. Project name is derived from git remote
# origin (deterministic across machines) or falls back to directory name.
INPUT=$(cat)

# Get cwd from hook input (more reliable than $PWD in hook context)
CWD=$(echo "$INPUT" | jq -r '.cwd // empty')
CWD="${CWD:-$PWD}"

# Ensure common global bin paths are on PATH
for p in "$HOME/.bun/bin" "/opt/homebrew/bin" "/usr/local/bin"; do
  [ -d "$p" ] && export PATH="$p:$PATH"
done

CLI="${CLAUDE_PLUGIN_ROOT}/scripts/fieldcraft"

# Must have the CLI available
if [ ! -x "$CLI" ] && ! command -v fieldcraft >/dev/null 2>&1; then
  exit 0
fi

jq -n --arg cli "$CLI" '{
  "hookSpecificOutput": {
    "hookEventName": "SessionStart",
    "additionalContext": ("FIELDCRAFT LEARNING SYSTEM ACTIVE.\nYour FIRST action — before reading files, before planning, before anything — MUST be to query prior field lessons:\n`" + $cli + " query \"<what the user is asking about>\"`\nThis retrieves distilled lessons from previous sessions. Without it, you may repeat past mistakes and miss prior decisions. This is mandatory, not optional.")
  }
}'

exit 0
