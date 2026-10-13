//! What rules do with packets, which tables the encoder refuses, and that the netlink form means what `decide` says.

mod common;

use cf_nft::{
    ARP, Chain, Error, Family, Hook, IPV4, IPV6, Interface, Load, Match, Packet, Rule, TCP, Table, UDP,
    Verdict, batch, decide, network, read,
};
use common::{device, messages, rule};
use proptest::prelude::*;

const TAP: &str = "tap0";

fn packet(ether_type: u16, dest: [u8; 4], protocol: u8, dest_port: u16) -> Packet {
    Packet {
        ether_type,
        in_interface: Some(device(TAP)),
        loopback: false,
        dest,
        protocol,
        dest_port,
        established: false,
    }
}

/// The kind of firewall the host puts in front of a machine's tap: no IPv6, ARP, DNS at one address, DHCP, and
/// none of the private ranges.
fn in_front_of_a_tap() -> Vec<Rule> {
    let tap = Match::InInterface(device(TAP));
    let mut rules = vec![
        rule(&[tap, Match::EtherType(IPV6)], Verdict::Drop),
        rule(&[tap, Match::EtherType(ARP)], Verdict::Accept),
        rule(
            &[
                tap,
                Match::DestIn {
                    network: [10, 0, 2, 3],
                    prefix: 32,
                },
                Match::Protocol(UDP),
                Match::DestPort(53),
            ],
            Verdict::Accept,
        ),
        rule(&[tap, Match::Protocol(UDP), Match::DestPort(67)], Verdict::Accept),
    ];
    for (network, prefix) in [([10, 0, 0, 0], 8), ([127, 0, 0, 0], 8), ([224, 0, 0, 0], 4)] {
        rules.push(rule(&[tap, Match::DestIn { network, prefix }], Verdict::Drop));
    }
    rules
}

#[test]
fn a_tap_reaches_the_internet_arp_dns_and_dhcp_and_nothing_private() {
    let rules = in_front_of_a_tap();
    let verdict = |packet: Packet| decide(&rules, Verdict::Accept, &packet);
    assert_eq!(
        verdict(packet(IPV4, [140, 82, 121, 4], TCP, 443)),
        Verdict::Accept
    );
    assert_eq!(verdict(packet(ARP, [10, 0, 2, 2], 0, 0)), Verdict::Accept);
    assert_eq!(verdict(packet(IPV4, [10, 0, 2, 3], UDP, 53)), Verdict::Accept);
    assert_eq!(
        verdict(packet(IPV4, [255, 255, 255, 255], UDP, 67)),
        Verdict::Accept
    );
    for blocked in [
        packet(IPV6, [140, 82, 121, 4], TCP, 443),
        packet(IPV4, [10, 0, 2, 3], TCP, 22),
        packet(IPV4, [10, 0, 2, 3], UDP, 54),
        packet(IPV4, [10, 255, 255, 255], UDP, 53),
        packet(IPV4, [127, 0, 0, 1], TCP, 80),
        packet(IPV4, [239, 255, 255, 250], UDP, 1900),
    ] {
        assert_eq!(verdict(blocked), Verdict::Drop, "{blocked:?}");
    }
    // What comes in through another device is not this tap's.
    let mut other = packet(IPV4, [10, 0, 0, 1], TCP, 22);
    other.in_interface = Some(device("tap1"));
    assert_eq!(verdict(other), Verdict::Accept);
}

