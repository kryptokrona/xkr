#!/usr/bin/env bash
# Compatibility wrapper: the Hugin relay test moved to testnet/scenarios/40-hugin-relay.sh (part of the
# regression suite, docs/testnet-suite.md). Same environment variables as before; state and logs
# now go to <work dir>/40-hugin-relay/.
#
#   testnet/hugin-relay-test.sh <work dir>          == testnet/scenarios/40-hugin-relay.sh <work dir>
#   testnet/suite.sh <work dir> 40-hugin-relay  (adds results.tsv / results.json and the final cleanup)
set -euo pipefail
exec "$(dirname "$0")/scenarios/40-hugin-relay.sh" "$@"
