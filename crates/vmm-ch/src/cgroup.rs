//! A cgroup for each machine, under the part of the cgroup tree that was delegated to the Runner.
//!
//! The delegated cgroup (`Delegate=yes` of the Runner's systemd unit, or of a `systemd-run` scope) holds two
//! children: `host`, where the Runner's own processes are (a cgroup with processes can hand no controller down,
//! so they live in a leaf), and `machines`, with a cgroup per machine. A machine's cgroup holds its monitor and
//! pasta and nothing else: its memory is capped at the machine's size and the overhead, swap is off, the number of
//! processes is bounded, its CPUs are the chosen cores, and `cgroup.kill` is the plug. `memory.current` is what the
//! machine holds of the host's memory.

/// The most processes and threads a machine's cgroup may hold: cloud-hypervisor's vCPU, device and I/O threads,
/// pasta and the namespace's helper, with room to spare.
pub const PIDS_MAX: u32 = 256;
/// The file that turns a machine's swap off. A kernel that keeps no account of swap per cgroup (booted with
/// `swapaccount=0`, or built without swap) has none.
pub const SWAP_MAX: &str = "memory.swap.max";

/// What a machine's cgroup is set to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// `memory.max`: the machine's memory and the overhead of its monitor and network.
    pub memory_max: u64,
    /// `pids.max`.
    pub pids_max: u32,
    /// `cpuset.cpus`, when the delegation has the cpuset controller.
    pub cpus: Option<String>,
}

impl Limits {
    /// The limits of a machine of `memory_bytes` whose monitor and network take at most `overhead`.
    pub fn of(memory_bytes: u64, overhead: u64, cpus: Option<String>) -> Result<Self, String> {
        let memory_max = memory_bytes
            .checked_add(overhead)
            .ok_or_else(|| format!("a machine of {memory_bytes} bytes and {overhead} more is no size"))?;
        Ok(Limits {
            memory_max,
            pids_max: PIDS_MAX,
            cpus,
        })
    }

    /// The files written and what goes into each, in the order they are written. Swap is off, and when the cap is
    /// reached the kernel kills the whole cgroup, never one of the monitor's threads alone.
    pub fn files(&self) -> Vec<(&'static str, String)> {
        let mut files = vec![
            ("memory.max", self.memory_max.to_string()),
            (SWAP_MAX, "0".to_string()),
            ("memory.oom.group", "1".to_string()),
            ("pids.max", self.pids_max.to_string()),
        ];
        if let Some(cpus) = &self.cpus {
            files.push(("cpuset.cpus", cpus.clone()));
        }
        files
    }
}

/// This process's cgroup as `/proc/self/cgroup` names it on a host with the unified hierarchy (`0::<path>`).
pub fn own_path(proc_self_cgroup: &str) -> Result<String, String> {
    let path = proc_self_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or("this host does not use the unified cgroup hierarchy (cgroup v2)")?;
    if !path.starts_with('/') || path.split('/').any(|part| part == "..") {
        return Err(format!("the cgroup path {path:?} is not one"));
    }
    Ok(path.to_string())
}

/// The value of `key` in a file of `key value` lines (`memory.events`, `memory.stat`, `cgroup.events`).
pub fn value(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(' ')?;
        (name == key).then(|| value.trim().parse().ok()).flatten()
    })
}

/// What a machine holds of the host's memory: its cgroup's `memory.current` less the page cache of the files it
/// read and wrote (the disk images, the kernel), which the host takes back whenever it needs to. The guest's memory
/// is anonymous memory of the monitor, or shared memory where it is a shared mapping, and stays in.
pub fn resident(current: u64, stat: &str) -> u64 {
    let file = value(stat, "file").unwrap_or(0);
    let shared = value(stat, "shmem").unwrap_or(0);
    current.saturating_sub(file.saturating_sub(shared))
}

/// The controllers a cgroup's children must have for machines: memory and pids always, cpuset where the delegation
/// has it. `None` when one that must be there is not.
pub fn wanted(available: &str) -> Option<Vec<&'static str>> {
    let has = |name: &str| available.split_whitespace().any(|one| one == name);
    if !has("memory") || !has("pids") {
        return None;
    }
    let mut wanted = vec!["memory", "pids"];
    if has("cpuset") {
        wanted.push("cpuset");
    }
    Some(wanted)
}

/// Whether a cgroup's name is one a machine's cgroup may have: what [`crate::ChVmm`] makes, nothing that walks out.
pub fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && !name.starts_with('-')
}

