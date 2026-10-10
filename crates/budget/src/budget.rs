use std::collections::{BTreeMap, BTreeSet, VecDeque};

use thiserror::Error;

use crate::level::{Watch, person_there, profile_for};
use crate::{Hold, HostReading, Level, Millis, Now, Profile, Settings, SettingsError, Size, Trouble};

/// One slot of the budget: the memory and CPUs of one VM, from its reservation to its release.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotId(pub u64);

/// What a slot was granted. The VM is created with exactly this; none of it changes while the slot lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grant {
    /// The slot, to confirm, report usage for and release.
    pub slot: SlotId,
    /// The size asked for.
    pub size: Size,
    /// The VM's memory.
    pub vm_bytes: u64,
    /// The job's memory limit inside the VM.
    pub job_limit_bytes: u64,
    /// The VM's vCPUs.
    pub cpus: u32,
    /// When the reservation is given back unless [`Budget::confirm`] came first. `None` for an adopted slot and
    /// for a job that was assigned ([`Budget::assign`]): those never run out.
    pub expires: Option<Millis>,
}

/// Why a slot was not granted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Someone set a hold.
    Held(Hold),
    /// There is no reading of the host that is new enough to act on.
    Blind,
    /// A reading shows something wrong.
    Trouble(Trouble),
    /// Nothing is wrong any more, but not for long enough yet.
    Calming { left: Millis },
    /// The person is at this Mac and it takes no job while they are.
    PersonThere,
    /// The sizes of the VMs would add up to more than the cap.
    Full { cap_bytes: u64, committed_bytes: u64 },
    /// The vCPUs of the VMs would add up to more than the CPU cap.
    Cpus { cap: u32, committed: u32 },
    /// The Mac could not keep its floor if every VM grew to its full size.
    Memory { short_bytes: u64 },
    /// Jobs that GitHub assigned without asking wait for room, and they go first.
    AssignedFirst { waiting: usize },
}

impl Refusal {
    /// A short fixed name for reports and logs.
    pub const fn name(&self) -> &'static str {
        match self {
            Refusal::Held(hold) => hold.name(),
            Refusal::Blind => Trouble::Blind.name(),
            Refusal::Trouble(trouble) => trouble.name(),
            Refusal::Calming { .. } => "calming",
            Refusal::PersonThere => "person-there",
            Refusal::Full { .. } => "full",
            Refusal::Cpus { .. } => "cpus",
            Refusal::Memory { .. } => "memory",
            Refusal::AssignedFirst { .. } => "assigned-first",
        }
    }

    const fn level(&self) -> Level {
        match self {
            Refusal::Held(_) | Refusal::Blind => Level::Hold,
            Refusal::Trouble(trouble) if !trouble.is_memory() => Level::Hold,
            _ => Level::Tight,
        }
    }
}

/// What became of a job that GitHub assigned without asking ([`Budget::assign`]). The job is never dropped: it
/// starts, or it waits until it can.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Assigned {
    /// It starts now. The slot is the job's until it is released.
    Start(Grant),
    /// It waits under this name, for this reason. [`Budget::observe`] hands out its grant
    /// ([`Observed::started`]) when the Mac has room; until then no other slot is granted.
    Waits { slot: SlotId, why: Refusal },
    /// This Mac can never run a VM of this size with its settings, so nothing waits. Hand the job back, and do
    /// not offer this size here ([`Settings::can_run`]).
    Never(Refusal),
}

/// A slot the budget does not hold: never granted, expired, or given back already.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("slot {} is not held: it was never granted, it expired, or it was given back already", .0.0)]
pub struct UnknownSlot(pub SlotId);

/// What a release gave back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Released {
    /// The slot's size.
    pub size: Size,
    /// The memory that is no longer promised.
    pub vm_bytes: u64,
    /// The vCPUs that are no longer promised.
    pub cpus: u32,
}

/// What a running VM uses, as its vm-host measures it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// The host memory the VM holds now (the footprint of its process).
    pub resident_bytes: u64,
    /// The size of the job's scratch disk on the host.
    pub scratch_bytes: u64,
}

