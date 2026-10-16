//! What the owner sets on top of the defaults: the most machines at once, the time without the person, the floor.

mod common;

use cf_budget::{GIB, Limits, Millis, Refusal, Settings, SettingsError, Size};
use common::*;

fn limited(settings: Settings, limits: Limits) -> Result<Settings, SettingsError> {
    settings.limited(&limits)
}

#[test]
fn no_limits_change_nothing() {
    assert_eq!(limited(dedicated(), Limits::default()), Ok(dedicated()));
    assert_eq!(limited(workstation(), Limits::default()), Ok(workstation()));
}

#[test]
fn the_most_machines_at_once_refuses_the_one_past_it_and_not_before() {
    let settings = limited(
        dedicated(),
        Limits {
            max_machines: Some(2),
            ..Limits::default()
        },
    )
    .unwrap();
    let mut budget = open(settings);
    budget.claim(secs(60), Size::G2).unwrap();
    budget.claim(secs(60), Size::G2).unwrap();
    assert_eq!(
        budget.claim(secs(60), Size::G2),
        Err(Refusal::Machines { cap: 2 })
    );
    // Without the limit the caps let a third one in: 6 of 8 CPUs, 6 of 12 GB.
    let mut free = open(dedicated());
    for _ in 0..3 {
        free.claim(secs(60), Size::G2).unwrap();
    }
}

#[test]
fn the_most_machines_at_once_is_what_side_by_side_says() {
    let settings = dedicated();
    assert_eq!(settings.side_by_side(&settings.away, Size::G2), 4);
    let one = limited(
        settings,
        Limits {
            max_machines: Some(1),
            ..Limits::default()
        },
    )
    .unwrap();
    assert_eq!(one.side_by_side(&one.away, Size::G2), 1);
    assert_eq!(Refusal::Machines { cap: 1 }.name(), "max-jobs");
}

#[test]
fn no_machine_at_all_is_refused() {
    assert_eq!(
        limited(
            dedicated(),
            Limits {
                max_machines: Some(0),
                ..Limits::default()
            }
        ),
        Err(SettingsError::NoMachines)
    );
}

#[test]
fn a_floor_of_eight_percent_lowers_a_dedicated_macs_floor_and_keeps_its_cap() {
    let settings = limited(
        dedicated(),
        Limits {
            floor_percent: Some(8),
            ..Limits::default()
        },
    )
    .unwrap();
    // 8% of 16 GiB: 1.28 GiB, under the default 2 GiB.
    assert_eq!(settings.away.floor_bytes, 1_374_389_534);
    assert_eq!(settings.in_use.floor_bytes, 1_374_389_534);
    assert_eq!(settings.away.cap_bytes, 12 * GIB);
    assert_eq!(settings.vcpus, dedicated().vcpus);
}

#[test]
fn a_floor_under_a_gigabyte_or_outside_one_to_ninety_percent_is_refused() {
    let floor = |percent| {
        limited(
            dedicated(),
            Limits {
                floor_percent: Some(percent),
                ..Limits::default()
            },
        )
    };
    // 6% of 16 GiB is 0.96 GiB, 7% is 1.12 GiB.
    assert_eq!(
        floor(6),
        Err(SettingsError::FloorTooLow {
            profile: "away",
            least_bytes: GIB
        })
    );
    assert!(floor(7).is_ok());
    for percent in [0, 91, u32::MAX] {
        assert_eq!(floor(percent), Err(SettingsError::FloorPercent { percent }));
    }
    assert!(floor(90).is_ok());
}

#[test]
fn a_high_floor_lowers_the_cap_to_what_is_left() {
    let settings = limited(
        dedicated(),
        Limits {
            floor_percent: Some(90),
            ..Limits::default()
        },
    )
    .unwrap();
    let floor = 16 * GIB / 10 * 9 + 16 * GIB % 10 * 9 / 10;
    assert_eq!(settings.away.floor_bytes, floor);
    assert_eq!(settings.away.cap_bytes, 16 * GIB - floor);
    assert!(
        settings.can_run(Size::G2).is_err(),
        "1.6 GiB holds no 2 GB machine"
    );
    // A cf-4g with no room beside it under the lower cap takes all the CPUs it may, at most 4: on a workstation
    // with 6 CPUs for CI 4, not the 3 it gets beside another machine.
    assert_eq!(workstation().vcpus_of(Size::G4), 3);
    let alone = limited(
        workstation(),
        Limits {
            floor_percent: Some(65),
            ..Limits::default()
        },
    )
    .unwrap();
    assert!(alone.away.cap_bytes < Size::G4.vm_bytes() + Size::G2.vm_bytes());
    assert_eq!(alone.vcpus_of(Size::G4), 4);
}

#[test]
fn on_a_workstation_the_floor_while_the_person_is_there_stays_the_higher() {
    let low = limited(
        workstation(),
        Limits {
            floor_percent: Some(8),
            ..Limits::default()
        },
    )
    .unwrap();
    assert_eq!(low.away.floor_bytes, 1_374_389_534);
    assert_eq!(low.in_use.floor_bytes, workstation().in_use.floor_bytes);
    let high = limited(
        workstation(),
        Limits {
            floor_percent: Some(50),
            ..Limits::default()
        },
    )
    .unwrap();
    assert_eq!(high.away.floor_bytes, 8 * GIB);
    assert_eq!(high.in_use.floor_bytes, 8 * GIB);
}

#[test]
fn the_time_without_the_person_is_set_and_never_zero() {
    let five = limited(
        workstation(),
        Limits {
            idle_after: Some(Millis::mins(5)),
            ..Limits::default()
        },
    )
    .unwrap();
    assert_eq!(five.idle_after, Millis::mins(5));
    assert_eq!(
        limited(
            workstation(),
            Limits {
                idle_after: Some(Millis::ZERO),
                ..Limits::default()
            }
        ),
        Err(SettingsError::ZeroDuration {
            what: "the time without the person"
        })
    );
}
