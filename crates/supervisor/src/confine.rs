//! What the supervisor reads and checks as a container's first process before the job starts (`container-guest`).
//!
//! The container is made with this process as its root, holding only what it needs to lock the job in: the
//! capability to load the firewall into the container's own network namespace, and those to drop to the job's user.
//! It loads the machine's firewall ([`crate::firewall::policy`]) with the name servers the engine wrote
//! ([`name_servers`]) and every network next to the container ([`crate::firewall::own_networks`]), drops every
//! capability from the bounding set, installs the seccomp filter ([`crate::seccomp`]), becomes the job's user, and
//! proves from the kernel's own account that nothing is left the job could undo any of it with ([`locked_down`]). A
//! step that fails ends the container before the job starts.

use cf_nft::Ip;

/// The most name servers the C library's resolver reads (`MAXNS`).
const MAXNS: usize = 3;

/// The IPv4 name servers of a `resolv.conf`, as the resolver reads it: the first three `nameserver` lines whose
/// address it takes, of either family. Those are the only addresses of a blocked range the job reaches, on port 53.
pub fn name_servers(resolv: &str) -> Vec<Ip> {
    let mut servers = Vec::new();
    let read = resolv.lines().filter_map(|line| {
        let line = line.split(['#', ';']).next().unwrap_or_default();
        let mut words = line.split_whitespace();
        (words.next() == Some("nameserver")).then_some(())?;
        words.next()?.parse::<std::net::IpAddr>().ok()
    });
    for address in read.take(MAXNS) {
        if let std::net::IpAddr::V4(address) = address
            && !servers.contains(&address.octets())
        {
            servers.push(address.octets());
        }
    }
    servers
}

/// The kernel's flag of a route through a gateway (`RTF_GATEWAY`).
const RTF_GATEWAY: u32 = 0x2;

/// The gateway of the default route in `/proc/net/route`, whose addresses are the kernel's own bytes in hex.
pub fn default_gateway(route: &str) -> Option<Ip> {
    route.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (destination, gateway, flags) = (fields.get(1)?, fields.get(2)?, fields.get(3)?);
        let flags = u32::from_str_radix(flags, 16).ok()?;
        (*destination == "00000000" && flags & RTF_GATEWAY != 0).then_some(())?;
        Some(u32::from_str_radix(gateway, 16).ok()?.to_ne_bytes())
    })
}

/// The job's memory and process limits as the container's control group holds them (`memory.max`, `pids.max`). An
/// engine that could not set one leaves `max`, and a job is not run without both.
pub fn limits_in_force(memory_max: &str, pids_max: &str) -> Result<(u64, u64), String> {
    let read = |name: &str, text: &str| {
        text.trim().parse::<u64>().map_err(|_| {
            format!(
                "the job's {name} limit is not in force: the container's control group says {:?}",
                text.trim()
            )
        })
    };
    Ok((read("memory", memory_max)?, read("process", pids_max)?))
}

/// Whether the kernel's account of this process (`/proc/self/status`) says it is the job's user `uid` with no
/// capability in any set, no way to gain one, and the seccomp filter on: what the job inherits.
pub fn locked_down(status: &str, uid: u32) -> Result<(), String> {
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
            .map(str::trim)
            .ok_or_else(|| format!("the kernel does not say {name}"))
    };
    let ids: Vec<&str> = field("Uid")?.split_whitespace().collect();
    if ids.len() != 4 || ids.iter().any(|id| id.parse() != Ok(uid)) {
        return Err(format!("the supervisor is user {ids:?}, not the job's {uid}"));
    }
    for set in ["CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"] {
        let caps = field(set)?;
        if u64::from_str_radix(caps, 16) != Ok(0) {
            return Err(format!("capabilities are left in {set}: {caps}"));
        }
    }
    if field("NoNewPrivs")? != "1" {
        return Err("the job could gain privileges by running a program".into());
    }
    if field("Seccomp")? != "2" {
        return Err("the seccomp filter is not on".into());
    }
    Ok(())
}
