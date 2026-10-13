//! What the suite reads of the host it runs on: its machines' processes, what they hold, the memory left, and on
//! Linux the cgroup of each machine and the host's own addresses.

use std::collections::BTreeSet;

use super::output;

/// The processes that each hold one machine's memory, whoever started them.
pub fn machines() -> BTreeSet<u32> {
    output("pgrep", &MACHINE_PROCESS)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

#[cfg(target_os = "macos")]
pub use mac::*;

#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "macos")]
mod mac {
    use super::output;

    /// `pgrep`'s words for a machine's process.
    pub const MACHINE_PROCESS: [&str; 2] = ["-f", "com.apple.Virtualization.VirtualMachine"];

    /// The memory a process holds, in bytes, as the system charges it: its footprint, or what `ps` says when that
    /// tool answers nothing.
    pub fn memory_of(pid: u32) -> u64 {
        let report = output("footprint", &[&pid.to_string()]);
        let footprint = report.split("Footprint: ").nth(1).and_then(|rest| {
            let mut words = rest.split_whitespace();
            let number: f64 = words.next()?.parse().ok()?;
            let unit = match words.next()? {
                "KB" => 1u64 << 10,
                "MB" => 1 << 20,
                "GB" => 1 << 30,
                _ => 1,
            };
            Some((number * unit as f64) as u64)
        });
        footprint.unwrap_or_else(|| super::rss_of(pid))
    }

    /// The Mac's memory that is free or given back at once when asked for: free, inactive, speculative, purgeable.
    pub fn available() -> u64 {
        let report = output("vm_stat", &[]);
        let page: u64 = report
            .split("page size of ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next()?.parse().ok())
            .unwrap_or(16384);
        let pages: u64 = report
            .lines()
            .filter(|line| {
                [
                    "Pages free",
                    "Pages inactive",
                    "Pages speculative",
                    "Pages purgeable",
                ]
                .iter()
                .any(|name| line.starts_with(name))
            })
            .filter_map(|line| line.rsplit(' ').next()?.trim_end_matches('.').parse::<u64>().ok())
            .sum();
        pages * page
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::super::Fallible;

    /// `pgrep`'s words for a machine's process: cloud-hypervisor, by the name the kernel keeps (15 bytes of it).
    pub const MACHINE_PROCESS: [&str; 2] = ["-x", "cloud-hyperviso"];

    /// The memory a process holds, in bytes: its resident set.
    pub fn memory_of(pid: u32) -> u64 {
        super::rss_of(pid)
    }

    /// The host's memory that is free or given back at once when asked for, as the kernel estimates it.
    pub fn available() -> u64 {
        fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|text| meminfo(&text, "MemAvailable:"))
            .unwrap_or(0)
    }

    /// A field of `/proc/meminfo`, in bytes.
    pub fn meminfo(text: &str, field: &str) -> Option<u64> {
        let line = text.lines().find(|line| line.starts_with(field))?;
        let kb: u64 = line[field.len()..].split_whitespace().next()?.parse().ok()?;
        Some(kb << 10)
    }

    /// Where the unified hierarchy is mounted.
    const MOUNT: &str = "/sys/fs/cgroup";

    /// Moves every process of this one's cgroup, `cargo` with it, into its `host` leaf, as the Runner does when it
    /// starts alone in a delegated cgroup: the vm-hosts the suite starts then find the delegation as their cgroup's
    /// parent, and machines can have cgroups beside them. A suite already in a `host` leaf stays.
    pub fn enter_host_cgroup() -> Fallible<()> {
        let own = own_cgroup()?;
        if own.file_name().is_some_and(|name| name == "host") {
            return Ok(());
        }
        let host = own.join("host");
        if let Err(error) = fs::create_dir(&host)
            && error.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(format!(
                "{}: {error}; run the suite in a cgroup delegated to this user, as `systemd-run --user --scope -p \
                 Delegate=yes cargo xtask it` does",
                host.display()
            )
            .into());
        }
        for pid in fs::read_to_string(own.join("cgroup.procs"))?.lines() {
            // A process that ended meanwhile is no matter.
            let _ = fs::write(host.join("cgroup.procs"), pid);
        }
        Ok(())
    }

    fn own_cgroup() -> Fallible<PathBuf> {
        let text = fs::read_to_string("/proc/self/cgroup")?;
        let path = text
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .ok_or("no cgroup v2 in /proc/self/cgroup")?;
        Ok(Path::new(MOUNT).join(path.trim().trim_start_matches('/')))
    }

    /// What the cgroup of a machine a vm-host started says now: its limit, and the most it has held.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct Group {
        pub max: u64,
        pub peak: u64,
    }

    /// The cgroups of the machines the vm-host `host` started (`vm-<host>-<n>`, beside the suite's `host` leaf).
    pub fn groups_of(host: u32) -> Vec<Group> {
        let Some(machines) = own_cgroup()
            .ok()
            .and_then(|own| Some(own.parent()?.join("machines")))
        else {
            return Vec::new();
        };
        let prefix = format!("vm-{host}-");
        let Ok(entries) = fs::read_dir(machines) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(&prefix))
            })
            .filter_map(|entry| {
                let read = |file: &str| {
                    fs::read_to_string(entry.path().join(file))
                        .ok()?
                        .trim()
                        .parse()
                        .ok()
                };
                Some(Group {
                    max: read("memory.max")?,
                    peak: read("memory.peak").or_else(|| read("memory.current"))?,
                })
            })
            .collect()
    }

    /// Every IPv4 address of the host but loopback's, and the routers its default routes go through: what no
    /// machine may reach.
    pub fn own_addresses() -> (Vec<String>, Vec<String>) {
        (
            addresses_in(&super::output("ip", &["-4", "-o", "addr", "show"])),
            routers_in(&super::output("ip", &["-4", "route", "show", "default"])),
        )
    }

    /// The addresses `ip -4 -o addr show` lists, but loopback's.
    fn addresses_in(listing: &str) -> Vec<String> {
        listing
            .lines()
            .filter_map(|line| {
                let mut words = line.split_whitespace().skip_while(|word| *word != "inet");
                words.next()?;
                let address = words.next()?.split('/').next()?;
                let parsed: std::net::Ipv4Addr = address.parse().ok()?;
                (!parsed.is_loopback()).then(|| address.to_string())
            })
            .collect()
    }

    /// The routers `ip -4 route show default` goes through, each once.
    fn routers_in(listing: &str) -> Vec<String> {
        let mut routers: Vec<String> = listing
            .lines()
            .filter_map(|line| {
                let mut words = line.split_whitespace().skip_while(|word| *word != "via");
                words.next()?;
                let router = words.next()?;
                router.parse::<std::net::Ipv4Addr>().ok()?;
                Some(router.to_string())
            })
            .collect();
        routers.sort();
        routers.dedup();
        routers
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_address_but_loopback_is_read_from_ip_addr() {
            let listing =
                "1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever
2: eth0    inet 10.20.30.40/24 brd 10.20.30.255 scope global dynamic eth0\\       valid_lft 86000sec
3: wlan0    inet 172.20.1.5/16 brd 172.20.255.255 scope global wlan0\\       valid_lft forever
3: wlan0    inet 172.20.1.6/16 scope global secondary wlan0\\       valid_lft forever
4: docker0    inet 172.17.0.1/16 brd 172.17.255.255 scope global docker0\\       valid_lft forever
5: lo2    inet 127.5.0.1/8 scope host lo2
";
            assert_eq!(
                addresses_in(listing),
                ["10.20.30.40", "172.20.1.5", "172.20.1.6", "172.17.0.1"]
            );
            assert!(addresses_in("").is_empty());
            assert!(addresses_in("2: eth0    inet\n2: eth0    inet not-an-address/24\n").is_empty());
        }

        #[test]
        fn each_router_of_the_default_routes_is_read_once() {
            let listing = "default via 10.20.30.1 dev eth0 proto dhcp src 10.20.30.40 metric 100
default via 172.20.0.1 dev wlan0 proto dhcp metric 600
default via 10.20.30.1 dev eth0 metric 1000
default dev wg0 scope link
";
            assert_eq!(routers_in(listing), ["10.20.30.1", "172.20.0.1"]);
            assert!(routers_in("").is_empty());
            assert!(routers_in("default via\ndefault via fe80::1 dev eth0\n").is_empty());
        }

        #[test]
        fn meminfo_is_read_in_bytes() {
            let text = "MemTotal:        8000000 kB\nMemFree:          100 kB\nMemAvailable:    2048 kB\n";
            assert_eq!(meminfo(text, "MemAvailable:"), Some(2048 << 10));
            assert_eq!(meminfo(text, "MemTotal:"), Some(8_000_000 << 10));
            assert_eq!(meminfo(text, "SwapTotal:"), None);
            assert_eq!(meminfo("MemAvailable: lots kB\n", "MemAvailable:"), None);
        }
    }
}

/// A process's resident set, as `ps` says it.
fn rss_of(pid: u32) -> u64 {
    output("ps", &["-o", "rss=", "-p", &pid.to_string()])
        .trim()
        .parse::<u64>()
        .unwrap_or(0)
        << 10
}
