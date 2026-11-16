#!/usr/bin/env bash
# Compatibility wrapper: the Hyperswarm two-node test moved to testnet/scenarios/20-hyper-two-nodes.sh (part of the
# regression suite, docs/testnet-suite.md). Same environment variables as before; state and logs
# now go to <work dir>/20-hyper-two-nodes/.
#
#   testnet/hyper-two-nodes.sh <work dir>          == testnet/scenarios/20-hyper-two-nodes.sh <work dir>
#   testnet/suite.sh <work dir> 20-hyper-two-nodes  (adds results.tsv / results.json and the final cleanup)
set -euo pipefail
exec "$(dirname "$0")/scenarios/20-hyper-two-nodes.sh" "$@"
