//! The guest's network as a machine gets it: the system's pasta in front, the sandbox's guard ([`guard`]) and the
//! guest's firewall on its device, and a stand-in for the guest, so no KVM is needed. The stand-in is a network
//! namespace at the end of a veth pair whose other end takes the tap's name and place; it has the guest's address and
//! gateway and tries one target per probe. Each blocked target is a test of its own.
//!
//! A probe says `dropped` when nothing answered within [`WAIT`]: the frame never reached pasta, which would have
//! answered a connection it could not make with a reset (`refused`). Every blocked target must be `dropped`, not
//! `refused`. The host listens on all its addresses and on its loopback for the whole of each test, so a target of
//! the host that the firewall let through would answer `ok`.
//!
//! A DNS probe asks as a resolver does, again when no answer or a server failure came ([`ASKS`]): one question over
//! UDP can be lost, or time out, on the way to the host's resolver and beyond.
//!
//! It needs pasta (`apt install passt`), user namespaces, and `ip` and `unshare` for the stand-in alone.
#![cfg(target_os = "linux")]

use cf_vmm_ch::command::{GATEWAY, GUEST, PREFIX};
use cf_vmm_ch::firewall::GUEST_DEVICE;
use cf_vmm_ch::sandbox::guard;
use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs as _, UdpSocket};
use std::process::{Command, Stdio};
use std::time::Duration;

/// How long a probe waits for an answer.
const WAIT: Duration = Duration::from_secs(3);
/// How often a DNS probe asks before it says `dropped`, [`WAIT`] apart: 9 s in all, as glibc's resolver gives up
/// after two questions 5 s apart.
const ASKS: u32 = 3;
const ROLE: &str = "CF_ISOLATION_ROLE";
const PROBES: &str = "CF_ISOLATION_PROBES";
const HOST: &str = "CF_ISOLATION_HOST";
/// The stand-in's end of the veth pair, before it moves into the stand-in's namespace.
const STAND_IN_DEVICE: &str = "guest0";

/// What a probe tries: `tcp <address> <port>`, or `dns <address>`: a question for github.com's address.
#[derive(Clone)]
struct Probe {
    name: String,
    what: String,
}

fn tcp(name: &str, address: impl std::fmt::Display, port: u16) -> Probe {
    Probe {
        name: name.into(),
        what: format!("tcp {address} {port}"),
    }
}

/// The host side of a test: listeners on all the host's addresses and on its loopback, which accept and answer for
/// as long as it lives.
struct Host {
    everywhere: u16,
    loopback: u16,
}

impl Host {
    fn new() -> Host {
        let listen = |address: &str| {
            let listener = TcpListener::bind(address).unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                for mut stream in listener.incoming().flatten() {
                    let _ = stream.write_all(b"x");
                }
            });
            port
        };
        Host {
            everywhere: listen("0.0.0.0:0"),
            loopback: listen("127.0.0.1:0"),
        }
    }
}

