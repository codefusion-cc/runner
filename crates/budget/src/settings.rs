use thiserror::Error;

use crate::{GIB, HostFacts, MIB, Millis, Refusal, Size, admit};

/// Whether a person works on this Mac.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    /// Nobody works here: the Mac is for CI.
    Dedicated,
    /// Someone's own Mac: slots open only after the person has been away for [`Settings::idle_after`].
    Workstation,
}

/// What keeps each job from the host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Sandbox {
    /// A virtual machine of its own, with its own kernel: the default, and the only kind on a Mac today.
    #[default]
    Vm,
    /// A container of its own in a Docker-compatible engine, on a host with no virtualization to give.
    Container,
}

/// The limits in force at one moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile {
    /// The most the sizes of all VMs may add up to.
    pub cap_bytes: u64,
    /// What the Mac must keep available even if every VM grows to its full size.
    pub floor_bytes: u64,
    /// The most the vCPUs of all VMs may add up to.
    pub cpus: u32,
}

/// How long an assigned job may wait behind jobs that came after it (main-branch and deploy jobs, other
/// repositories) before it goes first. Under the 5 minutes after which GitHub takes back a job no Actions runner
/// took (`docs/github-protocol.md`), so a job passed over still starts before it is taken back. Not a setting: it
/// follows from GitHub's take-back, which no Mac changes, and the scaler orders offered jobs by it too.
pub const DEFER_LIMIT: Millis = Millis::mins(4);

/// Every number the budget decides with. Nothing in the budget's code is a number of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Installed memory.
    pub total_bytes: u64,
    /// Logical CPU cores.
    pub cores: u32,
    /// Whether a person works here.
    pub mode: Mode,
    /// The limits on a dedicated Mac, and on a workstation while its person is away.
    pub away: Profile,
    /// The limits on a workstation while its person is there. A cap of 0 opens nothing ("Also while I work" off).
    pub in_use: Profile,
    /// How long the keyboard and the mouse must rest before the person counts as away.
    pub idle_after: Millis,
    /// What one VM costs the host on top of its size: the VM's own process and the vm-host next to it
    /// ([`Settings::VM_HOST_OVERHEAD`], measured).
    pub vm_host_overhead_bytes: u64,
    /// What the guest's kernel and supervisor take of the VM's size before the job gets any.
    pub guest_overhead_bytes: u64,
    /// vCPUs of a VM of each size, in the order of [`Size::ALL`].
    pub vcpus: [u32; 4],
    /// Whether this is a headless host ([`Settings::for_headless`]), whose cf-4g share the CPUs by how many the
    /// memory holds ([`vcpus_for`]).
    pub headless: bool,
    /// The most VMs at once, whatever the caps leave room for; `None` for as many as they do.
    pub max_machines: Option<u32>,
    /// The most the machines of CI may hold at once, each counted with its size and its overhead
    /// ([`Settings::host_bytes`]), whatever the caps leave room for; `None` for as much as they do. A host that
    /// runs other work beside CI keeps the rest for it.
    pub ci_bytes: Option<u64>,
    /// Once the Mac was under its floor, it must be this far above it before that counts as over.
    pub recover_margin_bytes: u64,
    /// How long nothing may be wrong before slots open again.
    pub calm_for: Millis,
    /// How long after a swap-out the Mac still counts as swapping.
    pub swap_quiet: Millis,
    /// How long pressure must be critical before a job is stopped.
    pub critical_for: Millis,
    /// Swap growing by this much within [`Settings::swap_growth_window`], under pressure, stops a job.
    pub swap_growth_bytes: u64,
    /// The window swap growth is measured over, and how long the pressure must have lasted.
    pub swap_growth_window: Millis,
    /// No job is stopped for memory while all VMs together hold less than this.
    pub evict_min_held_bytes: u64,
    /// At most one job is stopped per this span.
    pub evict_every: Millis,
    /// Under this much free disk, with what the system frees as soon as it is needed
    /// ([`crate::HostReading::disk_reclaimable_bytes`]), no slot opens.
    pub disk_hold_bytes: u64,
    /// Under this much free disk the job with the largest scratch disk is stopped. What the system would free does
    /// not count: a machine's writes need the space now.
    pub disk_evict_bytes: u64,
    /// The size of each job's disk: a machine's scratch disk, or the most a container may write into its layer.
    pub job_disk_bytes: u64,
    /// How long a reserved slot waits for its job before it is given back.
    pub reservation_ttl: Millis,
    /// A reading older than this, or two readings further apart, close the Mac.
    pub stale_after: Millis,
}

