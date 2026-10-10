//! The guest's firewall: what a job may reach, as rules, and the rules as the netlink messages that load them into
//! the kernel's nftables.
//!
//! A job reaches the internet. It does not reach private networks: the Mac itself (the gateway of the machine's
//! network), other jobs' machines next to it, the Mac's local network, link-local addresses and Tailscale. The one
//! exception is DNS at the name servers the network gave. Nothing reaches the job that the job did not ask for.
//! IPv6 exists only on the loopback device.
//!
//! [`decide`] says what a list of rules does with a packet and is what the tests hold against the specification;
//! [`netlink`] is the same list for the kernel.

/// An IPv4 address.
pub type Ip = [u8; 4];

/// The private and special ranges a job does not reach: `(network, prefix length)`.
pub const BLOCKED: &[(Ip, u8)] = &[
    ([10, 0, 0, 0], 8),
    ([172, 16, 0, 0], 12),
    ([192, 168, 0, 0], 16),
    ([169, 254, 0, 0], 16),
    // Carrier-grade NAT, which is where Tailscale's addresses are.
    ([100, 64, 0, 0], 10),
    // Multicast (mDNS on the Mac's network) and the reserved block with the broadcast address.
    ([224, 0, 0, 0], 4),
    ([240, 0, 0, 0], 4),
];

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
    /// It leaves through the loopback device.
    OutLoopback,
    /// It came in through the loopback device.
    InLoopback,
    /// It is IPv6.
    Ipv6,
    /// It is IPv4 and its destination is in this network.
    DestIn {
        /// The network's address.
        network: Ip,
        /// The prefix length, 0 to 32.
        prefix: u8,
    },
    /// Its transport protocol (6 TCP, 17 UDP).
    Protocol(u8),
    /// Its destination port. Only after a [`Match::Protocol`] of TCP or UDP.
    DestPort(u16),
    /// It belongs to, or answers for, a connection the guest opened.
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

/// The whole firewall.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// For packets the guest sends. A packet no rule takes passes.
    pub output: Vec<Rule>,
    /// For packets that arrive. A packet no rule takes is dropped.
    pub input: Vec<Rule>,
}

const TCP: u8 = 6;
const UDP: u8 = 17;

/// The firewall of a job's machine whose network named these name servers.
pub fn policy(name_servers: &[Ip]) -> Policy {
    let rule = |when: &[Match], then| Rule {
        when: when.to_vec(),
        then,
    };
    let mut output = vec![
        rule(&[Match::OutLoopback], Verdict::Accept),
        rule(&[Match::Ipv6], Verdict::Drop),
    ];
    for server in name_servers {
        let at = Match::DestIn {
            network: *server,
            prefix: 32,
        };
        output.push(rule(
            &[at, Match::Protocol(UDP), Match::DestPort(53)],
            Verdict::Accept,
        ));
        output.push(rule(
            &[at, Match::Protocol(TCP), Match::DestPort(53)],
            Verdict::Accept,
        ));
    }
    for (network, prefix) in BLOCKED {
        output.push(rule(
            &[Match::DestIn {
                network: *network,
                prefix: *prefix,
            }],
            Verdict::Drop,
        ));
    }
    Policy {
        output,
        input: vec![
            rule(&[Match::InLoopback], Verdict::Accept),
            rule(&[Match::Established], Verdict::Accept),
        ],
    }
}

/// A packet, as far as the rules look at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet {
    /// IPv6 instead of IPv4.
    pub ipv6: bool,
    /// On the loopback device.
    pub loopback: bool,
    /// The IPv4 destination (ignored for IPv6).
    pub dest: Ip,
    /// The transport protocol.
    pub protocol: u8,
    /// The destination port (TCP and UDP).
    pub dest_port: u16,
    /// Part of, or an answer for, a connection the guest opened.
    pub established: bool,
}

fn mask(prefix: u8) -> u32 {
    u32::MAX.checked_shl(32 - u32::from(prefix.min(32))).unwrap_or(0)
}

