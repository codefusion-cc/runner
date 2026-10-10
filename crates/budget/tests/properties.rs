//! What must hold for every order of readings, claims, releases, failures, sleeps and clock jumps.
//!
//! The test keeps its own record of what happened (every reading with its time, every slot) and checks each
//! answer of the budget against the rules as the plan states them, not against how the budget keeps its state.

mod common;

use cf_budget::{
    Budget, Charge, EvictionCause, GIB, Hold, HostFacts, HostReading, Level, MIB, Millis, Mode, Now,
    Pressure, Profile, Settings, Size, SlotId, Thermal, Usage, admit,
};
use common::*;
use proptest::prelude::*;

#[derive(Clone, Debug)]
enum Op {
    /// Time passes, then a reading is taken and handed over.
    Read {
        after_ms: u64,
        reading: Shape,
    },
    /// Time passes, then the reader fails.
    Fail {
        after_ms: u64,
    },
    /// Time passes and nobody reads.
    Idle {
        ms: u64,
    },
    /// The Mac sleeps: only the elapsed clock moves.
    Sleep {
        ms: u64,
    },
    /// A clock goes back (it cannot, and the budget must still not open on it).
    Back {
        ms: u64,
        both: bool,
    },
    Claim(Size),
    Confirm(usize),
    Release(usize),
    ReleaseGone(usize),
    Usage {
        slot: usize,
        resident_mib: u64,
        scratch_mib: u64,
    },
    Hold(bool),
}

/// What a reading shows, apart from its time.
#[derive(Clone, Debug)]
struct Shape {
    available_mib: u64,
    pressure: Pressure,
    swapped_out: u64,
    swap_grew_mib: u64,
    idle_min: u64,
    thermal: Thermal,
    on_battery: bool,
    disk_gib: u64,
    late_ms: u64,
}

fn fine() -> Shape {
    Shape {
        available_mib: 16 * 1024,
        pressure: Pressure::Normal,
        swapped_out: 0,
        swap_grew_mib: 0,
        idle_min: 60,
        thermal: Thermal::Nominal,
        on_battery: false,
        disk_gib: 100,
        late_ms: 0,
    }
}

fn shape() -> impl Strategy<Value = Shape> {
    let pressure =
        prop_oneof![8 => Just(Pressure::Normal), 2 => Just(Pressure::Warning), 2 => Just(Pressure::Critical)];
    let thermal = prop_oneof![
        6 => Just(Thermal::Nominal),
        2 => Just(Thermal::Fair),
        1 => Just(Thermal::Serious),
        1 => Just(Thermal::Critical)
    ];
    let any = (
        prop_oneof![3 => Just(16 * 1024u64), 2 => 0..16 * 1024u64, 1 => 1024..5 * 1024u64],
        pressure,
        prop_oneof![6 => Just(0u64), 1 => 1..1000u64],
        prop_oneof![4 => Just(0u64), 1 => 0..400u64],
        prop_oneof![4 => Just(60u64), 1 => 0..20u64],
        thermal,
        prop::bool::weighted(0.1),
        prop_oneof![5 => Just(100u64), 1 => 0..30u64],
        prop_oneof![9 => Just(0u64), 1 => 0..8000u64],
    )
        .prop_map(
            |(
                available_mib,
                pressure,
                swapped_out,
                swap_grew_mib,
                idle_min,
                thermal,
                on_battery,
                disk_gib,
                late_ms,
            )| {
                Shape {
                    available_mib,
                    pressure,
                    swapped_out,
                    swap_grew_mib,
                    idle_min,
                    thermal,
                    on_battery,
                    disk_gib,
                    late_ms,
                }
            },
        );
    // Mostly a Mac with nothing wrong, or with one thing wrong, so that sixty calm seconds do come together.
    prop_oneof![
        12 => Just(fine()),
        3 => (0..16 * 1024u64).prop_map(|available_mib| Shape { available_mib, ..fine() }),
        2 => any.clone().prop_map(|s| Shape { pressure: s.pressure, swap_grew_mib: s.swap_grew_mib, ..fine() }),
        1 => any,
    ]
}

