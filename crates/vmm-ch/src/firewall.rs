//! The host's firewall in front of a guest: a netdev chain on the ingress of the guest's tap device in the
//! machine's own network namespace, so it sees every frame the guest sends before the bridge and pasta do.
//!
//! It holds whatever the guest does to its own firewall. It lets through ARP, the guest's DHCP to pasta, DNS to
//! pasta's forwarder, and IPv4 to the internet; it drops IPv4 to the private and special ranges
//! ([`cf_nft::BLOCKED`]), to the loopback range, and to every network the host itself is in (which covers its
//! public addresses too), and anything that is not ARP or IPv4: IPv6 is not there for a guest at all.

use crate::command::{DNS, GATEWAY};
use cf_nft::{ARP, BLOCKED, Chain, Hook, IPV4, Interface, Ip, Match, Rule, TCP, Table, UDP, Verdict};

/// The table's name in the namespace.
pub const TABLE: &str = "codefusion-guest";

/// The guest's tap device in the namespace.
pub const GUEST_DEVICE: &str = "tap0";

/// The loopback range, which pasta could map to the host's own (it is told not to; this holds if it did).
const LOOPBACK: (Ip, u8) = ([127, 0, 0, 0], 8);

/// DHCP's server port.
const DHCP: u16 = 67;

fn rule(when: &[Match], then: Verdict) -> Rule {
    Rule {
        when: when.to_vec(),
        then,
    }
}

fn to(address: Ip) -> Match {
    Match::DestIn {
        network: address,
        prefix: 32,
    }
}

/// The guest's rules, in order; what none takes is dropped. `host` is the networks of the host's interfaces now.
pub fn rules(host: &[(Ip, u8)]) -> Vec<Rule> {
    let ipv4 = Match::EtherType(IPV4);
    let mut rules = vec![
        rule(&[Match::EtherType(ARP)], Verdict::Accept),
        rule(
            &[ipv4, to([255; 4]), Match::Protocol(UDP), Match::DestPort(DHCP)],
            Verdict::Accept,
        ),
        rule(
            &[ipv4, to(GATEWAY), Match::Protocol(UDP), Match::DestPort(DHCP)],
            Verdict::Accept,
        ),
        rule(
            &[ipv4, to(DNS), Match::Protocol(UDP), Match::DestPort(53)],
            Verdict::Accept,
        ),
        rule(
            &[ipv4, to(DNS), Match::Protocol(TCP), Match::DestPort(53)],
            Verdict::Accept,
        ),
    ];
    for &(network, prefix) in BLOCKED.iter().chain([&LOOPBACK]).chain(host) {
        rules.push(rule(&[Match::DestIn { network, prefix }], Verdict::Drop));
    }
    rules.push(rule(&[ipv4], Verdict::Accept));
    rules
}

