//! Admission by the numbers: the caps, the floor, the CPUs, reservations and releases.

mod common;

use cf_budget::{
    Budget, Free, GIB, Hold, HostFacts, Level, MIB, Millis, Mode, Profile, Refusal, Settings, SettingsError,
    Size, UnknownSlot, Usage,
};
use common::*;

const HOST_4G: u64 = 4 * GIB + 64 * MIB;

#[test]
fn the_defaults_are_the_plans_table() {
    let gib = |n: u64, d: u64| n * GIB / d;
    let host = |gb: u64| HostFacts {
        total_bytes: gb * GIB,
        cores: 8,
    };
    // (memory, dedicated cap, floor), (away cap, floor), (in use floor, cap when allowed)
    let table = [
        (
            8,
            (gib(5, 1), gib(3, 2)),
            (gib(4, 1), gib(3, 2)),
            (gib(2, 1), gib(5, 2)),
        ),
        (
            16,
            (gib(12, 1), gib(2, 1)),
            (16 * GIB - 16 * GIB / 5 * 2, gib(3, 1)),
            (gib(4, 1), 16 * GIB - 16 * GIB / 5 * 3),
        ),
        (
            32,
            (gib(24, 1), gib(3, 1)),
            (32 * GIB - 32 * GIB / 5 * 2, gib(4, 1)),
            (gib(6, 1), 32 * GIB - 32 * GIB / 5 * 3),
        ),
    ];
    for (gb, dedicated, away, (in_use_floor, allowed)) in table {
        let d = Settings::for_host(host(gb), Mode::Dedicated);
        assert_eq!(
            (d.away.cap_bytes, d.away.floor_bytes),
            dedicated,
            "{gb} GB dedicated"
        );
        assert_eq!(d.in_use, d.away, "a dedicated Mac has one set of limits");
        let w = Settings::for_host(host(gb), Mode::Workstation);
        assert_eq!((w.away.cap_bytes, w.away.floor_bytes), away, "{gb} GB away");
        assert_eq!(
            (w.in_use.cap_bytes, w.in_use.floor_bytes),
            (0, in_use_floor),
            "{gb} GB in use"
        );
        assert_eq!(
            Settings::cap_while_in_use(gb * GIB),
            allowed,
            "{gb} GB while in use, allowed"
        );
        assert_eq!(d.check(), Ok(()));
        assert_eq!(w.check(), Ok(()));
    }
    // 9.6 GB and 6.4 GB on a 16 GB Mac, to the byte the division leaves.
    assert_eq!(workstation().away.cap_bytes / MIB, 9830);
    assert_eq!(Settings::cap_while_in_use(16 * GIB) / MIB, 6553);
}

#[test]
fn the_default_cpus_and_times() {
    let d = dedicated();
    assert_eq!((d.away.cpus, d.vcpus), (8, [2, 4, 4, 4]));
    assert_eq!(d.critical_for, Millis::secs(30));
    let w = workstation();
    assert_eq!((w.away.cpus, w.in_use.cpus), (6, 4));
    assert_eq!(w.critical_for, Millis::secs(10));
    assert_eq!(
        (w.idle_after, w.reservation_ttl, w.calm_for),
        (Millis::mins(15), Millis::mins(2), Millis::secs(60))
    );
    assert_eq!(w.job_limit_bytes(Size::G4), 4 * GIB - GIB / 4);
    assert_eq!(w.host_bytes(Size::G4), HOST_4G);
    // A Mac with one or two cores still gets one CPU for CI, and never more than it has.
    for cores in [1, 2] {
        let small = Settings::for_host(
            HostFacts {
                total_bytes: 8 * GIB,
                cores,
            },
            Mode::Dedicated,
        );
        assert_eq!(small.away.cpus, 1);
    }
}

