#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
label="${FIELDCRAFT_LAUNCHD_LABEL:-com.fieldcraft.server}"
port="${FIELDCRAFT_SERVER_PORT:-${FIELDCRAFT_LOCAL_SERVER_PORT:-7777}}"

if [ -n "${FIELDCRAFT_DATA_DIR:-}" ]; then
  data_dir="$FIELDCRAFT_DATA_DIR"
elif [ -d "$HOME/.dam" ]; then
  data_dir="$HOME/.dam"
else
  data_dir="$HOME/.fieldcraft"
fi

launch_agents_dir="$HOME/Library/LaunchAgents"
plist="$launch_agents_dir/$label.plist"
old_plist="$launch_agents_dir/com.ohapotato.tr-server.plist"
old_label="com.ohapotato.tr-server"
backup_dir="$data_dir/backups/fieldcraft-update-$(date +%Y%m%d-%H%M%S)"

mkdir -p "$data_dir" "$backup_dir" "$launch_agents_dir"

backup_sqlite() {
  local src="$1"
  local dst="$2"
  [ -f "$src" ] || return 0
  if command -v sqlite3 >/dev/null 2>&1; then
    sqlite3 "$src" ".backup \"$dst\""
  else
    cp "$src" "$dst"
  fi
}

backup_sqlite "$data_dir/memory.sqlite3" "$backup_dir/memory.sqlite3"
backup_sqlite "$data_dir/tokens.sqlite3" "$backup_dir/tokens.sqlite3"
backup_sqlite "$data_dir/.tr-source-sessions.sqlite3" "$backup_dir/.tr-source-sessions.sqlite3"
backup_sqlite "$data_dir/sessions/.tr-source-sessions.sqlite3" "$backup_dir/sessions.tr-source-sessions.sqlite3"

cd "$repo_root"
cargo install --path crates/fieldcraft-cli --locked --force
cargo install --path crates/fieldcraft-server --locked --force

python3 - "$plist" "$old_plist" "$HOME/.cargo/bin/fieldcraft-server" "$data_dir" "$port" "$HOME" "$label" <<'PY'
import os
import plistlib
import sys

plist_path, old_plist, server_bin, data_dir, port, home, label = sys.argv[1:]
env = {}
for candidate in (plist_path, old_plist):
    if os.path.exists(candidate):
        with open(candidate, "rb") as f:
            env.update(plistlib.load(f).get("EnvironmentVariables", {}) or {})
env["RUST_LOG"] = os.environ.get("RUST_LOG", "info,fieldcraft_server=debug")
# The server reads <data_dir>/secrets.env itself; a plist copy would shadow a rotated
# file value (environment wins), so drop plist copies of keys the file defines.
secrets_path = os.path.join(data_dir, "secrets.env")
if os.path.exists(secrets_path):
    file_keys = set()
    for line in open(secrets_path, encoding="utf-8"):
        line = line.strip()
        if line and not line.startswith("#") and "=" in line:
            file_keys.add(line.removeprefix("export ").split("=", 1)[0].strip())
    for key in ("GEMINI_API_KEY", "GOOGLE_API_KEY", "FIELDCRAFT_ENROLL_SECRET"):
        if key in env and key in file_keys:
            del env[key]
            print(f"  {key}: removed plist copy; {secrets_path} is the source of truth")

plist = {
    "Label": label,
    "ProgramArguments": [server_bin, "--data-dir", data_dir, "--port", port],
    "WorkingDirectory": home,
    "RunAtLoad": True,
    "KeepAlive": True,
    "ProcessType": "Background",
    "EnvironmentVariables": env,
    "StandardOutPath": os.path.join(home, "Library/Logs/fieldcraft-server.log"),
    "StandardErrorPath": os.path.join(home, "Library/Logs/fieldcraft-server.err.log"),
}

with open(plist_path, "wb") as f:
    plistlib.dump(plist, f)
PY

wait_for_server() {
  for _ in $(seq 1 30); do
    if curl -fsS "http://127.0.0.1:$port/health" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.2
  done
  echo "ERROR: fieldcraft-server did not become healthy at http://127.0.0.1:$port" >&2
  return 1
}

uid="$(id -u)"
launchctl bootout "gui/$uid/$old_label" 2>/dev/null || true
launchctl bootout "gui/$uid/$label" 2>/dev/null || true
if ! launchctl bootstrap "gui/$uid" "$plist"; then
  echo "launchd bootstrap failed; retrying after cleanup..." >&2
  launchctl bootout "gui/$uid/$label" 2>/dev/null || true
  pkill -x fieldcraft-server 2>/dev/null || true
  sleep 0.5
  launchctl bootstrap "gui/$uid" "$plist"
fi
launchctl kickstart -k "gui/$uid/$label"
wait_for_server

echo "Fieldcraft server installed"
echo "  label: $label"
echo "  data_dir: $data_dir"
echo "  port: $port"
echo "  backup_dir: $backup_dir"
