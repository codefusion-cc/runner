//! Netlink as any of its protocols has it: messages, attributes, and on Linux a socket that sends requests and
//! reads the kernel's answers. nf_tables is one user; the host's network setup (rtnetlink) is another.

/// An error or an acknowledgement.
pub const NLMSG_ERROR: u16 = 2;
/// The end of a listing.
pub const NLMSG_DONE: u16 = 3;
/// A request.
pub const NLM_F_REQUEST: u16 = 0x1;
/// One of many answers.
pub const NLM_F_MULTI: u16 = 0x2;
/// Asks for an acknowledgement.
pub const NLM_F_ACK: u16 = 0x4;
/// A listing changed while it was dumped.
pub const NLM_F_DUMP_INTR: u16 = 0x10;
/// Asks for a listing.
pub const NLM_F_DUMP: u16 = 0x300;
/// Makes what is not there.
pub const NLM_F_CREATE: u16 = 0x400;
/// Adds at the end.
pub const NLM_F_APPEND: u16 = 0x800;
/// The flag of an attribute that holds attributes.
pub const NLA_F_NESTED: u16 = 0x8000;
/// The bits of an attribute's kind that are flags.
const NLA_FLAGS: u16 = 0xc000;

/// Appends an attribute: its length, its kind, the payload, padded to 4 bytes.
pub fn attribute(to: &mut Vec<u8>, kind: u16, payload: &[u8]) {
    to.extend_from_slice(&((4 + payload.len()) as u16).to_ne_bytes());
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

/// The messages in `bytes`, or an error at the first whose length does not fit.
pub fn messages(bytes: &[u8]) -> impl Iterator<Item = Result<Message<'_>, String>> {
    let mut rest = bytes;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let length = rest.get(..4).map_or(0, |length| {
            u32::from_ne_bytes(length.try_into().unwrap_or_default()) as usize
        });
        if length < 16 || length > rest.len() {
            let left = rest.len();
            rest = &[];
            return Some(Err(format!("a message of {length} bytes where {left} are left")));
        }
        let message = Message {
            kind: u16::from_ne_bytes([rest[4], rest[5]]),
            flags: u16::from_ne_bytes([rest[6], rest[7]]),
            sequence: u32::from_ne_bytes([rest[8], rest[9], rest[10], rest[11]]),
            body: &rest[16..length],
        };
        rest = &rest[length.next_multiple_of(4).min(rest.len())..];
        Some(Ok(message))
    })
}

/// The attributes in `bytes` as `(kind without its flags, payload)`, or an error at the first whose length does not
/// fit.
pub fn attributes(bytes: &[u8]) -> impl Iterator<Item = Result<(u16, &[u8]), String>> {
    let mut rest = bytes;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let length = rest.get(..2).map_or(0, |length| {
            usize::from(u16::from_ne_bytes([length[0], length[1]]))
        });
        if length < 4 || length > rest.len() {
            let left = rest.len();
            rest = &[];
            return Some(Err(format!(
                "an attribute of {length} bytes where {left} are left"
            )));
        }
        let kind = u16::from_ne_bytes([rest[2], rest[3]]) & !NLA_FLAGS;
        let payload = &rest[4..length];
        rest = &rest[length.next_multiple_of(4).min(rest.len())..];
        Some(Ok((kind, payload)))
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

        /// Sends a request for a listing and returns every message of the answer before its end.
        pub fn dump(&self, request: &[u8]) -> Result<Vec<u8>, Error> {
            self.send(request)?;
            let mut buffer = vec![0u8; BUFFER];
            let mut listing = Vec::new();
            loop {
                for message in messages(self.receive(&mut buffer)?) {
                    let message = message.map_err(Error::Unreadable)?;
                    match message.kind {
                        NLMSG_DONE => return Ok(listing),
                        NLMSG_ERROR => refusal(&message)?,
                        _ if message.flags & NLM_F_DUMP_INTR != 0 => return Err(Error::Interrupted),
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

    /// An error message's verdict: an acknowledgement, or the refusal of the message it names.
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