#[test]
fn an_address_and_a_port_are_read_only_where_they_exist() {
    let rules = [
        rule(
            &[Match::DestIn {
                network: [10, 0, 0, 0],
                prefix: 8,
            }],
            Verdict::Drop,
        ),
        rule(&[Match::Protocol(UDP)], Verdict::Drop),
        rule(&[Match::Protocol(TCP), Match::DestPort(22)], Verdict::Drop),
    ];
    // ARP, IPv6 and a VLAN tag carry no IPv4 destination, whatever their bytes there say.
    for ether_type in [ARP, IPV6, 0x8100, 0] {
        assert_eq!(
            decide(
                &rules[..1],
                Verdict::Accept,
                &packet(ether_type, [10, 1, 1, 1], TCP, 22)
            ),
            Verdict::Accept,
            "{ether_type:#x}"
        );
    }
    // A transport protocol and a port are IP's only.
    for ether_type in [ARP, 0x8100] {
        let other = packet(ether_type, [10, 1, 1, 1], UDP, 22);
        assert_eq!(decide(&rules[1..], Verdict::Accept, &other), Verdict::Accept);
        let other = packet(ether_type, [10, 1, 1, 1], TCP, 22);
        assert_eq!(decide(&rules[1..], Verdict::Accept, &other), Verdict::Accept);
    }
    assert_eq!(
        decide(&rules[1..], Verdict::Accept, &packet(IPV6, [0; 4], UDP, 1)),
        Verdict::Drop
    );
    assert_eq!(
        decide(&rules[1..], Verdict::Accept, &packet(IPV6, [0; 4], TCP, 22)),
        Verdict::Drop
    );
    // A network compares under its prefix, whatever bits past it say; 0 is every IPv4 address.
    for (network, prefix, inside, outside) in [
        ([10, 9, 9, 9], 8, [10, 255, 0, 1], [11, 0, 0, 0]),
        ([172, 31, 0, 0], 12, [172, 16, 0, 0], [172, 32, 0, 0]),
        ([1, 2, 3, 4], 32, [1, 2, 3, 4], [1, 2, 3, 5]),
        ([224, 1, 1, 1], 4, [239, 255, 255, 255], [240, 0, 0, 0]),
    ] {
        let rules = [rule(&[Match::DestIn { network, prefix }], Verdict::Drop)];
        assert_eq!(
            decide(&rules, Verdict::Accept, &packet(IPV4, inside, 6, 1)),
            Verdict::Drop
        );
        assert_eq!(
            decide(&rules, Verdict::Accept, &packet(IPV4, outside, 6, 1)),
            Verdict::Accept
        );
    }
    let everything = [rule(
        &[Match::DestIn {
            network: [9, 9, 9, 9],
            prefix: 0,
        }],
        Verdict::Drop,
    )];
    assert_eq!(
        decide(&everything, Verdict::Accept, &packet(IPV4, [1, 1, 1, 1], 6, 1)),
        Verdict::Drop
    );
    assert_eq!(network([10, 9, 9, 9], 8), [10, 0, 0, 0]);
    assert_eq!(network([10, 9, 9, 9], 33), [10, 9, 9, 9]);
    assert_eq!(network([10, 9, 9, 9], 0), [0; 4]);
}

