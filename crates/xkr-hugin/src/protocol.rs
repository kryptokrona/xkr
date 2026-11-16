//! The wire protocol between Hugin apps, hugin-node and us: bare-rpc frames carrying JSON.
//!
//! hugin-node, hugin-desktop and hugin-native wrap every Hyperswarm connection in a
//! [bare-rpc](https://github.com/holepunchto/bare-rpc) `RPC` and only ever use one command,
//! `PACKET = 1`: each side sends `rpc.request(1).send(JSON.stringify(packet))` and answers every
//! request it receives with `req.reply(JSON.stringify({ ok: true, data: null, error: null }))`.
//! Replies carry no information and are ignored; real answers travel as new requests. See
//! `docs/hugin-relay.md` §2.
//!
//! # bare-rpc framing (`bare-rpc/lib/messages.js`)
//!
//! ```text
//! frame    = len u32-LE (bytes after this field) | type uint | id uint | body
//! REQUEST  (1): command uint | stream uint | [stream == 0: data buffer]
//! RESPONSE (2): error bool | stream uint | error ? (message utf8, code utf8, errno int)
//!                                         : [stream == 0: data buffer]
//! STREAM   (3): stream uint | stream & ERROR ? error : [stream & DATA: data buffer]
//! ```
//!
//! `uint` is compact-encoding's variable-length integer (one byte up to 0xfc, else 0xfd + u16,
//! 0xfe + u32, 0xff + u64, little-endian), `buffer`/`utf8` are a `uint` length plus the bytes,
//! `int` is a zig-zag `uint`, `bool` one byte (1 = true). `id` 0 marks an event (no reply).
//! Byte-exact against frames captured from bare-rpc 1.3.3 (`docs/test-vectors/hugin-relay.json`,
//! produced by `bridge/test/hugin-vectors.js`).

use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde_json::{Value, json};
use tokio_util::codec::{Decoder, Encoder};

/// The only bare-rpc command Hugin uses.
pub const PACKET: u64 = 1;

/// bare-rpc message types.
pub mod msg_type {
    pub const REQUEST: u64 = 1;
    pub const RESPONSE: u64 = 2;
    pub const STREAM: u64 = 3;
}

/// bare-rpc stream flags (only `DATA` and `ERROR` matter for decoding).
pub mod stream_flag {
    pub const OPEN: u64 = 0x1;
    pub const CLOSE: u64 = 0x2;
    pub const PAUSE: u64 = 0x4;
    pub const RESUME: u64 = 0x8;
    pub const DATA: u64 = 0x10;
    pub const END: u64 = 0x20;
    pub const DESTROY: u64 = 0x40;
    pub const ERROR: u64 = 0x80;
    pub const REQUEST: u64 = 0x100;
    pub const RESPONSE: u64 = 0x200;
}

/// The JSON every Hugin peer answers each request with.
pub const REPLY_OK: &str = r#"{"ok":true,"data":null,"error":null}"#;

/// An error carried in a `RESPONSE` or `STREAM` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    pub message: String,
    pub code: String,
    pub errno: i64,
}

/// One bare-rpc message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcMessage {
    /// A request (`id > 0`) or an event (`id == 0`). `data` is present iff `stream == 0`.
    Request {
        id: u64,
        command: u64,
        stream: u64,
        data: Option<Bytes>,
    },
    /// A reply. `Ok(Some(data))` iff no error and `stream == 0`.
    Response {
        id: u64,
        stream: u64,
        result: Result<Option<Bytes>, RpcError>,
    },
    /// Stream control/data of a streaming request or response (unused by Hugin).
    Stream {
        id: u64,
        stream: u64,
        error: Option<RpcError>,
        data: Option<Bytes>,
    },
}

impl RpcMessage {
    /// `rpc.request(PACKET).send(json)`.
    #[must_use]
    pub fn packet(id: u64, json: impl Into<Bytes>) -> Self {
        Self::Request {
            id,
            command: PACKET,
            stream: 0,
            data: Some(json.into()),
        }
    }

    /// `req.reply('{"ok":true,"data":null,"error":null}')` for request `id`.
    #[must_use]
    pub fn reply_ok(id: u64) -> Self {
        Self::Response {
            id,
            stream: 0,
            result: Ok(Some(Bytes::from_static(REPLY_OK.as_bytes()))),
        }
    }
}

