//! Firewall rules for the kernel's nf_tables, shared by the guest's supervisor and the host.
//!
//! A [`Table`] holds base chains, a [`Chain`] holds [`Rule`]s, and a rule is a list of [`Match`]es with a
//! [`Verdict`]. [`decide`] says what a chain does with a [`Packet`]: it is what tests hold a rule list against,
//! with no kernel. [`batch`] is the same tables as one netlink transaction, written as `nft` writes them, and
//! [`read`] turns such messages, or what the kernel lists, back into tables. [`apply`] and [`list`] talk to the
//! kernel of the caller's network namespace (Linux only), through [`netlink::Socket`].
//!
//! Only what the Runner needs is here: the inet, bridge and netdev families, filter chains at priority 0, and
//! matches on devices, the network protocol, the IPv4 destination, the transport protocol and port, and the state of
//! a connection.

mod decode;
mod encode;
pub mod netlink;

pub use decode::read;
pub use encode::batch;

use std::fmt;
use std::io;

/// An IPv4 address.
pub type Ip = [u8; 4];

/// IPv4 as the link layer names it, for [`Packet::ether_type`] and [`Match::EtherType`].
pub const IPV4: u16 = 0x0800;
/// ARP as the link layer names it.
pub const ARP: u16 = 0x0806;
/// IPv6 as the link layer names it.
pub const IPV6: u16 = 0x86dd;
/// TCP, for [`Match::Protocol`].
pub const TCP: u8 = 6;
/// UDP, for [`Match::Protocol`].
pub const UDP: u8 = 17;

/// The netmask of a prefix length as a number; a prefix over 32 counts as 32.
fn mask(prefix: u8) -> u32 {
    u32::MAX.checked_shl(32 - u32::from(prefix.min(32))).unwrap_or(0)
}

/// The network `address` is in under a prefix length; a prefix over 32 counts as 32.
pub fn network(address: Ip, prefix: u8) -> Ip {
    (u32::from_be_bytes(address) & mask(prefix)).to_be_bytes()
}

/// The name of a network device as the kernel keeps it: 1 to 15 bytes, padded with zeros to 16.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Interface([u8; 16]);

impl Interface {
    /// A name the kernel would give a device: 1 to 15 bytes, not `.` or `..`, with no NUL, `/`, `:` or byte the
    /// kernel's `isspace` takes for white space (the vertical tab and 0xa0 among them, so a UTF-8 character that holds
    /// that byte too).
    pub fn new(name: &str) -> Result<Self, Error> {
        let bytes = name.as_bytes();
        let valid = !bytes.is_empty()
            && bytes.len() < 16
            && name != "."
            && name != ".."
            && !bytes
                .iter()
                .any(|byte| matches!(byte, 0 | b'/' | b':' | 0x0b | 0xa0) || byte.is_ascii_whitespace());
        if !valid {
            return Err(Error::Interface(name.to_string()));
        }
        let mut padded = [0; 16];
        padded[..bytes.len()].copy_from_slice(bytes);
        Ok(Self(padded))
    }

    /// The name.
    pub fn name(&self) -> &str {
        let end = self.0.iter().position(|byte| *byte == 0).unwrap_or(16);
        // Made from a `str` by `new`, cut at a NUL that only the padding has.
        std::str::from_utf8(&self.0[..end]).unwrap_or_default()
    }
}

impl fmt::Debug for Interface {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Interface({:?})", self.name())
    }
}

/// What happens to a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// It passes.
    Accept,
    /// It disappears.
    Drop,
}

/// One condition on a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Match {
    /// It leaves through the loopback device. Not on an input or ingress hook, where nothing has left yet.
    OutLoopback,
    /// It came in through the loopback device. Not on an output hook.
    InLoopback,
    /// It came in through this device. Not on an output hook.
    InInterface(Interface),
    /// It is IPv6, by the family of an inet table. Inet tables only; elsewhere `EtherType(IPV6)`.
    Ipv6,
    /// Its network protocol, as the link layer names it: [`IPV4`], [`ARP`], [`IPV6`].
    EtherType(u16),
    /// It is IPv4 and its destination is in this network.
    DestIn {
        /// The network's address; bits past the prefix are not looked at.
        network: Ip,
        /// The prefix length, 0 to 32.
        prefix: u8,
    },
    /// It is IP and carries this transport protocol ([`TCP`], [`UDP`], 1 for ICMP).
    Protocol(u8),
    /// Its destination port. Only after a [`Match::Protocol`] of TCP or UDP in the same rule.
    DestPort(u16),
    /// It belongs to, or answers for, a connection this side opened. Not in a netdev table, which sees packets
    /// before connections are tracked.
    Established,
}

/// A rule: when every condition holds, the verdict is final.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// The conditions, all of which must hold.
    pub when: Vec<Match>,
    /// The verdict.
    pub then: Verdict,
}

/// A packet, as far as the rules look at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet {
    /// Its network protocol, as the link layer names it: [`IPV4`], [`ARP`], [`IPV6`] or any other.
    pub ether_type: u16,
    /// The device it came in through; none on an output hook.
    pub in_interface: Option<Interface>,
    /// On the loopback device: in through it on an input hook, out through it on an output hook. A packet whose
    /// `in_interface` is `lo` is in through loopback, as the kernel sees it; `decide` takes both fields as given.
    pub loopback: bool,
    /// The IPv4 destination (only looked at for IPv4).
    pub dest: Ip,
    /// The transport protocol (only looked at for IP).
    pub protocol: u8,
    /// The destination port (only looked at for IP).
    pub dest_port: u16,
    /// Part of, or an answer for, a connection this side opened.
    pub established: bool,
}

