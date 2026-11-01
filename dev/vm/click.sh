#!/usr/bin/env bash
# Clicks at screen pixel X Y [left|right|middle] in a headless VM (`just vm-headless`), through
# QMP absolute events on the USB tablet. HMP `mouse_move` only sends relative
# motion, which KWin does not use here. Assumes a 1280x800 screen (override
# with SCREEN_W/SCREEN_H).
set -euo pipefail
X=$1 Y=$2 BUTTON=${3:-left}
W=${SCREEN_W:-1280} H=${SCREEN_H:-800}
SOCK=${QMP_SOCK:-/tmp/plasma-vm.qmp}
ax=$((X * 32767 / (W - 1))) ay=$((Y * 32767 / (H - 1)))
btn() {
  echo '{"execute":"input-send-event","arguments":{"events":[{"type":"btn","data":{"down":'"$1"',"button":"'"$BUTTON"'"}}]}}'
}
{
  echo '{"execute":"qmp_capabilities"}'
  echo '{"execute":"input-send-event","arguments":{"events":[{"type":"abs","data":{"axis":"x","value":'"$ax"'}},{"type":"abs","data":{"axis":"y","value":'"$ay"'}}]}}'
  sleep 0.2
  btn true
  sleep 0.1
  btn false
  sleep 0.2
} | socat - "UNIX-CONNECT:$SOCK" | grep -q '"error"' && { echo "QMP error" >&2; exit 1; } || true
