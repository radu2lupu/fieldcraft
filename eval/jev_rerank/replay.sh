#!/usr/bin/env bash
# Replay logged Fieldcraft queries through Jev (TypeSafe System One) as a
# reranker over the lessons that were actually served, then score the new
# order against the lessons the session later cited. See README.md.
#
# Usage: replay.sh <queries.jsonl> <lessons.jsonl> <out_dir> [limit] [parallel]
#   JEV_API_KEY must be set. JEV_MODEL defaults to jev-latest.
set -euo pipefail

QUERIES=$1; LESSONS=$2; OUT=$3; LIMIT=${4:-0}; PAR=${5:-6}
: "${JEV_API_KEY:?set JEV_API_KEY}"
DIR="$(cd "$(dirname "$0")" && pwd)"
MODEL=${JEV_MODEL:-jev-latest}
BUILD=${JEV_BUILD:-$DIR/build.jq}   # build.jq (one Noul per candidate) or build_choice.jq

mkdir -p "$OUT/requests" "$OUT/responses"
HDR="$OUT/.hdr"
(umask 077; printf 'Authorization: Bearer %s\n' "$JEV_API_KEY" > "$HDR")
trap 'rm -f "$HDR"' EXIT

# Lesson lookup map, then one request per query (limit applies to the input).
jq -s 'map({(.id): .}) | add' "$LESSONS" > "$OUT/map.json"
if [[ "$LIMIT" -gt 0 ]]; then
  head -n "$LIMIT" "$QUERIES"
else
  cat "$QUERIES"
fi | jq -c --arg model "$MODEL" --slurpfile m "$OUT/map.json" -f "$BUILD" \
   | jq -r '"\(.rid)\t\(.request | tojson)"' \
   | awk -F'\t' -v out="$OUT/requests" '{ f = out "/" $1 ".json"; print $2 > f; close(f) }'

echo "requests: $(ls "$OUT/requests" | wc -l | tr -d ' ')"

# Call Jev; skip requests that already have a response so reruns resume.
ls "$OUT/requests" | sed 's/\.json$//' \
  | xargs -P "$PAR" -I{} "$DIR/call_one.sh" "$OUT" {}

echo "responses: $(ls "$OUT/responses" | grep -c '\.json$')"
grep -h -v '^200 ' "$OUT"/responses/*.meta 2>/dev/null | sort | uniq -c | sed 's/^/non-200: /' || true
