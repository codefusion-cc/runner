//! The network devices of a machine's namespace, through rtnetlink: a bridge that joins pasta's tap and the
//! guest's, both up. The kernel takes these messages from the namespace's root (the sandbox) and from nobody else.

use cf_netlink::{NLM_F_ACK, NLM_F_CREATE, NLM_F_REQUEST, attribute, message, nested};

/// The bridge in the namespace.
pub const BRIDGE: &str = "br0";

// Numbers from the kernel's `rtnetlink.h`, `if_link.h` and `if.h`.
const RTM_NEWLINK: u16 = 16;
const NLM_F_EXCL: u16 = 0x200;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFLA_MASTER: u16 = 10;
const IFLA_LINKINFO: u16 = 18;
const IFLA_INFO_KIND: u16 = 1;
const IFF_UP: u32 = 1;

/// `struct ifinfomsg`: a device by its index, with the flags to set and which of them.
fn ifinfomsg(index: u32, flags: u32, change: u32) -> Vec<u8> {
    let mut body = vec![0u8, 0]; // AF_UNSPEC, padding
    body.extend_from_slice(&0u16.to_ne_bytes()); // ifi_type
    body.extend_from_slice(&index.to_ne_bytes());
    body.extend_from_slice(&flags.to_ne_bytes());
    body.extend_from_slice(&change.to_ne_bytes());
    body
}

fn name(text: &str) -> Vec<u8> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    bytes
}

/// Makes a bridge of this name and MTU, down; one of the name that is there already is an error.
pub fn new_bridge(bridge: &str, mtu: u16, sequence: u32) -> Vec<u8> {
    let mut body = ifinfomsg(0, 0, 0);
    attribute(&mut body, IFLA_IFNAME, &name(bridge));
    attribute(&mut body, IFLA_MTU, &u32::from(mtu).to_ne_bytes());
    nested(&mut body, IFLA_LINKINFO, |info| {
        attribute(info, IFLA_INFO_KIND, &name("bridge"))
    });
    let mut bytes = Vec::new();
    message(
        &mut bytes,
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        sequence,
        &body,
    );
    bytes
}

/// Sets a device up with this MTU and, when given, puts it into the bridge of index `master`.
pub fn set_up(index: u32, mtu: u16, master: Option<u32>, sequence: u32) -> Vec<u8> {
    let mut body = ifinfomsg(index, IFF_UP, IFF_UP);
    attribute(&mut body, IFLA_MTU, &u32::from(mtu).to_ne_bytes());
    if let Some(master) = master {
        attribute(&mut body, IFLA_MASTER, &master.to_ne_bytes());
    }
    let mut bytes = Vec::new();
    message(
        &mut bytes,
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK,
        sequence,
        &body,
    );
    bytes
}

#[cfg(target_os = "linux")]
pub use linux::{index, join};

#[cfg(target_os = "linux")]
mod linux {
    use super::{BRIDGE, new_bridge, set_up};
    use cf_netlink::Socket;

    /// The index of a device of the namespace.
    pub fn index(device: &str) -> Result<u32, String> {
        let name = std::ffi::CString::new(device).map_err(|_| format!("{device:?} is no device name"))?;
        // SAFETY: the name is NUL terminated and lives through the call.
        match unsafe { libc::if_nametoindex(name.as_ptr()) } {
            0 => Err(format!("no device {device}: {}", std::io::Error::last_os_error())),
            index => Ok(index),
        }
    }

    /// Joins the devices into a new bridge and sets the bridge and them up, all at `mtu`.
    pub fn join(devices: &[&str], mtu: u16) -> Result<(), String> {
        let socket = Socket::open(libc::NETLINK_ROUTE).map_err(|e| e.to_string())?;
        socket
            .request(&new_bridge(BRIDGE, mtu, 1), 1)
            .map_err(|e| format!("bridge {BRIDGE}: {e}"))?;
        let bridge = index(BRIDGE)?;
        for (sequence, device) in (2..).zip(devices) {
            let message = set_up(index(device)?, mtu, Some(bridge), sequence);
            socket
                .request(&message, 1)
                .map_err(|e| format!("{device} into {BRIDGE}: {e}"))?;
        }
        socket
            .request(&set_up(bridge, mtu, None, 99), 1)
            .map_err(|e| format!("{BRIDGE} up: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cf_netlink::{attributes, messages};

    fn one(bytes: &[u8]) -> (u16, u16, u32, Vec<u8>) {
        let all: Vec<_> = messages(bytes).collect::<Result<_, _>>().unwrap();
        assert_eq!(all.len(), 1);
        (all[0].kind, all[0].flags, all[0].sequence, all[0].body.to_vec())
    }

    fn attrs(bytes: &[u8]) -> Vec<(u16, Vec<u8>)> {
        attributes(bytes)
            .map(|a| a.map(|(kind, payload)| (kind, payload.to_vec())))
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn a_bridge_is_made_once_with_its_name_mtu_and_kind() {
        let (kind, flags, sequence, body) = one(&new_bridge("br0", 1500, 7));
        assert_eq!((kind, sequence), (RTM_NEWLINK, 7));
        assert_eq!(flags, NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL);
        assert_eq!(body[..16], ifinfomsg(0, 0, 0)[..]);
        let found = attrs(&body[16..]);
        assert_eq!(found[0], (IFLA_IFNAME, b"br0\0".to_vec()));
        assert_eq!(found[1], (IFLA_MTU, 1500u32.to_ne_bytes().to_vec()));
        assert_eq!(found[2].0 & !cf_netlink::NLA_F_NESTED, IFLA_LINKINFO);
        assert_eq!(attrs(&found[2].1), vec![(IFLA_INFO_KIND, b"bridge\0".to_vec())]);
    }

    #[test]
    fn a_device_is_set_up_into_its_bridge() {
        let (kind, flags, _, body) = one(&set_up(4, 1500, Some(9), 2));
        assert_eq!(kind, RTM_NEWLINK);
        assert_eq!(flags, NLM_F_REQUEST | NLM_F_ACK, "never makes a device");
        assert_eq!(&body[4..8], &4u32.to_ne_bytes());
        assert_eq!(&body[8..12], &IFF_UP.to_ne_bytes());
        assert_eq!(&body[12..16], &IFF_UP.to_ne_bytes());
        assert_eq!(
            attrs(&body[16..]),
            vec![
                (IFLA_MTU, 1500u32.to_ne_bytes().to_vec()),
                (IFLA_MASTER, 9u32.to_ne_bytes().to_vec())
            ]
        );
        let (_, _, _, body) = one(&set_up(9, 1400, None, 3));
        assert_eq!(
            attrs(&body[16..]),
            vec![(IFLA_MTU, 1400u32.to_ne_bytes().to_vec())]
        );
    }
}
