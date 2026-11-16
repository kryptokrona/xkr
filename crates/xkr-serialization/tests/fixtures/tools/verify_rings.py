import sys, json
sys.path.insert(0, "scripts")
from ref import parse_tx, R, keccak
from ed import check_ring
blocks = {b["height"]: b for b in json.load(open(sys.argv[1]))}
rings = json.load(open(sys.argv[2]))
ok = bad = 0
for r in rings:
    t = [t for t in blocks[r["height"]]["txs"] if t["hash"] == r["tx_hash"]][0]
    tx = parse_tx(R(bytes.fromhex(t["blob"])))
    ph = keccak(tx["prefix_bytes"])
    for i, keys in enumerate(r["ring_keys"]):
        if keys is None: continue
        if check_ring(ph, tx["ins"][i][3], [bytes.fromhex(k) for k in keys], tx["sigs"][i]): ok += 1
        else: bad += 1; print("BAD", r["tx_hash"], i)
print("ok", ok, "bad", bad)
