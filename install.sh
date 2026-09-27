#!/usr/bin/env bash
# Fieldcraft install script. Idempotent; safe to re-run after a `git pull`.
#
# Usage:
#   bash install.sh                                  # full local install
#   bash install.sh --server-url URL [--auth-token T] [--label NAME] [--enroll-secret S]
#                                                    # client mode
#   bash install.sh --yes [--distiller none|codex|grok|claude] [--skip-build]
#                                                    # non-interactive
#
# On a terminal with no mode flags, install.sh builds the binaries and then hands
# over to the interactive wizard `fieldcraft setup`, which calls back into this
# script with --yes --skip-build for the wiring.
#
# Full local install:
#   Builds binaries and uses ~/.fieldcraft/memory.sqlite3 as the catalog.
#
# Client mode:
#   FIELDCRAFT_SERVER_URL points at the remote fieldcraft-server. A local
#   gateway LaunchAgent may also be installed, but the configured server URL is
#   the routing authority.
#
# Enrollment:
#   Without --auth-token the script mints a token via POST /enroll. A server
#   accepts that only from loopback unless FIELDCRAFT_ENROLL_SECRET is set on
#   it; pass the same value with --enroll-secret (or FIELDCRAFT_ENROLL_SECRET).

set -e

REPO="$(cd "$(dirname "$0")" && pwd)"
HOOKS_DIR="$HOME/.fieldcraft/hooks"
ENV_FILE="$HOME/.fieldcraft/env"
BIN_PATH="$HOME/.cargo/bin/fieldcraft"
LAUNCHD_LABEL="${FIELDCRAFT_LAUNCHD_LABEL:-com.fieldcraft.server}"
LOCAL_PORT="${FIELDCRAFT_LOCAL_SERVER_PORT:-7777}"
LOCAL_SERVER_URL="http://127.0.0.1:$LOCAL_PORT"
SHELL_ENV_START="# fieldcraft-env:start"
SHELL_ENV_END="# fieldcraft-env:end"

SERVER_URL=""
AUTH_TOKEN=""
ENROLL_SECRET="${FIELDCRAFT_ENROLL_SECRET:-}"
YES=0
SKIP_BUILD=0
DISTILLER_CHOICE=""
LABEL="$(hostname -s 2>/dev/null || echo client)"

