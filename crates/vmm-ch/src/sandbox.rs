//! `codefusion-runner vm-sandbox`: what runs between the host and a machine's monitor, inside the machine's cgroup.
//!
//! With a network it runs under pasta, as root of pasta's user and network namespace. It makes the guest's tap
//! device, loads the guest's firewall ([`crate::firewall`]) onto it, and joins it with pasta's device in a bridge.
//! Without one it makes an empty user and network namespace of its own. Either way it then makes a cgroup, a PID
//! and a mount namespace and starts cloud-hypervisor in them with no capability left. The monitor's root is a
//! read-only tmpfs that holds the files and folders it is shown ([`Shown`]), each at its own path, and a `/proc` of
//! its own: it sees no other file of the host or of the user it runs as, and no process but itself. It cannot
//! change the firewall or the devices, leave its cgroup, or see or signal any process outside its machine. The
//! sandbox waits for it, writes how it ended ([`crate::ending::report`]) and exits as it did, or with
//! [`crate::ending::SANDBOX_FAILED`] when it could not set up.

use cf_nft::Ip;
use cf_vmm::{Network, VmSpec};
use std::ffi::{OsStr, OsString};
use std::net::Ipv4Addr;
use std::path::{Component, Path, PathBuf};

/// The hidden subcommand of the program that hosts this crate.
pub const SUBCOMMAND: &str = "vm-sandbox";

/// The most host networks a sandbox is told of.
pub const MAX_NETWORKS: usize = 256;

/// The most files and folders a monitor is shown.
pub const MAX_SHOWN: usize = 64;

/// Where the monitor's own `/proc` is, which no shown path may be in.
const PROC: &str = "/proc";

/// The folder in a machine's own folder that the monitor's root is mounted on.
pub const ROOT: &str = "root";

/// The host's devices cloud-hypervisor opens: KVM, and the kernel's random numbers.
pub const DEVICES: [(&str, Access); 2] = [("/dev/kvm", Access::Write), ("/dev/urandom", Access::Read)];

/// What cloud-hypervisor reads of the host where it is there: the first CPU's caches, which it tells the guest of.
pub const HOST_FACTS: [&str; 1] = ["/sys/devices/system/cpu/cpu0/cache"];

/// How the monitor may use a file or folder of the host it is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Read it, and nothing else: no write, and no program run from it.
    Read,
    /// Read and write it, a device included, but run no program from it.
    Write,
    /// Read it and run it: the monitor's own program.
    Run,
}

impl Access {
    fn word(self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::Write => "write",
            Access::Run => "run",
        }
    }
}

/// A file or folder of the host the monitor sees at the same path. A folder comes alone, without what is mounted
/// below it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shown {
    /// Its absolute path, without `.` or `..`.
    pub path: PathBuf,
    /// What the monitor may do with it.
    pub access: Access,
}

/// What a sandbox sets up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox {
    /// The machine's network.
    pub network: Network,
    /// The networks of the host's interfaces when the machine started, which the guest does not reach.
    pub host: Vec<(Ip, u8)>,
    /// A folder of the machine's own, which the sandbox makes if it is not there and mounts the monitor's root on.
    pub root: PathBuf,
    /// What of the host the monitor sees ([`Sandbox::show`]).
    pub shown: Vec<Shown>,
}

impl Sandbox {
    /// A sandbox that shows the monitor nothing yet.
    pub fn new(network: Network, host: Vec<(Ip, u8)>, root: PathBuf) -> Sandbox {
        Sandbox {
            network,
            host,
            root,
            shown: Vec::new(),
        }
    }

    /// The sandbox of a machine whose monitor is `program` and whose own folder is `dir`: it shows the monitor its
    /// program, the guest's kernel, initramfs and disks, its console, its folder and [`DEVICES`], and roots it in
    /// [`ROOT`] in its folder.
    pub fn for_machine(spec: &VmSpec, program: &Path, dir: &Path, host: Vec<(Ip, u8)>) -> Sandbox {
        let mut sandbox = Sandbox::new(spec.network, host, dir.join(ROOT));
        sandbox.show(program, Access::Run);
        sandbox.show(&spec.kernel, Access::Read);
        sandbox.show(&spec.initramfs, Access::Read);
        for disk in &spec.disks {
            let access = if disk.read_only {
                Access::Read
            } else {
                Access::Write
            };
            sandbox.show(&disk.path, access);
        }
        sandbox.show(&spec.console, Access::Write);
        sandbox.show(dir, Access::Write);
        for (path, access) in DEVICES {
            sandbox.show(path, access);
        }
        sandbox
    }