/// What the host's owner set on top of the defaults ([`Settings::limited`]); `None` keeps the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Limits {
    /// The most VMs at once: [`Settings::max_machines`].
    pub max_machines: Option<u32>,
    /// How long the keyboard and the mouse must rest before the person counts as away: [`Settings::idle_after`].
    pub idle_after: Option<Millis>,
    /// The floor in percent of the installed memory, whole: what the host keeps available for itself.
    pub floor_percent: Option<u32>,
    /// vCPUs of a VM of each size, in the order of [`Size::ALL`]: [`Settings::vcpus`].
    pub vcpus: [Option<u32>; 4],
    /// The most the machines of CI may hold at once, overheads included: [`Settings::ci_bytes`].
    pub ci_bytes: Option<u64>,
    /// The CPUs CI's machines may take together, from 1 to every core: for a host whose other work leaves CI more
    /// (or fewer) than the defaults give, such as a game server that takes CI jobs only while it is idle.
    pub ci_cpus: Option<u32>,
    /// The free disk under which no job starts: [`Settings::disk_hold_bytes`].
    pub disk_hold_bytes: Option<u64>,
    /// The free disk under which a job is stopped: [`Settings::disk_evict_bytes`].
    pub disk_evict_bytes: Option<u64>,
    /// The size of each job's disk: [`Settings::job_disk_bytes`].
    pub job_disk_bytes: Option<u64>,
}

/// A setting the budget refuses, with what is wrong. Settings come from a file, the tray and the console, so a bad
/// one is a result.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SettingsError {
    /// The Mac's memory or core count is zero.
    #[error("the host has no memory or no cores")]
    NoHost,
    /// A cap and its floor together are more than the Mac has.
    #[error("{profile}: a cap of {cap_bytes} bytes and a floor of {floor_bytes} do not fit {total_bytes}")]
    CapOverHost {
        profile: &'static str,
        cap_bytes: u64,
        floor_bytes: u64,
        total_bytes: u64,
    },
    /// A floor under the least any Mac needs for itself.
    #[error("{profile}: the floor is under {least_bytes} bytes")]
    FloorTooLow { profile: &'static str, least_bytes: u64 },
    /// More CPUs for CI than the Mac has.
    #[error("{profile}: {cpus} CPUs for CI on a Mac with {cores}")]
    CpusOverHost {
        profile: &'static str,
        cpus: u32,
        cores: u32,
    },
    /// CPUs for CI set to none, or to more than the host has.
    #[error("{cpus} CPUs for CI on a host with {cores}")]
    CiCpus { cpus: u32, cores: u32 },
    /// The limits while the person is there are looser than while away.
    #[error("the limits while the Mac is in use are looser than while its person is away")]
    InUseLooser,
    /// A VM size with no vCPU.
    #[error("{label} has no vCPU")]
    NoVcpu { label: &'static str },
    /// A VM size with more vCPUs than CI may use.
    #[error("{label} with {vcpus} vCPUs, where CI may use {cpus}")]
    VcpusOverCpus {
        label: &'static str,
        vcpus: u32,
        cpus: u32,
    },
    /// An overhead as large as the smallest VM leaves a job nothing.
    #[error("the guest or host overhead leaves the smallest VM nothing")]
    OverheadTooLarge,
    /// A limit of no VM at all.
    #[error("the most machines at once is zero")]
    NoMachines,
    /// A floor that is no share of the memory, or all of it.
    #[error("a floor of {percent}% is not between 1% and 90% of the memory")]
    FloorPercent { percent: u32 },
    /// A duration of zero where the budget waits or measures.
    #[error("{what} is zero")]
    ZeroDuration { what: &'static str },
    /// More memory for CI than the host has.
    #[error("memory for CI of {ci_bytes} bytes on a host with {total_bytes}")]
    CiOverHost { ci_bytes: u64, total_bytes: u64 },
    /// Memory for CI that holds not even the smallest machine.
    #[error("memory for CI of {ci_bytes} bytes holds no machine: the smallest needs {least_bytes}")]
    CiUnderSmallest { ci_bytes: u64, least_bytes: u64 },
    /// Jobs would be stopped for a full disk before slots stop opening for it.
    #[error(
        "the free disk under which a job is stopped, {evict_bytes} bytes, is above the one under which no job \
         starts, {hold_bytes}"
    )]
    DiskLimitsCrossed { evict_bytes: u64, hold_bytes: u64 },
    /// A free disk under which no job starts that is under the least a job is stopped at.
    #[error("the free disk under which no job starts is under {least_bytes} bytes")]
    DiskHoldTooLow { least_bytes: u64 },
    /// A disk limit that leaves the Runner no room for its own records and logs before a job is stopped.
    #[error("the free disk under which a job is stopped is under {least_bytes} bytes")]
    DiskEvictTooLow { least_bytes: u64 },
    /// A job's disk too small for a checkout and a build, or larger than any machine's disk may be.
    #[error("a job's disk of {bytes} bytes is not between {least_bytes} and {most_bytes}")]
    JobDisk {
        bytes: u64,
        least_bytes: u64,
        most_bytes: u64,
    },
    /// A disk limit no file system of this size ever meets: no job would start.
    #[error(
        "the free disk under which no job starts, {hold_bytes} bytes, is not under the size of the file system of \
         the state folder, {disk_bytes}: no job would ever start"
    )]
    DiskHoldOverDisk { hold_bytes: u64, disk_bytes: u64 },
    /// A job's disk larger than the file system it is on.
    #[error(
        "a job's disk of {job_bytes} bytes is larger than the file system of the state folder, {disk_bytes}"
    )]
    JobDiskOverDisk { job_bytes: u64, disk_bytes: u64 },
}