/// What a chain does with a packet: the first rule whose conditions all hold decides, else `otherwise`.
pub fn decide(rules: &[Rule], otherwise: Verdict, packet: &Packet) -> Verdict {
    let holds = |condition: &Match| match *condition {
        Match::OutLoopback | Match::InLoopback => packet.loopback,
        Match::Ipv6 => packet.ipv6,
        Match::DestIn { network, prefix } => {
            !packet.ipv6
                && u32::from_be_bytes(packet.dest) & mask(prefix)
                    == u32::from_be_bytes(network) & mask(prefix)
        }
        Match::Protocol(protocol) => packet.protocol == protocol,
        Match::DestPort(port) => packet.dest_port == port,
        Match::Established => packet.established,
    };
    rules
        .iter()
        .find(|rule| rule.when.iter().all(holds))
        .map_or(otherwise, |rule| rule.then)
}

/// The messages that load a firewall, and how many answers the kernel owes for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    /// One transaction: the kernel takes all of it or none.
    pub bytes: Vec<u8>,
    /// The messages in it that the kernel acknowledges one by one.
    pub acknowledged: u32,
}

/// The table's name in the kernel.
pub const TABLE: &str = "codefusion";

// Netlink and nftables numbers, from the kernel's `netlink.h`, `nfnetlink.h` and `nf_tables.h`.
const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_ACK: u16 = 0x4;
const NLM_F_CREATE: u16 = 0x400;
const NLM_F_APPEND: u16 = 0x800;
const NFNL_MSG_BATCH_BEGIN: u16 = 0x10;
const NFNL_MSG_BATCH_END: u16 = 0x11;
const NFNL_SUBSYS_NFTABLES: u16 = 10;
const NFT_MSG_NEWTABLE: u16 = 0;
const NFT_MSG_NEWCHAIN: u16 = 3;
const NFT_MSG_NEWRULE: u16 = 6;
const NFPROTO_INET: u8 = 1;
const NFPROTO_IPV4: u8 = 2;
const NFPROTO_IPV6: u8 = 10;
const NLA_F_NESTED: u16 = 0x8000;
const NF_INET_LOCAL_IN: u32 = 1;
const NF_INET_LOCAL_OUT: u32 = 3;
const NF_DROP: u32 = 0;
const NF_ACCEPT: u32 = 1;
const NFT_REG_VERDICT: u32 = 0;
const NFT_REG_1: u32 = 1;
const NFT_META_IIF: u32 = 4;
const NFT_META_OIF: u32 = 5;
const NFT_META_NFPROTO: u32 = 15;
const NFT_META_L4PROTO: u32 = 16;
const NFT_PAYLOAD_NETWORK_HEADER: u32 = 1;
const NFT_PAYLOAD_TRANSPORT_HEADER: u32 = 2;
const NFT_CMP_EQ: u32 = 0;
const NFT_CMP_NEQ: u32 = 1;
const NFT_CT_STATE: u32 = 0;
/// The loopback device is the first device of a network namespace.
const LOOPBACK_INDEX: u32 = 1;
/// Connection states "established" and "related" as nftables' bits.
const CT_ESTABLISHED_OR_RELATED: u32 = 2 | 4;

fn attribute(to: &mut Vec<u8>, kind: u16, payload: &[u8]) {
    to.extend_from_slice(&((4 + payload.len()) as u16).to_ne_bytes());
    to.extend_from_slice(&kind.to_ne_bytes());
    to.extend_from_slice(payload);
    to.resize(to.len().next_multiple_of(4), 0);
}

fn text(to: &mut Vec<u8>, kind: u16, value: &str) {
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(0);
    attribute(to, kind, &bytes);
}

fn number(to: &mut Vec<u8>, kind: u16, value: u32) {
    attribute(to, kind, &value.to_be_bytes());
}

fn nested(to: &mut Vec<u8>, kind: u16, fill: impl FnOnce(&mut Vec<u8>)) {
    let mut inner = Vec::new();
    fill(&mut inner);
    attribute(to, kind | NLA_F_NESTED, &inner);
}

