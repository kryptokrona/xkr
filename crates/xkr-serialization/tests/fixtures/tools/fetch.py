"""Fetch real Kryptokrona mainnet fixtures from a public node.

Writes JSON fixtures into the directory given as argv[1].
"""
import json
import sys
import urllib.request

import os
R = os.environ.get("XKR_RPC", "http://node.myxkr.com:11898")
OUT = sys.argv[1] if len(sys.argv) > 1 else "."


def post_raw(path, body, timeout=180):
    req = urllib.request.Request(R + path, data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.read().decode()


def post(path, body, timeout=180):
    return json.loads(post_raw(path, body, timeout))


def rpc(method, params):
    d = post("/json_rpc", {"jsonrpc": "2.0", "id": "0", "method": method, "params": params})
    if "result" not in d:
        raise RuntimeError(f"{method} {params}: {d}")
    return d["result"]


def header(h):
    return rpc("getblockheaderbyheight", {"height": h})["block_header"]


def headers_range(a, b):
    out = []
    while a <= b:
        e = min(b, a + 999)
        out += rpc("get_block_headers_range", {"start_height": a, "end_height": e})["headers"]
        a = e + 1
    return out


GENESIS = header(0)["hash"]


def raw_blocks(h, n):
    """Return [(block_hex, [tx_hex,...])] for heights h..h+n-1 via /getblocks."""
    ids = [GENESIS] if h == 0 else [header(h)["hash"], GENESIS]
    text = post_raw("/getblocks", {"block_ids": ids}, timeout=600)
    top = json.loads(text, object_pairs_hook=lambda p: p)
    blocks = dict(top)["response.blocks"]
    start = dict(top)["response.start_height"]
    assert start == h, (start, h)
    res = []
    for obj in blocks[:n]:
        blk = None
        txs = []
        for k, v in obj:
            if k == "block":
                blk = v
            elif k == "transaction":
                txs.append(v)
        res.append((blk, txs))
    return res


def fblock(hash_):
    return rpc("f_block_json", {"hash": hash_})["block"]


def tx_details(hashes):
    out = {}
    for i in range(0, len(hashes), 50):
        d = post("/get_transaction_details_by_hashes", {"transactionHashes": hashes[i:i + 50]})
        for t in d["transactions"]:
            out[t["hash"]] = t
    return out


_RAW_CACHE = {}


def raw_block_cached(h):
    for start, blocks in _RAW_CACHE.items():
        if start <= h < start + len(blocks):
            return blocks[h - start]
    blocks = raw_blocks(h, 1000)
    _RAW_CACHE[h] = blocks
    return blocks[0]


def block_fixture(h):
    hdr = header(h)
    blob, txs = raw_block_cached(h)
    fb = fblock(hdr["hash"])
    prev_generated = None
    if h > 0:
        prev_generated = int(fblock(hdr["prev_hash"])["alreadyGeneratedCoins"])
    tx_hashes = [t["hash"] for t in fb["transactions"]]
    # /getblocks serializes RawBlock with repeated JSON keys, so only the last transaction
    # survives; fetch the transaction blobs by hash instead.
    if len(tx_hashes) > 1:
        d = post("/gettransactions", {"txs_hashes": tx_hashes[1:]})
        assert not d.get("missed_tx"), d.get("missed_tx")
        txs = d["txs_as_hex"]
    return {
        "height": h,
        "hash": hdr["hash"],
        "prev_hash": hdr["prev_hash"],
        "major_version": hdr["major_version"],
        "minor_version": hdr["minor_version"],
        "timestamp": hdr["timestamp"],
        "nonce": hdr["nonce"],
        "difficulty": hdr["difficulty"],
        "reward": hdr["reward"],
        "num_txes": hdr["num_txes"],
        "blob": blob,
        "miner_tx_hash": tx_hashes[0],
        "txs": [{"hash": th, "blob": tb} for th, tb in zip(tx_hashes[1:], txs)],
        "already_generated_coins": int(fb["alreadyGeneratedCoins"]),
        "already_generated_coins_prev": prev_generated,
        "size_median": fb["sizeMedian"],
        "cumulative_size": fb["transactionsCumulativeSize"],
        "total_fee": fb["totalFeeAmount"],
        "base_reward": fb["baseReward"],
    }


def main():
    targets = list(range(0, 11)) + [1000, 5052, 5053, 187000, 187001, 187014, 440000, 440001,
                                    620000, 620001, 620106, 700001, 700003, 800001, 800006,
                                    1250000, 1250013, 1300000, 1300001, 1750000, 1750003,
                                    2200000, 2200001, 2200006, 2200011, 2617000, 2617019, 2617400]
    targets += [int(x) for x in sys.argv[2:]]
    targets = sorted(set(targets))
    blocks = []
    for h in targets:
        print("block", h, file=sys.stderr, flush=True)
        blocks.append(block_fixture(h))
    with open(f"{OUT}/blocks.json", "w") as f:
        json.dump(blocks, f, indent=1)

    ranges = [(1, 300), (186900, 187100), (439950, 440050), (619900, 620100), (699900, 700100),
              (799900, 800100), (2199900, 2200100), (2616900, 2617100)]
    hdrs = []
    for a, b in ranges:
        print("headers", a, b, file=sys.stderr, flush=True)
        for x in headers_range(a, b):
            hdrs.append({k: x[k] for k in ("height", "hash", "timestamp", "difficulty", "major_version",
                                              "block_size", "reward", "num_txes")})
    with open(f"{OUT}/headers.json", "w") as f:
        json.dump(hdrs, f, indent=0)


if __name__ == "__main__":
    main()
