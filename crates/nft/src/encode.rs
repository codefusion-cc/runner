//! Tables as one nf_tables transaction. Each match is written as `nft` writes it (`tests/nft.rs`).

use crate::netlink::{NLM_F_ACK, NLM_F_APPEND, NLM_F_CREATE, NLM_F_REQUEST, attribute, message, nested};
use crate::{Batch, Chain, Error, Family, Hook, IPV4, Load, Match, Rule, TCP, Table, UDP, Verdict, mask};

// Numbers from the kernel's `nfnetlink.h`, `netfilter.h` and `nf_tables.h`.
pub(crate) const NFNL_MSG_BATCH_BEGIN: u16 = 0x10;
pub(crate) const NFNL_MSG_BATCH_END: u16 = 0x11;
pub(crate) const NFNL_SUBSYS_NFTABLES: u16 = 10;
pub(crate) const NFT_MSG_NEWTABLE: u16 = 0;
#[cfg(target_os = "linux")]
pub(crate) const NFT_MSG_GETTABLE: u16 = 1;
pub(crate) const NFT_MSG_DELTABLE: u16 = 2;
pub(crate) const NFT_MSG_NEWCHAIN: u16 = 3;
#[cfg(target_os = "linux")]
pub(crate) const NFT_MSG_GETCHAIN: u16 = 4;
pub(crate) const NFT_MSG_NEWRULE: u16 = 6;
#[cfg(target_os = "linux")]
pub(crate) const NFT_MSG_GETRULE: u16 = 7;
pub(crate) const NFPROTO_UNSPEC: u8 = 0;
pub(crate) const NFPROTO_INET: u8 = 1;
pub(crate) const NFPROTO_IPV4: u8 = 2;
pub(crate) const NFPROTO_NETDEV: u8 = 5;
pub(crate) const NFPROTO_BRIDGE: u8 = 7;
pub(crate) const NFPROTO_IPV6: u8 = 10;
pub(crate) const NF_INET_LOCAL_IN: u32 = 1;
pub(crate) const NF_INET_FORWARD: u32 = 2;
pub(crate) const NF_INET_LOCAL_OUT: u32 = 3;
pub(crate) const NF_NETDEV_INGRESS: u32 = 0;
pub(crate) const NF_DROP: u32 = 0;
pub(crate) const NF_ACCEPT: u32 = 1;
pub(crate) const NFT_REG_VERDICT: u32 = 0;
pub(crate) const NFT_REG_1: u32 = 1;
pub(crate) const NFT_META_PROTOCOL: u32 = 1;
pub(crate) const NFT_META_IIF: u32 = 4;
pub(crate) const NFT_META_OIF: u32 = 5;
pub(crate) const NFT_META_IIFNAME: u32 = 6;
pub(crate) const NFT_META_NFPROTO: u32 = 15;
pub(crate) const NFT_META_L4PROTO: u32 = 16;
pub(crate) const NFT_PAYLOAD_NETWORK_HEADER: u32 = 1;
pub(crate) const NFT_PAYLOAD_TRANSPORT_HEADER: u32 = 2;
pub(crate) const NFT_CMP_EQ: u32 = 0;
pub(crate) const NFT_CMP_NEQ: u32 = 1;
pub(crate) const NFT_CT_STATE: u32 = 0;
/// The loopback device is the first device of a network namespace.
pub(crate) const LOOPBACK_INDEX: u32 = 1;
/// Connection states "established" and "related" as nftables' bits.
pub(crate) const CT_ESTABLISHED_OR_RELATED: u32 = 2 | 4;
/// The longest table or chain name, without its NUL.
const LONGEST_NAME: usize = 255;
/// The chain type of every chain here.
pub(crate) const FILTER: &str = "filter";

pub(crate) fn family_number(family: Family) -> u8 {
    match family {
        Family::Inet => NFPROTO_INET,
        Family::Bridge => NFPROTO_BRIDGE,
        Family::Netdev => NFPROTO_NETDEV,
    }
}

fn text(to: &mut Vec<u8>, kind: u16, value: &str) {
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(0);
    attribute(to, kind, &bytes);
}

