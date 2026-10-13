//! nf_tables messages back into tables: what a batch of this crate's says, or what the kernel lists. The kernel
//! writes its listing in its own way (no nesting flags, attributes of its own, extra ones such as handles), so only
//! the attributes that carry meaning are read, and anything this crate could not have written is an error.

use crate::encode::{
    CT_ESTABLISHED_OR_RELATED, FILTER, LOOPBACK_INDEX, NF_ACCEPT, NF_DROP, NF_INET_FORWARD, NF_INET_LOCAL_IN,
    NF_INET_LOCAL_OUT, NF_NETDEV_INGRESS, NFNL_MSG_BATCH_BEGIN, NFNL_MSG_BATCH_END, NFNL_SUBSYS_NFTABLES,
    NFPROTO_BRIDGE, NFPROTO_INET, NFPROTO_IPV4, NFPROTO_IPV6, NFPROTO_NETDEV, NFPROTO_UNSPEC, NFT_CMP_EQ,
    NFT_CMP_NEQ, NFT_CT_STATE, NFT_META_IIF, NFT_META_IIFNAME, NFT_META_L4PROTO, NFT_META_NFPROTO,
    NFT_META_OIF, NFT_META_PROTOCOL, NFT_MSG_DELTABLE, NFT_MSG_NEWCHAIN, NFT_MSG_NEWRULE, NFT_MSG_NEWTABLE,
    NFT_PAYLOAD_NETWORK_HEADER, NFT_PAYLOAD_TRANSPORT_HEADER, NFT_REG_1, NFT_REG_VERDICT,
};
use crate::netlink::{NLM_F_APPEND, NLM_F_REPLACE, NLM_F_REQUEST, NLMSG_DONE, attributes, messages};
use crate::{Chain, Error, Family, Hook, IPV4, Interface, Match, Rule, Table, Verdict};

/// A rule's place by the handle of another, from `nf_tables.h`.
const NFTA_RULE_POSITION: u16 = 6;
/// A rule's place by the id of another in the same batch.
const NFTA_RULE_POSITION_ID: u16 = 10;

fn unreadable(problem: impl Into<String>) -> Error {
    Error::Unreadable(problem.into())
}

/// The attributes of one level, kinds without their flags.
struct Attributes<'a>(Vec<(u16, &'a [u8])>);

impl<'a> Attributes<'a> {
    fn of(bytes: &'a [u8]) -> Result<Self, Error> {
        attributes(bytes)
            .collect::<Result<_, _>>()
            .map(Self)
            .map_err(Error::Unreadable)
    }

    fn get(&self, kind: u16) -> Option<&'a [u8]> {
        self.0
            .iter()
            .find(|(found, _)| *found == kind)
            .map(|(_, payload)| *payload)
    }

    fn nested(&self, kind: u16) -> Result<Attributes<'a>, Error> {
        Self::of(
            self.get(kind)
                .ok_or_else(|| unreadable(format!("no attribute {kind}")))?,
        )
    }

    fn text(&self, kind: u16) -> Result<&'a str, Error> {
        let bytes = self
            .get(kind)
            .ok_or_else(|| unreadable(format!("no attribute {kind}")))?;
        let text = bytes.strip_suffix(&[0]).unwrap_or(bytes);
        std::str::from_utf8(text)
            .ok()
            .filter(|text| !text.contains('\0'))
            .ok_or_else(|| unreadable("a name that is not text"))
    }

    fn number(&self, kind: u16) -> Result<u32, Error> {
        self.get(kind)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .map(u32::from_be_bytes)
            .ok_or_else(|| unreadable(format!("no number {kind}")))
    }

    /// `NFTA_*_DATA { NFTA_DATA_VALUE }`.
    fn value(&self, kind: u16) -> Result<&'a [u8], Error> {
        self.nested(kind)?
            .get(1)
            .ok_or_else(|| unreadable("data without a value"))
    }
}

/// One expression of a rule.
struct Expression<'a> {
    name: &'a str,
    data: Attributes<'a>,
}

impl Expression<'_> {
    fn is(&self, name: &str) -> bool {
        self.name == name
    }

    /// The register a load writes or a comparison reads must be the one this crate uses.
    fn on_register_1(&self, kind: u16) -> bool {
        self.data.number(kind).ok() == Some(NFT_REG_1)
    }
}

