#!/usr/bin/env bash
# Splice a hook entry (from a declarative JSON fragment) into an agent's
# hooks config file. Idempotent.
#
# Usage: merge-hook.sh <hooks-config-file> <fragment-file>
#
# Fragment is a JSON object with: { "event", "command", "purpose" }
# The literal token FIELDCRAFT_HOOKS (no leading $) is substituted with
# $FIELDCRAFT_HOOKS at install time.
#
# Claude Code (settings.json), Codex (hooks.json), and Grok (~/.grok/hooks/*.json)
# use the same top-level `hooks` JSON schema, so this works for all three.

set -e

config="$1"
fragment="$2"

if [[ -z "$config" || -z "$fragment" ]]; then
  echo "Usage: merge-hook.sh <hooks-config-file> <fragment-file>" >&2
  exit 1
fi

if ! command -v jq >/dev/null 2>&1; then
  echo "ERROR: jq is required to merge hooks. Install via 'brew install jq'." >&2
  exit 1
fi

if [[ -z "$FIELDCRAFT_HOOKS" ]]; then
  echo "ERROR: FIELDCRAFT_HOOKS env var must be set by the caller." >&2
  exit 1
fi

event=$(jq -r '.event' "$fragment")
cmd_raw=$(jq -r '.command' "$fragment")
cmd="${cmd_raw//FIELDCRAFT_HOOKS/$FIELDCRAFT_HOOKS}"

mkdir -p "$(dirname "$config")"
[[ -f "$config" ]] || echo '{"hooks":{}}' > "$config"

# Does this exact command already exist for this event?
existing=$(jq -r --arg event "$event" --arg cmd "$cmd" \
  '[.hooks[$event][]?.hooks[]?.command] | map(select(. == $cmd)) | length' \
  "$config" 2>/dev/null || echo 0)

if [[ "$existing" -gt 0 ]]; then
  echo "  $event hook already present in $config"
  exit 0
fi

tmp="$(mktemp)"
jq --arg event "$event" --arg cmd "$cmd" \
  '.hooks[$event] = ((.hooks[$event] // []) + [{"hooks":[{"type":"command","command":$cmd}]}])' \
  "$config" > "$tmp"
mv "$tmp" "$config"
echo "  added $event hook to $config ($(basename "$fragment"))"
