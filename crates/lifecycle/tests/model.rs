//! The slot's state machine checked against its spec, in two ways.
//!
//! 1. Every state that any order of events can reach is visited (a search over the whole state space, with a model
//!    of the world next to the slot: whether the VM's process lives, whether the budget counts the slot), and the
//!    rules of the crate's page are checked at each step and from each state.
//! 2. Several slots share one real budget through random orders of events, crashes and daemon restarts; at the
//!    end nothing is promised and no process is left.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

use cf_budget::{
    Budget, EvictionCause, GIB, HostFacts, HostReading, Millis, Mode, Now, Pressure, Settings, Size, SlotId,
    Thermal,
};
use cf_lifecycle::{Effect, End, Event, Limits, Phase, Saved, Slot, Timer};
use proptest::prelude::*;

/// The plan's limits, with a job of five minutes instead of six hours so that the search stays small. The order
/// of the limits is the same.
fn limits() -> Limits {
    Limits {
        job: Millis::mins(5),
        ..Limits::default()
    }
}

/// The slot, and next to it what is true in the world.
#[derive(Clone, Debug)]
struct World {
    slot: Slot,
    now: Millis,
    /// The vm-host's process exists.
    vm: bool,
    /// It was sent SIGKILL.
    killed: bool,
    /// The budget counts the slot.
    counted: bool,
    starts: u32,
    releases: u32,
    ended: Option<End>,
    naps: u8,
    restarts: u8,
}

type Key = (
    Saved,
    Vec<(Timer, Millis)>,
    bool,
    bool,
    bool,
    u32,
    (u32, Option<End>, u8, u8),
);

impl World {
    fn new() -> World {
        World {
            slot: Slot::reserved(limits()),
            now: Millis::secs(1000),
            vm: false,
            killed: false,
            counted: true,
            starts: 0,
            releases: 0,
            ended: None,
            naps: 0,
            restarts: 0,
        }
    }

    /// Two worlds with the same key behave the same from here on: time only matters as time left.
    fn key(&self) -> Key {
        let rest = (self.releases, self.ended, self.naps, self.restarts);
        (
            self.slot.saved(),
            self.slot.timers(self.now),
            self.vm,
            self.killed,
            self.counted,
            self.starts,
            rest,
        )
    }

    /// Does what the slot says, as the daemon would, and fails on anything the spec forbids.
    fn apply(&mut self, effects: &[Effect]) -> Result<(), String> {
        for (index, effect) in effects.iter().enumerate() {
            match effect {
                Effect::StartVm => {
                    if self.vm {
                        return Err("a second VM was started while the first one's process lives".into());
                    }
                    self.vm = true;
                    self.killed = false;
                    self.starts += 1;
                    if self.starts > limits().starts {
                        return Err(format!("{} VMs were started for one job", self.starts));
                    }
                }
                Effect::StopVm { .. } | Effect::Tell(_) if !self.vm => {
                    return Err(format!("{effect:?} with no VM"));
                }
                Effect::KillVm | Effect::Stuck if !self.vm => return Err(format!("{effect:?} with no VM")),
                Effect::KillVm => self.killed = true,
                Effect::Stuck if !self.killed => return Err("stuck without a kill before it".into()),
                Effect::Release => {
                    if self.vm {
                        return Err("memory given back while the VM's process lives".into());
                    }
                    if !self.counted {
                        return Err("memory given back that the budget does not count".into());
                    }
                    self.counted = false;
                    self.releases += 1;
                }
                Effect::Ended { end, .. } => {
                    if self.ended.is_some() {
                        return Err("ended twice".into());
                    }
                    if index + 1 != effects.len() {
                        return Err("something came after the end".into());
                    }
                    self.ended = Some(*end);
                }
                _ => {}
            }
        }
        self.check()
    }

