import json, sys
T = 90
M64 = (1 << 64) - 1

def version_for(h):
    for v, up in ((5, 4), (4, 3), (3, 2), (2, 1)):
        if up < h:
            return v
    return 1

def blocks_count(v, idx):
    if idx >= 620000:
        return 61
    win = 17 if v >= 3 else 2880
    lag = 0 if v >= 3 else 15
    return win + lag

def original(v, idx, ts, cd):
    ts_o, cd_o = list(ts), list(cd)
    win = 17 if v >= 3 else 2880
    cut = 0 if v >= 3 else 60
    ts = ts[:win]; cd = cd[:win]
    n = len(ts)
    if n <= 1: return 1
    ts = sorted(ts)
    if n <= win - 2 * cut:
        b, e = 0, n
    else:
        b = (n - (win - 2 * cut) + 1) // 2; e = b + (win - 2 * cut)
    span = ts[e - 1] - ts[b] or 1
    work = cd[e - 1] - cd[b]
    prod = work * T
    if prod >> 64 or M64 - (prod & M64) < span - 1: return 0
    if v >= 3:
        return prod // span
    raise NotImplementedError

def lwma(ts, cd, kind):
    N = 60
    if kind == 6:
        if len(ts) <= 10: return 10000
        if len(ts) < 61: N = len(ts) - 1
        L = 0; s3 = 0; prev = ts[0]
        for i in range(1, N + 1):
            this = ts[i] if ts[i] > prev else prev + 1
            ST = min(6 * T, this - prev); prev = this
            L += ST * i
            if i > N - 3: s3 += ST
        nd = ((cd[N] - cd[0]) * T * (N + 1) * 99) // (100 * 2 * L)
        pd = cd[N] - cd[N - 1]
        nd = max(pd * 67 // 100, min(nd, pd * 150 // 100))
        if s3 < 8 * T // 10: nd = max(nd, pd * 108 // 100)
        return nd
    def tdiv(a, b):  # C truncation
        q = abs(a) // abs(b)
        return q if (a >= 0) == (b > 0) else -q
    if kind == 5 and len(ts) < N + 1: return 10000
    if kind in (3, 4) and len(ts) <= N: return 1000
    L = 0; s3 = 0
    for i in range(1, N + 1):
        st = ts[i] - ts[i - 1]
        if kind == 5: st = max(-4 * T, min(st, 6 * T))
        elif kind == 4: st = max(st, -6 * T)  # clamp(n=-6T, lower=st, upper=6T) quirk
        else: st = max(-3 * T, min(st, 6 * T))
        L += st * i
        if i > N - 3: s3 += st
    nd = tdiv((cd[N] - cd[0]) * T * (N + 1) * 99, 100 * 2 * L)
    pd = cd[N] - cd[N - 1]
    if kind == 3:
        nd = max(tdiv(pd * 70, 100), min(nd, tdiv(pd * 107, 100)))
        if s3 < 8 * T // 10: nd = tdiv(pd * 110, 100)
    else:
        nd = max(tdiv(pd * 67, 100), min(nd, tdiv(pd * 150, 100)))
        if s3 < 8 * T // 10: nd = max(nd, tdiv(pd * (108 if kind == 5 else 110), 100))
    return nd

def next_diff(v, idx, ts, cd):
    if idx >= 2200000: return lwma(ts, cd, 6)
    if idx >= 800000: return lwma(ts, cd, 5)
    if idx >= 700000: return lwma(ts, cd, 4)
    if idx >= 620000: return lwma(ts, cd, 3)
    return original(v, idx, ts, cd)

hdrs = {h["height"]: h for h in json.load(open(sys.argv[1]))}
ok = bad = 0
for H in sorted(hdrs):
    idx = H - 1
    v = version_for(H)
    n = min(blocks_count(v, idx), idx)  # blocks [max(1, H-n) .. H-1]
    rng = range(max(1, H - blocks_count(v, idx)), H)
    if not all(x in hdrs for x in rng): continue
    ts = [hdrs[x]["timestamp"] for x in rng]
    cum = 0; cd = []
    for x in rng:
        cum += hdrs[x]["difficulty"]; cd.append(cum)
    got = next_diff(v, idx, ts, cd)
    if got == hdrs[H]["difficulty"]: ok += 1
    else:
        bad += 1
        if bad < 10: print("MISMATCH", H, got, hdrs[H]["difficulty"])
print("ok", ok, "bad", bad)