install_launch_agent() {
  local upstream_url="$1"
  local launch_agents_dir="$HOME/Library/LaunchAgents"
  local plist="$launch_agents_dir/$LAUNCHD_LABEL.plist"
  mkdir -p "$launch_agents_dir"
  python3 - "$plist" "$HOME/.cargo/bin/fieldcraft-server" "$HOME/.fieldcraft" "$LOCAL_PORT" "$HOME" "$LAUNCHD_LABEL" "$upstream_url" <<'PY'
import os
import plistlib
import sys

plist_path, server_bin, data_dir, port, home, label, upstream_url = sys.argv[1:]
args = [server_bin, "--data-dir", data_dir, "--port", port]
# Keep env vars an operator added to the plist (lifecycle flags, log level, ...).
env = {}
if os.path.exists(plist_path):
    with open(plist_path, "rb") as f:
        env.update(plistlib.load(f).get("EnvironmentVariables", {}) or {})
env["RUST_LOG"] = os.environ.get("RUST_LOG", "info,fieldcraft_server=debug")
# Secrets belong in ~/.fieldcraft/secrets.env, which the binaries read themselves and
# this script never rewrites. A copy in the plist would shadow a rotated file value
# (environment wins), so drop plist copies of keys the secrets file already defines.
secrets_path = os.path.join(data_dir, "secrets.env")
file_keys = set()
if os.path.exists(secrets_path):
    for line in open(secrets_path, encoding="utf-8"):
        line = line.strip()
        if line and not line.startswith("#") and "=" in line:
            file_keys.add(line.removeprefix("export ").split("=", 1)[0].strip())
for key in ("GEMINI_API_KEY", "GOOGLE_API_KEY", "FIELDCRAFT_ENROLL_SECRET"):
    if key in env and key in file_keys:
        del env[key]
        print(f"  {key}: removed plist copy; secrets.env is the source of truth")
if upstream_url:
    args = [server_bin, "--port", port, "--upstream-url", upstream_url]

plist = {
    "Label": label,
    "ProgramArguments": args,
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
os.chmod(plist_path, 0o600)
PY

  local uid
  uid="$(id -u)"
  launchctl bootout "gui/$uid/$LAUNCHD_LABEL" 2>/dev/null || true
  if ! launchctl bootstrap "gui/$uid" "$plist"; then
    echo "  launchd bootstrap failed; retrying after cleanup..." >&2
    launchctl bootout "gui/$uid/$LAUNCHD_LABEL" 2>/dev/null || true
    pkill -x fieldcraft-server 2>/dev/null || true
    sleep 0.5
    launchctl bootstrap "gui/$uid" "$plist"
  fi
  launchctl kickstart -k "gui/$uid/$LAUNCHD_LABEL"
  echo "  installed local fieldcraft service at $LOCAL_SERVER_URL ($LAUNCHD_LABEL)"
}

wait_for_local_server() {
  for _ in $(seq 1 30); do
    if curl -fsS "$LOCAL_SERVER_URL/health" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.2
  done
  echo "ERROR: local fieldcraft service did not become healthy at $LOCAL_SERVER_URL" >&2
  return 1
}

# ~/.fieldcraft/secrets.env: the one place for API keys. Created once, never
# rewritten by this script (only appended to when the installer's environment
# carries a key the file lacks), read by the binaries themselves, and sourced by
# ~/.fieldcraft/env for shells and hooks. Survives reinstalls and plist rewrites.
ensure_secrets_file() {
  local secrets="$HOME/.fieldcraft/secrets.env"
  mkdir -p "$(dirname "$secrets")"
  if [[ ! -f "$secrets" ]]; then
    (umask 077; cat > "$secrets" <<'SECEOF'
# Fieldcraft secrets. Owner-only. install.sh never rewrites this file.
# The fieldcraft binaries read it when a key is missing from the environment.
# Put the Gemini embedding key here:
#   export GEMINI_API_KEY=...
# Optional: FIELDCRAFT_ENROLL_SECRET=... (shared-server enrollment secret)
SECEOF
    )
    echo "  created $secrets (chmod 600)"
  fi
  chmod 600 "$secrets"
  local key
  for key in GEMINI_API_KEY GOOGLE_API_KEY FIELDCRAFT_ENROLL_SECRET; do
    if [[ -n "${!key:-}" ]] && ! grep -qE "^[[:space:]]*(export[[:space:]]+)?$key=" "$secrets"; then
      printf 'export %s=%s\n' "$key" "$(shell_quote "${!key}")" >> "$secrets"
      echo "  saved $key from the installer environment into $secrets"
    fi
  done
  if grep -qE '^[[:space:]]*(export[[:space:]]+)?(GEMINI_API_KEY|GOOGLE_API_KEY)=' "$secrets"; then
    echo "  embedding key: present in $secrets"
  else
    echo "  WARNING: no GEMINI_API_KEY in $secrets — retrieval will run keyword-only until you add one" >&2
  fi
}

# POST /enroll to the given server, sending the enroll secret when one is set.
enroll_request() {
  local url="$1"
  local -a args=(-sS -X POST "$url/enroll" -H "Content-Type: application/json")
  if [[ -n "$ENROLL_SECRET" ]]; then
    args+=(-H "X-Fieldcraft-Enroll-Secret: $ENROLL_SECRET")
  fi
  curl "${args[@]}" -d "$(enroll_payload)"
}

enroll_local_token() {
  local response token
  response="$(enroll_request "$LOCAL_SERVER_URL")"
  token="$(echo "$response" | jq -r '.token // empty')"
  if [[ -z "$token" ]]; then
    echo "ERROR: local enroll failed; server returned: $response" >&2
    return 1
  fi
  printf '%s\n' "$token"
}

enroll_payload() {
  jq -cn --arg label "$LABEL" '{label: $label}'
}

shell_quote() {
  local value="$1"
  jq -Rr @sh <<< "$value"
}

read_env_file_value() {
  local key="$1"
  [[ -f "$ENV_FILE" ]] || return 0
  python3 - "$ENV_FILE" "$key" <<'PY'
import shlex
import sys

path, key = sys.argv[1:]
with open(path, "r", encoding="utf-8") as handle:
    for raw in handle:
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        try:
            parts = shlex.split(line, comments=False, posix=True)
        except ValueError:
            continue
        if parts and parts[0] == "export":
            parts = parts[1:]
        for part in parts:
            prefix = key + "="
            if part.startswith(prefix):
                print(part[len(prefix):])
                raise SystemExit(0)
PY
}

install_shell_env_snippet() {
  local target="$1"
  mkdir -p "$(dirname "$target")"
  touch "$target"
  if grep -qF "$SHELL_ENV_START" "$target"; then
    return 0
  fi
  cat >> "$target" <<'SHELLEOF'

# fieldcraft-env:start
# Load Fieldcraft client/server settings for interactive shells.
if [ -f "$HOME/.fieldcraft/env" ]; then
  . "$HOME/.fieldcraft/env"
fi
# fieldcraft-env:end
SHELLEOF
  echo "  added Fieldcraft env loader to $target"
}

remove_shell_env_snippet() {
  local target="$1"
  [[ -f "$target" ]] || return 0
  if ! grep -qF "$SHELL_ENV_START" "$target"; then
    return 0
  fi
  local tmp
  tmp="$(mktemp)"
  awk -v start="$SHELL_ENV_START" -v end="$SHELL_ENV_END" '
    BEGIN { in_block = 0 }
    {
      if (index($0, start) > 0) { in_block = 1; next }
      if (in_block && index($0, end) > 0) { in_block = 0; next }
      if (!in_block) print
    }
  ' "$target" > "$tmp"
  mv "$tmp" "$target"
  echo "  removed Fieldcraft env loader from $target"
}

install_shell_env_loaders() {
  install_shell_env_snippet "$HOME/.zshrc"
  install_shell_env_snippet "$HOME/.zprofile"
  install_shell_env_snippet "$HOME/.bashrc"
  install_shell_env_snippet "$HOME/.bash_profile"
}

remove_shell_env_loaders() {
  remove_shell_env_snippet "$HOME/.zshrc"
  remove_shell_env_snippet "$HOME/.zprofile"
  remove_shell_env_snippet "$HOME/.bashrc"
  remove_shell_env_snippet "$HOME/.bash_profile"
}

# --- Argument parsing -----------------------------------------------------
while [[ $# -gt 0 ]]; do
  case "$1" in
    --server-url=*) SERVER_URL="${1#*=}"; shift ;;
    --server-url)   SERVER_URL="$2"; shift 2 ;;
    --auth-token=*) AUTH_TOKEN="${1#*=}"; shift ;;
    --auth-token)   AUTH_TOKEN="$2"; shift 2 ;;
    --label=*)      LABEL="${1#*=}"; shift ;;
    --label)        LABEL="$2"; shift 2 ;;
    --enroll-secret=*) ENROLL_SECRET="${1#*=}"; shift ;;
    --enroll-secret)   ENROLL_SECRET="$2"; shift 2 ;;
    --yes|-y)          YES=1; shift ;;
    --skip-build)      SKIP_BUILD=1; shift ;;
    --distiller=*)     DISTILLER_CHOICE="${1#*=}"; shift ;;
    --distiller)       DISTILLER_CHOICE="$2"; shift 2 ;;
    -h|--help)
      # Print the header comment (portable: BSD sed rejects the {…} range form).
      awk 'NR > 1 && /^set -e/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"
      exit 0
      ;;
    *) echo "Unknown flag: $1" >&2; exit 1 ;;
  esac
