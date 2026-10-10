//! The networks this Mac is in, which a job's machine is told to keep its job from: the guest's own list of
//! private ranges does not know a local network or a VPN with other addresses.

/// An IPv4 network as `(network, prefix length)`.
pub type Network = ([u8; 4], u8);

/// The network a job is kept from for one of the host's addresses: the address under its mask. `None` for the
/// host's loopback, which a guest cannot reach anyway. A mask that takes in more than a /8 (a VPN that claims
/// every address) would keep the job from the internet, so then it is the address alone.
pub fn network_of(address: [u8; 4], netmask: [u8; 4]) -> Option<Network> {
    if address[0] == 127 || address == [0; 4] {
        return None;
    }
    let prefix = u32::from_be_bytes(netmask).leading_ones() as u8;
    if prefix < 8 {
        return Some((address, 32));
    }
    let bits = u32::MAX << (32 - u32::from(prefix));
    Some(((u32::from_be_bytes(address) & bits).to_be_bytes(), prefix))
}

/// The IPv4 networks of this machine's interfaces now, each once. Empty when the system does not say: the guest's
/// own list of ranges still holds then.
pub fn host_networks() -> Vec<Network> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs sets the pointer to a list it allocates, or fails and gives none.
    if unsafe { libc::getifaddrs(&raw mut list) } != 0 {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut next = list;
    while !next.is_null() {
        // SAFETY: an entry of the list, which lives until freeifaddrs below.
        let entry = unsafe { &*next };
        next = entry.ifa_next;
        if let (Some(address), true) = (ipv4(entry.ifa_addr, true), !entry.ifa_netmask.is_null())
            // The mask of an IPv4 address is an IPv4 one, whatever family its own field names.
            && let Some(netmask) = ipv4(entry.ifa_netmask, false)
            && let Some(network) = network_of(address, netmask)
        {
            found.push(network);
        }
    }
    // SAFETY: the list getifaddrs gave, freed once; nothing above keeps a pointer into it.
    unsafe { libc::freeifaddrs(list) };
    found.sort_unstable();
    found.dedup();
    found
}

/// The address in a socket address of the interface list; with `check`, only when it says it is an IPv4 one.
fn ipv4(address: *const libc::sockaddr, check: bool) -> Option<[u8; 4]> {
    if address.is_null() {
        return None;
    }
    // SAFETY: a non-null socket address of the list; its family is in its first bytes whatever its kind, and one
    // of an IPv4 address is a `sockaddr_in`, read without assuming its alignment.
    unsafe {
        if check && libc::c_int::from((*address).sa_family) != libc::AF_INET {
            return None;
        }
        let inet = address.cast::<libc::sockaddr_in>().read_unaligned();
        Some(inet.sin_addr.s_addr.to_ne_bytes())
    }
}
