//! An outside look at a game server: one Source engine `A2S_INFO` query over UDP, as a player's server browser asks
//! it, and of its answer only the server's name, its players and its most players.
//!
//! The query is `FF FF FF FF 54 "Source Engine Query\0"`. A server that wants proof the asker is at its address
//! answers `FF FF FF FF 41` and four bytes; the query goes once more with those four bytes after it. The answer is
//! `FF FF FF FF 49`, a protocol byte, the name, map, folder and game as NUL-ended strings, the game's two-byte id, the
//! players and the most players; what follows (bots, the server's type and so on) is not read. `A2S_PLAYER`, which
//! names the players, is never sent: no player's name is read or kept.
//!
//! What comes back is a stranger's input. [`parse`] reads any bytes into a [`Reply`] or a [`Malformed`] and never
//! panics, and the name it keeps is at most [`NAME_CHARS`] characters with no control or direction characters.
//! [`query`] asks one server on one socket, at most twice (the challenge), and ends by its timeout whatever the
//! server does.

use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::time::Instant;

/// The most characters of a server's name kept.
pub const NAME_CHARS: usize = 64;
/// How long a query waits for its answer, the challenge's round included, by default.
pub const TIMEOUT: Duration = Duration::from_secs(3);
/// The longest host name a target may have (DNS's 253).
pub const HOST_MAX: usize = 253;
/// The bytes read of one reply: an `A2S_INFO` answer fits one packet of the Source engine's 1400.
const READ_BYTES: usize = 1500;

const HEADER: [u8; 4] = [0xFF; 4];
const INFO_REQUEST: u8 = 0x54;
const CHALLENGE: u8 = 0x41;
const INFO: u8 = 0x49;
const PAYLOAD: &[u8] = b"Source Engine Query\0";

/// What a server said of itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    /// Its name, clipped to [`NAME_CHARS`] with control and direction characters made spaces and the ends trimmed.
    pub name: String,
    /// Players on it now.
    pub players: u8,
    /// The most players it takes.
    pub max: u8,
}

/// A reply read: a challenge to send back, or the server's info.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// `0x41`: send the query again with these four bytes after it.
    Challenge([u8; 4]),
    /// `0x49`: the answer.
    Info(Info),
}

/// Why a reply is not one this reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Malformed {
    /// It ends before what it must hold.
    #[error("the reply is too short")]
    Short,
    /// It does not start `FF FF FF FF` (a split reply, `FF FF FF FE`, included: an info reply fits one packet).
    #[error("the reply does not start as a single packet")]
    Header,
    /// Its kind is neither a challenge nor info.
    #[error("the reply is of kind 0x{0:02x}, not info")]
    Kind(u8),
    /// A string of it has no NUL at its end.
    #[error("the reply's {0} has no end")]
    Unended(&'static str),
}

/// Why a query found no answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    /// The target is no host and port one asks.
    #[error("not a target: {0}")]
    Target(&'static str),
    /// Its name has no address.
    #[error("the name has no address: {0}")]
    Resolve(String),
    /// The socket failed: refused by the host (nothing listens), no route, and the like.
    #[error("no answer: {0}")]
    Io(String),
    /// Nothing came in time.
    #[error("no answer in {} s", .0.as_secs_f32())]
    Timeout(Duration),
    /// What came is not a reply.
    #[error("{0}")]
    Malformed(Malformed),
    /// It answered the challenged query with another challenge.
    #[error("the server challenged the answered challenge again")]
    ChallengedTwice,
}

/// The server's info and how long the query took, from its first send to the info's arrival.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// What it said.
    pub info: Info,
    /// The whole exchange, the challenge's round included.
    pub latency: Duration,
}

/// The `A2S_INFO` query, with the challenge a server sent, if any.
#[must_use]
pub fn request(challenge: Option<[u8; 4]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + 1 + PAYLOAD.len() + 4);
    out.extend_from_slice(&HEADER);
    out.push(INFO_REQUEST);
    out.extend_from_slice(PAYLOAD);
    if let Some(challenge) = challenge {
        out.extend_from_slice(&challenge);
    }
    out
}

