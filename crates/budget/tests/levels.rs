//! The host's signals: what closes the Mac, how long it stays closed, when a job has to stop.

mod common;

use cf_budget::{
    Budget, Eviction, EvictionCause, GIB, HostReading, Level, MIB, Millis, Now, Pressure, Refusal, Size,
    Thermal, Trouble, Usage,
};
use common::*;

/// A change to a calm reading.
type Change = fn(&mut HostReading);

fn refusal(budget: &Budget, second: u64) -> Option<Refusal> {
    budget.fits(secs(second), Size::G2).err()
}

/// Feeds one reading at `second` and says why a slot is refused then.
fn after(budget: &mut Budget, second: u64, change: impl Fn(&mut HostReading)) -> Option<Refusal> {
    watch(budget, second, second, change);
    refusal(budget, second)
}

#[test]
fn pressure_closes_at_once_and_opens_sixty_seconds_after_it_ends() {
    for pressure in [Pressure::Warning, Pressure::Critical] {
        let mut budget = open(dedicated());
        assert_eq!(
            after(&mut budget, 61, |r| r.pressure = pressure),
            Some(Refusal::Trouble(Trouble::Pressure))
        );
        assert_eq!(budget.level(secs(61)), Level::Tight);
        assert_eq!(
            after(&mut budget, 62, |_| {}),
            Some(Refusal::Calming {
                left: Millis::secs(60)
            })
        );
        watch(&mut budget, 63, 121, |_| {});
        assert_eq!(
            refusal(&budget, 121),
            Some(Refusal::Calming {
                left: Millis::secs(1)
            })
        );
        assert_eq!(after(&mut budget, 122, |_| {}), None);
    }
}

#[test]
fn one_bad_second_starts_the_wait_again() {
    let mut budget = open(dedicated());
    after(&mut budget, 61, |r| r.pressure = Pressure::Warning);
    watch(&mut budget, 62, 120, |_| {});
    // One second before it would open, pressure again.
    assert_eq!(
        after(&mut budget, 121, |r| r.pressure = Pressure::Warning),
        Some(Refusal::Trouble(Trouble::Pressure))
    );
    watch(&mut budget, 122, 181, |_| {});
    assert_eq!(
        refusal(&budget, 181),
        Some(Refusal::Calming {
            left: Millis::secs(1)
        })
    );
    assert_eq!(after(&mut budget, 182, |_| {}), None);
}

#[test]
fn a_swap_out_closes_for_thirty_seconds_and_the_calm_time() {
    let mut budget = open(dedicated());
    let swapped = |r: &mut HostReading| r.swapouts = 101;
    assert_eq!(
        after(&mut budget, 61, swapped),
        Some(Refusal::Trouble(Trouble::Swapping))
    );
    assert_eq!(budget.level(secs(61)), Level::Tight);
    // The counter stays where it is: no new swap-out, but the last one is recent.
    watch(&mut budget, 62, 90, swapped);
    assert_eq!(refusal(&budget, 90), Some(Refusal::Trouble(Trouble::Swapping)));
    assert_eq!(
        after(&mut budget, 91, swapped),
        Some(Refusal::Calming {
            left: Millis::secs(60)
        })
    );
    watch(&mut budget, 92, 150, swapped);
    assert_eq!(
        refusal(&budget, 150),
        Some(Refusal::Calming {
            left: Millis::secs(1)
        })
    );
    assert_eq!(after(&mut budget, 151, swapped), None);
    // A counter that falls is no better news than one that rises.
    assert_eq!(
        after(&mut budget, 152, |r| r.swapouts = 3),
        Some(Refusal::Trouble(Trouble::Swapping))
    );
}

#[test]
fn the_first_reading_cannot_show_a_swap_out() {
    // There is nothing to compare the first count with; the calm time covers what happened before.
    let mut budget = Budget::new(dedicated()).unwrap();
    assert_eq!(
        after(&mut budget, 0, |r| r.swapouts = 40_000_000),
        Some(Refusal::Calming {
            left: Millis::secs(60)
        })
    );
}

