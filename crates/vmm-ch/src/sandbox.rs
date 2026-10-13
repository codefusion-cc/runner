//! `codefusion-runner vm-sandbox`: what runs between the host and a machine's monitor, inside the machine's cgroup.
//!
//! With a network it runs under pasta, as root of pasta's user and network namespace. It makes the guest's tap
//! device, loads the guest's firewall ([`crate::firewall`]) onto it, and joins it with pasta's device in a bridge.
//! Without one it makes an empty user and network namespace of its own. Either way it then makes a cgroup and a
//! PID namespace and starts cloud-hypervisor in them with no capability left: the monitor cannot change the
//! firewall or the devices, leave its cgroup, or see or signal any process outside its machine. The sandbox waits
//! for it and exits as it did, or with [`crate::ending::SANDBOX_FAILED`] when it could not set up.

use cf_nft::Ip;
use cf_vmm::Network;
use std::ffi::OsString;
use std::net::Ipv4Addr;
use std::path::Path;

/// The hidden subcommand of the program that hosts this crate.
pub const SUBCOMMAND: &str = "vm-sandbox";

/// The most host networks a sandbox is told of.
pub const MAX_NETWORKS: usize = 256;

/// What a sandbox sets up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox {
    /// The machine's network.
    pub network: Network,
    /// The networks of the host's interfaces when the machine started, which the guest does not reach.
    pub host: Vec<(Ip, u8)>,
}

impl Sandbox {
    /// The command that runs `monitor` in this sandbox: `<program> vm-sandbox <nat|none> <networks|-> -- <monitor>`.
    pub fn command(&self, program: &Path, monitor: Vec<OsString>) -> Vec<OsString> {
        let networks: Vec<String> = self
            .host
            .iter()
            .map(|&(network, prefix)| format!("{}/{prefix}", Ipv4Addr::from(network)))
            .collect();
        let mut command: Vec<OsString> = vec![
            program.as_os_str().to_owned(),
            SUBCOMMAND.into(),
            match self.network {
                Network::None => "none",
                Network::Nat => "nat",
            }
            .into(),
            if networks.is_empty() {
                "-".into()
            } else {
                networks.join(",")
            }
            .into(),
            "--".into(),
        ];
        command.extend(monitor);
        command
    }

    /// Reads what follows the subcommand: the sandbox, and the monitor's command line.
    pub fn parse(args: &[OsString]) -> Result<(Sandbox, Vec<OsString>), String> {
        let usage = || format!("usage: {SUBCOMMAND} <nat|none> <networks|-> -- <monitor> [arguments]");
        let [network, host, dash, monitor @ ..] = args else {
            return Err(usage());
        };
        if dash != "--" || monitor.is_empty() {
            return Err(usage());
        }
        let network = match network.to_str() {
            Some("nat") => Network::Nat,
            Some("none") => Network::None,
            _ => return Err(usage()),
        };
        let host = host.to_str().ok_or_else(usage)?;
        let host = if host == "-" {
            Vec::new()
        } else {
            host.split(',')
                .map(parse_network)
                .collect::<Result<Vec<_>, _>>()?
        };
        if host.len() > MAX_NETWORKS {
            return Err(format!("more than {MAX_NETWORKS} host networks"));
        }
        Ok((Sandbox { network, host }, monitor.to_vec()))
    }
}

fn parse_network(text: &str) -> Result<(Ip, u8), String> {
    let refused = || format!("{text:?} is no IPv4 network");
    let (address, prefix) = text.split_once('/').ok_or_else(refused)?;
    let address: Ipv4Addr = address.parse().map_err(|_| refused())?;
    if prefix.is_empty() || prefix.len() > 2 || !prefix.bytes().all(|b| b.is_ascii_digit()) {
        return Err(refused());
    }
    let prefix: u8 = prefix.parse().map_err(|_| refused())?;
    if prefix > 32 {
        return Err(refused());
    }
    Ok((address.octets(), prefix))
}

#[cfg(target_os = "linux")]
pub use linux::main;

#[cfg(target_os = "linux")]
mod linux {
    use super::Sandbox;
    use crate::command::{MTU, PASTA_DEVICE, net};
    use crate::ending::SANDBOX_FAILED;
    use crate::firewall::{self, GUEST_DEVICE};
    use cf_nft::Ip;
    use cf_vmm::Network;
    use std::ffi::OsString;
    use std::fs;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
    use std::process::Command;
    use std::time::{Duration, Instant};

    /// How long pasta's device may take to show up in the namespace.
    const DEVICE_WAIT: Duration = Duration::from_secs(5);