#[test]
fn a_device_name_is_one_the_kernel_could_give() {
    for good in [
        "lo",
        "tap0",
        "a",
        "abcdefghijklmno",
        "tâp0",
        "wlP2p33s0",
        "veth-1_x.y",
    ] {
        assert_eq!(Interface::new(good).unwrap().name(), good);
    }
    // 15 bytes is the most, counted in bytes, not characters.
    assert!(Interface::new(&"é".repeat(7)).is_ok());
    for bad in [
        "",
        ".",
        "..",
        "abcdefghijklmnop",
        "a/b",
        "a:b",
        "a b",
        "a\tb",
        "a\nb",
        "a\0b",
        &"é".repeat(8),
        // The kernel's isspace also takes the vertical tab and 0xa0, which "à" and a no-break space hold.
        "a\x0bb",
        "tà0",
        "a\u{a0}b",
    ] {
        assert!(matches!(Interface::new(bad), Err(Error::Interface(_))), "{bad:?}");
    }
    assert_eq!(format!("{:?}", device("tap0")), r#"Interface("tap0")"#);
}

fn table(family: Family, hook: Hook, rules: Vec<Rule>) -> Table {
    Table {
        family,
        name: "t".into(),
        chains: vec![Chain {
            name: "c".into(),
            hook,
            policy: Verdict::Accept,
            rules,
        }],
    }
}

fn refusal(table: Table) -> String {
    match batch(&[table], Load::Replace) {
        Ok(_) => "written".into(),
        Err(error) => error.to_string(),
    }
}

#[test]
fn a_rule_that_could_not_do_what_it_says_is_refused() {
    let ingress = Hook::Ingress(device("lo"));
    for (family, hook, when, problem) in [
        (
            Family::Inet,
            Hook::Output,
            vec![Match::DestPort(53)],
            "a port without TCP or UDP before it",
        ),
        (
            Family::Inet,
            Hook::Output,
            vec![Match::Protocol(1), Match::DestPort(53)],
            "a port without TCP or UDP",
        ),
        (
            Family::Inet,
            Hook::Output,
            vec![Match::DestPort(53), Match::Protocol(UDP)],
            "a port without TCP or UDP",
        ),
        (
            Family::Bridge,
            Hook::Forward,
            vec![Match::Ipv6],
            "only an inet table knows IPv6 by its family",
        ),
        (
            Family::Netdev,
            ingress,
            vec![Match::Ipv6],
            "only an inet table knows IPv6 by its family",
        ),
        (
            Family::Inet,
            Hook::Input,
            vec![Match::OutLoopback],
            "nothing has left",
        ),
        (
            Family::Netdev,
            ingress,
            vec![Match::OutLoopback],
            "nothing has left",
        ),
        (
            Family::Inet,
            Hook::Output,
            vec![Match::InLoopback],
            "nothing came in",
        ),
        (
            Family::Bridge,
            Hook::Output,
            vec![Match::InInterface(device(TAP))],
            "nothing came in",
        ),
        (
            Family::Netdev,
            ingress,
            vec![Match::Established],
            "before connections are tracked",
        ),
        (
            Family::Inet,
            Hook::Output,
            vec![Match::DestIn {
                network: [0; 4],
                prefix: 33,
            }],
            "a prefix longer than 32 bits",
        ),
    ] {
        let rules = vec![rule(&[], Verdict::Accept), rule(&when, Verdict::Drop)];
        let refused = refusal(table(family, hook, rules));
        assert!(
            refused.contains(problem),
            "{family:?} {hook:?} {when:?}: {refused}"
        );
        assert!(refused.starts_with("rule 1 of t/c: "), "{refused}");
    }
    // What is fine where it is.
    for (family, hook, when) in [
        (
            Family::Inet,
            Hook::Output,
            vec![Match::Protocol(TCP), Match::DestPort(22)],
        ),
        (
            Family::Inet,
            Hook::Output,
            vec![Match::Protocol(UDP), Match::EtherType(IPV4), Match::DestPort(22)],
        ),
        (
            Family::Bridge,
            Hook::Forward,
            vec![Match::InInterface(device(TAP)), Match::OutLoopback],
        ),
        (
            Family::Bridge,
            Hook::Input,
            vec![Match::InLoopback, Match::Established],
        ),
        (
            Family::Netdev,
            ingress,
            vec![Match::InInterface(device("lo")), Match::EtherType(ARP)],
        ),
        (
            Family::Inet,
            Hook::Forward,
            vec![Match::Ipv6, Match::InLoopback, Match::OutLoopback],
        ),
    ] {
        assert_eq!(
            refusal(table(family, hook, vec![rule(&when, Verdict::Drop)])),
            "written",
            "{when:?}"
        );
    }
}

#[test]
fn a_table_the_kernel_would_refuse_is_refused_before_it() {
    let empty = |family, hook| table(family, hook, Vec::new());
    for (family, hook) in [
        (Family::Inet, Hook::Ingress(device("lo"))),
        (Family::Bridge, Hook::Ingress(device("lo"))),
        (Family::Netdev, Hook::Input),
        (Family::Netdev, Hook::Forward),
        (Family::Netdev, Hook::Output),
    ] {
        assert!(
            refusal(empty(family, hook)).contains("has no"),
            "{family:?} {hook:?}"
        );
    }
    let named = |table_name: &str, chain_name: &str| {
        let mut table = empty(Family::Inet, Hook::Input);
        table.name = table_name.into();
        table.chains[0].name = chain_name.into();
        refusal(table)
    };
    assert_eq!(named(&"t".repeat(255), &"c".repeat(255)), "written");
    for (table_name, chain_name) in [
        ("", "c"),
        ("t", ""),
        ("t\0", "c"),
        ("t", "c\0d"),
        (&*"t".repeat(256), "c"),
        ("t", &*"c".repeat(256)),
    ] {
        assert!(
            named(table_name, chain_name).contains("cannot be called"),
            "{table_name:?} {chain_name:?}"
        );
    }
    let mut twice = empty(Family::Inet, Hook::Input);
    twice.chains.push(twice.chains[0].clone());
    assert!(refusal(twice).contains("twice"));
    let one = empty(Family::Inet, Hook::Input);
    assert!(matches!(
        batch(&[one.clone(), one.clone()], Load::Add),
        Err(Error::Table(_))
    ));
    // The same name in another family is another table.
    let other = empty(Family::Bridge, Hook::Forward);
    assert!(batch(&[one, other], Load::Add).is_ok());
}

#[test]
fn a_batch_is_one_transaction_with_an_answer_for_each_change() {
    let tables = [
        table(Family::Bridge, Hook::Forward, in_front_of_a_tap()),
        table(
            Family::Netdev,
            Hook::Ingress(device(TAP)),
            vec![rule(&[], Verdict::Accept)],
        ),
    ];
    let kinds_of = |bytes: &[u8]| {
        messages(bytes)
            .into_iter()
            .map(|(kind, sequence, _)| (kind, sequence))
            .collect::<Vec<_>>()
    };
    for (load, removal) in [(Load::Add, 0), (Load::Replace, 1)] {
        let written = batch(&tables, load).unwrap();
        let kinds = kinds_of(&written.bytes);
        let rules = in_front_of_a_tap().len() + 1;
        // Begin, the removal, two tables with a chain each, the rules, end; numbered from 1 without a gap.
        assert_eq!(kinds.len(), 1 + removal + 2 * 2 + rules + 1);
        assert_eq!((kinds[0].0, kinds.last().unwrap().0), (0x10, 0x11));
        assert_eq!(kinds.iter().filter(|(kind, _)| *kind == 0x0a02).count(), removal);
        assert_eq!(written.acknowledged as usize, kinds.len() - 2);
        assert!(
            kinds
                .iter()
                .enumerate()
                .all(|(index, (_, sequence))| *sequence as usize == index + 1)
        );
        assert_eq!(read(&written.bytes).unwrap(), tables);
    }
    // A rule's expressions are one attribute of at most 65535 bytes: the most conditions that fit, each the longest
    // there is, still read back, and one more is refused instead of written with a length that wrapped. A rule of 256
    // such conditions is well inside it.
    let longest = |conditions: usize| {
        let when = vec![
            Match::DestIn {
                network: [10, 0, 0, 0],
                prefix: 8
            };
            conditions
        ];
        table(Family::Inet, Hook::Output, vec![rule(&when, Verdict::Drop)])
    };
    let most = (256..400)
        .find(|conditions| batch(&[longest(conditions + 1)], Load::Add).is_err())
        .expect("a rule too long for netlink was written");
    let written = batch(&[longest(most)], Load::Add).unwrap();
    assert_eq!(read(&written.bytes).unwrap(), [longest(most)]);
    assert!(matches!(
        batch(&[longest(most + 1)], Load::Add),
        Err(Error::Rule { rule: 0, .. })
    ));
    // No tables at all is a transaction that changes nothing, or removes everything.
    assert_eq!(batch(&[], Load::Add).unwrap().acknowledged, 0);
    assert_eq!(batch(&[], Load::Replace).unwrap().acknowledged, 1);
    assert_eq!(read(&batch(&[], Load::Replace).unwrap().bytes).unwrap(), []);
}

#[test]
fn a_network_reads_back_as_its_prefix_and_rules_written_elsewhere_are_not_read() {
    let written = |when: Match| {
        batch(
            &[table(
                Family::Inet,
                Hook::Output,
                vec![rule(&[when], Verdict::Drop)],
            )],
            Load::Add,
        )
        .unwrap()
    };
    let unmasked = written(Match::DestIn {
        network: [10, 9, 9, 9],
        prefix: 8,
    });
    assert_eq!(
        read(&unmasked.bytes).unwrap()[0].chains[0].rules[0].when,
        [Match::DestIn {
            network: [10, 0, 0, 0],
            prefix: 8
        }]
    );
    // Every byte of a batch cut short or changed is an answer, never a panic.
    let bytes = written(Match::Established).bytes;
    for cut in 0..bytes.len() {
        let _ = read(&bytes[..cut]);
    }
    for at in 0..bytes.len() {
        for value in [0, 1, 0x7f, 0xff] {
            let mut changed = bytes.clone();
            changed[at] = value;
            let _ = read(&changed);
        }
    }
    // A message of another subsystem, and a removal of one table, are not this crate's.
    let mut foreign = Vec::new();
    cf_netlink::message(&mut foreign, 0x0b00, 1, 1, &[0, 0, 0, 0]);
    assert!(matches!(read(&foreign), Err(Error::Unreadable(_))));
    let mut one_table = Vec::new();
    let mut body = vec![1, 0, 0, 0];
    cf_netlink::attribute(&mut body, 1, b"t\0");
    cf_netlink::message(&mut one_table, 0x0a02, 1, 1, &body);
    assert!(matches!(read(&one_table), Err(Error::Unreadable(_))));
}

/// The place of a byte pattern in `bytes`, which holds it once.
fn place_of(bytes: &[u8], pattern: &[u8]) -> usize {
    let mut places = (0..bytes.len()).filter(|&at| bytes[at..].starts_with(pattern));
    let at = places.next().unwrap();
    assert_eq!(places.next(), None, "{pattern:?} more than once");
    at
}

#[test]
fn what_the_crate_does_not_write_is_not_read_as_its_own() {
    // Tables `batch` would refuse to write are refused when read: a chain twice, and a rule moved to a hook where it
    // could not do what it says (an output chain's loopback rule, its chain now on the input hook).
    let looped = table(
        Family::Inet,
        Hook::Output,
        vec![rule(&[Match::OutLoopback], Verdict::Accept)],
    );
    let once = batch(std::slice::from_ref(&looped), Load::Add).unwrap().bytes;
    assert_eq!(read(&once).unwrap(), [looped]);
    assert!(matches!(
        read(&[once.clone(), once.clone()].concat()),
        Err(Error::Unreadable(_))
    ));
    // NFTA_HOOK_HOOKNUM, 3: the output hook.
    let output_hook = [&8u16.to_ne_bytes()[..], &1u16.to_ne_bytes(), &3u32.to_be_bytes()].concat();
    let mut moved = once;
    let at = moved
        .windows(output_hook.len())
        .position(|window| window == output_hook)
        .unwrap();
    moved[at + 7] = 1;
    assert!(matches!(read(&moved), Err(Error::Unreadable(_))));
    // A dormant table holds rules its chains never run: not the firewall it looks like.
    let table_with_flags = |flags: u32| {
        let mut body = vec![1, 0, 0, 0];
        cf_netlink::attribute(&mut body, 1, b"t\0");
        cf_netlink::attribute(&mut body, 2, &flags.to_be_bytes());
        let mut bytes = Vec::new();
        cf_netlink::message(&mut bytes, 0x0a00, 0x405, 1, &body);
        bytes
    };
    assert_eq!(read(&table_with_flags(0)).unwrap().len(), 1, "nft says flags 0");
    for flags in [1, 2] {
        assert!(
            matches!(read(&table_with_flags(flags)), Err(Error::Unreadable(_))),
            "{flags}"
        );
    }

    // "ct state established,related" on another register is another rule. The number after each name's
    // NFTA_EXPR_DATA is the register: the comparison's source, the bitwise's source and destination.
    let established = batch(
        &[table(
            Family::Inet,
            Hook::Input,
            vec![rule(&[Match::Established], Verdict::Accept)],
        )],
        Load::Add,
    )
    .unwrap()
    .bytes;
    assert_eq!(
        read(&established).unwrap()[0].chains[0].rules[0].when,
        [Match::Established]
    );
    let cmp = place_of(&established, b"cmp\0");
    let bitwise = place_of(&established, b"bitwise\0");
    for register in [cmp + 4 + 4 + 7, bitwise + 8 + 4 + 7, bitwise + 8 + 4 + 8 + 7] {
        let mut changed = established.clone();
        assert_eq!(changed[register], 1);
        changed[register] = 2;
        assert!(matches!(read(&changed), Err(Error::Unreadable(_))), "{register}");
    }

    // A rule a batch puts first, or in another rule's place, is not the last rule of its chain.
    let two = batch(
        &[table(
            Family::Inet,
            Hook::Output,
            vec![
                rule(&[Match::Protocol(TCP)], Verdict::Drop),
                rule(&[], Verdict::Accept),
            ],
        )],
        Load::Add,
    )
    .unwrap()
    .bytes;
    let mut at = 0;
    let mut rules = Vec::new();
    while at < two.len() {
        if u16::from_ne_bytes([two[at + 4], two[at + 5]]) == 0x0a06 {
            rules.push(at);
        }
        at += (u32::from_ne_bytes(two[at..at + 4].try_into().unwrap()) as usize).next_multiple_of(4);
    }
    let flags = |bytes: &[u8], at: usize| u16::from_ne_bytes([bytes[at + 6], bytes[at + 7]]);
    for change in [|flags: u16| flags & !0x800, |flags: u16| flags | 0x100] {
        let mut changed = two.clone();
        let new = change(flags(&changed, rules[1])).to_ne_bytes();
        changed[rules[1] + 6..rules[1] + 8].copy_from_slice(&new);
        assert!(matches!(read(&changed), Err(Error::Unreadable(_))));
    }
}

/// The netlink form run as the kernel runs it, for one packet: a test's own reading of the expressions, apart from
/// the crate's reader, with the packet's bytes laid out as the kernel would find them.
mod kernel_model {
    use super::*;
    use crate::common::{attributes, get};

    fn number(attributes: &[(u16, &[u8])], kind: u16) -> u32 {
        u32::from_be_bytes(get(attributes, kind).try_into().unwrap())
    }

    fn value<'a>(attributes: &[(u16, &'a [u8])], kind: u16) -> &'a [u8] {
        get(&self::attributes(get(attributes, kind)), 1)
    }

    /// What a load finds for this packet, or `None` where the kernel stops the rule (no such header, no such
    /// device).
    fn load(
        name: &str,
        data: &[(u16, &[u8])],
        family: Family,
        hook: Hook,
        packet: &Packet,
    ) -> Option<Vec<u8>> {
        let ip = packet.ether_type == IPV4 || packet.ether_type == IPV6;
        let receives = hook != Hook::Output;
        let sends = matches!(hook, Hook::Output | Hook::Forward);
        let index = |loopback: bool, other: u32| if loopback { 1u32 } else { other }.to_ne_bytes().to_vec();
        match name {
            "meta" => match number(data, 2) {
                1 => Some(packet.ether_type.to_be_bytes().to_vec()),
                4 => receives.then(|| index(packet.loopback, 7)),
                5 => sends.then(|| index(packet.loopback, 8)),
                6 => receives.then(|| {
                    let mut name = [0u8; 16];
                    let given = packet.in_interface.as_ref().map_or("", Interface::name);
                    name[..given.len()].copy_from_slice(given.as_bytes());
                    name.to_vec()
                }),
                15 => Some(vec![match family {
                    Family::Inet if packet.ether_type == IPV6 => 10,
                    Family::Inet => 2,
                    Family::Bridge => 7,
                    Family::Netdev => 5,
                }]),
                16 => ip.then(|| vec![packet.protocol]),
                key => panic!("meta key {key}"),
            },
            "payload" => match (number(data, 2), number(data, 3), number(data, 4)) {
                // Whatever the network protocol, its header has the address's bytes at the IPv4 destination's
                // place, so a missing check of the protocol shows.
                (1, 16, 4) => Some(packet.dest.to_vec()),
                (2, 2, 2) => ip.then(|| packet.dest_port.to_be_bytes().to_vec()),
                place => panic!("payload {place:?}"),
            },
            "ct" => {
                assert_eq!(number(data, 2), 0, "ct state");
                // Established is bit 1, new bit 3; what is not IP has no connection: invalid, bit 0.
                let state: u32 = if !ip {
                    1
                } else if packet.established {
                    2
                } else {
                    8
                };
                Some(state.to_ne_bytes().to_vec())
            }
            other => panic!("load {other}"),
        }
    }

    /// The verdict of the chain called `chain` in the batch for this packet.
    pub fn run(bytes: &[u8], chain: &str, family: Family, hook: Hook, packet: &Packet) -> Verdict {
        let verdict = |code: u32| match code {
            0 => Verdict::Drop,
            1 => Verdict::Accept,
            other => panic!("verdict {other}"),
        };
        let mut policy = None;
        for (kind, _, body) in messages(bytes) {
            if body.len() < 4 {
                continue;
            }
            let message = attributes(&body[4..]);
            if kind == 0x0a03 && get(&message, 3) == [chain.as_bytes(), b"\0"].concat() {
                policy = Some(verdict(number(&message, 5)));
            }
            if kind != 0x0a06 || get(&message, 2) != [chain.as_bytes(), b"\0"].concat() {
                continue;
            }
            let mut register = [0u8; 16];
            let mut taken = None;
            for (_, element) in attributes(get(&message, 4)) {
                let element = attributes(element);
                let name = std::str::from_utf8(get(&element, 1))
                    .unwrap()
                    .trim_end_matches('\0');
                let data = attributes(get(&element, 2));
                match name {
                    "cmp" => {
                        let against = value(&data, 3);
                        let same = register[..against.len()] == *against;
                        if same != (number(&data, 2) == 0) {
                            break;
                        }
                    }
                    "bitwise" => {
                        let (mask, xor) = (value(&data, 4), value(&data, 5));
                        for at in 0..number(&data, 3) as usize {
                            register[at] = (register[at] & mask[at]) ^ xor[at];
                        }
                    }
                    "immediate" => {
                        let code = attributes(get(&attributes(get(&data, 2)), 2));
                        taken = Some(verdict(number(&code, 1)));
                    }
                    load_name => match load(load_name, &data, family, hook, packet) {
                        Some(found) => {
                            register = [0; 16];
                            register[..found.len()].copy_from_slice(&found);
                        }
                        None => break,
                    },
                }
            }
            if let Some(verdict) = taken {
                return verdict;
            }
        }
        policy.expect("the chain is in the batch")
    }
}