#[test]
fn under_the_floor_closes_and_only_a_margin_above_it_opens() {
    let floor = 2 * GIB;
    let mut budget = open(dedicated());
    assert_eq!(
        after(&mut budget, 61, |r| r.available_bytes = floor),
        Some(Refusal::Memory {
            short_bytes: 2 * GIB + 64 * MIB
        })
    );
    assert_eq!(
        budget.level(secs(61)),
        Level::Open,
        "at the floor nothing fits, and nothing is wrong either"
    );
    assert_eq!(
        after(&mut budget, 62, |r| r.available_bytes = floor - 1),
        Some(Refusal::Trouble(Trouble::Short))
    );
    assert_eq!(budget.level(secs(62)), Level::Tight);
    // Back over the floor, but not by the margin: still short.
    assert_eq!(
        after(&mut budget, 63, |r| r.available_bytes = floor + GIB - 1),
        Some(Refusal::Trouble(Trouble::Short))
    );
    watch(&mut budget, 64, 200, |r| r.available_bytes = floor + GIB - 1);
    assert_eq!(refusal(&budget, 200), Some(Refusal::Trouble(Trouble::Short)));
    assert_eq!(
        after(&mut budget, 201, |r| r.available_bytes = floor + GIB),
        Some(Refusal::Calming {
            left: Millis::secs(60)
        })
    );
    // Having cleared the margin, the band between is fine again.
    watch(&mut budget, 202, 261, |r| r.available_bytes = floor + 1);
    assert_eq!(
        refusal(&budget, 261),
        Some(Refusal::Memory {
            short_bytes: 2 * GIB + 64 * MIB - 1
        })
    );
}

#[test]
fn memory_swinging_around_the_floor_never_opens_the_mac() {
    let floor = 2 * GIB;
    let mut budget = open(dedicated());
    let mut levels = Vec::new();
    for second in 61..2000 {
        let available = if second % 7 == 0 { floor - MIB } else { 16 * GIB };
        watch(&mut budget, second, second, |r| r.available_bytes = available);
        levels.push(budget.level(secs(second)));
    }
    assert!(
        levels[10..].iter().all(|level| *level == Level::Tight),
        "{levels:?}"
    );
}

#[test]
fn a_steady_host_changes_level_once() {
    // The same reading for an hour: closed, then open at sixty seconds, then nothing.
    let mut budget = Budget::new(dedicated()).unwrap();
    let mut changes = Vec::new();
    let mut last = None;
    for second in 0..3600 {
        let level = budget.observe(secs(second), Some(calm(secs(second)))).level;
        if last != Some(level) {
            changes.push((second, level));
            last = Some(level);
        }
    }
    assert_eq!(changes, [(0, Level::Tight), (60, Level::Open)]);
}

#[test]
fn a_signal_that_flips_every_second_keeps_the_mac_closed() {
    let flips: [fn(&mut HostReading, bool); 5] = [
        |r, on| r.pressure = if on { Pressure::Warning } else { Pressure::Normal },
        |r, on| r.thermal = if on { Thermal::Serious } else { Thermal::Fair },
        |r, on| r.on_battery = on,
        |r, on| r.disk_free_bytes = if on { 25 * GIB - 1 } else { 25 * GIB },
        |r, on| r.available_bytes = if on { 0 } else { 16 * GIB },
    ];
    for flip in flips {
        let mut budget = open(dedicated());
        for second in 62..400 {
            watch(&mut budget, second, second, |r| flip(r, second % 2 == 0));
            assert_ne!(budget.level(secs(second)), Level::Open, "second {second}");
            assert!(budget.fits(secs(second), Size::G2).is_err());
        }
    }
}

