//! Jobs GitHub assigned without asking: a poll with spare capacity can be handed one while the Mac is closed. Such
//! a job is never dropped and never started against what protects the Mac; it waits, first in line.

mod common;

use std::sync::{Arc, Mutex};
use std::thread;

use cf_budget::{
    Assigned, Budget, Free, GIB, Grant, Hold, HostFacts, HostReading, Level, MIB, Millis, Mode, Pressure,
    Refusal, Settings, Size, SlotId, Trouble, UnknownSlot,
};
use common::*;

const OVERHEAD: u64 = 64 * MIB;

/// One reading a second from `from` to `to`, each changed by `change`; the jobs that started, with the second.
fn run(budget: &mut Budget, from: u64, to: u64, change: impl Fn(&mut HostReading)) -> Vec<(u64, Grant)> {
    let mut started = Vec::new();
    for second in from..=to {
        let mut reading = calm(secs(second));
        change(&mut reading);
        let observed = budget.observe(secs(second), Some(reading));
        started.extend(observed.started.into_iter().map(|grant| (second, grant)));
    }
    started
}

fn waits(assigned: Assigned) -> (SlotId, Refusal) {
    match assigned {
        Assigned::Waits { slot, why } => (slot, why),
        other => panic!("expected a job that waits, got {other:?}"),
    }
}

fn starts(assigned: Assigned) -> Grant {
    match assigned {
        Assigned::Start(grant) => grant,
        other => panic!("expected a job that starts, got {other:?}"),
    }
}

#[test]
fn on_an_open_mac_an_assigned_job_starts_at_once_and_its_slot_never_runs_out() {
    let mut budget = open(dedicated());
    let grant = starts(budget.assign(secs(60), Size::G4));
    assert_eq!(
        (
            grant.size,
            grant.vm_bytes,
            grant.job_limit_bytes,
            grant.cpus,
            grant.expires
        ),
        (Size::G4, 4 * GIB, 4 * GIB - GIB / 4, 4, None)
    );
    assert_eq!(
        (
            budget.committed_bytes(),
            budget.cpus_committed(),
            budget.slots(),
            budget.waiting()
        ),
        (4 * GIB, 4, 1, 0)
    );
    // A reservation would be gone after two minutes; this job is here.
    let observed = budget.observe(secs(61), Some(calm(secs(61))));
    assert_eq!(observed.started, []);
    for second in 62..=600 {
        assert_eq!(budget.observe(secs(second), Some(calm(secs(second)))).expired, []);
    }
    assert_eq!(budget.slots(), 1);
    assert_eq!(
        budget.release(grant.slot).map(|released| released.vm_bytes),
        Ok(4 * GIB)
    );
    assert_eq!(budget.committed_bytes(), 0);
}

#[test]
fn under_pressure_it_waits_holds_nothing_and_starts_sixty_seconds_after_the_pressure() {
    let mut budget = open(dedicated());
    assert_eq!(run(&mut budget, 61, 61, |r| r.pressure = Pressure::Warning), []);
    let (slot, why) = waits(budget.assign(secs(61), Size::G4));
    assert_eq!(why, Refusal::Trouble(Trouble::Pressure));
    assert_eq!(
        (
            budget.committed_bytes(),
            budget.cpus_committed(),
            budget.slots(),
            budget.waiting()
        ),
        (0, 0, 0, 1)
    );

    // As long as the pressure lasts, and for the calm time after it, nothing starts.
    assert_eq!(run(&mut budget, 62, 300, |r| r.pressure = Pressure::Warning), []);
    assert_eq!(run(&mut budget, 301, 360, |_| {}), []);
    assert_eq!(budget.waiting(), 1);
    let started = run(&mut budget, 361, 361, |_| {});
    assert_eq!(started.len(), 1);
    let (second, grant) = started[0];
    assert_eq!(
        (second, grant.slot, grant.size, grant.expires),
        (361, slot, Size::G4, None)
    );
    assert_eq!(
        (budget.committed_bytes(), budget.slots(), budget.waiting()),
        (4 * GIB, 1, 0)
    );
    // It starts once.
    assert_eq!(run(&mut budget, 362, 500, |_| {}), []);
}

