//! Frames: a `u32` little-endian length, then that many bytes of JSON.

use std::io;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// Largest frame body (1 MiB): a block template or 1 000 block headers fit easily.
pub const MAX_FRAME: usize = 1 << 20;

/// Bytes of the length prefix.
pub const LEN_BYTES: usize = 4;

/// Why a frame could not be read, written or decoded.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {0} bytes is over the limit of {1}")]
    TooLarge(usize, usize),
    #[error("empty frame")]
    Empty,
    #[error("the connection ended inside a frame")]
    Truncated,
    #[error("not a message of the protocol: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Reads one frame body of at most `max` bytes; `Ok(None)` when the stream ends cleanly
/// between frames. The body is read as it arrives, so a length alone does not reserve `max`
/// bytes.
pub async fn read_frame<R: AsyncRead + Unpin>(
    r: &mut R,
    max: usize,
) -> Result<Option<Vec<u8>>, FrameError> {
    let mut len = [0u8; LEN_BYTES];
    let mut filled = 0;
    while filled < LEN_BYTES {
        let n = r.read(&mut len[filled..]).await?;
        if n == 0 {
            return if filled == 0 {
                Ok(None)
            } else {
                Err(FrameError::Truncated)
            };
        }
        filled += n;
    }
    let n = u32::from_le_bytes(len) as usize;
    if n == 0 {
        return Err(FrameError::Empty);
    }
    if n > max {
        return Err(FrameError::TooLarge(n, max));
    }
    let mut body = Vec::with_capacity(n.min(64 * 1024));
    (&mut *r).take(n as u64).read_to_end(&mut body).await?;
    if body.len() < n {
        return Err(FrameError::Truncated);
    }
    Ok(Some(body))
}

/// Writes `body` as one frame (length and body in one write) and flushes.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    body: &[u8],
    max: usize,
) -> Result<(), FrameError> {
    if body.is_empty() {
        return Err(FrameError::Empty);
    }
    if body.len() > max {
        return Err(FrameError::TooLarge(body.len(), max));
    }
    let len = u32::try_from(body.len()).map_err(|_| FrameError::TooLarge(body.len(), max))?;
    let mut buf = Vec::with_capacity(LEN_BYTES + body.len());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(body);
    w.write_all(&buf).await?;
    w.flush().await?;
    Ok(())
}

/// A message as a frame body.
pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>, FrameError> {
    Ok(serde_json::to_vec(message)?)
}

/// A frame body as a message.
pub fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, FrameError> {
    Ok(serde_json::from_slice(body)?)
}

#[cfg(test)]
mod tests {

    use super::*;

    #[tokio::test]
    async fn frames_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(4 * MAX_FRAME);
        let bodies: Vec<Vec<u8>> = vec![
            b"{}".to_vec(),
            br#"{"id":1,"method":"getinfo","params":{}}"#.to_vec(),
            vec![b' '; 70_000],
            vec![b'x'; MAX_FRAME],
        ];
        for body in &bodies {
            write_frame(&mut a, body, MAX_FRAME).await.unwrap();
        }
        drop(a);
        for body in &bodies {
            assert_eq!(&read_frame(&mut b, MAX_FRAME).await.unwrap().unwrap(), body);
        }
        assert!(
            read_frame(&mut b, MAX_FRAME).await.unwrap().is_none(),
            "clean end"
        );
    }

    #[tokio::test]
    async fn the_length_is_little_endian() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        write_frame(&mut a, b"{\"a\":1}", MAX_FRAME).await.unwrap();
        let mut raw = [0u8; 11];
        b.read_exact(&mut raw).await.unwrap();
        assert_eq!(&raw[..4], &[7, 0, 0, 0]);
        assert_eq!(&raw[4..], b"{\"a\":1}");
    }

    #[tokio::test]
    async fn oversize_frames_are_refused() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let too_long = u32::try_from(MAX_FRAME + 1).unwrap();
        a.write_all(&too_long.to_le_bytes()).await.unwrap();
        assert!(matches!(
            read_frame(&mut b, MAX_FRAME).await,
            Err(FrameError::TooLarge(n, MAX_FRAME)) if n == MAX_FRAME + 1
        ));
        // a smaller cap applies to the same stream format
        let (mut a, mut b) = tokio::io::duplex(1024);
        write_frame(&mut a, b"{\"abc\":1}", MAX_FRAME)
            .await
            .unwrap();
        assert!(matches!(
            read_frame(&mut b, 4).await,
            Err(FrameError::TooLarge(9, 4))
        ));
        // and the writer refuses to send one
        let (mut a, _b) = tokio::io::duplex(1024);
        assert!(matches!(
            write_frame(&mut a, &vec![b'x'; MAX_FRAME + 1], MAX_FRAME).await,
            Err(FrameError::TooLarge(_, _))
        ));
        assert!(matches!(
            write_frame(&mut a, b"", MAX_FRAME).await,
            Err(FrameError::Empty)
        ));
    }

    #[tokio::test]
    async fn empty_and_cut_frames_are_errors() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        a.write_all(&0u32.to_le_bytes()).await.unwrap();
        assert!(matches!(
            read_frame(&mut b, MAX_FRAME).await,
            Err(FrameError::Empty)
        ));
        let (mut a, mut b) = tokio::io::duplex(1024);
        a.write_all(&10u32.to_le_bytes()).await.unwrap();
        a.write_all(b"{\"a\"").await.unwrap();
        drop(a);
        assert!(matches!(
            read_frame(&mut b, MAX_FRAME).await,
            Err(FrameError::Truncated)
        ));
        let (mut a, mut b) = tokio::io::duplex(1024);
        a.write_all(&[5, 0]).await.unwrap();
        drop(a);
        assert!(matches!(
            read_frame(&mut b, MAX_FRAME).await,
            Err(FrameError::Truncated)
        ));
    }

    #[test]
    fn json_errors_are_reported() {
        assert!(matches!(
            decode::<serde_json::Value>(b"{not json"),
            Err(FrameError::Json(_))
        ));
        let v: serde_json::Value =
            decode(&encode(&serde_json::json!({"a": [1, 2]})).unwrap()).unwrap();
        assert_eq!(v["a"][1], 2);
    }
}
