#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

cleanup() {
  rm -rf "${TMP_ROOT:-}"
}
trap cleanup EXIT

echo "[pre-push] cargo test"
cargo test -q

echo "[pre-push] cargo build"
cargo build -q -p fieldcraft-cli -p fieldcraft-server

echo "[pre-push] cli smoke test"
TMP_ROOT="$(mktemp -d)"
export HOME="$TMP_ROOT"

bin="./target/debug/fieldcraft"

"$bin" write "Popup dismiss regression fix through PopUpView requestPopUpDismissal"
result="$("$bin" query "popup dismiss")"
if [[ -z "$result" ]]; then
  echo "cli smoke test failed: query returned empty output"
  exit 1
fi

echo "[pre-push] PASS"
