#!/usr/bin/env bash
# Live project status — the fast-changing facts that a checked-in doc can't keep fresh.
# Read-only. Pairs with STATUS.md (which holds the slow-changing direction/topology).
#
#   ./scripts/status.sh
#
set -uo pipefail
cd "$(git rev-parse --show-toplevel 2>/dev/null || echo .)"

bold(){ printf "\033[1m%s\033[0m\n" "$1"; }
dim(){ printf "\033[2m%s\033[0m\n" "$1"; }

TS="/Applications/Tailscale.app/Contents/MacOS/Tailscale"
ts_name=""; [ -x "$TS" ] && ts_name="$("$TS" status --json 2>/dev/null | python3 -c 'import sys,json;print(json.load(sys.stdin)["Self"]["DNSName"].rstrip("."))' 2>/dev/null)"

bold "▸ Machine"
echo "  host:      $(scutil --get LocalHostName 2>/dev/null || hostname)"
[ -n "$ts_name" ] && echo "  tailscale: $ts_name"

bold "▸ Repo"
echo "  branch:    $(git branch --show-current 2>/dev/null)  $(git status --porcelain 2>/dev/null | grep -q . && echo '(dirty)' || echo '(clean)')"
up="$(git rev-list --left-right --count @{u}...HEAD 2>/dev/null)"
[ -n "$up" ] && echo "  vs upstream (behind ahead): $up"
echo "  last:      $(git log -1 --format='%h %s (%cr)' 2>/dev/null)"

bold "▸ Parallel work (worktrees)"
git worktree list 2>/dev/null | sed 's/^/  /' || echo "  (none)"

bold "▸ Open PRs"
if command -v gh >/dev/null 2>&1; then
  gh pr list --state open --limit 10 2>/dev/null | sed 's/^/  /' | grep -q . \
    && gh pr list --state open --limit 10 2>/dev/null | sed 's/^/  /' \
    || echo "  (none open)"
else
  echo "  (gh not installed)"
fi

bold "▸ Fieldcraft server"
src="$HOME/.fieldcraft/env"; [ -f "$src" ] && . "$src"
chk(){ curl -s --max-time 4 "$1/health" >/dev/null 2>&1 && echo "up" || echo "DOWN"; }
echo "  gateway  ${FIELDCRAFT_SERVER_URL:-unset}: $([ -n "${FIELDCRAFT_SERVER_URL:-}" ] && chk "$FIELDCRAFT_SERVER_URL" || echo n/a)"
echo "  upstream ${FIELDCRAFT_UPSTREAM_URL:-unset}: $([ -n "${FIELDCRAFT_UPSTREAM_URL:-}" ] && chk "$FIELDCRAFT_UPSTREAM_URL" || echo n/a)"
if command -v fieldcraft >/dev/null 2>&1; then
  recs="$(fieldcraft status 2>/dev/null | grep -iE 'Total records' | head -1)"
  [ -n "$recs" ] && echo "  catalogue (this project): ${recs##*:  }"
fi

dim "── direction & topology: see STATUS.md ──"