/// What a container host's disk settings leave short of, though the Runner starts on them: the file system of the
/// state folder holds less than the free disk under which no job starts and one job's whole disk together. A job
/// that writes its whole layer then takes the host under that free disk, and may be stopped for it
/// ([`Settings::check_disk`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskShort {
    /// [`Settings::disk_hold_bytes`].
    pub hold_bytes: u64,
    /// [`Settings::job_disk_bytes`].
    pub job_bytes: u64,
    /// The size of the file system of the state folder.
    pub disk_bytes: u64,
}

impl std::fmt::Display for DiskShort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the free disk under which no job starts ({} bytes) and one job's disk ({}) do not fit the file system \
             of the state folder ({}): a job that writes its whole disk may be stopped for a full disk",
            self.hold_bytes, self.job_bytes, self.disk_bytes
        )
    }
}

impl Settings {
    /// What one VM is counted with on the host on top of its size: its vm-host, and what the hypervisor's process
    /// holds that is not the guest's memory. A number no full machine exceeds, since a slot is opened on the
    /// promise that the machine never takes more.
    ///
    /// Measured with jobs that take all the memory they get (`cargo xtask it --only full-memory`): the vm-host
    /// holds 50 MB. The hypervisor's share is known between two numbers, since the guest's kernel does not say
    /// how much it touched of the memory it keeps out of its own count: between 27 MB and 83 MB with a 1 GB
    /// machine, whose kernel keeps the least (a 4 GB machine says between 21 MB and 199 MB). So a full machine
    /// costs between 76 MB and 131 MB beyond its size, plus the page tables of a larger guest (2 MB for each
    /// 4 GB). 64 MB, the estimate before the measurement, was below even the lower number; 192 MB is above the
    /// higher one for every size.
    ///
    /// Measured again on a 16 GB Mac mini running CI (macOS 26.6, 2026-10-13), with `footprint` and `vmmap`: a
    /// cf-4g machine's process held 4109 MiB with 4061 MiB of guest memory touched, and 4136 MiB with 4088 MiB, so
    /// 48 MiB of its own both times; its vm-host 48 MiB. 96 MiB in all, half of what is counted. The 6.3 GiB `ps`
    /// shows as that process's resident size is no measure of it: it counts the guest's memory more than once.
    pub const VM_HOST_OVERHEAD: u64 = 192 * MIB;

    /// The plan's estimate of what the guest's kernel and supervisor take of a VM's size.
    pub const GUEST_OVERHEAD_ESTIMATE: u64 = GIB / 4;

    /// What one container costs the host on top of its size: the vm-host beside it and the engine's shim for it,
    /// some 30 MB together, with room. The container's own memory, its supervisor's included, is its size: the
    /// engine holds it to that.
    pub const CONTAINER_HOST_OVERHEAD: u64 = 64 * MIB;

    /// What the engine of a host that runs jobs in containers holds for itself: its daemon and containerd, and a
    /// rootless one's network helper.
    pub const ENGINE_USE: u64 = GIB / 4;

    /// No Mac keeps less than this for itself, whatever a setting asks for.
    pub const LEAST_FLOOR: u64 = GIB;

    /// The free disk under which no job starts unless the host's settings name another
    /// ([`Settings::disk_hold_bytes`]).
    pub const DISK_HOLD: u64 = 25 * GIB;

    /// The free disk under which a job is stopped unless the host's settings name another
    /// ([`Settings::disk_evict_bytes`]).
    pub const DISK_EVICT: u64 = 5 * GIB;

    /// The size of each job's disk unless the host's settings name another ([`Settings::job_disk_bytes`]).
    pub const JOB_DISK: u64 = 40 * GIB;

    /// The sizes a job's disk may have: a checkout, its dependencies and a build need a few gigabytes, and the
    /// vm-host refuses a disk over 512 GiB.
    pub const JOB_DISK_RANGE: std::ops::RangeInclusive<u64> = 4 * GIB..=512 * GIB;

    /// The least free disk under which a job is stopped ([`Settings::disk_evict_bytes`]): the Runner's records,
    /// its outbox and its log need room while a job is stopped.
    pub const LEAST_DISK_EVICT: u64 = GIB;

