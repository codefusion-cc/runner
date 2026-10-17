use std::collections::{BTreeMap, BTreeSet, VecDeque};

use thiserror::Error;

use crate::level::{Watch, person_there, profile_for};
use crate::{
    DEFER_LIMIT, Hold, HostReading, Level, Millis, Now, Profile, Settings, SettingsError, Size, Trouble,
};

/// What decides an assigned job's place in line besides when it came ([`Budget::assign_in_turn`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Turn {
    /// A job of the main branch or a deploy: it goes before the other jobs that wait, as long as none of those has
    /// waited [`DEFER_LIMIT`].
    pub urgent: bool,
    /// The repository, `owner/name`. Of the jobs of one kind, those of the repository that started a job least
    /// recently go first, so one busy repository does not keep another waiting.
    pub repo: String,
    /// Not GitHub's job but one the Mac may turn away (a test run): it gets room only as a claim does, while the Mac
    /// takes jobs and under the limits in force (the in-use ones while the person is there), never as a job that is
    /// already the Mac's. Waiting, it is passed over while it does not fit so, and those behind it go on.
    pub refusable: bool,
}

/// One assigned job that waits for room.
#[derive(Clone, Debug)]
struct Waiter {
    slot: SlotId,
    size: Size,
    turn: Turn,
    /// When it began to wait.
    since: Millis,
}

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
    /// How the slot came to be counted: say it where the job's start is logged and reported.
    pub origin: Origin,
}

/// How a slot came to be counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Origin {
    /// The Runner asked for it ([`Budget::claim`]) and the Mac was open.
    Claimed,
    /// GitHub assigned the job without asking ([`Budget::assign`]) and the Mac had room for it.
    Assigned,
    /// The same, while the person was at the Mac, when no claim is granted: the job runs at low priority.
    AssignedInUse,
    /// A VM found running after a restart ([`Budget::adopt`]).
    Adopted,
}

impl Origin {
    /// A short fixed name for reports and logs.
    pub const fn name(self) -> &'static str {
        match self {
            Origin::Claimed => "claimed",
            Origin::Assigned => "assigned",
            Origin::AssignedInUse => "assigned-in-use",
            Origin::Adopted => "adopted",
        }
    }
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
    /// Nothing is wrong any more, but not for long enough yet: `left` as of the newest reading.
    Calming { left: Millis },
    /// The person is at this Mac and it takes no job while they are.
    PersonThere,
    /// The sizes of the VMs would add up to more than the cap.
    Full { cap_bytes: u64, committed_bytes: u64 },
    /// The vCPUs of the VMs would add up to more than the CPU cap.
    Cpus { cap: u32, committed: u32 },
    /// As many VMs run as the owner allows at once ([`Settings::max_machines`]).
    Machines { cap: u32 },
    /// The Mac could not keep its floor if every VM grew to its full size.
    Memory { short_bytes: u64 },
    /// Jobs that GitHub assigned without asking wait for room, and they go first: so many of them.
    AssignedFirst { waiting: usize },
    /// Room is made for a machine of another size ([`Budget::make_room`]).
    MakingRoom { size: Size },
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
            Refusal::Machines { .. } => "max-jobs",
            Refusal::Memory { .. } => "memory",
            Refusal::AssignedFirst { .. } => "assigned-first",
            Refusal::MakingRoom { .. } => "making-room",
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
    /// The physical memory the VM's process holds now: its resident size, which is its footprint less what macOS
    /// compressed or swapped out of it. Not the footprint itself: those pages left the Mac's memory, show as
    /// available again, and come back when the guest touches them, so they are still to come. Nor the resident
    /// size `ps` shows, which counts a guest's memory more than once.
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
    /// Every slot that counts, in the order of their ids, with how it came to be: an assigned job that started while the
    /// person was there is why memory is committed under a cap of zero.
    pub slots: Vec<Held>,
    /// The assigned jobs that wait for room, first in line first, each with what keeps it waiting.
    pub waiting: Vec<Waiting>,
    /// The newest reading, if there is one new enough.
    pub reading: Option<HostReading>,
    /// What would fit now.
    pub free: Free,
}

/// One slot the budget counts, as [`Status`] shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Held {
    /// The slot.
    pub slot: SlotId,
    /// Its size.
    pub size: Size,
    /// How it came to be counted.
    pub origin: Origin,
}