#[test]
fn labels_name_sizes() {
    for size in Size::ALL {
        assert_eq!(Size::from_label(size.label()), Some(size));
        assert_eq!(Size::ALL[size.index()], size);
    }
    assert_eq!(
        Size::ALL.map(Size::vm_bytes),
        [2 * GIB, 4 * GIB, 6 * GIB, 8 * GIB]
    );
    assert_eq!(Size::ALL.map(Size::label), ["cf-2g", "cf-4g", "cf-6g", "cf-8g"]);
    assert_eq!(Size::from_label("self-hosted"), Some(Size::G4));
    assert_eq!(Size::from_label("CF-6G"), Some(Size::G6));
    for not_a_size in ["", "cf-3g", "cf-4g ", "cf-16g", "ubuntu-latest", "cf-4", "4g"] {
        assert_eq!(Size::from_label(not_a_size), None, "{not_a_size:?}");
    }
    assert_eq!(Size::of_labels(["self-hosted", "linux", "cf-8g"]), Some(Size::G8));
    assert_eq!(Size::of_labels(["cf-2g", "Self-Hosted"]), Some(Size::G2));
    assert_eq!(Size::of_labels(["self-hosted", "arm64"]), Some(Size::G4));
    assert_eq!(Size::of_labels(["cf-2g", "cf-2g"]), Some(Size::G2));
    assert_eq!(Size::of_labels(["cf-2g", "cf-4g"]), None);
    assert_eq!(Size::of_labels(["ubuntu-latest"]), None);
    assert_eq!(Size::of_labels([]), None);
}

#[test]
fn every_bad_setting_is_refused_by_name() {
    let good = workstation();
    let bad = |change: fn(&mut Settings)| {
        let mut settings = good.clone();
        change(&mut settings);
        settings.check().unwrap_err()
    };
    assert_eq!(bad(|s| s.total_bytes = 0), SettingsError::NoHost);
    assert_eq!(bad(|s| s.cores = 0), SettingsError::NoHost);
    assert!(matches!(
        bad(|s| s.away.floor_bytes = GIB - 1),
        SettingsError::FloorTooLow { profile: "away", .. }
    ));
    assert!(matches!(
        bad(|s| s.away.cap_bytes = 16 * GIB - s.away.floor_bytes + 1),
        SettingsError::CapOverHost { profile: "away", .. }
    ));
    assert!(matches!(
        bad(|s| s.away.cap_bytes = u64::MAX),
        SettingsError::CapOverHost { .. }
    ));
    assert!(matches!(
        bad(|s| s.away.cpus = 9),
        SettingsError::CpusOverHost {
            cpus: 9,
            cores: 8,
            ..
        }
    ));
    assert_eq!(
        bad(|s| s.in_use.cap_bytes = s.away.cap_bytes + 1),
        SettingsError::InUseLooser
    );
    assert_eq!(
        bad(|s| s.in_use.floor_bytes = s.away.floor_bytes - 1),
        SettingsError::InUseLooser
    );
    assert_eq!(
        bad(|s| s.in_use.cpus = s.away.cpus + 1),
        SettingsError::InUseLooser
    );
    assert_eq!(bad(|s| s.vcpus[2] = 0), SettingsError::NoVcpu { label: "cf-6g" });
    assert_eq!(
        bad(|s| s.guest_overhead_bytes = 2 * GIB),
        SettingsError::OverheadTooLarge
    );
    assert_eq!(
        bad(|s| s.vm_host_overhead_bytes = 2 * GIB),
        SettingsError::OverheadTooLarge
    );
    assert_eq!(
        bad(|s| s.disk_evict_bytes = s.disk_hold_bytes + 1),
        SettingsError::DiskLimitsCrossed
    );
    let durations: [fn(&mut Settings); 8] = [
        |s| s.idle_after = Millis::ZERO,
        |s| s.calm_for = Millis::ZERO,
        |s| s.swap_quiet = Millis::ZERO,
        |s| s.critical_for = Millis::ZERO,
        |s| s.swap_growth_window = Millis::ZERO,
        |s| s.evict_every = Millis::ZERO,
        |s| s.reservation_ttl = Millis::ZERO,
        |s| s.stale_after = Millis::ZERO,
    ];
    for change in durations {
        assert!(matches!(bad(change), SettingsError::ZeroDuration { .. }));
    }
    // The limits themselves are allowed: a cap that takes all but the floor, a floor of exactly the least.
    let mut edge = good.clone();
    edge.away = Profile {
        cap_bytes: 15 * GIB,
        floor_bytes: GIB,
        cpus: 8,
    };
    edge.in_use = Profile {
        cap_bytes: 15 * GIB,
        floor_bytes: GIB,
        cpus: 8,
    };
    edge.disk_evict_bytes = edge.disk_hold_bytes;
    edge.guest_overhead_bytes = 2 * GIB - 1;
    assert_eq!(edge.check(), Ok(()));
    assert!(
        Budget::new(bad_settings()).is_err(),
        "a budget is never built on refused settings"
    );
}

