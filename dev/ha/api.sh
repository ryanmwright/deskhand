#!/usr/bin/env bash
# Calls the dev HA REST API: api.sh METHOD PATH [JSON_BODY]
# e.g. api.sh POST /api/services/media_player/media_pause '{"entity_id":"media_player.x"}'
# Uses the refresh token saved by bootstrap.sh.
set -euo pipefail
HA="${HA_URL:-http://localhost:8123}"
TOKEN_FILE="$(dirname "$0")/config/.deskhand-dev-refresh-token"
ACCESS=$(curl -fsS -X POST "$HA/auth/token" -d grant_type=refresh_token \
  -d "refresh_token=$(cat "$TOKEN_FILE")" --data-urlencode "client_id=$HA/" | jq -r .access_token)
args=(-fsS -X "$1" "$HA$2" -H "Authorization: Bearer $ACCESS" -H "Content-Type: application/json")
if [ $# -ge 3 ]; then args+=(-d "$3"); fi
curl "${args[@]}"
echo