    /// The defaults for a Mac of this size.
    ///
    /// - Cap: all memory but a reserve of 25% (at least 3 GB) on a dedicated Mac, 40% (at least 4 GB) on a
    ///   workstation whose person is away, and nothing while the person is there. A 16 GB Mac: 12 GB and 9.6 GB.
    /// - Floor: 2 GB, 3 GB and 4 GB on a 16 GB Mac (dedicated, away, in use).
    /// - CPUs: all cores but two; half the cores while the person is there. A Mac whose cores differ gives CI only
    ///   its performance cores ([`Settings::on_fast_cores`]). 2 vCPUs for `cf-2g` and 4 for
    ///   `cf-6g` and `cf-8g`. `cf-4g` gets half of the CPUs CI may use, at most 4 and at least 2, so that another
    ///   machine runs beside it: 4 on a Mac with 10 cores, 3 on one with 8 (what the Go manager gave a job there).
    ///   Where the cap leaves no room beside a `cf-4g` (an 8 GB Mac), it takes up to 4 of them ([`vcpus_for`]).
    /// - 15 minutes without the person, a 2 minute reservation, 60 s of calm, 30 s after a swap-out, and a job
    ///   stopped after 10 s of critical pressure on a workstation, 30 s on a dedicated Mac.
    pub fn for_host(facts: HostFacts, mode: Mode) -> Settings {
        let total = facts.total_bytes;
        let all_but_two = facts.cores.saturating_sub(2).max(1).min(facts.cores);
        let (away, in_use) = match mode {
            Mode::Dedicated => {
                let always = Profile {
                    cap_bytes: total.saturating_sub((total / 4).max(3 * GIB)),
                    floor_bytes: GIB + total / 16,
                    cpus: all_but_two,
                };
                (always, always)
            }
            Mode::Workstation => (
                Profile {
                    cap_bytes: total.saturating_sub((total / 5 * 2).max(4 * GIB)),
                    floor_bytes: (total / 16 * 3).min(2 * GIB + total / 16),
                    cpus: all_but_two,
                },
                Profile {
                    cap_bytes: 0,
                    floor_bytes: (total / 4).min(2 * GIB + total / 8),
                    cpus: facts.cores / 2,
                },
            ),
        };
        Settings {
            total_bytes: total,
            cores: facts.cores,
            mode,
            away,
            in_use,
            idle_after: Millis::mins(15),
            vm_host_overhead_bytes: Self::VM_HOST_OVERHEAD,
            guest_overhead_bytes: Self::GUEST_OVERHEAD_ESTIMATE,
            vcpus: vcpus_for(all_but_two, away.cap_bytes, false),
            headless: false,
            max_machines: None,
            ci_bytes: None,
            recover_margin_bytes: GIB,
            calm_for: Millis::secs(60),
            swap_quiet: Millis::secs(30),
            critical_for: match mode {
                Mode::Dedicated => Millis::secs(30),
                Mode::Workstation => Millis::secs(10),
            },
            swap_growth_bytes: 512 * MIB,
            swap_growth_window: Millis::secs(60),
            evict_min_held_bytes: GIB,
            evict_every: Millis::secs(30),
            disk_hold_bytes: Self::DISK_HOLD,
            disk_evict_bytes: Self::DISK_EVICT,
            job_disk_bytes: Self::JOB_DISK,
            reservation_ttl: Millis::mins(2),
            stale_after: Millis::secs(5),
        }
    }

    /// These settings with CI on the fast cores of a host whose cores are not all alike: the performance cores of
    /// an Apple chip, the big cores of a board. A vCPU that lands on a slow core runs a job's tests at a fraction of
    /// the speed, and the guest cannot tell, so its timeouts fire. An M1 Mac mini has 4 performance cores of 8;
    /// all but two gave CI 6 and a cf-4g 3, so two jobs side by side ran on the efficiency cores too.
    ///
    /// `fast_cores` counts the fast kind. On a host whose cores are all alike it is all of them, or 0, and the
    /// settings stay as they are. One fast core holds no machine (a cf-2g has 2 vCPUs), so a host with a single
    /// prime core counts as one whose cores are alike. While a workstation's person is there CI keeps half the
    /// cores, and never more than the fast ones.
    pub fn on_fast_cores(mut self, fast_cores: u32) -> Settings {
        if !(2..self.cores).contains(&fast_cores) {
            return self;
        }
        self.away.cpus = fast_cores;
        self.in_use.cpus = match self.mode {
            Mode::Dedicated => fast_cores,
            Mode::Workstation => self.in_use.cpus.min(fast_cores),
        };
        self.vcpus = vcpus_for(fast_cores, self.away.cap_bytes, self.headless);
        self
    }

    /// These settings for jobs in containers ([`Sandbox::Container`]): no guest kernel takes a share of a job's
    /// size, each job costs the host [`Settings::CONTAINER_HOST_OVERHEAD`] rather than a hypervisor's, and the
    /// engine's own [`Settings::ENGINE_USE`] comes off the caps. A headless host counts its room with these from
    /// the start ([`Settings::for_headless_in`]).
    pub fn in_containers(mut self) -> Settings {
        self.vm_host_overhead_bytes = Self::CONTAINER_HOST_OVERHEAD;
        self.guest_overhead_bytes = 0;
        for profile in [&mut self.away, &mut self.in_use] {
            profile.cap_bytes = profile.cap_bytes.saturating_sub(Self::ENGINE_USE);
        }
        self
    }

    /// These settings on a host that runs one machine at a time and nothing else but the Runner: a trusted host
    /// (`class: trusted`), which asks GitHub for one job at a time. With no second machine and nobody at it, CI takes
    /// every core: the fast ones of a host whose cores differ ([`Settings::on_fast_cores`]), all of them otherwise.
    /// A cf-4g takes up to 4 vCPUs, as when it runs alone, and no size takes more than CI has. On a headless host the
    /// cap keeps the overhead of that one machine.
    ///
    /// Without this, a 2-vCPU cloud VM (an n2-standard-2 with 7.76 GB) would keep two for itself, leave CI one and
    /// run no size at all. With it, both go to a cf-4g, the largest its memory holds.
    pub fn one_at_a_time(mut self, fast_cores: u32) -> Settings {
        let cpus = if (2..self.cores).contains(&fast_cores) {
            fast_cores
        } else {
            self.cores
        };
        self.away.cpus = cpus;
        if self.headless {
            self.away.cap_bytes = self
                .total_bytes
                .saturating_sub(self.away.floor_bytes)
                .saturating_sub(Self::HEADLESS_HOST_USE)
                .saturating_sub(self.vm_host_overhead_bytes);
        }
        self.in_use = match self.mode {
            Mode::Dedicated => self.away,
            Mode::Workstation => Profile {
                cpus: self.in_use.cpus.min(cpus),
                ..self.in_use
            },
        };
        self.vcpus = [2, cpus.clamp(2, 4), 4, 4].map(|vcpus| vcpus.min(cpus));
        self.max_machines = Some(1);
        self
    }