#[test]
fn the_person_being_there_does_not_keep_it_waiting() {
    let mut budget = open(workstation());
    run(&mut budget, 61, 61, |r| r.idle = Millis::ZERO);
    assert_eq!(budget.claim(secs(61), Size::G4), Err(Refusal::PersonThere));
    let grant = starts(budget.assign(secs(61), Size::G4));
    assert_eq!((grant.size, grant.expires), (Size::G4, None));
    // The daemon runs it at low priority, as it does a job that was running when the person came back.
    let status = budget.status(secs(61));
    assert!(status.person_there);
    assert_eq!((status.committed_bytes, status.waiting), (4 * GIB, 0));
    // The Mac still takes nothing by itself.
    assert_eq!(budget.claim(secs(61), Size::G2), Err(Refusal::PersonThere));
    assert_eq!(budget.free(secs(61)), Free::default());
}

#[test]
fn while_the_person_is_there_it_needs_the_higher_floor() {
    // A workstation of 16 GB keeps 4 GB while its person is there and 3 GB while they are away.
    let with_person = 4 * GIB + OVERHEAD + 4 * GIB;
    let mut budget = open(workstation());
    let person =
        |available: u64| move |r: &mut HostReading| (r.idle, r.available_bytes) = (Millis::ZERO, available);

    run(&mut budget, 61, 61, person(with_person - 1));
    let (slot, why) = waits(budget.assign(secs(61), Size::G4));
    assert_eq!(why, Refusal::Memory { short_bytes: 1 });
    assert_eq!(run(&mut budget, 62, 70, person(with_person - 1)), []);
    let started = run(&mut budget, 71, 71, person(with_person));
    assert_eq!(
        started
            .iter()
            .map(|(second, grant)| (*second, grant.slot))
            .collect::<Vec<_>>(),
        [(71, slot)]
    );

    // The same shortage ends when the person leaves: a gigabyte less is enough then.
    let mut budget = open(workstation());
    run(&mut budget, 61, 61, person(with_person - GIB));
    let (slot, why) = waits(budget.assign(secs(61), Size::G4));
    assert_eq!(why, Refusal::Memory { short_bytes: GIB });
    assert_eq!(run(&mut budget, 62, 70, person(with_person - GIB)), []);
    let away = |r: &mut HostReading| (r.idle, r.available_bytes) = (Millis::mins(15), with_person - GIB);
    let started = run(&mut budget, 71, 71, away);
    assert_eq!(
        started.iter().map(|(_, grant)| grant.slot).collect::<Vec<_>>(),
        [slot]
    );
    // One byte less than that and it would still wait.
    let mut budget = open(workstation());
    run(&mut budget, 61, 61, |r| {
        (r.idle, r.available_bytes) = (Millis::mins(15), with_person - GIB - 1)
    });
    assert_eq!(
        waits(budget.assign(secs(61), Size::G4)).1,
        Refusal::Memory { short_bytes: 1 }
    );
}

#[test]
fn it_counts_against_the_cap_and_the_cpus_the_mac_has_without_its_person() {
    // 9.6 GB and 6 CPUs on a 16 GB workstation with 8 cores, also while the person is there.
    let settings = workstation();
    let cap = settings.away.cap_bytes;
    let person = |r: &mut HostReading| r.idle = Millis::ZERO;
    let mut budget = open(settings.clone());
    run(&mut budget, 61, 61, person);
    let big = starts(budget.assign(secs(61), Size::G8));
    let (small, why) = waits(budget.assign(secs(61), Size::G2));
    assert_eq!(
        why,
        Refusal::Full {
            cap_bytes: cap,
            committed_bytes: 8 * GIB
        }
    );
    assert_eq!(run(&mut budget, 62, 200, person), []);
    budget.release(big.slot).unwrap();
    let started = run(&mut budget, 201, 201, person);
    assert_eq!(
        started
            .iter()
            .map(|(_, grant)| (grant.slot, grant.size))
            .collect::<Vec<_>>(),
        [(small, Size::G2)]
    );

    // Two VMs of four vCPUs are more than six.
    let mut budget = open(settings);
    run(&mut budget, 61, 61, person);
    let first = starts(budget.assign(secs(61), Size::G4));
    let (second, why) = waits(budget.assign(secs(61), Size::G4));
    assert_eq!(why, Refusal::Cpus { cap: 6, committed: 4 });
    assert_eq!(run(&mut budget, 62, 100, person), []);
    budget.release(first.slot).unwrap();
    assert_eq!(run(&mut budget, 101, 101, person)[0].1.slot, second);
}

