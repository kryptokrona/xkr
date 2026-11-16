"""Python reference for the CryptoNote ed25519 pieces (ref10 semantics)."""
from ref import keccak

p = 2**255 - 19
d = (-121665 * pow(121666, p - 2, p)) % p
L = 2**252 + 27742317777372353535851937790883648493
SQRTM1 = pow(2, (p - 1) // 4, p)
A = 486662


def inv(x):
    return pow(x, p - 2, p)


# extended coordinates (X, Y, Z, T)
def padd(P, Q):
    X1, Y1, Z1, T1 = P
    X2, Y2, Z2, T2 = Q
    a = (Y1 - X1) * (Y2 - X2) % p
    b = (Y1 + X1) * (Y2 + X2) % p
    c = T1 * 2 * d * T2 % p
    dd = Z1 * 2 * Z2 % p
    e, f, g, h = b - a, dd - c, dd + c, b + a
    return (e * f % p, g * h % p, f * g % p, e * h % p)


IDENT = (0, 1, 1, 0)


def smul(P, k):
    Q = IDENT
    while k > 0:
        if k & 1:
            Q = padd(Q, P)
        P = padd(P, P)
        k >>= 1
    return Q


def encode(P):
    X, Y, Z, _ = P
    zi = inv(Z)
    x, y = X * zi % p, Y * zi % p
    return (y | ((x & 1) << 255)).to_bytes(32, 'little')


def decode(b):
    """ref10 ge_frombytes_vartime: None on failure."""
    v = int.from_bytes(b, 'little')
    sign = v >> 255
    y = v & ((1 << 255) - 1)
    if y >= p:
        return None
    u = (y * y - 1) % p
    w = (d * y * y + 1) % p
    x2 = u * inv(w) % p
    x = pow(x2, (p + 3) // 8, p)
    if (x * x - x2) % p != 0:
        x = x * SQRTM1 % p
        if (x * x - x2) % p != 0:
            return None
    if (x & 1) != sign:
        if x == 0:
            return None
        x = p - x
    return (x, y, 1, x * y % p)


Gy = 4 * inv(5) % p
G = decode(Gy.to_bytes(32, 'little'))


def is_identity(P):
    X, Y, Z, _ = P
    return X % p == 0 and (Y - Z) % p == 0


def fromfe(s):
    """Port of ge_fromfe_frombytes_vartime, returns affine point (x, y)."""
    u = int.from_bytes(s, 'little') % p
    v = 2 * u * u % p
    w = (v + 1) % p
    x = w * w % p
    y = (-A * A) * v % p
    x = (x + y) % p
    # divpowm1(w, x) = w * x^3 * (w x^7)^((p-5)/8)
    rx = w * pow(x, 3, p) * pow(w * pow(x, 7, p) % p, (p - 5) // 8, p) % p
    y = rx * rx % p
    x = y * x % p
    y = (w - x) % p
    z = (-A) % p
    fffb1 = pow(2, 0, p)  # placeholders, computed below
    negative = False
    if y != 0:
        y = (w + x) % p
        if y != 0:
            negative = True
        else:
            rx = rx * FFFB1 % p
    else:
        rx = rx * FFFB2 % p
    if negative:
        x = x * SQRTM1 % p
        y = (w - x) % p
        if y != 0:
            rx = rx * FFFB3 % p
        else:
            rx = rx * FFFB4 % p
        sign = 1
    else:
        rx = rx * u % p
        z = z * v % p
        sign = 0
    if (rx & 1) != sign:
        rx = (-rx) % p
    Z = (z + w) % p
    Y = (z - w) % p
    X = rx * Z % p
    zi = inv(Z)
    return X * zi % p, Y * zi % p


offs = [0, 26, 51, 77, 102, 128, 153, 179, 204, 230]
def limbs(l): return sum(a * (1 << o) for a, o in zip(l, offs)) % p
FFFB1 = limbs([-31702527, -2466483, -26106795, -12203692, -12169197, -321052, 14850977, -10296299, -16929438, -407568])
FFFB2 = limbs([8166131, -6741800, -17040804, 3154616, 21461005, 1466302, -30876704, -6368709, 10503587, -13363080])
FFFB3 = limbs([-13620103, 14639558, 4532995, 7679154, 16815101, -15883539, -22863840, -14813421, 13716513, -6477756])
FFFB4 = limbs([-21786234, -12173074, 21573800, 4524538, -4645904, 16204591, 8012863, -8444712, 3212926, 6885324])


def hash_to_ec(pub):
    x, y = fromfe(keccak(pub))
    P = (x, y, 1, x * y % p)
    assert (-x * x + y * y - 1 - d * x * x * y * y) % p == 0, "not on curve"
    return smul(P, 8)


def hs(data):
    return int.from_bytes(keccak(data), 'little') % L


def check_ring(prefix_hash, image, pubs, sigs):
    I = decode(image)
    if I is None or not is_identity(smul(I, L)):
        return False
    buf = bytearray(prefix_hash)
    s = 0
    for P, sig in zip(pubs, sigs):
        c = int.from_bytes(sig[:32], 'little')
        r = int.from_bytes(sig[32:], 'little')
        if c >= L or r >= L:
            return False
        Pp = decode(P)
        if Pp is None:
            return False
        La = padd(smul(Pp, c), smul(G, r))
        buf += encode(La)
        Rb = padd(smul(hash_to_ec(P), r), smul(I, c))
        buf += encode(Rb)
        s = (s + c) % L
    return (hs(bytes(buf)) - s) % L == 0
