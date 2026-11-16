#!/usr/bin/env bash
# capture-miner: the C++ solo miner's output format, its --limit behaviour and its hash rate (the
# baseline for the Rust miner benchmark).
#
#   xkrd X     fixture chain, P2P 24898 / RPC 24899 (own genesis, difficulty 1)
#   stub       127.0.0.1:24090, a tiny JSON-RPC server (python3) that serves one real template of X
#              with difficulty MINER_BENCH_DIFFICULTY, so the miner hashes without ever finding a
#              block and prints its "Mining at ..." line every 60 s
#   miner      every image in MINER_IMAGES (default: FX_IMAGE and CPP_OLD_IMAGE), --cpus 2
#
# Per image: --help and --version; --limit 3 against X (exit status, blocks the chain gained,
# seconds; the 2025 build aborts after the limit, the master build relaunches and keeps mining,
# so the run is stopped 10 s after "Mined requested amount"); a hash-rate
# run of MINER_BENCH_SECS seconds per thread count (stdout line-buffered through stdbuf: the miner's
# block-buffered output was lost when the container was killed) in MINER_BENCH_THREADS against the stub. The
# template is block major version 5 (CN-Turtle-Lite v2, the mainnet algorithm before fork 13).
#
# Output: crates/xkr-miner/tests/fixtures/cpp/<image tag>-{help,version,limit,bench-<n>t}.txt
#         (normalised) and summary.json
# Env: MINER_IMAGES, MINER_BENCH_SECS (130), MINER_BENCH_THREADS ("1 2"),
#      MINER_BENCH_DIFFICULTY (1000000000000), MINER_LIMIT (3). Never builds anything.
# suite: expected=660 timeout=1800
set -euo pipefail
: "${TIMEOUT:=300}"
. "$(dirname "$0")/lib-wallet.sh"
fx_init "${1:-}" fx-miner

MINER_BENCH_SECS=${MINER_BENCH_SECS:-130}
MINER_BENCH_THREADS=${MINER_BENCH_THREADS:-"1 2"}
MINER_BENCH_DIFFICULTY=${MINER_BENCH_DIFFICULTY:-1000000000000}
MINER_LIMIT=${MINER_LIMIT:-3}
OUT=$FX_OUT_ROOT/crates/xkr-miner/tests/fixtures/cpp

fx_fresh_chain
fx_mine "$FX_DUMMY_ADDR" 10
mkdir -p "$SD/m"
: > "$SD/summary.jsonl"

# ------------------------------------------------------------------ stub daemon

rpc "$FX_RPC" getblocktemplate "{\"wallet_address\":\"$FX_DUMMY_ADDR\",\"reserve_size\":0}" \
    | jq -c --argjson d "$MINER_BENCH_DIFFICULTY" '.result.difficulty = $d' > "$SD/template.json"
rpc "$FX_RPC" getlastblockheader > "$SD/header.json"
jq -e '.result.blocktemplate_blob' "$SD/template.json" > /dev/null || die stub "no template from X: $(head -c 300 "$SD/template.json")"
cat > "$SD/stub.py" <<'PY'
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

port, template, header = int(sys.argv[1]), sys.argv[2], sys.argv[3]
answers = {'getblocktemplate': json.load(open(template)), 'getlastblockheader': json.load(open(header))}


class H(BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get('Content-Length') or 0)
        try:
            req = json.loads(self.rfile.read(n) or b'{}')
        except ValueError:
            req = {}
        method = req.get('method', '')
        if method in answers:
            out = dict(answers[method])
            out['id'] = req.get('id', 0)
        elif method == 'submitblock':
            out = {'jsonrpc': '2.0', 'id': req.get('id', 0), 'result': {'status': 'OK'}}
        else:
            out = {'jsonrpc': '2.0', 'id': req.get('id', 0), 'error': {'code': -32601, 'message': 'Method not found'}}
        sys.stdout.write('%s\n' % method)
        sys.stdout.flush()
        body = json.dumps(out).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


HTTPServer(('127.0.0.1', port), H).serve_forever()
PY
require_ports_free "$PORT_FX_STUB"
nice -n 19 python3 -I "$SD/stub.py" "$PORT_FX_STUB" "$SD/template.json" "$SD/header.json" > "$SD/stub.log" 2>&1 < /dev/null &
FX_STUB_PID=$!
wait_for 15 port_busy "$PORT_FX_STUB" || die stub "stub daemon did not listen on $PORT_FX_STUB ($(tail -n 5 "$SD/stub.log"))"

# ------------------------------------------------------------------ per image

