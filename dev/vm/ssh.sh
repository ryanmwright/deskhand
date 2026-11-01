#!/usr/bin/env bash
# ssh into the Plasma VM (`just vm`) as user `dev`. Extra arguments run as a command:
#   dev/vm/ssh.sh 'systemctl --user restart deskhand-dev'
# The VM has no persistent host key worth pinning, so host key checks are off (loopback only).
set -euo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
exec ssh -p "${VM_SSH_PORT:-2222}" -i "$dir/ssh/id_ed25519" \
  -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
  -o ConnectTimeout=5 dev@127.0.0.1 "$@"