fn bad_settings() -> Settings {
    let mut settings = workstation();
    settings.away.cap_bytes = 17 * GIB;
    settings
}

#[test]
fn a_new_budget_is_closed_until_sixty_calm_seconds() {
    let mut budget = Budget::new(dedicated()).unwrap();
    assert_eq!(budget.claim(secs(0), Size::G2), Err(Refusal::Blind));
    assert_eq!(budget.level(secs(0)), Level::Hold);
    watch(&mut budget, 0, 59, |_| {});
    assert_eq!(
        budget.claim(secs(59), Size::G2),
        Err(Refusal::Calming {
            left: Millis::secs(1)
        })
    );
    assert_eq!(
        budget.claim(ms(59_999), Size::G2),
        Err(Refusal::Calming { left: Millis(1) })
    );
    assert_eq!(budget.level(ms(59_999)), Level::Tight);
    assert_eq!(budget.free(ms(59_999)), Free::default());
    assert_eq!(budget.level(secs(60)), Level::Open);
    let grant = budget.claim(secs(60), Size::G2).unwrap();
    assert_eq!(
        (grant.vm_bytes, grant.job_limit_bytes, grant.cpus),
        (2 * GIB, 2 * GIB - GIB / 4, 2)
    );
    assert_eq!(grant.expires, Some(Millis::secs(180)));
}

#[test]
fn the_dedicated_mac_fills_to_its_cap_and_no_further() {
    let mut budget = open(one_vcpu(dedicated()));
    budget.observe(
        secs(61),
        Some({
            let mut reading = calm(secs(61));
            reading.available_bytes = 16 * GIB;
            reading
        }),
    );
    for _ in 0..3 {
        budget.claim(secs(61), Size::G4).unwrap();
    }
    assert_eq!(budget.committed_bytes(), 12 * GIB);
    for size in Size::ALL {
        assert_eq!(
            budget.claim(secs(61), size),
            Err(Refusal::Full {
                cap_bytes: 12 * GIB,
                committed_bytes: 12 * GIB
            }),
            "{size:?} over a full cap"
        );
    }
    assert_eq!((budget.committed_bytes(), budget.slots()), (12 * GIB, 3));
}

#[test]
fn the_cap_is_met_exactly_and_missed_by_one_size() {
    // 12 GB: 8 + 4 fits, 8 + 6 does not, 6 + 6 fits and then nothing does.
    let mut budget = open(one_vcpu(dedicated()));
    let now = secs(60);
    budget.claim(now, Size::G8).unwrap();
    assert!(matches!(budget.claim(now, Size::G6), Err(Refusal::Full { .. })));
    budget.claim(now, Size::G4).unwrap();
    assert!(matches!(budget.claim(now, Size::G2), Err(Refusal::Full { .. })));
}

#[test]
fn vcpus_are_never_overcommitted() {
    // 10 cores, 8 for CI: two cf-4g take them all, whatever memory is left.
    let mut budget = open(dedicated());
    let now = secs(60);
    assert_eq!(budget.free(now), Free([4, 2, 2, 1]));
    budget.claim(now, Size::G4).unwrap();
    budget.claim(now, Size::G4).unwrap();
    assert_eq!(budget.cpus_committed(), 8);
    assert_eq!(
        budget.claim(now, Size::G2),
        Err(Refusal::Cpus { cap: 8, committed: 8 })
    );
    assert_eq!(
        budget.claim(now, Size::G4),
        Err(Refusal::Cpus { cap: 8, committed: 8 })
    );
    assert_eq!(budget.free(now), Free::default());
    // 4 + 2 + 2 is exactly the cap.
    let mut budget = open(dedicated());
    budget.claim(now, Size::G4).unwrap();
    budget.claim(now, Size::G2).unwrap();
    budget.claim(now, Size::G2).unwrap();
    assert_eq!(budget.cpus_committed(), 8);
    assert!(matches!(budget.claim(now, Size::G2), Err(Refusal::Cpus { .. })));
}