/// The name of the cgroup and the folder of a host's `index`th machine: the host's process id comes first, so
/// what a host that is gone left can be told from what a running one holds.
pub fn machine_name(host: u32, index: u64) -> String {
    format!("vm-{host}-{index}")
}

/// The process that named a machine's cgroup or folder with [`machine_name`].
pub fn owner(name: &str) -> Option<u32> {
    let (host, index) = name.strip_prefix("vm-")?.split_once('-')?;
    let digits =
        |text: &str| !text.is_empty() && text.len() <= 20 && text.bytes().all(|b| b.is_ascii_digit());
    if !digits(host) || !digits(index) {
        return None;
    }
    index.parse::<u64>().ok()?;
    host.parse().ok()
}

#[cfg(target_os = "linux")]
pub use linux::{Delegation, MachineGroup, enter_host, own};

#[cfg(target_os = "linux")]
mod linux {
    use super::{Limits, own_path, plain_name, value, wanted};
    use std::fs;
    use std::io::ErrorKind;
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// Where the unified hierarchy is mounted.
    const MOUNT: &str = "/sys/fs/cgroup";
    /// `CGROUP2_SUPER_MAGIC` from the kernel's `magic.h`.
    const CGROUP2_SUPER_MAGIC: i64 = 0x6367_7270;
    /// The leaf the Runner's own processes live in.
    const HOST: &str = "host";
    /// The parent of the machines' cgroups.
    const MACHINES: &str = "machines";
    /// The host's swap devices.
    const SWAPS: &str = "/proc/swaps";

    fn say(path: &Path, error: impl std::fmt::Display) -> String {
        format!("{}: {error}", path.display())
    }

    /// The part of the cgroup tree this process may make machines in.
    #[derive(Debug, Clone)]
    pub struct Delegation {
        root: PathBuf,
        machines: PathBuf,
        cpuset: bool,
    }

    impl Delegation {
        /// The delegated cgroup this process runs in, set up for machines. A process in `<delegated>/host` (the
        /// daemon and the vm-hosts it starts) finds it as its cgroup's parent. A process alone in a delegated
        /// cgroup (`codefusion-runner boot-proof` in a `systemd-run --scope -p Delegate=yes`) moves itself into
        /// `host` first. Anything else is refused with what to do.
        pub fn claim() -> Result<Self, String> {
            Self::claim_under(Path::new(MOUNT))
        }

        fn claim_under(mount: &Path) -> Result<Self, String> {
            // SAFETY: `found` is a plain struct statfs fills; the path is NUL terminated.
            let mut found: libc::statfs = unsafe { std::mem::zeroed() };
            let path =
                std::ffi::CString::new(mount.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
            // SAFETY: as above.
            if unsafe { libc::statfs(path.as_ptr(), &mut found) } != 0
                || found.f_type as i64 != CGROUP2_SUPER_MAGIC
            {
                return Err(format!(
                    "{} is not the unified cgroup hierarchy (cgroup v2)",
                    mount.display()
                ));
            }
            let own = own_under(mount)?;
            let root = if own.file_name().is_some_and(|name| name == HOST) {
                own.parent()
                    .ok_or("the cgroup root is no delegation")?
                    .to_path_buf()
            } else {
                let procs = fs::read_to_string(own.join("cgroup.procs")).map_err(|e| say(&own, e))?;
                let me = std::process::id().to_string();
                if procs.lines().any(|pid| pid != me) {
                    return Err(format!(
                        "{} holds other processes too, so no machine can have a cgroup under it: run the Runner \
                         alone in a delegated cgroup, as `systemd-run --scope -p Delegate=yes codefusion-runner …` \
                         does",
                        own.display()
                    ));
                }
                let host = own.join(HOST);
                make(&host)?;
                fs::write(host.join("cgroup.procs"), "0").map_err(|e| {
                    say(
                        &host,
                        format!("{e}; is {} delegated to this user?", own.display()),
                    )
                })?;
                own
            };
            let available = fs::read_to_string(root.join("cgroup.controllers")).map_err(|e| say(&root, e))?;
            let wanted = wanted(&available).ok_or_else(|| {
                format!(
                    "{} is not given the memory and pids controllers (it has: {}); delegate them",
                    root.display(),
                    available.trim()
                )
            })?;
            let enable: String = wanted.iter().map(|name| format!("+{name} ")).collect();
            let machines = root.join(MACHINES);
            enable_in(&root, enable.trim())?;
            make(&machines)?;
            enable_in(&machines, enable.trim())?;
            Ok(Delegation {
                root,
                machines,
                cpuset: wanted.contains(&"cpuset"),
            })
        }

        /// The delegated cgroup.
        pub fn root(&self) -> &Path {
            &self.root
        }

        /// The parent of the machines' cgroups.
        pub fn machines(&self) -> &Path {
            &self.machines
        }

        /// Whether machines can be held to cores by their cgroup, and not only by their vCPUs' affinity.
        pub fn has_cpuset(&self) -> bool {
            self.cpuset
        }

        /// Removes what machines of hosts that are gone left: the cgroup of a machine is named after the process
        /// that started it ([`super::owner`]), and one whose process is not `alive` is killed and removed.
        pub fn sweep(&self, alive: impl Fn(u32) -> bool) {
            let Ok(entries) = fs::read_dir(&self.machines) else {
                return;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(owner) = name.to_str().and_then(super::owner) else {
                    continue;
                };
                if !alive(owner) && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    MachineGroup { path: entry.path() }.remove(Duration::from_secs(1));
                }
            }
        }

