//! Levin bucket headers and the message codec (`vendor/levin` + `xkr-p2p-legacy`) on a byte
//! stream split at fuzzer-chosen points: never panics, header fields round-trip.
#![no_main]

use bytes::BytesMut;
use libfuzzer_sys::fuzz_target;
use tokio_util::codec::{Decoder, Encoder};
use xkr_levin::header::{BucketHead, HEADER_SIZE};
use xkr_levin::{LevinMessage, LevinMessageCodec, Protocol};
use xkr_p2p_legacy::wire::{LegacyCommand, LegacyMessage};

fuzz_target!(|data: &[u8]| {
    let Some((&chunk, stream)) = data.split_first() else {
        return;
    };
    if stream.len() >= HEADER_SIZE {
        let mut b = BytesMut::from(&stream[..HEADER_SIZE]);
        let head = BucketHead::<LegacyCommand>::from_bytes(&mut b);
        let mut out = BytesMut::new();
        head.write_bytes_into(&mut out);
        assert_eq!(out.len(), HEADER_SIZE);
        // `have_to_return_data` is any non-zero byte on the wire, so compare decoded headers
        assert_eq!(BucketHead::<LegacyCommand>::from_bytes(&mut out), head);
    }

    let mut codec = LevinMessageCodec::<LegacyMessage>::new(Protocol::default());
    let mut buf = BytesMut::new();
    'stream: for part in stream.chunks(usize::from(chunk).max(1)) {
        buf.extend_from_slice(part);
        loop {
            match codec.decode(&mut buf) {
                Ok(Some(msg)) => {
                    // decode-only variants (unknown / empty) refuse to encode; that is fine
                    let mut again = BytesMut::new();
                    let mut enc = LevinMessageCodec::<LegacyMessage>::default();
                    if enc.encode(LevinMessage::Body(msg), &mut again).is_ok() {
                        let _ = LevinMessageCodec::<LegacyMessage>::default().decode(&mut again);
                    }
                }
                Ok(None) => break,
                Err(_) => break 'stream,
            }
        }
    }
});