#[test]
fn the_floor_holds_to_the_byte() {
    // Three VMs of 4 GB with their overhead and the 2 GB floor: exactly that much available fits the third,
    // one byte less does not.
    let needed = 3 * HOST_4G + 2 * GIB;
    for (available, third) in [
        (needed, Ok(())),
        (needed - 1, Err(Refusal::Memory { short_bytes: 1 })),
    ] {
        let mut budget = open(one_vcpu(dedicated()));
        let mut reading = calm(secs(61));
        reading.available_bytes = available;
        budget.observe(secs(61), Some(reading));
        budget.claim(secs(61), Size::G4).unwrap();
        budget.claim(secs(61), Size::G4).unwrap();
        assert_eq!(budget.fits(secs(61), Size::G4), third);
        assert_eq!(budget.claim(secs(61), Size::G4).map(|_| ()), third);
    }
}

#[test]
fn a_vm_that_grew_is_not_counted_twice() {
    // One 4 GB VM is promised; 10.5 GB are available. A second and a third 4 GB VM need
    // 3 x (4 GB + 64 MB) + 2 GB = 14.19 GB: no. Once the first VM has taken 3 GB (available falls by 3 GB, its
    // share still to come too), the answer must be the same: growth of a VM changes nothing.
    let mut budget = open(one_vcpu(dedicated()));
    let first = budget.claim(secs(60), Size::G4).unwrap().slot;
    budget.confirm(first).unwrap();
    let before = 2 * HOST_4G + 2 * GIB;
    let look = |budget: &mut Budget, second: u64, available: u64| {
        let mut reading = calm(secs(second));
        reading.available_bytes = available;
        budget.observe(secs(second), Some(reading));
        (
            budget.fits(secs(second), Size::G4),
            budget.fits(secs(second), Size::G8),
        )
    };
    assert_eq!(
        look(&mut budget, 61, before),
        (Ok(()), Err(Refusal::Memory { short_bytes: 4 * GIB }))
    );
    budget
        .usage(
            first,
            Usage {
                resident_bytes: 3 * GIB,
                scratch_bytes: 0,
            },
        )
        .unwrap();
    assert_eq!(
        look(&mut budget, 62, before - 3 * GIB),
        (Ok(()), Err(Refusal::Memory { short_bytes: 4 * GIB }))
    );
    // A VM cannot have taken more than its size and overhead: a larger report frees nothing.
    budget
        .usage(
            first,
            Usage {
                resident_bytes: 64 * GIB,
                scratch_bytes: 0,
            },
        )
        .unwrap();
    assert_eq!(
        look(&mut budget, 63, HOST_4G + 2 * GIB),
        (Ok(()), Err(Refusal::Memory { short_bytes: 4 * GIB }))
    );
    assert_eq!(
        look(&mut budget, 64, HOST_4G + 2 * GIB - 1).0,
        Err(Refusal::Memory { short_bytes: 1 })
    );
}

#[test]
fn a_vm_nobody_measured_yet_is_all_still_to_come() {
    let mut budget = open(one_vcpu(dedicated()));
    let first = budget.claim(secs(60), Size::G4).unwrap().slot;
    let mut reading = calm(secs(61));
    reading.available_bytes = HOST_4G + (2 * GIB + 64 * MIB) + 2 * GIB - 1;
    budget.observe(secs(61), Some(reading));
    assert_eq!(
        budget.fits(secs(61), Size::G2),
        Err(Refusal::Memory { short_bytes: 1 })
    );
    budget
        .usage(
            first,
            Usage {
                resident_bytes: 1,
                scratch_bytes: 0,
            },
        )
        .unwrap();
    assert_eq!(budget.fits(secs(61), Size::G2), Ok(()));
}

#[test]
fn the_host_overhead_is_a_setting() {
    for overhead in [0, 64 * MIB, 512 * MIB] {
        let mut settings = one_vcpu(dedicated());
        settings.vm_host_overhead_bytes = overhead;
        let mut budget = open(settings);
        let mut reading = calm(secs(61));
        reading.available_bytes = 2 * (4 * GIB + overhead) + 2 * GIB;
        budget.observe(secs(61), Some(reading));
        budget.claim(secs(61), Size::G4).unwrap();
        budget.claim(secs(61), Size::G4).unwrap();
        assert_eq!(
            budget.claim(secs(61), Size::G2),
            Err(Refusal::Memory {
                short_bytes: 2 * GIB + overhead
            })
        );
    }
}

