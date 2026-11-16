#!/usr/bin/env bash
# Compatibility wrapper: the fork-14 scenario moved to testnet/scenarios/31-fork14.sh (part of the
# regression suite, docs/testnet-suite.md). Same environment variables as before; state and logs
# now go to <work dir>/31-fork14/.
#
#   testnet/fork14-scenario.sh <work dir>          == testnet/scenarios/31-fork14.sh <work dir>
#   testnet/suite.sh <work dir> 31-fork14  (adds results.tsv / results.json and the final cleanup)
set -euo pipefail
exec "$(dirname "$0")/scenarios/31-fork14.sh" "$@"
