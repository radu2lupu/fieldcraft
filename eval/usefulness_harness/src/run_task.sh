#!/usr/bin/env bash
# run_task.sh — run ONE held-out task in ONE condition (warm or cold) and write a
# result.json with the real oracle verdict (pytest FAIL_TO_PASS/PASS_TO_PASS), real step
# count, and real token count. No mocks: this clones the real repo at base_commit, applies
# the real test_patch, runs the real codex CLI as the solving agent, applies whatever patch
# the agent produced, and runs the real test suite.
#
# NO PYTHON in this script's own logic: JSON is read/written with `jq`. The ONLY Python
# involved anywhere in the harness is python3.9/3.11 acting as the TARGET REPO's own
# interpreter (pytest needs to run inside a venv matching the SWE task's era) — that is the
# system under test, not part of the harness's own implementation.
#
# Memory injection model (push-mode, deterministic):
#   - warm: this script itself calls `fieldcraft query` against a fully isolated, scratch
#     FIELDCRAFT_HOME (never the real shared DB) that was pre-seeded with exactly one gold
#     lesson for this task via `fieldcraft write`. The literal retrieved block (including
#     its real `lesson:<hex>` id) is embedded into the agent's prompt under a
#     "Fieldcraft memory" section.
#   - cold: no memory section in the prompt, plus an explicit instruction telling the agent
#     this is a memory-suppressed run. KNOWN LIMITATION (see section 7 below for the full
#     writeup): codex's own global ~/.codex/AGENTS.md tells it to auto-run `fieldcraft query`
#     at the start of any task, and we could NOT fully suppress that deterministically without
#     risking a parallel agent's live use of the real `fieldcraft` binary, so cold runs are
#     monitored post-hoc for contamination (`cold_contaminated` in result.json) instead.
#
# Usage:
#   run_task.sh <task_id> <condition: warm|cold> <work_root> <results_root> <tasks_json> <lessons_json>
#
# Writes: <results_root>/<condition>/<task_id>/result.json (+ patch.diff, codex_log.jsonl)

set -uo pipefail

TASK_ID="$1"
CONDITION="$2"   # warm | cold

# Resolve every path argument to absolute BEFORE any `cd` happens below — several steps
# `cd` into the shared repo clone dir, and a relative path resolved after that `cd` would
# silently nest under the wrong directory.
abs_path() {
  mkdir -p "$1" 2>/dev/null
  ( cd "$1" && pwd )
}
WORK_ROOT="$(abs_path "$3")"
RESULTS_ROOT="$(abs_path "$4")"
TASKS_JSON="$(cd "$(dirname "$5")" && pwd)/$(basename "$5")"
LESSONS_JSON="$(cd "$(dirname "$6")" && pwd)/$(basename "$6")"

if [[ "$CONDITION" != "warm" && "$CONDITION" != "cold" ]]; then
  echo "error: condition must be 'warm' or 'cold', got '$CONDITION'" >&2
  exit 2
fi

OUT_DIR="$RESULTS_ROOT/$CONDITION/$TASK_ID"
mkdir -p "$OUT_DIR"

# ---------------------------------------------------------------------------
# 1. Pull task fields out of tasks.json with jq.
# ---------------------------------------------------------------------------
TASK_FILTER=".[] | select(.instance_id == \$tid)"

REPO=$(jq -r --arg tid "$TASK_ID" "$TASK_FILTER | .repo" "$TASKS_JSON")
if [[ -z "$REPO" || "$REPO" == "null" ]]; then
  echo "error: task $TASK_ID not found in $TASKS_JSON" >&2
  exit 1
fi
BASE_COMMIT=$(jq -r --arg tid "$TASK_ID" "$TASK_FILTER | .base_commit" "$TASKS_JSON")
PROBLEM_STATEMENT=$(jq -r --arg tid "$TASK_ID" "$TASK_FILTER | .problem_statement" "$TASKS_JSON")
PY_VERSION=$(jq -r --arg tid "$TASK_ID" "$TASK_FILTER | .python_version" "$TASKS_JSON")

# bash 3.2 (macOS system default) has no readarray/mapfile; build arrays with a while-read
# loop instead, which is portable back to bash 3.x.
FAIL_TO_PASS=()
while IFS= read -r line; do
  [[ -n "$line" ]] && FAIL_TO_PASS+=("$line")
done < <(jq -r --arg tid "$TASK_ID" "$TASK_FILTER | .fail_to_pass[]" "$TASKS_JSON")