#[test]
fn the_workstation_waits_fifteen_minutes_for_its_person() {
    let mut budget = Budget::new(workstation()).unwrap();
    watch(&mut budget, 0, 60, |r| r.idle = Millis(15 * 60_000 - 1));
    assert_eq!(budget.claim(secs(60), Size::G2), Err(Refusal::PersonThere));
    assert_eq!(budget.level(secs(60)), Level::Tight);
    assert!(budget.status(secs(60)).person_there);
    watch(&mut budget, 61, 61, |r| r.idle = Millis::mins(15));
    assert!(!budget.status(secs(61)).person_there);
    // 9.6 GB and 6 CPUs: a cf-4g and a cf-2g, then the CPUs are used up.
    budget.claim(secs(61), Size::G4).unwrap();
    budget.claim(secs(61), Size::G2).unwrap();
    assert_eq!(
        budget.claim(secs(61), Size::G2),
        Err(Refusal::Cpus { cap: 6, committed: 6 })
    );
}

#[test]
fn the_workstation_cap_is_nine_point_six() {
    let mut budget = open(one_vcpu(workstation()));
    let now = secs(60);
    budget.claim(now, Size::G8).unwrap();
    let cap_bytes = workstation().away.cap_bytes;
    assert_eq!(
        budget.claim(now, Size::G2),
        Err(Refusal::Full {
            cap_bytes,
            committed_bytes: 8 * GIB
        })
    );
    let mut budget = open(one_vcpu(workstation()));
    budget.claim(now, Size::G4).unwrap();
    budget.claim(now, Size::G4).unwrap();
    assert!(matches!(budget.claim(now, Size::G2), Err(Refusal::Full { .. })));
}

#[test]
fn the_person_coming_back_stops_new_slots_and_no_job() {
    let mut budget = open(workstation());
    let job = budget.claim(secs(60), Size::G4).unwrap().slot;
    budget.confirm(job).unwrap();
    // A touch of the mouse.
    let mut back = calm(secs(61));
    back.idle = Millis(200);
    let seen = budget.observe(secs(61), Some(back));
    assert_eq!(
        (seen.level, seen.evict, seen.expired),
        (Level::Tight, None, vec![])
    );
    assert_eq!(budget.claim(secs(61), Size::G2), Err(Refusal::PersonThere));
    let status = budget.status(secs(61));
    assert!(status.person_there);
    assert_eq!(
        (status.committed_bytes, status.closed_by),
        (4 * GIB, Some(Refusal::PersonThere))
    );
    assert_eq!(status.limits, workstation().in_use);
    // Hours later the job is still there and nothing stopped it.
    for second in 62..600 {
        let mut reading = calm(secs(second));
        reading.idle = Millis::secs(second - 61);
        assert_eq!(budget.observe(secs(second), Some(reading)).evict, None);
    }
    assert_eq!(budget.committed_bytes(), 4 * GIB);
}

#[test]
fn also_while_i_work_opens_a_smaller_budget() {
    let mut settings = workstation();
    settings.in_use.cap_bytes = Settings::cap_while_in_use(settings.total_bytes);
    assert_eq!(settings.check(), Ok(()));
    let mut budget = Budget::new(settings).unwrap();
    watch(&mut budget, 0, 60, |r| r.idle = Millis::ZERO);
    let now = secs(60);
    // 6.4 GB, 4 CPUs and a 4 GB floor while the person is there.
    budget.claim(now, Size::G4).unwrap();
    assert_eq!(
        budget.claim(now, Size::G4),
        Err(Refusal::Full {
            cap_bytes: 6871947676,
            committed_bytes: 4 * GIB
        })
    );
    assert_eq!(
        budget.claim(now, Size::G2),
        Err(Refusal::Cpus { cap: 4, committed: 4 })
    );
    let mut budget = open(workstation());
    let mut settings = budget.settings().clone();
    settings.in_use.cap_bytes = 4 * GIB;
    budget.set_settings(settings).unwrap();
    let mut reading = calm(secs(61));
    reading.idle = Millis::ZERO;
    reading.available_bytes = HOST_4G + 4 * GIB - 1;
    budget.observe(secs(61), Some(reading));
    assert_eq!(
        budget.claim(secs(61), Size::G4),
        Err(Refusal::Memory { short_bytes: 1 }),
        "the floor in use is 4 GB"
    );
}