#[test]
fn heat_battery_and_a_full_disk_hold() {
    let cases: [(Change, Trouble); 4] = [
        (|r| r.thermal = Thermal::Serious, Trouble::Hot),
        (|r| r.thermal = Thermal::Critical, Trouble::Hot),
        (|r| r.on_battery = true, Trouble::Battery),
        (|r| r.disk_free_bytes = 25 * GIB - 1, Trouble::DiskLow),
    ];
    for (change, trouble) in cases {
        let mut budget = open(dedicated());
        assert_eq!(after(&mut budget, 61, change), Some(Refusal::Trouble(trouble)));
        assert_eq!(budget.level(secs(61)), Level::Hold);
        assert_eq!(
            after(&mut budget, 62, |_| {}),
            Some(Refusal::Calming {
                left: Millis::secs(60)
            })
        );
    }
    // The limits themselves are fine.
    let mut budget = open(dedicated());
    assert_eq!(
        after(&mut budget, 61, |r| {
            r.thermal = Thermal::Fair;
            r.disk_free_bytes = 25 * GIB
        }),
        None
    );
    let names = [
        Trouble::Blind,
        Trouble::Pressure,
        Trouble::Swapping,
        Trouble::Short,
        Trouble::Hot,
        Trouble::Battery,
        Trouble::DiskLow,
    ]
    .map(Trouble::name);
    assert_eq!(
        names,
        [
            "no-reading",
            "memory-pressure",
            "swapping",
            "under-floor",
            "thermal",
            "battery",
            "disk"
        ]
    );
    assert_eq!(Refusal::PersonThere.name(), "person-there");
    assert_eq!(Refusal::Trouble(Trouble::Hot).name(), "thermal");
    assert_eq!(Refusal::Blind.name(), "no-reading");
    assert_eq!(Refusal::Calming { left: Millis(1) }.name(), "calming");
    assert_eq!(
        Refusal::Full {
            cap_bytes: 0,
            committed_bytes: 0
        }
        .name(),
        "full"
    );
    assert_eq!(Refusal::Cpus { cap: 0, committed: 0 }.name(), "cpus");
    assert_eq!(Refusal::Memory { short_bytes: 1 }.name(), "memory");
}

#[test]
fn a_reading_that_fails_closes_the_mac() {
    let mut budget = open(dedicated());
    let seen = budget.observe(secs(61), None);
    assert_eq!((seen.level, seen.evict), (Level::Hold, None));
    assert_eq!(refusal(&budget, 61), Some(Refusal::Blind));
    assert_eq!(budget.status(secs(61)).reading, None);
    // It reads again: the wait starts from the first good reading.
    assert_eq!(
        after(&mut budget, 62, |_| {}),
        Some(Refusal::Calming {
            left: Millis::secs(60)
        })
    );
    watch(&mut budget, 63, 121, |_| {});
    assert!(refusal(&budget, 121).is_some());
    assert_eq!(after(&mut budget, 122, |_| {}), None);
}

#[test]
fn an_old_reading_closes_the_mac() {
    let mut budget = open(dedicated());
    // Nobody read the host for a while: five seconds are still fine, a millisecond more is not.
    assert_eq!(budget.fits(secs(65), Size::G2), Ok(()));
    assert_eq!(budget.fits(ms(65_001), Size::G2), Err(Refusal::Blind));
    assert_eq!(budget.claim(ms(65_001), Size::G2), Err(Refusal::Blind));
    assert_eq!(budget.level(ms(65_001)), Level::Hold);
    // A caller whose clock is before the reading has no reading either.
    assert_eq!(budget.fits(secs(59), Size::G2), Err(Refusal::Blind));
    assert_eq!(
        budget.fits(
            Now {
                awake: Millis::secs(59),
                elapsed: Millis::secs(60)
            },
            Size::G2
        ),
        Err(Refusal::Blind)
    );
    // Reading again after the pause: the calm time starts over, since nobody watched in between.
    assert_eq!(
        after(&mut budget, 66, |_| {}),
        Some(Refusal::Calming {
            left: Millis::secs(60)
        })
    );
}

#[test]
fn a_reading_handed_over_late_or_from_the_future_is_no_reading() {
    for (taken, now) in [(55_999, 61_000), (61_001, 61_000)] {
        let mut budget = open(dedicated());
        let seen = budget.observe(ms(now), Some(calm(ms(taken))));
        assert_eq!(seen.level, Level::Hold, "taken at {taken}, handed over at {now}");
        assert_eq!(budget.fits(ms(now), Size::G2), Err(Refusal::Blind));
    }
    // Exactly five seconds old still counts.
    let mut budget = open(dedicated());
    budget.observe(secs(61), Some(calm(secs(56))));
    assert_eq!(budget.fits(secs(61), Size::G2), Ok(()));
}

#[test]
fn readings_too_far_apart_start_the_wait_again() {
    let mut budget = open(dedicated());
    watch(&mut budget, 65, 65, |_| {});
    assert_eq!(
        refusal(&budget, 65),
        None,
        "five seconds apart is still one watch"
    );
    budget.observe(ms(70_001), Some(calm(ms(70_001))));
    assert_eq!(
        budget.fits(ms(70_001), Size::G2),
        Err(Refusal::Calming {
            left: Millis::secs(60)
        })
    );
}