for img in $MINER_IMAGES; do
    if ! docker image inspect "$img" > /dev/null 2>&1; then
        skip "$img" "image $img not present (docker pull it, or build it with testnet/build-cpp.sh for $CPP_OLD_IMAGE)"
        continue
    fi
    tag=$(printf '%s' "$img" | tr -c 'A-Za-z0-9._-' '_' | sed 's/_*$//')
    case $img in
        ghcr.io/kryptokrona/kryptokrona-testnet:*) tag=published ;;
        "$CPP_OLD_IMAGE") tag=master ;;
    esac
    fx_image_report "$img" "$SD/image-$tag.json"
    fx_docker_once "$img" "$FX_BIN/miner" --help > "$SD/m/$tag-help.txt"
    fx_docker_once "$img" "$FX_BIN/miner" --version > "$SD/m/$tag-version.txt"

    h0=$(height_of "$FX_RPC")
    t0=$(date +%s)
    # stopped 10 s after "Mined requested amount" (or after 120 s): chainGained then shows
    # whether the process really ended or started mining again
    rc=$(fx_tool_timed "$SD/m/$tag-limit.txt" "$img" 120 "Mined requested amount" 10 miner \
        --daemon-address "$FX_RPC" --address "$FX_DUMMY_ADDR" --threads 1 --scan-time 1 --limit "$MINER_LIMIT")
    secs=$(($(date +%s) - t0))
    sleep 3
    h1=$(height_of "$FX_RPC")
    found=$(grep -ac "Block found" "$SD/m/$tag-limit.txt" || true)
    jq -cn --arg img "$img" --arg tag "$tag" --arg rc "$rc" --argjson h0 "$h0" --argjson h1 "$h1" \
        --argjson secs "$secs" --argjson limit "$MINER_LIMIT" --argjson found "${found:-0}" \
        --arg tail "$(tail -n 6 "$SD/m/$tag-limit.txt" | sed 's/\x1b\[[0-9;]*m//g')" \
        '{kind: "limit", image: $img, tag: $tag, limit: $limit, exitCode: $rc, chainGained: ($h1 - $h0),
          blockFoundLines: $found, seconds: $secs, lastLines: ($tail | split("\n"))}' >> "$SD/summary.jsonl"
    note "$tag --limit $MINER_LIMIT: exit $rc, chain +$((h1 - h0)) blocks, $found \"Block found\" lines, ${secs}s"
    if [ "$((h1 - h0))" -ge "$MINER_LIMIT" ]; then
        pass "$tag" "--limit $MINER_LIMIT: the miner added $((h1 - h0)) blocks to X and exited with $rc"
    else
        fail "$tag" "--limit $MINER_LIMIT: X gained only $((h1 - h0)) blocks (exit $rc, see $SD/m/$tag-limit.txt)"
    fi

    for n in $MINER_BENCH_THREADS; do
        f=$SD/m/$tag-bench-${n}t.txt
        rc=$(fx_tool_timed "$f" "$img" "$MINER_BENCH_SECS" - 0 miner --daemon-address "127.0.0.1:$PORT_FX_STUB" \
            --address "$FX_DUMMY_ADDR" --threads "$n" --scan-time 1)
        rates=$(sed 's/\x1b\[[0-9;]*m//g' "$f" | { grep -aoE 'Mining at [0-9.]+ ?[A-Za-z/]+' || true; } | tr '\n' '|')
        jq -cn --arg img "$img" --arg tag "$tag" --argjson n "$n" --argjson secs "$MINER_BENCH_SECS" \
            --arg rc "$rc" --arg rates "$rates" --argjson d "$MINER_BENCH_DIFFICULTY" \
            --arg cpu "$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //')" \
            '{kind: "bench", image: $img, tag: $tag, threads: $n, seconds: $secs, difficulty: $d,
              exitCode: $rc, hashrateLines: ($rates | split("|") | map(select(length > 0))),
              hostCpu: $cpu, dockerCpus: 2, nice: 19}' >> "$SD/summary.jsonl"
        if [ -n "$rates" ]; then
            pass "$tag" "$n thread(s): ${rates//|/; }"
        else
            fail "$tag" "$n thread(s): no \"Mining at\" line in ${MINER_BENCH_SECS}s ($f)"
        fi
    done
done

# ------------------------------------------------------------------ publish

mkdir -p "$OUT"
rm -f "$OUT"/*.txt "$OUT"/summary.json
for f in "$SD"/m/*.txt; do
    fx_normalise text "$f" "$OUT/$(basename "$f")"
done
jq -s --slurpfile tpl "$SD/template.json" \
    '{template: {height: $tpl[0].result.height, majorVersion: ($tpl[0].result.blocktemplate_blob[0:2]),
                 algorithm: "CN-Turtle-Lite v2 (block major 5)"},
      runs: .}' "$SD/summary.jsonl" > "$OUT/summary.json"
pass publish "$(ls "$OUT"/*.txt | wc -l) miner transcripts and summary.json"
finish