/// One assigned job that waits, as [`Status`] shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Waiting {
    /// The name it waits under.
    pub slot: SlotId,
    /// Its size.
    pub size: Size,
    /// What keeps it waiting now: what is wrong with the Mac, what it is short of, or
    /// [`Refusal::AssignedFirst`] with the number of jobs before it. `None` when nothing does any more and the
    /// next reading starts it.
    pub why: Option<Refusal>,
    /// Whether the settings in force rule its size out altogether (`why` says which limit). It keeps its place
    /// and holds nobody up, but it starts only if the settings change: withdraw it or change them.
    pub never: bool,
}

/// One VM's share of the host, as admission counts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Charge {
    /// The VM's size, counted against the cap.
    pub vm_bytes: u64,
    /// The size plus the host's overhead for the VM: the most it can take from the host.
    pub host_bytes: u64,
    /// What it held when the Mac was last read. Unknown counts as nothing, so all of it is still to come.
    pub resident_bytes: u64,
    /// Its vCPUs.
    pub cpus: u32,
}

/// Whether one more VM of `size` fits: the arithmetic of admission, with no memory of its own.
///
/// It fits when the VMs stay within [`Settings::max_machines`], the sizes within `limits.cap_bytes`, the vCPUs
/// within `limits.cpus`, and
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
    if let Some(cap) = settings.max_machines
        && charges.len() >= cap as usize
    {
        return Err(Refusal::Machines { cap });
    }
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
    origin: Origin,
    /// The newest measurement, and when it came.
    usage: Option<(Millis, Usage)>,
    /// What of the VM's size admission takes as gone from the host already: its resident size as far as the
    /// reading in hand can show it. A measurement newer than that reading does not raise it.
    settled_bytes: u64,
    evicting: bool,
    /// Its place among the slots by when it began to hold memory. A job that waited has an older id than that.
    order: u64,
}

impl Slot {
    fn charge(&self) -> Charge {
        Charge {
            vm_bytes: self.size.vm_bytes(),
            host_bytes: self.host_bytes,
            resident_bytes: self.settled_bytes.min(self.host_bytes),
            cpus: self.cpus,
        }
    }

    /// Takes a measurement as settled once a reading taken no earlier is in hand: that reading's available
    /// memory is already without what the VM held then.
    fn settle(&mut self, reading: &HostReading) {
        if let Some((at, usage)) = self.usage
            && at <= reading.at.elapsed
        {
            self.settled_bytes = usage.resident_bytes;
        }
    }