/// The table, for the namespace's kernel.
pub fn table(host: &[(Ip, u8)]) -> Result<Table, cf_nft::Error> {
    Ok(Table {
        family: cf_nft::Family::Netdev,
        name: TABLE.into(),
        chains: vec![Chain {
            name: "guest".into(),
            hook: Hook::Ingress(Interface::new(GUEST_DEVICE)?),
            policy: Verdict::Drop,
            rules: rules(host),
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cf_nft::{IPV6, Packet, decide};
    use proptest::prelude::*;

    /// The host's networks as an RK3588 board on a wired and a wireless network with Docker has them, and a
    /// public address of its own.
    const HOST: &[(Ip, u8)] = &[
        ([192, 168, 1, 0], 24),
        ([192, 168, 100, 0], 22),
        ([172, 17, 0, 0], 16),
        ([203, 0, 113, 7], 32),
    ];

    fn packet(ether_type: u16, dest: Ip, protocol: u8, dest_port: u16) -> Packet {
        Packet {
            ether_type,
            in_interface: Some(Interface::new(GUEST_DEVICE).unwrap()),
            loopback: false,
            dest,
            protocol,
            dest_port,
            established: false,
        }
    }

    fn verdict(packet: &Packet) -> Verdict {
        decide(&rules(HOST), Verdict::Drop, packet)
    }

    fn tcp(dest: Ip, port: u16) -> Verdict {
        verdict(&packet(IPV4, dest, TCP, port))
    }

    fn udp(dest: Ip, port: u16) -> Verdict {
        verdict(&packet(IPV4, dest, UDP, port))
    }

    #[test]
    fn the_internet_is_reached() {
        assert_eq!(tcp([140, 82, 121, 4], 443), Verdict::Accept);
        assert_eq!(udp([1, 1, 1, 1], 53), Verdict::Accept);
        assert_eq!(verdict(&packet(IPV4, [8, 8, 8, 8], 1, 0)), Verdict::Accept);
    }

    #[test]
    fn dns_reaches_pasta_and_nothing_else_of_it() {
        assert_eq!(udp(DNS, 53), Verdict::Accept);
        assert_eq!(tcp(DNS, 53), Verdict::Accept);
        assert_eq!(tcp(DNS, 80), Verdict::Drop);
        assert_eq!(udp(DNS, 5353), Verdict::Drop);
    }

    #[test]
    fn dhcp_reaches_pasta_and_nothing_else_does() {
        assert_eq!(udp([255; 4], 67), Verdict::Accept);
        assert_eq!(udp(GATEWAY, 67), Verdict::Accept);
        assert_eq!(udp([192, 168, 1, 1], 67), Verdict::Drop);
        assert_eq!(udp([255; 4], 53), Verdict::Drop);
        assert_eq!(tcp(GATEWAY, 67), Verdict::Drop);
    }

    #[test]
    fn the_gateway_is_no_way_to_the_host() {
        for port in [22, 80, 443, 8080] {
            assert_eq!(tcp(GATEWAY, port), Verdict::Drop, "{port}");
        }
    }

    #[test]
    fn the_wired_and_the_wireless_local_network_and_their_routers_are_dropped() {
        assert_eq!(tcp([192, 168, 1, 1], 80), Verdict::Drop);
        assert_eq!(tcp([192, 168, 1, 23], 22), Verdict::Drop);
        assert_eq!(tcp([192, 168, 100, 1], 443), Verdict::Drop);
        assert_eq!(tcp([192, 168, 103, 254], 443), Verdict::Drop);
        assert_eq!(udp([192, 168, 1, 255], 9), Verdict::Drop);
    }

    #[test]
    fn the_hosts_own_addresses_are_dropped_public_and_dockers_too() {
        assert_eq!(tcp([203, 0, 113, 7], 22), Verdict::Drop);
        assert_eq!(tcp([172, 17, 0, 1], 2375), Verdict::Drop);
        // A host network outside every private range is dropped for being the host's.
        let host = [([198, 51, 100, 0], 24)];
        assert_eq!(
            decide(
                &rules(&host),
                Verdict::Drop,
                &packet(IPV4, [198, 51, 100, 9], TCP, 22)
            ),
            Verdict::Drop
        );
        assert_eq!(
            decide(
                &rules(&[]),
                Verdict::Drop,
                &packet(IPV4, [198, 51, 100, 9], TCP, 22)
            ),
            Verdict::Accept
        );
    }

    #[test]
    fn loopback_link_local_and_the_metadata_address_are_dropped() {
        assert_eq!(tcp([127, 0, 0, 1], 8080), Verdict::Drop);
        assert_eq!(udp([127, 0, 0, 53], 53), Verdict::Drop);
        assert_eq!(tcp([169, 254, 169, 254], 80), Verdict::Drop);
        assert_eq!(tcp([169, 254, 1, 1], 80), Verdict::Drop);
    }

    #[test]
    fn multicast_and_broadcast_are_dropped() {
        assert_eq!(udp([224, 0, 0, 251], 5353), Verdict::Drop);
        assert_eq!(udp([239, 255, 255, 250], 1900), Verdict::Drop);
        assert_eq!(udp([255; 4], 137), Verdict::Drop);
    }

    #[test]
    fn ipv6_and_other_protocols_are_dropped_and_arp_passes() {
        assert_eq!(verdict(&packet(IPV6, [0; 4], TCP, 443)), Verdict::Drop);
        assert_eq!(verdict(&packet(IPV6, [0; 4], UDP, 547)), Verdict::Drop);
        assert_eq!(verdict(&packet(0x8100, [1, 1, 1, 1], TCP, 443)), Verdict::Drop);
        assert_eq!(verdict(&packet(0x88cc, [0; 4], 0, 0)), Verdict::Drop);
        assert_eq!(verdict(&packet(ARP, [0; 4], 0, 0)), Verdict::Accept);
    }

    #[test]
    fn the_table_is_one_the_kernel_takes() {
        let table = table(HOST).unwrap();
        assert_eq!(table.chains[0].policy, Verdict::Drop);
        assert_eq!(
            table.chains[0].hook,
            Hook::Ingress(Interface::new(GUEST_DEVICE).unwrap())
        );
        let batch = cf_nft::batch(std::slice::from_ref(&table), cf_nft::Load::Replace).unwrap();
        assert_eq!(cf_nft::read(&batch.bytes).unwrap(), vec![table]);
    }

    proptest! {
        #[test]
        fn nothing_reaches_a_blocked_or_host_network_but_pasta_dhcp_and_dns(
            range in 0..BLOCKED.len() + HOST.len() + 1,
            host in any::<u32>(),
            protocol in any::<u8>(),
            port in any::<u16>(),
        ) {
            let (network, prefix) = *BLOCKED.iter().chain(HOST).chain([&LOOPBACK]).nth(range).unwrap();
            let free = u32::MAX.checked_shr(u32::from(prefix)).unwrap_or(0);
            let dest = ((u32::from_be_bytes(network) & !free) | (host & free)).to_be_bytes();
            let pasta = (dest == DNS && port == 53 && (protocol == TCP || protocol == UDP))
                || ((dest == GATEWAY || dest == [255; 4]) && port == DHCP && protocol == UDP);
            let expected = if pasta { Verdict::Accept } else { Verdict::Drop };
            prop_assert_eq!(verdict(&packet(IPV4, dest, protocol, port)), expected);
        }

        #[test]
        fn nothing_but_ipv4_and_arp_passes(ether_type in any::<u16>(), dest: [u8; 4], protocol: u8, port: u16) {
            prop_assume!(ether_type != IPV4 && ether_type != ARP);
            prop_assert_eq!(verdict(&packet(ether_type, dest, protocol, port)), Verdict::Drop);
        }
    }
}
