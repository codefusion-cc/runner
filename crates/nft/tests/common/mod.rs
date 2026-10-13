//! What the tests share: shorthands for rules, and a reading of netlink written here, apart from the crate's reader.
// Each test program takes what it needs of this.
#![allow(dead_code)]

use cf_nft::{Interface, Match, Rule, Verdict};

pub fn rule(when: &[Match], then: Verdict) -> Rule {
    Rule {
        when: when.to_vec(),
        then,
    }
}

pub fn device(name: &str) -> Interface {
    Interface::new(name).unwrap()
}

pub fn dest(network: [u8; 4], prefix: u8) -> Match {
    Match::DestIn { network, prefix }
}

/// `(kind, sequence number, what follows the header)` of each message.
pub fn messages(mut bytes: &[u8]) -> Vec<(u16, u32, &[u8])> {
    let mut found = Vec::new();
    while !bytes.is_empty() {
        let length = u32::from_ne_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert!(
            length >= 16 && length <= bytes.len(),
            "a message runs past its end"
        );
        found.push((
            u16::from_ne_bytes([bytes[4], bytes[5]]),
            u32::from_ne_bytes(bytes[8..12].try_into().unwrap()),
            &bytes[16..length],
        ));
        bytes = &bytes[length.next_multiple_of(4).min(bytes.len())..];
    }
    found
}

/// `(kind, payload)` of each attribute, the kind without its flags.
pub fn attributes(mut bytes: &[u8]) -> Vec<(u16, &[u8])> {
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

/// The payload of the first attribute of this kind.
pub fn get<'a>(attributes: &[(u16, &'a [u8])], kind: u16) -> &'a [u8] {
    attributes.iter().find(|(found, _)| *found == kind).unwrap().1
}
