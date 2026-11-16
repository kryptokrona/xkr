#!/usr/bin/env bash
# testnet/suite.sh: the repeatable testnet regression standard for xkrd (docs/testnet-suite.md).
#
#   testnet/suite.sh <work dir> [options] [scenario...]
#   testnet/suite.sh --list
#
# Runs the scenarios in testnet/scenarios/NN-name.sh in order (all by default), each as its own
# process under `nice -n 19` with a wall-clock limit, and writes
#   <work dir>/results.tsv    one row per scenario (status, exit code, seconds, check counts, log)
#   <work dir>/results.json   the same plus every check
#   <work dir>/checks.tsv     every PASS/FAIL/SKIP/MISSING line of this run
#   <work dir>/logs/NN-name.log   the scenario's output; <work dir>/NN-name/ its state and logs
#
# Scenario selection: a name (13-transactions), a number (13), a number range (10-14), "all",
# or a name prefix (3 matches 30-fork13 and 31-fork14). Default: all.
# Options:
#   --list        list the scenarios with expected durations and exit
#   --keep        leave the nodes running afterwards (KEEP=1, no final cleanup)
#   --fail-fast   stop after the first scenario that does not pass
#   --reuse       do not clean up an earlier run in <work dir> first (reuse the seed chain)
# Exit status: 0 when every selected scenario passed (SKIP checks allowed), 1 otherwise, 2 for
# usage errors. Never builds anything.
# Optional scenarios ("optional=1" in their "# suite:" header, e.g. 32-p2pool) whose
# prerequisite is missing (exit 2) count as SKIP, not as a failure.
set -euo pipefail
SUITE_DIR=$(cd "$(dirname "$0")" && pwd)
SCEN_DIR=$SUITE_DIR/scenarios
. "$SUITE_DIR/lib.sh"

usage() {
    sed -n '3,24p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

# meta <script> <key>: from the "# suite: expected=N timeout=N" header line
meta() {
    sed -n 's/^# suite:.*'"$2"'=\([0-9]*\).*/\1/p' "$1" | head -n 1
}
# summary <script>: the first header line after the shebang
summary() { sed -n '2s/^# //p' "$1"; }

all_scenarios() {
    local f
    for f in "$SCEN_DIR"/[0-9][0-9]-*.sh; do
        [ -e "$f" ] && basename "$f" .sh
    done
}

list() {
    local s total=0 e
    printf '%-24s %9s %8s  %s\n' scenario expected timeout summary
    while read -r s; do
        e=$(meta "$SCEN_DIR/$s.sh" expected)
        total=$((total + ${e:-0}))
        opt=""
        [ "$(meta "$SCEN_DIR/$s.sh" optional)" = 1 ] && opt="[optional] "
        printf '%-24s %8ss %7ss  %s\n' "$s" "${e:-?}" "$(meta "$SCEN_DIR/$s.sh" timeout)" \
            "$opt$(summary "$SCEN_DIR/$s.sh" | cut -d: -f2- | sed 's/^ //' | cut -c1-80)"
    done < <(all_scenarios)
    echo "total expected: about $((total / 60)) min"
}

# ------------------------------------------------------------------ arguments

KEEP_ALL=0
FAIL_FAST=0
REUSE=0
SEL=()
WD_ARG=""
for a in "$@"; do
    case $a in
        --list) list; exit 0 ;;
        --keep) KEEP_ALL=1 ;;
        --fail-fast) FAIL_FAST=1 ;;
        --reuse) REUSE=1 ;;
        -h | --help) usage ;;
        -*) echo "unknown option $a" >&2; usage ;;
        *)
            if [ -z "$WD_ARG" ]; then WD_ARG=$a; else SEL+=("$a"); fi
            ;;
    esac
done
[ -n "$WD_ARG" ] || usage

mapfile -t ALL < <(all_scenarios)
selected=()
pick() { # adds a scenario once, keeping the suite order
    local x
    for x in ${selected[@]+"${selected[@]}"}; do
        [ "$x" = "$1" ] && return 0
    done
    selected+=("$1")
}
if [ "${#SEL[@]}" = 0 ]; then
    selected=("${ALL[@]}")
