//! Length-prefixed postcard messages on reliable QUIC streams.

use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use umac_proto::{CodecError, MAX_FRAME_LEN, decode_body, encode_frame};

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame of {0} bytes exceeds limit")]
    TooLarge(usize),
    #[error("decode: {0}")]
    Codec(#[from] CodecError),
}

pub async fn write_msg<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> Result<(), FrameError> {
    w.write_all(&encode_frame(msg)?).await?;
    Ok(())
}

/// Reads one message. Returns `Ok(None)` on a clean end of stream between frames.
pub async fn read_msg<R: AsyncRead + Unpin, T: DeserializeOwned>(r: &mut R) -> Result<Option<T>, FrameError> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(Some(decode_body(&body)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use umac_proto::ClientMsg;

    #[tokio::test]
    async fn roundtrip_and_eof() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        write_msg(&mut a, &ClientMsg::Ping { nonce: 5 }).await.unwrap();
        drop(a);
        assert_eq!(read_msg::<_, ClientMsg>(&mut b).await.unwrap(), Some(ClientMsg::Ping { nonce: 5 }));
        assert_eq!(read_msg::<_, ClientMsg>(&mut b).await.unwrap(), None);
    }

    #[tokio::test]
    async fn rejects_oversized_length() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&((MAX_FRAME_LEN as u32) + 1).to_le_bytes()).await.unwrap();
        assert!(matches!(read_msg::<_, ClientMsg>(&mut b).await, Err(FrameError::TooLarge(_))));
    }
}
