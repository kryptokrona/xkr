//! epee portable storage (`vendor/epee-encoding`) through every P2P message type: never panics,
//! and the protocol messages round-trip.
#![no_main]

use libfuzzer_sys::fuzz_target;
use xkr_p2p_legacy::wire::{
    HandshakeRequest, HandshakeResponse, MissingTxs, NetworkStateResponse, NewBlock, NewLiteBlock,
    NewTransactions, PingResponse, RequestChain, RequestGetObjects, RequestTxPool,
    ResponseChainEntry, ResponseGetObjects, StatInfoResponse, TimedSyncRequest,
    TimedSyncResponse,
};

/// Types whose every field survives `to_bytes` (no required field that can be empty).
macro_rules! round_trip {
    ($data:expr, $($t:ty),* $(,)?) => {$(
        if let Ok(v) = xkr_epee::from_bytes::<$t, _>(&mut &$data[..]) {
            let bytes = xkr_epee::to_bytes(v.clone()).expect("a decoded message re-encodes");
            let back = xkr_epee::from_bytes::<$t, _>(&mut &bytes[..])
                .expect("a re-encoded message decodes");
            assert_eq!(back, v);
        }
    )*};
}

/// Types with required strings: C++ may send them empty, our encoder then omits them.
macro_rules! decode_only {
    ($data:expr, $($t:ty),* $(,)?) => {$(
        let _ = xkr_epee::from_bytes::<$t, _>(&mut &$data[..]);
    )*};
}

fuzz_target!(|data: &[u8]| {
    round_trip!(
        data,
        NewTransactions,
        NewBlock,
        RequestGetObjects,
        ResponseGetObjects,
        RequestChain,
        ResponseChainEntry,
        RequestTxPool,
        NewLiteBlock,
        MissingTxs,
        HandshakeRequest,
        HandshakeResponse,
        TimedSyncRequest,
        TimedSyncResponse,
        NetworkStateResponse,
    );
    decode_only!(data, PingResponse, StatInfoResponse);
});