    /// What holds after every step.
    fn check(&self) -> Result<(), String> {
        let phase = self.slot.phase();
        let vm_phases = !matches!(phase, Phase::Reserved | Phase::Released);
        if self.vm != vm_phases {
            return Err(format!("in {phase:?} the VM's process is alive: {}", self.vm));
        }
        if self.slot.holds_memory() != self.counted {
            return Err(format!(
                "the slot thinks it holds memory: {}, the budget: {}",
                self.slot.holds_memory(),
                self.counted
            ));
        }
        if (phase == Phase::Released) != self.ended.is_some() {
            return Err(format!("{phase:?} with end {:?}", self.ended));
        }
        if phase == Phase::Released {
            if self.counted || self.vm {
                return Err("released with memory still counted or a VM alive".into());
            }
            if self.slot.next_deadline().is_some() {
                return Err("a watchdog runs on a released slot".into());
            }
        } else if phase != Phase::Reserved && self.slot.next_deadline().is_none() {
            return Err(format!(
                "{phase:?} has no watchdog: nothing but a message could end it"
            ));
        }
        if self.releases > 1 {
            return Err("memory given back twice".into());
        }
        Ok(())
    }

    fn send(&mut self, event: Event) -> Result<(), String> {
        let before = self.slot.clone();
        let effects = self.slot.on(self.now, event);
        // A released slot does nothing more; it only keeps a word of a job that came late.
        // A word of a job after the end proves it came up and ran one; an end of "no job" was wrong then.
        let learned = |saved: Saved| {
            if !matches!(event, Event::JobStarted | Event::JobCompleted) {
                return saved;
            }
            Saved {
                ran_job: true,
                up: true,
                end: saved
                    .end
                    .map(|end| if end == End::NoJob { End::Stopped } else { end }),
                ..saved
            }
        };
        if before.phase() == Phase::Released
            && (!effects.is_empty() || self.slot.saved() != learned(before.saved()))
        {
            return Err(format!("{event:?} did something to a released slot"));
        }
        self.apply(&effects)?;
        // Only GitHub's 204 to our own removal says no job ran.
        let no_job = |slot: &Slot| slot.saved().end == Some(End::NoJob);
        if no_job(&self.slot) && !no_job(&before) && event != Event::Unregistered {
            return Err(format!("{event:?} ended the slot as one that ran no job"));
        }
        if before.phase() == Phase::Running
            && self.slot.phase() != Phase::Running
            && matches!(event, Event::Unregistered | Event::Retire)
        {
            return Err(format!("{event:?} stopped a running job"));
        }
        Ok(())
    }

    /// One move of the world. `None` when the move cannot happen where the world is.
    fn step(&self, action: Action) -> Option<Result<World, String>> {
        let mut next = self.clone();
        let result = match action {
            Action::Send(event) => {
                // A sleep is time that passed: the daemon hears of it after the wake.
                if let Event::Slept(span) = event {
                    next.now = next.now.plus(span);
                }
                next.send(event)
            }
            Action::VmDies => {
                if !self.vm {
                    return None;
                }
                next.vm = false;
                next.send(Event::VmExited)
            }
            Action::Expire => {
                // The budget takes back only what was never confirmed.
                if self.slot.phase() != Phase::Reserved || !self.counted {
                    return None;
                }
                next.counted = false;
                next.send(Event::Expired)
            }
            Action::Nap => {
                if self.naps >= 1 {
                    return None;
                }
                next.naps += 1;
                next.now = next.now.plus(Millis::secs(7));
                next.send(Event::Slept(Millis::secs(7)))
            }
            Action::Pass => {
                // Under a minute, so that a guest whose heartbeats arrive can grow old in its phase.
                next.now = next.now.plus(Millis::secs(45));
                next.send(Event::Tick)
            }
            Action::Timeout => {
                next.now = self.slot.next_deadline()?;
                next.send(Event::Tick)
            }
            Action::Restart => {
                if self.restarts >= 2 {
                    return None;
                }
                next.restarts += 1;
                // The new budget counts what the daemon adopts: the VMs that are alive.
                next.counted = self.vm;
                let (slot, effects) = Slot::restore(limits(), self.now, self.slot.saved(), self.vm);
                next.slot = slot;
                next.apply(&effects)
            }
        };
        Some(result.map(|()| next))
    }