#[test]
fn a_reservation_runs_out_after_two_minutes_unless_confirmed() {
    let mut budget = open(dedicated());
    let kept = budget.claim(secs(60), Size::G2).unwrap().slot;
    let lost = budget.claim(secs(60), Size::G4).unwrap().slot;
    assert_eq!(budget.confirm(kept), Ok(()));
    assert_eq!(budget.confirm(kept), Ok(()), "GitHub may say it twice");
    watch(&mut budget, 61, 179, |_| {});
    assert_eq!(budget.committed_bytes(), 6 * GIB);
    let seen = budget.observe(ms(179_999), Some(calm(ms(179_999))));
    assert_eq!(seen.expired, vec![]);
    let seen = budget.observe(secs(180), Some(calm(secs(180))));
    assert_eq!(seen.expired, vec![lost]);
    assert_eq!(
        (budget.committed_bytes(), budget.cpus_committed(), budget.slots()),
        (2 * GIB, 2, 1)
    );
    assert_eq!(
        budget.observe(secs(181), Some(calm(secs(181)))).expired,
        vec![],
        "told once"
    );
    assert_eq!(budget.confirm(lost), Err(UnknownSlot(lost)));
    assert_eq!(budget.usage(lost, Usage::default()), Err(UnknownSlot(lost)));
    assert_eq!(budget.release(lost), Err(UnknownSlot(lost)));
    watch(&mut budget, 182, 4000, |_| {});
    assert_eq!(budget.slots(), 1, "a confirmed slot never runs out");
}

#[test]
fn memory_is_given_back_once() {
    let mut budget = open(dedicated());
    let a = budget.claim(secs(60), Size::G4).unwrap().slot;
    let b = budget.claim(secs(60), Size::G4).unwrap().slot;
    assert_ne!(a, b);
    let released = budget.release(a).unwrap();
    assert_eq!(
        (released.size, released.vm_bytes, released.cpus),
        (Size::G4, 4 * GIB, 4)
    );
    assert_eq!((budget.committed_bytes(), budget.cpus_committed()), (4 * GIB, 4));
    assert_eq!(budget.release(a), Err(UnknownSlot(a)));
    assert_eq!(
        (budget.committed_bytes(), budget.cpus_committed()),
        (4 * GIB, 4),
        "a second release frees nothing"
    );
    let c = budget.claim(secs(60), Size::G4).unwrap().slot;
    assert!(c != a && c != b, "a slot's name is never used again");
    assert_eq!(budget.release(a), Err(UnknownSlot(a)));
    assert_eq!(budget.committed_bytes(), 8 * GIB);
}

#[test]
fn asking_reserves_nothing() {
    // One slot is left. Two callers ask, both hear yes; only the first to claim gets it.
    let mut settings = one_vcpu(dedicated());
    settings.away.cap_bytes = 4 * GIB;
    settings.in_use.cap_bytes = 4 * GIB;
    let mut budget = open(settings);
    let now = secs(60);
    assert_eq!(
        (budget.fits(now, Size::G4), budget.fits(now, Size::G4)),
        (Ok(()), Ok(()))
    );
    assert_eq!(budget.slots(), 0);
    budget.claim(now, Size::G4).unwrap();
    assert!(matches!(budget.claim(now, Size::G4), Err(Refusal::Full { .. })));
}

#[test]
fn a_hold_closes_at_once_and_opens_at_once() {
    let mut budget = open(dedicated());
    budget.hold(Hold::Paused, true);
    budget.hold(Hold::StartFailures, true);
    assert_eq!(budget.claim(secs(60), Size::G2), Err(Refusal::Held(Hold::Paused)));
    assert_eq!(budget.level(secs(60)), Level::Hold);
    budget.hold(Hold::Paused, false);
    assert_eq!(
        budget.claim(secs(60), Size::G2),
        Err(Refusal::Held(Hold::StartFailures))
    );
    budget.hold(Hold::StartFailures, false);
    budget.hold(Hold::StartFailures, false);
    assert!(
        budget.claim(secs(60), Size::G2).is_ok(),
        "lifting a hold needs no calm time"
    );
    let names = [
        Hold::Paused,
        Hold::QuietHour,
        Hold::StartFailures,
        Hold::NoImage,
        Hold::Draining,
    ]
    .map(Hold::name);
    assert_eq!(
        names,
        ["paused", "quiet-hour", "start-failures", "no-image", "draining"]
    );
}

