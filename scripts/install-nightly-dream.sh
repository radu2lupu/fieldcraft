#!/usr/bin/env bash
# Install the nightly dream-tier consolidation job (launchd, runs on the
# authoritative server). Fires at 00:10 LOCAL time — the Mac Mini runs
# Europe/Berlin (CET/CEST), i.e. the store's least-used window, per plan.
# Logs to ~/.fieldcraft/reports/nightly-dream.log. No Python.
set -euo pipefail

label="com.fieldcraft.dream"
plist="$HOME/Library/LaunchAgents/$label.plist"
bin="$HOME/.cargo/bin/fieldcraft"
reports="$HOME/.fieldcraft/reports"
mkdir -p "$reports" "$HOME/Library/LaunchAgents"

cat > "$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key>
  <array>
    <string>$bin</string>
    <string>dream</string>
    <string>--apply</string>
    <string>--max-clusters</string>
    <string>3</string>
    <string>--data-dir</string>
    <string>$HOME/.fieldcraft</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin</string>
    <key>HOME</key><string>$HOME</string>
  </dict>
  <key>StartCalendarInterval</key>
  <dict><key>Hour</key><integer>0</integer><key>Minute</key><integer>10</integer></dict>
  <key>StandardOutPath</key><string>$reports/nightly-dream.log</string>
  <key>StandardErrorPath</key><string>$reports/nightly-dream.err.log</string>
  <key>ProcessType</key><string>Background</string>
</dict>
</plist>
EOF

uid="$(id -u)"
launchctl bootout "gui/$uid/$label" 2>/dev/null || true
launchctl bootstrap "gui/$uid" "$plist"
echo "installed $label (nightly 00:10 local, logs in $reports/nightly-dream.log)"