    /// Time alone, the budget's two minutes for a reservation, and the death of a process that was sent SIGKILL
    /// must bring every slot to its end: nothing else may be needed.
    fn runs_out(&self) -> Result<World, String> {
        let mut world = self.clone();
        for _ in 0..12 {
            let action = match world.slot.phase() {
                Phase::Released => return Ok(world),
                Phase::Reserved => Action::Expire,
                _ if world.killed => Action::VmDies,
                _ => Action::Timeout,
            };
            world = world
                .step(action)
                .ok_or_else(|| format!("{:?} cannot happen in {:?}", action, world.slot.phase()))??;
        }
        Err(format!("still {:?} after twelve timeouts", world.slot.phase()))
    }
}

#[derive(Clone, Copy, Debug)]
enum Action {
    Send(Event),
    VmDies,
    Expire,
    Nap,
    Pass,
    Timeout,
    Restart,
}

fn actions() -> Vec<Action> {
    let events = [
        Event::Assigned,
        Event::Lost,
        Event::VmReady,
        Event::JobStarted,
        Event::Heartbeat,
        Event::OutOfMemory,
        Event::JobCompleted,
        Event::Evict(EvictionCause::Memory),
        Event::Evict(EvictionCause::Disk),
        Event::Stop,
        Event::Unregistered,
        Event::Retire,
        Event::Slept(Millis::mins(6)),
        Event::Tick,
    ];
    let mut all: Vec<Action> = events.into_iter().map(Action::Send).collect();
    all.extend([
        Action::VmDies,
        Action::Expire,
        Action::Nap,
        Action::Pass,
        Action::Timeout,
        Action::Restart,
    ]);
    all
}

#[test]
fn every_reachable_state_keeps_the_rules_and_has_a_way_out() {
    let mut seen: HashSet<Key> = HashSet::new();
    let mut queue = VecDeque::from([(World::new(), Vec::<Action>::new())]);
    seen.insert(World::new().key());
    let mut phases = BTreeSet::new();
    let mut ends = BTreeSet::new();
    while let Some((world, path)) = queue.pop_front() {
        phases.insert(world.slot.phase());
        ends.extend(world.ended);
        let out = world
            .runs_out()
            .unwrap_or_else(|why| panic!("{why}\nafter {path:?}"));
        assert_eq!(
            out.releases + u32::from(!out.counted && out.releases == 0),
            1,
            "after {path:?}"
        );
        for action in actions() {
            let Some(next) = world.step(action) else { continue };
            let next = next.unwrap_or_else(|why| panic!("{why}\nafter {path:?} then {action:?}"));
            if seen.insert(next.key()) {
                assert!(
                    seen.len() < 200_000,
                    "the search does not end: {path:?} then {action:?}"
                );
                let mut longer = path.clone();
                longer.push(action);
                queue.push_back((next, longer));
            }
        }
    }
    // The search must have gone everywhere: every phase, every end, and far more states than there are phases.
    assert_eq!(phases.len(), 10, "{phases:?}");
    assert_eq!(ends.len(), 11, "{ends:?}");
    assert!(seen.len() > 1000, "only {} states", seen.len());
}

#[test]
fn memory_is_given_back_once_on_every_path_that_held_it() {
    // The same search, counting per end how the memory went back: by the slot's release, by the budget's own
    // expiry, or by a restart that found no VM (the new budget never counted it).
    let mut seen: HashSet<Key> = HashSet::new();
    let mut queue = VecDeque::from([World::new()]);
    let mut by_release: BTreeMap<End, u32> = BTreeMap::new();
    while let Some(world) = queue.pop_front() {
        if let Some(end) = world.ended {
            assert!(world.releases <= 1 && !world.counted && !world.vm);
            *by_release.entry(end).or_default() += world.releases;
            // An end with a VM behind it gave memory back through the slot unless a restart found the VM dead.
            if world.starts > 0 && world.restarts == 0 {
                assert_eq!(world.releases, 1, "{end:?} after a VM was started");
            }
        }
        for action in actions() {
            if let Some(Ok(next)) = world.step(action)
                && seen.insert(next.key())
            {
                assert!(seen.len() < 200_000, "the search does not end");
                queue.push_back(next);
            }
        }
    }
    for end in [
        End::Ok,
        End::Oom,
        End::EvictedMemory,
        End::EvictedDisk,
        End::Timeout,
        End::GuestLost,
        End::HostSlept,
        End::Stopped,
        End::Lost,
        End::StartFailed,
        End::NoJob,
    ] {
        assert!(
            by_release.get(&end).copied().unwrap_or(0) > 0,
            "{end:?} never released memory"
        );
    }
}

