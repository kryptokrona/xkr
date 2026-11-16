#!/usr/bin/env bash
# Builds the seed corpus for every fuzz target from the repository's fixtures and test vectors:
#
#   crates/xkr-serialization/tests/fixtures/blocks.json  real mainnet blocks and transactions
#   docs/test-vectors/hyper-bridge-frames.json           golden bridge frames
#   docs/test-vectors/hugin-relay.json                   bare-rpc frames captured from bare-rpc
#
# plus epee / Levin / storage / RPC inputs synthesised from those blocks. Writes
# fuzz/corpus/<target>/<name>; existing files are kept (cargo-fuzz adds its own). Needs only
# python3 (standard library). Usage: fuzz/seeds.sh
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/.." && pwd)"

python3 -I - "$root" <<'PY'
import hashlib, json, os, struct, sys

root = sys.argv[1]
corpus = os.path.join(root, "fuzz", "corpus")


def put(target, name, data):
    d = os.path.join(corpus, target)
    os.makedirs(d, exist_ok=True)
    with open(os.path.join(d, name), "wb") as f:
        f.write(data)


def cn_varint(v):
    out = bytearray()
    while v >= 0x80:
        out.append((v & 0x7F) | 0x80)
        v >>= 7
    out.append(v)
    return bytes(out)


def epee_varint(v):
    if v <= 63:
        return bytes([v << 2])
    if v <= 16383:
        return struct.pack("<H", (v << 2) | 1)
    if v <= 1073741823:
        return struct.pack("<I", (v << 2) | 2)
    return struct.pack("<Q", (v << 2) | 3)


EPEE_HEADER = bytes([0x01, 0x11, 0x01, 0x01, 0x01, 0x01, 0x02, 0x01, 0x01])
T_U32, T_STRING, T_OBJECT, ARRAY = 6, 10, 12, 0x80


def name(n):
    return bytes([len(n)]) + n.encode()


def f_u32(n, v):
    return name(n) + bytes([T_U32]) + struct.pack("<I", v)


def f_str(n, b):
    return name(n) + bytes([T_STRING]) + epee_varint(len(b)) + b


def f_str_array(n, items):
    out = name(n) + bytes([ARRAY | T_STRING]) + epee_varint(len(items))
    for b in items:
        out += epee_varint(len(b)) + b
    return out


def obj(fields):
    return epee_varint(len(fields)) + b"".join(fields)


def f_obj(n, fields):
    return name(n) + bytes([T_OBJECT]) + obj(fields)


def f_obj_array(n, objects):
    out = name(n) + bytes([ARRAY | T_OBJECT]) + epee_varint(len(objects))
    for fields in objects:
        out += obj(fields)
    return out


def storage(fields):
    return EPEE_HEADER + obj(fields)


def levin(command, body, have_to_return=False, flags=1):
    head = struct.pack(
        "<QQBIiII", 0x0101010101012101, len(body), int(have_to_return), command, 0, flags, 1
    )
    return head + body


blocks = json.load(open(os.path.join(root, "crates/xkr-serialization/tests/fixtures/blocks.json")))
GENESIS_TX = bytes.fromhex(
    "010a01ff000188f3b501029b2e4c0281c0b02e7c53291a94d1d0cbff8883f8024f5142ee494ffbbd088071210142694232c5b04151d9e4c27d31ec7a68ea568b19488cfcb422659a07a0e44dd5"
)
put("cn_transaction", "genesis_coinbase", GENESIS_TX)

epee_seeds = []
for b in blocks:
    h = b["height"]
    blob = bytes.fromhex(b["blob"])
    txs = [bytes.fromhex(t["blob"]) for t in b["txs"]]
    put("cn_block", f"block_{h}", blob)
    raw = cn_varint(len(blob)) + blob + cn_varint(len(txs))
    for t in txs:
        raw += cn_varint(len(t)) + t
    put("cn_block", f"raw_{h}", raw)
    for i, t in enumerate(txs[:4]):
        put("cn_transaction", f"tx_{h}_{i}", t)

    # epee messages as the C++ node writes them
    entry = [f_str("block", blob)] + ([f_str_array("txs", txs)] if txs else [])
    epee_seeds.append((2001, f"new_block_{h}", storage(
        [f_obj("b", entry), f_u32("current_blockchain_height", h + 1), f_u32("hop", 0)])))
    if txs:
        epee_seeds.append((2002, f"new_txs_{h}", storage([f_str_array("txs", txs)])))
    epee_seeds.append((2004, f"get_objects_{h}", storage(
        [f_obj_array("blocks", [entry]), f_u32("current_blockchain_height", h + 1)])))

    # storage: a block record (keys not stripped) and a one-record chunk
    rec = bytes([1]) + cn_varint(len(blob)) + blob + cn_varint(0) + cn_varint(0)
    rec += cn_varint(len(txs))
    for t in txs:
        rec += cn_varint(len(t)) + t + cn_varint(0) + cn_varint(0) + cn_varint(0)
    put("storage_record", f"record_{h}", rec)
    put("storage_record", f"chunk_{h}", bytes([1]) + cn_varint(1) + cn_varint(len(rec)) + rec)
    put("storage_record", f"raw_frame_{h}", bytes([0]) + struct.pack("<I", len(rec)) + rec)

