//! Memory for CI (`Limits::ci_bytes`): what a host that runs other work keeps for CI's machines, each counted with
//! its size and its vm-host's overhead. The numbers are the game servers' that take CI jobs while idle: 10.6 GB on
//! the 12 GB hosts, 6.4 GB on the 8 GB one.

use Size::{G2, G4, G6, G8};
use cf_budget::{
    Charge, GIB, HostFacts, Limits, MIB, Profile, Refusal, Settings, SettingsError, Size, admit,
};

const OVERHEAD: u64 = Settings::VM_HOST_OVERHEAD;

/// `ci_memory_gb` as the configuration file reads it: GB of 2^30 bytes, as the sizes are.
fn gb(tenths: u64) -> u64 {
    tenths * GIB / 10
}

fn headless(total_bytes: u64, cores: u32) -> Settings {
    Settings::for_headless(HostFacts { total_bytes, cores }, cores)
}

fn with_ci(settings: Settings, ci_bytes: u64) -> Result<Settings, SettingsError> {
    settings.limited(&Limits {
        ci_bytes: Some(ci_bytes),
        ..Limits::default()
    })
}

/// Whether these sizes fit one after the other on an empty host with all its memory available and CPUs to spare:
/// the memory alone decides.
fn fits(settings: &Settings, sizes: &[Size]) -> Result<(), Refusal> {
    let limits = Profile {
        cpus: 64,
        ..settings.away
    };
    let mut charges = Vec::new();
    for &size in sizes {
        admit(settings, &limits, settings.total_bytes, &charges, size)?;
        charges.push(Charge {
            vm_bytes: size.vm_bytes(),
            host_bytes: settings.host_bytes(size),
            resident_bytes: 0,
            cpus: 0,
        });
    }
    Ok(())
}

#[test]
fn a_twelve_gigabyte_host_with_ten_point_six_runs_the_agreed_mixes_and_not_one_machine_more() {
    let settings = with_ci(headless(12 * GIB, 6), gb(106)).expect("settings");
    assert_eq!(settings.ci_bytes, Some(gb(106)));
    assert_eq!(
        fits(&settings, &[G4, G4, G2]),
        Ok(()),
        "2 GB + 2 × 4 GB and three overheads: 10.56 GB"
    );
    assert_eq!(fits(&settings, &[G6, G4]), Ok(()));
    assert_eq!(fits(&settings, &[G8, G2]), Ok(()));
    for over in [
        &[G4, G4, G2, G2][..],
        &[G6, G4, G2],
        &[G4, G4, G4],
        &[G8, G4],
        &[G6, G6],
    ] {
        assert!(
            matches!(fits(&settings, over), Err(Refusal::Full { .. })),
            "{over:?} is over 10.6 GB: {:?}",
            fits(&settings, over)
        );
    }
}

#[test]
fn an_eight_gigabyte_host_with_six_point_four_runs_a_cf_6g_or_a_cf_4g_with_a_cf_2g() {
    let settings = with_ci(headless(8 * GIB, 4), gb(64)).expect("settings");
    assert_eq!(fits(&settings, &[G6]), Ok(()));
    assert_eq!(fits(&settings, &[G4, G2]), Ok(()));
    for over in [&[G2, G2, G2][..], &[G4, G4], &[G6, G2], &[G8]] {
        assert!(
            matches!(fits(&settings, over), Err(Refusal::Full { .. })),
            "{over:?} is over 6.4 GB: {:?}",
            fits(&settings, over)
        );
    }
}

#[test]
fn every_machine_is_counted_with_its_overhead_to_the_byte() {
    let base = headless(12 * GIB, 6);
    // Exactly two cf-4g and a cf-2g with their overheads fits; a byte less does not.
    let exact = 10 * GIB + 3 * OVERHEAD;
    assert_eq!(
        fits(&with_ci(base.clone(), exact).unwrap(), &[G4, G4, G2]),
        Ok(())
    );
    let short = with_ci(base, exact - 1).unwrap();
    assert_eq!(
        fits(&short, &[G4, G4, G2]),
        Err(Refusal::Full {
            cap_bytes: exact - 1,
            committed_bytes: 8 * GIB + 2 * OVERHEAD
        })
    );
}

#[test]
fn side_by_side_counts_the_overheads_too() {
    let settings = with_ci(headless(12 * GIB, 12), 3 * (2 * GIB + OVERHEAD)).unwrap();
    let wide = Profile {
        cpus: 64,
        ..settings.away
    };
    assert_eq!(settings.side_by_side(&wide, G2), 3);
    let less = with_ci(headless(12 * GIB, 12), 3 * (2 * GIB + OVERHEAD) - 1).unwrap();
    assert_eq!(less.side_by_side(&wide, G2), 2);
}

#[test]
fn the_host_keeps_what_ci_leaves_it_and_never_less_than_the_least_floor() {
    let defaults = headless(12 * GIB, 6);
    let set = with_ci(defaults.clone(), gb(106)).unwrap();
    assert_eq!(
        set.away.floor_bytes,
        defaults.away.floor_bytes.min(12 * GIB - gb(106))
    );
    assert!(set.away.floor_bytes >= Settings::LEAST_FLOOR);
    assert!(set.away.cap_bytes + set.away.floor_bytes <= set.total_bytes);
    // Little for CI keeps the default floor: the floor is never raised by it.
    let little = with_ci(defaults.clone(), 4 * GIB).unwrap();
    assert_eq!(little.away.floor_bytes, defaults.away.floor_bytes);
    assert_eq!(little.away.cap_bytes, 4 * GIB - OVERHEAD);
    // All but half a gigabyte: the host still keeps a whole one, and the cap gives way.
    let greedy = with_ci(defaults, 12 * GIB - 512 * MIB).unwrap();
    assert_eq!(greedy.away.floor_bytes, Settings::LEAST_FLOOR);
    assert_eq!(greedy.away.cap_bytes, 11 * GIB);
}