PASS_TO_PASS=()
while IFS= read -r line; do
  [[ -n "$line" ]] && PASS_TO_PASS+=("$line")
done < <(jq -r --arg tid "$TASK_ID" "$TASK_FILTER | .pass_to_pass[]" "$TASKS_JSON")

REPO_SLUG=$(echo "$REPO" | tr '/' '_')
REPO_DIR="$WORK_ROOT/repos/$REPO_SLUG"
TASK_WORKTREE="$OUT_DIR/checkout"
VENV_SHARED="$WORK_ROOT/venvs/${REPO_SLUG}_py${PY_VERSION}"

echo "[$TASK_ID/$CONDITION] repo=$REPO base=$BASE_COMMIT" >&2

# ---------------------------------------------------------------------------
# 2. Shared clone cache per repo, fresh worktree per task run.
# ---------------------------------------------------------------------------
mkdir -p "$WORK_ROOT/repos" "$WORK_ROOT/venvs"
if [[ ! -d "$REPO_DIR/.git" ]]; then
  git clone --filter=blob:none -q "https://github.com/$REPO.git" "$REPO_DIR" >&2 || {
    echo "error: clone of $REPO failed" >&2
    exit 1
  }
fi
( cd "$REPO_DIR" && git fetch -q origin "$BASE_COMMIT" 2>/dev/null )

rm -rf "$TASK_WORKTREE"
( cd "$REPO_DIR" && git worktree prune 2>/dev/null )
( cd "$REPO_DIR" && git worktree add -q --detach "$TASK_WORKTREE" "$BASE_COMMIT" ) >&2 || {
  echo "error: worktree checkout of $BASE_COMMIT failed" >&2
  exit 1
}

# ---------------------------------------------------------------------------
# 3. Apply the test_patch so FAIL_TO_PASS tests are in their target (currently-red) state.
#    The agent NEVER sees this diff; it only sees the problem statement + failing test names.
# ---------------------------------------------------------------------------
TEST_PATCH_FILE="$OUT_DIR/test_patch.diff"
jq -r --arg tid "$TASK_ID" "$TASK_FILTER | .test_patch" "$TASKS_JSON" > "$TEST_PATCH_FILE"

if [[ -s "$TEST_PATCH_FILE" ]]; then
  ( cd "$TASK_WORKTREE" && git apply "$TEST_PATCH_FILE" ) >&2 || {
    echo "error: test_patch failed to apply for $TASK_ID" >&2
    jq -n '{resolved: false, error: "test_patch_apply_failed"}' > "$OUT_DIR/result.json"
    exit 1
  }
fi

# ---------------------------------------------------------------------------
# 4. Per-repo venv (shared across tasks of the same repo+python version; created once).
#    NOTE: this venv's python interpreter is the SYSTEM UNDER TEST's runtime (the target
#    repo, e.g. old pytest/django, needs a period-appropriate CPython), not part of the
#    harness's own implementation language.
# ---------------------------------------------------------------------------
PY_BIN="/opt/homebrew/bin/python${PY_VERSION}"
if [[ ! -x "$PY_BIN" ]]; then
  echo "error: $PY_BIN not found" >&2
  jq -n '{resolved: false, error: "python_not_found"}' > "$OUT_DIR/result.json"
  exit 1
fi
if [[ ! -d "$VENV_SHARED" ]]; then
  "$PY_BIN" -m venv "$VENV_SHARED" >&2
fi
# Pin setuptools<81: modern setuptools (82+) dropped the deprecated pkg_resources module,
# but pytest's older `_checkversion()` codepath (pytest 4.x/5.x/6.0-era, which several of
# our held-out tasks use) imports pkg_resources directly. Without this pin, EVERY test
# collection in that venv crashes with ModuleNotFoundError before a single test runs — a
# real bug we hit during harness development (it silently zeroed out PASS_TO_PASS, which
# looked like "the agent broke everything" when it was actually an environment bug).
"$VENV_SHARED/bin/pip" install -q "setuptools<81" >&2 2>&1 | tail -5 >&2
# IMPORTANT: when the target repo IS pytest itself (pytest-dev/pytest is self-hosting —
# running tests imports the installed pytest as the test runner), installing any helper
# package that lists `pytest` as a dependency (e.g. pytest-json-report) makes pip silently
# upgrade/replace the period-correct pytest with a fresh PyPI pytest, corrupting the very
# thing under test. Install report tooling with --no-deps FIRST (so its own pytest>=x
# dependency is never resolved), THEN install the target repo LAST so it always wins.
if ! "$VENV_SHARED/bin/python" -c "import pytest_jsonreport" 2>/dev/null; then
  "$VENV_SHARED/bin/pip" install -q --no-deps pytest-json-report >&2 2>&1 | tail -5 >&2
  # pytest-json-report's own light runtime deps (no pytest pin) — fine to pull normally.
  "$VENV_SHARED/bin/pip" install -q pluggy iniconfig >&2 2>&1 | tail -5 >&2