// ---- Several slots on one real budget ----

fn calm(at: Millis) -> HostReading {
    HostReading {
        at: Now {
            awake: at,
            elapsed: at,
        },
        available_bytes: 64 * GIB,
        pressure: Pressure::Normal,
        swap_used_bytes: 0,
        swapouts: 0,
        idle: Millis::mins(60),
        thermal: Thermal::Nominal,
        on_battery: false,
        disk_free_bytes: 500 * GIB,
        disk_reclaimable_bytes: 0,
    }
}

fn settings() -> Settings {
    let mut settings = Settings::for_host(
        HostFacts {
            total_bytes: 64 * GIB,
            cores: 16,
        },
        Mode::Dedicated,
    );
    settings.vcpus = [1; 4];
    settings
}

/// A budget that has watched a calm Mac for a minute, at `now`.
fn open_budget(now: &mut Millis) -> Budget {
    let mut budget = Budget::new(settings()).unwrap();
    for _ in 0..=60 {
        *now = now.plus(Millis::secs(1));
        budget.observe(
            Now {
                awake: *now,
                elapsed: *now,
            },
            Some(calm(*now)),
        );
    }
    budget
}

struct Job {
    id: SlotId,
    slot: Slot,
    vm: bool,
    killed: bool,
    ended: bool,
}

struct Fleet {
    budget: Budget,
    now: Millis,
    jobs: Vec<Job>,
}