fn family(number: u8) -> Result<Family, Error> {
    match number {
        NFPROTO_INET => Ok(Family::Inet),
        NFPROTO_BRIDGE => Ok(Family::Bridge),
        NFPROTO_NETDEV => Ok(Family::Netdev),
        other => Err(unreadable(format!("a table of family {other}"))),
    }
}

fn verdict(code: u32) -> Result<Verdict, Error> {
    match code {
        NF_ACCEPT => Ok(Verdict::Accept),
        NF_DROP => Ok(Verdict::Drop),
        other => Err(unreadable(format!("the verdict {other}"))),
    }
}

fn hook(family: Family, attributes: &Attributes<'_>) -> Result<Hook, Error> {
    if attributes.number(2)? != 0 {
        return Err(unreadable("a chain at a priority other than 0"));
    }
    match (family, attributes.number(1)?) {
        (Family::Netdev, NF_NETDEV_INGRESS) => {
            // One device, as NFTA_HOOK_DEV, or as the one NFTA_DEVICE_NAME of NFTA_HOOK_DEVS.
            let name = match attributes.get(3) {
                Some(_) => attributes.text(3)?,
                None => {
                    let devices = attributes.nested(4)?;
                    if devices.0.len() != 1 {
                        return Err(unreadable("an ingress chain on more than one device"));
                    }
                    devices.text(1)?
                }
            };
            Ok(Hook::Ingress(
                Interface::new(name).map_err(|error| unreadable(error.to_string()))?,
            ))
        }
        (Family::Inet | Family::Bridge, NF_INET_LOCAL_IN) => Ok(Hook::Input),
        (Family::Inet | Family::Bridge, NF_INET_FORWARD) => Ok(Hook::Forward),
        (Family::Inet | Family::Bridge, NF_INET_LOCAL_OUT) => Ok(Hook::Output),
        (family, number) => Err(unreadable(format!("hook {number} of a {family:?} table"))),
    }
}

/// The value a comparison of register 1 by `operation` holds it against.
fn compared<'a>(expression: Option<&Expression<'a>>, operation: u32) -> Result<&'a [u8], Error> {
    match expression {
        Some(compare)
            if compare.is("cmp") && compare.on_register_1(1) && compare.data.number(2)? == operation =>
        {
            compare.data.value(3)
        }
        _ => Err(unreadable("a load without its comparison")),
    }
}

/// The value a comparison for equality holds register 1 against.
fn equal_to<'a>(expression: Option<&Expression<'a>>) -> Result<&'a [u8], Error> {
    compared(expression, NFT_CMP_EQ)
}

/// The mask of a bitwise expression that ands register 1's 4 bytes with it in place, xor 0.
fn mask_of(bitwise: &Expression<'_>) -> Option<u32> {
    let plain = bitwise.is("bitwise")
        && bitwise.on_register_1(1)
        && bitwise.on_register_1(2)
        && bitwise.data.number(3).ok() == Some(4)
        && bitwise.data.value(5).ok() == Some(&[0; 4][..])
        && bitwise
            .data
            .get(6)
            .is_none_or(|_| bitwise.data.number(6).ok() == Some(0));
    let mask = <[u8; 4]>::try_from(bitwise.data.value(4).ok()?).ok()?;
    plain.then_some(u32::from_be_bytes(mask))
}

/// Whether this expression loads the IPv4 destination.
fn loads_destination(load: &Expression<'_>) -> bool {
    load.is("payload")
        && load.on_register_1(1)
        && [2, 3, 4].map(|kind| load.data.number(kind).ok())
            == [Some(NFT_PAYLOAD_NETWORK_HEADER), Some(16), Some(4)]
}

