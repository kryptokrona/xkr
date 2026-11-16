//! Rust <-> Bare bridge frames (`xkr-p2p-hyper::frame`): single frames and a split stream
//! through `FrameCodec`; never panics, decoded frames round-trip.
#![no_main]

use bytes::{Bytes, BytesMut};
use libfuzzer_sys::fuzz_target;
use tokio_util::codec::Decoder;
use xkr_p2p_hyper::frame::{Frame, FrameCodec, MIN_MAX_FRAME};

fn check(f: &Frame) {
    let wire = f.to_bytes().expect("a decoded frame re-encodes");
    assert_eq!(wire.len(), f.encoded_len());
    assert_eq!(&Frame::decode(wire).expect("a re-encoded frame decodes"), f);
}

fuzz_target!(|data: &[u8]| {
    if let Ok(f) = Frame::decode(Bytes::copy_from_slice(data)) {
        check(&f);
    }
    let Some((&chunk, stream)) = data.split_first() else {
        return;
    };
    // a small max_frame so the limit path is exercised too
    let mut codec = FrameCodec::new(MIN_MAX_FRAME * 4);
    let mut buf = BytesMut::new();
    'stream: for part in stream.chunks(usize::from(chunk).max(1)) {
        buf.extend_from_slice(part);
        loop {
            match codec.decode(&mut buf) {
                Ok(Some(f)) => check(&f),
                Ok(None) => break,
                Err(_) => break 'stream,
            }
        }
    }
});
