#!/usr/bin/env bash
# One Jev call for one logged query. Args: <out_dir> <rid>.
# Writes responses/<rid>.json and responses/<rid>.meta ("<http_code> <seconds>").
set -uo pipefail
OUT=$1; RID=$2
REQ="$OUT/requests/$RID.json"
RES="$OUT/responses/$RID.json"
[[ -s "$RES" ]] && exit 0
code_time=$(curl -sS -m 90 --retry 4 --retry-delay 3 \
  -H @"$OUT/.hdr" -H 'Content-Type: application/json' \
  -X POST https://api.typesafe.ai/v1/systemone \
  --data-binary @"$REQ" -o "$RES.tmp" -w '%{http_code} %{time_total}' 2>/dev/null) || true
code=${code_time%% *}
if [[ "$code" == "200" ]] && jq -e '.answers' "$RES.tmp" >/dev/null 2>&1; then
  mv "$RES.tmp" "$RES"
else
  mv "$RES.tmp" "$RES.err" 2>/dev/null || true
fi
echo "$code_time" > "$OUT/responses/$RID.meta"