fn size() -> impl Strategy<Value = Size> {
    prop::sample::select(Size::ALL.to_vec())
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        40 => (prop_oneof![6 => 500..5000u64, 1 => 0..9000u64], shape())
            .prop_map(|(after_ms, reading)| Op::Read { after_ms, reading }),
        2 => (0..9000u64).prop_map(|after_ms| Op::Fail { after_ms }),
        3 => prop_oneof![4 => 0..5000u64, 1 => 0..200_000u64].prop_map(|ms| Op::Idle { ms }),
        1 => (0..900_000u64).prop_map(|ms| Op::Sleep { ms }),
        1 => (1..100_000u64, any::<bool>()).prop_map(|(ms, both)| Op::Back { ms, both }),
        16 => size().prop_map(Op::Claim),
        5 => (0..8usize).prop_map(Op::Confirm),
        5 => (0..8usize).prop_map(Op::Release),
        1 => (0..8usize).prop_map(Op::ReleaseGone),
        4 => (0..8usize, 0..9000u64, 0..40_000u64)
            .prop_map(|(slot, resident_mib, scratch_mib)| Op::Usage { slot, resident_mib, scratch_mib }),
        1 => any::<bool>().prop_map(Op::Hold),
    ]
}

fn settings() -> impl Strategy<Value = Settings> {
    (
        prop::sample::select(vec![8u64, 16, 32]),
        4..16u32,
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(gb, cores, own, one_cpu, while_in_use)| {
            let facts = HostFacts {
                total_bytes: gb * GIB,
                cores,
            };
            let mut settings =
                Settings::for_host(facts, if own { Mode::Workstation } else { Mode::Dedicated });
            if one_cpu {
                settings.vcpus = [1; 4];
            }
            if while_in_use && own {
                settings.in_use.cap_bytes = Settings::cap_while_in_use(settings.total_bytes);
            }
            settings
        })
}

/// The test's own record of one slot.
#[derive(Clone, Debug)]
struct Held {
    id: SlotId,
    size: Size,
    cpus: u32,
    deadline: Option<Millis>,
    usage: Option<Usage>,
    told_to_stop: bool,
}

struct World {
    settings: Settings,
    budget: Budget,
    now: Now,
    /// Every call of `observe`: when, and the reading if there was one.
    seen: Vec<(Now, Option<HostReading>)>,
    held: Vec<Held>,
    gone: Vec<SlotId>,
    swapouts: u64,
    swap_used: u64,
    hold: bool,
    last_stop: Option<Millis>,
}

impl World {
    fn limits(&self, reading: &HostReading) -> Profile {
        let away = self.settings.mode == Mode::Dedicated || reading.idle >= self.settings.idle_after;
        if away {
            self.settings.away
        } else {
            self.settings.in_use
        }
    }

    /// Whether one reading, as handed over at `at`, shows nothing wrong.
    fn clean(&self, at: Now, reading: &HostReading) -> bool {
        reading.at.elapsed <= at.elapsed
            && at.elapsed.0 - reading.at.elapsed.0 <= self.settings.stale_after.0
            && reading.pressure == Pressure::Normal
            && reading.thermal < Thermal::Serious
            && !reading.on_battery
            && reading.disk_free_bytes >= self.settings.disk_hold_bytes
            && reading.available_bytes >= self.limits(reading).floor_bytes
    }

