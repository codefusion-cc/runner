use std::io::{ErrorKind, Read, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// The largest frame either side sends or accepts. A message is a few hundred bytes; a one-job configuration of the
/// Actions runner is a few kilobytes.
pub const MAX_FRAME: usize = 1 << 20;

/// Why a frame was refused, or why the connection could not carry one.
#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    /// The length prefix names more than [`MAX_FRAME`] bytes.
    #[error("a frame of {0} bytes is larger than the {MAX_FRAME} allowed")]
    TooLarge(usize),
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
        return Err(ProtoError::TooLarge(body.len()));
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

/// Reads one message, blocking. `Ok(None)` when the peer closed the connection between two frames.
pub fn read_message<T: DeserializeOwned>(from: &mut impl Read) -> Result<Option<T>, ProtoError> {
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
    if length > MAX_FRAME {
        return Err(ProtoError::TooLarge(length));
    }
    let mut body = vec![0u8; length];
    from.read_exact(&mut body).map_err(|error| match error.kind() {
        ErrorKind::UnexpectedEof => ProtoError::Truncated,
        _ => ProtoError::Io(error),
    })?;
    parse(&body).map(Some)
}

/// Turns bytes as they arrive into messages, for a reader that cannot block on one frame.
#[derive(Debug, Default)]
pub struct Decoder {
    buffer: Vec<u8>,
}

impl Decoder {
    /// A decoder with nothing buffered.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds bytes that arrived.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// The next whole message, `Ok(None)` when more bytes are needed. After an error the stream has lost its
    /// framing: drop the connection.
    pub fn take<T: DeserializeOwned>(&mut self) -> Result<Option<T>, ProtoError> {
        let Some(prefix) = self.buffer.first_chunk::<4>() else {
            return Ok(None);
        };
        let length = u32::from_be_bytes(*prefix) as usize;
        if length > MAX_FRAME {
            return Err(ProtoError::TooLarge(length));
        }
        if self.buffer.len() < 4 + length {
            return Ok(None);
        }
        let message = parse(&self.buffer[4..4 + length]);
        self.buffer.drain(..4 + length);
        message.map(Some)
    }

    /// Whether bytes of an unfinished frame are held: a connection that closes now was cut.
    pub fn mid_frame(&self) -> bool {
        !self.buffer.is_empty()
    }
}

fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, ProtoError> {
    serde_json::from_slice(body).map_err(|error| ProtoError::Malformed(error.to_string()))
}
