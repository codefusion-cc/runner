//! The networks a job is kept from because the Mac is in them.

use cf_vmhost::{host_networks, network_of};
use std::net::{IpAddr, UdpSocket};

#[test]
fn a_hosts_address_gives_the_network_under_its_mask() {
    for (address, netmask, network) in [
        // A home network, an office one with public addresses, a point-to-point VPN.
        (
            [192, 168, 1, 20],
            [255, 255, 255, 0],
            Some(([192, 168, 1, 0], 24)),
        ),
        ([145, 97, 20, 9], [255, 255, 252, 0], Some(([145, 97, 20, 0], 22))),
        ([25, 3, 2, 1], [255, 255, 255, 255], Some(([25, 3, 2, 1], 32))),
        ([9, 200, 3, 4], [255, 0, 0, 0], Some(([9, 0, 0, 0], 8))),
        // A mask with a hole counts up to the hole.
        ([145, 97, 20, 9], [255, 255, 0, 255], Some(([145, 97, 0, 0], 16))),
        // A mask that claims half of all addresses, or all of them, keeps a job from the address only: the
        // internet stays.
        ([25, 3, 2, 1], [254, 0, 0, 0], Some(([25, 3, 2, 1], 32))),
        ([25, 3, 2, 1], [128, 0, 0, 0], Some(([25, 3, 2, 1], 32))),
        ([25, 3, 2, 1], [0, 0, 0, 0], Some(([25, 3, 2, 1], 32))),
        // The host's loopback is not the guest's, and no address is none.
        ([127, 0, 0, 1], [255, 0, 0, 0], None),
        ([127, 9, 9, 9], [255, 255, 255, 255], None),
        ([0, 0, 0, 0], [255, 255, 255, 0], None),
    ] {
        assert_eq!(network_of(address, netmask), network, "{address:?} {netmask:?}");
    }
}

#[test]
fn the_networks_of_this_machine_hold_the_address_it_reaches_others_from() {
    let networks = host_networks();
    for (network, prefix) in &networks {
        assert!((8..=32).contains(prefix), "{network:?}/{prefix}");
        assert_ne!(network[0], 127, "{network:?}/{prefix}");
        let bits = u32::MAX << (32 - u32::from(*prefix));
        assert_eq!(
            u32::from_be_bytes(*network) & !bits,
            0,
            "{network:?}/{prefix} has host bits"
        );
    }
    let mut sorted = networks.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(networks, sorted, "each network once, in order");

    // The address this machine would send from (no packet is sent to find it out). A machine with no network
    // has none, and then there is nothing to find.
    let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
    if socket.connect("192.0.2.1:9").is_err() {
        return;
    }
    let Ok(IpAddr::V4(ours)) = socket.local_addr().map(|address| address.ip()) else {
        return;
    };
    if ours.is_loopback() || ours.is_unspecified() {
        return;
    }
    let ours = u32::from_be_bytes(ours.octets());
    assert!(
        networks.iter().any(|(network, prefix)| {
            let bits = u32::MAX << (32 - u32::from(*prefix));
            ours & bits == u32::from_be_bytes(*network)
        }),
        "no network of {networks:?} holds this machine's own address"
    );
}