else
    want=()
    for a in "${SEL[@]}"; do
        hit=0
        for s in "${ALL[@]}"; do
            n=$((10#${s%%-*}))
            if [[ $a =~ ^([0-9]+)-([0-9]+)$ ]]; then
                lo=$((10#${BASH_REMATCH[1]})); hi=$((10#${BASH_REMATCH[2]}))
                if [ "$n" -ge "$lo" ] && [ "$n" -le "$hi" ]; then want+=("$s"); hit=1; fi
            elif [[ $a =~ ^[0-9]+$ ]] && [ "${#a}" -ge 2 ]; then
                if [ "$n" = "$((10#$a))" ]; then want+=("$s"); hit=1; fi
            elif [ "$a" = all ] || [ "$s" = "$a" ] || [[ $s == "$a"* ]]; then
                want+=("$s"); hit=1
            fi
        done
        [ "$hit" = 1 ] || { echo "no scenario matches \"$a\" (testnet/suite.sh --list)" >&2; exit 2; }
    done
    for s in "${ALL[@]}"; do
        for w in "${want[@]}"; do
            [ "$s" = "$w" ] && pick "$s"
        done
    done
fi

mkdir -p "$WD_ARG"
WD=$(cd "$WD_ARG" && pwd)
mkdir -p "$WD/logs" "$WD/pids"
SCEN=suite
CHECKS_TSV=$WD/checks.tsv
: > "$CHECKS_TSV"
RESULTS_TSV=$WD/results.tsv
RESULTS_JSON=$WD/results.json
printf 'scenario\tstatus\texit\tseconds\texpected\tpassed\tfailed\tskipped\tlog\n' > "$RESULTS_TSV"
STARTED=$(date -u +%Y-%m-%dT%H:%M:%SZ)
export KEEP=$KEEP_ALL

echo "xkr testnet suite: ${#selected[@]} scenario(s) [${selected[*]}], work dir $WD"
echo "xkrd: $([ -x "$XKRD" ] && xkrd_version || echo "missing ($XKRD)")"
if [ "$REUSE" = 0 ]; then
    echo "cleaning up an earlier run in $WD (--reuse skips this)"
    suite_cleanup
fi

# ------------------------------------------------------------------ run

declare -A ST=() RC=() SECS=() NPASS=() NFAIL=() NSKIP=()
OVERALL=0
ran_cleanup=0
# Ctrl-C / SIGTERM reach the running scenario too (same process group: `timeout --foreground`),
# which cleans up after itself; the suite then stops, cleans up and still writes the results.
INTERRUPTED=0
trap 'INTERRUPTED=1' INT TERM
for s in "${selected[@]}"; do
    script=$SCEN_DIR/$s.sh
    limit=$(meta "$script" timeout)
    limit=${limit:-1800}
    log=$WD/logs/$s.log
    echo
    echo "##### $s (expected ~$(meta "$script" expected)s, limit ${limit}s)"
    t0=$(date +%s)
    set +e
    nice -n 19 timeout --foreground --signal=TERM --kill-after=60 "$limit" bash "$script" "$WD" 2>&1 \
        | (trap '' INT TERM; exec tee "$log")
    rc=${PIPESTATUS[0]}
    set -e
    secs=$(($(date +%s) - t0))
    np=$(awk -F'\t' -v s="$s" '$1 == s && $3 == "PASS"' "$CHECKS_TSV" | wc -l | tr -d ' ')
    nf=$(awk -F'\t' -v s="$s" '$1 == s && $3 == "FAIL"' "$CHECKS_TSV" | wc -l | tr -d ' ')
    nk=$(awk -F'\t' -v s="$s" '$1 == s && $3 == "SKIP"' "$CHECKS_TSV" | wc -l | tr -d ' ')
    nm=$(awk -F'\t' -v s="$s" '$1 == s && $3 == "MISSING"' "$CHECKS_TSV" | wc -l | tr -d ' ')
    if [ "$rc" = 124 ] || [ "$rc" = 137 ]; then
        status=TIMEOUT
        printf '%s\t%s\t%s\t%s\t%s\n' "$s" timeout FAIL "$(date +%s)" "killed after the ${limit}s limit" >> "$CHECKS_TSV"
        nf=$((nf + 1))
    elif [ "$rc" = 2 ] || { [ "$nm" -gt 0 ] && [ "$nf" = 0 ]; }; then
        status=MISSING
        if [ "$(meta "$script" optional)" = 1 ] && [ "$nf" = 0 ]; then
            # an optional scenario without its prerequisite (e.g. 32-p2pool without its image):
            # SKIP instead of MISSING, so the standard suite stays green without it
            status=SKIP
            if [ "$nm" -gt 0 ]; then
                awk -F'\t' -v OFS='\t' -v s="$s" \
                    '$1 == s && $3 == "MISSING" {$3 = "SKIP"; $5 = "optional scenario skipped: " $5} {print}' \
                    "$CHECKS_TSV" > "$CHECKS_TSV.tmp" && mv "$CHECKS_TSV.tmp" "$CHECKS_TSV"
            else
                printf '%s\t%s\t%s\t%s\t%s\n' "$s" optional SKIP "$(date +%s)" \
                    "optional scenario skipped: prerequisite missing (exit $rc)" >> "$CHECKS_TSV"
            fi
            nk=$((nk + (nm > 0 ? nm : 1)))
            printf '%-7s %s/%s: %s\n' SKIP "$s" optional "prerequisite missing; optional scenario, not a failure (see $log)"
        fi
    elif [ "$rc" = 0 ] && [ "$nf" = 0 ]; then
        status=PASS
    else
        status=FAIL
        if [ "$nf" = 0 ]; then
            printf '%s\t%s\t%s\t%s\t%s\n' "$s" exit FAIL "$(date +%s)" "exit status $rc without a FAIL line" >> "$CHECKS_TSV"
            nf=1
        fi
    fi
    [ "$s" = 90-cleanup ] && ran_cleanup=1
    ST[$s]=$status; RC[$s]=$rc; SECS[$s]=$secs; NPASS[$s]=$np; NFAIL[$s]=$nf; NSKIP[$s]=$nk
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$s" "$status" "$rc" "$secs" "$(meta "$script" expected)" \
        "$np" "$nf" "$nk" "$log" >> "$RESULTS_TSV"
    if [ "$INTERRUPTED" = 1 ]; then
        OVERALL=1
        echo "interrupted during $s: stopping"
        break
    fi
    if [ "$status" != PASS ] && [ "$status" != SKIP ]; then
        OVERALL=1
        if [ "$FAIL_FAST" = 1 ]; then
            echo "--fail-fast: stopping after $s ($status)"
            break
        fi
    fi
done

if [ "$KEEP_ALL" = 0 ] && [ "$ran_cleanup" = 0 ]; then
    echo
    echo "final cleanup (90-cleanup was not selected)"
    suite_cleanup
fi

# ------------------------------------------------------------------ results.json

json_str() {
    local s=$1
    s=${s//\\/\\\\}
    s=${s//\"/\\\"}
    s=$(printf '%s' "$s" | tr -d '\000-\037')
    printf '"%s"' "$s"
}
FINISHED=$(date -u +%Y-%m-%dT%H:%M:%SZ)
{
    printf '{\n  "suite": "xkr-testnet",\n  "work_dir": %s,\n  "started": "%s",\n  "finished": "%s",\n' \
        "$(json_str "$WD")" "$STARTED" "$FINISHED"
    printf '  "xkrd": %s,\n  "xkrd_path": %s,\n' "$(json_str "$( [ -x "$XKRD" ] && xkrd_version || echo missing)")" "$(json_str "$XKRD")"
    printf '  "status": "%s",\n  "scenarios": [' "$([ "$OVERALL" = 0 ] && echo PASS || echo FAIL)"
    first=1
    for s in "${selected[@]}"; do
        [ -n "${ST[$s]:-}" ] || continue
        [ "$first" = 1 ] || printf ','
        first=0
        printf '\n    {"name": %s, "status": "%s", "exit": %s, "seconds": %s, "expected_seconds": %s, "passed": %s, "failed": %s, "skipped": %s, "log": %s, "checks": [' \
            "$(json_str "$s")" "${ST[$s]}" "${RC[$s]}" "${SECS[$s]}" "$(meta "$SCEN_DIR/$s.sh" expected | grep . || echo null)" \
            "${NPASS[$s]}" "${NFAIL[$s]}" "${NSKIP[$s]}" "$(json_str "$WD/logs/$s.log")"
        cfirst=1
        while IFS=$'\t' read -r cs cid cst cts cmsg; do
            [ "$cs" = "$s" ] || continue
            [ "$cfirst" = 1 ] || printf ','
            cfirst=0
            printf '\n      {"id": %s, "status": "%s", "time": %s, "message": %s}' \
                "$(json_str "$cid")" "$cst" "${cts:-0}" "$(json_str "$cmsg")"
        done < "$CHECKS_TSV"
        [ "$cfirst" = 1 ] && printf ']}' || printf '\n    ]}'
    done
    printf '\n  ]\n}\n'
} > "$RESULTS_JSON"

# ------------------------------------------------------------------ summary

echo
echo "================================================================ xkr testnet suite"
printf '%-24s %-8s %6s %8s %6s %6s %6s\n' scenario status exit seconds pass fail skip
for s in "${selected[@]}"; do
    [ -n "${ST[$s]:-}" ] || { printf '%-24s %-8s\n' "$s" "not run"; continue; }
    printf '%-24s %-8s %6s %8s %6s %6s %6s\n' "$s" "${ST[$s]}" "${RC[$s]}" "${SECS[$s]}" \
        "${NPASS[$s]}" "${NFAIL[$s]}" "${NSKIP[$s]}"
done
if [ "$OVERALL" = 0 ]; then
    echo "SUITE PASS  ($RESULTS_TSV, $RESULTS_JSON)"
else
    echo "SUITE FAIL  ($RESULTS_TSV, $RESULTS_JSON)"
    awk -F'\t' '$3 == "FAIL" || $3 == "MISSING" {printf "  %-8s %s/%s: %s\n", $3, $1, $2, $5}' "$CHECKS_TSV" | head -n 40
fi
[ "$KEEP_ALL" = 1 ] && echo "--keep: nodes left running; testnet/suite.sh $WD 90 cleans up"
exit "$OVERALL"