#[test]
fn jobs_that_wait_start_in_the_order_they_came() {
    let mut budget = open(dedicated());
    // 5 GB available: a VM of 2 GB would fit (2 + 2 GB floor), one of 8 GB not.
    run(&mut budget, 61, 61, |r| r.available_bytes = 5 * GIB);
    let (big, why) = waits(budget.assign(secs(61), Size::G8));
    assert_eq!(
        why,
        Refusal::Memory {
            short_bytes: 8 * GIB + OVERHEAD + 2 * GIB - 5 * GIB
        }
    );
    let (small, why) = waits(budget.assign(secs(61), Size::G2));
    assert_eq!(why, Refusal::AssignedFirst { waiting: 1 });
    let (third, why) = waits(budget.assign(secs(61), Size::G2));
    assert_eq!(why, Refusal::AssignedFirst { waiting: 2 });
    assert!(big < small && small < third);

    // Room for the small ones is not room for the one in front of them.
    assert_eq!(run(&mut budget, 62, 100, |r| r.available_bytes = 9 * GIB), []);
    assert_eq!(budget.waiting(), 3);
    // Room for the big one and one small one: 8 + 2 GB of VMs, their overheads and the floor.
    let enough = 10 * GIB + 2 * OVERHEAD + 2 * GIB;
    let started = run(&mut budget, 101, 101, |r| r.available_bytes = enough);
    assert_eq!(
        started.iter().map(|(_, grant)| grant.slot).collect::<Vec<_>>(),
        [big, small]
    );
    // The third one waits for memory of its own, and gets it a second later.
    assert_eq!((budget.waiting(), budget.committed_bytes()), (1, 10 * GIB));
    let started = run(&mut budget, 102, 102, |r| {
        r.available_bytes = enough + 2 * GIB + OVERHEAD - 1
    });
    assert_eq!(started, []);
    let started = run(&mut budget, 103, 103, |r| {
        r.available_bytes = enough + 2 * GIB + OVERHEAD
    });
    assert_eq!(
        started.iter().map(|(_, grant)| grant.slot).collect::<Vec<_>>(),
        [third]
    );
    assert_eq!((budget.waiting(), budget.committed_bytes()), (0, 12 * GIB));
}

#[test]
fn while_a_job_waits_nothing_else_is_granted_or_offered() {
    let mut budget = open(dedicated());
    run(&mut budget, 61, 61, |r| r.available_bytes = 5 * GIB);
    assert!(budget.fits(secs(61), Size::G2).is_ok());
    let (slot, _) = waits(budget.assign(secs(61), Size::G8));

    let refusal = Refusal::AssignedFirst { waiting: 1 };
    assert_eq!(refusal.name(), "assigned-first");
    assert_eq!(budget.claim(secs(61), Size::G2), Err(refusal));
    assert_eq!(budget.fits(secs(61), Size::G2), Err(refusal));
    assert_eq!(budget.free(secs(61)), Free::default());
    let status = budget.status(secs(61));
    assert_eq!(
        (status.level, status.closed_by, status.waiting),
        (Level::Tight, Some(refusal), 1)
    );
    assert_eq!(budget.slots(), 0);

    // Once it runs, the Mac takes jobs again.
    let started = run(&mut budget, 62, 62, |_| {});
    assert_eq!(started[0].1.slot, slot);
    assert_eq!(budget.level(secs(62)), Level::Open);
    assert!(budget.claim(secs(62), Size::G2).is_ok());
}

