#!/bin/sh
# A stand-in for `agent` when testing the manager's buttons: it records each call's
# arguments (one call per line, arguments separated by " | ") in $FAKE_AGENT_LOG and
# prints something plausible. It changes nothing.
#   AGENT_MANAGER_FIXTURE=fixtures/view.json AGENT_MANAGER_AGENT=tools/fake-agent.sh agent-manager
log=${FAKE_AGENT_LOG:-/var/tmp/agent-manager-shots/fake-agent.log}
line=""
for a in "$@"; do
    if [ -z "$line" ]; then line=$a; else line="$line | $a"; fi
done
printf '%s\n' "$line" >>"$log"
case "$1 $2" in
"manage prune")
    case " $* " in
    *" --dry-run "*)
        echo "Would delete, older than $4:"
        echo "  command_log     81,204 rows"
        echo "  agent_events    40,118 rows"
        echo "  runs               912 rows of departed agents"
        echo "  messages         3,511 delivered"
        echo "  jobs               187 finished"
        echo "The database is backed up first."
        ;;
    *) echo "Backed up to .gomuxai/backups/gomuxai-fake.db; deleted 125,932 rows." ;;
    esac
    ;;
"manage backup") echo "Backed up to .gomuxai/backups/gomuxai-fake.db (60.9 MB)" ;;
"manage untrack") shift 2; echo "Untracked $# sessions." ;;
"manage track") shift 2; echo "Tracking $# sessions again." ;;
"friction resolve") echo "Resolved #$3." ;;
"logs "*)
    echo "17:02:11  > run the tests and report back"
    echo "17:02:15  Bash: cargo test -p agent-store"
    echo "17:03:40  test result: ok. 212 passed"
    echo "17:03:41  < all 212 store tests pass"
    ;;
"cleanup "*) echo "$2: cleaned up" ;;
"resume "*) echo "$2: resumed (idle)" ;;
"revive "*) echo "$2: revived; re-brief sent" ;;
*) echo "fake agent: $*" >&2; exit 1 ;;
esac