done

echo "Installing fieldcraft from $REPO"

# --- 0. Prerequisite check -------------------------------------------------
missing=()
command -v cargo >/dev/null 2>&1 || missing+=("cargo (install via https://rustup.rs)")
command -v jq    >/dev/null 2>&1 || missing+=("jq (brew install jq)")
if [[ -n "$SERVER_URL" ]]; then
  command -v curl >/dev/null 2>&1 || missing+=("curl (preinstalled on macOS)")
fi
if [[ ${#missing[@]} -gt 0 ]]; then
  echo "ERROR: missing prerequisites:" >&2
  for m in "${missing[@]}"; do echo "  - $m" >&2; done
  exit 1
fi

# --- 1. Build + install binaries ------------------------------------------
cd "$REPO"
if [[ $SKIP_BUILD -eq 1 ]]; then
  echo "  skipping build (--skip-build)"
else
echo "  building release binaries..."
cargo install --path crates/fieldcraft-cli    --quiet --force
cargo install --path crates/fieldcraft-server --quiet --force
# fieldcraft-backfill-cues is shipped as a separate bin under fieldcraft-cli;
# `cargo install` only installs the main bin per crate, so build + copy.
cargo build --release --bin fieldcraft-backfill-cues --quiet
install -m 755 "$REPO/target/release/fieldcraft-backfill-cues" \
  "$HOME/.cargo/bin/fieldcraft-backfill-cues"
echo "  installed binaries to ~/.cargo/bin/"

# Ensure the binaries are on PATH. `~/.cargo/bin` is not on PATH by default in
# new macOS shells (rustup adds it only via .cargo/env which isn't always
# sourced). Symlink into the first writable, on-PATH standard bin dir.
LINK_DIR=""
for d in /opt/homebrew/bin /usr/local/bin "$HOME/.local/bin"; do
  if [[ -d "$d" && -w "$d" ]] && echo ":$PATH:" | grep -q ":$d:"; then
    LINK_DIR="$d"
    break
  fi
done
if [[ -n "$LINK_DIR" ]]; then
  for bin in fieldcraft fieldcraft-server fieldcraft-backfill-cues; do
    ln -sf "$HOME/.cargo/bin/$bin" "$LINK_DIR/$bin"
  done
  echo "  symlinked binaries to $LINK_DIR/ (on PATH)"
else
  echo "  NOTE: ~/.cargo/bin is not on PATH and no writable on-PATH bin dir found." >&2
  echo "  Add this to ~/.zshrc or ~/.bashrc:  export PATH=\"\$HOME/.cargo/bin:\$PATH\"" >&2
fi
fi  # SKIP_BUILD

# --- 1b. Interactive onboarding ------------------------------------------
# A person at a terminal with no mode flags gets the wizard; it re-enters this
# script with --yes --skip-build once the choices are made. `fieldcraft update`
# and scripted installs pass --yes and never see it.
if [[ $YES -eq 0 && -z "$SERVER_URL" && -z "$AUTH_TOKEN" && -z "$DISTILLER_CHOICE" && -t 0 && -t 1 ]]; then
  echo ""
  exec "$BIN_PATH" setup --repo "$REPO"
fi

# The FIELDCRAFT_DISTILLER line for ~/.fieldcraft/env: an explicit --distiller
# wins, then whatever the previous env file had, then the historical default.
distiller_line() {
  if [[ -n "$DISTILLER_CHOICE" ]]; then
    case "$DISTILLER_CHOICE" in
      none|codex|grok|claude) printf 'export FIELDCRAFT_DISTILLER=%s\n' "$DISTILLER_CHOICE" ;;
      *) echo "ERROR: --distiller must be none|codex|grok|claude" >&2; exit 1 ;;
    esac
  else
    printf '%s\n' "${PREV_DISTILLER:-export FIELDCRAFT_DISTILLER=codex}"
  fi
}

# --- 2. Local service + credential resolution + env file -------------------
mkdir -p "$(dirname "$ENV_FILE")"
ensure_secrets_file
if [[ -n "$SERVER_URL" ]]; then
  # If the user didn't pass --auth-token but a previous install.sh run already
  # enrolled this client, reuse that token instead of minting a duplicate.
  # Without this guard, each re-run accumulates a fresh row in tokens.sqlite3
  # under whatever label `hostname -s` returned at that moment — which can
  # silently rebrand the same machine across installs.
  if [[ -z "$AUTH_TOKEN" && -f "$ENV_FILE" ]]; then
    AUTH_TOKEN="$(read_env_file_value FIELDCRAFT_AUTH_TOKEN || true)"
    if [[ -n "$AUTH_TOKEN" ]]; then
      echo "  reusing existing token from $ENV_FILE (no re-enrollment)"
    fi
  fi
  if [[ -z "$AUTH_TOKEN" ]]; then
    echo "  enrolling client at $SERVER_URL (label: $LABEL)..."
    response="$(enroll_request "$SERVER_URL")"
    AUTH_TOKEN="$(echo "$response" | jq -r '.token // empty')"
    if [[ -z "$AUTH_TOKEN" ]]; then
      echo "ERROR: enroll failed; server returned: $response" >&2
      exit 1
    fi
    echo "  enrolled token from $SERVER_URL/enroll"
  fi
  # Preserve any user-edited distiller / friction-gate settings on re-install.
  PREV_DISTILLER=""
  PREV_MIN_FRICTION=""
  PREV_DISTILL_FORCE=""
  if [[ -f "$ENV_FILE" ]]; then
    PREV_DISTILLER="$(grep -E '^[[:space:]]*(export[[:space:]]+)?FIELDCRAFT_DISTILLER=' "$ENV_FILE" | tail -1 || true)"
    PREV_MIN_FRICTION="$(grep -E '^[[:space:]]*(export[[:space:]]+)?FIELDCRAFT_DISTILL_MIN_FRICTION=' "$ENV_FILE" | tail -1 || true)"
    PREV_DISTILL_FORCE="$(grep -E '^[[:space:]]*(export[[:space:]]+)?FIELDCRAFT_DISTILL_FORCE=' "$ENV_FILE" | tail -1 || true)"
  fi
  install_launch_agent "$SERVER_URL"
  wait_for_local_server
  cat > "$ENV_FILE" <<ENVEOF
# Auto-managed by install.sh. Edit FIELDCRAFT_DISTILLER below if you want.
# 'export' is required so 'source ~/.fieldcraft/env' propagates to subprocesses.
export FIELDCRAFT_SERVER_URL=$SERVER_URL
export FIELDCRAFT_UPSTREAM_URL=$SERVER_URL
export FIELDCRAFT_AUTH_TOKEN=$(shell_quote "$AUTH_TOKEN")
# Background session-end distiller. Options: codex | grok | claude | none | (unset = auto).
# Default: codex. Change this line if you want grok/claude or want to disable distillation.
$(distiller_line)
# Skip LLM distill unless the session shows enough friction (default 3). Set 0 to disable the gate.
# Force with FIELDCRAFT_DISTILL_FORCE=1 (debug). Decisions land in ~/.fieldcraft/state/distill.log.
${PREV_MIN_FRICTION:-# export FIELDCRAFT_DISTILL_MIN_FRICTION=3}
${PREV_DISTILL_FORCE:-# export FIELDCRAFT_DISTILL_FORCE=0}
# API keys live in secrets.env (never rewritten by install.sh); load them for shells and hooks.
[ -f "\$HOME/.fieldcraft/secrets.env" ] && . "\$HOME/.fieldcraft/secrets.env"
ENVEOF
  chmod 600 "$ENV_FILE"
  echo "  wrote local client config to $ENV_FILE (chmod 600)"
  install_shell_env_loaders
else
  install_launch_agent ""
  wait_for_local_server
  AUTH_TOKEN="$(enroll_local_token)"
  PREV_DISTILLER=""
  PREV_MIN_FRICTION=""
  PREV_DISTILL_FORCE=""
  if [[ -f "$ENV_FILE" ]]; then
    PREV_DISTILLER="$(grep -E '^[[:space:]]*(export[[:space:]]+)?FIELDCRAFT_DISTILLER=' "$ENV_FILE" | tail -1 || true)"
    PREV_MIN_FRICTION="$(grep -E '^[[:space:]]*(export[[:space:]]+)?FIELDCRAFT_DISTILL_MIN_FRICTION=' "$ENV_FILE" | tail -1 || true)"
    PREV_DISTILL_FORCE="$(grep -E '^[[:space:]]*(export[[:space:]]+)?FIELDCRAFT_DISTILL_FORCE=' "$ENV_FILE" | tail -1 || true)"
  fi
  cat > "$ENV_FILE" <<ENVEOF
# Auto-managed by install.sh. Edit FIELDCRAFT_DISTILLER below if you want.
# 'export' is required so 'source ~/.fieldcraft/env' propagates to subprocesses.
export FIELDCRAFT_SERVER_URL=$LOCAL_SERVER_URL
export FIELDCRAFT_AUTH_TOKEN=$(shell_quote "$AUTH_TOKEN")
# Background session-end distiller. Options: codex | grok | claude | none | (unset = auto).
# Default: codex. Change this line if you want grok/claude or want to disable distillation.
$(distiller_line)
# Skip LLM distill unless the session shows enough friction (default 3). Set 0 to disable the gate.
# Force with FIELDCRAFT_DISTILL_FORCE=1 (debug). Decisions land in ~/.fieldcraft/state/distill.log.
${PREV_MIN_FRICTION:-# export FIELDCRAFT_DISTILL_MIN_FRICTION=3}
${PREV_DISTILL_FORCE:-# export FIELDCRAFT_DISTILL_FORCE=0}
# API keys live in secrets.env (never rewritten by install.sh); load them for shells and hooks.
[ -f "\$HOME/.fieldcraft/secrets.env" ] && . "\$HOME/.fieldcraft/secrets.env"
ENVEOF
  chmod 600 "$ENV_FILE"
  echo "  wrote local client config to $ENV_FILE (chmod 600)"
  install_shell_env_loaders
fi

# --- 3. Hook scripts to ~/.fieldcraft/hooks/ ------------------------------
mkdir -p "$HOOKS_DIR"
install -m 755 "$REPO/install/bin-hooks/session-start-query.sh" "$HOOKS_DIR/"
install -m 755 "$REPO/install/bin-hooks/stop-feedback.sh"        "$HOOKS_DIR/"
echo "  installed hook scripts to $HOOKS_DIR"

# --- 4. Merge hook fragments ----------------------------------------------
chmod +x "$REPO"/install/bin/*.sh
export FIELDCRAFT_HOOKS="$HOOKS_DIR"
for fragment in "$REPO"/install/hooks/*.json; do
  [[ -f "$fragment" ]] || continue
  bash "$REPO/install/bin/merge-hook.sh"  "$HOME/.claude/settings.json"  "$fragment"
  bash "$REPO/install/bin/merge-hook.sh"  "$HOME/.codex/hooks.json"      "$fragment"
  # Grok: native hooks under ~/.grok/hooks/ (always trusted global scope).
  if [[ -d "$HOME/.grok" ]] || command -v grok >/dev/null 2>&1; then
    mkdir -p "$HOME/.grok/hooks"
    bash "$REPO/install/bin/merge-hook.sh"  "$HOME/.grok/hooks/fieldcraft.json"  "$fragment"
  fi
done

# --- 5. Remove any legacy fieldcraft MCP entries --------------------------
# Earlier installs registered fieldcraft as an MCP server in claude_desktop_config.json
# and ~/.codex/config.toml. The CLI-only design replaces that path. Strip them.
if command -v claude >/dev/null 2>&1; then
  claude mcp remove fieldcraft 2>/dev/null || true
fi
for f in "$HOME/Library/Application Support/Claude/claude_desktop_config.json" \
         "$HOME/.codex/config.toml"; do
  [[ -f "$f" ]] || continue
  if grep -q '"fieldcraft"' "$f" 2>/dev/null || \
     grep -q '^\[mcp_servers\.fieldcraft\]' "$f" 2>/dev/null; then
    echo "  legacy fieldcraft MCP entry detected in $f — please remove manually"
  fi
done

# --- 6. Inject instruction blocks -----------------------------------------
bash "$REPO/install/bin/inject-markered.sh" \
  "$REPO/install/instructions/claude.md" \
  "$HOME/.claude/CLAUDE.md"
bash "$REPO/install/bin/inject-markered.sh" \
  "$REPO/install/instructions/codex.md" \
  "$HOME/.codex/AGENTS.md"
# Grok home rules (always scanned; SessionStart stdout is ignored on Grok).
if [[ -d "$HOME/.grok" ]] || command -v grok >/dev/null 2>&1; then
  mkdir -p "$HOME/.grok/rules"
  bash "$REPO/install/bin/inject-markered.sh" \
    "$REPO/install/instructions/grok.md" \
    "$HOME/.grok/rules/fieldcraft.md"
fi

echo ""
echo "Done."
if [[ -n "$SERVER_URL" ]]; then
  echo "  Mode: REMOTE SERVER (agents -> $SERVER_URL)"
else
  echo "  Mode: LOCAL SERVICE (agents -> $LOCAL_SERVER_URL -> ~/.fieldcraft/memory.sqlite3)"
fi
echo "  - Edit hook scripts at $HOOKS_DIR"
echo "  - View status: fieldcraft stats"
echo "  - Uninstall: bash uninstall.sh"