fn devices() -> impl Strategy<Value = Interface> {
    prop::sample::select(vec!["lo", TAP, "eth0", "tap1"]).prop_map(device)
}

fn addresses() -> impl Strategy<Value = [u8; 4]> {
    prop_oneof![
        prop::sample::select(vec![
            [10, 1, 2, 3],
            [172, 16, 5, 4],
            [192, 0, 2, 1],
            [8, 8, 8, 8],
            [100, 64, 0, 1]
        ]),
        any::<[u8; 4]>(),
    ]
}

fn ports() -> impl Strategy<Value = u16> {
    prop_oneof![prop::sample::select(vec![53, 67, 443]), any::<u16>()]
}

fn ether_types() -> impl Strategy<Value = u16> {
    prop_oneof![prop::sample::select(vec![IPV4, IPV6, ARP, 0x8100]), any::<u16>()]
}

fn protocols() -> impl Strategy<Value = u8> {
    prop_oneof![prop::sample::select(vec![1, TCP, UDP]), any::<u8>()]
}

/// A family and a hook it has.
fn places() -> impl Strategy<Value = (Family, Hook)> {
    prop_oneof![
        prop::sample::select(vec![Hook::Input, Hook::Forward, Hook::Output])
            .prop_map(|hook| (Family::Inet, hook)),
        prop::sample::select(vec![Hook::Input, Hook::Forward, Hook::Output])
            .prop_map(|hook| (Family::Bridge, hook)),
        devices().prop_map(|device| (Family::Netdev, Hook::Ingress(device))),
    ]
}

