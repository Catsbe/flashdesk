use crate::{ProtocolError, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Writes a 4-byte big-endian length-prefixed frame.
pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, payload: &[u8]) -> Result<()> {
    if payload.len() > MAX_FRAME_LEN {
        return Err(ProtocolError::Framing(format!(
            "outbound frame too large: {} > {}",
            payload.len(),
            MAX_FRAME_LEN
        )));
    }
    let len = payload.len() as u32;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads a 4-byte big-endian length-prefixed frame up to `MAX_FRAME_LEN`.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>> {
    read_frame_with_limit(reader, MAX_FRAME_LEN).await
}

pub async fn read_frame_with_limit<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_len: usize,
) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > max_len {
        return Err(ProtocolError::Framing(format!(
            "inbound frame too large: {len} > {max_len}"
        )));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(buf)
}
