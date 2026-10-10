//! The DHCP exchange that gives the guest its address: the two packets it sends and the two it reads.

/// What a server offered or confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    /// The guest's address.
    pub address: [u8; 4],
    /// The network's mask.
    pub netmask: [u8; 4],
    /// The way out.
    pub gateway: Option<[u8; 4]>,
    /// Name servers, in the server's order.
    pub dns: Vec<[u8; 4]>,
    /// The server, which a request must name.
    pub server: [u8; 4],
}

/// A server's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// An address is offered.
    Offer(Lease),
    /// The requested address is the guest's.
    Ack(Lease),
    /// The request was refused.
    Nak,
}

const COOKIE: [u8; 4] = [99, 130, 83, 99];
const OPTIONS_AT: usize = 240;

fn header(xid: u32, mac: [u8; 6]) -> Vec<u8> {
    let mut packet = vec![0u8; OPTIONS_AT];
    packet[0] = 1; // a request
    packet[1] = 1; // Ethernet
    packet[2] = 6; // address length
    packet[4..8].copy_from_slice(&xid.to_be_bytes());
    // The broadcast flag: the guest has no address yet, so answers must come to everyone.
    packet[10] = 0x80;
    packet[28..34].copy_from_slice(&mac);
    packet[236..240].copy_from_slice(&COOKIE);
    packet
}

fn finish(mut packet: Vec<u8>, mac: [u8; 6]) -> Vec<u8> {
    packet.extend_from_slice(&[61, 7, 1]);
    packet.extend_from_slice(&mac);
    // Ask for the mask, the gateway and the name servers.
    packet.extend_from_slice(&[55, 3, 1, 3, 6, 255]);
    // Some servers drop packets shorter than BOOTP's 300 bytes.
    packet.resize(packet.len().max(300), 0);
    packet
}

/// How long a question waits for its answer before it is asked again, by how often it was asked before. Soon at
/// first: the first question often goes out before the host's side of the device takes packets, and the network
/// is the host's own, with a handful of machines on it. Then every 700 ms.
pub fn ask_again_after(asked_before: u32) -> std::time::Duration {
    std::time::Duration::from_millis(match asked_before {
        0 => 100,
        1 => 200,
        2 => 400,
        _ => 700,
    })
}

/// The first packet: who offers an address?
pub fn discover(xid: u32, mac: [u8; 6]) -> Vec<u8> {
    let mut packet = header(xid, mac);
    packet.extend_from_slice(&[53, 1, 1]);
    finish(packet, mac)
}

/// The second packet: the offered address, please.
pub fn request(xid: u32, mac: [u8; 6], offer: &Lease) -> Vec<u8> {
    let mut packet = header(xid, mac);
    packet.extend_from_slice(&[53, 1, 3, 50, 4]);
    packet.extend_from_slice(&offer.address);
    packet.extend_from_slice(&[54, 4]);
    packet.extend_from_slice(&offer.server);
    finish(packet, mac)
}

/// Reads a server's answer to the exchange `xid` of the device `mac`. `None` for anything else: another
/// exchange, another device, a malformed packet.
pub fn parse(packet: &[u8], xid: u32, mac: [u8; 6]) -> Option<Reply> {
    if packet.len() < OPTIONS_AT
        || packet[0] != 2
        || packet[4..8] != xid.to_be_bytes()
        || packet[28..34] != mac
        || packet[236..240] != COOKIE
    {
        return None;
    }
    let address: [u8; 4] = packet[16..20].try_into().ok()?;
    let four = |bytes: &[u8]| -> Option<[u8; 4]> { bytes.get(..4)?.try_into().ok() };
    let (mut kind, mut netmask, mut gateway, mut dns, mut server) = (None, None, None, Vec::new(), None);
    let mut options = &packet[OPTIONS_AT..];
    while let Some((&code, rest)) = options.split_first() {
        match code {
            0 => {
                options = rest;
                continue;
            }
            255 => break,
            _ => {}
        }
        let (&length, rest) = rest.split_first()?;
        let value = rest.get(..length as usize)?;
        options = &rest[length as usize..];
        match code {
            53 => kind = value.first().copied(),
            1 => netmask = four(value),
            3 => gateway = four(value),
            6 => dns = value.chunks_exact(4).filter_map(four).collect(),
            54 => server = four(value),
            _ => {}
        }
    }
    let lease = || -> Option<Lease> {
        Some(Lease {
            address,
            netmask: netmask?,
            gateway,
            dns: dns.clone(),
            server: server?,
        })
    };
    match kind? {
        2 => lease().map(Reply::Offer),
        5 => lease().map(Reply::Ack),
        6 => Some(Reply::Nak),
        _ => None,
    }
}