/// nf_tables' numbers are big-endian.
fn number(to: &mut Vec<u8>, kind: u16, value: u32) {
    attribute(to, kind, &value.to_be_bytes());
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

fn condition(to: &mut Vec<u8>, family: Family, condition: &Match) {
    match *condition {
        Match::OutLoopback => {
            meta(to, NFT_META_OIF);
            compare(to, NFT_CMP_EQ, &LOOPBACK_INDEX.to_ne_bytes());
        }
        Match::InLoopback => {
            meta(to, NFT_META_IIF);
            compare(to, NFT_CMP_EQ, &LOOPBACK_INDEX.to_ne_bytes());
        }
        Match::InInterface(device) => {
            meta(to, NFT_META_IIFNAME);
            compare(to, NFT_CMP_EQ, &device.0);
        }
        Match::Ipv6 => {
            meta(to, NFT_META_NFPROTO);
            compare(to, NFT_CMP_EQ, &[NFPROTO_IPV6]);
        }
        Match::EtherType(kind) => {
            meta(to, NFT_META_PROTOCOL);
            compare(to, NFT_CMP_EQ, &kind.to_be_bytes());
        }
        Match::DestIn { network, prefix } => {
            // An address is read only from an IPv4 header: an inet table knows by its family, the others by the
            // network protocol.
            if family == Family::Inet {
                meta(to, NFT_META_NFPROTO);
                compare(to, NFT_CMP_EQ, &[NFPROTO_IPV4]);
            } else {
                meta(to, NFT_META_PROTOCOL);
                compare(to, NFT_CMP_EQ, &IPV4.to_be_bytes());
            }
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

pub(crate) fn verdict_code(verdict: Verdict) -> u32 {
    match verdict {
        Verdict::Accept => NF_ACCEPT,
        Verdict::Drop => NF_DROP,
    }
}

pub(crate) fn hook_number(hook: Hook) -> u32 {
    match hook {
        Hook::Input => NF_INET_LOCAL_IN,
        Hook::Forward => NF_INET_FORWARD,
        Hook::Output => NF_INET_LOCAL_OUT,
        Hook::Ingress(_) => NF_NETDEV_INGRESS,
    }
}

fn check_name(what: &str, name: &str) -> Result<(), Error> {
    if name.is_empty() || name.len() > LONGEST_NAME || name.contains('\0') {
        return Err(Error::Table(format!(
            "a {what} cannot be called {name:?}: 1 to {LONGEST_NAME} bytes, none of them NUL"
        )));
    }
    Ok(())
}

/// Why a rule in this chain would not do what it says, if it would not.
fn rule_problem(family: Family, hook: Hook, rule: &Rule) -> Option<&'static str> {
    let receives = !matches!(hook, Hook::Output);
    let sends = matches!(hook, Hook::Output | Hook::Forward);
    let mut transport = false;
    for condition in &rule.when {
        let problem = match *condition {
            Match::OutLoopback if !sends => "nothing has left through a device on this hook",
            Match::InLoopback | Match::InInterface(_) if !receives => {
                "nothing came in through a device on this hook"
            }
            Match::Ipv6 if family != Family::Inet => {
                "only an inet table knows IPv6 by its family; match the ether type"
            }
            Match::DestIn { prefix, .. } if prefix > 32 => "a prefix longer than 32 bits",
            Match::DestPort(_) if !transport => "a port without TCP or UDP before it",
            Match::Established if family == Family::Netdev => {
                "a netdev table sees packets before connections are tracked"
            }
            Match::Protocol(TCP | UDP) => {
                transport = true;
                continue;
            }
            _ => continue,
        };
        return Some(problem);
    }
    None
}

fn check(tables: &[Table]) -> Result<(), Error> {
    for (index, table) in tables.iter().enumerate() {
        check_name("table", &table.name)?;
        if tables[..index]
            .iter()
            .any(|other| other.family == table.family && other.name == table.name)
        {
            return Err(Error::Table(format!("table {:?} is there twice", table.name)));
        }
        for (index, chain) in table.chains.iter().enumerate() {
            check_name("chain", &chain.name)?;
            if table.chains[..index].iter().any(|other| other.name == chain.name) {
                return Err(Error::Table(format!(
                    "chain {:?} is in table {:?} twice",
                    chain.name, table.name
                )));
            }
            let fits = match chain.hook {
                Hook::Ingress(_) => table.family == Family::Netdev,
                Hook::Input | Hook::Forward | Hook::Output => table.family != Family::Netdev,
            };
            if !fits {
                return Err(Error::Table(format!(
                    "a {:?} table has no {:?} hook (chain {:?})",
                    table.family, chain.hook, chain.name
                )));
            }
            for (index, rule) in chain.rules.iter().enumerate() {
                if let Some(problem) = rule_problem(table.family, chain.hook, rule) {
                    return Err(Error::Rule {
                        chain: format!("{}/{}", table.name, chain.name),
                        rule: index,
                        problem,
                    });
                }
            }
        }
    }
    Ok(())
}

struct Builder {
    bytes: Vec<u8>,
    sequence: u32,
    acknowledged: u32,
}

impl Builder {
    /// One message: nfnetlink's header (family, version 0, resource id), then attributes.
    fn message(&mut self, kind: u16, flags: u16, family: u8, resource: u16, fill: impl FnOnce(&mut Vec<u8>)) {
        let mut body = vec![family, 0];
        body.extend_from_slice(&resource.to_be_bytes());
        fill(&mut body);
        message(&mut self.bytes, kind, flags, self.sequence, &body);
        self.sequence += 1;
    }

    fn nftables(&mut self, kind: u16, flags: u16, family: u8, fill: impl FnOnce(&mut Vec<u8>)) {
        self.acknowledged += 1;
        self.message(
            (NFNL_SUBSYS_NFTABLES << 8) | kind,
            NLM_F_REQUEST | NLM_F_ACK | flags,
            family,
            0,
            fill,
        );
    }

    fn chain(&mut self, table: &Table, chain: &Chain) {
        let family = family_number(table.family);
        self.nftables(NFT_MSG_NEWCHAIN, NLM_F_CREATE, family, |body| {
            text(body, 1, &table.name); // NFTA_CHAIN_TABLE
            text(body, 3, &chain.name); // NFTA_CHAIN_NAME
            nested(body, 4, |hook| {
                number(hook, 1, hook_number(chain.hook)); // NFTA_HOOK_HOOKNUM
                number(hook, 2, 0); // NFTA_HOOK_PRIORITY: the filter priority
                if let Hook::Ingress(device) = chain.hook {
                    text(hook, 3, device.name()); // NFTA_HOOK_DEV
                }
            });
            number(body, 5, verdict_code(chain.policy)); // NFTA_CHAIN_POLICY
            text(body, 7, FILTER); // NFTA_CHAIN_TYPE
        });
        for rule in &chain.rules {
            self.nftables(NFT_MSG_NEWRULE, NLM_F_CREATE | NLM_F_APPEND, family, |body| {
                text(body, 1, &table.name); // NFTA_RULE_TABLE
                text(body, 2, &chain.name); // NFTA_RULE_CHAIN
                nested(body, 4, |expressions| {
                    for when in &rule.when {
                        condition(expressions, table.family, when);
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

/// The tables as one nftables transaction, in order: each table, then its chains, each chain followed by its
/// rules. A name the kernel would refuse, a hook the family does not have, or a rule that could not do what it
/// says is an error, and nothing is written.
pub fn batch(tables: &[Table], load: Load) -> Result<Batch, Error> {
    check(tables)?;
    let mut builder = Builder {
        bytes: Vec::new(),
        sequence: 1,
        acknowledged: 0,
    };
    builder.message(
        NFNL_MSG_BATCH_BEGIN,
        NLM_F_REQUEST,
        NFPROTO_UNSPEC,
        NFNL_SUBSYS_NFTABLES,
        |_| {},
    );
    if load == Load::Replace {
        // No family and no name: every table of every family, as `nft flush ruleset` does.
        builder.nftables(NFT_MSG_DELTABLE, 0, NFPROTO_UNSPEC, |_| {});
    }
    for table in tables {
        builder.nftables(
            NFT_MSG_NEWTABLE,
            NLM_F_CREATE,
            family_number(table.family),
            |body| {
                text(body, 1, &table.name); // NFTA_TABLE_NAME
            },
        );
        for chain in &table.chains {
            builder.chain(table, chain);
        }
    }
    builder.message(
        NFNL_MSG_BATCH_END,
        NLM_F_REQUEST,
        NFPROTO_UNSPEC,
        NFNL_SUBSYS_NFTABLES,
        |_| {},
    );
    Ok(Batch {
        bytes: builder.bytes,
        acknowledged: builder.acknowledged,
    })
}

/// A request for a listing of every family's tables, chains or rules.
#[cfg(target_os = "linux")]
pub(crate) fn dump_request(kind: u16) -> Vec<u8> {
    use crate::netlink::NLM_F_DUMP;
    let mut bytes = Vec::new();
    message(
        &mut bytes,
        (NFNL_SUBSYS_NFTABLES << 8) | kind,
        NLM_F_REQUEST | NLM_F_DUMP,
        0,
        &[NFPROTO_UNSPEC, 0, 0, 0],
    );
    bytes
}