    /// Shows the monitor `path` as `access` says. A path shown already is shown once, writable if either time
    /// said so.
    pub fn show(&mut self, path: impl Into<PathBuf>, access: Access) {
        let path = path.into();
        match self.shown.iter_mut().find(|shown| shown.path == path) {
            Some(shown) if access == Access::Write || shown.access == Access::Write => {
                shown.access = Access::Write;
            }
            Some(shown) if access == Access::Run => shown.access = Access::Run,
            Some(_) => {}
            None => self.shown.push(Shown { path, access }),
        }
    }

    /// The command that runs `monitor` in this sandbox:
    /// `<program> vm-sandbox <nat|none> <networks|-> <root> [<read|write|run> <path>]... -- <monitor>`.
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
            self.root.as_os_str().to_owned(),
        ];
        for shown in &self.shown {
            command.push(shown.access.word().into());
            command.push(shown.path.as_os_str().to_owned());
        }
        command.push("--".into());
        command.extend(monitor);
        command
    }

    /// Reads what follows the subcommand: the sandbox, and the monitor's command line.
    pub fn parse(args: &[OsString]) -> Result<(Sandbox, Vec<OsString>), String> {
        let usage = || {
            format!(
                "usage: {SUBCOMMAND} <nat|none> <networks|-> <root> [<read|write|run> <path>]... -- <monitor> \
                 [arguments]"
            )
        };
        let [network, host, root, rest @ ..] = args else {
            return Err(usage());
        };
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
        let root = plain(root)?;
        let mut shown: Vec<Shown> = Vec::new();
        let mut rest = rest;
        let monitor = loop {
            match rest {
                [dash, monitor @ ..] if dash == "--" => break monitor,
                [access, path, more @ ..] => {
                    let access = match access.to_str() {
                        Some("read") => Access::Read,
                        Some("write") => Access::Write,
                        Some("run") => Access::Run,
                        _ => return Err(usage()),
                    };
                    shown.push(Shown {
                        path: plain(path)?,
                        access,
                    });
                    rest = more;
                }
                _ => return Err(usage()),
            }
        };
        if monitor.is_empty() {
            return Err(usage());
        }
        if shown.len() > MAX_SHOWN {
            return Err(format!("more than {MAX_SHOWN} paths shown"));
        }
        for (index, one) in shown.iter().enumerate() {
            let shows = |path: &Path| path.display().to_string();
            if one.path.starts_with(&root) {
                return Err(format!("{} is the monitor's root, or in it", shows(&one.path)));
            }
            if one.path.starts_with(PROC) {
                return Err(format!("{} is in the monitor's own {PROC}", shows(&one.path)));
            }
            if let Some(other) = shown
                .iter()
                .enumerate()
                .find(|&(at, other)| at != index && one.path.starts_with(&other.path))
                .map(|(_, other)| other)
            {
                return Err(if other.path == one.path {
                    format!("{} is shown twice", shows(&one.path))
                } else {
                    format!(
                        "{} is in {}, which is shown",
                        shows(&one.path),
                        shows(&other.path)
                    )
                });
            }
        }
        Ok((
            Sandbox {
                network,
                host,
                root,
                shown,
            },
            monitor.to_vec(),
        ))
    }
}

