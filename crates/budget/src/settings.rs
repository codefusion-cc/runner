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
    /// What one VM costs the host on top of its size: the VM's own process and the vm-host next to it.
    /// M1 measures it; until then this is the estimate [`Settings::VM_HOST_OVERHEAD_ESTIMATE`].
    pub vm_host_overhead_bytes: u64,
    /// What the guest's kernel and supervisor take of the VM's size before the job gets any.
    pub guest_overhead_bytes: u64,
    /// vCPUs of a VM of each size, in the order of [`Size::ALL`].
    pub vcpus: [u32; 4],
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
    /// Under this much free disk no slot opens.
    pub disk_hold_bytes: u64,
    /// Under this much free disk the job with the largest scratch disk is stopped.
    pub disk_evict_bytes: u64,
    /// How long a reserved slot waits for its job before it is given back.
    pub reservation_ttl: Millis,
    /// A reading older than this, or two readings further apart, close the Mac.
    pub stale_after: Millis,
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
    /// The limits while the person is there are looser than while away.
    #[error("the limits while the Mac is in use are looser than while its person is away")]
    InUseLooser,
    /// A VM size with no vCPU.
    #[error("{label} has no vCPU")]
    NoVcpu { label: &'static str },
    /// An overhead as large as the smallest VM leaves a job nothing.
    #[error("the guest or host overhead leaves the smallest VM nothing")]
    OverheadTooLarge,
    /// A duration of zero where the budget waits or measures.
    #[error("{what} is zero")]
    ZeroDuration { what: &'static str },
    /// Jobs would be stopped for a full disk before slots stop opening for it.
    #[error("the disk limit that stops a job is above the one that stops new slots")]
    DiskLimitsCrossed,
}

impl Settings {
    /// The estimate of what one VM costs the host on top of its size, until M1 has measured it: 20 MB for the
    /// vm-host (the plan's target) plus 23 MB that the VM's own process showed over a 10 GB guest (the memory
    /// report: Virtualization at 10,263 MB), rounded up.
    pub const VM_HOST_OVERHEAD_ESTIMATE: u64 = 64 * MIB;

    /// The plan's estimate of what the guest's kernel and supervisor take of a VM's size.
    pub const GUEST_OVERHEAD_ESTIMATE: u64 = GIB / 4;

    /// No Mac keeps less than this for itself, whatever a setting asks for.
    pub const LEAST_FLOOR: u64 = GIB;

    /// The defaults for a Mac of this size.
    ///
    /// - Cap: all memory but a reserve of 25% (at least 3 GB) on a dedicated Mac, 40% (at least 4 GB) on a
    ///   workstation whose person is away, and nothing while the person is there. A 16 GB Mac: 12 GB and 9.6 GB.
    /// - Floor: 2 GB, 3 GB and 4 GB on a 16 GB Mac (dedicated, away, in use).
    /// - CPUs: all cores but two; half the cores while the person is there. 2 vCPUs for `cf-2g` and 4 for
    ///   `cf-6g` and `cf-8g`. `cf-4g` gets half of the CPUs CI may use, at most 4 and at least 2, so that two of
    ///   them run side by side: 4 on a Mac with 10 cores, 3 on one with 8 (what the Go manager gave a job there).
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
            vm_host_overhead_bytes: Self::VM_HOST_OVERHEAD_ESTIMATE,
            guest_overhead_bytes: Self::GUEST_OVERHEAD_ESTIMATE,
            vcpus: [2, (all_but_two / 2).clamp(2, 4), 4, 4],
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
            disk_hold_bytes: 25 * GIB,
            disk_evict_bytes: 5 * GIB,
            reservation_ttl: Millis::mins(2),
            stale_after: Millis::secs(5),
        }
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
        if self.disk_evict_bytes > self.disk_hold_bytes {
            return Err(SettingsError::DiskLimitsCrossed);
        }
        Ok(())
    }

    /// Whether this Mac could run a VM of this size at all: alone, with all its memory available and its person
    /// away. Sizes that fail are not worth offering to GitHub from this Mac.
    pub fn can_run(&self, size: Size) -> Result<(), Refusal> {
        admit(self, &self.away, self.total_bytes, &[], size)
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