    /// What a lean Linux host with nobody at it holds for itself: the kernel, systemd, sshd and the Runner. 0.4 GB
    /// measured with nothing to do on an RK3588 board with 7.75 GB (`MemTotal` less `MemAvailable`), Docker installed
    /// and idle; half a gigabyte with a little room.
    pub const HEADLESS_HOST_USE: u64 = GIB / 2;

    /// The defaults for a Linux host nobody works at, such as a board on a shelf: the reserve is what the host
    /// needs, not the Mac's.
    ///
    /// [`Settings::for_host`] keeps 25% and at least 3 GB, for macOS and a person's session, which hold several
    /// gigabytes. A headless Linux host holds half a gigabyte ([`Settings::HEADLESS_HOST_USE`]); the Mac's rule
    /// would leave a 7.75 GB board 4.75 GB, one cf-4g and never two cf-2g.
    ///
    /// `fast_cores` is how many cores the machines run on: the fast kind of a board whose cores are not all alike,
    /// which machines are pinned to, so that a vCPU never moves between two kinds of core
    /// ([`Settings::on_fast_cores`]).
    ///
    /// - Floor: 1 GB and 1/32 of the memory, for the page cache that keeps the image the machines boot from and
    ///   for what the host's programs do between two readings. 1.24 GB on 7.75 GB.
    /// - Room for machines: all memory but the floor and [`Settings::HEADLESS_HOST_USE`]. 6.0 GB on 7.75 GB.
    /// - Cap: that room less [`Settings::VM_HOST_OVERHEAD`] for each machine the CPUs let run side by side. The cap
    ///   counts sizes while each machine costs its size and the overhead, so every set of machines under the cap
    ///   fits the room with all their overheads. 5.64 GB on 7.75 GB with 4 fast cores: one cf-4g (4.2 GB with
    ///   its overhead) or two cf-2g (4.4 GB) at a time; a cf-4g beside a cf-2g (6.4 GB) never, nor a cf-6g (6.2 GB
    ///   alone would leave the host 1.5 GB). A board of the same kind with 8.13 GB or more takes either.
    /// - CPUs: the fast cores, with the slow ones left to the host; on a host whose cores are all alike, all but
    ///   two, and at least four where the memory holds two cf-4g: a 4-vCPU cloud VM with 16 GB runs two of 2. 2 vCPUs for cf-2g, half the CPUs for cf-4g but no more than its share among as many cf-4g as the
    ///   cap holds (at least 2, at most 4), 4 for the larger sizes: on 4 fast cores, two machines of 2 vCPUs; on
    ///   a 16 GB cloud VM with 8 vCPUs, three cf-4g of 2. A cf-4g with no room beside it under the cap takes up to
    ///   4 ([`vcpus_for`]): on 7.75 GB, the four fast cores, which no other machine could use while it runs.
    /// - Times and disk limits as on a dedicated Mac.
    pub fn for_headless(facts: HostFacts, fast_cores: u32) -> Settings {
        Settings::for_headless_in(facts, fast_cores, Sandbox::Vm)
    }

    /// [`Settings::for_headless`] for jobs in this sandbox. In containers each job costs the host
    /// [`Settings::CONTAINER_HOST_OVERHEAD`] and the engine [`Settings::ENGINE_USE`] once
    /// ([`Settings::in_containers`]), so the room and the cap are counted with those.
    pub fn for_headless_in(facts: HostFacts, fast_cores: u32, sandbox: Sandbox) -> Settings {
        let total = facts.total_bytes;
        let mut settings = Settings::for_host(facts, Mode::Dedicated).on_fast_cores(fast_cores);
        let engine = match sandbox {
            Sandbox::Vm => 0,
            Sandbox::Container => {
                settings = settings.in_containers();
                Self::ENGINE_USE
            }
        };
        let floor = GIB + total / 32;
        // Where the cores are alike and the memory holds two cf-4g, two of 2 vCPUs on four cores: nobody sits at the
        // host, so it needs no two cores of its own.
        let alike = !(2..facts.cores).contains(&fast_cores);
        let two_cf_4g = 2 * (Size::G4.vm_bytes() + settings.vm_host_overhead_bytes);
        let room = total
            .saturating_sub(floor)
            .saturating_sub(Self::HEADLESS_HOST_USE)
            .saturating_sub(engine);
        let cpus = if alike && facts.cores >= 4 && room >= two_cf_4g {
            settings.away.cpus.max(4)
        } else {
            settings.away.cpus
        };
        let side_by_side = u64::from(cpus / settings.vcpus_of(Size::G2));
        let limits = Profile {
            cap_bytes: room.saturating_sub(side_by_side.saturating_mul(settings.vm_host_overhead_bytes)),
            floor_bytes: floor,
            cpus,
        };
        settings.headless = true;
        settings.vcpus = vcpus_for(cpus, limits.cap_bytes, true);
        settings.away = limits;
        settings.in_use = limits;
        settings
    }

