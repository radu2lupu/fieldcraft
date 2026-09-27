#!/usr/bin/env bash
# Fieldcraft uninstall — reverses install.sh.
#
# Removes installed binaries, hook scripts, hook entries from agent configs,
# MCP server entries, and the markered instruction blocks.
#
# Does NOT touch ~/.fieldcraft/memory.sqlite3 or ~/.dam/memory.sqlite3.
# Your knowledge corpus is preserved; only the integration glue is removed.

set -e

REPO="$(cd "$(dirname "$0")" && pwd)"
HOOKS_DIR="$HOME/.fieldcraft/hooks"
LAUNCHD_LABEL="${FIELDCRAFT_LAUNCHD_LABEL:-com.fieldcraft.server}"
SHELL_ENV_START="# fieldcraft-env:start"
SHELL_ENV_END="# fieldcraft-env:end"

echo "Uninstalling fieldcraft integration"

remove_shell_env_snippet() {
  local target="$1"
  [[ -f "$target" ]] || return 0
  if ! grep -qF "$SHELL_ENV_START" "$target"; then
    return 0
  fi
  local tmp
  tmp="$(mktemp)"
  awk -v start="$SHELL_ENV_START" -v end="$SHELL_ENV_END" '
    BEGIN { in_block = 0 }
    {
      if (index($0, start) > 0) { in_block = 1; next }
      if (in_block && index($0, end) > 0) { in_block = 0; next }
      if (!in_block) print
    }
  ' "$target" > "$tmp"
  mv "$tmp" "$target"
  echo "  removed Fieldcraft env loader from $target"
}

# --- Strip instruction blocks ---------------------------------------------
START="<!-- fieldcraft:start -->"
END="<!-- fieldcraft:end -->"
for target in "$HOME/.claude/CLAUDE.md" "$HOME/.codex/AGENTS.md" "$HOME/.grok/rules/fieldcraft.md"; do
  [[ -f "$target" ]] || continue
  if grep -qF "$START" "$target"; then
    tmp="$(mktemp)"
    awk -v start="$START" -v end="$END" '
      BEGIN { in_block = 0 }
      {
        if (index($0, start) > 0) { in_block = 1; next }
        if (in_block && index($0, end) > 0) { in_block = 0; next }
        if (!in_block) print
      }
    ' "$target" > "$tmp"
    # Grok rule file is fieldcraft-only — remove entirely if empty after strip.
    if [[ "$target" == "$HOME/.grok/rules/fieldcraft.md" ]]; then
      if [[ ! -s "$tmp" ]] || ! grep -q '[^[:space:]]' "$tmp"; then
        rm -f "$target" "$tmp"
        echo "  removed $target"
        continue
      fi
    fi
    mv "$tmp" "$target"
    echo "  stripped fieldcraft block from $target"
  elif [[ "$target" == "$HOME/.grok/rules/fieldcraft.md" ]]; then
    rm -f "$target"
    echo "  removed $target"
  fi
done

# --- Remove hook entries from settings.json / hooks.json ------------------
remove_hook_from_config() {
  local config="$1"
  local cmd_substr="$2"
  [[ -f "$config" ]] || return 0
  if ! command -v jq >/dev/null 2>&1; then
    echo "  WARN: jq not found; skipping $config hook cleanup" >&2
    return 0
  fi
  if ! jq -e --arg c "$cmd_substr" \
      '[.. | objects | select(.command // "" | contains($c))] | length > 0' \
      "$config" >/dev/null 2>&1; then
    return 0
  fi
  local tmp
  tmp="$(mktemp)"
  jq --arg c "$cmd_substr" '
    .hooks |= with_entries(
      .value |= map(
        .hooks |= map(select((.command // "") | contains($c) | not))
      ) | .value |= map(select(.hooks | length > 0))
    )
  ' "$config" > "$tmp"
  mv "$tmp" "$config"
  echo "  removed fieldcraft hooks from $config"
}
remove_hook_from_config "$HOME/.claude/settings.json"  "$HOOKS_DIR"
remove_hook_from_config "$HOME/.codex/hooks.json"      "$HOOKS_DIR"
if [[ -f "$HOME/.grok/hooks/fieldcraft.json" ]]; then
  rm -f "$HOME/.grok/hooks/fieldcraft.json"
  echo "  removed ~/.grok/hooks/fieldcraft.json"
fi


for shell_config in "$HOME/.zshrc" "$HOME/.zprofile" "$HOME/.bashrc" "$HOME/.bash_profile"; do
  remove_shell_env_snippet "$shell_config"
done

uid="$(id -u)"
launchctl bootout "gui/$uid/$LAUNCHD_LABEL" 2>/dev/null || true
plist="$HOME/Library/LaunchAgents/$LAUNCHD_LABEL.plist"
if [[ -f "$plist" ]]; then
  rm "$plist"
  echo "  removed $plist"
fi

# --- Remove MCP server entries --------------------------------------------
claude_config="$HOME/.claude/claude_desktop_config.json"
if [[ -f "$claude_config" ]] && command -v jq >/dev/null 2>&1; then
  if jq -e '.mcpServers.fieldcraft' "$claude_config" >/dev/null 2>&1; then
    tmp="$(mktemp)"
    jq 'del(.mcpServers.fieldcraft)' "$claude_config" > "$tmp"
    mv "$tmp" "$claude_config"
    echo "  removed fieldcraft mcp entry from $claude_config"
  fi
fi
if command -v claude >/dev/null 2>&1; then
  claude mcp remove fieldcraft >/dev/null 2>&1 || true
fi

codex_config="$HOME/.codex/config.toml"
if [[ -f "$codex_config" ]] && grep -qF "# fieldcraft:start" "$codex_config"; then
  tmp="$(mktemp)"
  awk '
    BEGIN { in_block = 0 }
    {
      if ($0 ~ /^# fieldcraft:start/) { in_block = 1; next }
      if (in_block && $0 ~ /^# fieldcraft:end/) { in_block = 0; next }
      if (!in_block) print
    }
  ' "$codex_config" > "$tmp"
  mv "$tmp" "$codex_config"
  echo "  stripped fieldcraft mcp block from $codex_config"
fi

# --- Remove client env file + hook scripts + binaries ---------------------
if [[ -f "$HOME/.fieldcraft/env" ]]; then
  rm "$HOME/.fieldcraft/env"
  echo "  removed ~/.fieldcraft/env (client credentials)"
fi
[[ -d "$HOOKS_DIR" ]] && rm -rf "$HOOKS_DIR" && echo "  removed $HOOKS_DIR"
for bin in fieldcraft fieldcraft-server fieldcraft-backfill-cues; do
  [[ -f "$HOME/.cargo/bin/$bin" ]] && rm "$HOME/.cargo/bin/$bin" && \
    echo "  removed ~/.cargo/bin/$bin"
done

echo ""
echo "Uninstalled. Knowledge corpus preserved at ~/.fieldcraft/memory.sqlite3 (and ~/.dam/ if you use that)."