/// Codec errors. Any of them ends the connection (bare-rpc destroys the stream).
#[derive(Debug, thiserror::Error)]
pub enum RpcCodecError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("bare-rpc frame of {0} bytes exceeds the limit")]
    FrameTooLarge(usize),
    #[error("malformed bare-rpc message: {0}")]
    Malformed(&'static str),
    #[error("unknown bare-rpc message type {0}")]
    UnknownType(u64),
}

// ---------------------------------------------------------------------------------------------
// compact-encoding primitives

/// Encoded size of a compact `uint`.
#[must_use]
pub fn uint_len(n: u64) -> usize {
    if n <= 0xfc {
        1
    } else if n <= 0xffff {
        3
    } else if n <= 0xffff_ffff {
        5
    } else {
        9
    }
}

/// compact-encoding `uint`.
pub fn put_uint(buf: &mut impl BufMut, n: u64) {
    if n <= 0xfc {
        buf.put_u8(n as u8);
    } else if n <= 0xffff {
        buf.put_u8(0xfd);
        buf.put_u16_le(n as u16);
    } else if n <= 0xffff_ffff {
        buf.put_u8(0xfe);
        buf.put_u32_le(n as u32);
    } else {
        buf.put_u8(0xff);
        buf.put_u64_le(n);
    }
}

/// A cursor over one frame body; every read is bounds-checked.
struct Reader<'a> {
    buf: &'a [u8],
}

impl Reader<'_> {
    fn u8(&mut self) -> Result<u8, RpcCodecError> {
        let (&b, rest) = self
            .buf
            .split_first()
            .ok_or(RpcCodecError::Malformed("out of bounds"))?;
        self.buf = rest;
        Ok(b)
    }

    fn take(&mut self, n: usize) -> Result<&[u8], RpcCodecError> {
        if self.buf.len() < n {
            return Err(RpcCodecError::Malformed("out of bounds"));
        }
        let (a, b) = self.buf.split_at(n);
        self.buf = b;
        Ok(a)
    }

    fn uint(&mut self) -> Result<u64, RpcCodecError> {
        let a = self.u8()?;
        Ok(match a {
            0xfd => u64::from(u16::from_le_bytes(
                self.take(2)?.try_into().unwrap_or_default(),
            )),
            0xfe => u64::from(u32::from_le_bytes(
                self.take(4)?.try_into().unwrap_or_default(),
            )),
            0xff => u64::from_le_bytes(self.take(8)?.try_into().unwrap_or_default()),
            small => u64::from(small),
        })
    }

    fn int(&mut self) -> Result<i64, RpcCodecError> {
        let n = self.uint()?;
        Ok(((n >> 1) as i64) ^ -((n & 1) as i64))
    }

    fn bool(&mut self) -> Result<bool, RpcCodecError> {
        Ok(self.u8()? == 1)
    }

    fn buffer(&mut self) -> Result<Bytes, RpcCodecError> {
        let len = usize::try_from(self.uint()?)
            .map_err(|_| RpcCodecError::Malformed("length overflow"))?;
        Ok(Bytes::copy_from_slice(self.take(len)?))
    }

    fn utf8(&mut self) -> Result<String, RpcCodecError> {
        let b = self.buffer()?;
        // TextDecoder in bare-rpc replaces invalid sequences; do the same.
        Ok(String::from_utf8_lossy(&b).into_owned())
    }

    fn error(&mut self) -> Result<RpcError, RpcCodecError> {
        Ok(RpcError {
            message: self.utf8()?,
            code: self.utf8()?,
            errno: self.int()?,
        })
    }
}

fn put_buffer(buf: &mut BytesMut, b: &[u8]) {
    put_uint(buf, b.len() as u64);
    buf.put_slice(b);
}

fn put_error(buf: &mut BytesMut, e: &RpcError) {
    put_buffer(buf, e.message.as_bytes());
    put_buffer(buf, e.code.as_bytes());
    let z = ((e.errno << 1) ^ (e.errno >> 63)) as u64;
    put_uint(buf, z);
}