#[test]
fn a_hold_keeps_it_waiting_and_lifting_the_hold_starts_it() {
    for hold in [
        Hold::Paused,
        Hold::QuietHour,
        Hold::StartFailures,
        Hold::NoImage,
        Hold::Draining,
    ] {
        let mut budget = open(dedicated());
        budget.hold(hold, true);
        let (slot, why) = waits(budget.assign(secs(60), Size::G4));
        assert_eq!(why, Refusal::Held(hold));
        assert_eq!(run(&mut budget, 61, 400, |_| {}), [], "{hold:?}");
        budget.hold(hold, false);
        // A hold is no reading: there is no calm time after it.
        let started = run(&mut budget, 401, 401, |_| {});
        assert_eq!(
            started.iter().map(|(_, grant)| grant.slot).collect::<Vec<_>>(),
            [slot],
            "{hold:?}"
        );
    }
}

#[test]
fn everything_else_that_closes_the_mac_keeps_it_waiting() {
    type Change = fn(&mut HostReading);
    let cases: [(Change, Trouble); 5] = [
        (|r| r.pressure = Pressure::Critical, Trouble::Pressure),
        (|r| r.thermal = cf_budget::Thermal::Serious, Trouble::Hot),
        (|r| r.on_battery = true, Trouble::Battery),
        (|r| r.disk_free_bytes = 25 * GIB - 1, Trouble::DiskLow),
        (|r| r.available_bytes = 2 * GIB - 1, Trouble::Short),
    ];
    for (change, trouble) in cases {
        let mut budget = open(dedicated());
        run(&mut budget, 61, 61, change);
        let (slot, why) = waits(budget.assign(secs(61), Size::G2));
        assert_eq!(why, Refusal::Trouble(trouble));
        assert_eq!(run(&mut budget, 62, 90, change), [], "{trouble:?}");
        let started = run(&mut budget, 91, 151, |_| {});
        assert_eq!(
            started
                .iter()
                .map(|(second, grant)| (*second, grant.slot))
                .collect::<Vec<_>>(),
            [(151, slot)],
            "{trouble:?}: sixty seconds after the last bad reading"
        );
    }
}

#[test]
fn without_a_reading_it_waits_and_a_sleep_does_not_lose_it() {
    let mut budget = Budget::new(dedicated()).unwrap();
    let (first, why) = waits(budget.assign(secs(0), Size::G4));
    assert_eq!(why, Refusal::Blind);
    // Readings that fail are no reason to start, however many.
    for second in 1..=100 {
        assert_eq!(budget.observe(secs(second), None).started, []);
    }
    assert_eq!(run(&mut budget, 101, 160, |_| {}), []);
    let started = run(&mut budget, 161, 161, |_| {});
    assert_eq!(started[0].1.slot, first);

    // The Mac sleeps for 40 minutes with a job waiting: it is still there, and starts after the calm time.
    run(&mut budget, 162, 162, |r| r.on_battery = true);
    let (second, _) = waits(budget.assign(secs(162), Size::G2));
    let woke = 162 + 40 * 60;
    assert_eq!(run(&mut budget, woke, woke + 59, |_| {}), []);
    assert_eq!(budget.waiting(), 1);
    assert_eq!(run(&mut budget, woke + 60, woke + 60, |_| {})[0].1.slot, second);
    // A clock that went back does not lose it either.
    run(&mut budget, woke + 61, woke + 61, |r| {
        r.pressure = Pressure::Warning
    });
    let (third, _) = waits(budget.assign(secs(woke + 61), Size::G2));
    assert_eq!(run(&mut budget, 10, 69, |_| {}), []);
    assert_eq!(run(&mut budget, 70, 70, |_| {})[0].1.slot, third);
}