#[test]
fn sleep_closes_the_mac_and_ends_reservations() {
    let mut budget = open(dedicated());
    let reserved = budget.claim(secs(60), Size::G4).unwrap().slot;
    let running = budget.claim(secs(60), Size::G2).unwrap().slot;
    budget.confirm(running).unwrap();
    // Ten minutes pass with the lid closed: the awake clock stood still.
    let woke = Now {
        awake: Millis::secs(61),
        elapsed: Millis::secs(661),
    };
    let seen = budget.observe(woke, Some(calm(woke)));
    assert_eq!(seen.slept, Millis::secs(600));
    assert_eq!(seen.expired, vec![reserved]);
    assert_eq!(seen.level, Level::Tight);
    assert_eq!(
        budget.fits(woke, Size::G2),
        Err(Refusal::Calming {
            left: Millis::secs(60)
        })
    );
    assert_eq!(
        budget.committed_bytes(),
        2 * GIB,
        "the running VM is still counted"
    );
    // A second without sleep reports none.
    let next = Now {
        awake: Millis::secs(62),
        elapsed: Millis::secs(662),
    };
    assert_eq!(budget.observe(next, Some(calm(next))).slept, Millis::ZERO);
}

#[test]
fn two_looks_in_the_same_moment_are_not_a_clock_gone_back() {
    let mut budget = open(dedicated());
    // The daemon may look twice within a millisecond, after an event and on its tick.
    for _ in 0..3 {
        budget.observe(secs(60), Some(calm(secs(60))));
        assert_eq!(budget.level(secs(60)), Level::Open);
    }
    let slot = budget.claim(secs(60), Size::G4).unwrap().slot;
    // Nor does a second look give a reservation its two minutes again.
    for second in [100, 100, 179, 179] {
        assert_eq!(budget.observe(secs(second), Some(calm(secs(second)))).expired, []);
    }
    assert_eq!(budget.observe(secs(180), Some(calm(secs(180)))).expired, [slot]);
}

#[test]
fn a_clock_that_goes_back_closes_the_mac_and_frees_nothing_early() {
    let mut budget = open(dedicated());
    watch(&mut budget, 61, 100, |_| {});
    let reserved = budget.claim(secs(100), Size::G4).unwrap().slot;
    // The clock is suddenly 50 seconds earlier.
    let seen = budget.observe(secs(50), Some(calm(secs(50))));
    assert_eq!(
        (seen.level, seen.slept, seen.expired),
        (Level::Tight, Millis::ZERO, vec![])
    );
    assert_eq!(
        budget.fits(secs(50), Size::G2),
        Err(Refusal::Calming {
            left: Millis::secs(60)
        })
    );
    // The reservation gets its two minutes from here, not from a time that no longer means anything.
    watch(&mut budget, 51, 169, |_| {});
    assert_eq!(budget.slots(), 1);
    let seen = budget.observe(secs(170), Some(calm(secs(170))));
    assert_eq!(seen.expired, vec![reserved]);
    // Only the awake clock going back is a gap too.
    let mut budget = open(dedicated());
    let odd = Now {
        awake: Millis::secs(10),
        elapsed: Millis::secs(61),
    };
    budget.observe(odd, Some(calm(odd)));
    assert_eq!(
        budget.fits(odd, Size::G2),
        Err(Refusal::Calming {
            left: Millis::secs(60)
        })
    );
}

/// An open workstation with two running jobs, the cf-2g started last.
fn two_jobs() -> (Budget, cf_budget::SlotId, cf_budget::SlotId) {
    let mut budget = open(workstation());
    let older = budget.claim(secs(60), Size::G4).unwrap().slot;
    let newer = budget.claim(secs(60), Size::G2).unwrap().slot;
    budget.confirm(older).unwrap();
    budget.confirm(newer).unwrap();
    (budget, older, newer)
}

fn critical(budget: &mut Budget, at: Now) -> cf_budget::Observed {
    let mut reading = calm(at);
    reading.pressure = Pressure::Critical;
    budget.observe(at, Some(reading))
}