/// One expression of a rule: its name and its own attributes.
fn expression(to: &mut Vec<u8>, name: &str, fill: impl FnOnce(&mut Vec<u8>)) {
    // NFTA_LIST_ELEM { NFTA_EXPR_NAME, NFTA_EXPR_DATA }
    nested(to, 1, |element| {
        text(element, 1, name);
        nested(element, 2, fill);
    });
}

fn meta(to: &mut Vec<u8>, key: u32) {
    expression(to, "meta", |data| {
        number(data, 2, key); // NFTA_META_KEY
        number(data, 1, NFT_REG_1); // NFTA_META_DREG
    });
}

fn payload(to: &mut Vec<u8>, base: u32, offset: u32, length: u32) {
    expression(to, "payload", |data| {
        number(data, 1, NFT_REG_1); // NFTA_PAYLOAD_DREG
        number(data, 2, base);
        number(data, 3, offset);
        number(data, 4, length);
    });
}

fn compare(to: &mut Vec<u8>, operation: u32, value: &[u8]) {
    expression(to, "cmp", |data| {
        number(data, 1, NFT_REG_1); // NFTA_CMP_SREG
        number(data, 2, operation);
        nested(data, 3, |inner| attribute(inner, 1, value)); // NFTA_CMP_DATA { NFTA_DATA_VALUE }
    });
}

/// `register = register & mask`.
fn and(to: &mut Vec<u8>, mask: &[u8]) {
    expression(to, "bitwise", |data| {
        number(data, 1, NFT_REG_1); // NFTA_BITWISE_SREG
        number(data, 2, NFT_REG_1); // NFTA_BITWISE_DREG
        number(data, 3, mask.len() as u32);
        nested(data, 4, |inner| attribute(inner, 1, mask)); // NFTA_BITWISE_MASK
        nested(data, 5, |inner| attribute(inner, 1, &vec![0; mask.len()])); // NFTA_BITWISE_XOR
    });
}

fn condition(to: &mut Vec<u8>, condition: &Match) {
    match *condition {
        Match::OutLoopback => {
            meta(to, NFT_META_OIF);
            compare(to, NFT_CMP_EQ, &LOOPBACK_INDEX.to_ne_bytes());
        }
        Match::InLoopback => {
            meta(to, NFT_META_IIF);
            compare(to, NFT_CMP_EQ, &LOOPBACK_INDEX.to_ne_bytes());
        }
        Match::Ipv6 => {
            meta(to, NFT_META_NFPROTO);
            compare(to, NFT_CMP_EQ, &[NFPROTO_IPV6]);
        }
        Match::DestIn { network, prefix } => {
            // The table sees both families; an address is read only from an IPv4 header.
            meta(to, NFT_META_NFPROTO);
            compare(to, NFT_CMP_EQ, &[NFPROTO_IPV4]);
            payload(to, NFT_PAYLOAD_NETWORK_HEADER, 16, 4);
            let mask = mask(prefix);
            if mask != u32::MAX {
                and(to, &mask.to_be_bytes());
            }
            compare(
                to,
                NFT_CMP_EQ,
                &(u32::from_be_bytes(network) & mask).to_be_bytes(),
            );
        }
        Match::Protocol(protocol) => {
            meta(to, NFT_META_L4PROTO);
            compare(to, NFT_CMP_EQ, &[protocol]);
        }
        Match::DestPort(port) => {
            payload(to, NFT_PAYLOAD_TRANSPORT_HEADER, 2, 2);
            compare(to, NFT_CMP_EQ, &port.to_be_bytes());
        }
        Match::Established => {
            expression(to, "ct", |data| {
                number(data, 2, NFT_CT_STATE); // NFTA_CT_KEY
                number(data, 1, NFT_REG_1); // NFTA_CT_DREG
            });
            and(to, &CT_ESTABLISHED_OR_RELATED.to_ne_bytes());
            compare(to, NFT_CMP_NEQ, &0u32.to_ne_bytes());
        }
    }
}

fn verdict_code(verdict: Verdict) -> u32 {
    match verdict {
        Verdict::Accept => NF_ACCEPT,
        Verdict::Drop => NF_DROP,
    }
}