#[test]
fn settings_change_under_running_jobs() {
    let mut budget = open(one_vcpu(dedicated()));
    let now = secs(60);
    let old = budget.claim(now, Size::G8).unwrap();
    // The console lowers the Mac to 6 GB for CI: the running job stays, nothing new opens.
    let mut smaller = budget.settings().clone();
    smaller.away.cap_bytes = 6 * GIB;
    smaller.in_use.cap_bytes = 6 * GIB;
    smaller.vcpus = [3; 4];
    budget.set_settings(smaller.clone()).unwrap();
    assert_eq!(budget.committed_bytes(), 8 * GIB);
    assert_eq!(
        budget.claim(now, Size::G2),
        Err(Refusal::Full {
            cap_bytes: 6 * GIB,
            committed_bytes: 8 * GIB
        })
    );
    assert_eq!(
        budget.release(old.slot).unwrap().cpus,
        1,
        "a slot keeps the CPUs it was granted"
    );
    assert_eq!(budget.claim(now, Size::G2).unwrap().cpus, 3);
    // A setting that is refused changes nothing.
    assert!(budget.set_settings(bad_settings()).is_err());
    assert_eq!(budget.settings(), &smaller);
}

#[test]
fn a_vm_found_after_a_restart_counts_without_being_asked_for() {
    let mut budget = Budget::new(dedicated()).unwrap();
    let found = budget.adopt(Size::G8);
    let second = budget.adopt(Size::G8);
    assert_eq!((found.expires, found.vm_bytes, found.cpus), (None, 8 * GIB, 4));
    assert_eq!(
        budget.committed_bytes(),
        16 * GIB,
        "what runs is counted even over the cap"
    );
    watch(&mut budget, 0, 400, |_| {});
    assert_eq!(budget.slots(), 2, "an adopted slot never runs out");
    assert!(matches!(
        budget.claim(secs(400), Size::G2),
        Err(Refusal::Full { .. })
    ));
    budget.release(found.slot).unwrap();
    assert!(matches!(
        budget.claim(secs(400), Size::G6),
        Err(Refusal::Full { .. })
    ));
    assert!(
        budget.claim(secs(400), Size::G4).is_ok(),
        "8 + 4 GB and 4 + 4 CPUs are the caps exactly"
    );
    assert_eq!(
        budget.claim(secs(400), Size::G2),
        Err(Refusal::Full {
            cap_bytes: 12 * GIB,
            committed_bytes: 12 * GIB
        })
    );
    budget.release(second.slot).unwrap();
    assert_eq!(budget.claim(secs(400), Size::G4).unwrap().cpus, 4);
    assert_eq!(
        budget.claim(secs(400), Size::G2),
        Err(Refusal::Cpus { cap: 8, committed: 8 })
    );
}

#[test]
fn free_slots_and_status_say_what_a_claim_would() {
    let mut budget = open(one_vcpu(dedicated()));
    let mut reading = calm(secs(61));
    reading.available_bytes = 16 * GIB;
    budget.observe(secs(61), Some(reading));
    let now = secs(61);
    // 12 GB cap; 16 GB available less the 2 GB floor is room for six VMs of 2 GB + 64 MB, by the cap too.
    assert_eq!(budget.free(now), Free([6, 3, 2, 1]));
    let job = budget.claim(now, Size::G6).unwrap().slot;
    assert_eq!(budget.free(now), Free([3, 1, 1, 0]));
    assert_eq!(budget.free(now).of(Size::G2), 3);
    budget
        .usage(
            job,
            Usage {
                resident_bytes: GIB,
                scratch_bytes: 7,
            },
        )
        .unwrap();
    let status = budget.status(now);
    assert_eq!((status.level, status.closed_by), (Level::Open, None));
    assert_eq!(
        (
            status.committed_bytes,
            status.resident_bytes,
            status.cpus_committed
        ),
        (6 * GIB, GIB, 1)
    );
    assert_eq!(
        (status.limits, status.free),
        (dedicated().away, Free([3, 1, 1, 0]))
    );
    assert_eq!(status.reading, Some(reading));
    assert!(!status.person_there);
    // Six seconds on without a reading nothing is known and nothing is free.
    let late = budget.status(secs(67));
    assert_eq!(
        (late.level, late.closed_by, late.reading, late.free),
        (Level::Hold, Some(Refusal::Blind), None, Free::default())
    );
    assert_eq!(late.committed_bytes, 6 * GIB);
}
