use std::io::{ErrorKind, Read, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::message::GuestMessage;

/// The largest frame either side sends, and the largest a guest accepts. A message is a few hundred bytes; a one-job
/// configuration of the Actions runner is a few kilobytes.
pub const MAX_FRAME: usize = 1 << 20;

/// The largest frame a host accepts from a guest. What a guest says is a few hundred bytes, and a guest runs code
/// nobody vetted, so the host holds no more of one frame than this.
pub const MAX_GUEST_FRAME: usize = 64 << 10;

/// Why a frame was refused, or why the connection could not carry one.
#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    /// The length prefix names more bytes than the reader takes, or a message is longer than [`MAX_FRAME`].
    #[error("a frame of {size} bytes is larger than the {limit} allowed")]
    TooLarge {
        /// The frame's size.
        size: usize,
        /// The most that is allowed.
        limit: usize,
    },
    /// The frame's bytes are not a message of the expected kind.
    #[error("a frame is not a message: {0}")]
    Malformed(String),
    /// The connection ended inside a frame.
    #[error("the connection ended inside a frame")]
    Truncated,
    /// The connection failed.
    #[error("the connection failed: {0}")]
    Io(#[from] std::io::Error),
}

/// One message as a frame: the length, then JSON.
///
/// Fails only for a message larger than [`MAX_FRAME`], which no message of this protocol is unless a field (a
/// configuration, a process name) is absurdly long.
pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>, ProtoError> {
    let body = serde_json::to_vec(message).map_err(|error| ProtoError::Malformed(error.to_string()))?;
    if body.len() > MAX_FRAME {
        return Err(ProtoError::TooLarge {
            size: body.len(),
            limit: MAX_FRAME,
        });
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Writes one message and flushes.
pub fn write_message<T: Serialize>(to: &mut impl Write, message: &T) -> Result<(), ProtoError> {
    to.write_all(&encode(message)?)?;
    to.flush()?;
    Ok(())
}

/// Reads one message of at most [`MAX_FRAME`] bytes, blocking. `Ok(None)` when the peer closed the connection
/// between two frames.
pub fn read_message<T: DeserializeOwned>(from: &mut impl Read) -> Result<Option<T>, ProtoError> {
    read_message_within(from, MAX_FRAME)
}

/// Reads one message of at most `limit` bytes, blocking: a frame that claims more is refused before a byte of it is
/// read. `Ok(None)` when the peer closed the connection between two frames. After an error the stream has lost its
/// framing: drop the connection.
pub fn read_message_within<T: DeserializeOwned>(
    from: &mut impl Read,
    limit: usize,
) -> Result<Option<T>, ProtoError> {
    let mut length = [0u8; 4];
    let mut have = 0;
    while have < length.len() {
        match from.read(&mut length[have..]) {
            Ok(0) if have == 0 => return Ok(None),
            Ok(0) => return Err(ProtoError::Truncated),
            Ok(read) => have += read,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > limit {
        return Err(ProtoError::TooLarge { size: length, limit });
    }
    let mut body = vec![0u8; length];
    from.read_exact(&mut body).map_err(|error| match error.kind() {
        ErrorKind::UnexpectedEof => ProtoError::Truncated,
        _ => ProtoError::Io(error),
    })?;
    parse(&body).map(Some)
}

/// Reads a guest's next message as a host takes it: a frame of at most [`MAX_GUEST_FRAME`] bytes, its texts cut
/// short and cleaned ([`GuestMessage::clipped`]). Every read of a guest on the host goes through here.
pub fn read_guest_message(from: &mut impl Read) -> Result<Option<GuestMessage>, ProtoError> {
    Ok(read_message_within::<GuestMessage>(from, MAX_GUEST_FRAME)?.map(GuestMessage::clipped))
}

fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, ProtoError> {
    serde_json::from_slice(body).map_err(|error| ProtoError::Malformed(error.to_string()))
}
