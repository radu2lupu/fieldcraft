#!/usr/bin/env bash
# Fieldcraft Stop hook — non-blocking background distiller.
#
# At end of session: append a marker, then optionally spawn a detached
# coding-agent CLI ("distiller") that reviews the transcript and writes a
# fieldcraft lesson if warranted. The hook itself exits 0 immediately;
# the agent's stop is never blocked.
#
# Distiller choice (FIELDCRAFT_DISTILLER env var, set in ~/.fieldcraft/env):
#   codex   — use `codex exec` (default if available; recommended)
#   grok    — use `grok -p` (Grok Build headless)
#   claude  — use `claude -p` (fallback; subject to Anthropic's programmatic
#             rate limits, may not work on every account)
#   none    — marker only, no distillation
#
# If unset: auto-detect — codex preferred, then grok, then claude, then marker-only.
#
# Friction gate (cheap local `fieldcraft distill-gate`, no LLM):
#   Quiet sessions do not spawn a distiller. Session-end marker + trace-analyze
#   always run. Tune with:
#     FIELDCRAFT_DISTILL_MIN_FRICTION  — default 3; 0 disables the score gate
#     FIELDCRAFT_DISTILL_FORCE=1       — always spawn (if a distiller is selected)

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

# Claude/Codex snake_case; Grok camelCase.
if [ "$(echo "$INPUT" | jq -r '.stop_hook_active // .stopHookActive // false' 2>/dev/null)" = "true" ]; then
  exit 0
fi

SESSION_ID="$(echo "$INPUT" | jq -r '.session_id // .sessionId // ""' 2>/dev/null)"
CWD="$(echo "$INPUT" | jq -r '.cwd // .workspaceRoot // ""' 2>/dev/null)"
TRANSCRIPT_PATH="$(echo "$INPUT" | jq -r '.transcript_path // .transcriptPath // empty' 2>/dev/null)"

STATE_DIR="$HOME/.fieldcraft/state"
MARKER="$STATE_DIR/session-ends.jsonl"
LOG="$STATE_DIR/distill.log"
mkdir -p "$STATE_DIR"

# Source ~/.fieldcraft/env so FIELDCRAFT_DISTILLER (and any auth env) is in scope.
if [ -f "$HOME/.fieldcraft/env" ]; then
  set -a
  # shellcheck disable=SC1090
  . "$HOME/.fieldcraft/env"
  set +a
fi

resolve_transcript_path() {
  if [ -n "$TRANSCRIPT_PATH" ] && [ -f "$TRANSCRIPT_PATH" ]; then
    printf '%s\n' "$TRANSCRIPT_PATH"
    return 0
  fi
  if [ -z "$SESSION_ID" ]; then
    return 0
  fi

  for root in \
    "$HOME/.codex/sessions" \
    "$HOME/.codex/archived_sessions" \
    "$HOME/.claude/projects" \
    "$HOME/.grok/sessions"
  do
    [ -d "$root" ] || continue
    # Prefer chat_history for Grok, then any session jsonl matching the id.
    found="$(find "$root" -type f \( -path "*${SESSION_ID}*/chat_history.jsonl" -o -name "*${SESSION_ID}*.jsonl" \) -print 2>/dev/null | head -n 1 || true)"
    if [ -n "$found" ]; then
      printf '%s\n' "$found"
      return 0
    fi
  done
}

RESOLVED_TRANSCRIPT_PATH="$(resolve_transcript_path)"
if [ -n "$RESOLVED_TRANSCRIPT_PATH" ] && [ "$RESOLVED_TRANSCRIPT_PATH" != "$TRANSCRIPT_PATH" ]; then
  echo "[fieldcraft-stop-hook] recovered transcript for session $SESSION_ID: $RESOLVED_TRANSCRIPT_PATH" \
    >> "$LOG"
  TRANSCRIPT_PATH="$RESOLVED_TRANSCRIPT_PATH"
fi

# --- Marker (always) ---------------------------------------------------------
{
  jq -nc \
    --arg session_id "$SESSION_ID" \
    --arg cwd        "$CWD" \
    --arg ts         "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    --arg transcript "$TRANSCRIPT_PATH" \
    '{session_id:$session_id, cwd:$cwd, ts:$ts, transcript:$transcript}' \
    >> "$MARKER"
} 2>/dev/null || true