/// Decodes the body of one frame (`frame` excludes the 4-byte length). Trailing bytes inside
/// the frame are ignored, as in bare-rpc.
pub fn decode_message(frame: &[u8]) -> Result<RpcMessage, RpcCodecError> {
    let mut r = Reader { buf: frame };
    let ty = r.uint()?;
    let id = r.uint()?;
    match ty {
        msg_type::REQUEST => {
            let command = r.uint()?;
            let stream = r.uint()?;
            let data = if stream == 0 { Some(r.buffer()?) } else { None };
            Ok(RpcMessage::Request {
                id,
                command,
                stream,
                data,
            })
        }
        msg_type::RESPONSE => {
            let err = r.bool()?;
            let stream = r.uint()?;
            let result = if err {
                Err(r.error()?)
            } else if stream == 0 {
                Ok(Some(r.buffer()?))
            } else {
                Ok(None)
            };
            Ok(RpcMessage::Response { id, stream, result })
        }
        msg_type::STREAM => {
            let stream = r.uint()?;
            let (error, data) = if stream & stream_flag::ERROR != 0 {
                (Some(r.error()?), None)
            } else if stream & stream_flag::DATA != 0 {
                (None, Some(r.buffer()?))
            } else {
                (None, None)
            };
            Ok(RpcMessage::Stream {
                id,
                stream,
                error,
                data,
            })
        }
        other => Err(RpcCodecError::UnknownType(other)),
    }
}

/// Appends one complete frame (length included) for `msg` to `out`.
pub fn encode_message(msg: &RpcMessage, out: &mut BytesMut) {
    let mut body = BytesMut::with_capacity(64);
    match msg {
        RpcMessage::Request {
            id,
            command,
            stream,
            data,
        } => {
            put_uint(&mut body, msg_type::REQUEST);
            put_uint(&mut body, *id);
            put_uint(&mut body, *command);
            put_uint(&mut body, *stream);
            if *stream == 0 {
                put_buffer(&mut body, data.as_deref().unwrap_or_default());
            }
        }
        RpcMessage::Response { id, stream, result } => {
            put_uint(&mut body, msg_type::RESPONSE);
            put_uint(&mut body, *id);
            body.put_u8(u8::from(result.is_err()));
            put_uint(&mut body, *stream);
            match result {
                Err(e) => put_error(&mut body, e),
                Ok(data) if *stream == 0 => {
                    put_buffer(&mut body, data.as_deref().unwrap_or_default());
                }
                Ok(_) => {}
            }
        }
        RpcMessage::Stream {
            id,
            stream,
            error,
            data,
        } => {
            put_uint(&mut body, msg_type::STREAM);
            put_uint(&mut body, *id);
            put_uint(&mut body, *stream);
            if stream & stream_flag::ERROR != 0 {
                put_error(
                    &mut body,
                    error.as_ref().unwrap_or(&RpcError {
                        message: String::new(),
                        code: String::new(),
                        errno: 0,
                    }),
                );
            } else if stream & stream_flag::DATA != 0 {
                put_buffer(&mut body, data.as_deref().unwrap_or_default());
            }
        }
    }
    out.reserve(4 + body.len());
    out.put_u32_le(body.len() as u32);
    out.put_slice(&body);
}

/// The start of the frame [`encode_message`] writes for `RpcMessage::packet(id, payload)`, up to
/// and including the payload's length: the payload itself follows as it is, so a large packet
/// can be written in pieces without copying it.
#[must_use]
pub fn packet_header(id: u64, payload_len: usize) -> BytesMut {
    let len = payload_len as u64;
    let fields = uint_len(msg_type::REQUEST) + uint_len(id) + uint_len(PACKET) + uint_len(0);
    let body = fields + uint_len(len) + payload_len;
    let mut out = BytesMut::with_capacity(4 + 16);
    out.put_u32_le(body as u32);
    put_uint(&mut out, msg_type::REQUEST);
    put_uint(&mut out, id);
    put_uint(&mut out, PACKET);
    put_uint(&mut out, 0);
    put_uint(&mut out, len);
    out
}

/// `tokio_util` codec for bare-rpc frames with a size limit (`len + 4 <= max_frame`).
#[derive(Debug, Clone)]
pub struct BareRpcCodec {
    max_frame: usize,
}

impl BareRpcCodec {
    #[must_use]
    pub fn new(max_frame: usize) -> Self {
        Self {
            max_frame: max_frame.max(16),
        }
    }

    pub fn set_max_frame(&mut self, max_frame: usize) {
        self.max_frame = max_frame.max(16);
    }
}

impl Decoder for BareRpcCodec {
    type Item = RpcMessage;
    type Error = RpcCodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<RpcMessage>, RpcCodecError> {
        if src.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([src[0], src[1], src[2], src[3]]) as usize;
        let total = len.saturating_add(4);
        if total > self.max_frame {
            return Err(RpcCodecError::FrameTooLarge(total));
        }
        if src.len() < total {
            src.reserve(total - src.len());
            return Ok(None);
        }
        src.advance(4);
        let frame = src.split_to(len);
        decode_message(&frame).map(Some)
    }
}