#[test]
fn a_floor_set_as_a_share_stays_and_the_cap_gives_way() {
    let settings = headless(12 * GIB, 6)
        .limited(&Limits {
            ci_bytes: Some(gb(106)),
            floor_percent: Some(25),
            ..Limits::default()
        })
        .unwrap();
    assert_eq!(settings.away.floor_bytes, 3 * GIB);
    assert_eq!(settings.away.cap_bytes, 9 * GIB);
}

#[test]
fn memory_for_ci_over_the_host_or_under_the_smallest_machine_is_refused() {
    assert_eq!(
        with_ci(headless(8 * GIB, 4), 8 * GIB + 1),
        Err(SettingsError::CiOverHost {
            ci_bytes: 8 * GIB + 1,
            total_bytes: 8 * GIB
        })
    );
    let least = 2 * GIB + OVERHEAD;
    assert_eq!(
        with_ci(headless(8 * GIB, 4), least - 1),
        Err(SettingsError::CiUnderSmallest {
            ci_bytes: least - 1,
            least_bytes: least
        })
    );
    assert!(with_ci(headless(8 * GIB, 4), least).is_ok());
    assert!(matches!(
        with_ci(headless(8 * GIB, 4), 0),
        Err(SettingsError::CiUnderSmallest { .. })
    ));
}

#[test]
fn none_set_changes_nothing() {
    let defaults = headless(12 * GIB, 6);
    assert_eq!(defaults.clone().limited(&Limits::default()), Ok(defaults.clone()));
    assert_eq!(defaults.ci_bytes, None);
}

fn limited(
    settings: Settings,
    ci_bytes: Option<u64>,
    ci_cpus: Option<u32>,
) -> Result<Settings, SettingsError> {
    settings.limited(&Limits {
        ci_bytes,
        ci_cpus,
        ..Limits::default()
    })
}

/// Whether these sizes start one after the other on an empty host, its CPUs counted as the settings give them.
fn starts(settings: &Settings, sizes: &[Size]) -> Result<(), Refusal> {
    let mut charges = Vec::new();
    for &size in sizes {
        admit(settings, &settings.away, settings.total_bytes, &charges, size)?;
        charges.push(Charge {
            vm_bytes: size.vm_bytes(),
            host_bytes: settings.host_bytes(size),
            resident_bytes: 0,
            cpus: settings.vcpus_of(size),
        });
    }
    Ok(())
}

#[test]
fn a_six_core_game_host_runs_the_agreed_mixes_with_every_core_and_two_vcpus_a_cf_4g() {
    // The defaults keep two cores for the host; every core still gives a cf-4g 3 vCPUs, so two of them fill it.
    for ci_cpus in [None, Some(6)] {
        let settings = limited(headless(12 * GIB, 6), Some(gb(106)), ci_cpus).unwrap();
        assert!(
            matches!(starts(&settings, &[G4, G4, G2]), Err(Refusal::Cpus { .. })),
            "{ci_cpus:?}"
        );
    }
    let set = headless(12 * GIB, 6)
        .limited(&Limits {
            ci_bytes: Some(gb(106)),
            ci_cpus: Some(6),
            vcpus: [None, Some(2), None, None],
            ..Limits::default()
        })
        .unwrap();
    assert_eq!(set.away.cpus, 6);
    for mix in [&[G4, G4, G2][..], &[G6, G4], &[G8, G2]] {
        assert_eq!(starts(&set, mix), Ok(()), "{mix:?}");
    }
    assert!(starts(&set, &[G4, G4, G2, G2]).is_err());
    // The 8 GB host on its 4 cores: a cf-4g beside a cf-2g, or a cf-6g.
    let small = limited(headless(8 * GIB, 4), Some(gb(64)), Some(4)).unwrap();
    assert_eq!(starts(&small, &[G4, G2]), Ok(()));
    assert_eq!(starts(&small, &[G6]), Ok(()));
}

#[test]
fn fewer_vcpus_for_a_size_leave_memory_for_ci_whole() {
    // Memory for CI counts each machine's overhead as it starts: fewer vCPUs take nothing more off the cap.
    let settings = headless(12 * GIB, 6)
        .limited(&Limits {
            ci_bytes: Some(gb(106)),
            ci_cpus: Some(6),
            vcpus: [Some(1), Some(1), None, None],
            ..Limits::default()
        })
        .unwrap();
    assert_eq!(settings.away.cap_bytes, gb(106) - OVERHEAD);
    assert_eq!(starts(&settings, &[G4, G4, G2]), Ok(()));
}

#[test]
fn cpus_for_ci_of_none_or_more_than_the_host_has_are_refused() {
    for cpus in [0, 7] {
        assert_eq!(
            limited(headless(12 * GIB, 6), None, Some(cpus)),
            Err(SettingsError::CiCpus { cpus, cores: 6 })
        );
    }
    let fewer = limited(headless(12 * GIB, 6), None, Some(2)).unwrap();
    assert_eq!((fewer.away.cpus, fewer.in_use.cpus), (2, 2));
    assert!(matches!(starts(&fewer, &[G2, G2]), Err(Refusal::Cpus { .. })));
}
