#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

tmp_root=""
server_pid=""
server_log=""

fail() {
  echo "[loop-e2e] FAIL: $*" >&2
  exit 1
}

cleanup() {
  local status=$?
  if [[ $status -ne 0 && -n "${server_log:-}" && -f "$server_log" ]]; then
    echo "[loop-e2e] server log tail:" >&2
    tail -120 "$server_log" >&2 || true
  fi
  if [[ -n "${server_pid:-}" ]] && kill -0 "$server_pid" 2>/dev/null; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  if [[ -n "${tmp_root:-}" ]]; then
    rm -rf "$tmp_root"
  fi
  exit "$status"
}
trap cleanup EXIT

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

assert_gt_zero() {
  local value="$1"
  local label="$2"
  [[ "$value" =~ ^[0-9]+$ ]] || fail "$label is not an integer: $value"
  (( value > 0 )) || fail "$label must be > 0, got $value"
}

sqlite_scalar() {
  sqlite3 -batch -noheader "$data_dir/memory.sqlite3" "$1"
}

json_token() {
  sed -n 's/.*"token"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p'
}

pick_port() {
  local port
  for _ in {1..100}; do
    port=$((20000 + RANDOM % 30000))
    if ! lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then
      printf '%s\n' "$port"
      return 0
    fi
  done
  return 1
}

wait_for_health() {
  local base_url="$1"
  for _ in {1..100}; do
    if ! kill -0 "$server_pid" 2>/dev/null; then
      fail "fieldcraft-server exited before /health was ready"
    fi
    if [[ "$(curl -fsS "$base_url/health" 2>/dev/null || true)" == "ok" ]]; then
      return 0
    fi
    sleep 0.1
  done
  fail "timed out waiting for $base_url/health"
}

require_cmd cargo
require_cmd curl
require_cmd git
require_cmd lsof
require_cmd sed
require_cmd sqlite3

server_bin="$repo_root/target/release/fieldcraft-server"
cli_bin="$repo_root/target/release/fieldcraft"

if [[ ! -x "$server_bin" || ! -x "$cli_bin" ]]; then
  echo "[loop-e2e] building release binaries"
  cargo build --release -p fieldcraft-server -p fieldcraft-cli
else
  echo "[loop-e2e] reusing release binaries"
fi

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/fieldcraft-loop-e2e.XXXXXX")"
data_dir="$tmp_root/data"
home_dir="$tmp_root/home"
git_repo="$tmp_root/fieldcraft-loop-repo"
transcript="$tmp_root/transcript.jsonl"
server_log="$tmp_root/server.log"
mkdir -p "$data_dir" "$home_dir" "$git_repo/src"

port="$(pick_port)" || fail "could not find a free port"
base_url="http://127.0.0.1:$port"

echo "[loop-e2e] starting server on $base_url"
(
  cd "$repo_root"
  exec env \
    -u FIELDCRAFT_UPSTREAM_URL \
    -u FIELDCRAFT_UPSTREAM_AUTH_TOKEN \
    RUST_LOG=info,fieldcraft_server=debug,tower_http=debug \
    "$server_bin" --data-dir "$data_dir" --port "$port"
) >"$server_log" 2>&1 &
server_pid=$!

wait_for_health "$base_url"

enroll_body='{"label":"loop-e2e"}'
enroll_json="$(curl -fsS -X POST "$base_url/enroll" \
  -H 'content-type: application/json' \
  --data "$enroll_body")"
token="$(printf '%s\n' "$enroll_json" | json_token)"
[[ -n "$token" ]] || fail "POST /enroll did not return a token: $enroll_json"

export HOME="$home_dir"
export FIELDCRAFT_SERVER_URL="$base_url"
export FIELDCRAFT_AUTH_TOKEN="$token"
unset FIELDCRAFT_UPSTREAM_URL FIELDCRAFT_UPSTREAM_AUTH_TOKEN

git -C "$git_repo" init -q
printf 'pub fn widget() -> &' >"$git_repo/src/widget.rs"
git -C "$git_repo" add src/widget.rs
git -C "$git_repo" -c user.name=LoopE2E -c user.email=loop-e2e@example.test commit -qm "seed widget"
printf 'pub fn widget() -> &' >"$git_repo/src/widget.rs"
printf '\n// loop e2e changed\n' >>"$git_repo/src/widget.rs"
[[ "$(git -C "$git_repo" diff --name-only)" == "src/widget.rs" ]] \
  || fail "temp git repo did not report src/widget.rs in git diff --name-only"

