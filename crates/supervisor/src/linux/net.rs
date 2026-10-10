//! The guest's network: the device up, an address from the host's DHCP, and the firewall in the kernel.

use super::sys::{self, check, context};
use cf_supervisor::dhcp::{self, Lease, Reply};
use cf_supervisor::firewall;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

const DEVICE: &str = "eth0";
/// How long the host's DHCP may take in all.
const DHCP_WAIT: Duration = Duration::from_secs(12);
/// How long one question waits for its answer before it is asked again.
const DHCP_RETRY: Duration = Duration::from_millis(700);

/// The kernel's `struct ifreq`: a device's name and one value for it.
#[repr(C)]
struct IfReq {
    name: [u8; 16],
    value: [u8; 24],
}

impl IfReq {
    fn new(device: &str) -> Self {
        let mut name = [0; 16];
        name[..device.len()].copy_from_slice(device.as_bytes());
        Self { name, value: [0; 24] }
    }

    /// With a `sockaddr_in` of this address as the value.
    fn address(device: &str, address: [u8; 4]) -> Self {
        let mut request = Self::new(device);
        request.value[..2].copy_from_slice(&(libc::AF_INET as u16).to_ne_bytes());
        request.value[4..8].copy_from_slice(&address);
        request
    }
}

/// The kernel's `struct rtentry`, for the one route the guest adds.
#[repr(C)]
struct RtEntry {
    pad1: libc::c_ulong,
    destination: libc::sockaddr,
    gateway: libc::sockaddr,
    mask: libc::sockaddr,
    flags: libc::c_ushort,
    pad2: libc::c_short,
    pad3: libc::c_ulong,
    tos: libc::c_uchar,
    class: libc::c_uchar,
    pad4: [libc::c_short; 3],
    metric: libc::c_short,
    device: *mut libc::c_char,
    mtu: libc::c_ulong,
    window: libc::c_ulong,
    irtt: libc::c_ushort,
}

fn socket(domain: libc::c_int, kind: libc::c_int, protocol: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: socket takes no pointers.
    let raw = check(unsafe { libc::socket(domain, kind | libc::SOCK_CLOEXEC, protocol) })?;
    // SAFETY: the descriptor was just made and belongs to nobody else.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn ioctl(control: &OwnedFd, request: libc::c_ulong, value: &mut IfReq) -> io::Result<()> {
    // SAFETY: each request used here reads or fills a `struct ifreq`, which `IfReq` is.
    check(unsafe { libc::ioctl(control.as_raw_fd(), request as _, value as *mut IfReq) }).map(drop)
}

fn bring_up(control: &OwnedFd, device: &str) -> io::Result<()> {
    let mut request = IfReq::new(device);
    ioctl(control, libc::SIOCGIFFLAGS as libc::c_ulong, &mut request)?;
    let flags = i16::from_ne_bytes([request.value[0], request.value[1]]) | libc::IFF_UP as i16;
    request.value[..2].copy_from_slice(&flags.to_ne_bytes());
    ioctl(control, libc::SIOCSIFFLAGS as libc::c_ulong, &mut request)
}

fn sockaddr(address: [u8; 4]) -> libc::sockaddr {
    let mut data = [0; 14];
    for (to, from) in data[2..6].iter_mut().zip(address) {
        *to = from as libc::c_char;
    }
    libc::sockaddr {
        sa_family: libc::AF_INET as libc::sa_family_t,
        sa_data: data,
    }
}

fn timeout(socket: &OwnedFd, after: Duration) {
    let time = libc::timeval {
        tv_sec: after.as_secs() as _,
        tv_usec: after.subsec_micros() as _,
    };
    // SAFETY: setsockopt reads the timeval we own, of the size we give.
    unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&raw const time).cast(),
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
}

/// The DHCP payload of an IPv4 packet for the client port, if it is one.
fn dhcp_payload(packet: &[u8]) -> Option<&[u8]> {
    let header = usize::from(packet.first()? & 0x0f) * 4;
    if packet[0] >> 4 != 4 || header < 20 || *packet.get(9)? != 17 {
        return None;
    }
    let udp = packet.get(header..)?;
    (udp.len() >= 8 && u16::from_be_bytes([udp[2], udp[3]]) == 68).then(|| &udp[8..])
}