/// The host's own IPv4 addresses, of every interface but the loopback, as the kernel lists them now.
fn host_addresses() -> Vec<Ipv4Addr> {
    let listed = Command::new("ip")
        .args(["-4", "-o", "addr", "show"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&listed.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().skip_while(|word| *word != "inet").nth(1))
        .filter_map(|cidr| cidr.split('/').next()?.parse().ok())
        .filter(|address: &Ipv4Addr| !address.is_loopback())
        .collect()
}

/// The routers of the host's default routes.
fn routers() -> Vec<Ipv4Addr> {
    let listed = Command::new("ip")
        .args(["-4", "route", "show", "default"])
        .output()
        .unwrap();
    let mut found: Vec<Ipv4Addr> = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .filter_map(|line| {
            line.split_whitespace()
                .skip_while(|word| *word != "via")
                .nth(1)?
                .parse()
                .ok()
        })
        .collect();
    found.sort_unstable();
    found.dedup();
    found
}

/// Runs the probes from a stand-in guest behind pasta and the firewall, and says what each found.
fn run(probes: &[Probe]) -> BTreeMap<String, String> {
    let pasta = cf_vmm_ch::programs::pasta().unwrap();
    let host: Vec<String> = cf_vmhost::host_networks()
        .iter()
        .map(|(network, prefix)| format!("{}/{prefix}", Ipv4Addr::from(*network)))
        .collect();
    let me = std::env::current_exe().unwrap();
    let mut command = cf_vmm_ch::command::pasta(
        &pasta,
        [
            "--exact",
            "stand_in",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ]
        .into_iter()
        .fold(vec![me.into_os_string()], |mut all, arg| {
            all.push(arg.into());
            all
        }),
    );
    let program = command.remove(0);
    let lines: Vec<String> = probes
        .iter()
        .map(|probe| format!("{} {}", probe.name, probe.what))
        .collect();
    let output = Command::new(program)
        .args(command)
        .env(ROLE, "stand-in")
        .env(PROBES, lines.join("\n"))
        .env(HOST, host.join(","))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&output.stdout);
    let found: BTreeMap<String, String> = said
        .lines()
        // The test harness's own words may come first on the line.
        .filter_map(|line| line.split_once("probe ")?.1.split_once(' '))
        .map(|(name, result)| (name.to_string(), result.to_string()))
        .collect();
    assert_eq!(
        found.len(),
        probes.len(),
        "the stand-in did not say what every probe found: {said}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    found
}

/// Asserts that every probe found what is wanted.
fn expect(probes: &[Probe], wanted: &str) {
    let found = run(probes);
    let wrong: Vec<String> = found
        .iter()
        .filter(|(_, result)| result.as_str() != wanted)
        .map(|(name, result)| format!("{name}: {result}"))
        .collect();
    assert!(wrong.is_empty(), "wanted {wanted}, found {wrong:?}");
}

#[test]
fn dns_through_the_gateway_answers() {
    expect(
        &[Probe {
            name: "dns".into(),
            what: format!("dns {}", Ipv4Addr::from(GATEWAY)),
        }],
        "ok",
    );
}

#[test]
fn the_internet_is_reached() {
    let github = ("github.com", 443)
        .to_socket_addrs()
        .unwrap()
        .find(SocketAddr::is_ipv4)
        .unwrap();
    expect(&[tcp("github", github.ip(), 443)], "ok");
}

#[test]
fn every_address_of_the_host_is_dropped() {
    let host = Host::new();
    let addresses = host_addresses();
    assert!(!addresses.is_empty(), "the host has no address but its loopback");
    let probes: Vec<Probe> = addresses
        .iter()
        .map(|address| tcp(&format!("host-{address}"), address, host.everywhere))
        .collect();
    expect(&probes, "dropped");
}

#[test]
fn the_routers_of_the_host_are_dropped() {
    let probes: Vec<Probe> = routers()
        .iter()
        .flat_map(|router| {
            [
                tcp(&format!("router-{router}-dns"), router, 53),
                tcp(&format!("router-{router}-web"), router, 80),
            ]
        })
        .collect();
    if probes.is_empty() {
        eprintln!("the host has no default route over IPv4: nothing to try");
        return;
    }
    expect(&probes, "dropped");
}

#[test]
fn the_gateway_leads_to_nothing_of_the_host() {
    let host = Host::new();
    let gateway = Ipv4Addr::from(GATEWAY);
    expect(
        &[
            tcp("gateway-all", gateway, host.everywhere),
            tcp("gateway-loopback", gateway, host.loopback),
            tcp("gateway-ssh", gateway, 22),
        ],
        "dropped",
    );
}

#[test]
fn the_hosts_loopback_is_not_reached() {
    let host = Host::new();
    // The guest's own loopback answers 127.0.0.1, not the host's: nothing listens there in the stand-in.
    let found = run(&[tcp("loopback", Ipv4Addr::LOCALHOST, host.loopback)]);
    assert_ne!(found["loopback"], "ok");
    // Nor does any address pasta could map to the host's loopback lead there.
    expect(
        &[tcp("mapped", Ipv4Addr::new(10, 0, 2, 3), host.loopback)],
        "dropped",
    );
}

#[test]
fn the_lan_ranges_are_dropped() {
    expect(
        &[
            tcp("ten", Ipv4Addr::new(10, 1, 2, 3), 80),
            tcp("one-seven-two", Ipv4Addr::new(172, 16, 0, 1), 80),
            tcp("one-nine-two", Ipv4Addr::new(192, 168, 1, 1), 80),
        ],
        "dropped",
    );
}

#[test]
fn the_shared_and_special_ranges_are_dropped() {
    expect(
        &[
            tcp("cgnat", Ipv4Addr::new(100, 64, 0, 1), 80),
            tcp("benchmark", Ipv4Addr::new(198, 18, 0, 1), 80),
            tcp("this-network", Ipv4Addr::new(0, 0, 0, 1), 80),
        ],
        "dropped",
    );
}

#[test]
fn link_local_and_the_metadata_service_are_dropped() {
    expect(
        &[
            tcp("metadata", Ipv4Addr::new(169, 254, 169, 254), 80),
            tcp("link-local", Ipv4Addr::new(169, 254, 1, 1), 80),
        ],
        "dropped",
    );
}

/// On a host without IPv6 pasta has none either, and this holds whatever the firewall does; the firewall's unit
/// tests carry its IPv6 rule there.
#[test]
fn ipv6_goes_nowhere() {
    let found = run(&[
        tcp("ipv6-internet", "2606:4700:4700::1111", 443),
        tcp("ipv6-link-local", "fe80::1%eth0", 80),
    ]);
    for (name, result) in found {
        assert_ne!(result, "ok", "{name}");
    }
}

/// The stand-in's side, run by [`run`] through pasta as root of pasta's namespaces: makes the veth pair under the
/// sandbox's guard, moves its own end into a namespace of its own, and tries the probes from there.
#[test]
#[ignore = "run by the other tests, inside pasta"]
fn stand_in() {
    match std::env::var(ROLE).as_deref() {
        Ok("stand-in") => stand_in_host(),
        Ok("guest") => stand_in_guest(),
        _ => {}
    }
}

fn ip(args: &[&str]) -> Result<(), String> {
    let done = Command::new("ip")
        .args(args)
        .output()
        .map_err(|e| format!("ip: {e}"))?;
    if done.status.success() {
        Ok(())
    } else {
        Err(format!(
            "ip {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&done.stderr)
        ))
    }
}

fn stand_in_host() {
    let host: Vec<(cf_nft::Ip, u8)> = std::env::var(HOST)
        .unwrap()
        .split(',')
        .filter(|cidr| !cidr.is_empty())
        .map(|cidr| {
            let (address, prefix) = cidr.split_once('/').unwrap();
            (
                address.parse::<Ipv4Addr>().unwrap().octets(),
                prefix.parse().unwrap(),
            )
        })
        .collect();
    guard(&host, || {
        ip(&[
            "link",
            "add",
            GUEST_DEVICE,
            "type",
            "veth",
            "peer",
            "name",
            STAND_IN_DEVICE,
        ])
    })
    .unwrap();
    let me = std::env::current_exe().unwrap();
    // `unshare` waits for a line before it runs the guest, so that its end of the pair can be moved in first.
    let mut guest = Command::new("unshare")
        .args(["--net", "--", "sh", "-c", "read go && exec \"$0\" \"$@\""])
        .arg(&me)
        .args([
            "--exact",
            "stand_in",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ROLE, "guest")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    // Its namespace is there once its link differs from this one's.
    let ns = |pid: &str| std::fs::read_link(format!("/proc/{pid}/ns/net")).ok();
    let pid = guest.id().to_string();
    let began = std::time::Instant::now();
    let own = ns("self");
    while ns(&pid).is_none_or(|theirs| Some(theirs) == own) && began.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(5));
    }
    ip(&["link", "set", STAND_IN_DEVICE, "netns", &pid]).unwrap();
    guest.stdin.take().unwrap().write_all(b"go\n").unwrap();
    assert!(guest.wait().unwrap().success());
}

