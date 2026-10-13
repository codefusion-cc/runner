//! Netlink as any of its protocols has it: messages, attributes, and on Linux a socket that sends requests and
//! reads the kernel's answers. nf_tables is one user; any other protocol, such as rtnetlink, can use it the same way.

/// An error or an acknowledgement.
pub const NLMSG_ERROR: u16 = 2;
/// The end of a listing.
pub const NLMSG_DONE: u16 = 3;
/// A request.
pub const NLM_F_REQUEST: u16 = 0x1;
/// Asks for an acknowledgement.
pub const NLM_F_ACK: u16 = 0x4;
/// A listing changed while it was dumped.
pub const NLM_F_DUMP_INTR: u16 = 0x10;
/// Asks for a listing.
pub const NLM_F_DUMP: u16 = 0x300;
/// Makes what is not there.
pub const NLM_F_CREATE: u16 = 0x400;
/// Replaces what the request names.
pub const NLM_F_REPLACE: u16 = 0x100;
/// Adds at the end.
pub const NLM_F_APPEND: u16 = 0x800;
/// The flag of an attribute that holds attributes.
pub const NLA_F_NESTED: u16 = 0x8000;
/// The bits of an attribute's kind that are flags.
const NLA_FLAGS: u16 = 0xc000;
/// The longest payload an attribute holds: its length, header included, is 16 bits.
pub const LONGEST_PAYLOAD: usize = u16::MAX as usize - 4;

/// Appends an attribute: its length, its kind, the payload, padded to 4 bytes.
///
/// # Panics
///
/// When the payload is longer than [`LONGEST_PAYLOAD`]; a caller that cannot know checks first.
pub fn attribute(to: &mut Vec<u8>, kind: u16, payload: &[u8]) {
    let length = u16::try_from(4 + payload.len()).expect("an attribute longer than netlink holds");
    to.extend_from_slice(&length.to_ne_bytes());
    to.extend_from_slice(&kind.to_ne_bytes());
    to.extend_from_slice(payload);
    to.resize(to.len().next_multiple_of(4), 0);
}

/// Appends an attribute that holds the attributes `fill` writes.
pub fn nested(to: &mut Vec<u8>, kind: u16, fill: impl FnOnce(&mut Vec<u8>)) {
    let mut inner = Vec::new();
    fill(&mut inner);
    attribute(to, kind | NLA_F_NESTED, &inner);
}

/// Appends a message: the header (length, kind, flags, sequence number, port 0), then `body`, padded to 4 bytes.
pub fn message(to: &mut Vec<u8>, kind: u16, flags: u16, sequence: u32, body: &[u8]) {
    to.extend_from_slice(&((16 + body.len()) as u32).to_ne_bytes());
    to.extend_from_slice(&kind.to_ne_bytes());
    to.extend_from_slice(&flags.to_ne_bytes());
    to.extend_from_slice(&sequence.to_ne_bytes());
    to.extend_from_slice(&0u32.to_ne_bytes());
    to.extend_from_slice(body);
    to.resize(to.len().next_multiple_of(4), 0);
}

/// One message read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Message<'a> {
    /// Its kind.
    pub kind: u16,
    /// Its flags.
    pub flags: u16,
    /// Its sequence number.
    pub sequence: u32,
    /// What follows the header.
    pub body: &'a [u8],
}

/// The pieces of `bytes` that each start with their own length, read by `length` and at least `header` bytes long,
/// or an error at the first whose length does not fit. Each is followed by padding to 4 bytes.
fn pieces<'a>(
    bytes: &'a [u8],
    header: usize,
    what: &'static str,
    length: fn(&[u8]) -> usize,
) -> impl Iterator<Item = Result<&'a [u8], String>> {
    let mut rest = bytes;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let size = if rest.len() < header { 0 } else { length(rest) };
        if size < header || size > rest.len() {
            let left = rest.len();
            rest = &[];
            return Some(Err(format!("{what} of {size} bytes where {left} are left")));
        }
        let piece = &rest[..size];
        rest = &rest[size.next_multiple_of(4).min(rest.len())..];
        Some(Ok(piece))
    })
}

/// The messages in `bytes`, or an error at the first whose length does not fit.
pub fn messages(bytes: &[u8]) -> impl Iterator<Item = Result<Message<'_>, String>> {
    pieces(bytes, 16, "a message", |header| {
        u32::from_ne_bytes([header[0], header[1], header[2], header[3]]) as usize
    })
    .map(|piece| {
        piece.map(|message| Message {
            kind: u16::from_ne_bytes([message[4], message[5]]),
            flags: u16::from_ne_bytes([message[6], message[7]]),
            sequence: u32::from_ne_bytes([message[8], message[9], message[10], message[11]]),
            body: &message[16..],
        })
    })
}

/// The attributes in `bytes` as `(kind without its flags, payload)`, or an error at the first whose length does not
/// fit.
pub fn attributes(bytes: &[u8]) -> impl Iterator<Item = Result<(u16, &[u8]), String>> {
    pieces(bytes, 4, "an attribute", |header| {
        usize::from(u16::from_ne_bytes([header[0], header[1]]))
    })
    .map(|piece| {
        piece.map(|attribute| {
            let kind = u16::from_ne_bytes([attribute[2], attribute[3]]) & !NLA_FLAGS;
            (kind, &attribute[4..])
        })
    })
}