    /// What the VM holds now; all of it when nobody measured yet, nothing while it is only reserved.
    fn held_bytes(&self) -> u64 {
        if self.expires.is_some() {
            return 0;
        }
        self.usage.map_or(self.host_bytes, |(_, usage)| {
            usage.resident_bytes.min(self.host_bytes)
        })
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
    /// Assigned jobs that have no room yet, in the order they came. They hold nothing; [`Budget::line`] says
    /// which goes first.
    waiting: VecDeque<Waiter>,
    /// For each repository, when it last started an assigned job, as a count of such starts.
    started_last: BTreeMap<String, u64>,
    starts: u64,
    next_slot: u64,
    /// How many slots ever held memory here.
    inserted: u64,
    holds: BTreeSet<Hold>,
    last_eviction: Option<Millis>,
    /// The size room is made for ([`Budget::make_room`]).
    making_room: Option<Size>,
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
            started_last: BTreeMap::new(),
            starts: 0,
            next_slot: 0,
            inserted: 0,
            holds: BTreeSet::new(),
            last_eviction: None,
            making_room: None,
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

    /// Makes room for one machine of `size`, or stops making it with `None`: no slot of another size is granted,
    /// an assigned job of another size waits, and the jobs of `size` that wait go first in line: a slot of `size`
    /// waits only for them. A Mac that keeps running jobs of smaller sizes would otherwise never have the room a
    /// larger one needs.
    pub fn make_room(&mut self, size: Option<Size>) {
        self.making_room = size;
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
            // job was promised or let jobs be stopped in a burst: start both again from now. A measurement from
            // before would look newer than every reading until the clock caught up: it counts as made now.
            let deadline = now.elapsed.plus(self.settings.reservation_ttl);
            for slot in self.slots.values_mut() {
                slot.expires = slot.expires.map(|_| deadline);
                slot.usage = slot.usage.map(|(_, usage)| (now.elapsed, usage));
            }
            self.last_eviction = self.last_eviction.map(|_| now.elapsed);
            for waiter in &mut self.waiting {
                waiter.since = waiter.since.min(now.elapsed);
            }
        }
        if let Some(reading) = self.watch.fresh(&self.settings, now) {
            for slot in self.slots.values_mut() {
                slot.settle(reading);
            }
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
        for slot in self.line(now) {
            let Some(index) = self.waiting.iter().position(|waiter| waiter.slot == slot) else {
                continue;
            };
            let size = self.waiting[index].size;
            if self.waiter_fits(now, size, &self.waiting[index].turn).is_err() {
                // A job that may be turned away holds up nobody behind it while it is: each behind it asks for
                // itself.
                if self.waiting[index].turn.refusable {
                    continue;
                }
                break;
            }
            let Some(waiter) = self.waiting.remove(index) else {
                break;
            };
            self.started(&waiter.turn);
            started.push(self.insert(slot, size, None, self.assigned_origin(now)));
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
        Ok(self.insert(slot, size, Some(expires), Origin::Claimed))
    }

    /// Takes a job that GitHub assigned without asking: a poll with spare capacity can get one while the Mac is
    /// closed. The job is never dropped. It starts now if the Mac can carry it, else it waits and
    /// [`Budget::observe`] starts it ([`Observed::started`]); nothing runs out in either case.
    ///
    /// The job is already this Mac's, so it is treated like one that was running when the person came back: the
    /// person being there does not keep it waiting (it runs at low priority, [`Status::person_there`]), and it
    /// counts against the cap and the CPUs the Mac has while its person is away. Its grant and [`Status::slots`]
    /// say so ([`Origin::AssignedInUse`]). Everything that protects the Mac
    /// still holds for it: the floor in force (the higher one while the person is there), pressure, swapping, the
    /// calm time, heat, battery, disk, a missing reading, and every hold. Those it waits for.
    ///
    /// Jobs that wait start in the order of [`Budget::line`], and while one waits no other slot is granted
    /// ([`Refusal::AssignedFirst`], and [`Budget::free`] is zero, so the Runner offers GitHub nothing), but one of
    /// the size room is made for ([`Budget::make_room`]), which goes before the jobs of other sizes.
    pub fn assign(&mut self, now: Now, size: Size) -> Assigned {
        self.assign_in_turn(now, size, Turn::default())
    }

    /// [`Budget::assign`] for a job whose kind and repository are known: they decide its place in line.
    pub fn assign_in_turn(&mut self, now: Now, size: Size, turn: Turn) -> Assigned {
        if let Err(never) = self.settings.can_run(size) {
            return Assigned::Never(never);
        }
        let slot = self.next_id();
        let fits = self
            .in_turn(size)
            .and_then(|()| self.waiter_fits(now, size, &turn));
        match fits {
            Ok(()) => {
                self.started(&turn);
                Assigned::Start(self.insert(slot, size, None, self.assigned_origin(now)))
            }
            Err(why) => {
                self.waiting.push_back(Waiter {
                    slot,
                    size,
                    turn,
                    since: now.elapsed,
                });
                Assigned::Waits { slot, why }
            }
        }
    }

    /// The jobs that wait, in the order they start: first those of the size room is made for
    /// ([`Budget::make_room`]); then those that waited [`DEFER_LIMIT`] or longer, oldest first; then the urgent ones
    /// ([`Turn::urgent`]); then the rest. Within the urgent ones and within the rest, the repository that started a
    /// job least recently goes first, then the job that came first. A job the settings have since ruled out is not
    /// in it: it holds nobody up.
    ///
    /// No job waits for ever while room is made for no size: each one that waits reaches the group of the overdue
    /// after [`DEFER_LIMIT`], and nothing that comes later goes before it then. Room is made for as long as the
    /// caller says.
    pub fn line(&self, now: Now) -> Vec<SlotId> {
        let mut last = self.started_last.clone();
        let mut starts = self.starts;
        let mut left: Vec<usize> = (0..self.waiting.len())
            .filter(|&index| self.settings.can_run(self.waiting[index].size).is_ok())
            .collect();
        let mut order = Vec::with_capacity(left.len());
        while let Some(pick) = left
            .iter()
            .enumerate()
            .min_by_key(|&(_, &index)| {
                let waiter = &self.waiting[index];
                let overdue = now.elapsed.since(waiter.since) >= DEFER_LIMIT;
                let made = self.making_room == Some(waiter.size);
                let (kind, repo) = match (made, overdue, waiter.turn.urgent) {
                    (true, _, _) => (0, None),
                    (false, true, _) => (1, None),
                    (false, false, true) => (2, Some(last.get(&waiter.turn.repo).copied())),
                    (false, false, false) => (3, Some(last.get(&waiter.turn.repo).copied())),
                };
                (kind, repo, index)
            })
            .map(|(at, _)| at)
        {
            let waiter = &self.waiting[left.remove(pick)];
            starts += 1;
            last.insert(waiter.turn.repo.clone(), starts);
            order.push(waiter.slot);
        }
        order
    }

    /// Gives a job that waits another turn: what it stands for changed while it waited. It keeps the time it began
    /// to wait. An error means it does not wait.
    pub fn set_turn(&mut self, slot: SlotId, turn: Turn) -> Result<(), UnknownSlot> {
        let waiter = self
            .waiting
            .iter_mut()
            .find(|waiter| waiter.slot == slot)
            .ok_or(UnknownSlot(slot))?;
        waiter.turn = turn;
        Ok(())
    }

    /// Forgets an assigned job that still waits: GitHub took it back or it was cancelled. It held nothing. An
    /// error means it does not wait: it started (then [`Budget::release`] is the call), or it was never there.
    pub fn withdraw(&mut self, slot: SlotId) -> Result<Size, UnknownSlot> {
        let index = self.waiting.iter().position(|waiter| waiter.slot == slot);
        let waiter = index
            .and_then(|index| self.waiting.remove(index))
            .ok_or(UnknownSlot(slot))?;
        Ok(waiter.size)
    }

    /// How many assigned jobs wait for room.
    pub fn waiting(&self) -> usize {
        self.waiting.len()
    }

    /// Whether a slot of `size` would be granted now, without reserving it. The answer may be out of date by the
    /// time it is read: only [`Budget::claim`] decides.
    pub fn fits(&self, now: Now, size: Size) -> Result<(), Refusal> {
        let reading = self.takes_jobs(now)?;
        self.in_turn(size)?;
        self.admits(reading, profile_for(&self.settings, Some(reading)), size)
    }

    /// The job for a reserved slot arrived: the reservation no longer runs out. Confirming twice is fine (GitHub
    /// may say so twice). An error means the reservation ran out first: nothing is held, so claim again or let
    /// the job go.
    pub fn confirm(&mut self, slot: SlotId) -> Result<(), UnknownSlot> {
        let held = self.slots.get_mut(&slot).ok_or(UnknownSlot(slot))?;
        held.expires = None;
        Ok(())
    }

    /// [`Budget::confirm`] for a job whose repository is known: its start counts for the repository's turn
    /// ([`Budget::line`]) as one through [`Budget::assign_in_turn`] does.
    pub fn confirm_in_turn(&mut self, slot: SlotId, turn: &Turn) -> Result<(), UnknownSlot> {
        self.confirm(slot)?;
        self.started(turn);
        Ok(())
    }

    /// Counts a VM that was found running after a restart. It was admitted before, so nothing is checked: the
    /// memory is taken whether the budget likes it or not. Adopt the oldest VM first, since the newest slot is
    /// the first to be stopped when memory runs short.
    pub fn adopt(&mut self, size: Size) -> Grant {
        let slot = self.next_id();
        self.insert(slot, size, None, Origin::Adopted)
    }

    /// Records what a slot's VM uses, measured no later than `now`. What the VM has taken from the host stops
    /// counting as still to come with the first reading of the Mac taken at `now` or later: the reading in hand
    /// may be older than the measurement and still show that memory as available, and taking it off twice would
    /// promise memory the Mac no longer has. A VM that holds less than before counts with less at once.
    pub fn usage(&mut self, now: Now, slot: SlotId, usage: Usage) -> Result<(), UnknownSlot> {
        let held = self.slots.get_mut(&slot).ok_or(UnknownSlot(slot))?;
        held.usage = Some((now.elapsed, usage));
        held.settled_bytes = held.settled_bytes.min(usage.resident_bytes);
        Ok(())
    }

    /// What a slot's VM is counted with on the host: the most of the Mac's memory it can take.
    pub fn host_bytes(&self, slot: SlotId) -> Result<u64, UnknownSlot> {
        self.slots
            .get(&slot)
            .map(|held| held.host_bytes)
            .ok_or(UnknownSlot(slot))
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
        self.closed_by(now).map_or(Level::Open, |refusal| refusal.level())
    }

    /// How many more slots of each size would fit now, each size counted alone. This is what the Runner tells
    /// GitHub it has; a slot still needs [`Budget::claim`].
    pub fn free(&self, now: Now) -> Free {
        let Ok(reading) = self.takes_jobs(now) else {
            return Free::default();
        };
        let limits = profile_for(&self.settings, Some(reading));
        let held = self.charges();
        Free(Size::ALL.map(|size| {
            if self.in_turn(size).is_err() {
                return 0;
            }
            let mut charges = held.clone();
            let one_more = Charge {
                vm_bytes: size.vm_bytes(),
                host_bytes: self.settings.host_bytes(size),
                resident_bytes: 0,
                cpus: self.settings.vcpus_of(size),
            };
            let mut more = 0;
            // The cap is at most the Mac's memory, so this ends; the bound is for a setting that slipped through.
            while more < 1024
                && admit(&self.settings, limits, reading.available_bytes, &charges, size).is_ok()
            {
                charges.push(one_more);
                more += 1;
            }
            more
        }))
    }

    /// Everything the tray, the console and `status` show.
    pub fn status(&self, now: Now) -> Status {
        let reading = self.watch.fresh(&self.settings, now).copied();
        Status {
            level: self.level(now),
            closed_by: self.closed_by(now),
            limits: *profile_for(&self.settings, reading.as_ref()),
            committed_bytes: self.committed_bytes(),
            resident_bytes: self.slots.values().fold(0, |sum, slot| {
                sum.saturating_add(slot.usage.map_or(0, |(_, usage)| usage.resident_bytes))
            }),
            cpus_committed: self.cpus_committed(),
            person_there: person_there(&self.settings, reading.as_ref()),
            slots: self
                .slots
                .iter()
                .map(|(id, slot)| Held {
                    slot: *id,
                    size: slot.size,
                    origin: slot.origin,
                })
                .collect(),
            waiting: self.waiting_now(now),
            reading,
            free: self.free(now),
        }
    }

    /// Why the Mac takes no job of any size now: it does not take jobs, or jobs in line go first.
    fn closed_by(&self, now: Now) -> Option<Refusal> {
        self.takes_jobs(now).err().or_else(|| self.assigned_first(None))
    }

    /// [`Budget::health`], and a person at a Mac that takes no new job while they are.
    fn takes_jobs(&self, now: Now) -> Result<&HostReading, Refusal> {
        let reading = self.health(now)?;
        if person_there(&self.settings, Some(reading)) && self.settings.in_use.cap_bytes == 0 {
            return Err(Refusal::PersonThere);
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
        match self.watch.calm_left(&self.settings) {
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
        self.made_for(size)?;
        let limits = Profile {
            floor_bytes: profile_for(&self.settings, Some(reading)).floor_bytes,
            ..self.settings.away
        };
        self.admits(reading, &limits, size)
    }

    /// Whether a job of this turn may start now: an assigned one as [`Budget::assigned_fits`] says, one that may be
    /// turned away ([`Turn::refusable`]) as a claim would be, under the limits in force.
    fn waiter_fits(&self, now: Now, size: Size, turn: &Turn) -> Result<(), Refusal> {
        if !turn.refusable {
            return self.assigned_fits(now, size);
        }
        let reading = self.takes_jobs(now)?;
        self.made_for(size)?;
        self.admits(reading, profile_for(&self.settings, Some(reading)), size)
    }

    /// The arithmetic for one more slot of `size` under `limits`, on what `reading` shows.
    fn admits(&self, reading: &HostReading, limits: &Profile, size: Size) -> Result<(), Refusal> {
        admit(
            &self.settings,
            limits,
            reading.available_bytes,
            &self.charges(),
            size,
        )
    }

    /// Whether a slot of `size` may go before what waits: room made for another size is why it may not, more than
    /// the jobs in line, since none of them gets a slot either.
    fn in_turn(&self, size: Size) -> Result<(), Refusal> {
        self.made_for(size)?;
        self.assigned_first(Some(size)).map_or(Ok(()), Err)
    }

    /// Whether a slot of `size` may be granted while room is made ([`Budget::make_room`]).
    fn made_for(&self, size: Size) -> Result<(), Refusal> {
        match self.making_room {
            Some(made) if made != size => Err(Refusal::MakingRoom { size: made }),
            _ => Ok(()),
        }
    }

    /// What whoever comes after the waiting jobs is told: how many of them go first, before a slot of `size`, or
    /// of any size. A job the settings have since made impossible goes before nobody, so it does not count; nor
    /// does one of another size when room is made for `size` ([`Budget::make_room`]): that size is first in line.
    fn assigned_first(&self, size: Option<Size>) -> Option<Refusal> {
        let made = size.filter(|&size| self.making_room == Some(size));
        let waiting = self
            .waiting
            .iter()
            .filter(|waiter| made.is_none_or(|made| waiter.size == made))
            .filter(|waiter| self.settings.can_run(waiter.size).is_ok())
            .count();
        (waiting > 0).then_some(Refusal::AssignedFirst { waiting })
    }

    /// A repository started an assigned job: its other jobs go after those of repositories that did not since.
    fn started(&mut self, turn: &Turn) {
        self.starts += 1;
        self.started_last.insert(turn.repo.clone(), self.starts);
    }

    /// Every slot as admission counts it.
    fn charges(&self) -> Vec<Charge> {
        self.slots.values().map(Slot::charge).collect()
    }

    /// Every waiting job with what keeps it waiting at `now`.
    fn waiting_now(&self, now: Now) -> Vec<Waiting> {
        let mut ahead = 0;
        // A job the settings ruled out keeps its place; the others take theirs in the order they start.
        let mut line = self.line(now).into_iter();
        self.waiting
            .iter()
            .map(|waiter| match self.settings.can_run(waiter.size) {
                Ok(()) => line
                    .next()
                    .and_then(|slot| self.waiting.iter().find(|w| w.slot == slot))
                    .unwrap_or(waiter),
                Err(_) => waiter,
            })
            .map(|&Waiter { slot, size, .. }| {
                let ruled_out = self.settings.can_run(size).err();
                let why = match ruled_out {
                    Some(limit) => Some(limit),
                    None if ahead == 0 => self.assigned_fits(now, size).err(),
                    None => Some(Refusal::AssignedFirst { waiting: ahead }),
                };
                ahead += usize::from(ruled_out.is_none());
                Waiting {
                    slot,
                    size,
                    why,
                    never: ruled_out.is_some(),
                }
            })
            .collect()
    }

    /// How an assigned job that starts at `now` is marked: with the person there or not.
    fn assigned_origin(&self, now: Now) -> Origin {
        if person_there(&self.settings, self.watch.fresh(&self.settings, now)) {
            Origin::AssignedInUse
        } else {
            Origin::Assigned
        }
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
                running.max_by_key(|(_, slot)| slot.order).map(|(id, _)| *id)?
            }
            EvictionCause::Disk => running
                .map(|(id, slot)| (slot.usage.map_or(0, |(_, usage)| usage.scratch_bytes), *id))
                .filter(|(scratch, _)| *scratch > 0)
                .max()
                .map(|(_, id)| id)?,
        };
        self.slots.get_mut(&slot)?.evicting = true;
        self.last_eviction = Some(now.elapsed);
        Some(Eviction { slot, cause })
    }

    fn insert(&mut self, slot: SlotId, size: Size, expires: Option<Millis>, origin: Origin) -> Grant {
        let cpus = self.settings.vcpus_of(size);
        let host_bytes = self.settings.host_bytes(size);
        self.inserted += 1;
        self.slots.insert(
            slot,
            Slot {
                size,
                cpus,
                host_bytes,
                expires,
                origin,
                usage: None,
                settled_bytes: 0,
                evicting: false,
                order: self.inserted,
            },
        );
        Grant {
            slot,
            size,
            vm_bytes: size.vm_bytes(),
            job_limit_bytes: self.settings.job_limit_bytes(size),
            cpus,
            expires,
            origin,
        }
    }
}