/// A path as the sandbox takes it: absolute, below `/`, without `.`, `..` or a NUL, so that it names the same place
/// below the monitor's root as on the host.
fn plain(path: &OsStr) -> Result<PathBuf, String> {
    let path = Path::new(path);
    let mut components = path.components();
    let plain = components.next() == Some(Component::RootDir)
        && path.components().count() > 1
        && components.all(|component| matches!(component, Component::Normal(_)))
        && !path.as_os_str().as_encoded_bytes().contains(&0)
        // `Path::components` drops a `.` within the path; the monitor's options would not.
        && !path
            .as_os_str()
            .as_encoded_bytes()
            .split(|&byte| byte == b'/')
            .any(|part| part == b"." || part == b"..");
    if !plain {
        return Err(format!("{:?} is no plain absolute path", path));
    }
    Ok(path.components().collect())
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
pub use linux::{guard, main};

#[cfg(target_os = "linux")]
mod linux {
    use super::{Access, Sandbox, Shown};
    use crate::command::{MTU, PASTA_DEVICE, net};
    use crate::ending::{SANDBOX_FAILED, Status, report};
    use crate::firewall::{self, GUEST_DEVICE};
    use cf_nft::Ip;
    use cf_vmm::Network;
    use std::ffi::{CStr, CString, OsString};
    use std::fs;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
    use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
    use std::path::Path;
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
        // SAFETY: prctl, getppid and close_range take no pointers.
        unsafe {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(format!("PR_SET_PDEATHSIG: {}", io::Error::last_os_error()));
            }
            // The parent ended before the line above: nobody is left to end this machine.
            if libc::getppid() == 1 {
                return Err("the host is gone".into());
            }
            // No descriptor this process was handed reaches the monitor, which could reach the host's files through
            // one: only the standard three and the tap device, made below.
            if libc::syscall(libc::SYS_close_range, 3, u32::MAX, libc::CLOSE_RANGE_CLOEXEC) != 0 {
                return Err(format!("close_range: {}", io::Error::last_os_error()));
            }
        }
        let namespaces = libc::CLONE_NEWCGROUP | libc::CLONE_NEWPID | libc::CLONE_NEWNS;
        let tap = match sandbox.network {
            Network::Nat => {
                let tap = network(&sandbox.host)?;
                monitor.extend(net(tap.as_raw_fd()).map(OsString::from));
                unshare(namespaces)?;
                Some(tap)
            }
            Network::None => {
                // SAFETY: getuid and getgid take no pointers.
                let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
                unshare(namespaces | libc::CLONE_NEWUSER | libc::CLONE_NEWNET)?;
                // Root of the namespace is this user outside it, as under pasta: the files it makes are this
                // user's, and the mount namespace is its own.
                for (file, line) in [
                    ("setgroups", "deny".to_string()),
                    ("uid_map", format!("0 {uid} 1")),
                    ("gid_map", format!("0 {gid} 1")),
                ] {
                    fs::write(format!("/proc/self/{file}"), line).map_err(|e| format!("{file}: {e}"))?;
                }
                None
            }
        };
        let root = build_root(&sandbox.root, &sandbox.shown)?;
        // The monitor's root becomes this process's too; nothing of the host's stays open here.
        std::env::set_current_dir("/").map_err(|e| format!("/: {e}"))?;
        let (program, args) = monitor.split_first().ok_or("no monitor")?;
        let mut command = Command::new(program);
        command.args(args);
        let inherited = tap.as_ref().map(AsRawFd::as_raw_fd);
        // SAFETY: between fork and exec the child makes only system calls, on strings made before the fork: fcntl,
        // prctl, mount, chdir, pivot_root, umount2 and capset.
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
                root.enter()?;
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
        let ended = match status.code() {
            Some(code) => Status::Code(code),
            None => Status::Signal(status.signal().unwrap_or(0)),
        };
        // The monitor was the first process of its PID namespace: all of it is gone, and this line comes last.
        eprintln!("{}", report(ended));
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

    /// What `mount` says it is to do, with no pointer it would read.
    fn mount(
        source: Option<&CStr>,
        target: &CStr,
        kind: Option<&CStr>,
        flags: libc::c_ulong,
        data: Option<&CStr>,
    ) -> io::Result<()> {
        let pointer = |text: Option<&CStr>| text.map_or(std::ptr::null(), CStr::as_ptr);
        // SAFETY: every pointer is null or a NUL terminated string that lives through the call.
        let done = unsafe {
            libc::mount(
                pointer(source),
                target.as_ptr(),
                pointer(kind),
                flags,
                pointer(data).cast(),
            )
        };
        if done == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn c_path(path: &Path) -> Result<CString, String> {
        CString::new(path.as_os_str().as_bytes()).map_err(|_| format!("{} holds a NUL", path.display()))
    }

    /// The monitor's root, made and ready for the monitor's process to enter ([`Root::enter`]).
    struct Root {
        root: CString,
        proc: CString,
    }

    impl Root {
        /// Run in the monitor's process, the first of its PID namespace, between fork and exec: mounts its own
        /// `/proc`, which lists no process but its own, while the host's still shows that it may; then makes the
        /// root its root and lets go of the host's. Only system calls, on strings made before.
        fn enter(&self) -> io::Result<()> {
            mount(
                Some(c"proc"),
                &self.proc,
                Some(c"proc"),
                libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                Some(c"subset=pid"),
            )?;
            let fail = || Err(io::Error::last_os_error());
            // SAFETY: chdir, pivot_root and umount2 read NUL terminated strings that live through each call.
            unsafe {
                if libc::chdir(self.root.as_ptr()) != 0 {
                    return fail();
                }
                // The host's root goes on top of the new one, at the same place, and is let go of at once.
                if libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c".".as_ptr()) != 0 {
                    return fail();
                }
                if libc::umount2(c".".as_ptr(), libc::MNT_DETACH) != 0 {
                    return fail();
                }
                if libc::chdir(c"/".as_ptr()) != 0 {
                    return fail();
                }
            }
            Ok(())
        }
    }

    /// Makes the monitor's root on `root` in this mount namespace: a tmpfs with a `/proc` to mount on and each shown
    /// path at its own place, read-only once it is made.
    fn build_root(root: &Path, shown: &[Shown]) -> Result<Root, String> {
        let say = |what: &Path, error: io::Error| format!("the monitor's root, {}: {error}", what.display());
        // What is mounted here from now on stays here.
        mount(None, c"/", None, libc::MS_REC | libc::MS_PRIVATE, None).map_err(|e| say(Path::new("/"), e))?;
        match fs::DirBuilder::new().mode(0o700).create(root) {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(say(root, error)),
            _ => {}
        }
        let c_root = c_path(root)?;
        mount(
            Some(c"tmpfs"),
            &c_root,
            Some(c"tmpfs"),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            Some(c"mode=0755,size=64k,nr_inodes=1024"),
        )
        .map_err(|e| say(root, e))?;
        let proc = root.join(&super::PROC[1..]);
        fs::create_dir(&proc).map_err(|e| say(&proc, e))?;
        let mut shown = shown.to_vec();
        // A folder that holds another's comes first.
        shown.sort_by(|one, other| one.path.cmp(&other.path));
        for one in &shown {
            show(root, one).map_err(|e| say(&one.path, e))?;
        }
        mount(
            None,
            &c_root,
            None,
            libc::MS_BIND
                | libc::MS_REMOUNT
                | libc::MS_RDONLY
                | libc::MS_NOSUID
                | libc::MS_NODEV
                | libc::MS_NOEXEC,
            None,
        )
        .map_err(|e| say(root, e))?;
        Ok(Root {
            proc: c_path(&proc)?,
            root: c_root,
        })
    }

    /// Mounts what `shown` names at the same path below `root`, alone, and then as its access allows.
    fn show(root: &Path, shown: &Shown) -> io::Result<()> {
        let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidInput, what.to_string());
        let below = shown
            .path
            .strip_prefix("/")
            .map_err(|_| invalid("not absolute"))?;
        let target = root.join(below);
        let folder = fs::metadata(&shown.path)?.is_dir();
        if let Some(parent) = target.parent() {
            fs::DirBuilder::new().recursive(true).mode(0o755).create(parent)?;
        }
        if folder {
            fs::DirBuilder::new().mode(0o755).create(&target)?;
        } else {
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&target)?;
        }
        let source = CString::new(shown.path.as_os_str().as_bytes()).map_err(|_| invalid("holds a NUL"))?;
        let target = CString::new(target.as_os_str().as_bytes()).map_err(|_| invalid("holds a NUL"))?;
        mount(Some(&source), &target, None, libc::MS_BIND, None)?;
        // SAFETY: `statvfs` is plain data, all zeros a valid value.
        let mut status: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: statvfs reads a NUL terminated path and writes the struct, both of which live through the call.
        if unsafe { libc::statvfs(target.as_ptr(), &raw mut status) } != 0 {
            return Err(io::Error::last_os_error());
        }
        mount(
            None,
            &target,
            None,
            remount_flags(status.f_flag, shown.access),
            None,
        )
    }

    /// The flags that make a bind mount of a file system mounted with `mounted` (`statvfs`'s `f_flag`) what
    /// `access` allows. What the host's mount has of read-only, nodev, noexec and its atime stays as it is: those are
    /// locked for a namespace that did not mount it, and a remount that drops one is refused.
    pub(super) fn remount_flags(mounted: libc::c_ulong, access: Access) -> libc::c_ulong {
        let mut flags = libc::MS_BIND | libc::MS_REMOUNT | libc::MS_NOSUID;
        for (has, keeps) in [
            (libc::ST_RDONLY, libc::MS_RDONLY),
            (libc::ST_NODEV, libc::MS_NODEV),
            (libc::ST_NOEXEC, libc::MS_NOEXEC),
            (libc::ST_NODIRATIME, libc::MS_NODIRATIME),
        ] {
            if mounted & has != 0 {
                flags |= keeps;
            }
        }
        // Without either, a remount would take relatime: strictatime has to be said.
        flags |= if mounted & libc::ST_NOATIME != 0 {
            libc::MS_NOATIME
        } else if mounted & libc::ST_RELATIME != 0 {
            libc::MS_RELATIME
        } else {
            libc::MS_STRICTATIME
        };
        flags |= match access {
            Access::Read => libc::MS_RDONLY | libc::MS_NOEXEC,
            Access::Write => libc::MS_NOEXEC,
            Access::Run => libc::MS_RDONLY,
        };
        flags
    }

    /// The guest's side of the namespace pasta made: its tap device behind the firewall, in a bridge with pasta's.
    fn network(host: &[(Ip, u8)]) -> Result<OwnedFd, String> {
        guard(host, || tap(GUEST_DEVICE))
    }

    /// Sets up the guest's side of the namespace pasta made, run as root of it: waits for pasta's device, turns
    /// IPv6 off, has `make` make the guest's device [`GUEST_DEVICE`] (a tap; a stand-in in tests), loads the guest's
    /// firewall onto it and checks it reads back as written, and only then joins the two devices in a bridge and
    /// sets them up. What `make` returns comes back.
    pub fn guard<T>(host: &[(Ip, u8)], make: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
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
        let device = make()?;
        let table = firewall::table(host).map_err(|e| e.to_string())?;
        let batch =
            cf_nft::batch(std::slice::from_ref(&table), cf_nft::Load::Replace).map_err(|e| e.to_string())?;
        cf_nft::apply(&batch).map_err(|e| format!("the guest's firewall: {e}"))?;
        let loaded = cf_nft::list().map_err(|e| format!("the guest's firewall, read back: {e}"))?;
        if loaded != [table] {
            return Err(format!("the guest's firewall reads back as {loaded:?}"));
        }
        crate::link::join(&[PASTA_DEVICE, GUEST_DEVICE], MTU)?;
        Ok(device)
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

    #[cfg(test)]
    mod tests {
        use super::*;

        const KEPT: libc::c_ulong = libc::MS_RDONLY | libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_NODIRATIME;
        const ATIME: libc::c_ulong = libc::MS_NOATIME | libc::MS_RELATIME | libc::MS_STRICTATIME;

        #[test]
        fn each_access_is_a_bind_remount_without_setuid_and_only_run_executes_only_write_writes() {
            let plain = libc::ST_RELATIME;
            let flags = |access| remount_flags(plain, access);
            for access in [Access::Read, Access::Write, Access::Run] {
                let flags = flags(access);
                assert_eq!(
                    flags & (libc::MS_BIND | libc::MS_REMOUNT | libc::MS_NOSUID),
                    libc::MS_BIND | libc::MS_REMOUNT | libc::MS_NOSUID
                );
                assert_eq!(flags & libc::MS_NODEV, 0, "{access:?}: a device stays a device");
                assert_eq!(flags & ATIME, libc::MS_RELATIME);
            }
            assert_eq!(flags(Access::Read) & KEPT, libc::MS_RDONLY | libc::MS_NOEXEC);
            assert_eq!(flags(Access::Write) & KEPT, libc::MS_NOEXEC);
            assert_eq!(flags(Access::Run) & KEPT, libc::MS_RDONLY);
        }

        #[test]
        fn what_the_hosts_mount_locks_stays() {
            for access in [Access::Read, Access::Write, Access::Run] {
                let locked = remount_flags(
                    libc::ST_RDONLY
                        | libc::ST_NODEV
                        | libc::ST_NOEXEC
                        | libc::ST_NODIRATIME
                        | libc::ST_NOSUID,
                    access,
                );
                assert_eq!(locked & KEPT, KEPT, "{access:?}");
            }
        }

        #[test]
        fn the_hosts_atime_is_said_as_it_is_strictatime_included() {
            for (mounted, wanted) in [
                (libc::ST_NOATIME, libc::MS_NOATIME),
                (libc::ST_NOATIME | libc::ST_RELATIME, libc::MS_NOATIME),
                (libc::ST_RELATIME, libc::MS_RELATIME),
                (0, libc::MS_STRICTATIME),
            ] {
                assert_eq!(
                    remount_flags(mounted, Access::Write) & ATIME,
                    wanted,
                    "{mounted:#x}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cf_vmm::Disk;
    use proptest::prelude::*;
    use std::os::unix::ffi::OsStringExt as _;

    fn strings(args: &[OsString]) -> Vec<&str> {
        args.iter().map(|arg| arg.to_str().unwrap()).collect()
    }

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    fn shown(path: &str, access: Access) -> Shown {
        Shown {
            path: path.into(),
            access,
        }
    }

    #[test]
    fn a_sandbox_is_written_as_its_subcommand_and_read_back() {
        let sandbox = Sandbox {
            network: Network::Nat,
            host: vec![([192, 168, 1, 0], 24), ([172, 17, 0, 0], 16)],
            root: "/run/m/root".into(),
            shown: vec![
                shown("/bin/ch", Access::Run),
                shown("/images/disk", Access::Read),
                shown("/run/m", Access::Write),
            ],
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
                "/run/m/root",
                "run",
                "/bin/ch",
                "read",
                "/images/disk",
                "write",
                "/run/m",
                "--",
                "/bin/ch",
                "--kernel"
            ]
        );
        assert_eq!(
            Sandbox::parse(&command[2..]),
            Ok((sandbox, vec!["/bin/ch".into(), "--kernel".into()]))
        );
        let none = Sandbox::new(Network::None, vec![], "/m/root".into());
        let command = none.command(Path::new("/r"), vec!["/ch".into()]);
        assert_eq!(strings(&command[2..]), ["none", "-", "/m/root", "--", "/ch"]);
        assert_eq!(Sandbox::parse(&command[2..]), Ok((none, vec!["/ch".into()])));
    }

    #[test]
    fn a_path_that_is_not_utf8_is_carried_as_it_is() {
        let odd = PathBuf::from(OsString::from_vec(b"/data/\xff\xfe/disk".to_vec()));
        let mut sandbox = Sandbox::new(Network::None, vec![], "/m/root".into());
        sandbox.show(&odd, Access::Write);
        let command = sandbox.command(Path::new("/r"), vec!["/ch".into()]);
        assert_eq!(
            Sandbox::parse(&command[2..]).unwrap().0.shown,
            [Shown {
                path: odd,
                access: Access::Write
            }]
        );
    }

    #[test]
    fn what_is_not_a_sandbox_is_refused() {
        let cases: &[&[&str]] = &[
            &[],
            &["nat", "-", "--"],
            &["nat", "-", "/root", "--"],
            &["nat", "-", "/root", "-x", "/ch"],
            &["nat", "-", "--", "/ch"],
            &["bridge", "-", "/root", "--", "/ch"],
            &["nat", "10.0.0.0/33", "/root", "--", "/ch"],
            &["nat", "10.0.0/8", "/root", "--", "/ch"],
            &["nat", "10.0.0.0/+8", "/root", "--", "/ch"],
            &["nat", "10.0.0.0/008", "/root", "--", "/ch"],
            &["nat", "10.0.0.0", "/root", "--", "/ch"],
            &["nat", "10.0.0.0/8,", "/root", "--", "/ch"],
            &["nat", "", "/root", "--", "/ch"],
            &["nat", "::1/128", "/root", "--", "/ch"],
            // The root is a plain absolute path, and not `/`.
            &["nat", "-", "root", "--", "/ch"],
            &["nat", "-", "/", "--", "/ch"],
            &["nat", "-", "/m/../root", "--", "/ch"],
            // Each shown path comes with its access, which is one of three.
            &["nat", "-", "/root", "read", "--", "/ch"],
            &["nat", "-", "/root", "/disk", "--", "/ch"],
            &["nat", "-", "/root", "exec", "/disk", "--", "/ch"],
            &["nat", "-", "/root", "READ", "/disk", "--", "/ch"],
            &["nat", "-", "/root", "read", "/disk", "write", "--", "/ch"],
            // A shown path names the same place below the root as on the host.
            &["nat", "-", "/root", "read", "disk", "--", "/ch"],
            &["nat", "-", "/root", "read", "/", "--", "/ch"],
            &["nat", "-", "/root", "read", "/data/../etc/shadow", "--", "/ch"],
            &["nat", "-", "/root", "read", "/data/..", "--", "/ch"],
            &["nat", "-", "/root", "read", "/data/./disk", "--", "/ch"],
            &["nat", "-", "/root", "read", "/data/.", "--", "/ch"],
            &["nat", "-", "/root", "read", "", "--", "/ch"],
            // Not the root or what is in it, not the monitor's own /proc.
            &["nat", "-", "/m/root", "write", "/m/root", "--", "/ch"],
            &["nat", "-", "/m/root", "write", "/m/root/x", "--", "/ch"],
            &["nat", "-", "/root", "read", "/proc", "--", "/ch"],
            &["nat", "-", "/root", "read", "/proc/1/root", "--", "/ch"],
            // Each path once, and none in another: the one shown later would be made in the host's folder.
            &[
                "nat", "-", "/root", "read", "/disk", "write", "/disk", "--", "/ch",
            ],
            &[
                "nat",
                "-",
                "/root",
                "write",
                "/data",
                "read",
                "/data/disk",
                "--",
                "/ch",
            ],
            &[
                "nat",
                "-",
                "/root",
                "read",
                "/data/disk",
                "write",
                "/data",
                "--",
                "/ch",
            ],
        ];
        for case in cases {
            assert!(Sandbox::parse(&args(case)).is_err(), "{case:?} was taken");
        }
        let many = vec!["10.0.0.0/8"; MAX_NETWORKS + 1].join(",");
        assert!(Sandbox::parse(&args(&["nat", &many, "/root", "--", "/ch"])).is_err());
        let nul = vec![
            "nat".into(),
            "-".into(),
            "/root".into(),
            "read".into(),
            OsString::from_vec(b"/data/a\0b".to_vec()),
            "--".into(),
            "/ch".into(),
        ];
        assert!(Sandbox::parse(&nul).is_err());
    }

    #[test]
    fn as_many_paths_as_allowed_are_taken_and_one_more_is_not() {
        let paths: Vec<String> = (0..=MAX_SHOWN).map(|n| format!("/d/{n}")).collect();
        let command = |count: usize| {
            let mut words = vec!["none", "-", "/root"];
            for path in &paths[..count] {
                words.extend(["read", path.as_str()]);
            }
            words.extend(["--", "/ch"]);
            args(&words)
        };
        assert_eq!(
            Sandbox::parse(&command(MAX_SHOWN)).unwrap().0.shown.len(),
            MAX_SHOWN
        );
        assert!(Sandbox::parse(&command(MAX_SHOWN + 1)).is_err());
    }

    #[test]
    fn a_folder_and_a_name_that_only_begins_like_it_are_two_paths() {
        let (sandbox, _) = Sandbox::parse(&args(&[
            "none",
            "-",
            "/m/root",
            "write",
            "/data",
            "read",
            "/data2",
            "write",
            "/m/rooted",
            "--",
            "/ch",
        ]))
        .unwrap();
        assert_eq!(sandbox.shown.len(), 3);
    }

    #[test]
    fn a_path_shown_twice_is_shown_once_writable_if_either_said_so() {
        let cases = [
            (Access::Read, Access::Write, Access::Write),
            (Access::Write, Access::Read, Access::Write),
            (Access::Read, Access::Read, Access::Read),
            (Access::Read, Access::Run, Access::Run),
            (Access::Run, Access::Read, Access::Run),
            (Access::Write, Access::Run, Access::Write),
            (Access::Run, Access::Write, Access::Write),
        ];
        for (first, then, wanted) in cases {
            let mut sandbox = Sandbox::new(Network::None, vec![], "/root".into());
            sandbox.show("/disk", first);
            sandbox.show("/other", Access::Read);
            sandbox.show("/disk", then);
            assert_eq!(
                sandbox.shown,
                [shown("/disk", wanted), shown("/other", Access::Read)],
                "{first:?} then {then:?}"
            );
        }
    }

    fn spec(network: Network) -> VmSpec {
        VmSpec {
            kernel: "/var/lib/cf/guest/Image".into(),
            initramfs: "/var/lib/cf/guest/initramfs.cpio.gz".into(),
            memory_bytes: 1 << 30,
            cpus: 2,
            disks: vec![
                Disk {
                    path: "/var/lib/cf/images/rootfs.erofs".into(),
                    id: "image".into(),
                    read_only: true,
                },
                Disk {
                    path: "/var/lib/cf/jobs/one/scratch.img".into(),
                    id: "scratch".into(),
                    read_only: false,
                },
            ],
            network,
            console: "/var/lib/cf/logs/one.console.log".into(),
        }
    }

    #[test]
    fn a_machines_monitor_is_shown_its_files_and_devices_and_nothing_else() {
        let dir = Path::new("/var/lib/cf/machines/vm-7-0");
        let sandbox = Sandbox::for_machine(
            &spec(Network::Nat),
            Path::new("/opt/cf/programs/cloud-hypervisor"),
            dir,
            vec![([10, 0, 0, 0], 8)],
        );
        assert_eq!(sandbox.network, Network::Nat);
        assert_eq!(sandbox.host, [([10, 0, 0, 0], 8)]);
        assert_eq!(sandbox.root, dir.join("root"));
        assert_eq!(
            sandbox.shown,
            [
                shown("/opt/cf/programs/cloud-hypervisor", Access::Run),
                shown("/var/lib/cf/guest/Image", Access::Read),
                shown("/var/lib/cf/guest/initramfs.cpio.gz", Access::Read),
                shown("/var/lib/cf/images/rootfs.erofs", Access::Read),
                shown("/var/lib/cf/jobs/one/scratch.img", Access::Write),
                shown("/var/lib/cf/logs/one.console.log", Access::Write),
                shown("/var/lib/cf/machines/vm-7-0", Access::Write),
                shown("/dev/kvm", Access::Write),
                shown("/dev/urandom", Access::Read),
            ]
        );
        // What the host writes, the sandbox takes.
        let command = sandbox.command(
            Path::new("/opt/cf/runner"),
            vec!["/opt/cf/programs/cloud-hypervisor".into()],
        );
        assert_eq!(Sandbox::parse(&command[2..]).unwrap().0, sandbox);
        assert_eq!(
            Sandbox::for_machine(&spec(Network::None), Path::new("/ch"), dir, vec![]).network,
            Network::None
        );
    }

    fn plain_path() -> impl Strategy<Value = PathBuf> {
        proptest::collection::vec("[a-z0-9_.-]{1,8}", 1..5)
            .prop_filter("no . or ..", |parts| {
                parts.iter().all(|part| part != "." && part != "..")
            })
            .prop_map(|parts| PathBuf::from(format!("/{}", parts.join("/"))))
    }

    proptest! {
        #[test]
        fn any_sandbox_reads_back_as_written(
            nat: bool,
            host in proptest::collection::vec((any::<[u8; 4]>(), 0u8..=32), 0..MAX_NETWORKS),
            shown in proptest::collection::vec((plain_path(), 0..3usize), 0..MAX_SHOWN),
            monitor in proptest::collection::vec("[ -~]{1,20}", 1..6),
        ) {
            let mut sandbox =
                Sandbox::new(if nat { Network::Nat } else { Network::None }, host, "/sandbox/root".into());
            for (path, access) in shown {
                // None in another, as the sandbox wants.
                if sandbox.shown.iter().any(|other| path.starts_with(&other.path) || other.path.starts_with(&path)) {
                    continue;
                }
                sandbox.show(path, [Access::Read, Access::Write, Access::Run][access]);
            }
            let monitor: Vec<OsString> = monitor.into_iter().map(OsString::from).collect();
            let command = sandbox.command(Path::new("/runner"), monitor.clone());
            prop_assert_eq!(Sandbox::parse(&command[2..]), Ok((sandbox, monitor)));
        }

        #[test]
        fn a_path_with_a_dot_part_is_never_taken(
            before in plain_path(),
            dots in prop_oneof![Just("."), Just("..")],
            after in proptest::option::of(plain_path()),
        ) {
            let path = format!("{}/{dots}{}", before.display(), after.map(|p| p.display().to_string()).unwrap_or_default());
            prop_assert!(Sandbox::parse(&args(&["none", "-", "/root", "read", &path, "--", "/ch"])).is_err(), "{}", path);
        }
    }
}