impl Encoder<RpcMessage> for BareRpcCodec {
    type Error = RpcCodecError;

    fn encode(&mut self, item: RpcMessage, dst: &mut BytesMut) -> Result<(), RpcCodecError> {
        encode_message(&item, dst);
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// JSON packets (shapes from hugin-node `network.js`/`nodes.js`)

/// Version string hugin-node 2.0.0 sends; we report the same protocol level.
pub const HUGIN_NODE_VERSION: &str = "2.0.0";

/// Node → client on connect: `{ address, version }` (the node's XKR address, 99 characters).
#[must_use]
pub fn client_hello(address: &str) -> Value {
    json!({ "address": address, "version": HUGIN_NODE_VERSION })
}

/// Node → node on connect: `{ version }`, plus our `xkrd` capability object (ignored by
/// hugin-node, which only looks at `signal` and `push` in node packets).
#[must_use]
pub fn node_hello(xkrd_version: &str) -> Value {
    json!({
        "version": HUGIN_NODE_VERSION,
        "xkrd": { "version": xkrd_version, "sync": XKRD_SYNC_VERSION },
    })
}

/// Version of our node-to-node sync extension (`xkrd_sync_request` / `xkrd_sync`).
pub const XKRD_SYNC_VERSION: u64 = 1;

/// `{ success: true, id }` or with `duplicate: true`.
#[must_use]
pub fn success(id: &Value, duplicate: bool) -> Value {
    if duplicate {
        json!({ "success": true, "duplicate": true, "id": id })
    } else {
        json!({ "success": true, "id": id })
    }
}

/// `{ success: false, reason, id }`.
#[must_use]
pub fn failure(reason: &str, id: &Value) -> Value {
    json!({ "success": false, "reason": reason, "id": id })
}

/// Rejected by the pool: `{ reason: 'pool_reject', success: false, id, rejects }`.
#[must_use]
pub fn pool_reject(id: &Value, rejects: &[(String, Option<String>)]) -> Value {
    let rejects: Vec<Value> = rejects
        .iter()
        .map(|(reason, error)| json!({ "reason": reason, "error": error }))
        .collect();
    json!({ "reason": "pool_reject", "success": false, "id": id, "rejects": rejects })
}

/// A pool job for a client: `{ type: 'job', job }`, with `id` when it answers a request.
#[must_use]
pub fn job(job: &Value, id: Option<&Value>) -> Value {
    match id {
        Some(id) => json!({ "type": "job", "job": job, "id": id }),
        None => json!({ "type": "job", "job": job }),
    }
}

/// `{ type: 'job_pending', id }`.
#[must_use]
pub fn job_pending(id: &Value) -> Value {
    json!({ "type": "job_pending", "id": id })
}

/// Answer to `{ request: true, type: 'some', timestamp }`: `{ response: [...], id }`.
#[must_use]
pub fn some_response(messages: &[Value], id: &Value) -> Value {
    json!({ "response": messages, "id": id })
}

/// `"[a,b,...]"` from already serialized JSON values.
fn raw_array(out: &mut Vec<u8>, items: &[Bytes]) {
    out.push(b'[');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(item);
    }
    out.push(b']');
}

/// [`some_response`] from serialized messages: `{"id":...,"response":[...]}`.
#[must_use]
pub fn some_response_raw(messages: &[Bytes], id: &Value) -> Bytes {
    let len: usize = messages.iter().map(|m| m.len() + 1).sum();
    let mut out = Vec::with_capacity(len + 64);
    out.extend_from_slice(b"{\"id\":");
    out.extend_from_slice(&serde_json::to_vec(id).unwrap_or_else(|_| b"null".to_vec()));
    out.extend_from_slice(b",\"response\":");
    raw_array(&mut out, messages);
    out.push(b'}');
    Bytes::from(out)
}

/// [`sync_batch`] from serialized messages:
/// `{"done":...,"id":...,"messages":[...],"type":"xkrd_sync"}`.
#[must_use]
pub fn sync_batch_raw(id: &Value, messages: &[Bytes], done: bool) -> Bytes {
    let len: usize = messages.iter().map(|m| m.len() + 1).sum();
    let mut out = Vec::with_capacity(len + 96);
    out.extend_from_slice(if done {
        b"{\"done\":true,\"id\":".as_slice()
    } else {
        b"{\"done\":false,\"id\":".as_slice()
    });
    out.extend_from_slice(&serde_json::to_vec(id).unwrap_or_else(|_| b"null".to_vec()));
    out.extend_from_slice(b",\"messages\":");
    raw_array(&mut out, messages);
    out.extend_from_slice(b",\"type\":\"xkrd_sync\"}");
    Bytes::from(out)
}

/// Pushed to clients every few seconds: `{ type: 'new-message', messages }`.
#[must_use]
pub fn new_messages(messages: &[Value]) -> Value {
    json!({ "type": "new-message", "messages": messages })
}

/// Node gossip of an accepted message: `{ signal: true, message }`.
#[must_use]
pub fn signal(message: &Value) -> Value {
    json!({ "signal": true, "message": message })
}

/// Forward to the push-notification cluster: `{ push: true, message }` (hugin-nodes ignore it).
#[must_use]
pub fn push(message: &Value) -> Value {
    json!({ "push": true, "message": message })
}

/// xkrd extension: ask an xkrd peer for its pool since `since` (ms).
#[must_use]
pub fn sync_request(id: u64, since: i64) -> Value {
    json!({ "type": "xkrd_sync_request", "id": id, "since": since })
}

/// xkrd extension: one batch of a sync answer.
#[must_use]
pub fn sync_batch(id: &Value, messages: &[Value], done: bool) -> Value {
    json!({ "type": "xkrd_sync", "id": id, "messages": messages, "done": done })
}

#[cfg(test)]
mod tests {
    use super::*;

