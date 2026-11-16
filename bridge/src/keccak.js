// Keccak-256 with the original Keccak padding (0x01 ... 0x80), i.e. CryptoNote's `cn_fast_hash`
// (not SHA3-256, which pads with 0x06). Used for the Hugin topic (mid.js). Small inputs
// only (a topic hashes 43 bytes), so 64-bit lanes are plain BigInts: no dependency, runs on
// Node and Bare.

const MASK = (1n << 64n) - 1n
const RATE = 136 // bytes, 1088-bit rate of Keccak-256

const RC = [
  0x0000000000000001n, 0x0000000000008082n, 0x800000000000808an, 0x8000000080008000n,
  0x000000000000808bn, 0x0000000080000001n, 0x8000000080008081n, 0x8000000000008009n,
  0x000000000000008an, 0x0000000000000088n, 0x0000000080008009n, 0x000000008000000an,
  0x000000008000808bn, 0x800000000000008bn, 0x8000000000008089n, 0x8000000000008003n,
  0x8000000000008002n, 0x8000000000000080n, 0x000000000000800an, 0x800000008000000an,
  0x8000000080008081n, 0x8000000000008080n, 0x0000000080000001n, 0x8000000080008008n
]

// rho offsets, lane index x + 5y
const ROT = [0, 1, 62, 28, 27, 36, 44, 6, 55, 20, 3, 10, 43, 25, 39, 41, 45, 15, 21, 8, 18, 2, 61, 56, 14]

function rotl(v, n) {
  if (n === 0) return v
  return ((v << BigInt(n)) | (v >> BigInt(64 - n))) & MASK
}

function keccakF1600(s) {
  const c = new Array(5)
  const b = new Array(25)
  for (let round = 0; round < 24; round++) {
    // theta
    for (let x = 0; x < 5; x++) c[x] = s[x] ^ s[x + 5] ^ s[x + 10] ^ s[x + 15] ^ s[x + 20]
    for (let x = 0; x < 5; x++) {
      const d = c[(x + 4) % 5] ^ rotl(c[(x + 1) % 5], 1)
      for (let y = 0; y < 25; y += 5) s[x + y] ^= d
    }
    // rho + pi: B[y, 2x + 3y] = rot(A[x, y])
    for (let x = 0; x < 5; x++) {
      for (let y = 0; y < 5; y++) {
        b[y + 5 * ((2 * x + 3 * y) % 5)] = rotl(s[x + 5 * y], ROT[x + 5 * y])
      }
    }
    // chi
    for (let y = 0; y < 25; y += 5) {
      for (let x = 0; x < 5; x++) {
        s[x + y] = b[x + y] ^ (~b[((x + 1) % 5) + y] & b[((x + 2) % 5) + y])
      }
    }
    // iota
    s[0] ^= RC[round]
  }
}

function readLane(buf, off) {
  let v = 0n
  for (let j = 7; j >= 0; j--) v = (v << 8n) | BigInt(buf[off + j])
  return v
}

/** `cn_fast_hash(data)`: Keccak-256, original padding. Returns a 32-byte Uint8Array. */
export function keccak256(data) {
  const blocks = Math.floor(data.byteLength / RATE) + 1
  const padded = new Uint8Array(blocks * RATE)
  padded.set(data)
  padded[data.byteLength] ^= 0x01
  padded[padded.length - 1] ^= 0x80
  const s = new Array(25).fill(0n)
  for (let off = 0; off < padded.length; off += RATE) {
    for (let i = 0; i < RATE / 8; i++) s[i] ^= readLane(padded, off + 8 * i)
    keccakF1600(s)
  }
  const out = new Uint8Array(32)
  for (let i = 0; i < 4; i++) {
    for (let j = 0; j < 8; j++) out[8 * i + j] = Number((s[i] >> BigInt(8 * j)) & 0xffn)
  }
  return out
}