    /// The floor an owner may set, in percent of the memory ([`Limits::floor_percent`]).
    pub const FLOOR_PERCENT: std::ops::RangeInclusive<u32> = 1..=90;

    /// These settings with what the owner set on top of them, checked.
    ///
    /// - The most machines at once, when set.
    /// - The vCPUs of a size, when set: at least 1, and no more than CI may use. A floor set beside them changes the
    ///   other sizes' only. On a headless host, a cap that kept the overhead of fewer machines than the CPUs now run
    ///   side by side is lowered by theirs.
    /// - The time without the person, which only a workstation waits for.
    /// - The floor as a share of the memory, 1% to 90%, and never under [`Settings::LEAST_FLOOR`]. It is the floor
    ///   on a dedicated host and on a workstation whose person is away; while a workstation's person is there the
    ///   floor stays the higher of the two. A cap that would leave the host less than its floor is lowered to what is left.
    /// - The memory for CI ([`Settings::ci_bytes`]), when set: the cap becomes what it leaves after one machine's
    ///   overhead, and a floor not set as a share becomes what it leaves the host, at least [`Settings::LEAST_FLOOR`].
    /// - The free disk under which no job starts and the one under which a job is stopped, when set. A hold set
    ///   without the other lowers the default stop to it, never raises it: a hold of 3 GiB stops jobs under 3 GiB.
    /// - The size of each job's disk, when set.
    pub fn limited(mut self, limits: &Limits) -> Result<Settings, SettingsError> {
        self.max_machines = limits.max_machines.or(self.max_machines);
        if let Some(hold) = limits.disk_hold_bytes {
            self.disk_hold_bytes = hold;
            if limits.disk_evict_bytes.is_none() {
                self.disk_evict_bytes = self.disk_evict_bytes.min(hold);
            }
        }
        if let Some(evict) = limits.disk_evict_bytes {
            self.disk_evict_bytes = evict;
        }
        if let Some(job) = limits.job_disk_bytes {
            self.job_disk_bytes = job;
        }
        if let Some(idle_after) = limits.idle_after {
            self.idle_after = idle_after;
        }
        if let Some(percent) = limits.floor_percent {
            if !Self::FLOOR_PERCENT.contains(&percent) {
                return Err(SettingsError::FloorPercent { percent });
            }
            let floor = crate::percent_of(self.total_bytes, percent);
            self.away.floor_bytes = floor;
            self.in_use.floor_bytes = self.in_use.floor_bytes.max(floor);
            let away_cap = self.away.cap_bytes.min(self.total_bytes.saturating_sub(floor));
            if away_cap != self.away.cap_bytes {
                self.away.cap_bytes = away_cap;
                self.vcpus = vcpus_for(self.away.cpus, away_cap, self.headless);
            }
            self.in_use.cap_bytes = self
                .in_use
                .cap_bytes
                .min(self.total_bytes.saturating_sub(self.in_use.floor_bytes));
        }
        // As many machines side by side as the cap kept the overhead of: more CPUs for CI, or fewer vCPUs a size, run
        // more.
        let side_by_side = self.most_side_by_side();
        // CPUs for CI, set: the most every profile gives, and the sizes' vCPUs follow, as on a host with that many.
        if let Some(cpus) = limits.ci_cpus {
            if !(1..=self.cores).contains(&cpus) {
                return Err(SettingsError::CiCpus {
                    cpus,
                    cores: self.cores,
                });
            }
            self.away.cpus = cpus;
            self.in_use.cpus = self.in_use.cpus.min(cpus);
            self.vcpus = vcpus_for(cpus, self.away.cap_bytes, self.headless);
        }
        // Memory for CI, set: the host keeps the rest for itself, and never less than the least floor. A floor set as
        // a share stays. The sizes alone add up to no more than it leaves after one machine's overhead, and `admit`
        // counts each machine's overhead on top. It may be more than the default cap: whoever set it knows the host.
        if let Some(ci) = limits.ci_bytes {
            self.ci_bytes = Some(ci);
            if limits.floor_percent.is_none() {
                let rest = self.total_bytes.saturating_sub(ci).max(Self::LEAST_FLOOR);
                self.away.floor_bytes = self.away.floor_bytes.min(rest);
                self.in_use.floor_bytes = self.in_use.floor_bytes.min(rest).max(self.away.floor_bytes);
            }
            let sizes = ci
                .saturating_sub(self.vm_host_overhead_bytes)
                .min(self.total_bytes.saturating_sub(self.away.floor_bytes));
            self.away.cap_bytes = sizes;
            self.vcpus = vcpus_for(self.away.cpus, sizes, self.headless);
            self.in_use.cap_bytes = self.in_use.cap_bytes.min(sizes);
        }
        for (size, vcpus) in Size::ALL.into_iter().zip(limits.vcpus) {
            let Some(vcpus) = vcpus else { continue };
            if vcpus > self.away.cpus {
                return Err(SettingsError::VcpusOverCpus {
                    label: size.label(),
                    vcpus,
                    cpus: self.away.cpus,
                });
            }
            self.vcpus[size.index()] = vcpus;
        }
        // A headless host's cap keeps the overhead of as many machines as its CPUs run: more CPUs for CI or fewer vCPUs
        // run more. Memory for CI counts each machine's overhead as it starts already.
        let more = self.most_side_by_side().saturating_sub(side_by_side);
        if self.headless && more > 0 && self.ci_bytes.is_none() {
            let overheads = u64::from(more).saturating_mul(self.vm_host_overhead_bytes);
            self.away.cap_bytes = self.away.cap_bytes.saturating_sub(overheads);
            self.in_use.cap_bytes = self.in_use.cap_bytes.min(self.away.cap_bytes);
        }
        // Nobody is ever there on a dedicated host: one set of limits.
        if self.mode == Mode::Dedicated {
            self.in_use = self.away;
        }
        self.check()?;
        Ok(self)
    }