    /// The plan's "open" as far as the readings go: a reading new enough to act on, and before it an unbroken
    /// watch of at least the calm time in which every reading was clean and no page was swapped out.
    fn watched_calm(&self) -> Option<HostReading> {
        let (last_at, last) = self.seen.last()?;
        let last = (*last)?;
        let in_order = self.now.elapsed >= last_at.elapsed && self.now.awake >= last_at.awake;
        if !in_order || self.now.elapsed.0 - last_at.elapsed.0 > self.settings.stale_after.0 {
            return None;
        }
        let mut newer = *last_at;
        for (at, reading) in self.seen.iter().rev() {
            let joined = at.elapsed <= newer.elapsed
                && at.awake <= newer.awake
                && newer.elapsed.0 - at.elapsed.0 <= self.settings.stale_after.0;
            let reading = (*reading)?;
            if !joined || !self.clean(*at, &reading) || reading.swapouts != last.swapouts {
                return None;
            }
            if last_at.elapsed.0 - at.elapsed.0 >= self.settings.calm_for.0 {
                return Some(last);
            }
            newer = *at;
        }
        None
    }

    fn charges(&self) -> Vec<Charge> {
        self.held
            .iter()
            .map(|slot| {
                let host_bytes = slot.size.vm_bytes() + self.settings.vm_host_overhead_bytes;
                Charge {
                    vm_bytes: slot.size.vm_bytes(),
                    host_bytes,
                    resident_bytes: slot.usage.map_or(0, |usage| usage.resident_bytes.min(host_bytes)),
                    cpus: slot.cpus,
                }
            })
            .collect()
    }

    fn committed(&self) -> u64 {
        self.held.iter().map(|slot| slot.size.vm_bytes()).sum()
    }

    /// Everything that must be true the moment a slot is granted.
    fn check_grant(&self, size: Size) -> Result<(), TestCaseError> {
        prop_assert!(!self.hold, "granted under a hold");
        let reading = self.watched_calm();
        prop_assert!(
            reading.is_some(),
            "granted without sixty calm, watched seconds behind a fresh reading"
        );
        let reading = reading.unwrap();
        let limits = self.limits(&reading);
        let vm = size.vm_bytes();
        prop_assert!(self.committed() + vm <= limits.cap_bytes, "over the cap in force");
        prop_assert!(self.committed() + vm <= self.settings.away.cap_bytes);
        let cpus: u32 = self.held.iter().map(|slot| slot.cpus).sum();
        prop_assert!(
            cpus + self.settings.vcpus_of(size) <= limits.cpus,
            "CPUs overcommitted"
        );
        let to_come: u64 = self
            .charges()
            .iter()
            .map(|c| c.host_bytes - c.resident_bytes)
            .sum();
        let needed = to_come + vm + self.settings.vm_host_overhead_bytes + limits.floor_bytes;
        prop_assert!(
            reading.available_bytes >= needed,
            "the floor would not hold if every VM grew"
        );
        Ok(())
    }

