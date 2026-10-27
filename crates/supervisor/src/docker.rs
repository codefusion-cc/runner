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
//! What is decided here: the sandbox's ids ([`extents`]), the daemon's networks ([`networks`]) and the commands of
//! the daemon and of pasta.

use serde::{Deserialize, Serialize};

/// Where the sandbox's root is outside it: the first of its [`IDS`] shifted ids. Above every id an image gives a
/// user, and far from 0.
pub const SHIFT: u32 = 1 << 20;
/// How many ids the sandbox has, from 0: what images give their users and groups.
pub const IDS: u32 = 65536;

/// The folder of the daemon's socket, the same in the job's container and in the sandbox, and of its log.
pub const SOCKET_DIR: &str = "/run/cf-docker";
/// The socket's name in [`SOCKET_DIR`].
pub const SOCKET_NAME: &str = "docker.sock";
/// The log's name in [`SOCKET_DIR`].
pub const LOG_NAME: &str = "dockerd.log";
/// The daemon's socket, [`SOCKET_DIR`] and [`SOCKET_NAME`]. `/run/docker.sock` (and `/var/run/docker.sock`) links
/// to it in the job's container.
pub const SOCKET: &str = "/run/cf-docker/docker.sock";
/// What the daemon and the sandbox say, [`SOCKET_DIR`] and [`LOG_NAME`], for the job to read when the daemon does
/// not start.
pub const LOG: &str = "/run/cf-docker/dockerd.log";
/// Where the daemon keeps images, containers and volumes: a folder on the job's disk.
pub const DATA: &str = "/var/lib/docker";
/// Why a Docker job has no daemon, in the job's shared folder, for its start hook.
pub const PROBLEM: &str = "docker-problem.txt";
/// The group the socket belongs to, which the image gives the job's user.
pub const GROUP: &str = "docker";

/// Where the sandbox's programs are looked for, its `PATH`.
pub const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// The programs the sandbox runs from the image: the daemon, iptables for its network rule and pasta. Images built
/// before Docker jobs (2026-10-14) have the daemon but neither of the others.
pub const PROGRAMS: [&str; 3] = ["dockerd", "iptables", "pasta"];

/// Why a job gets no daemon when its image lacks some of [`PROGRAMS`], where `exists` says whether a path in the
/// image is there, or `None` when it has them all.
pub fn missing_programs(exists: impl Fn(&str) -> bool) -> Option<String> {
    let missing: Vec<&str> = PROGRAMS
        .into_iter()
        .filter(|name| !PATH.split(':').any(|folder| exists(&format!("{folder}/{name}"))))
        .collect();
    (!missing.is_empty()).then(|| {
        format!(
            "the image has no {}: it was built before Docker jobs; the Runner needs a newer image",
            missing.join(" or ")
        )
    })
}

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

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| word.to_string()).collect()
}

/// The daemon's command, as the sandbox's root in the sandbox's network. IPv6 stays off as in the rest of the
/// machine; the published ports' proxy stays on, since pasta forwards a port it sees bound.
pub fn dockerd_command(networks: &Networks) -> Vec<String> {
    let cidr = |(at, prefix): (Ip, u8)| format!("{}.{}.{}.{}/{prefix}", at[0], at[1], at[2], at[3]);
    let [a, b, c, _] = networks.bridge.0;
    let mut command = argv(&[
        "dockerd",
        "--bip",
        &cidr(([a, b, c, 1], networks.bridge.1)),
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
    ]);
    for pool in &networks.pools {
        command.push("--default-address-pool".into());
        command.push(format!("base={},size={POOL_NETWORK}", cidr(*pool)));
    }
    command
}

/// An IPv4 address, or the first address of a network.
pub type Ip = [u8; 4];

/// The networks of a job's daemon, inside its sandbox: the bridge of its containers, and the pools the networks
/// of `docker network create` and Compose are cut from, [`POOL_NETWORK`] bits each.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Networks {
    /// The bridge's network; the daemon has its first address.
    pub bridge: (Ip, u8),
    /// The pools, as `(network, prefix length)`.
    pub pools: Vec<(Ip, u8)>,
}

/// The prefix length of a network cut from a pool: 254 containers.
pub const POOL_NETWORK: u8 = 24;
/// How many pools the daemon gets: 1024 networks of [`POOL_NETWORK`].
pub const POOLS: usize = 4;

/// The blocks the daemon's networks are taken from, in order, each a /16 of a range set aside for private use:
/// Docker's own first (172.17 to 172.31), then the rest of 172.16.0.0/12, 10.0.0.0/8 and 198.18.0.0/15.
fn blocks() -> impl Iterator<Item = (Ip, u8)> {
    let docker = (17..=31).chain([16]).map(|b| [172, b, 0, 0]);
    let ten = (0..=255).map(|b| [10, b, 0, 0]);
    let benchmarks = [18, 19].map(|b| [198, b, 0, 0]);
    docker.chain(ten).chain(benchmarks).map(|at| (at, 16))
}

