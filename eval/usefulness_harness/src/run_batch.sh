#!/usr/bin/env bash
# run_batch.sh — drive run_task.sh across a list of task ids, both conditions, sequentially.
# Writes <results_root>/<condition>/<task_id>/result.json per run (via run_task.sh) and a
# consolidated <results_root>/paired_tasks.json (jq, no Python) once both conditions exist
# for a task, ready to feed straight into the Rust bootstrap binary.
#
# Usage:
#   run_batch.sh <task_ids_or_selector> <work_root> <results_root> <tasks_json> <lessons_json>
#
# <task_ids_or_selector> (difficulty-calibration hook, G1 packet item 4):
#   - Space-separated literal instance ids (original behavior, unchanged):
#       run_batch.sh "pytest-dev__pytest-5809 pytest-dev__pytest-7571" ...
#   - "@<difficulty-label>" selects every task in tasks_json whose .difficulty field equals
#     <difficulty-label> exactly as it appears in the SWE-bench Verified dataset, e.g.:
#       run_batch.sh "@<15 min fix"       ...   # the original proven-easy tier (0 headroom)
#       run_batch.sh "@15 min - 1 hour"   ...   # medium tier
#       run_batch.sh "@1-4 hours"         ...   # harder tier WITH resolution headroom — this
#                                                # is the set to use once a run needs cold to
#                                                # NOT trivially solve every task (the first run's
#                                                # documented resolved_delta=0 problem). Must be
#                                                # quoted as a single shell argument.
#   - "@all" selects every task_id present in tasks_json, any difficulty.
#   A later run can mix tiers manually too: pass a literal space-separated list assembled with
#   `jq -r '.[] | select(.difficulty=="1-4 hours") | .instance_id' tasks/tasks.json | tr '\n' ' '`
#   and anything else you want appended, since the selector and literal-list forms are just two
#   ways of producing the same space-separated TASK_IDS string consumed below.

set -uo pipefail

HARNESS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TASK_IDS_RAW="$1"
WORK_ROOT="$2"
RESULTS_ROOT="$3"
TASKS_JSON="$4"
LESSONS_JSON="$5"

if [[ "$TASK_IDS_RAW" == @* ]]; then
  SELECTOR="${TASK_IDS_RAW#@}"
  if [[ "$SELECTOR" == "all" ]]; then
    TASK_IDS=$(jq -r '.[].instance_id' "$TASKS_JSON" | tr '\n' ' ')
  else
    TASK_IDS=$(jq -r --arg d "$SELECTOR" '.[] | select(.difficulty == $d) | .instance_id' "$TASKS_JSON" | tr '\n' ' ')
  fi
  echo "[batch] selector '@$SELECTOR' resolved to: $TASK_IDS" >&2
  if [[ -z "${TASK_IDS// /}" ]]; then
    echo "error: selector '@$SELECTOR' matched zero tasks in $TASKS_JSON (check the difficulty label spelling, e.g. '1-4 hours')" >&2
    exit 1
  fi
else
  TASK_IDS="$TASK_IDS_RAW"
fi

mkdir -p "$RESULTS_ROOT"
SUMMARY_LOG="$RESULTS_ROOT/batch_run_log.txt"
: > "$SUMMARY_LOG"

for TASK_ID in $TASK_IDS; do
  for CONDITION in cold warm; do
    echo "=== [$(date '+%Y-%m-%d %H:%M:%S')] starting $TASK_ID / $CONDITION ===" | tee -a "$SUMMARY_LOG"
    bash "$HARNESS_DIR/src/run_task.sh" "$TASK_ID" "$CONDITION" "$WORK_ROOT" "$RESULTS_ROOT" "$TASKS_JSON" "$LESSONS_JSON" \
      2>&1 | tee -a "$SUMMARY_LOG"
    STATUS=${PIPESTATUS[0]}
    echo "=== [$(date '+%Y-%m-%d %H:%M:%S')] finished $TASK_ID / $CONDITION exit=$STATUS ===" | tee -a "$SUMMARY_LOG"
  done
done

# ---------------------------------------------------------------------------
# Assemble paired_tasks.json: only tasks that have BOTH a warm and a cold result.json with
# no setup error, in the schema usefulness-bootstrap expects.
# ---------------------------------------------------------------------------
PAIRED_FILE="$RESULTS_ROOT/paired_tasks.json"
echo "[]" > "$PAIRED_FILE"

for TASK_ID in $TASK_IDS; do
  WARM_RESULT="$RESULTS_ROOT/warm/$TASK_ID/result.json"
  COLD_RESULT="$RESULTS_ROOT/cold/$TASK_ID/result.json"
  if [[ -f "$WARM_RESULT" && -f "$COLD_RESULT" ]] \
     && jq -e '.resolved != null' "$WARM_RESULT" >/dev/null 2>&1 \
     && jq -e '.resolved != null' "$COLD_RESULT" >/dev/null 2>&1; then
    ENTRY=$(jq -n \
      --arg task_id "$TASK_ID" \
      --argjson warm "$(cat "$WARM_RESULT")" \
      --argjson cold "$(cat "$COLD_RESULT")" \
      '{
        task_id: $task_id,
        warm_resolved: $warm.resolved,
        warm_steps: $warm.steps,
        warm_tokens: $warm.tokens,
        cold_resolved: $cold.resolved,
        cold_steps: $cold.steps,
        cold_tokens: $cold.tokens,
        warm_expected_source_hit: $warm.warm_expected_source_hit,
        cold_contaminated: $cold.cold_contaminated
      }')
    jq --argjson entry "$ENTRY" '. + [$entry]' "$PAIRED_FILE" > "$PAIRED_FILE.tmp" && mv "$PAIRED_FILE.tmp" "$PAIRED_FILE"
    echo "[batch] paired $TASK_ID into $PAIRED_FILE" | tee -a "$SUMMARY_LOG"
  else
    echo "[batch] SKIPPING $TASK_ID: missing or incomplete warm/cold result" | tee -a "$SUMMARY_LOG"
  fi
done

echo "[batch] done. $(jq 'length' "$PAIRED_FILE") tasks paired in $PAIRED_FILE" | tee -a "$SUMMARY_LOG"