    /// The cap "Also while I work" gives a workstation: all memory but 60% (at least 5.5 GB). A 16 GB Mac: 6.4 GB.
    pub fn cap_while_in_use(total_bytes: u64) -> u64 {
        total_bytes.saturating_sub((total_bytes / 5 * 3).max(11 * GIB / 2))
    }

    /// Whether the budget can work with these settings. Every setting that passes keeps the cap and the floor
    /// together inside the Mac's memory.
    pub fn check(&self) -> Result<(), SettingsError> {
        if self.total_bytes == 0 || self.cores == 0 {
            return Err(SettingsError::NoHost);
        }
        for (profile, limits) in [("away", &self.away), ("in use", &self.in_use)] {
            if limits.floor_bytes < Self::LEAST_FLOOR {
                return Err(SettingsError::FloorTooLow {
                    profile,
                    least_bytes: Self::LEAST_FLOOR,
                });
            }
            let fits = limits
                .cap_bytes
                .checked_add(limits.floor_bytes)
                .is_some_and(|sum| sum <= self.total_bytes);
            if !fits {
                return Err(SettingsError::CapOverHost {
                    profile,
                    cap_bytes: limits.cap_bytes,
                    floor_bytes: limits.floor_bytes,
                    total_bytes: self.total_bytes,
                });
            }
            if limits.cpus > self.cores {
                return Err(SettingsError::CpusOverHost {
                    profile,
                    cpus: limits.cpus,
                    cores: self.cores,
                });
            }
        }
        if self.in_use.cap_bytes > self.away.cap_bytes
            || self.in_use.floor_bytes < self.away.floor_bytes
            || self.in_use.cpus > self.away.cpus
        {
            return Err(SettingsError::InUseLooser);
        }
        if let Some(size) = Size::ALL.into_iter().find(|size| self.vcpus[size.index()] == 0) {
            return Err(SettingsError::NoVcpu { label: size.label() });
        }
        let smallest = Size::G2.vm_bytes();
        if self.guest_overhead_bytes >= smallest || self.vm_host_overhead_bytes >= smallest {
            return Err(SettingsError::OverheadTooLarge);
        }
        for (what, span) in [
            ("the time without the person", self.idle_after),
            ("the calm time", self.calm_for),
            ("the time after a swap-out", self.swap_quiet),
            ("the time under critical pressure", self.critical_for),
            ("the swap growth window", self.swap_growth_window),
            ("the time between stopped jobs", self.evict_every),
            ("the reservation time", self.reservation_ttl),
            ("the age a reading may have", self.stale_after),
        ] {
            if span == Millis::ZERO {
                return Err(SettingsError::ZeroDuration { what });
            }
        }
        if self.max_machines == Some(0) {
            return Err(SettingsError::NoMachines);
        }
        if let Some(ci_bytes) = self.ci_bytes {
            if ci_bytes > self.total_bytes {
                return Err(SettingsError::CiOverHost {
                    ci_bytes,
                    total_bytes: self.total_bytes,
                });
            }
            let least_bytes = self.host_bytes(Size::G2);
            if ci_bytes < least_bytes {
                return Err(SettingsError::CiUnderSmallest {
                    ci_bytes,
                    least_bytes,
                });
            }
        }
        // Said of the hold first: one set alone takes the stop down with it, and the owner set the hold.
        if self.disk_hold_bytes < Self::LEAST_DISK_EVICT {
            return Err(SettingsError::DiskHoldTooLow {
                least_bytes: Self::LEAST_DISK_EVICT,
            });
        }
        if self.disk_evict_bytes < Self::LEAST_DISK_EVICT {
            return Err(SettingsError::DiskEvictTooLow {
                least_bytes: Self::LEAST_DISK_EVICT,
            });
        }
        if self.disk_evict_bytes > self.disk_hold_bytes {
            return Err(SettingsError::DiskLimitsCrossed {
                evict_bytes: self.disk_evict_bytes,
                hold_bytes: self.disk_hold_bytes,
            });
        }
        if !Self::JOB_DISK_RANGE.contains(&self.job_disk_bytes) {
            return Err(SettingsError::JobDisk {
                bytes: self.job_disk_bytes,
                least_bytes: *Self::JOB_DISK_RANGE.start(),
                most_bytes: *Self::JOB_DISK_RANGE.end(),
            });
        }
        Ok(())
    }