/// Why a job has to stop for the Mac's sake.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EvictionCause {
    /// The Mac is short of memory.
    Memory,
    /// The Mac's disk is nearly full.
    Disk,
}

/// A job to stop, with the reason to tell it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Eviction {
    /// The slot whose VM has to stop. It stays counted until it is released.
    pub slot: SlotId,
    /// Why.
    pub cause: EvictionCause,
}

/// What one reading led to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observed {
    /// The level after this reading.
    pub level: Level,
    /// How long the Mac slept since the reading before; zero when it did not.
    pub slept: Millis,
    /// Reservations that ran out and were given back. Their slots are gone.
    pub expired: Vec<SlotId>,
    /// A job to stop now. At most one per [`Settings::evict_every`].
    pub evict: Option<Eviction>,
    /// Assigned jobs that waited and start now, oldest first. Each grant's slot is the name the job waited under.
    pub started: Vec<Grant>,
}

/// How many more slots of each size would fit now, each size counted alone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Free(pub [u32; 4]);

impl Free {
    /// How many more slots of this size would fit if no other size were claimed.
    pub fn of(&self, size: Size) -> u32 {
        self.0[size.index()]
    }
}

/// The budget's state for the tray, the console and `status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// The level.
    pub level: Level,
    /// Why no slot opens; `None` when the level is open.
    pub closed_by: Option<Refusal>,
    /// The limits in force.
    pub limits: Profile,
    /// The sizes of all VMs and reservations together.
    pub committed_bytes: u64,
    /// The host memory the VMs hold now, as last reported.
    pub resident_bytes: u64,
    /// The vCPUs of all VMs and reservations together.
    pub cpus_committed: u32,
    /// Whether the person counts as being at the Mac: running jobs go on at low priority.
    pub person_there: bool,
    /// How many assigned jobs wait for room.
    pub waiting: usize,
    /// The newest reading, if there is one new enough.
    pub reading: Option<HostReading>,
    /// What would fit now.
    pub free: Free,
}

/// One VM's share of the host, as admission counts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Charge {
    /// The VM's size, counted against the cap.
    pub vm_bytes: u64,
    /// The size plus the host's overhead for the VM: the most it can take from the host.
    pub host_bytes: u64,
    /// What it holds now. Unknown counts as nothing, so all of it is still to come.
    pub resident_bytes: u64,
    /// Its vCPUs.
    pub cpus: u32,
}