# --- Trace analyzer: emit Confirmed / Contradicted / cue-gated Ignored events ---
# Cheap regex scan, no LLM — runs whenever a transcript exists, independent of
# distiller choice or transcript size. Detached + idempotent on rerun.
if [ -n "$TRANSCRIPT_PATH" ] && [ -f "$TRANSCRIPT_PATH" ] && command -v fieldcraft >/dev/null 2>&1; then
  nohup fieldcraft trace-analyze \
    --session "$SESSION_ID" \
    --transcript "$TRANSCRIPT_PATH" \
    --cwd "${CWD:-$HOME}" \
    >> "$LOG" 2>&1 &
  disown
fi

# --- Pick distiller ----------------------------------------------------------
choice="${FIELDCRAFT_DISTILLER:-auto}"
distiller=""
case "$choice" in
  none)
    exit 0 ;;
  codex)
    command -v codex  >/dev/null 2>&1 && distiller="codex" ;;
  grok)
    command -v grok   >/dev/null 2>&1 && distiller="grok" ;;
  claude)
    command -v claude >/dev/null 2>&1 && distiller="claude" ;;
  auto|"")
    if   command -v codex  >/dev/null 2>&1; then distiller="codex"
    elif command -v grok   >/dev/null 2>&1; then distiller="grok"
    elif command -v claude >/dev/null 2>&1; then distiller="claude"
    fi ;;
  *)
    echo "[fieldcraft-stop-hook] unknown FIELDCRAFT_DISTILLER='$choice', skipping" \
      >> "$LOG"
    exit 0 ;;
esac

if [ -z "$distiller" ]; then
  exit 0
fi

# --- Skip distillation when transcript is missing or trivial -----------------
if [ -z "$TRANSCRIPT_PATH" ] || [ ! -f "$TRANSCRIPT_PATH" ]; then
  exit 0
fi
TRANSCRIPT_BYTES="$(wc -c < "$TRANSCRIPT_PATH" 2>/dev/null || echo 0)"
if [ "$TRANSCRIPT_BYTES" -lt 10000 ]; then
  echo "[fieldcraft-stop-hook] decision=skip score=- threshold=- reasons=transcript_bytes:${TRANSCRIPT_BYTES}" \
    >> "$LOG"
  exit 0
fi

# --- Friction gate: skip LLM distill on quiet sessions -----------------------
# Marker + trace-analyze already ran above. This gate only decides whether to
# spend an LLM distiller. Log lines are machine-grepable: decision=run|skip.
MIN_FRICTION="${FIELDCRAFT_DISTILL_MIN_FRICTION:-3}"
if [ "${FIELDCRAFT_DISTILL_FORCE:-}" = "1" ]; then
  echo "[fieldcraft-stop-hook] decision=run score=forced threshold=${MIN_FRICTION} reasons=FIELDCRAFT_DISTILL_FORCE" \
    >> "$LOG"
elif ! command -v fieldcraft >/dev/null 2>&1; then
  echo "[fieldcraft-stop-hook] decision=run score=unknown threshold=${MIN_FRICTION} reasons=distill-gate-unavailable" \
    >> "$LOG"
else
  set +e
  GATE_OUT="$(fieldcraft distill-gate --transcript "$TRANSCRIPT_PATH" --min-friction "$MIN_FRICTION" 2>&1)"
  GATE_RC=$?
  set -e
  if echo "$GATE_OUT" | grep -qiE 'unrecognized subcommand|unexpected argument'; then
    echo "[fieldcraft-stop-hook] decision=run score=unknown threshold=${MIN_FRICTION} reasons=distill-gate-unavailable" \
      >> "$LOG"
  elif [ "$GATE_RC" -ne 0 ] || [ -z "$GATE_OUT" ]; then
    echo "[fieldcraft-stop-hook] decision=skip score=error threshold=${MIN_FRICTION} reasons=distill-gate-failed" \
      >> "$LOG"
    exit 0
  else
    GATE_LINE="$(echo "$GATE_OUT" | grep -E 'decision=(run|skip)' | tail -n 1 || true)"
    if [ -z "$GATE_LINE" ]; then
      echo "[fieldcraft-stop-hook] decision=skip score=error threshold=${MIN_FRICTION} reasons=distill-gate-unparseable" \
        >> "$LOG"
      exit 0
    fi
    GATE_LINE="${GATE_LINE#\[fieldcraft-distill-gate\] }"
    echo "[fieldcraft-stop-hook] $GATE_LINE" >> "$LOG"
    case "$GATE_LINE" in
      *decision=skip*) exit 0 ;;
    esac
  fi
fi

if [ -z "$CWD" ] || [ ! -d "$CWD" ]; then
  CWD="$HOME"
fi

# --- Build the prompt --------------------------------------------------------
read -r -d '' PROMPT <<EOF || true
Review the coding session transcript at $TRANSCRIPT_PATH.

