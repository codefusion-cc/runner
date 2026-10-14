//! Docker for a job that asks for it (`cf-docker`): a daemon of the job's own, in a sandbox beside the job's
//! container that the supervisor makes, and gone with the machine.
//!
//! Why beside the container and not in it: the job's root has the machine's user ids (its user namespace maps every
//! id to itself), so its container keeps `/proc/sys` read-only and masks what would reach the machine. The kernel
//! mounts a new `/proc` only where an unmasked one is already visible, and every container the daemon starts mounts
//! one. The sandbox's root is a shifted id instead, [`SHIFT`], which owns nothing of the machine's: an unmasked
//! `/proc` gives it nothing (the kernel's settings, `sysrq-trigger` and `kcore` belong to the machine's root), and
//! the daemon and every container it starts, privileged ones too, are root of the sandbox alone. Its network is a
//! namespace of its own, joined to the machine's by pasta, whose sockets are the machine's and pass its firewall like
//! any other the job opens.
//!
//! What is decided here: the sandbox's ids ([`extents`]) and the commands of the daemon and of pasta.

/// Where the sandbox's root is outside it: the first of its [`IDS`] shifted ids. Above every id an image gives a
/// user, and far from 0.
pub const SHIFT: u32 = 1 << 20;
/// How many ids the sandbox has, from 0: what images give their users and groups.
pub const IDS: u32 = 65536;

/// The folder of the daemon's socket, the same in the job's container and in the sandbox, and of its log.
pub const SOCKET_DIR: &str = "/run/cf-docker";
/// The daemon's socket. `/run/docker.sock` (and `/var/run/docker.sock`) links to it in the job's container.
pub const SOCKET: &str = "/run/cf-docker/docker.sock";
/// What the daemon and the sandbox say, for the job to read when the daemon does not start.
pub const LOG: &str = "/run/cf-docker/dockerd.log";
/// Where the daemon keeps images, containers and volumes: a folder on the job's disk.
pub const DATA: &str = "/var/lib/docker";
/// Why a Docker job has no daemon, in the job's shared folder, for its start hook.
pub const PROBLEM: &str = "docker-problem.txt";
/// The group the socket belongs to, which the image gives the job's user.
pub const GROUP: &str = "docker";

/// One line of a `uid_map` or `gid_map`: the first id inside, the first outside, how many.
pub type Extent = (u32, u32, u32);

/// The most lines a map may have (the kernel's limit).
pub const MOST_EXTENTS: usize = 340;

/// The sandbox's ids: each of `0..IDS` inside, at `SHIFT + id` outside, except the ids in `same`, which keep their
/// own, so that the job's user and group and the socket's group mean the same on both sides. An id in `same` that is
/// 0 or not below [`IDS`] is left out: no id of the sandbox is ever the machine's root. The extents are in order,
/// and none of their outside ranges meet.
pub fn extents(same: &[u32]) -> Vec<Extent> {
    let mut kept: Vec<u32> = same.iter().copied().filter(|id| (1..IDS).contains(id)).collect();
    kept.sort_unstable();
    kept.dedup();
    kept.truncate(MOST_EXTENTS / 2 - 1);
    let mut out = Vec::new();
    let mut next = 0;
    for id in kept {
        if id > next {
            out.push((next, SHIFT + next, id - next));
        }
        out.push((id, id, 1));
        next = id + 1;
    }
    if next < IDS {
        out.push((next, SHIFT + next, IDS - next));
    }
    out
}

/// A map as the kernel reads it from `/proc/<pid>/uid_map`.
pub fn map_text(extents: &[Extent]) -> String {
    extents
        .iter()
        .map(|(inside, outside, count)| format!("{inside} {outside} {count}\n"))
        .collect()
}

/// The daemon's command, as the sandbox's root in the sandbox's network. IPv6 stays off as in the rest of the
/// machine; the published ports' proxy stays on, since pasta forwards a port it sees bound.
pub fn dockerd_command() -> Vec<String> {
    [
        "dockerd",
        "--host",
        &format!("unix://{SOCKET}"),
        "--group",
        GROUP,
        "--data-root",
        DATA,
        "--exec-root",
        "/run/docker",
        "--pidfile",
        "/run/docker.pid",
        "--ip6tables=false",
        "--userland-proxy=true",
    ]
    .iter()
    .map(|text| text.to_string())
    .collect()
}

/// pasta's command: joins the daemon's network namespace at `netns` to the machine's, IPv4 only. `--no-map-gw`
/// keeps the gateway the gateway: without it pasta would hand the gateway's address to the machine's own loopback.
/// Ports bound in the namespace (a container's published port) are forwarded from the machine's side, where the
/// job reaches them at `localhost`; nothing is forwarded the other way.
pub fn pasta_command(netns: &str) -> Vec<String> {
    [
        "pasta",
        "--config-net",
        "--quiet",
        "--ipv4-only",
        "--no-map-gw",
        "--tcp-ports",
        "auto",
        "--udp-ports",
        "auto",
        "--tcp-ns",
        "none",
        "--udp-ns",
        "none",
        "--netns-only",
        "--netns",
        netns,
    ]
    .iter()
    .map(|text| text.to_string())
    .collect()
}

/// The rule the daemon's network gets before the daemon starts: TCP segments with no flag at all are dropped on the
/// way in. Ubuntu 24.04's pasta sends a window update without the ACK flag (passt bug 84, fixed upstream in 2024-03);
/// a receiver ignores such a segment (RFC 9293), but the daemon's NAT takes it for invalid, does not map it to the
/// container, and the daemon's own kernel answers it with a reset that ends the container's connection.
pub fn flagless_tcp_rule() -> Vec<String> {
    [
        "iptables",
        "--wait",
        "-t",
        "raw",
        "-I",
        "PREROUTING",
        "-p",
        "tcp",
        "--tcp-flags",
        "ALL",
        "NONE",
        "-j",
        "DROP",
    ]
    .iter()
    .map(|text| text.to_string())
    .collect()
}