impl Fleet {
    fn apply(&mut self, index: usize, effects: Vec<Effect>) -> Result<(), TestCaseError> {
        for effect in effects {
            let job = &mut self.jobs[index];
            match effect {
                Effect::StartVm => {
                    prop_assert!(!job.vm, "two VMs for one slot");
                    job.vm = true;
                    job.killed = false;
                }
                Effect::KillVm => job.killed = true,
                Effect::Release => {
                    prop_assert!(!job.vm, "released under a living VM");
                    prop_assert!(
                        self.budget.release(job.id).is_ok(),
                        "the budget did not hold what was released"
                    );
                }
                Effect::Ended { .. } => {
                    prop_assert!(!job.ended, "ended twice");
                    job.ended = true;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn send(&mut self, index: usize, event: Event) -> Result<(), TestCaseError> {
        let event = match event {
            // The daemon's rule: a job is only started on a reservation the budget confirmed.
            Event::Assigned if self.jobs[index].slot.phase() == Phase::Reserved => {
                if self.budget.confirm(self.jobs[index].id).is_ok() {
                    Event::Assigned
                } else {
                    Event::Expired
                }
            }
            Event::VmExited => {
                if !self.jobs[index].vm {
                    return Ok(());
                }
                self.jobs[index].vm = false;
                Event::VmExited
            }
            // Only the budget says that a reservation ran out.
            Event::Expired => return Ok(()),
            other => other,
        };
        let effects = self.jobs[index].slot.on(self.now, event);
        self.apply(index, effects)
    }

    fn pass(&mut self, span: Millis) -> Result<(), TestCaseError> {
        self.now = self.now.plus(span);
        let observed = self.budget.observe(
            Now {
                awake: self.now,
                elapsed: self.now,
            },
            Some(calm(self.now)),
        );
        for id in observed.expired {
            let index = self
                .jobs
                .iter()
                .position(|job| job.id == id && !job.ended)
                .unwrap();
            prop_assert_eq!(
                self.jobs[index].slot.phase(),
                Phase::Reserved,
                "the budget took back a slot with a VM"
            );
            let effects = self.jobs[index].slot.on(self.now, Event::Expired);
            self.apply(index, effects)?;
        }
        for index in 0..self.jobs.len() {
            self.send(index, Event::Tick)?;
        }
        Ok(())
    }

    /// The daemon dies and starts again: a new budget, live VMs adopted, every slot restored from what was saved.
    /// The new budget numbers its slots from the start again, so the ids of slots that ended name nothing.
    fn restart(&mut self) -> Result<(), TestCaseError> {
        self.budget = open_budget(&mut self.now);
        for index in 0..self.jobs.len() {
            let saved = self.jobs[index].slot.saved();
            let alive = self.jobs[index].vm;
            if alive {
                self.jobs[index].id = self.budget.adopt(Size::G2).slot;
            }
            if self.jobs[index].ended {
                continue;
            }
            let (slot, effects) = Slot::restore(Limits::default(), self.now, saved, alive);
            self.jobs[index].slot = slot;
            self.apply(index, effects)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
enum Move {
    Claim,
    Send(usize, Event),
    Pass(u64),
    Restart,
}

fn a_move() -> impl Strategy<Value = Move> {
    let event = prop_oneof![
        6 => Just(Event::Assigned),
        1 => Just(Event::Lost),
        5 => Just(Event::VmReady),
        5 => Just(Event::JobStarted),
        3 => Just(Event::Heartbeat),
        1 => Just(Event::OutOfMemory),
        4 => Just(Event::JobCompleted),
        4 => Just(Event::VmExited),
        1 => Just(Event::Evict(EvictionCause::Memory)),
        1 => Just(Event::Evict(EvictionCause::Disk)),
        1 => Just(Event::Stop),
        1 => (0..400_000u64).prop_map(|ms| Event::Slept(Millis(ms))),
    ];
    prop_oneof![
        4 => Just(Move::Claim),
        20 => (0..6usize, event).prop_map(|(slot, event)| Move::Send(slot, event)),
        6 => prop_oneof![3 => 0..4000u64, 1 => 0..200_000u64].prop_map(Move::Pass),
        1 => Just(Move::Restart),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 300, ..ProptestConfig::default() })]

    #[test]
    fn any_order_of_events_crashes_and_restarts_ends_with_everything_released(moves in prop::collection::vec(a_move(), 1..120)) {
        let mut now = Millis::secs(5);
        let mut fleet = Fleet { budget: open_budget(&mut now), now, jobs: Vec::new() };
        for step in moves {
            match step {
                Move::Claim => {
                    let now = Now { awake: fleet.now, elapsed: fleet.now };
                    if let Ok(grant) = fleet.budget.claim(now, Size::G2) {
                        fleet.jobs.push(Job { id: grant.slot, slot: Slot::reserved(Limits::default()), vm: false, killed: false, ended: false });
                    }
                }
                Move::Send(index, event) => {
                    if index < fleet.jobs.len() {
                        fleet.send(index, event)?;
                    }
                }
                Move::Pass(ms) => fleet.pass(Millis(ms))?,
                Move::Restart => fleet.restart()?,
            }
            let held = fleet.jobs.iter().filter(|job| job.slot.holds_memory()).count();
            prop_assert_eq!(fleet.budget.slots(), held, "the budget and the slots disagree about what is held");
            for job in &fleet.jobs {
                let vm_phase = !matches!(job.slot.phase(), Phase::Reserved | Phase::Released);
                prop_assert_eq!(job.vm, vm_phase, "{:?}", job.slot.phase());
            }
        }
        // Now nothing more arrives from GitHub or the guests: time, and the death of what was killed, must do.
        for _ in 0..40 {
            for index in 0..fleet.jobs.len() {
                if fleet.jobs[index].killed {
                    fleet.send(index, Event::VmExited)?;
                }
            }
            fleet.pass(Millis::mins(4))?;
        }
        for job in &fleet.jobs {
            prop_assert!(job.ended && !job.vm, "{:?} is still {:?}", job.id, job.slot.phase());
        }
        prop_assert_eq!((fleet.budget.slots(), fleet.budget.committed_bytes(), fleet.budget.cpus_committed()), (0, 0, 0));
    }
}
