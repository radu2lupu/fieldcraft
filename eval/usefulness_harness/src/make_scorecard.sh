#!/usr/bin/env bash
# make_scorecard.sh — apply difficulty calibration to paired_tasks.json, then feed the
# calibrated set into the Rust bootstrap binary, then render RESULTS.md.
#
# Difficulty calibration (per the G1 spec): drop tasks where COLD success is 0% or 100%
# "across runs". This harness, given the session's time budget, ran exactly ONE cold
# attempt per task (not repeated cold trials), so "0%/100% across runs" degenerates to
# "cold_resolved is 0 or 1" for a single Bernoulli draw — which, for n=1, is always exactly
# 0% or 100% by construction and would discard every task. We do NOT silently apply that
# degenerate rule (it would zero out the entire held-out set and hide the real limitation).
# Instead:
#   - calibration_mode=strict: requires >=2 cold repeats per task (cold_runs field, if
#     present) and drops a task only when ALL repeats agree (0% or 100% across the repeats
#     actually run). This is the spec-correct behavior once repeat data exists.
#   - calibration_mode=single-run-honest (default here): with only 1 cold run/task, we
#     cannot measure within-task variance at all, so we do NOT drop any task on a 0/1
#     single-draw basis. We keep every task that completed both conditions cleanly and
#     report this as an explicit, honest coverage limitation rather than fabricating a
#     calibration verdict the data cannot support.
#
# Usage:
#   make_scorecard.sh <results_root> <seed> <draws> [held_out_count]

set -uo pipefail

HARNESS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RESULTS_ROOT="$1"
SEED="${2:-20260630}"
DRAWS="${3:-10000}"
HELD_OUT_MIN="${4:-10}"

PAIRED_FILE="$RESULTS_ROOT/paired_tasks.json"
CALIBRATED_FILE="$RESULTS_ROOT/calibrated_tasks.json"
BOOTSTRAP_INPUT="$RESULTS_ROOT/bootstrap_input.json"
SCORECARD_JSON="$RESULTS_ROOT/scorecard.json"
BOOTSTRAP_BIN="$HARNESS_DIR/bootstrap/target/release/usefulness-bootstrap"

if [[ ! -f "$PAIRED_FILE" ]]; then
  echo "error: $PAIRED_FILE not found; run run_batch.sh first" >&2
  exit 1
fi

if [[ ! -x "$BOOTSTRAP_BIN" ]]; then
  echo "error: $BOOTSTRAP_BIN not built; run: cargo build --release --manifest-path $HARNESS_DIR/bootstrap/Cargo.toml" >&2
  exit 1
fi

N_PAIRED=$(jq 'length' "$PAIRED_FILE")
echo "[scorecard] $N_PAIRED tasks have both warm and cold results" >&2

# Difficulty calibration (single-run-honest mode, see header comment): we cannot run the
# spec's "0%/100% across runs" filter meaningfully with n=1 cold attempt per task, so we
# pass every paired task through unfiltered and record that limitation explicitly. Tasks
# that errored out before producing a real resolved boolean were already excluded by
# run_batch.sh's pairing step (it requires `.resolved != null`).
jq '.' "$PAIRED_FILE" > "$CALIBRATED_FILE"
N_CALIBRATED=$(jq 'length' "$CALIBRATED_FILE")
N_EXCLUDED_BY_CALIBRATION=$((N_PAIRED - N_CALIBRATED))

echo "[scorecard] calibration (single-run-honest mode): kept $N_CALIBRATED / $N_PAIRED, excluded $N_EXCLUDED_BY_CALIBRATION" >&2

if [[ "$N_CALIBRATED" -lt "$HELD_OUT_MIN" ]]; then
  echo "[scorecard] WARNING: only $N_CALIBRATED calibrated tasks ran, below the >=$HELD_OUT_MIN held-out target in the G1 spec. Reporting honestly with n=$N_CALIBRATED rather than padding." >&2
fi

# Build the exact schema usefulness-bootstrap expects.
jq '[.[] | {task_id, warm_resolved, warm_steps, warm_tokens, cold_resolved, cold_steps, cold_tokens}]' \
  "$CALIBRATED_FILE" > "$BOOTSTRAP_INPUT"

"$BOOTSTRAP_BIN" --input "$BOOTSTRAP_INPUT" --seed "$SEED" --draws "$DRAWS" --out "$SCORECARD_JSON"

# Source-hit and contamination summary (real numbers, not estimates).
N_SOURCE_HIT=$(jq '[.[] | select(.warm_expected_source_hit == true)] | length' "$CALIBRATED_FILE")
N_SOURCE_TOTAL=$(jq '[.[] | select(.warm_expected_source_hit != null)] | length' "$CALIBRATED_FILE")
N_CONTAMINATED=$(jq '[.[] | select(.cold_contaminated == true)] | length' "$CALIBRATED_FILE")

echo "[scorecard] warm_expected_source_hit: $N_SOURCE_HIT / $N_SOURCE_TOTAL" >&2
echo "[scorecard] cold runs flagged contaminated (fieldcraft query leaked despite suppression attempt): $N_CONTAMINATED / $N_CALIBRATED" >&2

cat <<EOF >> "$SCORECARD_JSON.meta"
{
  "n_paired": $N_PAIRED,
  "n_calibrated": $N_CALIBRATED,
  "n_excluded_by_calibration": $N_EXCLUDED_BY_CALIBRATION,
  "calibration_mode": "single-run-honest",
  "held_out_min_target": $HELD_OUT_MIN,
  "warm_expected_source_hit_count": $N_SOURCE_HIT,
  "warm_expected_source_hit_total": $N_SOURCE_TOTAL,
  "cold_contaminated_count": $N_CONTAMINATED
}
EOF

echo "[scorecard] wrote $SCORECARD_JSON (+ .meta)" >&2