/// Asks the host's DHCP for an address. Questions leave through an ordinary socket bound to the device; answers
/// are read below IP, since a server may send them to the address the guest does not have yet.
fn lease(mac: [u8; 6]) -> Result<Lease, String> {
    let sender = context("DHCP socket", socket(libc::AF_INET, libc::SOCK_DGRAM, 0))?;
    let one: libc::c_int = 1;
    let device = sys::cstr(DEVICE);
    // SAFETY: both options read a value we own, of the size we give.
    unsafe {
        libc::setsockopt(
            sender.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_BROADCAST,
            (&raw const one).cast(),
            4,
        );
        libc::setsockopt(
            sender.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            device.as_ptr().cast(),
            DEVICE.len() as libc::socklen_t + 1,
        );
    }
    let address = |port: u16, to: [u8; 4]| libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: port.to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(to),
        },
        sin_zero: [0; 8],
    };
    let (own, everyone) = (address(68, [0; 4]), address(67, [255; 4]));
    let length = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    // SAFETY: bind reads `length` bytes of the address we own.
    context(
        "DHCP bind",
        check(unsafe { libc::bind(sender.as_raw_fd(), (&raw const own).cast(), length) }),
    )?;
    let receiver = context(
        "packet socket",
        socket(
            libc::AF_PACKET,
            libc::SOCK_DGRAM,
            (libc::ETH_P_IP as u16).to_be() as libc::c_int,
        ),
    )?;
    timeout(&receiver, Duration::from_millis(100));

    let send = |packet: &[u8]| {
        // SAFETY: sendto reads the packet and `length` bytes of the address, both ours.
        unsafe {
            libc::sendto(
                sender.as_raw_fd(),
                packet.as_ptr().cast(),
                packet.len(),
                0,
                (&raw const everyone).cast(),
                length,
            )
        };
    };
    let xid =
        (sys::uptime_ms() as u32).rotate_left(16) ^ u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]);
    let deadline = Instant::now() + DHCP_WAIT;
    let mut offer: Option<Lease> = None;
    let mut asked = None;
    let mut buffer = [0u8; 1500];
    while Instant::now() < deadline {
        if asked.is_none_or(|at: Instant| at.elapsed() >= DHCP_RETRY) {
            match &offer {
                Some(offer) => send(&dhcp::request(xid, mac, offer)),
                None => send(&dhcp::discover(xid, mac)),
            }
            asked = Some(Instant::now());
        }
        // SAFETY: recv fills at most the buffer's length.
        let read = unsafe { libc::recv(receiver.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len(), 0) };
        let Some(payload) = usize::try_from(read)
            .ok()
            .and_then(|read| dhcp_payload(&buffer[..read]))
        else {
            continue;
        };
        match dhcp::parse(payload, xid, mac) {
            Some(Reply::Offer(lease)) if offer.is_none() => {
                offer = Some(lease);
                asked = None;
            }
            Some(Reply::Ack(lease)) if offer.is_some() => return Ok(lease),
            Some(Reply::Nak) => {
                offer = None;
                asked = None;
            }
            _ => {}
        }
    }
    Err("the host's DHCP gave no address".into())
}

/// What the guest's network turned out to be.
pub struct Network {
    /// The name servers a job may ask.
    pub name_servers: Vec<[u8; 4]>,
    /// The guest's own address.
    pub address: [u8; 4],
    /// How long the address took, for the console log.
    pub took: Duration,
}

