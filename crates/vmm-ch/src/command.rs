//! The command lines of a machine: cloud-hypervisor's, and pasta's around it for a machine with a network.

use crate::cpus::List;
use cf_vmm::VmSpec;
use std::ffi::OsString;
use std::path::Path;

/// The guest's address, which pasta hands out by DHCP.
pub const GUEST: [u8; 4] = [10, 0, 2, 15];
/// The gateway pasta answers as, and the guest's name server: pasta forwards what the guest sends to its port 53 to
/// the host's resolver (`--dns-forward`), and maps nothing else of it to the host (`--no-map-gw`). Its DHCP names no
/// name server, so the guest asks its gateway. pasta of every version forwards so, also to a resolver on the host's
/// loopback; with `--dns` given, 2023-03's and later's both forward nothing.
pub const GATEWAY: [u8; 4] = [10, 0, 2, 2];
/// The prefix length of the guest's network.
pub const PREFIX: u8 = 24;
/// The guest's MAC address: every machine has a namespace of its own, so they never meet.
pub const MAC: &str = "52:54:00:0c:f0:01";
/// The MTU of the guest's link, as on the Mac.
pub const MTU: u16 = 1500;
/// pasta's device in the namespace.
pub const PASTA_DEVICE: &str = "pasta0";

/// The files a machine's monitor uses beside the specification's.
#[derive(Debug, Clone, Copy)]
pub struct Files<'a> {
    /// The vsock's Unix socket.
    pub vsock: &'a Path,
    /// Where the monitor writes its events.
    pub events: &'a Path,
}

/// A path as cloud-hypervisor's options can carry it: UTF-8 and absolute, without the characters that separate
/// its options and lists, or anything a line could break on.
pub fn option_path(path: &Path) -> Result<&str, String> {
    let text = path
        .to_str()
        .ok_or_else(|| format!("{} is not UTF-8", path.display()))?;
    let plain = path.is_absolute()
        && !text
            .chars()
            .any(|c| matches!(c, ',' | '=' | '[' | ']' | '"' | '@') || c.is_control());
    if !plain {
        return Err(format!("{text:?} is no path the monitor's options can carry"));
    }
    Ok(text)
}

fn ip(address: [u8; 4]) -> String {
    std::net::Ipv4Addr::from(address).to_string()
}

/// cloud-hypervisor's command line for `spec`, its vCPUs held to `cores`. A network device is not in it: the
/// sandbox adds it with the descriptor of the device it made ([`net`]).
pub fn monitor(
    program: &Path,
    spec: &VmSpec,
    files: Files<'_>,
    cores: &[u32],
) -> Result<Vec<OsString>, String> {
    if cores.is_empty() {
        return Err("no host CPU for the machine".into());
    }
    let cores = List(cores).to_string();
    let each: Vec<String> = (0..spec.cpus).map(|vcpu| format!("{vcpu}@[{cores}]")).collect();
    let mut args: Vec<String> = vec![
        "--kernel".into(),
        option_path(&spec.kernel)?.into(),
        "--initramfs".into(),
        option_path(&spec.initramfs)?.into(),
        "--cmdline".into(),
        cf_vmm::COMMAND_LINE.into(),
        "--cpus".into(),
        format!("boot={},affinity=[{}]", spec.cpus, each.join(",")),
        "--memory".into(),
        format!("size={}M", spec.memory_bytes >> 20),
        "--vsock".into(),
        format!("cid=3,socket={}", option_path(files.vsock)?),
        "--console".into(),
        format!("file={}", option_path(&spec.console)?),
        "--serial".into(),
        "off".into(),
        "--seccomp".into(),
        "true".into(),
        "--event-monitor".into(),
        format!("path={}", option_path(files.events)?),
    ];
    // A disk the guest writes goes past the host's page cache (`direct=on`): the guest caches it already, and the
    // host's dirty pages would count against the machine's cgroup, which holds only the guest's memory and the
    // overhead. On a disk slower to write back than the guest writes (a cloud VM's), they filled what was left and
    // the machine ended in an OOM it did not cause. A read-only disk keeps the cache, clean and shared by every
    // machine that reads it.
    for disk in &spec.disks {
        args.push("--disk".into());
        args.push(format!(
            "path={},readonly={},direct={},serial={},image_type=raw",
            option_path(&disk.path)?,
            if disk.read_only { "on" } else { "off" },
            if disk.read_only { "off" } else { "on" },
            disk.id
        ));
    }
    let mut command = vec![program.as_os_str().to_owned()];
    command.extend(args.into_iter().map(OsString::from));
    Ok(command)
}