#[cfg(target_os = "linux")]
pub use socket::Socket;

#[cfg(target_os = "linux")]
mod socket {
    use super::{NLM_F_DUMP_INTR, NLMSG_DONE, NLMSG_ERROR, messages};
    use crate::Error;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::time::Duration;

    /// How long the kernel may take with an answer.
    const ANSWER_WAIT: Duration = Duration::from_secs(3);
    /// The largest datagram the kernel sends a socket that reads with a buffer this size.
    const BUFFER: usize = 64 * 1024;

    /// A netlink socket of one protocol (`libc::NETLINK_NETFILTER`, `libc::NETLINK_ROUTE`, ...).
    #[derive(Debug)]
    pub struct Socket(OwnedFd);

    impl Socket {
        /// Opens a socket of this netlink protocol, whose reads wait 3 s at most.
        pub fn open(protocol: libc::c_int) -> Result<Self, Error> {
            // SAFETY: socket takes no pointers.
            let raw =
                unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, protocol) };
            if raw < 0 {
                return Err(Error::Io("netlink socket", io::Error::last_os_error()));
            }
            // SAFETY: the descriptor was just made and belongs to nobody else.
            let socket = Self(unsafe { OwnedFd::from_raw_fd(raw) });
            let wait = libc::timeval {
                tv_sec: ANSWER_WAIT.as_secs() as _,
                tv_usec: 0,
            };
            // SAFETY: setsockopt reads the timeval we own, of the size we give.
            let set = unsafe {
                libc::setsockopt(
                    socket.0.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVTIMEO,
                    (&raw const wait).cast(),
                    std::mem::size_of::<libc::timeval>() as libc::socklen_t,
                )
            };
            if set != 0 {
                return Err(Error::Io("netlink socket", io::Error::last_os_error()));
            }
            Ok(socket)
        }

        /// Sends these messages as one datagram and reads `acknowledged` acknowledgements, or the first refusal.
        pub fn request(&self, bytes: &[u8], acknowledged: u32) -> Result<(), Error> {
            self.send(bytes)?;
            let mut buffer = vec![0u8; BUFFER];
            let mut heard = 0;
            while heard < acknowledged {
                for message in messages(self.receive(&mut buffer)?) {
                    let message = message.map_err(Error::Unreadable)?;
                    if message.kind == NLMSG_ERROR {
                        refusal(&message)?;
                        heard += 1;
                    }
                }
            }
            Ok(())
        }

        /// Sends a request for a listing and returns every message of the answer before its end. A listing the
        /// kernel ended with an error, or that changed while it was written (said on any message, its end included),
        /// is an error, never a part of it.
        pub fn dump(&self, request: &[u8]) -> Result<Vec<u8>, Error> {
            self.send(request)?;
            let mut buffer = vec![0u8; BUFFER];
            let mut listing = Vec::new();
            loop {
                for message in messages(self.receive(&mut buffer)?) {
                    let message = message.map_err(Error::Unreadable)?;
                    if message.flags & NLM_F_DUMP_INTR != 0 {
                        return Err(Error::Interrupted);
                    }
                    match message.kind {
                        // The end carries the listing's own error code, 0 when it is whole.
                        NLMSG_DONE => return refusal(&message).map(|()| listing),
                        NLMSG_ERROR => refusal(&message)?,
                        _ => super::message(
                            &mut listing,
                            message.kind,
                            message.flags,
                            message.sequence,
                            message.body,
                        ),
                    }
                }
            }
        }

        fn send(&self, bytes: &[u8]) -> Result<(), Error> {
            // SAFETY: send reads the bytes, which we own.
            let sent = unsafe { libc::send(self.0.as_raw_fd(), bytes.as_ptr().cast(), bytes.len(), 0) };
            if sent != bytes.len() as isize {
                return Err(Error::Io("netlink send", io::Error::last_os_error()));
            }
            Ok(())
        }

        /// One datagram; one larger than the buffer is an error, not a cut answer.
        fn receive<'a>(&self, buffer: &'a mut [u8]) -> Result<&'a [u8], Error> {
            // SAFETY: recv fills at most the buffer's length; MSG_TRUNC only makes it say the datagram's.
            let read = unsafe {
                libc::recv(
                    self.0.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    libc::MSG_TRUNC,
                )
            };
            let Ok(read) = usize::try_from(read) else {
                return Err(Error::Io("the kernel did not answer", io::Error::last_os_error()));
            };
            if read > buffer.len() {
                return Err(Error::Unreadable(format!("an answer of {read} bytes")));
            }
            Ok(&buffer[..read])
        }
    }

    /// An error message's or a listing end's verdict: an acknowledgement, or the refusal of the message it names.
    fn refusal(message: &super::Message<'_>) -> Result<(), Error> {
        let code = message
            .body
            .get(..4)
            .map(|code| i32::from_ne_bytes(code.try_into().unwrap_or_default()))
            .ok_or_else(|| Error::Unreadable("an error message without its code".into()))?;
        if code == 0 {
            return Ok(());
        }
        Err(Error::Refused {
            message: message.sequence,
            error: io::Error::from_raw_os_error(-code),
        })
    }
}