ids = b"".join(hashlib.sha256(bytes([i])).digest() for i in range(4))
epee_seeds.append((2006, "request_chain", storage([f_str("block_ids", ids)])))
epee_seeds.append((2007, "chain_entry", storage(
    [f_u32("start_height", 1), f_u32("total_height", 9), f_str("m_block_ids", ids)])))
epee_seeds.append((2008, "request_tx_pool", storage([f_str("txs", ids[:64])])))
epee_seeds.append((2010, "missing_txs", storage(
    [f_u32("current_blockchain_height", 5), f_str("blockHash", ids[:32]), f_str("missing_txs", ids)])))
for cmd, n, body in epee_seeds:
    put("epee", n, body)
    # first byte = chunk size of the split stream
    put("levin_header", n, bytes([7]) + levin(cmd, body))
# a handshake header first lifts the pre-handshake size limit
put("levin_header", "handshake_then_ping", bytes([33]) + levin(1001, b"", True) + levin(1003, b"", True))

frames = json.load(open(os.path.join(root, "docs/test-vectors/hyper-bridge-frames.json")))
stream = b""
for v in frames["vectors"]:
    raw = bytes.fromhex(v["hex"])
    put("hyper_frame", v["name"], raw)
    stream += raw
put("hyper_frame", "all_vectors_stream", bytes([5]) + stream)

hugin = json.load(open(os.path.join(root, "docs/test-vectors/hugin-relay.json")))
for v in hugin["rpc"]:
    for key in ("hex", "request_hex"):
        if key in v:
            raw = bytes.fromhex(v[key])
            put("hugin_packet", f"{v['name']}_{key}", raw)
            put("hugin_packet", f"{v['name']}_{key}_stream", bytes([6]) + raw)
    data = v.get("data")
    for i, text in enumerate(data if isinstance(data, list) else [data]):
        if isinstance(text, str):
            put("hugin_packet", f"{v['name']}_json_{i}", bytes([0]) + text.encode())

# rpc_json: byte 0 = kind (even: raw body, odd: JSON-RPC), byte 1 = path / method index,
# byte 2 = GET (1) or POST (0), rest = body or params
rpc = [
    (0, 1, b'{"blockHashCheckpoints":[],"startHeight":0,"startTimestamp":0,"blockCount":100}'),
    (0, 2, b'{"startHeight":0,"endHeight":10}'),
    (0, 3, b'{"transactionHashes":["' + ids[:32].hex().encode() + b'"]}'),
    (0, 4, b'{"txid":"' + ids[:32].hex().encode() + b'"}'),
    (0, 5, b'{"txs_hashes":["' + ids[:32].hex().encode() + b'"]}'),
    (0, 6, b'{"tx_as_hex":"' + GENESIS_TX.hex().encode() + b'"}'),
    (0, 7, b'{"amounts":[1,10,100],"outs_count":4}'),
    (0, 8, b'{"tailBlockId":"","knownTxsIds":[]}'),
    (1, 1, b"[0]"),
    (1, 4, b'{"height":0}'),
    (1, 3, b'{"hash":"' + ids[:32].hex().encode() + b'"}'),
    (1, 7, b'{"reserve_size":8,"wallet_address":"SEKReYU57DLLvUjNzmjVhaK7jqc8SdZZ3cyKJS5f4gWXK4NQQYChzKUUwzCGhgqUPkWQypeR94rqpgMPjXWG9ijnZKNw2LWXnZU"}'),
    (1, 8, b'["' + bytes.fromhex(blocks[1]["blob"]).hex().encode() + b'"]'),
    (1, 9, b'{"height":1}'),
    (1, 10, b'{"hash":"0"}'),
]
for i, (kind, pick, body) in enumerate(rpc):
    put("rpc_json", f"rpc_{i}", bytes([kind, pick, 0]) + body)

for t in sorted(os.listdir(corpus)):
    print(f"{t}: {len(os.listdir(os.path.join(corpus, t)))} seeds")
PY