/// The network device of a machine, on the tap device open as `fd`. Its MTU is the tap's, which the sandbox set:
/// the monitor runs without the capability to set it (`mtu=` here would have it try, and fail).
pub fn net(fd: i32) -> [String; 2] {
    ["--net".into(), format!("fd=[{fd}],mac={MAC}")]
}

/// pasta's command line in front of `then`: a user and network namespace of its own, IPv4 alone, the guest's
/// DHCP answered and DNS forwarded by pasta ([`GATEWAY`]), and nothing of the host reachable through it. No port of the host leads in, and
/// no address maps to the host (`--no-map-gw`: in pasta since 2024 that also means `--map-host-loopback none`, and
/// `--map-guest-addr` maps nothing unless given). Every option here is in pasta since 2023-03, the oldest one this
/// runs with (Debian 12's); newer options are left out, since an older pasta refuses to start with one it lacks.
pub fn pasta(program: &Path, then: Vec<OsString>) -> Vec<OsString> {
    let mut command: Vec<OsString> = vec![program.as_os_str().to_owned()];
    let args = [
        "--foreground",
        "--quiet",
        "--ipv4-only",
        "--ns-ifname",
        PASTA_DEVICE,
        "--mtu",
        &MTU.to_string(),
        "--address",
        &ip(GUEST),
        "--netmask",
        &PREFIX.to_string(),
        "--gateway",
        &ip(GATEWAY),
        "--dns-forward",
        &ip(GATEWAY),
        "--no-map-gw",
        "--tcp-ports",
        "none",
        "--udp-ports",
        "none",
        "--tcp-ns",
        "none",
        "--udp-ns",
        "none",
        "--",
    ]
    .map(String::from);
    command.extend(args.into_iter().map(OsString::from));
    command.extend(then);
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use cf_vmm::{Disk, Network};
    use std::path::PathBuf;

    fn spec() -> VmSpec {
        VmSpec {
            kernel: "/state/run/job/kernel".into(),
            initramfs: "/state/run/job/initramfs".into(),
            memory_bytes: 2 << 30,
            cpus: 2,
            disks: vec![
                Disk {
                    path: "/state/images/root.img".into(),
                    id: "root".into(),
                    read_only: true,
                },
                Disk {
                    path: "/state/run/job/work.img".into(),
                    id: "work".into(),
                    read_only: false,
                },
            ],
            network: Network::Nat,
            console: "/state/logs/job.console.log".into(),
        }
    }

    fn files() -> (PathBuf, PathBuf) {
        (
            "/state/machines/vm-1-0/vsock".into(),
            "/state/machines/vm-1-0/events".into(),
        )
    }

    fn strings(command: &[OsString]) -> Vec<&str> {
        command.iter().map(|arg| arg.to_str().unwrap()).collect()
    }

    #[test]
    fn the_monitor_gets_the_machine_as_specified() {
        let (vsock, events) = files();
        let command = monitor(
            Path::new("/bin/ch"),
            &spec(),
            Files {
                vsock: &vsock,
                events: &events,
            },
            &[4, 5, 6, 7],
        )
        .unwrap();
        assert_eq!(
            strings(&command),
            [
                "/bin/ch",
                "--kernel",
                "/state/run/job/kernel",
                "--initramfs",
                "/state/run/job/initramfs",
                "--cmdline",
                cf_vmm::COMMAND_LINE,
                "--cpus",
                "boot=2,affinity=[0@[4-7],1@[4-7]]",
                "--memory",
                "size=2048M",
                "--vsock",
                "cid=3,socket=/state/machines/vm-1-0/vsock",
                "--console",
                "file=/state/logs/job.console.log",
                "--serial",
                "off",
                "--seccomp",
                "true",
                "--event-monitor",
                "path=/state/machines/vm-1-0/events",
                "--disk",
                "path=/state/images/root.img,readonly=on,direct=off,serial=root,image_type=raw",
                "--disk",
                "path=/state/run/job/work.img,readonly=off,direct=on,serial=work,image_type=raw",
            ]
        );
    }

    #[test]
    fn every_vcpu_is_held_to_the_same_cores() {
        let (vsock, events) = files();
        let mut wide = spec();
        wide.cpus = 3;
        let command = monitor(
            Path::new("/ch"),
            &wide,
            Files {
                vsock: &vsock,
                events: &events,
            },
            &[0, 2, 3],
        )
        .unwrap();
        assert!(strings(&command).contains(&"boot=3,affinity=[0@[0,2-3],1@[0,2-3],2@[0,2-3]]"));
        assert!(
            monitor(
                Path::new("/ch"),
                &wide,
                Files {
                    vsock: &vsock,
                    events: &events
                },
                &[]
            )
            .is_err()
        );
    }

    #[test]
    fn a_path_that_would_break_an_option_is_refused() {
        let (vsock, events) = files();
        for bad in [
            "/a,b",
            "/a=b",
            "/a[b",
            "/a]b",
            "/a\"b",
            "/a\nb",
            "relative/disk",
            "/a@b",
        ] {
            let mut with = spec();
            with.disks[1].path = bad.into();
            let refused = monitor(
                Path::new("/ch"),
                &with,
                Files {
                    vsock: &vsock,
                    events: &events,
                },
                &[0],
            );
            assert!(refused.is_err(), "{bad:?} was taken");
            let mut with = spec();
            with.console = bad.into();
            assert!(
                monitor(
                    Path::new("/ch"),
                    &with,
                    Files {
                        vsock: &vsock,
                        events: &events
                    },
                    &[0]
                )
                .is_err()
            );
        }
        assert!(option_path(Path::new("/plain/path with space/ü.img")).is_ok());
    }

    #[test]
    fn a_path_that_is_not_utf8_is_refused() {
        use std::os::unix::ffi::OsStrExt as _;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"/disk\xff.img"));
        assert!(option_path(path).is_err());
    }

    #[test]
    fn the_network_device_is_the_sandbox_descriptor_and_sets_nothing_on_it() {
        assert_eq!(net(3), ["--net".to_string(), format!("fd=[3],mac={MAC}")]);
    }

    #[test]
    fn pasta_opens_no_way_to_the_host() {
        let command = pasta(Path::new("/bin/pasta"), vec!["/bin/sandbox".into(), "nat".into()]);
        let args = strings(&command);
        let after = |flag: &str| args[args.iter().position(|arg| *arg == flag).unwrap() + 1];
        assert_eq!(args[0], "/bin/pasta");
        assert!(args.contains(&"--ipv4-only"));
        assert!(args.contains(&"--no-map-gw"));
        assert!(args.contains(&"--foreground"));
        // A name server given or handed out by DHCP stops the forwarding; the guest asks its gateway.
        assert!(
            !args.iter().any(|arg| ["--dns", "-D", "--dhcp-dns"].contains(arg)),
            "{args:?}"
        );
        // Options of 2024 and later, which Debian 12's pasta refuses to start with.
        assert!(!args.iter().any(|arg| arg.starts_with("--map-")), "{args:?}");
        for flag in ["--tcp-ports", "--udp-ports", "--tcp-ns", "--udp-ns"] {
            assert_eq!(after(flag), "none", "{flag}");
        }
        assert_eq!(after("--address"), "10.0.2.15");
        assert_eq!(after("--gateway"), "10.0.2.2");
        assert_eq!(after("--dns-forward"), "10.0.2.2");
        assert_eq!(after("--ns-ifname"), PASTA_DEVICE);
        assert_eq!(&args[args.len() - 3..], ["--", "/bin/sandbox", "nat"]);
    }
}