fn stand_in_guest() {
    let address = format!("{}/{PREFIX}", Ipv4Addr::from(GUEST));
    let gateway = Ipv4Addr::from(GATEWAY).to_string();
    ip(&["link", "set", "lo", "up"]).unwrap();
    ip(&["link", "set", STAND_IN_DEVICE, "name", "eth0"]).unwrap();
    ip(&["link", "set", "eth0", "address", "52:54:00:0c:f0:01", "up"]).unwrap();
    ip(&["addr", "add", &address, "dev", "eth0"]).unwrap();
    ip(&["route", "add", "default", "via", &gateway]).unwrap();
    // IPv6 as a guest that wanted it would have it: a route out of its device.
    let _ = ip(&["-6", "route", "add", "default", "dev", "eth0"]);
    let probes = std::env::var(PROBES).unwrap();
    let threads: Vec<_> = probes
        .lines()
        .map(|line| {
            let line = line.to_string();
            std::thread::spawn(move || {
                let mut words = line.split_whitespace();
                let name = words.next().unwrap().to_string();
                let found = match (words.next(), words.next(), words.next()) {
                    (Some("tcp"), Some(address), Some(port)) => connect(address, port.parse().unwrap()),
                    (Some("dns"), Some(address), None) => ask(address.parse().unwrap()),
                    _ => format!("unknown-probe:{line}"),
                };
                format!("probe {name} {found}")
            })
        })
        .collect();
    for thread in threads {
        println!("{}", thread.join().unwrap());
    }
}