/// Whether two networks share an address.
pub fn overlap((a, a_prefix): (Ip, u8), (b, b_prefix): (Ip, u8)) -> bool {
    let prefix = a_prefix.min(b_prefix).min(32);
    cf_nft::network(a, prefix) == cf_nft::network(b, prefix)
}

/// The daemon's networks for a machine with `beside` next to it ([`crate::firewall::beside`]: its network, its
/// gateway, the networks the host is in): the first blocks that share no address with any of them. Docker's own
/// choice, 172.17.0.0/16, may be a network the host is in; an address of the host there would be the bridge's,
/// inside the machine, where the firewall never sees a packet to it. `None` when fewer than a bridge and one pool
/// are free: the job then runs without Docker.
pub fn networks(beside: &[(Ip, u8)]) -> Option<Networks> {
    let mut free = blocks().filter(|block| !beside.iter().any(|near| overlap(*block, *near)));
    let bridge = free.next()?;
    let pools: Vec<(Ip, u8)> = free.take(POOLS).collect();
    (!pools.is_empty()).then_some(Networks { bridge, pools })
}

/// pasta's command: joins the daemon's network namespace at `netns` to the machine's, IPv4 only. `--no-map-gw`
/// keeps the gateway the gateway: without it pasta would hand the gateway's address to the machine's own loopback.
/// Ports bound in the namespace (a container's published port) are forwarded from the machine's side, where the
/// job reaches them at `localhost`; nothing is forwarded the other way.
pub fn pasta_command(netns: &str) -> Vec<String> {
    argv(&[
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
    ])
}

/// The rule the daemon's network gets before the daemon starts: TCP segments with no flag at all are dropped on the
/// way in. Ubuntu 24.04's pasta sends a window update without the ACK flag (passt bug 84, fixed upstream in 2024-03);
/// a receiver ignores such a segment (RFC 9293), but the daemon's NAT takes it for invalid, does not map it to the
/// container, and the daemon's own kernel answers it with a reset that ends the container's connection.
pub fn flagless_tcp_rule() -> Vec<String> {
    argv(&[
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
    ])
}

/// The most of the daemon's log [`last_words`] reads, from its end.
pub const LAST_WORDS_BYTES: usize = 16 * 1024;
/// The most lines [`last_words`] gives.
pub const LAST_WORDS_LINES: usize = 5;
/// The most characters of a line [`last_words`] keeps.
pub const LAST_WORDS_CHARS: usize = 300;

/// What the machine's console shows of the daemon's log, `tail` (its last [`LAST_WORDS_BYTES`]), once the sandbox
/// ended: its last [`LAST_WORDS_LINES`] lines that say something, each clipped ([`cf_guest_proto::clip`]). The
/// sandbox's and the launcher's reasons and the daemon's last words are there; the job can write to the log too, so
/// nothing in it may steer the console.
pub fn last_words(tail: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(tail);
    let mut lines: Vec<String> = text
        .lines()
        .rev()
        .map(|line| cf_guest_proto::clip(line, LAST_WORDS_CHARS).trim().to_string())
        .filter(|line| !line.is_empty())
        .take(LAST_WORDS_LINES)
        .collect();
    lines.reverse();
    lines
}

/// What a Docker job's end asks its daemon before its cache may be kept: every container, and every volume.
pub const LEFT_OVER: [&str; 2] = ["/containers/json?all=1", "/volumes"];

/// Whether the daemon's answers to [`LEFT_OVER`], as raw HTTP responses in that order, say the job left nothing: no
/// container and no volume. Anything else, an error or an answer this cannot read included, says something is left.
pub fn left_nothing(containers: &str, volumes: &str) -> bool {
    fn body(response: &str) -> Option<serde_json::Value> {
        let (head, body) = response.split_once("\r\n\r\n")?;
        let status = head.lines().next()?.split_whitespace().nth(1)?;
        (status == "200").then(|| serde_json::from_str(body.trim()).ok())?
    }
    let none = |value: Option<&serde_json::Value>| match value {
        Some(serde_json::Value::Array(items)) => items.is_empty(),
        Some(serde_json::Value::Null) => true,
        _ => false,
    };
    let containers = body(containers);
    let volumes = body(volumes);
    containers.is_some_and(|c| none(Some(&c)))
        && volumes.is_some_and(|v| v.is_object() && none(v.get("Volumes")))
}