    const VECTORS: &str = include_str!("../../../docs/test-vectors/hugin-relay.json");

    fn rpc_vectors() -> Vec<Value> {
        let v: Value = serde_json::from_str(VECTORS).unwrap();
        v["rpc"].as_array().unwrap().clone()
    }

    fn vector(name: &str) -> Value {
        rpc_vectors()
            .into_iter()
            .find(|v| v["name"] == name)
            .unwrap_or_else(|| panic!("vector {name}"))
    }

    fn enc(msg: &RpcMessage) -> String {
        let mut b = BytesMut::new();
        encode_message(msg, &mut b);
        hex::encode(&b)
    }

    fn dec(hex_frame: &str) -> RpcMessage {
        let bytes = hex::decode(hex_frame).unwrap();
        let mut src = BytesMut::from(&bytes[..]);
        let msg = BareRpcCodec::new(1 << 20)
            .decode(&mut src)
            .unwrap()
            .unwrap();
        assert!(src.is_empty());
        msg
    }

    /// The header plus the payload is the frame `encode_message` writes.
    #[test]
    fn packet_header_matches_the_full_frame() {
        for (id, len) in [(1u64, 0usize), (300, 20), (70_000, 300), (5, 70_000)] {
            let payload = Bytes::from(vec![b'x'; len]);
            let mut whole = BytesMut::new();
            encode_message(&RpcMessage::packet(id, payload.clone()), &mut whole);
            let mut split = packet_header(id, len);
            split.extend_from_slice(&payload);
            assert_eq!(split, whole, "id {id}, {len} bytes");
        }
    }

    #[test]
    fn requests_match_bare_rpc() {
        for name in [
            "request_version",
            "request_len_3_byte",
            "request_id_3_byte",
            "request_id_5_byte",
            "event",
        ] {
            let v = vector(name);
            let msg = RpcMessage::packet(
                v["id"].as_u64().unwrap(),
                v["data"].as_str().unwrap().to_owned(),
            );
            assert_eq!(enc(&msg), v["hex"].as_str().unwrap(), "{name}");
            assert_eq!(dec(v["hex"].as_str().unwrap()), msg, "{name}");
        }
    }