        /// Makes the cgroup of a machine, empty, with its limits. A cgroup of the name that a vm-host before this
        /// one left empty is taken over; one that still holds a process is refused.
        pub fn machine(&self, name: &str, limits: &Limits) -> Result<MachineGroup, String> {
            if !plain_name(name) {
                return Err(format!("{name:?} is no name for a machine's cgroup"));
            }
            let path = self.machines.join(name);
            if let Err(error) = fs::create_dir(&path) {
                if error.kind() != ErrorKind::AlreadyExists {
                    return Err(say(&path, error));
                }
                fs::remove_dir(&path).map_err(|e| say(&path, format!("left by an earlier machine: {e}")))?;
                fs::create_dir(&path).map_err(|e| say(&path, e))?;
            }
            let group = MachineGroup { path };
            let limited = limits.files().into_iter().try_for_each(|(file, content)| {
                let path = group.path.join(file);
                if file == super::SWAP_MAX && !path.exists() {
                    let swaps = fs::read_to_string(SWAPS).map_err(|e| format!("{SWAPS}: {e}"))?;
                    return super::without_swap_account(&swaps);
                }
                fs::write(&path, &content).map_err(|e| say(&path, format!("{content}: {e}")))
            });
            if let Err(error) = limited {
                let _ = fs::remove_dir(&group.path);
                return Err(error);
            }
            Ok(group)
        }
    }

    /// This process's cgroup, as a path under the unified hierarchy's mount.
    pub fn own() -> Result<PathBuf, String> {
        own_under(Path::new(MOUNT))
    }

    fn own_under(mount: &Path) -> Result<PathBuf, String> {
        let text = fs::read_to_string("/proc/self/cgroup").map_err(|e| format!("/proc/self/cgroup: {e}"))?;
        Ok(mount.join(own_path(&text)?.trim_start_matches('/')))
    }

    /// Moves every process of this one's cgroup into its `host` leaf, this one with them, so that the Runners it
    /// starts find the delegation as their cgroup's parent: what a test suite that shares its delegated cgroup with
    /// its `cargo` does first. A process already in a `host` leaf stays.
    pub fn enter_host() -> Result<(), String> {
        let own = own()?;
        if own.file_name().is_some_and(|name| name == HOST) {
            return Ok(());
        }
        let host = own.join(HOST);
        make(&host).map_err(|e| format!("{e}; is {} delegated to this user?", own.display()))?;
        let procs = fs::read_to_string(own.join("cgroup.procs")).map_err(|e| say(&own, e))?;
        for pid in procs.lines() {
            // A process that ended meanwhile is no matter.
            let _ = fs::write(host.join("cgroup.procs"), pid);
        }
        if self::own()? != host {
            return Err(format!("this process could not move into {}", host.display()));
        }
        Ok(())
    }

    fn make(path: &Path) -> Result<(), String> {
        match fs::create_dir(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(say(path, error)),
        }
    }

    fn enable_in(path: &Path, controllers: &str) -> Result<(), String> {
        let file = path.join("cgroup.subtree_control");
        let enabled = fs::read_to_string(&file).map_err(|e| say(&file, e))?;
        let missing = controllers
            .split(' ')
            .any(|one| !enabled.split_whitespace().any(|have| one == format!("+{have}")));
        if !missing {
            return Ok(());
        }
        fs::write(&file, controllers).map_err(|e| say(&file, format!("{controllers}: {e}")))
    }

