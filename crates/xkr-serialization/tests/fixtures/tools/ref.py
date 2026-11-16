"""Independent Python reference of the C++ serialization/hashing rules, used to validate the
fixtures and the algorithms ported to Rust (run with python3 -I)."""
import json, sys

M64 = (1 << 64) - 1
RC = [0x0000000000000001, 0x0000000000008082, 0x800000000000808a, 0x8000000080008000,
      0x000000000000808b, 0x0000000080000001, 0x8000000080008081, 0x8000000000008009,
      0x000000000000008a, 0x0000000000000088, 0x0000000080008009, 0x000000008000000a,
      0x000000008000808b, 0x800000000000008b, 0x8000000000008089, 0x8000000000008003,
      0x8000000000008002, 0x8000000000000080, 0x000000000000800a, 0x800000008000000a,
      0x8000000080008081, 0x8000000000008080, 0x0000000080000001, 0x8000000080008008]
ROTC = [1, 3, 6, 10, 15, 21, 28, 36, 45, 55, 2, 14, 27, 41, 56, 8, 25, 43, 62, 18, 39, 61, 20, 44]
PILN = [10, 7, 11, 17, 18, 3, 5, 16, 8, 21, 24, 4, 15, 23, 19, 13, 12, 2, 20, 14, 22, 9, 6, 1]


def rol(x, n):
    return ((x << n) | (x >> (64 - n))) & M64


def keccakf(st):
    for rnd in range(24):
        bc = [st[i] ^ st[i + 5] ^ st[i + 10] ^ st[i + 15] ^ st[i + 20] for i in range(5)]
        for i in range(5):
            t = bc[(i + 4) % 5] ^ rol(bc[(i + 1) % 5], 1)
            for j in range(0, 25, 5):
                st[j + i] ^= t
        t = st[1]
        for i in range(24):
            j = PILN[i]
            bc0 = st[j]
            st[j] = rol(t, ROTC[i])
            t = bc0
        for j in range(0, 25, 5):
            row = st[j:j + 5]
            for i in range(5):
                st[j + i] ^= (~row[(i + 1) % 5] & M64) & row[(i + 2) % 5]
        st[0] ^= RC[rnd]


def keccak(data):
    st = [0] * 25
    rate = 136
    i = 0
    while len(data) - i >= rate:
        for w in range(17):
            st[w] ^= int.from_bytes(data[i + 8 * w:i + 8 * w + 8], 'little')
        keccakf(st)
        i += rate
    temp = bytearray(data[i:]) + b'\x01' + bytes(rate - (len(data) - i) - 1)
    temp[rate - 1] |= 0x80
    for w in range(17):
        st[w] ^= int.from_bytes(temp[8 * w:8 * w + 8], 'little')
    keccakf(st)
    return b''.join(x.to_bytes(8, 'little') for x in st)[:32]


assert keccak(b'').hex() == 'c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470'


def varint(n):
    out = bytearray()
    while n >= 0x80:
        out.append((n & 0x7f) | 0x80)
        n >>= 7
    out.append(n)
    return bytes(out)


class R:
    def __init__(self, b):
        self.b = b
        self.p = 0

    def raw(self, n):
        if self.p + n > len(self.b):
            raise ValueError('eof')
        v = self.b[self.p:self.p + n]
        self.p += n
        return v

    def u8(self):
        return self.raw(1)[0]

    def vi(self, bits=64):
        v = 0
        shift = 0
        while True:
            piece = self.u8()
            if shift >= bits - 7 and piece >= (1 << (bits - shift)):
                raise ValueError('varint overflow')
            v |= (piece & 0x7f) << shift
            if piece & 0x80 == 0:
                if piece == 0 and shift != 0:
                    raise ValueError('noncanonical')
                return v
            shift += 7


def parse_prefix(r, base=False):
    tx = {}
    tx['version'] = r.vi(8)
    if not base and tx['version'] > 1:
        raise ValueError('version')
    tx['unlock'] = r.vi()
    n = r.vi()
    ins = []
    for _ in range(n):
        tag = r.u8()
        if tag == 0xff:
            ins.append(('gen', r.vi(32)))
        elif tag == 0x02:
            amount = r.vi()
            k = r.vi()
            offs = [r.vi(32) for _ in range(k)]
            ki = r.raw(32)
            ins.append(('key', amount, offs, ki))
        else:
            raise ValueError('in tag')
    tx['ins'] = ins
    n = r.vi()
    outs = []
    for _ in range(n):
        amount = r.vi()
        tag = r.u8()
        if tag != 2:
            raise ValueError('out tag')
        outs.append((amount, r.raw(32)))
    tx['outs'] = outs
    n = r.vi()
    tx['extra'] = r.raw(n)
    if base and tx['version'] >= 2:
        r.vi()
    return tx


def parse_tx(r):
    start = r.p
    tx = parse_prefix(r)
    tx['prefix_bytes'] = r.b[start:r.p]
    ins = tx['ins']
    sigs = []
    coinbase_only = len(ins) == 1 and ins[0][0] == 'gen'
    if not coinbase_only:
        for i in ins:
            cnt = 0 if i[0] == 'gen' else len(i[2])
            sigs.append([r.raw(64) for _ in range(cnt)])
    tx['sigs'] = sigs
    tx['bytes'] = r.b[start:r.p]
    return tx