    #[test]
    fn replies_match_bare_rpc() {
        let v = vector("response_ok");
        let ok = RpcMessage::reply_ok(5);
        assert_eq!(enc(&ok), v["hex"].as_str().unwrap());
        assert_eq!(dec(v["hex"].as_str().unwrap()), ok);
        // the request it answered
        assert_eq!(
            dec(v["request_hex"].as_str().unwrap()),
            RpcMessage::packet(5, r#"{"request":true,"type":"some","timestamp":0}"#)
        );

        let v = vector("response_error");
        let err = RpcMessage::Response {
            id: 7,
            stream: 0,
            result: Err(RpcError {
                message: "boom".into(),
                code: "E_BOOM".into(),
                errno: -3,
            }),
        };
        assert_eq!(enc(&err), v["hex"].as_str().unwrap());
        assert_eq!(dec(v["hex"].as_str().unwrap()), err);
    }

    #[test]
    fn split_and_coalesced_frames() {
        let v = vector("two_requests_split");
        let bytes = hex::decode(v["hex"].as_str().unwrap()).unwrap();
        let mut codec = BareRpcCodec::new(1 << 20);
        let mut src = BytesMut::new();
        let mut out = Vec::new();
        for chunk in [&bytes[..3], &bytes[3..20], &bytes[20..]] {
            src.extend_from_slice(chunk);
            while let Some(m) = codec.decode(&mut src).unwrap() {
                out.push(m);
            }
        }
        assert_eq!(
            out,
            vec![
                RpcMessage::packet(8, r#"{"x":1}"#),
                RpcMessage::packet(9, r#"{"x":2}"#)
            ]
        );
    }

    #[test]
    fn limits_and_garbage() {
        let v = vector("request_len_3_byte");
        let bytes = hex::decode(v["hex"].as_str().unwrap()).unwrap();
        let mut src = BytesMut::from(&bytes[..]);
        assert!(matches!(
            BareRpcCodec::new(100).decode(&mut src),
            Err(RpcCodecError::FrameTooLarge(_))
        ));
        // unknown type 9
        let mut src = BytesMut::from(&[2u8, 0, 0, 0, 9, 1][..]);
        assert!(matches!(
            BareRpcCodec::new(100).decode(&mut src),
            Err(RpcCodecError::UnknownType(9))
        ));
        // data length beyond the frame
        let mut src = BytesMut::from(&[5u8, 0, 0, 0, 1, 1, 1, 0, 9][..]);
        assert!(matches!(
            BareRpcCodec::new(100).decode(&mut src),
            Err(RpcCodecError::Malformed(_))
        ));
    }

    #[test]
    fn compact_uints_round_trip() {
        for n in [
            0u64,
            0xfc,
            0xfd,
            0xffff,
            0x1_0000,
            0xffff_ffff,
            0x1_0000_0000,
            u64::MAX,
        ] {
            let mut b = BytesMut::new();
            put_uint(&mut b, n);
            assert_eq!(b.len(), uint_len(n));
            let mut r = Reader { buf: &b };
            assert_eq!(r.uint().unwrap(), n);
        }
        for e in [0i64, -1, 1, -3, i64::MIN, i64::MAX] {
            let mut b = BytesMut::new();
            put_error(
                &mut b,
                &RpcError {
                    message: String::new(),
                    code: String::new(),
                    errno: e,
                },
            );
            let mut r = Reader { buf: &b };
            assert_eq!(r.error().unwrap().errno, e);
        }
    }

    #[test]
    fn packet_shapes() {
        let j = |s: &str| serde_json::from_str::<Value>(s).unwrap();
        assert_eq!(
            client_hello("SEKRx"),
            j(r#"{"address":"SEKRx","version":"2.0.0"}"#)
        );
        assert_eq!(
            success(&json!(5), true),
            j(r#"{"success":true,"duplicate":true,"id":5}"#)
        );
        let msgs = [
            Bytes::from_static(br#"{"hash":"a"}"#),
            Bytes::from_static(br#"{"hash":"b"}"#),
        ];
        let raw: Value = serde_json::from_slice(&some_response_raw(&msgs, &json!(7))).unwrap();
        assert_eq!(
            raw,
            some_response(&[json!({"hash": "a"}), json!({"hash": "b"})], &json!(7))
        );
        let raw: Value = serde_json::from_slice(&some_response_raw(&[], &json!("x"))).unwrap();
        assert_eq!(raw, some_response(&[], &json!("x")));
        for done in [true, false] {
            let raw: Value =
                serde_json::from_slice(&sync_batch_raw(&json!(5), &msgs, done)).unwrap();
            assert_eq!(
                raw,
                sync_batch(
                    &json!(5),
                    &[json!({"hash": "a"}), json!({"hash": "b"})],
                    done
                )
            );
        }
        assert_eq!(
            pool_reject(&json!(1), &[("stale_job".into(), None)]),
            j(
                r#"{"reason":"pool_reject","success":false,"id":1,"rejects":[{"reason":"stale_job","error":null}]}"#
            )
        );
    }
}