#[test]
fn a_size_the_mac_can_never_run_does_not_wait() {
    // 8 GB, dedicated: a cap of 5 GB.
    let facts = HostFacts {
        total_bytes: 8 * GIB,
        cores: 8,
    };
    let settings = Settings::for_host(facts, Mode::Dedicated);
    assert_eq!(settings.can_run(Size::G2), Ok(()));
    assert_eq!(settings.can_run(Size::G4), Ok(()));
    let full = Refusal::Full {
        cap_bytes: 5 * GIB,
        committed_bytes: 0,
    };
    assert_eq!(settings.can_run(Size::G6), Err(full));
    assert_eq!(settings.can_run(Size::G8), Err(full));

    let mut budget = Budget::new(settings.clone()).unwrap();
    watch(&mut budget, 0, 60, |r| r.available_bytes = 8 * GIB);
    assert_eq!(budget.assign(secs(60), Size::G8), Assigned::Never(full));
    assert_eq!((budget.waiting(), budget.slots()), (0, 0));
    // Nothing is held up by it.
    assert!(budget.claim(secs(60), Size::G2).is_ok());

    // Four vCPUs on a Mac that gives CI two.
    let two_cores = Settings::for_host(
        HostFacts {
            total_bytes: 16 * GIB,
            cores: 4,
        },
        Mode::Dedicated,
    );
    assert_eq!(two_cores.can_run(Size::G2), Ok(()));
    assert_eq!(
        two_cores.can_run(Size::G4),
        Err(Refusal::Cpus { cap: 2, committed: 0 })
    );
    // A cap that fits a size the floor does not: 4 GB of VM, its overhead and the floor are more than the Mac.
    let mut tight = settings;
    tight.away.floor_bytes = 4 * GIB;
    tight.in_use.floor_bytes = 4 * GIB;
    tight.away.cap_bytes = 4 * GIB;
    tight.in_use.cap_bytes = 4 * GIB;
    assert_eq!(tight.check(), Ok(()));
    assert_eq!(
        tight.can_run(Size::G4),
        Err(Refusal::Memory {
            short_bytes: OVERHEAD
        })
    );
    assert_eq!(tight.can_run(Size::G2), Ok(()));
}

#[test]
fn a_waiting_job_the_settings_made_impossible_keeps_its_place_and_holds_nobody_up() {
    let mut budget = open(dedicated());
    run(&mut budget, 61, 61, |r| r.available_bytes = 5 * GIB);
    let (big, _) = waits(budget.assign(secs(61), Size::G8));
    let (small, _) = waits(budget.assign(secs(61), Size::G2));

    // The owner lowers the cap to 6 GB: the big one cannot run here any more.
    let mut lower = dedicated();
    lower.away.cap_bytes = 6 * GIB;
    lower.in_use.cap_bytes = 6 * GIB;
    budget.set_settings(lower).unwrap();
    let started = run(&mut budget, 62, 62, |_| {});
    assert_eq!(
        started.iter().map(|(_, grant)| grant.slot).collect::<Vec<_>>(),
        [small]
    );
    assert_eq!(budget.waiting(), 1);
    assert!(
        budget.claim(secs(62), Size::G2).is_ok(),
        "only a job that can run goes first"
    );
    // A new one of that size is told so at once.
    assert!(matches!(
        budget.assign(secs(62), Size::G8),
        Assigned::Never(Refusal::Full { .. })
    ));

    // The cap comes back, and with it the job's turn: first in line again.
    budget.set_settings(dedicated()).unwrap();
    assert_eq!(
        budget.claim(secs(62), Size::G2),
        Err(Refusal::AssignedFirst { waiting: 1 })
    );
    assert_eq!(run(&mut budget, 63, 63, |_| {})[0].1.slot, big);
    assert_eq!((budget.waiting(), budget.committed_bytes()), (0, 12 * GIB));
}

#[test]
fn a_job_github_took_back_stops_waiting_and_the_next_one_moves_up() {
    let mut budget = open(dedicated());
    run(&mut budget, 61, 61, |r| r.available_bytes = 5 * GIB);
    let (big, _) = waits(budget.assign(secs(61), Size::G8));
    let (small, _) = waits(budget.assign(secs(61), Size::G2));

    // It holds nothing, so there is nothing to release.
    assert_eq!(budget.release(big), Err(UnknownSlot(big)));
    assert_eq!(budget.confirm(big), Err(UnknownSlot(big)));
    assert_eq!(budget.waiting(), 2);
    assert_eq!(budget.withdraw(big), Ok(Size::G8));
    assert_eq!(budget.withdraw(big), Err(UnknownSlot(big)));
    assert_eq!(budget.withdraw(SlotId(999)), Err(UnknownSlot(SlotId(999))));
    assert_eq!(budget.waiting(), 1);

    let started = run(&mut budget, 62, 62, |r| r.available_bytes = 5 * GIB);
    assert_eq!(
        started.iter().map(|(_, grant)| grant.slot).collect::<Vec<_>>(),
        [small]
    );
    // A job that started is not withdrawn: it is released.
    assert_eq!(budget.withdraw(small), Err(UnknownSlot(small)));
    assert_eq!(budget.slots(), 1);
    assert!(budget.release(small).is_ok());
    // The name of a job that was withdrawn is not given to another.
    let next = starts(budget.assign(secs(62), Size::G2));
    assert!(next.slot > small);
}