    /// Runs the subcommand with what follows its name and returns the exit code.
    pub fn main(args: &[OsString]) -> i32 {
        match run(args) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("{}: {error}", super::SUBCOMMAND);
                SANDBOX_FAILED
            }
        }
    }

    fn run(args: &[OsString]) -> Result<i32, String> {
        let (sandbox, mut monitor) = Sandbox::parse(args)?;
        // SAFETY: prctl and getppid take no pointers.
        unsafe {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(format!("PR_SET_PDEATHSIG: {}", io::Error::last_os_error()));
            }
            // The parent ended before the line above: nobody is left to end this machine.
            if libc::getppid() == 1 {
                return Err("the host is gone".into());
            }
        }
        let tap = match sandbox.network {
            Network::Nat => {
                let tap = network(&sandbox.host)?;
                monitor.extend(net(tap.as_raw_fd()).map(OsString::from));
                unshare(libc::CLONE_NEWCGROUP | libc::CLONE_NEWPID)?;
                Some(tap)
            }
            Network::None => {
                unshare(
                    libc::CLONE_NEWUSER | libc::CLONE_NEWNET | libc::CLONE_NEWCGROUP | libc::CLONE_NEWPID,
                )?;
                None
            }
        };
        let (program, args) = monitor.split_first().ok_or("no monitor")?;
        let mut command = Command::new(program);
        command.args(args);
        let inherited = tap.as_ref().map(AsRawFd::as_raw_fd);
        // SAFETY: between fork and exec the child makes only system calls: fcntl, prctl and capset.
        unsafe {
            command.pre_exec(move || {
                if let Some(fd) = inherited
                    && libc::fcntl(fd, libc::F_SETFD, 0) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(io::Error::last_os_error());
                }
                drop_capabilities()
            });
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("{}: {e}", program.to_string_lossy()))?;
        drop(tap);
        drop_capabilities().map_err(|e| format!("dropping capabilities: {e}"))?;
        let status = child
            .wait()
            .map_err(|e| format!("waiting for the monitor: {e}"))?;
        Ok(status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
    }

    fn unshare(flags: libc::c_int) -> Result<(), String> {
        // SAFETY: unshare takes no pointers.
        if unsafe { libc::unshare(flags) } != 0 {
            return Err(format!("unshare: {}", io::Error::last_os_error()));
        }
        Ok(())
    }

    /// The guest's side of the namespace pasta made: its tap device behind the firewall, in a bridge with pasta's.
    fn network(host: &[(Ip, u8)]) -> Result<OwnedFd, String> {
        let began = Instant::now();
        while crate::link::index(PASTA_DEVICE).is_err() {
            if began.elapsed() > DEVICE_WAIT {
                return Err(format!("pasta's device {PASTA_DEVICE} is not there"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        for file in ["all", "default"] {
            let path = format!("/proc/sys/net/ipv6/conf/{file}/disable_ipv6");
            match fs::write(&path, "1") {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    return Err(format!("{path}: {error}"));
                }
                _ => {}
            }
        }
        let tap = tap(GUEST_DEVICE)?;
        let table = firewall::table(host).map_err(|e| e.to_string())?;
        let batch =
            cf_nft::batch(std::slice::from_ref(&table), cf_nft::Load::Replace).map_err(|e| e.to_string())?;
        cf_nft::apply(&batch).map_err(|e| format!("the guest's firewall: {e}"))?;
        let loaded = cf_nft::list().map_err(|e| format!("the guest's firewall, read back: {e}"))?;
        if loaded != [table] {
            return Err(format!("the guest's firewall reads back as {loaded:?}"));
        }
        crate::link::join(&[PASTA_DEVICE, GUEST_DEVICE], MTU)?;
        Ok(tap)
    }

    /// A tap device of this name, which lives as long as the descriptor: frames with a virtio-net header, no
    /// packet information.
    fn tap(name: &str) -> Result<OwnedFd, String> {
        const TUNSETIFF: libc::c_ulong = 0x4004_54ca;
        const IFF_TAP: libc::c_short = 0x0002;
        const IFF_NO_PI: libc::c_short = 0x1000;
        const IFF_VNET_HDR: libc::c_short = 0x4000;
        // SAFETY: the path is a NUL terminated literal.
        let raw = unsafe { libc::open(c"/dev/net/tun".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if raw < 0 {
            return Err(format!("/dev/net/tun: {}", io::Error::last_os_error()));
        }
        // SAFETY: the descriptor was just opened and belongs to nobody else.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: `ifreq` is plain data, all zeros a valid value.
        let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
        if name.is_empty() || name.len() >= request.ifr_name.len() {
            return Err(format!("{name:?} is no device name"));
        }
        for (to, from) in request.ifr_name.iter_mut().zip(name.bytes()) {
            *to = from as libc::c_char;
        }
        request.ifr_ifru.ifru_flags = IFF_TAP | IFF_NO_PI | IFF_VNET_HDR;
        // SAFETY: TUNSETIFF reads and writes an `ifreq`, which lives through the call.
        if unsafe { libc::ioctl(fd.as_raw_fd(), TUNSETIFF as _, &raw mut request) } != 0 {
            return Err(format!("tap {name}: {}", io::Error::last_os_error()));
        }
        Ok(fd)
    }

    /// Leaves this process no capability, now or after an exec, and no way to gain one: the bounding and ambient
    /// sets emptied, root's special treatment off and locked, `no_new_privs` set. Only system calls, so it may run
    /// between fork and exec.
    fn drop_capabilities() -> io::Result<()> {
        const SECURE_BITS: libc::c_ulong = 0xef; // NOROOT, NO_SETUID_FIXUP, KEEP_CAPS_LOCKED, NO_CAP_AMBIENT_RAISE, each locked
        const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
        let fail = || Err(io::Error::last_os_error());
        // SAFETY: prctl takes no pointers in these calls.
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return fail();
            }
            for capability in 0..64 {
                if libc::prctl(libc::PR_CAPBSET_DROP, capability, 0, 0, 0) != 0
                    && *libc::__errno_location() != libc::EINVAL
                {
                    return fail();
                }
            }
            if libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0) != 0 {
                return fail();
            }
            if libc::prctl(libc::PR_SET_SECUREBITS, SECURE_BITS, 0, 0, 0) != 0 {
                return fail();
            }
        }
        let header = [LINUX_CAPABILITY_VERSION_3, 0];
        let data = [0u32; 6];
        // SAFETY: capset reads a version 3 header and two sets of (effective, permitted, inheritable) words, both of
        // which live through the call.
        if unsafe { libc::syscall(libc::SYS_capset, header.as_ptr(), data.as_ptr()) } != 0 {
            return fail();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn strings(args: &[OsString]) -> Vec<&str> {
        args.iter().map(|arg| arg.to_str().unwrap()).collect()
    }

    #[test]
    fn a_sandbox_is_written_as_its_subcommand_and_read_back() {
        let sandbox = Sandbox {
            network: Network::Nat,
            host: vec![([192, 168, 1, 0], 24), ([172, 17, 0, 0], 16)],
        };
        let command = sandbox.command(
            Path::new("/bin/runner"),
            vec!["/bin/ch".into(), "--kernel".into()],
        );
        assert_eq!(
            strings(&command),
            [
                "/bin/runner",
                SUBCOMMAND,
                "nat",
                "192.168.1.0/24,172.17.0.0/16",
                "--",
                "/bin/ch",
                "--kernel"
            ]
        );
        assert_eq!(
            Sandbox::parse(&command[2..]),
            Ok((sandbox, vec!["/bin/ch".into(), "--kernel".into()]))
        );
        let none = Sandbox {
            network: Network::None,
            host: vec![],
        };
        let command = none.command(Path::new("/r"), vec!["/ch".into()]);
        assert_eq!(strings(&command[2..]), ["none", "-", "--", "/ch"]);
        assert_eq!(Sandbox::parse(&command[2..]), Ok((none, vec!["/ch".into()])));
    }

    #[test]
    fn what_is_not_a_sandbox_is_refused() {
        let cases: &[&[&str]] = &[
            &[],
            &["nat", "-", "--"],
            &["nat", "-", "-x", "/ch"],
            &["bridge", "-", "--", "/ch"],
            &["nat", "10.0.0.0/33", "--", "/ch"],
            &["nat", "10.0.0/8", "--", "/ch"],
            &["nat", "10.0.0.0/+8", "--", "/ch"],
            &["nat", "10.0.0.0/008", "--", "/ch"],
            &["nat", "10.0.0.0", "--", "/ch"],
            &["nat", "10.0.0.0/8,", "--", "/ch"],
            &["nat", "", "--", "/ch"],
            &["nat", "::1/128", "--", "/ch"],
        ];
        for case in cases {
            let args: Vec<OsString> = case.iter().map(OsString::from).collect();
            assert!(Sandbox::parse(&args).is_err(), "{case:?} was taken");
        }
        let many = vec!["10.0.0.0/8"; MAX_NETWORKS + 1].join(",");
        let args: Vec<OsString> = ["nat", &many, "--", "/ch"].iter().map(OsString::from).collect();
        assert!(Sandbox::parse(&args).is_err());
    }

    proptest! {
        #[test]
        fn any_sandbox_reads_back_as_written(
            nat: bool,
            host in proptest::collection::vec((any::<[u8; 4]>(), 0u8..=32), 0..MAX_NETWORKS),
            monitor in proptest::collection::vec("[ -~]{1,20}", 1..6),
        ) {
            let sandbox = Sandbox { network: if nat { Network::Nat } else { Network::None }, host };
            let monitor: Vec<OsString> = monitor.into_iter().map(OsString::from).collect();
            let command = sandbox.command(Path::new("/runner"), monitor.clone());
            prop_assert_eq!(Sandbox::parse(&command[2..]), Ok((sandbox, monitor)));
        }
    }
}