/// Brings the network up and loads the firewall. An error leaves the job without a start: a job never runs
/// without the firewall.
pub fn up() -> Result<Network, String> {
    let started = Instant::now();
    let control = context("control socket", socket(libc::AF_INET, libc::SOCK_DGRAM, 0))?;
    context("loopback", bring_up(&control, "lo"))?;
    let class = format!("/sys/class/net/{DEVICE}");
    for _ in 0..200 {
        if fs::metadata(&class).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // IPv6 stays on the loopback device only.
    for scope in [DEVICE, "default"] {
        let _ = fs::write(format!("/proc/sys/net/ipv6/conf/{scope}/disable_ipv6"), "1");
    }
    context("network device", bring_up(&control, DEVICE))?;
    let mac_text = context("network device", fs::read_to_string(format!("{class}/address")))?;
    let mut mac = [0u8; 6];
    for (byte, text) in mac.iter_mut().zip(mac_text.trim().split(':')) {
        *byte = u8::from_str_radix(text, 16).map_err(|_| "the network device has no address".to_string())?;
    }

    let lease = lease(mac)?;
    context(
        "address",
        ioctl(
            &control,
            libc::SIOCSIFADDR as libc::c_ulong,
            &mut IfReq::address(DEVICE, lease.address),
        ),
    )?;
    context(
        "netmask",
        ioctl(
            &control,
            libc::SIOCSIFNETMASK as libc::c_ulong,
            &mut IfReq::address(DEVICE, lease.netmask),
        ),
    )?;
    if let Some(gateway) = lease.gateway {
        let route = RtEntry {
            pad1: 0,
            destination: sockaddr([0; 4]),
            gateway: sockaddr(gateway),
            mask: sockaddr([0; 4]),
            flags: libc::RTF_UP | libc::RTF_GATEWAY,
            pad2: 0,
            pad3: 0,
            tos: 0,
            class: 0,
            pad4: [0; 3],
            metric: 0,
            device: std::ptr::null_mut(),
            mtu: 0,
            window: 0,
            irtt: 0,
        };
        // SAFETY: SIOCADDRT reads a `struct rtentry`, which `RtEntry` is, and the null device name it allows.
        context(
            "route",
            check(unsafe { libc::ioctl(control.as_raw_fd(), libc::SIOCADDRT as _, &raw const route) })
                .map(drop),
        )?;
    }
    let name_servers = if lease.dns.is_empty() {
        lease.gateway.into_iter().collect()
    } else {
        lease.dns.clone()
    };
    load_firewall(&firewall::netlink(&firewall::policy(&name_servers)))?;
    Ok(Network {
        name_servers,
        address: lease.address,
        took: started.elapsed(),
    })
}

/// Hands the firewall to the kernel and reads its answers: every message acknowledged, or the first refusal.
fn load_firewall(batch: &firewall::Batch) -> Result<(), String> {
    const NETLINK_NETFILTER: libc::c_int = 12;
    const NLMSG_ERROR: u16 = 2;
    let netlink = context(
        "firewall socket",
        socket(libc::AF_NETLINK, libc::SOCK_RAW, NETLINK_NETFILTER),
    )?;
    timeout(&netlink, Duration::from_secs(3));
    // SAFETY: send reads the batch's bytes, which we own.
    let sent = unsafe {
        libc::send(
            netlink.as_raw_fd(),
            batch.bytes.as_ptr().cast(),
            batch.bytes.len(),
            0,
        )
    };
    if sent != batch.bytes.len() as isize {
        return Err(format!("firewall: {}", io::Error::last_os_error()));
    }
    let mut acknowledged = 0;
    let mut buffer = vec![0u8; 16384];
    while acknowledged < batch.acknowledged {
        // SAFETY: recv fills at most the buffer's length.
        let read = unsafe { libc::recv(netlink.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len(), 0) };
        let Ok(read) = usize::try_from(read) else {
            return Err(format!(
                "firewall: the kernel did not answer: {}",
                io::Error::last_os_error()
            ));
        };
        let mut messages = &buffer[..read];
        while messages.len() >= 16 {
            let length = u32::from_ne_bytes([messages[0], messages[1], messages[2], messages[3]]) as usize;
            if length < 16 || length > messages.len() {
                break;
            }
            let kind = u16::from_ne_bytes([messages[4], messages[5]]);
            let sequence = u32::from_ne_bytes([messages[8], messages[9], messages[10], messages[11]]);
            if kind == NLMSG_ERROR && length >= 20 {
                let code = i32::from_ne_bytes([messages[16], messages[17], messages[18], messages[19]]);
                if code != 0 {
                    return Err(format!(
                        "firewall: the kernel refused message {sequence}: {}",
                        io::Error::from_raw_os_error(-code)
                    ));
                }
                acknowledged += 1;
            }
            messages = &messages[length.next_multiple_of(4).min(messages.len())..];
        }
    }
    Ok(())
}