fi
# Regular (non-editable) install: copies the package into site-packages instead of linking
# back to the worktree path. Editable installs (`pip install -e .`) pin a path reference to
# the worktree, which then dangles once the worktree is removed at the end of this script
# (or gets silently repointed by the NEXT task that reuses this shared venv) — a real bug we
# hit during harness development. A plain install is slightly slower per task but immune to
# worktree-lifetime drift, which matters because the venv is shared/reused across tasks.
# --force-reinstall guarantees THIS task's exact source always wins over whatever a prior
# task in the same shared venv left behind; deps ARE resolved normally here (none of pytest's
# / django's own install_requires pull in a newer copy of themselves).
( cd "$TASK_WORKTREE" && "$VENV_SHARED/bin/pip" install -q --force-reinstall . ) >&2 2>&1 | tail -20 >&2
# Re-pin setuptools<81 immediately after: confirmed by direct reproduction during harness
# development that `pip install --force-reinstall .` against pytest's legacy setup.py
# (use_scm_version, setup_requires=["setuptools-scm","setuptools>=40.0"]) silently bumps
# the RUNTIME venv's setuptools to latest (e.g. 82.0.1) even though we pinned <81 moments
# earlier — this is pip's build-time dependency resolution leaking into the target venv,
# not a one-off fluke (it reproduced on every task in this self-hosting-pytest venv). The
# precheck below runs pytest, and old pytest's `_checkversion()` imports the now-removed
# `pkg_resources` module, so without re-pinning HERE the precheck silently collects 0 tests.
"$VENV_SHARED/bin/pip" install -q "setuptools<81" >&2 2>&1 | tail -5 >&2

