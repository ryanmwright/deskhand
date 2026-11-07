#!/usr/bin/env bash
# A fake coding agent in a scratch herdr session, to try the herdr
# integration without touching real sessions. herdr takes the reported
# state as if an agent hook sent it.
#
#   dev/herdr/fake-agent.sh start                 # server for the scratch session
#   dev/herdr/fake-agent.sh report w1:p1 working  # idle|working|blocked|unknown [agent]
#   dev/herdr/fake-agent.sh release w1:p1         # the agent quits
#   dev/herdr/fake-agent.sh split w1:p1           # a new pane (prints its id)
#   dev/herdr/fake-agent.sh snapshot              # agents as herdr sees them
#   dev/herdr/fake-agent.sh stop                  # stop and delete the session
#
# Point Deskhand at it with `[integrations.herdr] session = "deskhand-dev"`
# (or HERDR_DEV_SESSION).
set -euo pipefail
session=${HERDR_DEV_SESSION:-deskhand-dev}
h() { herdr --session "$session" "$@"; }
socket="${XDG_CONFIG_HOME:-$HOME/.config}/herdr/sessions/$session/herdr.sock"

case ${1:-} in
start)
    if [ -S "$socket" ] && h api snapshot >/dev/null 2>&1; then
        echo "herdr session $session is running"
        exit 0
    fi
    setsid herdr --session "$session" server >/dev/null 2>&1 < /dev/null &
    for _ in $(seq 50); do
        [ -S "$socket" ] && h api snapshot >/dev/null 2>&1 && break
        sleep 0.1
    done
    echo "herdr session $session: $socket"
    ;;
report)
    # The pane id goes first: herdr 0.9.1 misparses it after the options.
    h pane report-agent "$2" --source deskhand-dev --agent "${4:-claude}" --state "$3"
    ;;
release)
    h pane release-agent "$2" --source deskhand-dev --agent "${3:-claude}"
    ;;
split)
    h pane split "${2:-w1:p1}" --direction right --no-focus | jq -r ".result.pane.pane_id"
    ;;
snapshot)
    h api snapshot | jq -c '.result.snapshot.agents[] | {pane_id, agent, agent_status}'
    ;;
stop)
    h server stop >/dev/null 2>&1 || true
    herdr session delete "$session" >/dev/null 2>&1 || true
    echo "herdr session $session stopped and deleted"
    ;;
*)
    sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'
    exit 1
    ;;
esac