struct Builder {
    bytes: Vec<u8>,
    sequence: u32,
    acknowledged: u32,
}

impl Builder {
    /// One message: the netlink header, nfnetlink's own (family, version 0, resource id), then attributes.
    fn message(&mut self, kind: u16, flags: u16, family: u8, resource: u16, fill: impl FnOnce(&mut Vec<u8>)) {
        let mut body = vec![family, 0];
        body.extend_from_slice(&resource.to_be_bytes());
        fill(&mut body);
        self.bytes
            .extend_from_slice(&((16 + body.len()) as u32).to_ne_bytes());
        self.bytes.extend_from_slice(&kind.to_ne_bytes());
        self.bytes.extend_from_slice(&flags.to_ne_bytes());
        self.bytes.extend_from_slice(&self.sequence.to_ne_bytes());
        self.bytes.extend_from_slice(&0u32.to_ne_bytes());
        self.bytes.extend_from_slice(&body);
        self.bytes.resize(self.bytes.len().next_multiple_of(4), 0);
        self.sequence += 1;
    }

    fn nftables(&mut self, kind: u16, extra_flags: u16, fill: impl FnOnce(&mut Vec<u8>)) {
        self.acknowledged += 1;
        self.message(
            (NFNL_SUBSYS_NFTABLES << 8) | kind,
            NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | extra_flags,
            NFPROTO_INET,
            0,
            fill,
        );
    }

    fn chain(&mut self, name: &str, hook: u32, policy: Verdict, rules: &[Rule]) {
        self.nftables(NFT_MSG_NEWCHAIN, 0, |body| {
            text(body, 1, TABLE); // NFTA_CHAIN_TABLE
            text(body, 3, name); // NFTA_CHAIN_NAME
            nested(body, 4, |hook_attributes| {
                number(hook_attributes, 1, hook); // NFTA_HOOK_HOOKNUM
                number(hook_attributes, 2, 0); // NFTA_HOOK_PRIORITY: the filter priority
            });
            number(body, 5, verdict_code(policy)); // NFTA_CHAIN_POLICY
            text(body, 7, "filter"); // NFTA_CHAIN_TYPE
        });
        for rule in rules {
            self.nftables(NFT_MSG_NEWRULE, NLM_F_APPEND, |body| {
                text(body, 1, TABLE); // NFTA_RULE_TABLE
                text(body, 2, name); // NFTA_RULE_CHAIN
                nested(body, 4, |expressions| {
                    for when in &rule.when {
                        condition(expressions, when);
                    }
                    expression(expressions, "immediate", |data| {
                        number(data, 1, NFT_REG_VERDICT); // NFTA_IMMEDIATE_DREG
                        // NFTA_IMMEDIATE_DATA { NFTA_DATA_VERDICT { NFTA_VERDICT_CODE } }
                        nested(data, 2, |inner| {
                            nested(inner, 2, |verdict| number(verdict, 1, verdict_code(rule.then)));
                        });
                    });
                });
            });
        }
    }
}

/// The firewall as one nftables transaction: a table for both IP families, a chain for what the guest sends and
/// one for what arrives.
pub fn netlink(policy: &Policy) -> Batch {
    let mut builder = Builder {
        bytes: Vec::new(),
        sequence: 1,
        acknowledged: 0,
    };
    builder.message(
        NFNL_MSG_BATCH_BEGIN,
        NLM_F_REQUEST,
        0,
        NFNL_SUBSYS_NFTABLES,
        |_| {},
    );
    builder.nftables(NFT_MSG_NEWTABLE, 0, |body| text(body, 1, TABLE));
    builder.chain("output", NF_INET_LOCAL_OUT, Verdict::Accept, &policy.output);
    builder.chain("input", NF_INET_LOCAL_IN, Verdict::Drop, &policy.input);
    builder.message(NFNL_MSG_BATCH_END, NLM_F_REQUEST, 0, NFNL_SUBSYS_NFTABLES, |_| {});
    Batch {
        bytes: builder.bytes,
        acknowledged: builder.acknowledged,
    }
}