#[test]
fn critical_pressure_that_lasts_stops_the_newest_job_and_one_at_a_time() {
    let (mut budget, older, newer) = two_jobs();
    for second in 61..71 {
        let seen = critical(&mut budget, secs(second));
        assert_eq!((seen.evict, seen.level), (None, Level::Tight), "second {second}");
    }
    assert_eq!(
        critical(&mut budget, ms(70_999)).evict,
        None,
        "ten seconds less a millisecond"
    );
    let seen = critical(&mut budget, secs(71));
    assert_eq!(
        seen.evict,
        Some(Eviction {
            slot: newer,
            cause: EvictionCause::Memory
        })
    );
    assert_eq!(seen.level, Level::Evict);
    assert_eq!(
        budget.committed_bytes(),
        6 * GIB,
        "a job that was told to stop is counted until its VM is gone"
    );
    // The next one only thirty seconds later, and not the same job again.
    for second in 72..101 {
        assert_eq!(critical(&mut budget, secs(second)).evict, None, "second {second}");
    }
    assert_eq!(critical(&mut budget, ms(100_999)).evict, None);
    assert_eq!(
        critical(&mut budget, secs(101)).evict,
        Some(Eviction {
            slot: older,
            cause: EvictionCause::Memory
        })
    );
    // Nothing is left to stop.
    for second in 102..200 {
        assert_eq!(critical(&mut budget, secs(second)).evict, None);
    }
}

#[test]
fn a_dedicated_mac_waits_thirty_seconds_of_critical_pressure() {
    let mut budget = open(dedicated());
    let job = budget.claim(secs(60), Size::G4).unwrap().slot;
    budget.confirm(job).unwrap();
    for second in 61..91 {
        assert_eq!(critical(&mut budget, secs(second)).evict, None, "second {second}");
    }
    assert_eq!(
        critical(&mut budget, secs(91)).evict,
        Some(Eviction {
            slot: job,
            cause: EvictionCause::Memory
        })
    );
}

#[test]
fn critical_pressure_with_a_break_does_not_add_up() {
    let (mut budget, ..) = two_jobs();
    for second in 61..300 {
        // Nine seconds critical, one warning, again and again.
        let mut reading = calm(secs(second));
        reading.pressure = if second % 10 == 0 {
            Pressure::Warning
        } else {
            Pressure::Critical
        };
        assert_eq!(
            budget.observe(secs(second), Some(reading)).evict,
            None,
            "second {second}"
        );
    }
}

#[test]
fn no_job_is_stopped_when_ci_holds_next_to_nothing() {
    // Reservations hold no memory: stopping nothing would free nothing.
    let mut budget = open(workstation());
    budget.claim(secs(60), Size::G4).unwrap();
    for second in 61..120 {
        assert_eq!(critical(&mut budget, secs(second)).evict, None);
    }
    // A running VM that holds under 1 GB is not the cause either; at 1 GB it is stopped.
    let mut budget = open(workstation());
    let job = budget.claim(secs(60), Size::G4).unwrap().slot;
    budget.confirm(job).unwrap();
    budget
        .usage(
            job,
            Usage {
                resident_bytes: GIB - 1,
                scratch_bytes: 0,
            },
        )
        .unwrap();
    for second in 61..120 {
        let seen = critical(&mut budget, secs(second));
        assert_eq!(seen.evict, None);
        if second > 71 {
            assert_eq!(
                seen.level,
                Level::Evict,
                "the level says how the Mac is, whoever caused it"
            );
        }
    }
    budget
        .usage(
            job,
            Usage {
                resident_bytes: GIB,
                scratch_bytes: 0,
            },
        )
        .unwrap();
    assert_eq!(
        critical(&mut budget, secs(120)).evict,
        Some(Eviction {
            slot: job,
            cause: EvictionCause::Memory
        })
    );
}

#[test]
fn a_vm_nobody_measured_counts_as_holding_its_size() {
    let mut budget = open(workstation());
    let job = budget.claim(secs(60), Size::G2).unwrap().slot;
    budget.confirm(job).unwrap();
    for second in 61..71 {
        critical(&mut budget, secs(second));
    }
    assert_eq!(critical(&mut budget, secs(71)).evict.map(|e| e.slot), Some(job));
}

#[test]
fn no_job_is_stopped_without_a_reading() {
    let (mut budget, ..) = two_jobs();
    for second in 61..70 {
        critical(&mut budget, secs(second));
    }
    // The reader fails just before the ten seconds are over, and for a long time after.
    for second in 70..200 {
        let seen = budget.observe(secs(second), None);
        assert_eq!((seen.evict, seen.level), (None, Level::Hold));
    }
    // Critical again: the ten seconds count from here.
    for second in 200..210 {
        assert_eq!(critical(&mut budget, secs(second)).evict, None, "second {second}");
    }
    assert!(critical(&mut budget, secs(210)).evict.is_some());
}