#[test]
fn an_assigned_job_is_stopped_for_the_mac_like_any_other() {
    let mut budget = open(dedicated());
    let older = budget.claim(secs(60), Size::G2).unwrap().slot;
    budget.confirm(older).unwrap();
    let assigned = starts(budget.assign(secs(60), Size::G2)).slot;
    // Critical for 30 s on a dedicated Mac: the newest job goes.
    let mut evicted = Vec::new();
    for second in 61..=91 {
        let mut reading = calm(secs(second));
        reading.pressure = Pressure::Critical;
        evicted.extend(
            budget
                .observe(secs(second), Some(reading))
                .evict
                .map(|eviction| (second, eviction.slot)),
        );
    }
    assert_eq!(evicted, [(91, assigned)]);
}

#[test]
fn forty_assigned_jobs_all_run_two_at_a_time_in_order_and_none_is_lost() {
    // 16 GB, 10 cores, dedicated: 8 CPUs for CI, so two VMs of four.
    let mut budget = open(dedicated());
    let mut order = Vec::new();
    let mut running: Vec<SlotId> = Vec::new();
    let mut names = Vec::new();
    for _ in 0..40 {
        match budget.assign(secs(60), Size::G4) {
            Assigned::Start(grant) => {
                names.push(grant.slot);
                order.push(grant.slot);
                running.push(grant.slot);
            }
            Assigned::Waits { slot, .. } => names.push(slot),
            Assigned::Never(why) => panic!("{why:?}"),
        }
    }
    assert_eq!((running.len(), budget.waiting()), (2, 38));
    for second in 61..=200 {
        // One job ends each second; whoever is next takes its place.
        if !running.is_empty() {
            budget.release(running.remove(0)).unwrap();
        }
        let observed = budget.observe(secs(second), Some(calm(secs(second))));
        for grant in observed.started {
            order.push(grant.slot);
            running.push(grant.slot);
        }
        assert!(running.len() <= 2 && budget.committed_bytes() <= 8 * GIB && budget.cpus_committed() <= 8);
    }
    assert_eq!(order, names, "every job ran, once, in the order it was assigned");
    assert_eq!((budget.waiting(), budget.slots()), (0, 0));
}

#[test]
fn forty_jobs_assigned_at_once_from_many_threads_start_two_and_keep_thirty_eight() {
    let budget = Arc::new(Mutex::new(open(dedicated())));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let budget = Arc::clone(&budget);
            thread::spawn(move || {
                (0..5)
                    .map(|_| budget.lock().unwrap().assign(secs(60), Size::G4))
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let all: Vec<Assigned> = handles
        .into_iter()
        .flat_map(|handle| handle.join().unwrap())
        .collect();
    let started = all
        .iter()
        .filter(|assigned| matches!(assigned, Assigned::Start(_)))
        .count();
    let mut names: Vec<SlotId> = all
        .iter()
        .map(|assigned| match assigned {
            Assigned::Start(grant) => grant.slot,
            Assigned::Waits { slot, .. } => *slot,
            Assigned::Never(why) => panic!("{why:?}"),
        })
        .collect();
    names.sort();
    names.dedup();
    let budget = budget.lock().unwrap();
    assert_eq!(
        (started, names.len()),
        (2, 40),
        "two start, and every job has a name of its own"
    );
    assert_eq!(
        (budget.slots(), budget.waiting(), budget.committed_bytes()),
        (2, 38, 8 * GIB)
    );
}
