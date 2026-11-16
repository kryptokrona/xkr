#!/usr/bin/env bash
# 90-cleanup: stops everything the suite started under <wd> and checks that nothing is left.
# Idempotent; also the way to clean up after `--keep` or a scenario run on its own.
#
# Checks: no process from <wd>/pids alive; no container labelled xkr-suite; every suite port
# free. Logs of the removed containers go to <wd>/<container>.log; results and logs stay.
# suite: expected=10 timeout=180
set -euo pipefail
. "$(dirname "$0")/../lib.sh"
suite_init "${1:-}"

before=$(ls "$WD/pids" 2>/dev/null | wc -l | tr -d ' ')
containers=$(docker ps -aq --filter "label=$SUITE_LABEL" 2>/dev/null | wc -l | tr -d ' ' || true)
suite_cleanup
pass 1 "stopped $before tracked process(es), removed ${containers:-0} suite container(s)"

left=""
for f in "$WD"/pids/*.pid; do
    [ -e "$f" ] && left="$left $(basename "$f" .pid)"
done
if [ -z "$left" ]; then
    pass 2 "no tracked process left"
else
    fail 2 "pid files left:$left"
fi
if command -v docker >/dev/null 2>&1; then
    rest=$(docker ps -aq --filter "label=$SUITE_LABEL" 2>/dev/null | tr '\n' ' ' || true)
    if [ -z "${rest// /}" ]; then
        pass 2 "no container labelled $SUITE_LABEL left"
    else
        fail 2 "containers left: $rest"
    fi
fi
busy_ports() {
    BUSY=""
    local p
    for p in $SUITE_PORTS; do
        if port_busy "$p"; then BUSY="$BUSY $p"; fi
    done
    [ -z "$BUSY" ]
}
BUSY=""
if wait_for 15 busy_ports; then
    pass 3 "all suite ports free"
else
    fail 3 "ports still in use:$BUSY"
fi
finish