The agent's working directory was $CWD.

If — and only if — this session contained a non-obvious, durable finding
that a future agent would benefit from (per the quality bar in your
agent instructions' fieldcraft block), record at most ONE concise lesson
by running this exact shell command:

    fieldcraft write --kind <claim|procedure|episode|directive|context> "<one-sentence summary>"

Otherwise, exit silently without writing. Do NOT write trivial lessons
(typos, dep bumps, formatting, routine investigation). Be terse — no
preamble, no summary back to the user. Either call fieldcraft write once
or do nothing.
EOF

# --- Spawn detached background distiller -------------------------------------
# Wall-clock timeout for the distiller. A stuck `codex exec` / `grok -p` /
# `claude -p` (e.g. an LLM call that never returns) would otherwise live as a
# launchd orphan forever. Override via FIELDCRAFT_DISTILL_TIMEOUT (seconds).
DISTILL_TIMEOUT="${FIELDCRAFT_DISTILL_TIMEOUT:-600}"

# Nested distiller sessions must not re-enter fieldcraft hooks.
export FIELDCRAFT_HOOK_SKIP=1

# Detached subshell with watchdog: runs the distiller in the background, then
# a sibling sleeper that SIGTERMs it after $DISTILL_TIMEOUT. If the distiller
# exits first, the sleeper is reaped so nothing lingers.
case "$distiller" in
  codex)
    # Distiller sessions are meta-work; nested hooks create untraceable retrieval noise.
    (
      codex exec \
        --dangerously-bypass-approvals-and-sandbox \
        --disable hooks \
        --ephemeral \
        --skip-git-repo-check \
        --cd "$CWD" \
        "$PROMPT" &
      cmd_pid=$!
      (
        sleep "$DISTILL_TIMEOUT"
        if kill -0 "$cmd_pid" 2>/dev/null; then
          echo "[fieldcraft-stop-hook] codex distiller PID $cmd_pid exceeded ${DISTILL_TIMEOUT}s — terminating" >&2
          kill -TERM "$cmd_pid" 2>/dev/null
          sleep 5
          kill -KILL "$cmd_pid" 2>/dev/null
        fi
      ) &
      watchdog_pid=$!
      wait "$cmd_pid" 2>/dev/null
      kill -TERM "$watchdog_pid" 2>/dev/null
      wait "$watchdog_pid" 2>/dev/null
    ) >> "$LOG" 2>&1 < /dev/null &
    disown
    ;;
  grok)
    (
      grok -p \
        --always-approve \
        --cwd "$CWD" \
        --disable-memory \
        "$PROMPT" &
      cmd_pid=$!
      (
        sleep "$DISTILL_TIMEOUT"
        if kill -0 "$cmd_pid" 2>/dev/null; then
          echo "[fieldcraft-stop-hook] grok distiller PID $cmd_pid exceeded ${DISTILL_TIMEOUT}s — terminating" >&2
          kill -TERM "$cmd_pid" 2>/dev/null
          sleep 5
          kill -KILL "$cmd_pid" 2>/dev/null
        fi
      ) &
      watchdog_pid=$!
      wait "$cmd_pid" 2>/dev/null
      kill -TERM "$watchdog_pid" 2>/dev/null
      wait "$watchdog_pid" 2>/dev/null
    ) >> "$LOG" 2>&1 < /dev/null &
    disown
    ;;
  claude)
    (
      # Prompt goes first: --add-dir is variadic and would swallow a trailing prompt.
      claude \
        --print "$PROMPT" \
        --dangerously-skip-permissions \
        --no-session-persistence \
        --max-budget-usd "${FIELDCRAFT_DISTILL_BUDGET_USD:-0.30}" \
        --add-dir "$CWD" &
      cmd_pid=$!
      (
        sleep "$DISTILL_TIMEOUT"
        if kill -0 "$cmd_pid" 2>/dev/null; then
          echo "[fieldcraft-stop-hook] claude distiller PID $cmd_pid exceeded ${DISTILL_TIMEOUT}s — terminating" >&2
          kill -TERM "$cmd_pid" 2>/dev/null
          sleep 5
          kill -KILL "$cmd_pid" 2>/dev/null
        fi
      ) &
      watchdog_pid=$!
      wait "$cmd_pid" 2>/dev/null
      kill -TERM "$watchdog_pid" 2>/dev/null
      wait "$watchdog_pid" 2>/dev/null
    ) >> "$LOG" 2>&1 < /dev/null &
    disown
    ;;
esac

exit 0