    /// These settings against the file system of the state folder, of `disk_bytes` (0 when the system will not
    /// say, and then nothing is checked). Only what the owner set (`limits`) is refused, so a host on the defaults
    /// starts as it always did:
    ///
    /// - a free disk under which no job starts that is not under the file system's size, since no file system is
    ///   ever that free and no job would start;
    /// - a job's disk larger than the file system.
    ///
    /// In containers, where every job writes onto that file system, a hold and one job's disk that do not fit it
    /// together are [`DiskShort`]: the Runner starts and says so. Not refused, since a job's disk is the most it may
    /// write, not what it takes: most jobs write a fraction of it, and the free disk under which a job is stopped
    /// still keeps the host from filling up.
    pub fn check_disk(
        &self,
        limits: &Limits,
        sandbox: Sandbox,
        disk_bytes: u64,
    ) -> Result<Option<DiskShort>, SettingsError> {
        if disk_bytes == 0 {
            return Ok(None);
        }
        if limits.disk_hold_bytes.is_some() && self.disk_hold_bytes >= disk_bytes {
            return Err(SettingsError::DiskHoldOverDisk {
                hold_bytes: self.disk_hold_bytes,
                disk_bytes,
            });
        }
        if limits.job_disk_bytes.is_some() && self.job_disk_bytes > disk_bytes {
            return Err(SettingsError::JobDiskOverDisk {
                job_bytes: self.job_disk_bytes,
                disk_bytes,
            });
        }
        let fits = self
            .disk_hold_bytes
            .checked_add(self.job_disk_bytes)
            .is_some_and(|both| both <= disk_bytes);
        Ok((sandbox == Sandbox::Container && !fits).then_some(DiskShort {
            hold_bytes: self.disk_hold_bytes,
            job_bytes: self.job_disk_bytes,
            disk_bytes,
        }))
    }

    /// Whether this Mac could run a VM of this size at all: alone, with all its memory available and its person
    /// away. Sizes that fail are not worth offering to GitHub from this Mac.
    pub fn can_run(&self, size: Size) -> Result<(), Refusal> {
        admit(self, &self.away, self.total_bytes, &[], size)
    }

    /// How many VMs of this size run side by side under `limits`: as many as its cap and its CPUs take. Whether
    /// the Mac has the memory for each is decided when it is asked for.
    pub fn side_by_side(&self, limits: &Profile, size: Size) -> u32 {
        let by_cap = u32::try_from(limits.cap_bytes / size.vm_bytes().max(1)).unwrap_or(u32::MAX);
        let by_ci = self.ci_bytes.map_or(u32::MAX, |ci| {
            u32::try_from(ci / self.host_bytes(size).max(1)).unwrap_or(u32::MAX)
        });
        by_cap
            .min(by_ci)
            .min(limits.cpus / self.vcpus_of(size).max(1))
            .min(self.max_machines.unwrap_or(u32::MAX))
    }

    /// The most machines the CPUs CI may use run side by side: as many as of the size with the fewest vCPUs.
    fn most_side_by_side(&self) -> u32 {
        self.away.cpus / self.vcpus.iter().copied().min().unwrap_or(1).max(1)
    }

    /// vCPUs of a VM of this size.
    pub fn vcpus_of(&self, size: Size) -> u32 {
        self.vcpus[size.index()]
    }

    /// The memory limit of the job inside a VM of this size: the size minus the guest's own share.
    pub fn job_limit_bytes(&self, size: Size) -> u64 {
        size.vm_bytes().saturating_sub(self.guest_overhead_bytes)
    }

    /// What a VM of this size is counted with on the host: its size plus the host's overhead for it.
    pub fn host_bytes(&self, size: Size) -> u64 {
        size.vm_bytes().saturating_add(self.vm_host_overhead_bytes)
    }
}

/// The vCPUs of each size where CI may use `cpus` and the sizes of all machines may add up to `cap_bytes`: 2 for
/// cf-2g, 4 for the larger sizes, and for cf-4g half of the CPUs (at least 2, at most 4), so that another machine
/// runs beside it. Where the cap has no room for even a cf-2g beside a cf-4g, the cf-4g runs alone and takes all of
/// them, at most 4: half would leave the rest idle for as long as it runs. Measured on an RK3588 board with 4 fast
/// cores, a Vitest suite took 210 s on 2 of them and 95 s on all 4.
///
/// On a `headless` host, which runs CI and nothing else, a cf-4g takes no more than its share of the CPUs among
/// as many cf-4g as the cap holds: a 16 GB cloud VM with 6 CPUs for CI runs three of 2 vCPUs, not two of 3 with
/// 4 GB idle while jobs queue. A Mac keeps half: its jobs run faster, and its person's work needs the rest.
fn vcpus_for(cpus: u32, cap_bytes: u64, headless: bool) -> [u32; 4] {
    let alone = cap_bytes < Size::G4.vm_bytes() + Size::G2.vm_bytes();
    let mut g4 = if alone { cpus } else { cpus / 2 };
    if headless {
        let fit = u32::try_from(cap_bytes / Size::G4.vm_bytes()).unwrap_or(u32::MAX);
        g4 = g4.min(cpus / fit.max(1));
    }
    [2, g4.clamp(2, 4), 4, 4]
}
