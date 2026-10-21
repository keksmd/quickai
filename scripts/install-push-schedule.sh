#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: QUICKAI_PUSH_TOKEN=... $0 --url URL [--host NAME] [--at HH:MM]" >&2
  echo "Installs a daily 'quickai push --index' (launchd on macOS, cron on Linux)." >&2
  exit 2
}

URL="" TOKEN="${QUICKAI_PUSH_TOKEN:-}" HOST_NAME="" AT="09:00"
while [ $# -gt 0 ]; do
  case "$1" in
    --url) URL="$2"; shift 2 ;;
    --token) TOKEN="$2"; shift 2 ;;
    --host) HOST_NAME="$2"; shift 2 ;;
    --at) AT="$2"; shift 2 ;;
    *) usage ;;
  esac
done
[ -n "$URL" ] && [ -n "$TOKEN" ] || usage
[[ "$AT" =~ ^([01]?[0-9]|2[0-3]):([0-5][0-9])$ ]] || { echo "--at must be HH:MM" >&2; exit 2; }
HH="${BASH_REMATCH[1]}" MM="${BASH_REMATCH[2]}"

BIN="${QUICKAI_BIN:-}"
if [ -z "$BIN" ]; then
  for c in "$HOME/.cargo/bin/quickai" "$(command -v quickai || true)"; do
    if [ -n "$c" ] && [ -x "$c" ] && "$c" push --help >/dev/null 2>&1; then BIN="$c"; break; fi
  done
fi
[ -n "$BIN" ] || { echo "quickai with 'push' not found: cargo install --locked --force --git https://github.com/keksmd/quickai --branch feat/multi-agent-sources quickai" >&2; exit 1; }

DIR="$HOME/.quickai"
mkdir -p "$DIR"
umask 077
{
  printf 'export QUICKAI_PUSH_URL=%q\n' "$URL"
  printf 'export QUICKAI_PUSH_TOKEN=%q\n' "$TOKEN"
  if [ -n "$HOST_NAME" ]; then printf 'export QUICKAI_HOST=%q\n' "$HOST_NAME"; fi
} > "$DIR/push.env"
chmod 600 "$DIR/push.env"

RUN=". '$DIR/push.env' && '$BIN' push --index >> '$DIR/push.log' 2>&1"

if [ "$(uname)" = "Darwin" ]; then
  PLIST="$HOME/Library/LaunchAgents/dev.quickai.push.plist"
  XRUN="${RUN//&/&amp;}"; XRUN="${XRUN//</&lt;}"; XRUN="${XRUN//>/&gt;}"
  mkdir -p "$(dirname "$PLIST")"
  cat > "$PLIST" <<PL
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>dev.quickai.push</string>
  <key>ProgramArguments</key><array><string>/bin/sh</string><string>-c</string><string>$XRUN</string></array>
  <key>StartCalendarInterval</key><dict><key>Hour</key><integer>$((10#$HH))</integer><key>Minute</key><integer>$((10#$MM))</integer></dict>
  <key>RunAtLoad</key><false/>
</dict></plist>
PL
  launchctl bootout "gui/$(id -u)/dev.quickai.push" 2>/dev/null || true
  launchctl bootstrap "gui/$(id -u)" "$PLIST"
  echo "launchd: dev.quickai.push daily at $AT -> $PLIST"
else
  LINE="$((10#$MM)) $((10#$HH)) * * * $RUN"
  ( crontab -l 2>/dev/null | grep -v 'quickai push' || true; echo "$LINE" ) | crontab -
  echo "cron: daily at $AT"
fi

echo "binary: $BIN"
echo "test now: . $DIR/push.env && $BIN push --index"