/// Whether one more VM of `size` fits: the arithmetic of admission, with no memory of its own.
///
/// It fits when the sizes stay within `limits.cap_bytes`, the vCPUs within `limits.cpus`, and
/// `available_bytes` covers what the VMs in `charges` may still grow by, the new VM in full with its overhead,
/// and the floor. More available memory, fewer charges or looser limits never turn a yes into a no.
pub fn admit(
    settings: &Settings,
    limits: &Profile,
    available_bytes: u64,
    charges: &[Charge],
    size: Size,
) -> Result<(), Refusal> {
    let mut committed_bytes = 0u64;
    let mut committed_cpus = 0u32;
    let mut to_come = 0u64;
    for charge in charges {
        committed_bytes = committed_bytes.saturating_add(charge.vm_bytes);
        committed_cpus = committed_cpus.saturating_add(charge.cpus);
        to_come = to_come.saturating_add(charge.host_bytes.saturating_sub(charge.resident_bytes));
    }
    if committed_bytes.saturating_add(size.vm_bytes()) > limits.cap_bytes {
        return Err(Refusal::Full {
            cap_bytes: limits.cap_bytes,
            committed_bytes,
        });
    }
    if committed_cpus.saturating_add(settings.vcpus_of(size)) > limits.cpus {
        return Err(Refusal::Cpus {
            cap: limits.cpus,
            committed: committed_cpus,
        });
    }
    let needed = to_come
        .saturating_add(settings.host_bytes(size))
        .saturating_add(limits.floor_bytes);
    if available_bytes < needed {
        return Err(Refusal::Memory {
            short_bytes: needed - available_bytes,
        });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct Slot {
    size: Size,
    cpus: u32,
    host_bytes: u64,
    /// When the reservation runs out; `None` once a VM is to be started for it.
    expires: Option<Millis>,
    usage: Option<Usage>,
    evicting: bool,
}

impl Slot {
    fn charge(&self) -> Charge {
        Charge {
            vm_bytes: self.size.vm_bytes(),
            host_bytes: self.host_bytes,
            resident_bytes: self
                .usage
                .map_or(0, |usage| usage.resident_bytes.min(self.host_bytes)),
            cpus: self.cpus,
        }
    }

    /// What the VM holds now; all of it when nobody measured yet, nothing while it is only reserved.
    fn held_bytes(&self) -> u64 {
        if self.expires.is_some() {
            return 0;
        }
        self.usage
            .map_or(self.host_bytes, |usage| usage.resident_bytes.min(self.host_bytes))
    }
}

/// The budget of one Mac. Every method takes the time from the caller, so the same calls in the same order always
/// give the same answers.
///
/// `claim` checks and reserves in one call on `&mut self`: two callers can never both be told that the same
/// memory is free. Share it behind one mutex.
#[derive(Clone, Debug)]
pub struct Budget {
    settings: Settings,
    watch: Watch,
    slots: BTreeMap<SlotId, Slot>,
    /// Assigned jobs that have no room yet, oldest first. They hold nothing.
    waiting: VecDeque<(SlotId, Size)>,
    next_slot: u64,
    holds: BTreeSet<Hold>,
    last_eviction: Option<Millis>,
}

impl Budget {
    /// A budget with no slot and no reading: closed until readings say otherwise for the calm time.
    pub fn new(settings: Settings) -> Result<Budget, SettingsError> {
        settings.check()?;
        Ok(Budget {
            settings,
            watch: Watch::default(),
            slots: BTreeMap::new(),
            waiting: VecDeque::new(),
            next_slot: 0,
            holds: BTreeSet::new(),
            last_eviction: None,
        })
    }

    /// The settings in force.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Replaces the settings. Slots already granted keep what they were granted and stay counted; a smaller cap
    /// stops new slots until enough of them are released.
    pub fn set_settings(&mut self, settings: Settings) -> Result<(), SettingsError> {
        settings.check()?;
        self.settings = settings;
        Ok(())
    }

    /// Sets or lifts a hold. While any hold is set no slot is granted; running jobs go on.
    pub fn hold(&mut self, hold: Hold, on: bool) {
        if on {
            self.holds.insert(hold);
        } else {
            self.holds.remove(&hold);
        }
    }

    /// Takes one reading of the host, or `None` when it could not be read. Call it about once a second, also when
    /// reading failed: a missing call closes the Mac just as a missing reading does.
    pub fn observe(&mut self, now: Now, reading: Option<HostReading>) -> Observed {
        let went_back = self
            .watch
            .observed()
            .is_some_and(|before| now.elapsed < before.elapsed);
        let slept = self.watch.observe(&self.settings, now, reading);
        if went_back {
            // Deadlines set on a clock that then went back would come early or never. Neither may free memory a
            // job was promised or let jobs be stopped in a burst: start both again from now.
            let deadline = now.elapsed.plus(self.settings.reservation_ttl);
            for slot in self.slots.values_mut() {
                slot.expires = slot.expires.map(|_| deadline);
            }
            self.last_eviction = self.last_eviction.map(|_| now.elapsed);
        }
        let expired: Vec<SlotId> = self
            .slots
            .iter()
            .filter(|(_, slot)| slot.expires.is_some_and(|at| now.elapsed >= at))
            .map(|(id, _)| *id)
            .collect();
        for id in &expired {
            self.slots.remove(id);
        }
        let evict = self.pick_eviction(now);
        let mut started = Vec::new();
        while let Some((index, slot, size)) = self.next_waiting() {
            if self.assigned_fits(now, size).is_err() {
                break;
            }
            self.waiting.remove(index);
            started.push(self.insert(slot, size, None));
        }
        Observed {
            level: self.level(now),
            slept,
            expired,
            evict,
            started,
        }
    }

    /// Reserves a slot of `size` if it fits now. The reservation counts in full at once and is given back after
    /// [`Settings::reservation_ttl`] unless [`Budget::confirm`] comes first.
    pub fn claim(&mut self, now: Now, size: Size) -> Result<Grant, Refusal> {
        self.fits(now, size)?;
        let expires = now.elapsed.plus(self.settings.reservation_ttl);
        let slot = self.next_id();
        Ok(self.insert(slot, size, Some(expires)))
    }

    /// Takes a job that GitHub assigned without asking: a poll with spare capacity can get one while the Mac is
    /// closed. The job is never dropped. It starts now if the Mac can carry it, else it waits and
    /// [`Budget::observe`] starts it ([`Observed::started`]); nothing runs out in either case.
    ///
    /// The job is already this Mac's, so it is treated like one that was running when the person came back: the
    /// person being there does not keep it waiting (it runs at low priority, [`Status::person_there`]), and it
    /// counts against the cap and the CPUs the Mac has while its person is away. Everything that protects the Mac
    /// still holds for it: the floor in force (the higher one while the person is there), pressure, swapping, the
    /// calm time, heat, battery, disk, a missing reading, and every hold. Those it waits for.
    ///
    /// Jobs that wait start in the order they came, and while one waits no other slot is granted
    /// ([`Refusal::AssignedFirst`], and [`Budget::free`] is zero, so the Runner offers GitHub nothing).
    pub fn assign(&mut self, now: Now, size: Size) -> Assigned {
        if let Err(never) = self.settings.can_run(size) {
            return Assigned::Never(never);
        }
        let why = match self.next_waiting() {
            Some(_) => Refusal::AssignedFirst {
                waiting: self.waiting.len(),
            },
            None => match self.assigned_fits(now, size) {
                Ok(()) => {
                    let slot = self.next_id();
                    return Assigned::Start(self.insert(slot, size, None));
                }
                Err(why) => why,
            },
        };
        let slot = self.next_id();
        self.waiting.push_back((slot, size));
        Assigned::Waits { slot, why }
    }

    /// Forgets an assigned job that still waits: GitHub took it back or it was cancelled. It held nothing. An
    /// error means it does not wait: it started (then [`Budget::release`] is the call), or it was never there.
    pub fn withdraw(&mut self, slot: SlotId) -> Result<Size, UnknownSlot> {
        let index = self.waiting.iter().position(|(id, _)| *id == slot);
        let (_, size) = index
            .and_then(|index| self.waiting.remove(index))
            .ok_or(UnknownSlot(slot))?;
        Ok(size)
    }

    /// How many assigned jobs wait for room.
    pub fn waiting(&self) -> usize {
        self.waiting.len()
    }

    /// Whether a slot of `size` would be granted now, without reserving it. The answer may be out of date by the
    /// time it is read: only [`Budget::claim`] decides.
    pub fn fits(&self, now: Now, size: Size) -> Result<(), Refusal> {
        let reading = self.gate(now)?;
        let charges: Vec<Charge> = self.slots.values().map(Slot::charge).collect();
        admit(
            &self.settings,
            profile_for(&self.settings, Some(reading)),
            reading.available_bytes,
            &charges,
            size,
        )
    }

    /// The job for a reserved slot arrived: the reservation no longer runs out. Confirming twice is fine (GitHub
    /// may say so twice). An error means the reservation ran out first: nothing is held, so claim again or let
    /// the job go.
    pub fn confirm(&mut self, slot: SlotId) -> Result<(), UnknownSlot> {
        let held = self.slots.get_mut(&slot).ok_or(UnknownSlot(slot))?;
        held.expires = None;
        Ok(())
    }

    /// Counts a VM that was found running after a restart. It was admitted before, so nothing is checked: the
    /// memory is taken whether the budget likes it or not. Adopt the oldest VM first, since the newest slot is
    /// the first to be stopped when memory runs short.
    pub fn adopt(&mut self, size: Size) -> Grant {
        let slot = self.next_id();
        self.insert(slot, size, None)
    }

    /// Records what a slot's VM uses now. What it has already taken from the host is no longer counted as still
    /// to come.
    pub fn usage(&mut self, slot: SlotId, usage: Usage) -> Result<(), UnknownSlot> {
        self.slots.get_mut(&slot).ok_or(UnknownSlot(slot))?.usage = Some(usage);
        Ok(())
    }

    /// Gives a slot's memory and CPUs back. Call it when the VM's process is gone, or when no VM was started. A
    /// second release of the same slot is an error and changes nothing.
    pub fn release(&mut self, slot: SlotId) -> Result<Released, UnknownSlot> {
        let freed = self.slots.remove(&slot).ok_or(UnknownSlot(slot))?;
        Ok(Released {
            size: freed.size,
            vm_bytes: freed.size.vm_bytes(),
            cpus: freed.cpus,
        })
    }

    /// The sizes of all VMs and reservations together.
    pub fn committed_bytes(&self) -> u64 {
        self.slots
            .values()
            .fold(0, |sum, slot| sum.saturating_add(slot.size.vm_bytes()))
    }

    /// The vCPUs of all VMs and reservations together.
    pub fn cpus_committed(&self) -> u32 {
        self.slots
            .values()
            .fold(0, |sum, slot| sum.saturating_add(slot.cpus))
    }

    /// How many slots the budget counts, reservations included.
    pub fn slots(&self) -> usize {
        self.slots.len()
    }

    /// The level now.
    pub fn level(&self, now: Now) -> Level {
        if self.emergency(now).is_some() {
            return Level::Evict;
        }
        self.gate(now)
            .map_or_else(|refusal| refusal.level(), |_| Level::Open)
    }

    /// How many more slots of each size would fit now, each size counted alone. This is what the Runner tells
    /// GitHub it has; a slot still needs [`Budget::claim`].
    pub fn free(&self, now: Now) -> Free {
        let mut free = Free::default();
        let Ok(reading) = self.gate(now) else { return free };
        let limits = profile_for(&self.settings, Some(reading));
        for size in Size::ALL {
            let mut charges: Vec<Charge> = self.slots.values().map(Slot::charge).collect();
            let one_more = Charge {
                vm_bytes: size.vm_bytes(),
                host_bytes: self.settings.host_bytes(size),
                resident_bytes: 0,
                cpus: self.settings.vcpus_of(size),
            };
            // The cap is at most the Mac's memory, so this ends; the bound is for a setting that slipped through.
            while free.0[size.index()] < 1024
                && admit(&self.settings, limits, reading.available_bytes, &charges, size).is_ok()
            {
                charges.push(one_more);
                free.0[size.index()] += 1;
            }
        }
        free
    }

    /// Everything the tray, the console and `status` show.
    pub fn status(&self, now: Now) -> Status {
        let reading = self.watch.fresh(&self.settings, now).copied();
        Status {
            level: self.level(now),
            closed_by: self.gate(now).err(),
            limits: *profile_for(&self.settings, reading.as_ref()),
            committed_bytes: self.committed_bytes(),
            resident_bytes: self.slots.values().fold(0, |sum, slot| {
                sum.saturating_add(slot.usage.map_or(0, |usage| usage.resident_bytes))
            }),
            cpus_committed: self.cpus_committed(),
            person_there: person_there(&self.settings, reading.as_ref()),
            waiting: self.waiting.len(),
            reading,
            free: self.free(now),
        }
    }

    /// Everything that must hold before the arithmetic is even tried. `Ok` carries the reading to decide with.
    fn gate(&self, now: Now) -> Result<&HostReading, Refusal> {
        let reading = self.health(now)?;
        if person_there(&self.settings, Some(reading)) && self.settings.in_use.cap_bytes == 0 {
            return Err(Refusal::PersonThere);
        }
        if self.next_waiting().is_some() {
            return Err(Refusal::AssignedFirst {
                waiting: self.waiting.len(),
            });
        }
        Ok(reading)
    }

    /// What protects the Mac whoever asks: no hold, a reading to act on, nothing wrong, and not for the calm time.
    fn health(&self, now: Now) -> Result<&HostReading, Refusal> {
        if let Some(hold) = self.holds.first() {
            return Err(Refusal::Held(*hold));
        }
        let Some(reading) = self.watch.fresh(&self.settings, now) else {
            return Err(Refusal::Blind);
        };
        if let Some(trouble) = self.watch.troubles().next() {
            return Err(Refusal::Trouble(trouble));
        }
        match self.watch.calm_left(&self.settings, now) {
            Some(Millis::ZERO) => {}
            Some(left) => return Err(Refusal::Calming { left }),
            None => return Err(Refusal::Blind),
        }
        Ok(reading)
    }

    /// Whether an assigned job of `size` may start now: the Mac's health, the widest cap and CPUs it has, and the
    /// floor in force.
    fn assigned_fits(&self, now: Now, size: Size) -> Result<(), Refusal> {
        let reading = self.health(now)?;
        let limits = Profile {
            floor_bytes: profile_for(&self.settings, Some(reading)).floor_bytes,
            ..self.settings.away
        };
        let charges: Vec<Charge> = self.slots.values().map(Slot::charge).collect();
        admit(&self.settings, &limits, reading.available_bytes, &charges, size)
    }

    /// The oldest waiting job this Mac can run at all, with its place in the line. A job that the settings have
    /// since made impossible keeps its place and holds nobody up.
    fn next_waiting(&self) -> Option<(usize, SlotId, Size)> {
        self.waiting
            .iter()
            .enumerate()
            .find(|(_, (_, size))| self.settings.can_run(*size).is_ok())
            .map(|(index, (slot, size))| (index, *slot, *size))
    }

    fn next_id(&mut self) -> SlotId {
        let slot = SlotId(self.next_slot);
        self.next_slot += 1;
        slot
    }

    /// Why a job would have to stop now, if anything says so. Only a fresh reading can say so.
    fn emergency(&self, now: Now) -> Option<EvictionCause> {
        let reading = self.watch.fresh(&self.settings, now)?;
        if self.watch.memory_emergency(&self.settings, now) {
            Some(EvictionCause::Memory)
        } else if reading.disk_free_bytes < self.settings.disk_evict_bytes {
            Some(EvictionCause::Disk)
        } else {
            None
        }
    }

    fn pick_eviction(&mut self, now: Now) -> Option<Eviction> {
        let cause = self.emergency(now)?;
        if self
            .last_eviction
            .is_some_and(|at| now.elapsed.since(at) < self.settings.evict_every)
        {
            return None;
        }
        let running = self
            .slots
            .iter()
            .filter(|(_, slot)| slot.expires.is_none() && !slot.evicting);
        let slot = match cause {
            EvictionCause::Memory => {
                // Stopping a job frees nothing worth having while CI holds next to nothing: then the shortage is
                // someone else's, and the Runner never touches a host process.
                let held = self
                    .slots
                    .values()
                    .fold(0u64, |sum, slot| sum.saturating_add(slot.held_bytes()));
                if held < self.settings.evict_min_held_bytes {
                    return None;
                }
                running.map(|(id, _)| *id).max()?
            }
            EvictionCause::Disk => running
                .map(|(id, slot)| (slot.usage.map_or(0, |usage| usage.scratch_bytes), *id))
                .filter(|(scratch, _)| *scratch > 0)
                .max()
                .map(|(_, id)| id)?,
        };
        self.slots.get_mut(&slot)?.evicting = true;
        self.last_eviction = Some(now.elapsed);
        Some(Eviction { slot, cause })
    }

    fn insert(&mut self, slot: SlotId, size: Size, expires: Option<Millis>) -> Grant {
        let cpus = self.settings.vcpus_of(size);
        let host_bytes = self.settings.host_bytes(size);
        self.slots.insert(
            slot,
            Slot {
                size,
                cpus,
                host_bytes,
                expires,
                usage: None,
                evicting: false,
            },
        );
        Grant {
            slot,
            size,
            vm_bytes: size.vm_bytes(),
            job_limit_bytes: self.settings.job_limit_bytes(size),
            cpus,
            expires,
        }
    }
}