/// What a chain does with a packet: the first rule whose conditions all hold decides, else `otherwise`.
pub fn decide(rules: &[Rule], otherwise: Verdict, packet: &Packet) -> Verdict {
    let ip = packet.ether_type == IPV4 || packet.ether_type == IPV6;
    let holds = |condition: &Match| match *condition {
        Match::OutLoopback | Match::InLoopback => packet.loopback,
        Match::InInterface(device) => packet.in_interface == Some(device),
        Match::Ipv6 => packet.ether_type == IPV6,
        Match::EtherType(kind) => packet.ether_type == kind,
        Match::DestIn { network: at, prefix } => {
            packet.ether_type == IPV4 && network(packet.dest, prefix) == network(at, prefix)
        }
        Match::Protocol(protocol) => ip && packet.protocol == protocol,
        Match::DestPort(port) => ip && packet.dest_port == port,
        Match::Established => packet.established,
    };
    rules
        .iter()
        .find(|rule| rule.when.iter().all(holds))
        .map_or(otherwise, |rule| rule.then)
}

/// The families of tables this crate writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// IPv4 and IPv6 as the host's IP stack sees them.
    Inet,
    /// Frames that cross a bridge.
    Bridge,
    /// Frames as one device receives them, before anything else sees them.
    Netdev,
}

/// Where a base chain sees packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hook {
    /// Packets for this host (inet, bridge).
    Input,
    /// Packets that pass through (inet, bridge).
    Forward,
    /// Packets this host sends (inet, bridge).
    Output,
    /// Frames this device receives (netdev).
    Ingress(Interface),
}

/// A base chain of the filter type at priority 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    /// Its name in the table.
    pub name: String,
    /// Where it sees packets.
    pub hook: Hook,
    /// What happens to a packet no rule takes.
    pub policy: Verdict,
    /// The rules, in order.
    pub rules: Vec<Rule>,
}

/// A table and its chains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    /// Its family.
    pub family: Family,
    /// Its name.
    pub name: String,
    /// Its chains.
    pub chains: Vec<Chain>,
}

/// What a batch does with the tables the kernel already holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Load {
    /// Adds the tables beside them; for a kernel that holds none yet.
    Add,
    /// Removes every table of every family first, in the same transaction: the namespace's rules are these.
    Replace,
}

/// The messages that load tables, and how many answers the kernel owes for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    /// One transaction: the kernel takes all of it or none.
    pub bytes: Vec<u8>,
    /// The messages in it that the kernel acknowledges one by one.
    pub acknowledged: u32,
}

/// Applies a batch to the kernel's nf_tables in this process's network namespace: every message acknowledged, or
/// the first refusal, after which the kernel holds what it held before.
#[cfg(target_os = "linux")]
pub fn apply(batch: &Batch) -> Result<(), Error> {
    netlink::Socket::open(libc::NETLINK_NETFILTER)?.request(&batch.bytes, batch.acknowledged)
}

/// The tables the kernel holds in this process's network namespace, read back with [`read`]: a table, chain or
/// rule this crate cannot have written is an [`Error::Unreadable`], and rules that changed between the listings of
/// tables, chains and rules are [`Error::Interrupted`].
#[cfg(target_os = "linux")]
pub fn list() -> Result<Vec<Table>, Error> {
    let socket = netlink::Socket::open(libc::NETLINK_NETFILTER)?;
    let mut answers = Vec::new();
    for kind in [
        encode::NFT_MSG_GETTABLE,
        encode::NFT_MSG_GETCHAIN,
        encode::NFT_MSG_GETRULE,
    ] {
        answers.extend(socket.dump(&encode::dump_request(kind))?);
    }
    // nf_tables puts the generation of the rules it lists in each message's resource id: one generation across the
    // three listings is one set of rules.
    // A broken message is left to `read`, which says what is wrong with it.
    let mut generations = netlink::messages(&answers)
        .flatten()
        .map(|message| message.body.get(2..4));
    if let Some(first) = generations.next()
        && generations.any(|other| other != first)
    {
        return Err(Error::Interrupted);
    }
    read(&answers)
}

/// What went wrong.
#[derive(Debug)]
pub enum Error {
    /// A name no device can have.
    Interface(String),
    /// A table or chain the kernel would refuse, or one that is there twice.
    Table(String),
    /// A rule that would not do what it says.
    Rule {
        /// The table and chain it is in, as `table/chain`.
        chain: String,
        /// Its place in the chain, from 0.
        rule: usize,
        /// What is wrong with it.
        problem: &'static str,
    },
    /// Messages this crate did not write, or broken ones.
    Unreadable(String),
    /// The socket failed.
    Io(&'static str, io::Error),
    /// The kernel refused the message with this sequence number.
    Refused {
        /// The message's sequence number.
        message: u32,
        /// What the kernel said.
        error: io::Error,
    },
    /// What the kernel holds changed while it was listed.
    Interrupted,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Interface(name) => write!(formatter, "no device can be called {name:?}"),
            Self::Table(problem) => formatter.write_str(problem),
            Self::Rule { chain, rule, problem } => write!(formatter, "rule {rule} of {chain}: {problem}"),
            Self::Unreadable(problem) => write!(formatter, "unreadable rules: {problem}"),
            Self::Io(what, error) => write!(formatter, "{what}: {error}"),
            Self::Refused { message, error } => {
                write!(formatter, "the kernel refused message {message}: {error}")
            }
            Self::Interrupted => formatter.write_str("the rules changed while they were listed"),
        }
    }
}

impl std::error::Error for Error {}