def tree_hash(hs):
    n = len(hs)
    if n == 1:
        return hs[0]
    if n == 2:
        return keccak(hs[0] + hs[1])
    cnt = n - 1
    i = 1
    while i < 64:
        cnt |= cnt >> i
        i <<= 1
    cnt &= ~(cnt >> 1)
    ints = list(hs[:2 * cnt - n]) + [None] * (cnt - (2 * cnt - n))
    i = 2 * cnt - n
    j = 2 * cnt - n
    while j < cnt:
        ints[j] = keccak(hs[i] + hs[i + 1])
        i += 2
        j += 1
    while cnt > 2:
        cnt >>= 1
        for j in range(cnt):
            ints[j] = keccak(ints[2 * j] + ints[2 * j + 1])
    return keccak(ints[0] + ints[1])


def tree_depth(c):
    d = 0
    i = 32
    while i > 0:
        if c >> i > 0:
            c >>= i
            d += i
        i >>= 1
    return d


def tree_hash_from_branch(branch, leaf, path=None):
    depth = len(branch)
    if depth == 0:
        return leaf
    buf = [None, None]
    from_leaf = True
    while depth > 0:
        depth -= 1
        bit = path is not None and (path[depth >> 3] & (1 << (depth & 7))) != 0
        li, bi = (1, 0) if bit else (0, 1)
        if from_leaf:
            buf[li] = leaf
            from_leaf = False
        else:
            buf[li] = keccak(buf[0] + buf[1])
        buf[bi] = branch[depth]
    return keccak(buf[0] + buf[1])


def mm_depth(extra):
    # first merge mining tag in extra (C++ parseTransactionExtra semantics, simplified: fixtures)
    p = 0
    seen = set()
    while p < len(extra):
        t = extra[p]
        p += 1
        if t == 0:
            return None
        if t == 1:
            if 1 in seen: return None
            seen.add(1); p += 32
        elif t == 2:
            if 2 in seen: return None
            seen.add(2); n = extra[p]; p += 1 + n
        elif t == 3:
            r = R(extra[p:])
            n = r.vi()
            body = R(r.raw(n))
            depth = body.vi()
            root = body.raw(32)
            return depth, root
    return None


def parse_block(b):
    r = R(b)
    blk = {}
    major = r.vi(8)
    if major > 5 or major == 0:
        raise ValueError('major')
    minor = r.vi(8)
    blk['major'], blk['minor'] = major, minor
    if major == 1:
        blk['timestamp'] = r.vi()
        blk['prev'] = r.raw(32)
        blk['nonce'] = int.from_bytes(r.raw(4), 'little')
    else:
        blk['prev'] = r.raw(32)
        pb = {}
        pb['major'] = r.vi(8)
        pb['minor'] = r.vi(8)
        blk['timestamp'] = r.vi()
        pb['prev'] = r.raw(32)
        blk['nonce'] = int.from_bytes(r.raw(4), 'little')
        pb['txcount'] = r.vi() & 0xffff
        pb['branch'] = [r.raw(32) for _ in range(tree_depth(pb['txcount']))]
        s = r.p
        pb['minertx'] = parse_prefix(r, base=True)
        pb['minertx_bytes'] = r.b[s:r.p]
        d, root = mm_depth(pb['minertx']['extra'])
        pb['mm_root'] = root
        pb['bc_branch'] = [r.raw(32) for _ in range(d)]
        blk['parent'] = pb
    blk['minertx'] = parse_tx(r)
    n = r.vi()
    blk['txhashes'] = [r.raw(32) for _ in range(n)]
    if r.p != len(b):
        raise ValueError('trailing')
    return blk


def header_blob(blk):
    out = varint(blk['major']) + varint(blk['minor'])
    if blk['major'] == 1:
        out += varint(blk['timestamp']) + blk['prev'] + blk['nonce'].to_bytes(4, 'little')
    else:
        out += blk['prev']
    return out


def hashing_blob(blk):
    hs = [keccak(blk['minertx']['bytes'])] + blk['txhashes']
    return header_blob(blk) + tree_hash(hs) + varint(len(blk['txhashes']) + 1)


def parent_blob(blk, hashing, header_only):
    pb = blk['parent']
    out = varint(pb['major']) + varint(pb['minor']) + varint(blk['timestamp']) + pb['prev'] + blk['nonce'].to_bytes(4, 'little')
    if hashing:
        assert pb['minertx']['version'] < 2
        mh = keccak(pb['minertx_bytes'])
        out += tree_hash_from_branch(pb['branch'], mh)
    out += varint(pb['txcount'])
    if header_only:
        return out
    out += b''.join(pb['branch']) + pb['minertx_bytes'] + b''.join(pb['bc_branch'])
    return out


def block_hash(blk):
    b = hashing_blob(blk)
    if blk['major'] >= 2:
        b += parent_blob(blk, True, False)
    return keccak(varint(len(b)) + b)


def pow_blob(blk):
    if blk['major'] == 1:
        return hashing_blob(blk)
    return parent_blob(blk, True, True)


if __name__ == '__main__':
    blocks = json.load(open(sys.argv[1]))
    for f in blocks:
        b = bytes.fromhex(f['blob'])
        blk = parse_block(b)
        h = block_hash(blk).hex()
        mt = keccak(blk['minertx']['bytes']).hex()
        ok_txs = all(keccak(bytes.fromhex(t['blob'])).hex() == t['hash'] for t in f['txs'])
        hashes_ok = [x.hex() for x in blk['txhashes']] == [t['hash'] for t in f['txs']]
        cum = len(blk['minertx']['bytes']) + sum(len(t['blob']) // 2 for t in f['txs'])
        print(f['height'], blk['major'], h == f['hash'], mt == f['miner_tx_hash'], ok_txs, hashes_ok,
              cum == f['cumulative_size'], len(f['txs']), f['num_txes'] - 1)