/// A rule that may stand in this place, its networks given as their prefix keeps them.
fn rules_for(family: Family, hook: Hook) -> impl Strategy<Value = Rule> {
    let receives = hook != Hook::Output;
    let sends = matches!(hook, Hook::Output | Hook::Forward);
    (
        (
            prop::option::of(devices()),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
        ),
        (
            prop::option::of(ether_types()),
            prop::option::of((addresses(), 0..=32u8)),
        ),
        (
            prop::option::of((protocols(), prop::option::of(ports()))),
            any::<bool>(),
            any::<bool>(),
        ),
    )
        .prop_map(
            move |(
                (device, in_loopback, out_loopback, ipv6),
                (ether_type, dest),
                (transport, established, accept),
            )| {
                let mut when = Vec::new();
                if receives {
                    when.extend(device.map(Match::InInterface));
                    when.extend(in_loopback.then_some(Match::InLoopback));
                }
                when.extend((sends && out_loopback).then_some(Match::OutLoopback));
                when.extend((family == Family::Inet && ipv6).then_some(Match::Ipv6));
                when.extend(ether_type.map(Match::EtherType));
                when.extend(dest.map(|(address, prefix)| Match::DestIn {
                    network: network(address, prefix),
                    prefix,
                }));
                if let Some((protocol, port)) = transport {
                    when.push(Match::Protocol(protocol));
                    if protocol == TCP || protocol == UDP {
                        when.extend(port.map(Match::DestPort));
                    }
                }
                when.extend((family != Family::Netdev && established).then_some(Match::Established));
                Rule {
                    when,
                    then: if accept { Verdict::Accept } else { Verdict::Drop },
                }
            },
        )
}

