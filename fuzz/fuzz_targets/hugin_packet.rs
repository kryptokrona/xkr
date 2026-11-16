//! Hugin relay input (`xkr-hugin`): bare-rpc frames on a split stream, the JSON inside, every
//! PoW / shape / signature check and the message pool. Never panics; bare-rpc messages
//! round-trip; the pool keeps its limits.
#![no_main]

use std::cell::RefCell;

use bytes::BytesMut;
use libfuzzer_sys::fuzz_target;
use serde_json::Value;
use tokio_util::codec::Decoder;
use xkr_hugin::pow::{self, PowJob, PrevIds};
use xkr_hugin::protocol::{BareRpcCodec, RpcMessage, decode_message, encode_message};
use xkr_hugin::relay::{HUGIN_INBOUND_LIMIT, XKRD_INBOUND_LIMIT};
use xkr_hugin::store::{MessagePool, ONE_DAY_MS, StoreLimits};

const NOW_MS: f64 = 1_731_500_000_000.0;
const LIMITS: StoreLimits = StoreLimits {
    max_messages: 64,
    max_bytes: 64 * 1024,
    ttl_ms: ONE_DAY_MS,
};

thread_local! {
    static POOL: RefCell<MessagePool> = RefCell::new(MessagePool::new(LIMITS));
}

fn packet(v: &Value) {
    let message = v.get("message").unwrap_or(v);
    let pow_v = message.get("pow").unwrap_or(message);
    let hash = message.get("hash");
    let _ = pow::check_message(message, NOW_MS, pow::MAX_MESSAGE_PAST_MS);
    let _ = pow::message_kind(message);
    let _ = pow::truthy(Some(v));
    let _ = pow::pow_version_is_current(Some(pow_v));
    let _ = pow::auth_cache_key(hash, Some(pow_v));
    let _ = pow::auth_context_message(message);
    let _ = pow::auth_context_register(v);
    let _ = pow::verify_pow_auth(hash, message.get("timestamp"), pow_v, "dm:00");
    let prev = PrevIds::default();
    for fresh in [None, Some(&prev)] {
        if let Ok(valid) = pow::validate_pow_payload(hash, pow_v, fresh) {
            for s in valid.shares {
                let _ = pow::validate_share_fast(s, &valid.job.job_id).map(|f| f.key());
            }
            let _ = pow::extract_prev_id(&valid.job.blob);
            let _ = pow::insert_nonce(&valid.job.blob, &[1, 2, 3, 4]);
            let _ = pow::parse_target(&valid.job.target);
        }
    }
    let _ = PowJob::from_value(pow_v.get("job").unwrap_or(pow_v), None);
    POOL.with(|p| {
        let mut pool = p.borrow_mut();
        let _ = pool.insert(message.clone(), NOW_MS);
        assert!(pool.len() <= LIMITS.max_messages && pool.bytes() <= LIMITS.max_bytes);
        let _ = pool.since(v.get("timestamp").and_then(Value::as_f64).unwrap_or(0.0), 500);
    });
}

fuzz_target!(|data: &[u8]| {
    if let Ok(msg) = decode_message(data) {
        let mut out = BytesMut::new();
        encode_message(&msg, &mut out);
        assert_eq!(decode_message(&out[4..]).expect("re-encoded message decodes"), msg);
    }
    let Some((&chunk, stream)) = data.split_first() else {
        return;
    };
    let limit = if chunk & 1 == 0 {
        HUGIN_INBOUND_LIMIT + 64
    } else {
        XKRD_INBOUND_LIMIT + 64
    };
    let mut codec = BareRpcCodec::new(limit);
    let mut buf = BytesMut::new();
    'stream: for part in stream.chunks(usize::from(chunk >> 1).max(1)) {
        buf.extend_from_slice(part);
        loop {
            match codec.decode(&mut buf) {
                Ok(Some(RpcMessage::Request {
                    data: Some(d), ..
                })) => {
                    if let Ok(v) = serde_json::from_slice::<Value>(&d) {
                        packet(&v);
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => break 'stream,
            }
        }
    }
    // the JSON parser alone, on the whole input (depth limit 128)
    if let Ok(v) = serde_json::from_slice::<Value>(stream) {
        packet(&v);
    }
});