    fn step(&mut self, op: &Op) -> Result<(), TestCaseError> {
        match op {
            Op::Read { after_ms, reading } => {
                self.pass(*after_ms);
                self.swapouts += reading.swapped_out;
                self.swap_used += reading.swap_grew_mib * MIB;
                let taken = Millis(self.now.elapsed.0.saturating_sub(reading.late_ms));
                let handed = HostReading {
                    at: Now {
                        awake: self.now.awake,
                        elapsed: taken,
                    },
                    available_bytes: reading.available_mib * MIB,
                    pressure: reading.pressure,
                    swap_used_bytes: self.swap_used,
                    swapouts: self.swapouts,
                    idle: Millis::mins(reading.idle_min),
                    thermal: reading.thermal,
                    on_battery: reading.on_battery,
                    disk_free_bytes: reading.disk_gib * GIB,
                };
                self.observe(Some(handed))?;
            }
            Op::Fail { after_ms } => {
                self.pass(*after_ms);
                self.observe(None)?;
            }
            Op::Idle { ms } => self.pass(*ms),
            Op::Sleep { ms } => self.now.elapsed.0 += ms,
            Op::Back { ms, both } => {
                self.now.elapsed.0 = self.now.elapsed.0.saturating_sub(*ms);
                if *both {
                    self.now.awake.0 = self.now.awake.0.saturating_sub(*ms);
                }
            }
            Op::Claim(size) => {
                let fits = self.budget.fits(self.now, *size);
                let free = self.budget.free(self.now);
                match self.budget.claim(self.now, *size) {
                    Ok(grant) => {
                        self.check_grant(*size)?;
                        prop_assert_eq!(fits, Ok(()));
                        prop_assert!(free.of(*size) >= 1);
                        prop_assert_eq!(self.budget.level(self.now), Level::Open);
                        prop_assert!(self.held.iter().all(|slot| slot.id != grant.slot));
                        prop_assert!(!self.gone.contains(&grant.slot), "a slot's name came back");
                        prop_assert_eq!(grant.vm_bytes, size.vm_bytes());
                        prop_assert_eq!(grant.cpus, self.settings.vcpus_of(*size));
                        let deadline = Millis(self.now.elapsed.0 + self.settings.reservation_ttl.0);
                        prop_assert_eq!(grant.expires, Some(deadline));
                        self.held.push(Held {
                            id: grant.slot,
                            size: *size,
                            cpus: grant.cpus,
                            deadline: Some(deadline),
                            usage: None,
                            told_to_stop: false,
                        });
                    }
                    Err(refusal) => {
                        prop_assert_eq!(fits, Err(refusal));
                        prop_assert_eq!(free.of(*size), 0, "said free, then refused: {:?}", refusal);
                    }
                }
            }
            Op::Confirm(index) => {
                if let Some(slot) = self.held.get_mut(*index) {
                    prop_assert_eq!(self.budget.confirm(slot.id), Ok(()));
                    slot.deadline = None;
                }
            }
            Op::Release(index) => {
                if *index < self.held.len() {
                    let slot = self.held.remove(*index);
                    let released = self.budget.release(slot.id);
                    prop_assert_eq!(
                        released.map(|r| (r.size, r.vm_bytes, r.cpus)),
                        Ok((slot.size, slot.size.vm_bytes(), slot.cpus))
                    );
                    self.gone.push(slot.id);
                }
            }
            Op::ReleaseGone(index) => {
                if let Some(id) = self.gone.get(*index) {
                    prop_assert!(self.budget.release(*id).is_err(), "memory given back twice");
                    prop_assert!(self.budget.confirm(*id).is_err());
                }
            }
            Op::Usage {
                slot,
                resident_mib,
                scratch_mib,
            } => {
                if let Some(slot) = self.held.get_mut(*slot) {
                    let usage = Usage {
                        resident_bytes: resident_mib * MIB,
                        scratch_bytes: scratch_mib * MIB,
                    };
                    prop_assert_eq!(self.budget.usage(slot.id, usage), Ok(()));
                    slot.usage = Some(usage);
                }
            }
            Op::Hold(on) => {
                self.budget.hold(Hold::Paused, *on);
                self.hold = *on;
            }
        }
        self.check_always()
    }

    fn pass(&mut self, ms: u64) {
        self.now.elapsed.0 += ms;
        self.now.awake.0 += ms;
    }