/// A chain in some place, with up to 8 rules.
fn chains() -> impl Strategy<Value = (Family, Chain)> {
    places().prop_flat_map(|(family, hook)| {
        (
            prop::collection::vec(rules_for(family, hook), 0..8),
            any::<bool>(),
        )
            .prop_map(move |(rules, accept)| {
                let chain = Chain {
                    name: "chain".into(),
                    hook,
                    policy: if accept { Verdict::Accept } else { Verdict::Drop },
                    rules,
                };
                (family, chain)
            })
    })
}

/// A packet as a chain of this family and hook could see it.
fn packets_for(family: Family, hook: Hook) -> impl Strategy<Value = Packet> {
    let ether_type = if family == Family::Inet {
        prop::sample::select(vec![IPV4, IPV6]).boxed()
    } else {
        ether_types().boxed()
    };
    (
        ether_type,
        devices(),
        any::<bool>(),
        addresses(),
        protocols(),
        ports(),
        any::<bool>(),
    )
        .prop_map(
            move |(ether_type, device, loopback, dest, protocol, dest_port, established)| Packet {
                ether_type,
                in_interface: (hook != Hook::Output).then_some(device),
                loopback,
                dest,
                protocol,
                dest_port,
                // Only IP has connections to track.
                established: established && (ether_type == IPV4 || ether_type == IPV6),
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// What the kernel would do with the netlink form is what `decide` says the rules do.
    #[test]
    fn the_kernel_does_what_decide_says(
        (family, chain, packets) in chains().prop_flat_map(|(family, chain)| {
            let packets = prop::collection::vec(packets_for(family, chain.hook), 1..16);
            (Just(family), Just(chain), packets)
        })
    ) {
        let table = Table { family, name: "t".into(), chains: vec![chain.clone()] };
        let written = batch(&[table], Load::Replace).unwrap();
        for packet in packets {
            prop_assert_eq!(
                kernel_model::run(&written.bytes, &chain.name, family, chain.hook, &packet),
                decide(&chain.rules, chain.policy, &packet),
                "{:?}", packet
            );
        }
    }

    /// Tables written and read back are the same tables.
    #[test]
    fn tables_read_back_as_written(
        chains in prop::collection::vec(chains(), 0..4), replace in any::<bool>(),
    ) {
        let tables: Vec<Table> = chains
            .into_iter()
            .enumerate()
            .map(|(index, (family, chain))| Table { family, name: format!("table{index}"), chains: vec![chain] })
            .collect();
        let load = if replace { Load::Replace } else { Load::Add };
        prop_assert_eq!(read(&batch(&tables, load).unwrap().bytes).unwrap(), tables);
    }

    /// Whatever the bytes, reading them gives tables or an error.
    #[test]
    fn arbitrary_bytes_never_panic_the_reader(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        let _ = read(&bytes);
    }
}