/// A destination network from the check for IPv4 that starts `expressions`, followed by the load of the IPv4
/// destination: the network and how many expressions it took.
fn destination(expressions: &[Expression<'_>]) -> Result<(Match, usize), Error> {
    if !expressions.get(2).is_some_and(loads_destination) {
        return Err(unreadable("an IPv4 check with no destination after it"));
    }
    let (mask, used) = match expressions.get(3) {
        Some(bitwise) if bitwise.is("bitwise") => match mask_of(bitwise) {
            Some(mask) if mask.leading_ones() + mask.trailing_zeros() == 32 => (mask, 2),
            _ => return Err(unreadable("a destination under a mask that is no prefix")),
        },
        _ => (u32::MAX, 1),
    };
    let value = <[u8; 4]>::try_from(equal_to(expressions.get(2 + used))?)
        .map(u32::from_be_bytes)
        .map_err(|_| unreadable("an address that is not 4 bytes"))?;
    if value & !mask != 0 {
        return Err(unreadable("a network with bits past its prefix"));
    }
    let found = Match::DestIn {
        network: value.to_be_bytes(),
        prefix: mask.leading_ones() as u8,
    };
    Ok((found, 2 + used + 1))
}

/// The condition `expressions` start with, and how many expressions it took.
fn condition(expressions: &[Expression<'_>], family: Family) -> Result<(Match, usize), Error> {
    let first = &expressions[0];
    if first.is("meta") && first.on_register_1(1) {
        let value = equal_to(expressions.get(1))?;
        let loopback = LOOPBACK_INDEX.to_ne_bytes();
        let found = match (first.data.number(2)?, value) {
            (NFT_META_OIF, value) if value == loopback => Match::OutLoopback,
            (NFT_META_IIF, value) if value == loopback => Match::InLoopback,
            (NFT_META_IIFNAME, value) => Match::InInterface(
                Interface::from_kernel(value)
                    .ok_or_else(|| unreadable("a device name no device could have"))?,
            ),
            (NFT_META_NFPROTO, [NFPROTO_IPV6]) if family == Family::Inet => Match::Ipv6,
            (NFT_META_NFPROTO, [NFPROTO_IPV4]) if family == Family::Inet => return destination(expressions),
            (NFT_META_PROTOCOL, value)
                if family != Family::Inet
                    && value == IPV4.to_be_bytes()
                    && expressions.get(2).is_some_and(loads_destination) =>
            {
                return destination(expressions);
            }
            (NFT_META_PROTOCOL, &[high, low]) => Match::EtherType(u16::from_be_bytes([high, low])),
            (NFT_META_L4PROTO, &[protocol]) => Match::Protocol(protocol),
            (key, _) => {
                return Err(unreadable(format!(
                    "a meta match on key {key} this crate does not write"
                )));
            }
        };
        return Ok((found, 2));
    }
    if first.is("payload") && first.on_register_1(1) {
        let place = (
            first.data.number(2)?,
            first.data.number(3)?,
            first.data.number(4)?,
        );
        if place == (NFT_PAYLOAD_TRANSPORT_HEADER, 2, 2)
            && let &[high, low] = equal_to(expressions.get(1))?
        {
            return Ok((Match::DestPort(u16::from_be_bytes([high, low])), 2));
        }
        return Err(unreadable(format!(
            "a load of {place:?} this crate does not write"
        )));
    }
    if first.is("ct") && first.on_register_1(1) && first.data.number(2)? == NFT_CT_STATE {
        // The states are a bit field in the host's byte order; `mask_of` reads masks as addresses, big-endian.
        let states = u32::from_be_bytes(CT_ESTABLISHED_OR_RELATED.to_ne_bytes());
        let masked = expressions.get(1).and_then(mask_of) == Some(states);
        let not_zero = compared(expressions.get(2), NFT_CMP_NEQ).is_ok_and(|value| value == [0; 4]);
        if masked && not_zero {
            return Ok((Match::Established, 3));
        }
    }
    Err(unreadable(format!(
        "a {} expression this crate does not write",
        first.name
    )))
}

fn rule(family: Family, list: &Attributes<'_>) -> Result<Rule, Error> {
    let mut expressions = Vec::new();
    for (kind, element) in &list.0 {
        if *kind != 1 {
            return Err(unreadable("a rule's expressions hold something else"));
        }
        let element = Attributes::of(element)?;
        expressions.push(Expression {
            name: element.text(1)?,
            data: element.nested(2)?,
        });
    }
    let Some((last, conditions)) = expressions.split_last() else {
        return Err(unreadable("a rule without a verdict"));
    };
    if !last.is("immediate") || last.data.number(1)? != NFT_REG_VERDICT {
        return Err(unreadable("a rule that does not end in its verdict"));
    }
    let then = verdict(last.data.nested(2)?.nested(2)?.number(1)?)?;
    let mut when = Vec::new();
    let mut at = 0;
    while at < conditions.len() {
        let (found, used) = condition(&conditions[at..], family)?;
        when.push(found);
        at += used;
    }
    Ok(Rule { when, then })
}

/// The tables these messages leave in a kernel that held none: a batch of [`crate::batch`], or the kernel's listing
/// of its tables, chains and rules, in that order. A table, chain or rule this crate could not have written is an
/// error.
pub fn read(bytes: &[u8]) -> Result<Vec<Table>, Error> {
    let mut tables: Vec<Table> = Vec::new();
    for message in messages(bytes) {
        let message = message.map_err(Error::Unreadable)?;
        if matches!(
            message.kind,
            NFNL_MSG_BATCH_BEGIN | NFNL_MSG_BATCH_END | NLMSG_DONE
        ) {
            continue;
        }
        if message.kind >> 8 != NFNL_SUBSYS_NFTABLES || message.body.len() < 4 {
            return Err(unreadable(format!("a message of kind {:#x}", message.kind)));
        }
        let number = message.body[0];
        let attributes = Attributes::of(&message.body[4..])?;
        let kind = message.kind & 0xff;
        if kind == NFT_MSG_DELTABLE {
            if number != NFPROTO_UNSPEC || attributes.get(1).is_some() {
                return Err(unreadable("the removal of one table"));
            }
            tables.clear();
            continue;
        }
        let family = family(number)?;
        let table_name = attributes.text(1)?;
        let table = tables
            .iter()
            .position(|table| table.family == family && table.name == table_name);
        match (kind, table) {
            // A flag such as dormant (its chains see nothing) is a table this crate does not write.
            (NFT_MSG_NEWTABLE, _) if attributes.get(2).is_some() && attributes.number(2)? != 0 => {
                return Err(unreadable(format!("table {table_name:?} has flags")));
            }
            (NFT_MSG_NEWTABLE, None) => tables.push(Table {
                family,
                name: table_name.to_string(),
                chains: Vec::new(),
            }),
            (NFT_MSG_NEWTABLE, Some(_)) => {}
            (NFT_MSG_NEWCHAIN, Some(table)) => {
                let name = attributes.text(3)?;
                if attributes.text(7)? != FILTER {
                    return Err(unreadable(format!("chain {name:?} is not a filter chain")));
                }
                let hook = hook(
                    family,
                    &attributes
                        .nested(4)
                        .map_err(|_| unreadable(format!("chain {name:?} has no hook")))?,
                )?;
                tables[table].chains.push(Chain {
                    name: name.to_string(),
                    hook,
                    policy: verdict(attributes.number(5)?)?,
                    rules: Vec::new(),
                });
            }
            (NFT_MSG_NEWRULE, Some(table)) => {
                let name = attributes.text(2)?;
                // A request that does not add at the end, after every rule before it, puts the rule elsewhere.
                let request = message.flags & NLM_F_REQUEST != 0;
                let elsewhere = message.flags & NLM_F_APPEND == 0
                    || message.flags & NLM_F_REPLACE != 0
                    || attributes.get(NFTA_RULE_POSITION).is_some()
                    || attributes.get(NFTA_RULE_POSITION_ID).is_some();
                if request && elsewhere {
                    return Err(unreadable(format!(
                        "a rule of chain {name:?} that is not added at its end"
                    )));
                }
                let rule = rule(family, &attributes.nested(4)?)?;
                tables[table]
                    .chains
                    .iter_mut()
                    .find(|chain| chain.name == name)
                    .ok_or_else(|| unreadable(format!("a rule of chain {name:?}, which is not there")))?
                    .rules
                    .push(rule);
            }
            (NFT_MSG_NEWCHAIN | NFT_MSG_NEWRULE, None) => {
                return Err(unreadable(format!(
                    "a chain or rule of table {table_name:?}, which is not there"
                )));
            }
            (kind, _) => return Err(unreadable(format!("an nf_tables message of kind {kind}"))),
        }
    }
    // What `batch` would refuse to write, this crate could not have written.
    crate::encode::check(&tables).map_err(|error| unreadable(error.to_string()))?;
    Ok(tables)
}
