use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::types::{Frame, MAX_FRAME_BYTES};

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("frame is empty")]
    Empty,

    #[error("frame is too large: {0} bytes")]
    TooLarge(usize),

    #[error("codec error: {0}")]
    Codec(#[from] postcard::Error),
}

impl From<FrameError> for crate::types::IpcError {
    fn from(error: FrameError) -> Self {
        crate::types::IpcError::internal(error.to_string())
    }
}

pub fn encode_frame(frame: &Frame) -> Result<Vec<u8>, FrameError> {
    encode_frame_limited(frame, MAX_FRAME_BYTES)
}

/// Encode `frame`, refusing a payload larger than `max_bytes`.
///
/// The handshake uses this with [`crate::types::MAX_HANDSHAKE_BYTES`] so a
/// peer cannot make the other end allocate a post-handshake-sized buffer
/// before it has proved it speaks the protocol.
pub fn encode_frame_limited(frame: &Frame, max_bytes: usize) -> Result<Vec<u8>, FrameError> {
    let payload = postcard::to_allocvec(frame)?;
    if payload.is_empty() {
        return Err(FrameError::Empty);
    }
    if payload.len() > max_bytes {
        return Err(FrameError::TooLarge(payload.len()));
    }
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

pub fn decode_frame(bytes: &[u8]) -> Result<Frame, FrameError> {
    if bytes.len() < 4 {
        return Err(FrameError::Empty);
    }
    let length = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if length == 0 {
        return Err(FrameError::Empty);
    }
    if length > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(length));
    }
    if bytes.len() != 4 + length {
        return Err(FrameError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "truncated frame",
        )));
    }
    Ok(postcard::from_bytes(&bytes[4..])?)
}

pub async fn write_frame<W>(writer: &mut W, frame: &Frame) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    write_frame_limited(writer, frame, MAX_FRAME_BYTES).await
}

/// Write `frame`, refusing a payload larger than `max_bytes`.
pub async fn write_frame_limited<W>(
    writer: &mut W,
    frame: &Frame,
    max_bytes: usize,
) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    let bytes = encode_frame_limited(frame, max_bytes)?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_frame<R>(reader: &mut R) -> Result<Frame, FrameError>
where
    R: AsyncRead + Unpin,
{
    read_frame_limited(reader, MAX_FRAME_BYTES).await
}

/// Read one frame, refusing a zero length or a length above `max_bytes`
/// before allocating the payload.
///
/// The check happens on the 4-byte length prefix alone, so a peer that
/// announces a huge frame costs nothing but the prefix read.
pub async fn read_frame_limited<R>(reader: &mut R, max_bytes: usize) -> Result<Frame, FrameError>
where
    R: AsyncRead + Unpin,
{
    let mut length = [0u8; 4];
    reader.read_exact(&mut length).await?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 {
        return Err(FrameError::Empty);
    }
    if length > max_bytes {
        return Err(FrameError::TooLarge(length));
    }
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload).await?;
    Ok(postcard::from_bytes(&payload)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Hello, IpcError, MAX_HANDSHAKE_BYTES, Reply};

    #[test]
    fn roundtrip_every_frame_shape() {
        let frames = [
            Frame::Hello(Hello::new("test")),
            Frame::Request {
                id: 7,
                op: crate::types::Op::ListGuilds,
            },
            Frame::Response {
                id: 7,
                result: Err(IpcError::not_found("guild")),
            },
            Frame::Response {
                id: 8,
                result: Ok(Reply::Unit),
            },
            Frame::Ping(3),
            Frame::Pong(3),
            Frame::Goodbye,
        ];
        for frame in frames {
            let bytes = encode_frame(&frame).unwrap();
            assert_eq!(decode_frame(&bytes).unwrap(), frame);
        }
    }

    #[test]
    fn truncation_and_oversize_are_rejected() {
        let frame = Frame::Hello(Hello::new("test"));
        let bytes = encode_frame(&frame).unwrap();
        assert!(decode_frame(&bytes[..bytes.len() - 1]).is_err());

        let mut huge = Vec::new();
        huge.extend_from_slice(&(u32::MAX).to_le_bytes());
        assert!(matches!(decode_frame(&huge), Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn a_handshake_sized_frame_roundtrips_within_the_limit() {
        let frame = Frame::Hello(Hello::new("test"));
        let bytes = encode_frame_limited(&frame, MAX_HANDSHAKE_BYTES).unwrap();
        assert!(bytes.len() <= MAX_HANDSHAKE_BYTES);
        assert_eq!(decode_frame(&bytes).unwrap(), frame);
    }

    #[test]
    fn a_frame_one_byte_over_the_limit_is_refused() {
        let frame = Frame::Hello(Hello::new("test"));
        let bytes = encode_frame(&frame).unwrap();
        let payload_len = bytes.len() - 4;
        assert_eq!(
            encode_frame_limited(&frame, payload_len).unwrap(),
            bytes,
            "a payload exactly at the limit must still encode"
        );
        assert!(matches!(
            encode_frame_limited(&frame, payload_len - 1),
            Err(FrameError::TooLarge(_))
        ));
    }

    #[tokio::test]
    async fn the_limited_read_refuses_oversize_and_zero_length_prefixes() {
        // The announced length alone is enough to refuse, without a body.
        let mut oversize = Vec::new();
        oversize.extend_from_slice(&((MAX_HANDSHAKE_BYTES + 1) as u32).to_le_bytes());
        let mut reader: &[u8] = &oversize;
        assert!(matches!(
            read_frame_limited(&mut reader, MAX_HANDSHAKE_BYTES).await,
            Err(FrameError::TooLarge(_))
        ));

        let zero = 0u32.to_le_bytes();
        let mut reader: &[u8] = &zero;
        assert!(matches!(
            read_frame_limited(&mut reader, MAX_HANDSHAKE_BYTES).await,
            Err(FrameError::Empty)
        ));
    }

    #[tokio::test]
    async fn limited_write_and_read_roundtrip_a_handshake_frame() {
        let frame = Frame::Hello(Hello::new("test"));
        let mut out = std::io::Cursor::new(Vec::new());
        write_frame_limited(&mut out, &frame, MAX_HANDSHAKE_BYTES)
            .await
            .unwrap();
        let written = out.into_inner();
        let mut reader: &[u8] = &written;
        assert_eq!(
            read_frame_limited(&mut reader, MAX_HANDSHAKE_BYTES)
                .await
                .unwrap(),
            frame
        );
    }
}
