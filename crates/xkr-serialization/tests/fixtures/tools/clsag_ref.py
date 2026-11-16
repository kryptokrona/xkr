"""Independent Python reference for transaction version 2 (docs/tx-v2.md).

* view tags: checks Monero's derive_view_tag vectors (tests/crypto/tests.txt, passed as argv[1]
  if given) and prints nothing else for them;
* single-key CLSAG: signs the fixed vectors of crates/xkr-crypto/src/clsag.rs (same ring
  secrets, message and deterministic nonce stream) and prints them as Rust `Vector` literals.

Run: python3 -I clsag_ref.py [path/to/monero/tests/crypto/tests.txt] (from this directory).
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from ref import keccak, varint  # noqa: E402
from ed import G, L, decode, encode, hash_to_ec, hs, is_identity, padd, smul  # noqa: E402


def view_tag(derivation, index):
    return keccak(b"view_tag" + derivation + varint(index))[0]


def pad32(s):
    return s + bytes(32 - len(s))


AGG = pad32(b"XKR_CLSAG_agg_0")
ROUND = pad32(b"XKR_CLSAG_round")


class StreamRng:
    """Block k = keccak("xkr clsag test rng" || seed_le64 || k_le64), concatenated."""

    def __init__(self, seed):
        self.seed = seed
        self.block = 0
        self.buf = b""

    def bytes(self, n):
        while len(self.buf) < n:
            self.buf += keccak(b"xkr clsag test rng" + self.seed.to_bytes(8, "little")
                               + self.block.to_bytes(8, "little"))
            self.block += 1
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def scalar(self):
        # random_scalar: 64 bytes reduced mod l (sc_reduce)
        return int.from_bytes(self.bytes(64), "little") % L


def le(x):
    return x.to_bytes(32, "little")


def mu_of(ring, ki):
    return hs(AGG + b"".join(ring) + ki)


def sign(msg, ring, x, l, rng):
    n = len(ring)
    pts = [decode(p) for p in ring]
    assert encode(smul(G, x)) == ring[l]
    hp = hash_to_ec(ring[l])
    I = smul(hp, x)
    ki = encode(I)
    mu = mu_of(ring, ki)
    base = ROUND + b"".join(ring) + msg
    alpha = rng.scalar()
    c = hs(base + encode(smul(G, alpha)) + encode(smul(hp, alpha)))
    s = [0] * n
    c1 = None
    i = (l + 1) % n
    if i == 0:
        c1 = c
    while i != l:
        s[i] = rng.scalar()
        cp = c * mu % L
        Lp = padd(smul(G, s[i]), smul(pts[i], cp))
        Rp = padd(smul(hash_to_ec(ring[i]), s[i]), smul(I, cp))
        c = hs(base + encode(Lp) + encode(Rp))
        i = (i + 1) % n
        if i == 0:
            c1 = c
    s[l] = (alpha - c * mu * x) % L
    return ki, mu, c1, s


def verify(msg, ring, ki, c1, s):
    n = len(ring)
    if n == 0 or len(s) != n or c1 >= L or any(x >= L for x in s):
        return False
    I = decode(ki)
    if I is None or is_identity(I) or not is_identity(smul(I, L)):
        return False
    mu = mu_of(ring, ki)
    base = ROUND + b"".join(ring) + msg
    c = c1
    for i in range(n):
        P = decode(ring[i])
        if P is None:
            return False
        cp = c * mu % L
        Lp = padd(smul(G, s[i]), smul(P, cp))
        Rp = padd(smul(hash_to_ec(ring[i]), s[i]), smul(I, cp))
        c = hs(base + encode(Lp) + encode(Rp))
        if c == 0:
            return False
    return c == c1


def main():
    if len(sys.argv) > 1:
        n = 0
        with open(sys.argv[1]) as f:
            for line in f:
                parts = line.split()
                if parts and parts[0] == "derive_view_tag":
                    d, i, t = bytes.fromhex(parts[1]), int(parts[2]), int(parts[3], 16)
                    assert view_tag(d, i) == t, line
                    n += 1
        print(f"// {n} Monero view tag vectors OK", file=sys.stderr)

    msg = keccak(b"xkr clsag vector message")
    print("const VECTORS: &[Vector] = &[")
    for (n, l, seed) in ((1, 0, 11), (2, 1, 12), (4, 2, 13), (11, 7, 14)):
        xs = [hs(b"xkr clsag vector key" + bytes([i])) for i in range(n)]
        ring = [encode(smul(G, x)) for x in xs]
        ki, mu, c1, s = sign(msg, ring, xs[l], l, StreamRng(seed))
        assert verify(msg, ring, ki, c1, s)
        bad = list(s)
        bad[0] = (bad[0] + 1) % L
        assert not verify(msg, ring, ki, c1, bad)
        print("    Vector {")
        print(f"        n: {n},")
        print(f"        index: {l},")
        print(f"        seed: {seed},")
        print(f'        key_image: "{ki.hex()}",')
        print(f'        mu: "{le(mu).hex()}",')
        print(f'        c1: "{le(c1).hex()}",')
        print("        s: &[")
        for x in s:
            print(f'            "{le(x).hex()}",')
        print("        ],")
        print("    },")
    print("];")


if __name__ == "__main__":
    main()