    /// One machine's cgroup.
    #[derive(Debug)]
    pub struct MachineGroup {
        path: PathBuf,
    }

    impl MachineGroup {
        /// Its folder.
        pub fn path(&self) -> &Path {
            &self.path
        }

        /// `cgroup.procs`, open for writing: a child writes `0` to it between fork and exec to start inside.
        pub fn procs(&self) -> Result<fs::File, String> {
            let file = self.path.join("cgroup.procs");
            fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_CLOEXEC)
                .open(&file)
                .map_err(|e| say(&file, e))
        }

        /// Kills every process in it at once, threads and children included.
        pub fn kill(&self) {
            if fs::write(self.path.join("cgroup.kill"), "1").is_ok() {
                return;
            }
            // A kernel before 5.14 has no `cgroup.kill`: one by one, until none is left to start another.
            for _ in 0..10 {
                let pids = fs::read_to_string(self.path.join("cgroup.procs")).unwrap_or_default();
                let pids: Vec<i32> = pids
                    .lines()
                    .filter_map(|pid| pid.parse().ok())
                    .filter(|&pid| pid > 0)
                    .collect();
                if pids.is_empty() {
                    return;
                }
                for pid in pids {
                    // SAFETY: kill takes no pointers; a pid that ended meanwhile gets nothing.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
        }

        /// Whether a process is still in it.
        pub fn populated(&self) -> bool {
            fs::read_to_string(self.path.join("cgroup.events"))
                .ok()
                .and_then(|text| value(&text, "populated"))
                .is_none_or(|populated| populated != 0)
        }

        /// What its machine holds of the host's memory ([`super::resident`]).
        pub fn resident(&self) -> Option<u64> {
            let current = fs::read_to_string(self.path.join("memory.current"))
                .ok()?
                .trim()
                .parse()
                .ok()?;
            let stat = fs::read_to_string(self.path.join("memory.stat")).ok()?;
            Some(super::resident(current, &stat))
        }

        /// The most memory it held since it was made, `memory.peak` (kernel 5.19 and later).
        pub fn peak(&self) -> Option<u64> {
            fs::read_to_string(self.path.join("memory.peak"))
                .ok()?
                .trim()
                .parse()
                .ok()
        }

        /// How often the kernel killed in it for want of memory.
        pub fn oom_kills(&self) -> u64 {
            fs::read_to_string(self.path.join("memory.events"))
                .ok()
                .and_then(|text| value(&text, "oom_kill"))
                .unwrap_or(0)
        }

        /// Kills what is left in it and removes it, waiting at most `within` for the kernel to let go of the
        /// processes. `false` when it is still there.
        pub fn remove(&self, within: Duration) -> bool {
            let began = Instant::now();
            loop {
                match fs::remove_dir(&self.path) {
                    Ok(()) => return true,
                    Err(error) if error.kind() == ErrorKind::NotFound => return true,
                    Err(_) if began.elapsed() < within => {
                        self.kill();
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => return false,
                }
            }
        }
    }
}
/// Whether a machine may run where its cgroup has no [`SWAP_MAX`]: only when the host swaps to nothing, as
/// `/proc/swaps` (a line of headings, then one line per device) says. Otherwise the refusal says what to do.
pub fn without_swap_account(proc_swaps: &str) -> Result<(), String> {
    let devices: Vec<&str> = proc_swaps
        .lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    if devices.is_empty() {
        return Ok(());
    }
    Err(format!(
        "this kernel keeps no account of swap per cgroup (a machine's cgroup has no {SWAP_MAX}) and the host swaps \
         to {}, where a machine's memory could go: turn swap off, or boot the kernel with swapaccount=1",
        devices.join(" and ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_is_capped_at_its_size_and_overhead_without_swap() {
        let limits = Limits::of(2 << 30, 160 << 20, Some("4-7".into())).unwrap();
        assert_eq!(
            limits.files(),
            vec![
                ("memory.max", ((2u64 << 30) + (160 << 20)).to_string()),
                ("memory.swap.max", "0".into()),
                ("memory.oom.group", "1".into()),
                ("pids.max", PIDS_MAX.to_string()),
                ("cpuset.cpus", "4-7".into()),
            ]
        );
        let without = Limits::of(1 << 30, 0, None).unwrap();
        assert!(without.files().iter().all(|(file, _)| *file != "cpuset.cpus"));
    }

    #[test]
    fn without_a_swap_account_a_machine_runs_only_on_a_host_that_swaps_to_nothing() {
        let headings = "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n";
        assert_eq!(without_swap_account(headings), Ok(()));
        assert_eq!(without_swap_account(""), Ok(()));
        let one =
            format!("{headings}/dev/zram0                              partition\t4029436\t\t0\t\t100\n");
        let refused = without_swap_account(&one).unwrap_err();
        assert!(refused.contains("swaps to /dev/zram0, where"), "{refused}");
        assert!(refused.contains("swapaccount=1"), "{refused}");
        let two = format!("{one}/swap\\040file                                file\t1048572\t\t0\t\t-2\n");
        assert!(
            without_swap_account(&two)
                .unwrap_err()
                .contains("/dev/zram0 and /swap\\040file,"),
        );
    }

    #[test]
    fn a_size_past_what_a_number_holds_is_refused() {
        assert!(Limits::of(u64::MAX, 1, None).is_err());
        assert_eq!(Limits::of(u64::MAX - 1, 1, None).unwrap().memory_max, u64::MAX);
    }

    #[test]
    fn the_own_cgroup_is_read_from_the_unified_line_only() {
        assert_eq!(
            own_path("0::/system.slice/codefusion-runner.service/host\n"),
            Ok("/system.slice/codefusion-runner.service/host".into())
        );
        assert_eq!(own_path("12:pids:/x\n0::/a\n"), Ok("/a".into()));
        assert!(own_path("12:memory:/x\n").is_err(), "cgroup v1 only");
        assert!(own_path("").is_err());
        assert!(own_path("0::/a/../../etc").is_err());
        assert!(own_path("0::relative").is_err());
    }

    #[test]
    fn values_are_read_by_their_whole_key() {
        let events = "low 0\nhigh 0\nmax 12\noom 1\noom_kill 1\noom_group_kill 1\n";
        assert_eq!(value(events, "oom_kill"), Some(1));
        assert_eq!(value(events, "oom"), Some(1));
        assert_eq!(value(events, "max"), Some(12));
        assert_eq!(value(events, "kill"), None);
        assert_eq!(value("populated x\n", "populated"), None);
        assert_eq!(value("", "populated"), None);
    }

    #[test]
    fn the_page_cache_of_files_is_not_held_but_shared_memory_is() {
        let stat = "anon 1000\nfile 300\nkernel 50\nshmem 100\n";
        assert_eq!(resident(1500, stat), 1300);
        assert_eq!(resident(100, "file 300\n"), 0, "never below nothing");
        assert_eq!(resident(1500, "anon 1500\n"), 1500);
        assert_eq!(
            resident(1500, "file 10\nshmem 20\n"),
            1500,
            "shared memory above the files' is no cache"
        );
    }

    #[test]
    fn machines_need_memory_and_pids_and_take_cpuset_where_there() {
        assert_eq!(
            wanted("cpuset cpu io memory pids\n"),
            Some(vec!["memory", "pids", "cpuset"])
        );
        assert_eq!(wanted("cpu memory pids"), Some(vec!["memory", "pids"]));
        assert_eq!(wanted("cpu pids"), None);
        assert_eq!(wanted("memory"), None);
        assert_eq!(wanted("memoryx pids"), None);
        assert_eq!(wanted(""), None);
    }

    #[test]
    fn a_machine_is_named_after_its_host_and_found_by_it() {
        assert_eq!(machine_name(4321, 0), "vm-4321-0");
        assert_eq!(owner(&machine_name(4321, 17)), Some(4321));
        assert_eq!(owner(&machine_name(u32::MAX, u64::MAX)), Some(u32::MAX));
        for name in [
            "vm-",
            "vm-1",
            "vm-1-",
            "vm--1",
            "vm-a-1",
            "vm-1-a",
            "host",
            "machines",
            "vm-99999999999-1",
            "vm-+1-1",
        ] {
            assert_eq!(owner(name), None, "{name:?}");
        }
        assert!(plain_name(&machine_name(u32::MAX, u64::MAX)));
    }

    #[test]
    fn a_machine_cgroup_name_cannot_walk_out() {
        assert!(plain_name("vm-1234-0"));
        for name in ["", "..", "a/b", "-x", "a b", "x\n", &"a".repeat(65)] {
            assert!(!plain_name(name), "{name:?}");
        }
    }
}
