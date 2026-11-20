#!/bin/sh
# Launches the manager on a fixture with the Makepad remote, for screenshots:
#   tools/shoot.sh [fixture.json]   (prints the app's pid; logs to $SHOTS/app.log)
# Drive it with makepad-remote (docs/agents/app-remote.md in the Makepad checkout).
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
fixture=${1:-$here/fixtures/view.json}
SHOTS=${SHOTS:-/var/tmp/agent-manager-shots}
mkdir -p "$SHOTS"
bin=${CARGO_TARGET_DIR:-/var/tmp/agent-manager-target}/debug/agent-manager
MAKEPAD_HIDE_WINDOWS=1 AGENT_MANAGER_FIXTURE=$fixture "$bin" --remote --window 1100x700@1 >"$SHOTS/app.log" 2>&1 &
i=0
until grep -q '^\[makepad-remote\] pid=' "$SHOTS/app.log" 2>/dev/null; do
    i=$((i + 1))
    [ $i -gt 60 ] && { echo "no remote line in $SHOTS/app.log" >&2; exit 1; }
    sleep 0.25
done
sed -n 's/^\[makepad-remote\] pid=\([0-9]*\).*/\1/p' "$SHOTS/app.log" | head -1