/// `ok` for a connection that answered, `refused`, `dropped` for no answer at all, or what else went wrong.
fn connect(address: &str, port: u16) -> String {
    let target: SocketAddr = match address.split_once('%') {
        Some((v6, device)) => {
            let scope = std::fs::read_to_string(format!("/sys/class/net/{device}/ifindex"))
                .ok()
                .and_then(|index| index.trim().parse().ok())
                .unwrap_or(0);
            SocketAddr::V6(std::net::SocketAddrV6::new(v6.parse().unwrap(), port, 0, scope))
        }
        None => SocketAddr::new(address.parse::<IpAddr>().unwrap(), port),
    };
    match TcpStream::connect_timeout(&target, WAIT) {
        Ok(mut stream) => {
            // A connection pasta took but could not make on the host ends at once, with nothing read.
            stream.set_read_timeout(Some(WAIT)).unwrap();
            let mut byte = [0u8; 1];
            match stream.read(&mut byte) {
                Ok(1) => "ok".into(),
                Ok(_) => "closed".into(),
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => "refused".into(),
                Err(_) => "ok".into(),
            }
        }
        Err(error) => match error.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => "dropped".into(),
            std::io::ErrorKind::ConnectionRefused => "refused".into(),
            kind => format!("error:{kind:?}"),
        },
    }
}

/// `ok` when the name server at `server` answered a question for github.com's address with one, `no-answer` when it
/// answered without one, else `dropped`. The same question goes out again after each [`WAIT`] without an answer and
/// after a server failure, and a late answer to an earlier one counts.
fn ask(server: Ipv4Addr) -> String {
    let mut question = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in ["github", "com"] {
        question.push(label.len() as u8);
        question.extend_from_slice(label.as_bytes());
    }
    question.extend_from_slice(&[0, 0, 1, 0, 1]);
    let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
    socket.set_read_timeout(Some(WAIT)).unwrap();
    let mut found = "dropped";
    for asked in 1..=ASKS {
        socket.send_to(&question, (server, 53)).unwrap();
        let mut answer = [0u8; 512];
        match socket.recv(&mut answer) {
            // A server failure (SERVFAIL): the host's resolver gave up upstream, and a resolver asks again, [`WAIT`]
            // later, so upstream has the time to recover that a question without an answer gets.
            Ok(read) if read > 12 && answer[..2] == [0x12, 0x34] && answer[3] & 0x0f == 2 => {
                found = "no-answer";
                if asked < ASKS {
                    std::thread::sleep(WAIT);
                }
            }
            Ok(read) if read > 12 && answer[..2] == [0x12, 0x34] => {
                return if u16::from_be_bytes([answer[6], answer[7]]) > 0 {
                    "ok".into()
                } else {
                    "no-answer".into()
                };
            }
            // Not the answer to this question: ask again, as a resolver would.
            Ok(_) | Err(_) => {}
        }
    }
    found.into()
}
