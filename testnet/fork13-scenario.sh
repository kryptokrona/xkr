#!/usr/bin/env bash
# Compatibility wrapper: the fork-13 scenario moved to testnet/scenarios/30-fork13.sh (part of the
# regression suite, docs/testnet-suite.md). Same environment variables as before; state and logs
# now go to <work dir>/30-fork13/.
#
#   testnet/fork13-scenario.sh <work dir>          == testnet/scenarios/30-fork13.sh <work dir>
#   testnet/suite.sh <work dir> 30-fork13  (adds results.tsv / results.json and the final cleanup)
set -euo pipefail
exec "$(dirname "$0")/scenarios/30-fork13.sh" "$@"
