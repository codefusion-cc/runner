//! The netlink form against what `nft` itself sends for the same rules.
//!
//! `nft/<family>.hex` is the one datagram nftables 1.0.6 sent for `nft/<family>.nft`, recorded once on an arm64
//! Linux host (kernel 6.1) inside a fresh user and network namespace:
//! `unshare -Urn sh -c 'ip link set lo up; strace -e trace=sendmsg -e write=all nft -f <family>.nft'`, with the hex
//! columns of strace's dump joined; `nft --debug=netlink` printed the same expressions. Netlink headers are in the
//! host's byte order, so the files hold for little-endian hosts.
//!
//! nft writes its messages its own way: other flags, no acknowledgements asked for, a table flags attribute, chain
//! attributes in another order. What is held equal is what carries meaning: each rule's expressions byte for byte,
//! each chain's attributes one by one, and the tables `read` makes of nft's own bytes.
#![cfg(target_endian = "little")]

use cf_nft::{
    ARP, Chain, Family, Hook, IPV4, IPV6, Interface, Load, Match, Rule, TCP, Table, UDP, Verdict, batch, read,
};
use std::collections::BTreeMap;

fn hex(text: &str) -> Vec<u8> {
    let digits: String = text.split_whitespace().collect();
    (0..digits.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&digits[at..at + 2], 16).unwrap())
        .collect()
}

/// `(kind, payload)` of each attribute, the kind without its flags. Written here, apart from the crate's reader.
fn attributes(mut bytes: &[u8]) -> Vec<(u16, &[u8])> {
    let mut found = Vec::new();
    while !bytes.is_empty() {
        let length = usize::from(u16::from_ne_bytes([bytes[0], bytes[1]]));
        assert!(
            length >= 4 && length <= bytes.len(),
            "an attribute runs past its end"
        );
        found.push((
            u16::from_ne_bytes([bytes[2], bytes[3]]) & 0x3fff,
            &bytes[4..length],
        ));
        bytes = &bytes[length.next_multiple_of(4).min(bytes.len())..];
    }
    found
}

/// `(kind, nfnetlink family, attributes)` of each nf_tables message.
fn messages(mut bytes: &[u8]) -> Vec<(u16, u8, &[u8])> {
    let mut found = Vec::new();
    while !bytes.is_empty() {
        let length = u32::from_ne_bytes(bytes[..4].try_into().unwrap()) as usize;
        let kind = u16::from_ne_bytes([bytes[4], bytes[5]]);
        if kind >> 8 == 10 {
            found.push((kind & 0xff, bytes[16], &bytes[20..length]));
        }
        bytes = &bytes[length.next_multiple_of(4)..];
    }
    found
}

fn get<'a>(attributes: &[(u16, &'a [u8])], kind: u16) -> &'a [u8] {
    attributes.iter().find(|(found, _)| *found == kind).unwrap().1
}

/// Each rule as `(family, chain, its expressions' bytes)`.
fn rules(bytes: &[u8]) -> Vec<(u8, Vec<u8>, Vec<u8>)> {
    messages(bytes)
        .into_iter()
        .filter(|(kind, _, _)| *kind == 6)
        .map(|(_, family, body)| {
            let attributes = attributes(body);
            (family, get(&attributes, 2).to_vec(), get(&attributes, 4).to_vec())
        })
        .collect()
}

/// Attributes by kind.
type ByKind = BTreeMap<u16, Vec<u8>>;

/// Each chain's attributes that carry meaning, and the hook's own attributes.
fn chains(bytes: &[u8]) -> Vec<(u8, ByKind, ByKind)> {
    messages(bytes)
        .into_iter()
        .filter(|(kind, _, _)| *kind == 3)
        .map(|(_, family, body)| {
            let outer = attributes(body);
            let hook = attributes(get(&outer, 4))
                .into_iter()
                .map(|(kind, payload)| (kind, payload.to_vec()))
                .collect();
            let chain = outer
                .into_iter()
                .filter(|(kind, _)| [1, 3, 5, 7].contains(kind))
                .map(|(kind, payload)| (kind, payload.to_vec()))
                .collect();
            (family, chain, hook)
        })
        .collect()
}

fn rule(when: &[Match], then: Verdict) -> Rule {
    Rule {
        when: when.to_vec(),
        then,
    }
}

fn device(name: &str) -> Interface {
    Interface::new(name).unwrap()
}

fn dest(network: [u8; 4], prefix: u8) -> Match {
    Match::DestIn { network, prefix }
}

/// `nft/inet.nft`: the guest's kinds of rules.
fn inet() -> Table {
    let server = dest([192, 168, 64, 1], 32);
    Table {
        family: Family::Inet,
        name: "codefusion".into(),
        chains: vec![
            Chain {
                name: "output".into(),
                hook: Hook::Output,
                policy: Verdict::Accept,
                rules: vec![
                    rule(&[Match::OutLoopback], Verdict::Accept),
                    rule(&[Match::Ipv6], Verdict::Drop),
                    rule(
                        &[server, Match::Protocol(UDP), Match::DestPort(53)],
                        Verdict::Accept,
                    ),
                    rule(
                        &[server, Match::Protocol(TCP), Match::DestPort(53)],
                        Verdict::Accept,
                    ),
                    rule(&[dest([172, 16, 0, 0], 12)], Verdict::Drop),
                    rule(&[dest([100, 64, 0, 0], 10)], Verdict::Drop),
                    rule(&[server], Verdict::Drop),
                    rule(&[Match::EtherType(ARP)], Verdict::Drop),
                ],
            },
            Chain {
                name: "input".into(),
                hook: Hook::Input,
                policy: Verdict::Drop,
                rules: vec![
                    rule(&[Match::InLoopback], Verdict::Accept),
                    rule(&[Match::Established], Verdict::Accept),
                    rule(&[Match::InInterface(device("eth0"))], Verdict::Accept),
                ],
            },
        ],
    }
}

