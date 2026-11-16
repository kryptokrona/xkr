// Hand-written frames for the shared golden vectors in
// docs/test-vectors/hyper-bridge-frames.json. crates/xkr-p2p-hyper/src/frame.rs builds the same
// frames by hand and checks them against the same hex.

import b4a from 'b4a'
import { TYPE, FLAG, TLV } from '../src/frames.js'

const fill = (n, byte) => b4a.alloc(n, byte)
const seq = (n) => b4a.from(Array.from({ length: n }, (_, i) => i))
const v4 = (a, b, c, d, port) => {
  const addr = b4a.alloc(16)
  addr.set([a, b, c, d])
  return { family: 4, port, addr }
}
const BARE = 1n << 63n

export const VECTORS = [
  {
    name: 'hello',
    frame: {
      type: TYPE.HELLO,
      body: {
        major: 1,
        minor: 0,
        maxFrame: 16777216,
        initialCredit: 1048576,
        workers: 1,
        worker: 0,
        features: 1n,
        tlvs: [{ tag: TLV.PUBLIC_KEY, value: fill(32, 0x11) }]
      }
    }
  },
  { name: 'ping', frame: { type: TYPE.PING, body: { nonce: 0x0102030405060708n } } },
  { name: 'pong', frame: { type: TYPE.PONG, body: { nonce: 0x0102030405060708n } } },
  {
    name: 'open',
    frame: {
      type: TYPE.OPEN,
      connId: 1n,
      body: {
        requestId: 7,
        remotePublicKey: fill(32, 0xaa),
        topic: fill(32, 0xbb),
        openFlags: 1,
        tlvs: [{ tag: TLV.HUGIN_TOPIC_KEY, value: fill(32, 0xcc) }]
      }
    }
  },
  {
    name: 'opened_inbound_v4',
    frame: {
      type: TYPE.OPENED,
      connId: BARE | 1n,
      body: {
        requestId: 0,
        remotePublicKey: fill(32, 0xaa),
        handshakeHash: fill(64, 0xdd),
        topic: fill(32, 0x00),
        initiator: false,
        addr: v4(192, 168, 1, 10, 49737),
        tlvs: []
      }
    }
  },
  {
    name: 'opened_outbound_v6',
    frame: {
      type: TYPE.OPENED,
      shard: 0,
      connId: 2n,
      body: {
        requestId: 8,
        remotePublicKey: fill(32, 0xaa),
        handshakeHash: fill(64, 0xdd),
        topic: fill(32, 0xbb),
        initiator: true,
        addr: {
          family: 6,
          port: 443,
          addr: b4a.from('20010db8000000000000000000000001', 'hex')
        },
        tlvs: [{ tag: TLV.PUBLIC_KEY, value: fill(32, 0x11) }]
      }
    }
  },
  {
    name: 'close',
    frame: {
      type: TYPE.CLOSE,
      connId: 1n,
      body: { reason: 3, code: 5, message: 'mid rejected' }
    }
  },
  { name: 'data', frame: { type: TYPE.DATA, connId: 1n, body: { payload: b4a.from('hello') } } },
  { name: 'data_empty', frame: { type: TYPE.DATA, connId: BARE | 2n, body: { payload: b4a.alloc(0) } } },
  { name: 'credit', frame: { type: TYPE.CREDIT, connId: 1n, body: { bytes: 65536 } } },
  {
    name: 'announce_hugin',
    frame: {
      type: TYPE.ANNOUNCE,
      body: {
        requestId: 9,
        topic: fill(32, 0xbb),
        mode: 3,
        tlvs: [
          { tag: TLV.HUGIN_MID, value: seq(64) },
          { tag: TLV.HUGIN_TOPIC_KEY, value: fill(32, 0xcc) }
        ]
      }
    }
  },
  {
    name: 'announce_plain',
    frame: { type: TYPE.ANNOUNCE, body: { requestId: 1, topic: fill(32, 0x01), mode: 1, tlvs: [] } }
  },
  {
    name: 'unannounce',
    frame: { type: TYPE.UNANNOUNCE, body: { requestId: 10, topic: fill(32, 0xbb), tlvs: [] } }
  },
  {
    name: 'lookup',
    frame: { type: TYPE.LOOKUP, body: { requestId: 11, topic: fill(32, 0xbb), maxPeers: 32, tlvs: [] } }
  },
  {
    name: 'lookup_result',
    frame: {
      type: TYPE.LOOKUP_RESULT,
      flags: FLAG.END,
      body: {
        requestId: 11,
        peers: [
          { publicKey: fill(32, 0xaa), addr: v4(1, 2, 3, 4, 5) },
          { publicKey: fill(32, 0xee), addr: { family: 0, port: 0, addr: b4a.alloc(16) } }
        ],
        tlvs: []
      }
    }
  },
  {
    name: 'ack',
    frame: {
      type: TYPE.ACK,
      body: { requestId: 9, tlvs: [{ tag: TLV.PUBLIC_KEY, value: fill(32, 0x11) }] }
    }
  },
  { name: 'peer_info_request', frame: { type: TYPE.PEER_INFO, connId: 1n, body: { request: true } } },
  {
    name: 'peer_info',
    frame: {
      type: TYPE.PEER_INFO,
      connId: BARE | 1n,
      body: {
        request: false,
        rttUs: 1500,
        bytesIn: 1000n,
        bytesOut: 2000n,
        relayed: false,
        holepunched: true,
        addr: v4(10, 0, 0, 1, 1234)
      }
    }
  },
  {
    name: 'dht_status',
    frame: {
      type: TYPE.DHT_STATUS,
      flags: FLAG.IGNORABLE,
      body: { state: 1, firewalled: true, nat: 2, udpPort: 49737, tlvs: [] }
    }
  },
  {
    name: 'error',
    frame: { type: TYPE.ERROR, body: { requestId: 9, code: 5, message: 'dht' } }
  },
  {
    name: 'unknown_ignorable',
    frame: { type: 0x55, flags: FLAG.IGNORABLE, shard: 2, body: { raw: b4a.from([1, 2, 3]) } }
  }
]
