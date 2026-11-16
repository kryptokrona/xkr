// Rust Levin Library
// Written in 2023 by
//   Cuprate Contributors
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//

//! A tokio-codec for levin buckets

use std::{
    fmt::Debug,
    marker::PhantomData,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use bytes::{Buf, BufMut, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

use crate::u64_to_usize;

use crate::{
    header::{Flags, HEADER_SIZE},
    message::{make_dummy_message, LevinMessage},
    Bucket, BucketBuilder, BucketError, BucketHead, LevinBody, LevinCommand, MessageType, Protocol,
};

#[derive(Debug, Clone)]
pub enum LevinBucketState<C> {
    /// Waiting for the peer to send a header.
    WaitingForHeader,
    /// Waiting for a peer to send a body.
    WaitingForBody(BucketHead<C>),
}

/// Most bytes [`LevinBucketCodec`] reserves for a body that has not arrived yet (xkr, see
/// `vendor/VENDORED.md`).
pub const MAX_EAGER_RESERVE: usize = 64 * 1024;

/// The levin tokio-codec for decoding and encoding raw levin buckets
///
#[derive(Debug, Clone)]
pub struct LevinBucketCodec<C> {
    state: LevinBucketState<C>,
    protocol: Protocol,
    handshake_message_seen: bool,
    /// xkr: lift the pre-handshake limit as soon as a handshake *header* arrives (upstream
    /// behaviour). [`LevinMessageCodec`] turns this off and lifts it only once a handshake
    /// message has decoded.
    lift_on_handshake_header: bool,
    /// xkr: an extra limit on every bucket body that the owner may change at any time (see
    /// [`LevinBucketCodec::set_read_limit`]).
    read_limit: Option<Arc<AtomicU64>>,
}

impl<C> Default for LevinBucketCodec<C> {
    fn default() -> Self {
        Self::new(Protocol::default())
    }
}

impl<C> LevinBucketCodec<C> {
    pub const fn new(protocol: Protocol) -> Self {
        Self {
            state: LevinBucketState::WaitingForHeader,
            protocol,
            handshake_message_seen: false,
            lift_on_handshake_header: true,
            read_limit: None,
        }
    }

    /// xkr: every bucket whose header declares a body larger than the current value of `limit`
    /// fails with [`BucketError::BucketExceededReadLimit`] (on top of `max_packet_size` and the
    /// per-command limits). The owner raises it while it waits for an answer it asked for and
    /// lowers it again afterwards, so a connection only buffers large bodies on request.
    pub fn set_read_limit(&mut self, limit: Arc<AtomicU64>) {
        self.read_limit = Some(limit);
    }

    fn current_read_limit(&self) -> u64 {
        self.read_limit
            .as_ref()
            .map_or(u64::MAX, |l| l.load(Ordering::Relaxed))
    }
}

impl<C: LevinCommand + Debug> Decoder for LevinBucketCodec<C> {
    type Item = Bucket<C>;
    type Error = BucketError;
    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        loop {
            match &self.state {
                LevinBucketState::WaitingForHeader => {
                    if src.len() < HEADER_SIZE {
                        return Ok(None);
                    }

                    let head = BucketHead::<C>::from_bytes(src);

                    if head.signature != self.protocol.signature {
                        #[cfg(feature = "tracing")]
                        tracing::debug!("Peer sent a levin header with an invalid signature.");

                        return Err(BucketError::InvalidHeaderSignature);
                    }

                    #[cfg(feature = "tracing")]
                    tracing::trace!(
                        "Received new bucket header, command: {:?}, waiting for body, body len: {}",
                        head.command,
                        head.size
                    );

                    if head.size > self.protocol.max_packet_size
                        || head.size > head.command.bucket_size_limit()
                    {
                        #[cfg(feature = "tracing")]
                        tracing::debug!("Peer sent message which is too large.");

                        return Err(BucketError::BucketExceededMaxSize);
                    }

                    if head.size > self.current_read_limit() {
                        return Err(BucketError::BucketExceededReadLimit);
                    }

                    if !self.handshake_message_seen {
                        if head.size > self.protocol.max_packet_size_before_handshake {
                            #[cfg(feature = "tracing")]
                            tracing::debug!("Peer sent message which is too large.");

                            return Err(BucketError::BucketExceededMaxSize);
                        }

                        if self.lift_on_handshake_header && head.command.is_handshake() {
                            #[cfg(feature = "tracing")]
                            tracing::debug!(
                                "Peer handshake message seen, increasing bucket size limit."
                            );

                            self.handshake_message_seen = true;
                        }
                    }

                    drop(std::mem::replace(
                        &mut self.state,
                        LevinBucketState::WaitingForBody(head),
                    ));
                }
                LevinBucketState::WaitingForBody(head) => {
                    let body_len = u64_to_usize(head.size);
                    if src.len() < body_len {
                        // xkr: reserve at most `MAX_EAGER_RESERVE` ahead of the data. A 33-byte
                        // header may claim a body of up to `max_packet_size` (100 MB); reserving
                        // all of it up front let one connection pin that much memory without
                        // sending it. The buffer still grows (by doubling) as the body arrives.
                        src.reserve((body_len - src.len()).min(MAX_EAGER_RESERVE));
                        return Ok(None);
                    }

                    let LevinBucketState::WaitingForBody(header) =
                        std::mem::replace(&mut self.state, LevinBucketState::WaitingForHeader)
                    else {
                        unreachable!()
                    };

                    #[cfg(feature = "tracing")]
                    tracing::trace!("Received full bucket for command: {:?}", header.command);

                    return Ok(Some(Bucket {
                        header,
                        body: src.copy_to_bytes(body_len),
                    }));
                }
            }
        }
    }
}

impl<C: LevinCommand> Encoder<Bucket<C>> for LevinBucketCodec<C> {
    type Error = BucketError;
    fn encode(&mut self, item: Bucket<C>, dst: &mut BytesMut) -> Result<(), Self::Error> {
        if let Some(additional) = (HEADER_SIZE + item.body.len()).checked_sub(dst.capacity()) {
            dst.reserve(additional);
        }

        item.header.write_bytes_into(dst);
        dst.put_slice(&item.body);
        Ok(())
    }
}

#[derive(Default, Debug, Clone)]
enum MessageState {
    #[default]
    WaitingForBucket,
    /// Waiting for the rest of a fragmented message.
    ///
    /// We keep the fragmented message as a Vec<u8> instead of [`Bytes`](bytes::Bytes) as [`Bytes`](bytes::Bytes) could point to a
    /// large allocation even if the [`Bytes`](bytes::Bytes) itself is small, so is not safe to keep around for long.
    /// To avoid this completely we just use Vec<u8> for fragmented messages.
    WaitingForRestOfFragment(Vec<u8>),
}

/// A tokio-codec for levin messages or in other words the decoded body
/// of a levin bucket.
#[derive(Debug, Clone)]
pub struct LevinMessageCodec<T: LevinBody> {
    message_ty: PhantomData<T>,
    bucket_codec: LevinBucketCodec<T::Command>,
    state: MessageState,
}

impl<T: LevinBody> Default for LevinMessageCodec<T> {
    fn default() -> Self {
        Self::new(Protocol::default())
    }
}

impl<T: LevinBody> LevinMessageCodec<T> {
    /// Creates a message codec with custom [`Protocol`] settings (signature, size limits).
    ///
    /// xkr: added locally, upstream only offers `Default` (Monero settings).
    pub fn new(protocol: Protocol) -> Self {
        let mut bucket_codec = LevinBucketCodec::new(protocol);
        bucket_codec.lift_on_handshake_header = false;
        Self {
            message_ty: PhantomData,
            bucket_codec,
            state: MessageState::WaitingForBucket,
        }
    }

    /// xkr: see [`LevinBucketCodec::set_read_limit`]; also bounds reassembled fragments.
    #[must_use]
    pub fn with_read_limit(mut self, limit: Arc<AtomicU64>) -> Self {
        self.bucket_codec.set_read_limit(limit);
        self
    }

    /// xkr: a handshake message decoded, so the pre-handshake limit no longer applies.
    fn note_decoded(&mut self, command: &T::Command) {
        if command.is_handshake() {
            self.bucket_codec.handshake_message_seen = true;
        }
    }
}

impl<T: LevinBody> Decoder for LevinMessageCodec<T> {
    type Item = T;
    type Error = BucketError;
    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        loop {
            match &mut self.state {
                MessageState::WaitingForBucket => {
                    let Some(mut bucket) = self.bucket_codec.decode(src)? else {
                        return Ok(None);
                    };

                    let flags = &bucket.header.flags;

                    if flags.contains(Flags::DUMMY) {
                        // Dummy message

                        #[cfg(feature = "tracing")]
                        tracing::trace!("Received DUMMY bucket from peer, ignoring.");
                        // We may have another bucket in `src`.
                        continue;
                    }

                    if flags.contains(Flags::END_FRAGMENT) {
                        return Err(BucketError::InvalidHeaderFlags(
                            "Flag end fragment received before a start fragment",
                        ));
                    }

                    if flags.contains(Flags::START_FRAGMENT) {
                        // monerod does not require a start flag before starting a fragmented message,
                        // but will always produce one, so it is ok for us to require one.

                        #[cfg(feature = "tracing")]
                        tracing::debug!("Bucket is a fragment, waiting for rest of message.");

                        self.state = MessageState::WaitingForRestOfFragment(bucket.body.to_vec());

                        continue;
                    }

                    // Normal, non fragmented bucket

                    let message_type = MessageType::from_flags_and_have_to_return(
                        bucket.header.flags,
                        bucket.header.have_to_return_data,
                    )?;

                    let command = bucket.header.command.clone();
                    let message =
                        T::decode_message(&mut bucket.body, message_type, command.clone())?;
                    self.note_decoded(&command);
                    return Ok(Some(message));
                }
                MessageState::WaitingForRestOfFragment(bytes) => {
                    let Some(bucket) = self.bucket_codec.decode(src)? else {
                        return Ok(None);
                    };

                    let flags = &bucket.header.flags;

                    if flags.contains(Flags::DUMMY) {
                        // Dummy message

                        #[cfg(feature = "tracing")]
                        tracing::trace!("Received DUMMY bucket from peer, ignoring.");
                        // We may have another bucket in `src`.
                        continue;
                    }

                    let max_size = u64_to_usize(
                        if self.bucket_codec.handshake_message_seen {
                            self.bucket_codec.protocol.max_packet_size
                        } else {
                            self.bucket_codec.protocol.max_packet_size_before_handshake
                        }
                        .min(self.bucket_codec.current_read_limit()),
                    );

                    if bytes.len().saturating_add(bucket.body.len()) > max_size {
                        return Err(BucketError::InvalidFragmentedMessage(
                            "Fragmented message exceeded maximum size",
                        ));
                    }

                    #[cfg(feature = "tracing")]
                    tracing::trace!("Received another bucket fragment.");

                    bytes.extend_from_slice(bucket.body.as_ref());

                    if flags.contains(Flags::END_FRAGMENT) {
                        // make sure we only look at the internal bucket and don't use this.
                        drop(bucket);

                        let MessageState::WaitingForRestOfFragment(bytes) =
                            std::mem::replace(&mut self.state, MessageState::WaitingForBucket)
                        else {
                            unreachable!();
                        };

                        // Check there are enough bytes in the fragment to build a header.
                        if bytes.len() < HEADER_SIZE {
                            return Err(BucketError::InvalidFragmentedMessage(
                                "Fragmented message is not large enough to build a bucket.",
                            ));
                        }

                        let mut header_bytes = BytesMut::from(&bytes[0..HEADER_SIZE]);

                        let header = BucketHead::<T::Command>::from_bytes(&mut header_bytes);

                        // xkr: the inner header gets the same checks as an outer one.
                        if header.signature != self.bucket_codec.protocol.signature {
                            return Err(BucketError::InvalidHeaderSignature);
                        }
                        if header.size > header.command.bucket_size_limit()
                            || header.size > self.bucket_codec.protocol.max_packet_size
                        {
                            return Err(BucketError::BucketExceededMaxSize);
                        }
                        if header.size > self.bucket_codec.current_read_limit() {
                            return Err(BucketError::BucketExceededReadLimit);
                        }

                        // Check the fragmented message contains enough bytes to build the message.
                        if bytes.len().saturating_sub(HEADER_SIZE) < u64_to_usize(header.size) {
                            return Err(BucketError::InvalidFragmentedMessage(
                                "Fragmented message does not have enough bytes to fill bucket body",
                            ));
                        }

                        #[cfg(feature = "tracing")]
                        tracing::debug!(
                            "Received final fragment, combined message command: {:?}.",
                            header.command
                        );

                        let message_type = MessageType::from_flags_and_have_to_return(
                            header.flags,
                            header.have_to_return_data,
                        )?;

                        let command = header.command.clone();
                        let message = T::decode_message(
                            &mut &bytes[HEADER_SIZE..],
                            message_type,
                            command.clone(),
                        )?;
                        // xkr: lifted only once the handshake decoded.
                        self.note_decoded(&command);
                        return Ok(Some(message));
                    }
                }
            }
        }
    }
}

impl<T: LevinBody> Encoder<LevinMessage<T>> for LevinMessageCodec<T> {
    type Error = BucketError;
    fn encode(&mut self, item: LevinMessage<T>, dst: &mut BytesMut) -> Result<(), Self::Error> {
        match item {
            LevinMessage::Body(body) => {
                let mut bucket_builder = BucketBuilder::new(&self.bucket_codec.protocol);
                body.encode(&mut bucket_builder)?;
                let bucket = bucket_builder.finish();
                self.bucket_codec.encode(bucket, dst)
            }
            LevinMessage::Bucket(bucket) => self.bucket_codec.encode(bucket, dst),
            LevinMessage::Dummy(size) => {
                let bucket = make_dummy_message(&self.bucket_codec.protocol, size);
                self.bucket_codec.encode(bucket, dst)
            }
        }
    }
}