# ---------------------------------------------------------------------------
# 5. Oracle helper: run a list of pytest node ids. Prints "<passed> <collected>" — collected
#    is how many of the requested ids pytest could actually resolve and run (via jq on the
#    pytest-json-report output), which can be LESS than the requested count when the
#    upstream dataset has malformed node ids (see --continue-on-collection-errors note below).
#    Callers must use the COLLECTED count as the real denominator, not the raw requested
#    count, or a single bad id in the dataset would make a fully-correct fix look "failed".
# ---------------------------------------------------------------------------
run_pytest_subset() {
  local report_file="$1"
  shift
  local -a requested=("$@")
  if [[ ${#requested[@]} -eq 0 ]]; then
    echo "0 0"
    return 0
  fi

  # Drop node ids with an unbalanced '[' (no matching ']') BEFORE invoking pytest. The
  # upstream swe-bench-cl-instances.jsonl dataset has a handful of genuinely truncated
  # PASS_TO_PASS parametrize ids (e.g. "test_x[foo.py-1" missing its closing "]" — verified
  # directly against the source JSONL; this harness did not introduce the truncation).
  # pytest's own node-id resolution treats ANY unresolvable id in the argv list as a hard
  # usage error and reports ZERO tests for the entire invocation (--continue-on-collection-
  # errors does NOT help here — that flag only covers import-time collection errors, not
  # "no such test id" errors), so a single bad id would otherwise zero out every real result
  # in the batch. We filter, not patch the IDs, since we don't know what the truncated
  # suffix was supposed to be.
  local -a tests=()
  local -a skipped=()
  for t in "${requested[@]}"; do
    local open_count close_count
    open_count=$(grep -o '\[' <<<"$t" | wc -l | tr -d ' ')
    close_count=$(grep -o '\]' <<<"$t" | wc -l | tr -d ' ')
    # A real pytest node id always contains "::" (file::test separator). We also found a
    # stray "[100%]" entry in the pytest-5262 dataset row — a captured pytest progress-bar
    # artifact, not a test id at all (balanced brackets, so the bracket check alone misses
    # it). Require "::" as a second, independent sanity check.
    if [[ "$open_count" != "$close_count" || "$t" != *"::"* ]]; then
      skipped+=("$t")
    else
      tests+=("$t")
    fi
  done
  if [[ ${#skipped[@]} -gt 0 ]]; then
    echo "[$TASK_ID/$CONDITION] excluding ${#skipped[@]} malformed dataset node id(s) (unbalanced brackets, pre-existing dataset defect): ${skipped[*]}" >&2
  fi
  if [[ ${#tests[@]} -eq 0 ]]; then
    echo "0 0"
    return 0
  fi

  # Note: --no-header is NOT used here — it does not exist in pytest's older versions
  # (this harness spans pytest 4.5 through 6.3 era checkouts) and would fail with
  # "unrecognized arguments" on old pytest, silently producing a zero-test report.
  ( cd "$TASK_WORKTREE" && "$VENV_SHARED/bin/python" -m pytest -q -p no:cacheprovider \
      --continue-on-collection-errors \
      "${tests[@]}" --json-report --json-report-file="$report_file" \
      > "${report_file%.json}.stdout.log" 2>&1 )
  if [[ -f "$report_file" ]]; then
    local passed collected
    passed=$(jq -r '.summary.passed // 0' "$report_file")
    collected=$(jq -r '.summary.collected // .summary.total // 0' "$report_file")
    echo "$passed $collected"
  else
    echo "0 0"
  fi
}

PRECHECK_REPORT="$OUT_DIR/_precheck_report.json"
read -r PRECHECK_PASS_COUNT PRECHECK_COLLECTED < <(run_pytest_subset "$PRECHECK_REPORT" "${FAIL_TO_PASS[@]}")
echo "[$TASK_ID/$CONDITION] precheck (before agent): FAIL_TO_PASS passed=$PRECHECK_PASS_COUNT / $PRECHECK_COLLECTED collected (expect 0 = genuinely red)" >&2

# ---------------------------------------------------------------------------
# 6. Memory injection (warm only): query the ISOLATED fieldcraft DB, never the real one.
# ---------------------------------------------------------------------------
MEMORY_BLOCK=""
WARM_LESSON_ID=""
if [[ "$CONDITION" == "warm" ]]; then
  SCRATCH_FC_HOME="$OUT_DIR/fc_isolated_home"
  FAKE_HOME="$OUT_DIR/fc_fake_home"
  mkdir -p "$SCRATCH_FC_HOME" "$FAKE_HOME"

  LESSON_TEXT=$(jq -r --arg tid "$TASK_ID" '.lessons[$tid] // empty' "$LESSONS_JSON")
  if [[ -z "$LESSON_TEXT" ]]; then
    echo "error: no gold lesson defined for $TASK_ID in $LESSONS_JSON" >&2
    jq -n '{resolved: false, error: "no_gold_lesson"}' > "$OUT_DIR/result.json"
    exit 1
  fi

  WRITE_OUT=$(env -u FIELDCRAFT_SERVER_URL -u FIELDCRAFT_UPSTREAM_URL -u FIELDCRAFT_AUTH_TOKEN \
    HOME="$FAKE_HOME" FIELDCRAFT_HOME="$SCRATCH_FC_HOME" \
    fieldcraft write "$LESSON_TEXT" --project "${REPO_SLUG}" --kind claim 2>&1)
  echo "[$TASK_ID/warm] $WRITE_OUT" >&2
  WARM_LESSON_ID=$(echo "$WRITE_OUT" | grep -oE '[0-9a-f]{60,64}' | head -1)

  QUERY_TEXT=$(echo "$PROBLEM_STATEMENT" | head -c 300)
  MEMORY_BLOCK=$(env -u FIELDCRAFT_SERVER_URL -u FIELDCRAFT_UPSTREAM_URL -u FIELDCRAFT_AUTH_TOKEN \
    HOME="$FAKE_HOME" FIELDCRAFT_HOME="$SCRATCH_FC_HOME" \
    fieldcraft query "$QUERY_TEXT" --project "${REPO_SLUG}" --limit 5 2>&1)
fi

# ---------------------------------------------------------------------------
# 6b. Cold isolation: give the cold arm a genuinely memory-free codex environment.
#
#    BACKGROUND ON WHY THE FIRST ATTEMPT FAILED: codex's global ~/.codex/AGENTS.md (Radu's
#    user-level instructions, shared across all codex sessions on this machine) tells codex to
#    run `fieldcraft query` at the start of any non-trivial task. The first version of this
#    harness tried to suppress that by blanking FIELDCRAFT_SERVER_URL/FIELDCRAFT_HOME and
#    overriding HOME on the `codex exec` invocation itself, and reported "didn't work because
#    codex always spawns zsh -lc". That diagnosis was half right and half wrong:
#      - RIGHT: codex always spawns command_execution steps as `/bin/zsh -lc '<cmd>'`, a LOGIN
#        shell, and both ~/.zprofile and ~/.zshrc unconditionally `. "$HOME/.fieldcraft/env"`
#        (re-exporting the real FIELDCRAFT_SERVER_URL) whenever $HOME resolves to the real
#        home — confirmed directly: `env -u FIELDCRAFT_SERVER_URL zsh -lc 'echo $FIELDCRAFT_SERVER_URL'`
#        prints the real server URL even with the var unset by the caller.
#      - WRONG: overriding HOME on `codex exec` itself does NOT just "not work" — it breaks
#        codex's OWN authentication. codex resolves its auth.json/config.toml from $HOME/.codex
#        by default, so HOME=<scratch> makes codex itself fail with 401 Unauthorized before it
#        even reaches the model (confirmed by direct reproduction). The prior author likely hit
#        this 401 and mis-attributed total failure to the zsh -lc re-source problem, when the
#        REAL blocker was that codex never got to run a single command at all.
#
#    THE FIX: codex has a SEPARATE env var, CODEX_HOME, that controls where it loads its own
#    config/auth from (independent of $HOME). Setting CODEX_HOME to a scratch dir that contains
#    ONLY a copy of the real auth.json + config.toml (no AGENTS.md) lets codex authenticate and
#    run normally while:
#      (a) HOME is overridden to an empty scratch dir for the COLD run, so the zsh -lc login
#          shell codex spawns for every command_execution step sources THAT home's (nonexistent)
#          ~/.zprofile / ~/.zshrc instead of the real ones — ~/.fieldcraft/env is never
#          re-sourced, FIELDCRAFT_SERVER_URL/UPSTREAM_URL/AUTH_TOKEN stay unset for real, and
#      (b) because the scratch CODEX_HOME has no AGENTS.md, codex never loads the "run
#          `fieldcraft query` at the start of any task" directive in the first place — this is a
#          second, independent suppression layer on top of (a), confirmed by direct
#          reproduction: an unprompted generic coding task run with this HOME/CODEX_HOME
#          combination produced ZERO fieldcraft invocations in the codex log, vs. the documented
#          4/4 contamination rate before this fix.
#      (c) even in the belt-and-suspenders case where fieldcraft IS invoked anyway (e.g. the
#          agent runs it on its own initiative, not via AGENTS.md), `fieldcraft` with no
#          FIELDCRAFT_SERVER_URL falls back to a LOCAL sqlite store at $HOME/.fieldcraft/, which
#          is brand new and empty under the scratch HOME, so the query returns nothing to leak.
#
#    We do NOT touch the real `fieldcraft` binary, the real ~/.codex, or any global dotfile —
#    this is purely additive scratch state per cold task run, safe to run alongside another
#    agent's concurrent use of the live fieldcraft CLI elsewhere on this machine.
# ---------------------------------------------------------------------------
COLD_HOME=""
COLD_CODEX_HOME=""
if [[ "$CONDITION" == "cold" ]]; then
  COLD_HOME="$OUT_DIR/cold_isolated_home"
  COLD_CODEX_HOME="$OUT_DIR/cold_isolated_codex_home"
  rm -rf "$COLD_HOME" "$COLD_CODEX_HOME"
  mkdir -p "$COLD_HOME" "$COLD_CODEX_HOME"

  # Seed the embedding-model cache (fieldcraft's local fallback store uses a fastembed model
  # for vector search; without a cache it re-downloads ~130MB on first query). Shared, read-only
  # content-addressed blobs — safe to reuse across every cold task run. Cloned via `cp -c`
  # (APFS copy-on-write clonefile) so this costs ~0 extra disk, not a real 130MB-per-task copy.
  MODEL_CACHE_SRC="$WORK_ROOT/shared_model_cache/models"
  if [[ -d "$MODEL_CACHE_SRC" ]]; then
    mkdir -p "$COLD_HOME/.ai-memory"
    cp -Rc "$MODEL_CACHE_SRC" "$COLD_HOME/.ai-memory/models" 2>/dev/null \
      || cp -R "$MODEL_CACHE_SRC" "$COLD_HOME/.ai-memory/models" 2>/dev/null
  fi

  # CODEX_HOME needs only auth + config to authenticate and run with the right sandbox/approval
  # policy; deliberately NOT copying AGENTS.md (that's the file whose fieldcraft directive we're
  # suppressing) or any session-state files (~/.codex has multi-GB state files that are pure
  # bloat here and irrelevant to a single non-interactive `codex exec` run).
  if [[ -f "$HOME/.codex/auth.json" ]]; then
    cp "$HOME/.codex/auth.json" "$COLD_CODEX_HOME/auth.json"
    chmod 600 "$COLD_CODEX_HOME/auth.json"
  fi
  if [[ -f "$HOME/.codex/config.toml" ]]; then
    cp "$HOME/.codex/config.toml" "$COLD_CODEX_HOME/config.toml"
  fi
fi

# ---------------------------------------------------------------------------
# 7. Build the agent prompt and run codex as the solving agent.
# ---------------------------------------------------------------------------
FTP_LIST=$(printf '%s\n' "${FAIL_TO_PASS[@]}" | sed 's/^/  - /')

PROMPT_FILE="$OUT_DIR/prompt.txt"
{
  echo "You are solving a real GitHub issue in this repository checkout. The repository is already"
  echo "checked out at the correct base commit. A failing-test update has already been applied so"
  echo "the listed tests are in their target (currently failing) state."
  echo ""
  echo "Problem statement:"
  echo "$PROBLEM_STATEMENT"
  echo ""
  echo "Tests that must PASS after your fix (do not modify any test file):"
  echo "$FTP_LIST"
  echo ""
  if [[ "$CONDITION" == "warm" && -n "$MEMORY_BLOCK" ]]; then
    echo "Fieldcraft memory (retrieved from a prior-session lesson store before you started):"
    echo "$MEMORY_BLOCK"
    echo ""
  fi
  if [[ "$CONDITION" == "cold" ]]; then
    echo "This is a COLD/memory-suppressed run for an evaluation harness. Do not run 'fieldcraft'"
    echo "yourself, and if any fieldcraft output appears in your environment anyway, IGNORE it"
    echo "entirely and solve this from first principles using only the problem statement above."
  fi
  echo "Make the minimal source code change needed (do not touch test files). When you believe the"
  echo "fix is complete and you have run the listed tests yourself to confirm, stop."
} > "$PROMPT_FILE"

CODEX_LOG="$OUT_DIR/codex_log.jsonl"
START_TS=$(date +%s)
if [[ "$CONDITION" == "cold" ]]; then
  # See section 6b above: HOME -> empty scratch dir (kills the zsh -lc ~/.fieldcraft/env
  # re-source), CODEX_HOME -> scratch dir with auth+config but no AGENTS.md (kills the
  # auto-query directive at the source). FIELDCRAFT_* explicitly unset defensively even though
  # the scratch HOME's login shell never re-exports them.
  env -u FIELDCRAFT_SERVER_URL -u FIELDCRAFT_UPSTREAM_URL -u FIELDCRAFT_AUTH_TOKEN \
    HOME="$COLD_HOME" CODEX_HOME="$COLD_CODEX_HOME" PATH="$VENV_SHARED/bin:$PATH" \
    codex exec --json --sandbox danger-full-access -C "$TASK_WORKTREE" \
    "$(cat "$PROMPT_FILE")" > "$CODEX_LOG" 2>"$OUT_DIR/codex_stderr.log"
else
  env PATH="$VENV_SHARED/bin:$PATH" codex exec --json --sandbox danger-full-access -C "$TASK_WORKTREE" \
    "$(cat "$PROMPT_FILE")" > "$CODEX_LOG" 2>"$OUT_DIR/codex_stderr.log"
fi
CODEX_EXIT=$?
END_TS=$(date +%s)
WALL_SECONDS=$((END_TS - START_TS))

echo "[$TASK_ID/$CONDITION] codex exit=$CODEX_EXIT wall=${WALL_SECONDS}s" >&2

# Post-hoc contamination check (cold only): did codex run `fieldcraft query` anyway, AND if so
# did it actually return real lesson content? With the isolation above, `fieldcraft query` (if
# invoked at all) hits the scratch HOME's empty local sqlite fallback, so even a stray
# invocation should be a documented non-event (empty result), not a real leak. We flag BOTH
# cases distinctly: cold_contaminated=true only if a `fieldcraft query` call's own captured
# output contains actual lesson content (a `lesson:<id>`-shaped hex id or non-"no lessons"
# text), so a harmless empty-store call doesn't falsely flag a clean run.
COLD_CONTAMINATED="false"
if [[ "$CONDITION" == "cold" ]]; then
  FC_CALLS=$(jq -c '[.[] | select(.type=="item.completed" and .item.type=="command_execution") | select(.item.command | test("fieldcraft query"))]' -s "$CODEX_LOG" 2>/dev/null)
  FC_CALL_COUNT=$(echo "$FC_CALLS" | jq 'length' 2>/dev/null || echo 0)
  if [[ "${FC_CALL_COUNT:-0}" -gt 0 ]]; then
    echo "[$TASK_ID/cold] NOTE: cold run invoked 'fieldcraft query' anyway ($FC_CALL_COUNT call(s)) despite isolation; checking whether the isolated store actually returned real lesson content..." >&2
    # A leak is real lesson content in the call's own output: either an explicit lesson:<hex>
    # id, or output that is NOT the empty-store message fieldcraft prints when its local
    # sqlite fallback has zero rows ("No lessons found for project ..."). Belt-and-suspenders
    # check directly against the captured aggregated_output of those specific calls.
    LEAKED=$(echo "$FC_CALLS" | jq -r '[.[] | .item.aggregated_output // ""] | join("\n")' 2>/dev/null)
    if echo "$LEAKED" | grep -qE 'lesson:[0-9a-f]{6,}' || ! echo "$LEAKED" | grep -qi "no lessons found"; then
      COLD_CONTAMINATED="true"
      echo "[$TASK_ID/cold] WARNING: isolated 'fieldcraft query' call returned non-empty content. Flagged cold_contaminated=true." >&2
    else
      echo "[$TASK_ID/cold] OK: isolated 'fieldcraft query' call(s) returned empty ('No lessons found') — no real leak, cold_contaminated stays false." >&2
    fi
  fi
fi

# ---------------------------------------------------------------------------
# 8. Extract step count + token count from the codex JSON stream via jq (real numbers):
#    steps = count of command_execution + file_change item.completed events; tokens = sum
#    of input_tokens + output_tokens across all turn.completed events.
# ---------------------------------------------------------------------------
STEPS=$(jq -s '[.[] | select(.type == "item.completed") | select(.item.type == "command_execution" or .item.type == "file_change")] | length' "$CODEX_LOG" 2>/dev/null || echo 0)
TOKENS=$(jq -s '[.[] | select(.type == "turn.completed") | (.usage.input_tokens // 0) + (.usage.output_tokens // 0)] | add // 0' "$CODEX_LOG" 2>/dev/null || echo 0)
STEPS=${STEPS:-0}
TOKENS=${TOKENS:-0}

# warm_expected_source_hit: did the EXACT written lesson id show up in the prompt's memory
# block (i.e. retrieval actually surfaced the seeded lesson)? Real ID match, not a pattern
# guess — this is the fix for the documented Gauntlet ID-format mismatch.
SOURCE_HIT="null"
if [[ "$CONDITION" == "warm" ]]; then
  if [[ -n "$WARM_LESSON_ID" ]] && echo "$MEMORY_BLOCK" | grep -q "$WARM_LESSON_ID"; then
    SOURCE_HIT="true"
  else
    SOURCE_HIT="false"
  fi
fi

# ---------------------------------------------------------------------------
# 9. Real oracle: snapshot the agent's diff, REINSTALL so the oracle tests the agent's
#    actual patched code (not the stale pre-agent install from step 4 — a real bug we hit
#    during harness development: the agent edits files in the worktree, but pip's
#    site-packages copy from the earlier `pip install --force-reinstall .` does not
#    automatically pick up post-install file edits unless we reinstall again), then run
#    FAIL_TO_PASS + PASS_TO_PASS for real.
# ---------------------------------------------------------------------------
( cd "$TASK_WORKTREE" && git diff > "$OUT_DIR/patch_full.diff" 2>/dev/null )
( cd "$TASK_WORKTREE" && "$VENV_SHARED/bin/pip" install -q --force-reinstall . ) >&2 2>&1 | tail -20 >&2
# Re-pin setuptools<81 defensively: we observed the agent itself sometimes runs `pip
# install` during its own verification (e.g. diagnosing the same pkg_resources issue we
# document above and fixing it FORWARD to a too-new setuptools), and a force-reinstall of
# the target package can pull build-system requirements that bump it back. Re-asserting the
# pin here, immediately before the oracle runs, removes the ambiguity entirely rather than
# debugging exactly which install step caused the drift.
"$VENV_SHARED/bin/pip" install -q "setuptools<81" >&2 2>&1 | tail -5 >&2

POST_FTP_REPORT="$OUT_DIR/_post_ftp_report.json"
POST_PTP_REPORT="$OUT_DIR/_post_ptp_report.json"
read -r FTP_PASS FTP_TOTAL < <(run_pytest_subset "$POST_FTP_REPORT" "${FAIL_TO_PASS[@]}")
read -r PTP_PASS PTP_TOTAL < <(run_pytest_subset "$POST_PTP_REPORT" "${PASS_TO_PASS[@]}")
# FTP_TOTAL/PTP_TOTAL are the COLLECTED counts (tests pytest could actually resolve and run),
# not the raw requested-id counts — see run_pytest_subset's comment. A handful of dataset
# node ids are malformed (truncated parametrize brackets); we do not penalize the agent's
# fix for a pre-existing dataset defect outside its control.
FTP_REQUESTED=${#FAIL_TO_PASS[@]}
PTP_REQUESTED=${#PASS_TO_PASS[@]}
if [[ "$FTP_TOTAL" -lt "$FTP_REQUESTED" || "$PTP_TOTAL" -lt "$PTP_REQUESTED" ]]; then
  echo "[$TASK_ID/$CONDITION] NOTE: dataset has uncollectable node ids — FAIL_TO_PASS requested=$FTP_REQUESTED collected=$FTP_TOTAL, PASS_TO_PASS requested=$PTP_REQUESTED collected=$PTP_TOTAL" >&2
fi

RESOLVED="false"
# Require FTP_TOTAL > 0 (the regression target actually collected) for FAIL_TO_PASS. For
# PASS_TO_PASS, require it to be > 0 too UNLESS the task genuinely has zero PASS_TO_PASS
# tests in the dataset to begin with (PTP_REQUESTED == 0) — a real bug we hit during harness
# development: when every PASS_TO_PASS node id failed to collect (PTP_TOTAL=0) while the
# dataset actually requested >0 tests (PTP_REQUESTED>0), the old "0 == 0" check silently
# passed, marking a broken oracle run as resolved=true.
if [[ "$FTP_PASS" -eq "$FTP_TOTAL" && "$FTP_TOTAL" -gt 0 \
      && "$PTP_PASS" -eq "$PTP_TOTAL" \
      && ( "$PTP_TOTAL" -gt 0 || "$PTP_REQUESTED" -eq 0 ) ]]; then
  RESOLVED="true"
fi

WARM_LESSON_ID_JSON="null"
if [[ "$CONDITION" == "warm" ]]; then
  WARM_LESSON_ID_JSON="\"$WARM_LESSON_ID\""
fi

jq -n \
  --arg task_id "$TASK_ID" \
  --arg condition "$CONDITION" \
  --argjson resolved "$RESOLVED" \
  --argjson ftp_pass "$FTP_PASS" \
  --argjson ftp_total "$FTP_TOTAL" \
  --argjson ptp_pass "$PTP_PASS" \
  --argjson ptp_total "$PTP_TOTAL" \
  --argjson precheck "$PRECHECK_PASS_COUNT" \
  --argjson steps "$STEPS" \
  --argjson tokens "$TOKENS" \
  --argjson wall_seconds "$WALL_SECONDS" \
  --argjson codex_exit "$CODEX_EXIT" \
  --argjson warm_lesson_id "$WARM_LESSON_ID_JSON" \
  --argjson source_hit "$SOURCE_HIT" \
  --argjson cold_contaminated "$COLD_CONTAMINATED" \
  '{
    task_id: $task_id,
    condition: $condition,
    resolved: $resolved,
    fail_to_pass_pass: $ftp_pass,
    fail_to_pass_total: $ftp_total,
    pass_to_pass_pass: $ptp_pass,
    pass_to_pass_total: $ptp_total,
    precheck_fail_to_pass_pass_before_agent: $precheck,
    steps: $steps,
    tokens: $tokens,
    wall_seconds: $wall_seconds,
    codex_exit_code: $codex_exit,
    warm_lesson_id: $warm_lesson_id,
    warm_expected_source_hit: $source_hit,
    cold_contaminated: $cold_contaminated
  }' > "$OUT_DIR/result.json"

cat "$OUT_DIR/result.json"

# ---------------------------------------------------------------------------
# 10. Cleanup: drop the worktree (keeps disk bounded) but keep venv (shared/reused) and logs.
#     Also drop the cold scratch HOME/CODEX_HOME (contains a copy of the real codex auth token
#     plus the local-fallback fieldcraft sqlite store) — no reason to leave auth material or
#     even an empty DB lying around per task once the run is over.
# ---------------------------------------------------------------------------
( cd "$REPO_DIR" && git worktree remove --force "$TASK_WORKTREE" 2>/dev/null )
if [[ -n "$COLD_HOME" ]]; then
  rm -rf "$COLD_HOME" "$COLD_CODEX_HOME"
fi

exit 0