    fn observe(&mut self, reading: Option<HostReading>) -> Result<(), TestCaseError> {
        let went_back = self
            .seen
            .last()
            .is_some_and(|(before, _)| self.now.elapsed < before.elapsed);
        if went_back {
            // A clock that went back gives every waiting reservation its full time again, and the pause between
            // stopped jobs starts over.
            let deadline = Millis(self.now.elapsed.0 + self.settings.reservation_ttl.0);
            for slot in &mut self.held {
                slot.deadline = slot.deadline.map(|_| deadline);
            }
            self.last_stop = self.last_stop.map(|_| self.now.elapsed);
        }
        let held_bytes: u64 = self
            .charges()
            .iter()
            .zip(&self.held)
            .filter(|(_, slot)| slot.deadline.is_none())
            .map(|(charge, slot)| {
                if slot.usage.is_some() {
                    charge.resident_bytes
                } else {
                    charge.host_bytes
                }
            })
            .sum();
        let observed = self.budget.observe(self.now, reading);
        self.seen.push((self.now, reading));

        let mut due: Vec<SlotId> = self
            .held
            .iter()
            .filter(|s| s.deadline.is_some_and(|d| self.now.elapsed >= d))
            .map(|s| s.id)
            .collect();
        due.sort();
        prop_assert_eq!(&observed.expired, &due, "the reservations that ran out");
        self.gone.extend(&due);
        self.held.retain(|slot| !due.contains(&slot.id));

        prop_assert_eq!(observed.level, self.budget.level(self.now));
        if observed.level == Level::Open {
            prop_assert!(
                self.watched_calm().is_some() && !self.hold,
                "open without sixty calm seconds"
            );
        }
        if let Some(eviction) = observed.evict {
            let reading = self
                .seen
                .last()
                .and_then(|(at, reading)| reading.filter(|r| self.fresh(*at, r)));
            prop_assert!(reading.is_some(), "a job stopped without a reading");
            let reading = reading.unwrap();
            let victim = self.held.iter().find(|slot| slot.id == eviction.slot);
            prop_assert!(victim.is_some(), "a slot that is not held was told to stop");
            let victim = victim.unwrap();
            prop_assert!(victim.deadline.is_none(), "a reservation has no job to stop");
            prop_assert!(!victim.told_to_stop, "the same job told twice");
            if let Some(last) = self.last_stop {
                prop_assert!(
                    self.now.elapsed.0 - last.0 >= self.settings.evict_every.0,
                    "two jobs within 30 s"
                );
            }
            let running = || {
                self.held
                    .iter()
                    .filter(|slot| slot.deadline.is_none() && !slot.told_to_stop)
            };
            match eviction.cause {
                EvictionCause::Memory => {
                    prop_assert!(
                        reading.pressure >= Pressure::Warning,
                        "stopped for memory without pressure"
                    );
                    prop_assert!(
                        held_bytes >= self.settings.evict_min_held_bytes,
                        "stopped while CI held nothing"
                    );
                    prop_assert_eq!(
                        Some(victim.id),
                        running().map(|slot| slot.id).max(),
                        "not the newest job"
                    );
                }
                EvictionCause::Disk => {
                    prop_assert!(reading.disk_free_bytes < self.settings.disk_evict_bytes);
                    let scratch = |slot: &Held| slot.usage.map_or(0, |usage| usage.scratch_bytes);
                    prop_assert!(scratch(victim) > 0);
                    prop_assert_eq!(
                        Some(scratch(victim)),
                        running().map(scratch).max(),
                        "not the largest scratch"
                    );
                }
            }
            self.last_stop = Some(self.now.elapsed);
            self.held
                .iter_mut()
                .find(|slot| slot.id == eviction.slot)
                .unwrap()
                .told_to_stop = true;
        }
        Ok(())
    }

    fn fresh(&self, at: Now, reading: &HostReading) -> bool {
        reading.at.elapsed <= at.elapsed && at.elapsed.0 - reading.at.elapsed.0 <= self.settings.stale_after.0
    }

