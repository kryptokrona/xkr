"""Collect (amount, global index) -> output key for blocks [0, END] via /queryblocksdetailed."""
import json, sys, urllib.request
R = "http://node.myxkr.com:11898"
END = int(sys.argv[1]); OUT = sys.argv[2]
def post(p, b, t=600):
    req = urllib.request.Request(R + p, data=json.dumps(b).encode(), headers={"Content-Type": "application/json"})
    return json.loads(urllib.request.urlopen(req, timeout=t).read())
def rpc(m, p): return post("/json_rpc", {"jsonrpc": "2.0", "id": "0", "method": m, "params": p})["result"]
G = rpc("getblockheaderbyheight", {"height": 0})["block_header"]["hash"]
outs = {}
h = 0
while h <= END:
    ids = [G] if h == 0 else [rpc("getblockheaderbyheight", {"height": h})["block_header"]["hash"], G]
    d = post("/queryblocksdetailed", {"blockIds": ids, "timestamp": 0, "blockCount": 200})
    assert d["startHeight"] == h, (d["startHeight"], h)
    for b in d["blocks"]:
        if b["index"] > END: break
        for t in b["transactions"]:
            for o in t["outputs"]:
                outs[f'{o["output"]["amount"]}:{o["globalIndex"]}'] = o["output"]["target"]["data"]["key"]
    h = d["blocks"][-1]["index"] + 1
    print("upto", h, len(outs), file=sys.stderr, flush=True)
json.dump(outs, open(OUT, "w"))