/// Pressure at warning with swap in use growing by `per_minute` a minute.
fn swapping(budget: &mut Budget, second: u64, per_minute: u64) -> cf_budget::Observed {
    let mut reading = calm(secs(second));
    reading.pressure = Pressure::Warning;
    reading.swap_used_bytes = GIB + (second - 61) * per_minute / 60;
    budget.observe(secs(second), Some(reading))
}

#[test]
fn warning_with_swap_growing_fast_for_a_minute_stops_a_job() {
    let (mut budget, _, newer) = two_jobs();
    for second in 61..121 {
        assert_eq!(
            swapping(&mut budget, second, 512 * MIB).evict,
            None,
            "second {second}"
        );
    }
    assert_eq!(
        swapping(&mut budget, 121, 512 * MIB).evict,
        Some(Eviction {
            slot: newer,
            cause: EvictionCause::Memory
        })
    );
    // A byte a minute less is not fast enough, however long it lasts.
    let (mut budget, ..) = two_jobs();
    for second in 61..600 {
        assert_eq!(
            swapping(&mut budget, second, 512 * MIB - 60).evict,
            None,
            "second {second}"
        );
    }
}

#[test]
fn swap_growing_without_pressure_or_pressure_without_growth_stops_no_job() {
    let (mut budget, ..) = two_jobs();
    for second in 61..400 {
        let mut reading = calm(secs(second));
        reading.swap_used_bytes = (second - 61) * 100 * MIB;
        assert_eq!(budget.observe(secs(second), Some(reading)).evict, None);
    }
    let (mut budget, ..) = two_jobs();
    for second in 61..400 {
        assert_eq!(swapping(&mut budget, second, 0).evict, None);
    }
    // Growth that happened before the pressure began does not count for it: 2 GB at once, then flat.
    let (mut budget, ..) = two_jobs();
    watch(&mut budget, 61, 61, |r| r.swap_used_bytes = 0);
    for second in 62..400 {
        let mut reading = calm(secs(second));
        reading.pressure = Pressure::Warning;
        reading.swap_used_bytes = 2 * GIB;
        let seen = budget.observe(secs(second), Some(reading));
        if second >= 123 {
            assert_eq!(seen.evict, None, "second {second}");
        }
    }
}

#[test]
fn a_nearly_full_disk_stops_the_job_with_the_largest_scratch() {
    let (mut budget, older, newer) = two_jobs();
    let full = |budget: &mut Budget, second: u64, free: u64| {
        let mut reading = calm(secs(second));
        reading.disk_free_bytes = free;
        budget.observe(secs(second), Some(reading))
    };
    // Nobody has written anything yet: stopping a job would free no disk.
    assert_eq!(full(&mut budget, 61, 5 * GIB - 1).evict, None);
    assert_eq!(budget.level(secs(61)), Level::Evict);
    budget
        .usage(
            older,
            Usage {
                resident_bytes: 0,
                scratch_bytes: 9 * GIB,
            },
        )
        .unwrap();
    budget
        .usage(
            newer,
            Usage {
                resident_bytes: 0,
                scratch_bytes: 3 * GIB,
            },
        )
        .unwrap();
    assert_eq!(
        full(&mut budget, 62, 5 * GIB).evict,
        None,
        "5 GB free is still enough"
    );
    assert_eq!(
        full(&mut budget, 63, 5 * GIB - 1).evict,
        Some(Eviction {
            slot: older,
            cause: EvictionCause::Disk
        })
    );
    assert_eq!(full(&mut budget, 64, 0).evict, None, "one job per thirty seconds");
    assert_eq!(
        full(&mut budget, 93, 0).evict,
        Some(Eviction {
            slot: newer,
            cause: EvictionCause::Disk
        })
    );
}

#[test]
fn a_clock_that_goes_back_does_not_stop_jobs_in_a_burst() {
    let (mut budget, older, newer) = two_jobs();
    for second in 61..=71 {
        critical(&mut budget, secs(second));
    }
    // One job was just stopped at second 71. The clock goes back; pressure stays critical.
    let mut stops = Vec::new();
    for second in 20..=60 {
        if let Some(eviction) = critical(&mut budget, secs(second)).evict {
            stops.push((second, eviction.slot));
        }
    }
    assert_eq!(
        stops,
        [(50, older)],
        "thirty seconds after the clock went back, not at once: {newer:?} went before"
    );
}