    /// What holds after every step, whatever it was.
    fn check_always(&self) -> Result<(), TestCaseError> {
        prop_assert_eq!(
            self.budget.committed_bytes(),
            self.committed(),
            "the budget counts what is held"
        );
        prop_assert_eq!(
            self.budget.cpus_committed(),
            self.held.iter().map(|slot| slot.cpus).sum::<u32>()
        );
        prop_assert_eq!(self.budget.slots(), self.held.len());
        prop_assert!(
            self.committed() <= self.settings.away.cap_bytes,
            "more promised than the cap"
        );
        prop_assert!(self.committed() + self.settings.away.floor_bytes <= self.settings.total_bytes);
        prop_assert!(
            self.budget.cpus_committed() <= self.settings.away.cpus,
            "more vCPUs than the CPU cap"
        );
        let free = self.budget.free(self.now);
        for size in Size::ALL {
            prop_assert_eq!(free.of(size) > 0, self.budget.fits(self.now, size).is_ok());
        }
        if self.budget.level(self.now) != Level::Open {
            prop_assert_eq!(free, Default::default(), "slots offered while not open");
        }
        Ok(())
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 400, ..ProptestConfig::default() })]

    #[test]
    fn nothing_is_promised_that_the_rules_do_not_allow(settings in settings(), ops in prop::collection::vec(op(), 1..400)) {
        let mut world = World {
            budget: Budget::new(settings.clone()).unwrap(),
            settings,
            now: ms(1_000_000),
            seen: Vec::new(),
            held: Vec::new(),
            gone: Vec::new(),
            swapouts: 0,
            swap_used: 0,
            hold: false,
            last_stop: None,
        };
        let mut granted = 0;
        for op in &ops {
            let before = world.held.len();
            world.step(op)?;
            granted += usize::from(matches!(op, Op::Claim(_)) && world.held.len() > before);
        }
        // Everything is given back, once, and then nothing is promised.
        for slot in std::mem::take(&mut world.held) {
            prop_assert!(world.budget.release(slot.id).is_ok());
            prop_assert!(world.budget.release(slot.id).is_err());
        }
        prop_assert_eq!((world.budget.committed_bytes(), world.budget.cpus_committed(), world.budget.slots()), (0, 0, 0));
        // Not a rule, but a guard for this test: it must not pass because nothing was ever granted.
        prop_assume!(granted > 0 || ops.len() < 150);
    }

    /// More memory, fewer VMs, more of them already grown or looser limits never turn a yes into a no; the other
    /// way round a no never becomes a yes.
    #[test]
    fn admission_is_monotone(
        settings in settings(),
        available_mib in 0..40_000u64,
        less_mib in 0..40_000u64,
        charges in prop::collection::vec((size(), 0..9000u64), 0..6),
        extra in prop::collection::vec((size(), 0..9000u64), 0..3),
        grew_less_mib in 0..9000u64,
        tighter in (0..9000u64, 0..9000u64, 0..8u32),
        wanted in size(),
    ) {
        let charge = |(size, resident_mib): (Size, u64)| Charge {
            vm_bytes: size.vm_bytes(),
            host_bytes: settings.host_bytes(size),
            resident_bytes: (resident_mib * MIB).min(settings.host_bytes(size)),
            cpus: settings.vcpus_of(size),
        };
        let better: Vec<Charge> = charges.iter().copied().map(charge).collect();
        let limits = settings.away;
        let available = available_mib * MIB;
        let good = admit(&settings, &limits, available, &better, wanted);

        let mut worse: Vec<Charge> = better.clone();
        for charge in &mut worse {
            charge.resident_bytes = charge.resident_bytes.saturating_sub(grew_less_mib * MIB);
        }
        worse.extend(extra.iter().copied().map(charge));
        let worse_limits = Profile {
            cap_bytes: limits.cap_bytes.saturating_sub(tighter.0 * MIB),
            floor_bytes: limits.floor_bytes + tighter.1 * MIB,
            cpus: limits.cpus.saturating_sub(tighter.2),
        };
        let bad = admit(&settings, &worse_limits, available.saturating_sub(less_mib * MIB), &worse, wanted);
        prop_assert!(good.is_ok() || bad.is_err(), "worse on every count, and it fits now: {:?} then {:?}", good, bad);
        // A larger VM never fits where a smaller one does not.
        for larger in Size::ALL.into_iter().filter(|s| *s > wanted) {
            if settings.vcpus_of(larger) >= settings.vcpus_of(wanted) {
                prop_assert!(good.is_ok() || admit(&settings, &limits, available, &better, larger).is_err());
            }
        }
    }

    /// Two Macs with the same past: the one whose newest reading is worse in any way offers no more slots.
    #[test]
    fn slots_never_rise_when_the_host_gets_worse(
        settings in settings(),
        held in prop::collection::vec(size(), 0..3),
        base in shape(),
        less_mib in 0..16_000u64,
        pressure_up in any::<bool>(),
        heat_up in any::<bool>(),
        person_back in any::<bool>(),
        swapped in any::<bool>(),
    ) {
        let mut better = open(settings.clone());
        for size in held {
            let _ = better.claim(secs(60), size);
        }
        let mut worse = better.clone();
        let reading = |shape: &Shape| HostReading {
            at: secs(61),
            available_bytes: shape.available_mib * MIB,
            pressure: shape.pressure,
            swap_used_bytes: 0,
            swapouts: 100 + shape.swapped_out,
            idle: Millis::mins(shape.idle_min),
            thermal: shape.thermal,
            on_battery: shape.on_battery,
            disk_free_bytes: shape.disk_gib * GIB,
        };
        let mut bad = base.clone();
        bad.available_mib = bad.available_mib.saturating_sub(less_mib);
        if pressure_up { bad.pressure = Pressure::Critical; }
        if heat_up { bad.thermal = Thermal::Critical; }
        if person_back { bad.idle_min = 0; }
        if swapped { bad.swapped_out += 1; }
        better.observe(secs(61), Some(reading(&base)));
        worse.observe(secs(61), Some(reading(&bad)));
        let (more, fewer) = (better.free(secs(61)), worse.free(secs(61)));
        for size in Size::ALL {
            prop_assert!(fewer.of(size) <= more.of(size), "{:?}: {:?} against {:?}", size, fewer, more);
        }
        prop_assert!(worse.level(secs(61)) != Level::Open || better.level(secs(61)) == Level::Open);
    }

    /// Every setting the budget accepts keeps the cap and the floor inside the Mac, and what it then grants too.
    #[test]
    fn every_accepted_setting_gives_a_sane_budget(
        total_gib in 1..65u64,
        cores in 0..33u32,
        caps_mib in (0..70_000u64, 0..70_000u64),
        floors_mib in (0..70_000u64, 0..70_000u64),
        cpus in (0..40u32, 0..40u32),
        vcpus in prop::array::uniform4(0..9u32),
        overhead_mib in 0..3000u64,
        claims in prop::collection::vec(size(), 0..40),
    ) {
        let mut settings = Settings::for_host(HostFacts { total_bytes: 16 * GIB, cores: 8 }, Mode::Workstation);
        settings.total_bytes = total_gib * GIB;
        settings.cores = cores;
        settings.away = Profile { cap_bytes: caps_mib.0 * MIB, floor_bytes: floors_mib.0 * MIB, cpus: cpus.0 };
        settings.in_use = Profile { cap_bytes: caps_mib.1 * MIB, floor_bytes: floors_mib.1 * MIB, cpus: cpus.1 };
        settings.vcpus = vcpus;
        settings.vm_host_overhead_bytes = overhead_mib * MIB;
        let Ok(mut budget) = Budget::new(settings.clone()) else { return Ok(()) };
        prop_assert!(settings.away.cap_bytes + settings.away.floor_bytes <= settings.total_bytes);
        prop_assert!(settings.in_use.cap_bytes + settings.in_use.floor_bytes <= settings.total_bytes);
        prop_assert!(settings.away.floor_bytes >= GIB && settings.away.cpus <= settings.cores);
        // The Mac reports all its memory as available: the most generous reading there can be.
        for second in 0..=60 {
            let mut reading = calm(secs(second));
            reading.available_bytes = settings.total_bytes;
            budget.observe(secs(second), Some(reading));
        }
        for size in claims {
            if let Ok(grant) = budget.claim(secs(60), size) {
                prop_assert!(grant.cpus >= 1 && grant.job_limit_bytes > 0 && grant.job_limit_bytes < grant.vm_bytes + 1);
            }
            prop_assert!(budget.committed_bytes() <= settings.away.cap_bytes);
            prop_assert!(budget.committed_bytes() + settings.away.floor_bytes <= settings.total_bytes);
            prop_assert!(budget.cpus_committed() <= settings.cores);
        }
    }
}