project_id="$(basename "$git_repo")"
session_id="loop-e2e-session-$(date -u +%Y%m%d%H%M%S)-$$"
lesson_id="feedface$(date -u +%Y%m%d%H%M%S)$$"
sentinel="loop-e2e file overlap widget sentinel $lesson_id"
created_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

record_json="$(cat <<JSON
{
  "id": "$lesson_id",
  "project_id": "$project_id",
  "kind": "claim",
  "title": "$sentinel",
  "payload_json": "{\"statement\":\"$sentinel confirms src/widget.rs overlap telemetry\",\"mechanism\":\"server e2e file overlap confirmation\",\"scope\":\"loop-e2e\",\"exceptions\":[],\"affected_files\":[\"src/widget.rs\"],\"retrieval_questions\":[\"$sentinel\"],\"trigger\":\"loop-e2e\",\"invalid_if\":null}",
  "state": "active",
  "confidence": 0.5,
  "source": {"type": "manual"},
  "superseded_by": null,
  "project_specific": true,
  "created_at": "$created_at",
  "valid_until": null,
  "retrieval_text": "$sentinel src/widget.rs file_overlap retrieval_log query_cue_bundle",
  "schema_version": 4,
  "phase": "edit",
  "tier": "active",
  "owner": null,
  "session_date": null,
  "source_refs": []
}
JSON
)"

echo "[loop-e2e] POST /lessons $lesson_id"
curl -fsS -X POST "$base_url/lessons" \
  -H "authorization: Bearer $token" \
  -H 'content-type: application/json' \
  --data "$record_json" >/dev/null

echo "[loop-e2e] query through CLI with session id"
query_output="$("$cli_bin" query "$sentinel" --project "$project_id" --session-id "$session_id" --limit 5)"
printf '%s\n' "$query_output" | grep -F "$sentinel" >/dev/null \
  || fail "CLI query did not retrieve lesson $lesson_id"

cat >"$transcript" <<JSONL
{"timestamp":"2000-01-01T00:00:00Z","type":"user","text":"Run the loop e2e."}
{"timestamp":"2999-01-01T00:00:00Z","type":"assistant","text":"Applied the retrieved widget overlap lesson to src/widget.rs."}
JSONL

echo "[loop-e2e] trace-analyze"
"$cli_bin" trace-analyze --session "$session_id" --transcript "$transcript" --cwd "$git_repo"

qcb_count="$(sqlite_scalar "SELECT COUNT(*) FROM retrieval_log WHERE session_id = '$session_id' AND query_cue_bundle IS NOT NULL AND TRIM(query_cue_bundle) <> '' AND result_ids LIKE '%$lesson_id%';")"
assert_gt_zero "$qcb_count" "retrieval_log.query_cue_bundle rows"

confirmed_count="$(sqlite_scalar "SELECT COUNT(*) FROM lesson_outcome_events WHERE knowledge_id = '$lesson_id' AND session_id = '$session_id' AND outcome = 'confirmed' AND note LIKE '%file_overlap%';")"
assert_gt_zero "$confirmed_count" "confirmed file_overlap outcome rows"

confidence_increased="$(sqlite_scalar "SELECT COUNT(*) FROM knowledge WHERE id = '$lesson_id' AND dynamic_confidence > confidence;")"
assert_gt_zero "$confidence_increased" "dynamic_confidence increase rows"

stats_output="$("$cli_bin" stats --days 7)"
stats_confirmed="$(printf '%s\n' "$stats_output" | sed -n 's/[[:space:]]*- confirmed:[[:space:]]*\([0-9][0-9]*\).*/\1/p' | head -n 1)"
stats_qcb="$(printf '%s\n' "$stats_output" | sed -n 's/[[:space:]]*- retrieval_log rows with query_cue_bundle:[[:space:]]*\([0-9][0-9]*\).*/\1/p' | head -n 1)"
assert_gt_zero "$stats_confirmed" "fieldcraft stats confirmed"
assert_gt_zero "$stats_qcb" "fieldcraft stats retrieval_log_with_query_cue_bundle"

echo "[loop-e2e] relevant server log lines"
grep -E 'request|/health|/enroll|/lessons|trace-analyze|lesson written|token|fieldcraft-server listening' "$server_log" || true

echo "[loop-e2e] PASS"