/// `nft/bridge.nft`: a firewall in front of a machine's tap.
fn bridge() -> Table {
    let tap = Match::InInterface(device("tap0"));
    Table {
        family: Family::Bridge,
        name: "codefusion".into(),
        chains: vec![
            Chain {
                name: "forward".into(),
                hook: Hook::Forward,
                policy: Verdict::Accept,
                rules: vec![
                    rule(&[tap, Match::EtherType(IPV6)], Verdict::Drop),
                    rule(&[tap, Match::EtherType(ARP)], Verdict::Accept),
                    rule(&[tap, dest([172, 16, 0, 0], 12)], Verdict::Drop),
                    rule(
                        &[
                            tap,
                            dest([192, 0, 2, 1], 32),
                            Match::Protocol(UDP),
                            Match::DestPort(53),
                        ],
                        Verdict::Accept,
                    ),
                    rule(&[tap, Match::Protocol(UDP), Match::DestPort(67)], Verdict::Accept),
                    rule(&[dest([100, 64, 0, 0], 10)], Verdict::Drop),
                    rule(&[dest([203, 0, 113, 7], 32)], Verdict::Drop),
                    rule(&[Match::Protocol(TCP), Match::DestPort(53)], Verdict::Accept),
                    rule(&[Match::Protocol(UDP)], Verdict::Accept),
                    rule(&[Match::Established], Verdict::Accept),
                ],
            },
            Chain {
                name: "output".into(),
                hook: Hook::Output,
                policy: Verdict::Drop,
                rules: vec![rule(&[Match::OutLoopback], Verdict::Accept)],
            },
        ],
    }
}

/// `nft/netdev.nft`: an allowlist on a device's ingress.
fn netdev() -> Table {
    Table {
        family: Family::Netdev,
        name: "codefusion".into(),
        chains: vec![Chain {
            name: "ingress".into(),
            hook: Hook::Ingress(device("lo")),
            policy: Verdict::Drop,
            rules: vec![
                rule(&[Match::EtherType(ARP)], Verdict::Accept),
                rule(&[Match::EtherType(IPV6)], Verdict::Drop),
                rule(&[dest([172, 16, 0, 0], 12)], Verdict::Drop),
                rule(
                    &[
                        dest([192, 0, 2, 1], 32),
                        Match::Protocol(UDP),
                        Match::DestPort(53),
                    ],
                    Verdict::Accept,
                ),
                rule(&[Match::Protocol(UDP), Match::DestPort(67)], Verdict::Accept),
                rule(
                    &[
                        Match::InInterface(device("lo")),
                        Match::Protocol(TCP),
                        Match::DestPort(443),
                    ],
                    Verdict::Accept,
                ),
                rule(&[Match::EtherType(IPV4)], Verdict::Accept),
            ],
        }],
    }
}

fn cases() -> [(&'static str, Vec<u8>, Table); 3] {
    [
        ("inet", hex(include_str!("nft/inet.hex")), inet()),
        ("bridge", hex(include_str!("nft/bridge.hex")), bridge()),
        ("netdev", hex(include_str!("nft/netdev.hex")), netdev()),
    ]
}

#[test]
fn every_rule_is_the_expressions_nft_writes_for_it() {
    for (name, recorded, table) in cases() {
        let ours = batch(&[table], Load::Replace).unwrap();
        let (theirs, ours) = (rules(&recorded), rules(&ours.bytes));
        assert_eq!(ours.len(), theirs.len(), "{name}");
        for (index, (ours, theirs)) in ours.iter().zip(&theirs).enumerate() {
            assert_eq!(ours, theirs, "rule {index} of {name}");
        }
    }
}

#[test]
fn every_chain_and_table_says_what_nft_says() {
    for (name, recorded, table) in cases() {
        let ours = batch(&[table], Load::Replace).unwrap();
        assert_eq!(chains(&ours.bytes), chains(&recorded), "{name}");
        // The table by family and name; nft adds its flags, 0.
        let tables = |bytes| {
            messages(bytes)
                .into_iter()
                .filter(|(kind, _, _)| *kind == 0)
                .map(|(_, family, body)| (family, get(&attributes(body), 1).to_vec()))
                .collect::<Vec<_>>()
        };
        assert_eq!(tables(&ours.bytes), tables(&recorded), "{name}");
        // `flush ruleset`: a removal with no family and no name, first.
        let removal = |bytes| messages(bytes).into_iter().find(|(kind, _, _)| *kind == 2);
        assert_eq!(removal(&ours.bytes), Some((2, 0, &[][..])), "{name}");
        assert_eq!(removal(&ours.bytes), removal(&recorded), "{name}");
    }
}

#[test]
fn nfts_own_messages_read_as_the_same_tables() {
    for (name, recorded, table) in cases() {
        assert_eq!(read(&recorded).unwrap(), [table], "{name}");
    }
}