/// Reads one reply. Never panics, whatever `bytes` hold.
///
/// # Errors
///
/// [`Malformed`] when the bytes are no challenge and no info reply.
pub fn parse(bytes: &[u8]) -> Result<Reply, Malformed> {
    let mut at = Reader { bytes, at: 0 };
    if at.take(4).ok_or(Malformed::Short)? != HEADER {
        return Err(Malformed::Header);
    }
    match at.byte()? {
        CHALLENGE => {
            let c = at.take(4).ok_or(Malformed::Short)?;
            Ok(Reply::Challenge([c[0], c[1], c[2], c[3]]))
        }
        INFO => {
            let _protocol = at.byte()?;
            let name = at.string("name")?;
            at.string("map")?;
            at.string("folder")?;
            at.string("game")?;
            at.take(2).ok_or(Malformed::Short)?;
            let players = at.byte()?;
            let max = at.byte()?;
            Ok(Reply::Info(Info {
                name: clean(name),
                players,
                max,
            }))
        }
        kind => Err(Malformed::Kind(kind)),
    }
}

/// A name as kept: lossy UTF-8, at most [`NAME_CHARS`] characters, control and direction characters made spaces,
/// the ends trimmed.
fn clean(raw: &[u8]) -> String {
    cf_guest_proto::clip(&String::from_utf8_lossy(raw), NAME_CHARS)
        .trim()
        .to_owned()
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let out = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(out)
    }

    fn byte(&mut self) -> Result<u8, Malformed> {
        self.take(1).map(|b| b[0]).ok_or(Malformed::Short)
    }

    /// The bytes before the next NUL, which is passed.
    fn string(&mut self, what: &'static str) -> Result<&'a [u8], Malformed> {
        let rest = self.bytes.get(self.at..).unwrap_or_default();
        let end = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or(Malformed::Unended(what))?;
        self.at += end + 1;
        Ok(&rest[..end])
    }
}

/// Whether `host` is one a target may name: a DNS name or an address, 1 to [`HOST_MAX`] of letters, digits, `.`,
/// `-` and `:`, not starting with `-`.
#[must_use]
pub fn is_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= HOST_MAX
        && !host.starts_with('-')
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'))
}

/// Asks `host:port` for its info on a socket of its own: resolve, send, and on a challenge send once more. Ends
/// within `timeout`, the name's lookup included. Only the target's address is heard: the socket is connected to it.
/// Of a name's addresses each is asked in turn while the one before refuses at once.
///
/// # Errors
///
/// [`QueryError`] for a target that is none, no address, a socket error, no answer in time, a malformed reply or
/// a second challenge.
pub async fn query(host: &str, port: u16, timeout: Duration) -> Result<Answer, QueryError> {
    if !is_host(host) {
        return Err(QueryError::Target("the host is not a name or an address"));
    }
    if port == 0 {
        return Err(QueryError::Target("port 0"));
    }
    match tokio::time::timeout(timeout, exchange(host, port)).await {
        Ok(result) => result,
        Err(_) => Err(QueryError::Timeout(timeout)),
    }
}

async fn exchange(host: &str, port: u16) -> Result<Answer, QueryError> {
    let addresses: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| QueryError::Resolve(e.to_string()))?
        .collect();
    // A name may have several addresses (`localhost`: `::1` before `127.0.0.1`): one the host refuses at once, as
    // nothing listens there, gives way to the next.
    let mut last = QueryError::Resolve("none".into());
    for address in addresses {
        match ask(address).await {
            Err(QueryError::Io(why)) => last = QueryError::Io(why),
            done => return done,
        }
    }
    Err(last)
}

async fn ask(address: std::net::SocketAddr) -> Result<Answer, QueryError> {
    let local = if address.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let io = |e: std::io::Error| QueryError::Io(e.to_string());
    let socket = UdpSocket::bind(local).await.map_err(io)?;
    socket.connect(address).await.map_err(io)?;
    let started = Instant::now();
    socket.send(&request(None)).await.map_err(io)?;
    let mut buf = [0u8; READ_BYTES];
    let mut challenged = false;
    loop {
        let n = socket.recv(&mut buf).await.map_err(io)?;
        match parse(&buf[..n]).map_err(QueryError::Malformed)? {
            Reply::Info(info) => {
                return Ok(Answer {
                    info,
                    latency: started.elapsed(),
                });
            }
            Reply::Challenge(_) if challenged => return Err(QueryError::ChallengedTwice),
            Reply::Challenge(challenge) => {
                challenged = true;
                socket.send(&request(Some(challenge))).await.map_err(io)?;
            }
        }
    }
}
