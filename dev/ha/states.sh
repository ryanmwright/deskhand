#!/usr/bin/env bash
# Prints HA entity states matching a regex (default: all deskhand entities).
# Uses the refresh token saved by bootstrap.sh.
set -euo pipefail
HA="${HA_URL:-http://localhost:8123}"
TOKEN_FILE="$(dirname "$0")/config/.deskhand-dev-refresh-token"
ACCESS=$(curl -fsS -X POST "$HA/auth/token" -d grant_type=refresh_token \
  -d "refresh_token=$(cat "$TOKEN_FILE")" --data-urlencode "client_id=$HA/" | jq -r .access_token)
curl -fsS "$HA/api/states" -H "Authorization: Bearer $ACCESS" |
  jq -r --arg re "${1:-deskhand}" '.[] | select(.entity_id | test($re)) | "\(.entity_id)\t\(.state)"' | sort
