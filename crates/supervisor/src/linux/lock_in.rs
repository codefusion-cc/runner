//! A container's job locked in before it starts: the machine's firewall in the container's own network namespace,
//! limits that are in force, no capability left in any set, the seccomp filter on, and the job's user
//! ([`cf_supervisor::confine`]). This runs as the container's root with the few capabilities it needs for that, and
//! before any other thread: the bounding set and the filter are each thread's own.

use super::init::Failure;
use super::sys::say;
use cf_nft::Ip;
use cf_supervisor::confine::{default_gateway, limits_in_force, locked_down, name_servers};
use cf_supervisor::environment::find_user;
use cf_supervisor::firewall;
use cf_supervisor::seccomp::no_user_namespaces;
use std::fs;
use std::io;

const CGROUP: &str = "/sys/fs/cgroup";

/// Locks the job in as the user `user`, and returns that user's id. Any step that fails is the job's end before
/// it starts.
pub fn lock_in(user: &str, blocked: &[(Ip, u8)]) -> Result<u32, Failure> {
    firewall_up(blocked).map_err(|error| ("firewall", error))?;
    let read = |name: &str| fs::read_to_string(format!("{CGROUP}/{name}")).unwrap_or_default();
    limits_in_force(&read("memory.max"), &read("pids.max")).map_err(|error| ("cgroup", error))?;
    let passwd = fs::read_to_string("/etc/passwd").unwrap_or_default();
    let (uid, gid, _) =
        find_user(&passwd, user).ok_or_else(|| ("container", format!("the image has no user {user}")))?;
    if uid == 0 {
        return Err(("container", format!("the image's {user} is root")));
    }
    drop_bounding_set().map_err(|error| ("capabilities", format!("the bounding set: {error}")))?;
    filter().map_err(|error| ("seccomp", error.to_string()))?;
    become_user(uid, gid).map_err(|error| ("user", error.to_string()))?;
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    locked_down(&status, uid).map_err(|error| ("capabilities", error))?;
    say("the job's firewall, limits, filter and user are in place");
    Ok(uid)
}

/// The machine's firewall, with the name servers the engine wrote and every network next to the container.
fn firewall_up(blocked: &[(Ip, u8)]) -> Result<(), String> {
    let servers = name_servers(&fs::read_to_string("/etc/resolv.conf").unwrap_or_default());
    let gateway = default_gateway(&fs::read_to_string("/proc/net/route").unwrap_or_default());
    let own = firewall::own_networks(&interfaces(), gateway, blocked);
    firewall::netlink(&firewall::policy(&servers, &own))
        .and_then(|batch| cf_nft::apply(&batch))
        .map_err(|error| error.to_string())
}

/// The container's IPv4 addresses with their masks.
fn interfaces() -> Vec<(Ip, Ip)> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs sets the pointer to a list it allocates, or fails and gives none.
    if unsafe { libc::getifaddrs(&raw mut list) } != 0 {
        return Vec::new();
    }
    let ipv4 = |address: *const libc::sockaddr| -> Option<Ip> {
        // SAFETY: a non-null address of the list, which lives until freeifaddrs below; only an AF_INET one is read
        // as the sockaddr_in it then is.
        unsafe {
            (!address.is_null() && i32::from((*address).sa_family) == libc::AF_INET).then(|| {
                (*address.cast::<libc::sockaddr_in>())
                    .sin_addr
                    .s_addr
                    .to_ne_bytes()
            })
        }
    };
    let mut found = Vec::new();
    let mut next = list;
    while !next.is_null() {
        // SAFETY: an entry of the list, which lives until freeifaddrs below.
        let entry = unsafe { &*next };
        next = entry.ifa_next;
        if let (Some(address), Some(netmask)) = (ipv4(entry.ifa_addr), ipv4(entry.ifa_netmask)) {
            found.push((address, netmask));
        }
    }
    // SAFETY: the list getifaddrs gave, freed once; nothing above keeps a pointer into it.
    unsafe { libc::freeifaddrs(list) };
    found
}

/// Takes every capability out of the bounding set, up to the last one this kernel has.
fn drop_bounding_set() -> io::Result<()> {
    for cap in 0..64 {
        // SAFETY: prctl with integer arguments only.
        if unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) } != 0 {
            let error = io::Error::last_os_error();
            // Past the kernel's last capability.
            if error.raw_os_error() == Some(libc::EINVAL) && cap > 0 {
                return Ok(());
            }
            return Err(error);
        }
    }
    Ok(())
}

/// Installs the filter. The container has no new privileges (the engine's `no-new-privileges`), which is what lets
/// a process without `CAP_SYS_ADMIN` install one; without it this fails, and the job does not run.
fn filter() -> io::Result<()> {
    let mut program: Vec<libc::sock_filter> = no_user_namespaces()
        .into_iter()
        .map(|insn| libc::sock_filter {
            code: insn.code,
            jt: insn.jt,
            jf: insn.jf,
            k: insn.k,
        })
        .collect();
    let fprog = libc::sock_fprog {
        len: u16::try_from(program.len()).map_err(|_| io::Error::other("the filter is too long"))?,
        filter: program.as_mut_ptr(),
    };
    // SAFETY: the kernel copies the program, which lives in `program` for the call.
    let set = unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER,
            &raw const fprog,
            0,
            0,
        )
    };
    if set == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The kernel's `struct __user_cap_header_struct` and `__user_cap_data_struct`, version 3.
#[repr(C)]
struct CapHeader {
    version: u32,
    pid: libc::c_int,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

const CAPABILITY_VERSION_3: u32 = 0x2008_0522;

/// Becomes the job's user and group, with no other group, and ends with no capability in any set: the change of
/// user clears the permitted and effective sets, and the inheritable one is cleared here.
fn become_user(uid: u32, gid: u32) -> io::Result<()> {
    let check = |result: libc::c_int| {
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    };
    // SAFETY: setgroups reads one gid we own; the rest take integers.
    unsafe {
        check(libc::setgroups(1, &raw const gid))?;
        check(libc::setgid(gid))?;
        check(libc::setuid(uid))?;
    }
    let header = CapHeader {
        version: CAPABILITY_VERSION_3,
        pid: 0,
    };
    let data = [CapData::default(); 2];
    // SAFETY: capset reads the header and two data structs, which live for the call.
    if unsafe { libc::syscall(libc::SYS_capset, &raw const header, data.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // A change of user makes the process dumpable again: the job, the same user, could open this process's
    // descriptors through /proc and speak to the host in the supervisor's name.
    // SAFETY: prctl with integer arguments only.
    check(unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) })
}
