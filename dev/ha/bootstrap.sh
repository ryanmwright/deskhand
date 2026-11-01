#!/usr/bin/env bash
# Completes Home Assistant onboarding and adds the MQTT integration through the REST API,
# so a fresh dev instance is usable without clicking through the UI. Safe to re-run.
set -euo pipefail

HA="${HA_URL:-http://localhost:8123}"
CLIENT_ID="$HA/"
USER_NAME="${HA_USER:-dev}"
USER_PASS="${HA_PASS:-devpassword}"
BROKER_HOST="${BROKER_HOST:-mosquitto}"
TOKEN_FILE="$(dirname "$0")/config/.deskhand-dev-refresh-token"

log() { printf '\033[1;34m[bootstrap]\033[0m %s\n' "$*"; }

# The API answers 401 without a token once HA is up.
api_up() { [ "$(curl -s -o /dev/null -w '%{http_code}' "$HA/api/" || true)" = 401 ]; }

# HA serves /api/onboarding only until onboarding is finished (404 afterwards).
step_done() {
  local body
  body=$(curl -fsS "$HA/api/onboarding" 2>/dev/null) || return 0
  [ "$(echo "$body" | jq -r --arg s "$1" '.[] | select(.step==$s) | .done')" = "true" ]
}

log "waiting for Home Assistant at $HA"
for _ in $(seq 1 180); do
  if api_up; then break; fi
  sleep 2
done
api_up || { log "HA did not come up"; exit 1; }

if ! step_done user; then
  log "creating owner user '$USER_NAME'"
  auth_code=$(curl -fsS -X POST "$HA/api/onboarding/users" \
    -H 'Content-Type: application/json' \
    -d "$(jq -n --arg c "$CLIENT_ID" --arg u "$USER_NAME" --arg p "$USER_PASS" \
      '{client_id:$c, name:"Dev", username:$u, password:$p, language:"en"}')" | jq -r .auth_code)
  tokens=$(curl -fsS -X POST "$HA/auth/token" \
    -d grant_type=authorization_code -d "code=$auth_code" --data-urlencode "client_id=$CLIENT_ID")
  echo "$tokens" | jq -r .refresh_token >"$TOKEN_FILE"
  chmod 600 "$TOKEN_FILE"
fi

[ -s "$TOKEN_FILE" ] || { log "no refresh token at $TOKEN_FILE; delete dev/ha/config and re-run"; exit 1; }
ACCESS=$(curl -fsS -X POST "$HA/auth/token" -d grant_type=refresh_token \
  -d "refresh_token=$(cat "$TOKEN_FILE")" --data-urlencode "client_id=$CLIENT_ID" | jq -r .access_token)
auth=(-H "Authorization: Bearer $ACCESS" -H 'Content-Type: application/json')

for step in core_config analytics; do
  if ! step_done "$step"; then
    log "onboarding step: $step"
    curl -fsS -X POST "$HA/api/onboarding/$step" "${auth[@]}" -d '{}' >/dev/null
  fi
done
if ! step_done integration; then
  log "onboarding step: integration"
  curl -fsS -X POST "$HA/api/onboarding/integration" "${auth[@]}" \
    -d "$(jq -n --arg c "$CLIENT_ID" '{client_id:$c, redirect_uri:($c + "?auth_callback=1")}')" >/dev/null
fi

if curl -fsS "$HA/api/config/config_entries/entry?domain=mqtt" "${auth[@]}" | jq -e 'length > 0' >/dev/null; then
  log "MQTT integration already configured"
else
  log "adding MQTT integration (broker $BROKER_HOST:1883)"
  flow=$(curl -fsS -X POST "$HA/api/config/config_entries/flow" "${auth[@]}" -d '{"handler":"mqtt"}')
  flow_id=$(echo "$flow" | jq -r .flow_id)
  result=$(curl -sS -X POST "$HA/api/config/config_entries/flow/$flow_id" "${auth[@]}" \
    -d "$(jq -n --arg h "$BROKER_HOST" '{broker:$h, port:1883, protocol:"5", username:"homeassistant", password:"homeassistant-dev",
        other_settings:{set_client_cert:false, set_ca_cert:"off", transport:"tcp"}}')")
  if [ "$(echo "$result" | jq -r .type)" != "create_entry" ]; then
    log "unexpected MQTT flow result:"; echo "$result" | jq .; exit 1
  fi
fi

log "done: open $HA (user '$USER_NAME', password '$USER_PASS')"
