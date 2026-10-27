//! The defaults for a Linux host nobody works at (`Settings::for_headless`): a reserve by what the host needs, the
//! cap that keeps every machine's overhead inside it, and the fast cores of a board whose cores differ.

mod common;

use Size::{G2, G4, G6, G8};
use cf_budget::{Budget, Charge, GIB, HostFacts, Limits, MIB, Mode, Refusal, Settings, Size, admit};
use common::*;
use proptest::prelude::*;

const OVERHEAD: u64 = Settings::VM_HOST_OVERHEAD;

/// `MemTotal` of an RK3588 board sold as 8 GB: 8129820 kB, 7.75 GiB.
const BOARD_BYTES: u64 = 8_129_820 * 1024;

fn host(total_bytes: u64, cores: u32) -> HostFacts {
    HostFacts { total_bytes, cores }
}

/// The RK3588 board: 8 cores, the 4 fast ones for CI.
fn board() -> Settings {
    Settings::for_headless(host(BOARD_BYTES, 8), 4)
}

/// What a budget that has all the host's memory available grants, size by size, in this order.
fn grants(settings: Settings, sizes: &[Size]) -> Vec<Result<u32, Refusal>> {
    let total = settings.total_bytes;
    let mut budget = Budget::new(settings).expect("settings the budget takes");
    watch(&mut budget, 0, 60, |reading| reading.available_bytes = total);
    sizes
        .iter()
        .map(|size| budget.claim(secs(60), *size).map(|grant| grant.cpus))
        .collect()
}

#[test]
fn the_board_runs_one_cf_4g_or_two_cf_2g_and_nothing_more() {
    let settings = board();
    assert_eq!(settings.check(), Ok(()));
    assert_eq!(settings.mode, Mode::Dedicated);
    assert_eq!(
        settings.in_use, settings.away,
        "nobody works here: one set of limits"
    );
    let cap = settings.away.cap_bytes;
    // 7.75 GB less a floor of 1.24 GB and 0.5 GB for the host: 6.0 GB of room, less two machines' overhead.
    assert_eq!(settings.away.floor_bytes, GIB + BOARD_BYTES / 32);
    assert_eq!(
        cap,
        BOARD_BYTES - (GIB + BOARD_BYTES / 32) - GIB / 2 - 2 * OVERHEAD
    );
    assert_eq!(cap / MIB, 5771, "5.63 GB");
    // Nothing fits beside a cf-4g, so it has all four fast cores: with two, the other two would stand idle.
    assert_eq!((settings.away.cpus, settings.vcpus), (4, [2, 4, 4, 4]));

    assert_eq!(grants(board(), &[G2, G2]), [Ok(2), Ok(2)]);
    assert_eq!(grants(board(), &[G4]), [Ok(4)]);
    assert_eq!(
        grants(board(), &[G4, G2]),
        [
            Ok(4),
            Err(Refusal::Full {
                cap_bytes: cap,
                committed_bytes: 4 * GIB
            })
        ]
    );
    assert_eq!(
        grants(board(), &[G2, G4])[1],
        Err(Refusal::Full {
            cap_bytes: cap,
            committed_bytes: 2 * GIB
        })
    );
    // A third cf-2g has neither the cap nor the cores.
    assert_eq!(
        grants(board(), &[G2, G2, G2])[2],
        Err(Refusal::Full {
            cap_bytes: cap,
            committed_bytes: 4 * GIB
        })
    );
    // With one vCPU each the cores would take a third; the cap still does not, since the overhead it keeps room
    // for is that of two machines.
    let mut one_each = board();
    one_each.vcpus = [1; 4];
    assert!(matches!(
        grants(one_each, &[G2, G2, G2])[2],
        Err(Refusal::Full { .. })
    ));
    for size in [G2, G4] {
        assert_eq!(settings.can_run(size), Ok(()), "{size:?}");
    }
    for size in [G6, G8] {
        assert!(
            matches!(settings.can_run(size), Err(Refusal::Full { .. })),
            "{size:?} is never offered from the board"
        );
    }
    assert_eq!(settings.side_by_side(&settings.away, G2), 2);
    assert_eq!(settings.side_by_side(&settings.away, G4), 1);
    assert_eq!(settings.side_by_side(&settings.away, G6), 0);
}

#[test]
fn the_board_at_idle_has_the_memory_for_what_its_cap_allows() {
    // 7.36 GB available with nothing running (MemAvailable on the board, 7712136 kB).
    let idle = 7_712_136 * 1024;
    for sizes in [&[G2, G2][..], &[G4][..]] {
        let mut budget = Budget::new(board()).unwrap();
        watch(&mut budget, 0, 60, |reading| reading.available_bytes = idle);
        for size in sizes {
            assert!(budget.claim(secs(60), *size).is_ok(), "{sizes:?}");
        }
    }
    // A host whose own programs grew by 2 GB keeps its floor: the second cf-2g waits.
    let mut budget = Budget::new(board()).unwrap();
    watch(&mut budget, 0, 60, |reading| {
        reading.available_bytes = idle - 2 * GIB
    });
    budget.claim(secs(60), G2).unwrap();
    assert!(matches!(budget.claim(secs(60), G2), Err(Refusal::Memory { .. })));
}

#[test]
fn the_rule_at_each_size_of_host() {
    // (memory, cores, fast cores) -> (cap, floor, CPUs, vCPUs of cf-4g), and which sizes it offers at all.
    let table = [
        // A 4 GB board with four cores alike: one cf-2g.
        (
            4 * GIB,
            4,
            4,
            (2 * GIB + 3 * GIB / 16, GIB + GIB / 8, 2, 2),
            [true, false, false, false],
        ),
        (
            BOARD_BYTES,
            8,
            4,
            (
                BOARD_BYTES - GIB - BOARD_BYTES / 32 - GIB / 2 - 2 * OVERHEAD,
                GIB + BOARD_BYTES / 32,
                4,
                4,
            ),
            [true, true, false, false],
        ),
        (
            8 * GIB,
            8,
            4,
            (5 * GIB + 7 * GIB / 8, GIB + GIB / 4, 4, 4),
            [true, true, false, false],
        ),
        // 16 GB on the same kind of board: the four fast cores decide, not the memory, and a cf-4g leaves two of
        // them to the machine beside it.
        (
            16 * GIB,
            8,
            4,
            (13 * GIB + 5 * GIB / 8, GIB + GIB / 2, 4, 2),
            [true, true, true, true],
        ),
        // A 16 GB cloud VM with eight vCPUs alike: all but two for CI, and the three cf-4g its memory holds share
        // them, 2 vCPUs each, rather than two of 3 leaving 4 GB idle.
        (
            16 * GIB,
            8,
            0,
            (13 * GIB + 7 * GIB / 16, GIB + GIB / 2, 6, 2),
            [true, true, true, true],
        ),
        // 32 GB with sixteen cores alike: all but two for CI, seven machines of 2 vCPUs side by side, cf-4g too.
        (
            32 * GIB,
            16,
            0,
            (28 * GIB + 3 * GIB / 16, 2 * GIB, 14, 2),
            [true, true, true, true],
        ),
    ];
    for (total, cores, fast, (cap, floor, cpus, g4), offered) in table {
        let settings = Settings::for_headless(host(total, cores), fast);
        let what = format!("{} MiB, {cores} cores, {fast} fast", total / MIB);
        assert_eq!(settings.check(), Ok(()), "{what}");
        assert_eq!(
            (
                settings.away.cap_bytes,
                settings.away.floor_bytes,
                settings.away.cpus
            ),
            (cap, floor, cpus),
            "{what}"
        );
        assert_eq!(settings.vcpus, [2, g4, 4, 4], "{what}");
        assert_eq!(
            Size::ALL.map(|size| settings.can_run(size).is_ok()),
            offered,
            "{what}"
        );
        // What a dedicated Mac keeps is not what a headless host keeps: the reserve is smaller.
        let mac = Settings::for_host(host(total, cores), Mode::Dedicated);
        assert!(settings.away.cap_bytes > mac.away.cap_bytes, "{what}");
    }
}

#[test]
fn a_cf_6g_and_a_cf_4g_beside_a_cf_2g_come_at_8_13_gb() {
    // Four fast cores, two machines' overhead: the cap reaches 6 GB where 31/32 of the memory is 7.875 GB.
    let at = |mib: u64| Settings::for_headless(host(mib * MIB, 8), 4);
    let below = at(8323);
    let above = at(8325);
    assert!(below.away.cap_bytes < 6 * GIB && above.away.cap_bytes >= 6 * GIB);
    assert!(matches!(below.can_run(G6), Err(Refusal::Full { .. })));
    assert_eq!(above.can_run(G6), Ok(()));
    // Below, a cf-4g runs alone and takes the four fast cores; above, it leaves two to a cf-2g beside it.
    assert_eq!(grants(below.clone(), &[G4]), [Ok(4)]);
    assert!(grants(below, &[G4, G2])[1].is_err());
    assert_eq!(grants(above, &[G4, G2]), [Ok(2), Ok(2)]);
}

#[test]
fn the_smallest_host_that_runs_a_cf_2g_has_3_81_gb() {
    // Four cores alike: two for CI, one machine. 31/32 of the memory must cover 2 GB, its overhead, 1 GB of floor
    // and the host's 0.5 GB.
    let at = |mib: u64| Settings::for_headless(host(mib * MIB, 4), 0);
    assert!(matches!(at(3897).can_run(G2), Err(Refusal::Full { .. })));
    assert_eq!(at(3898).can_run(G2), Ok(()));
    // A host this small is still one the budget takes: it opens nothing.
    for mib in [1100, 2048, 3897] {
        assert_eq!(at(mib).check(), Ok(()), "{mib} MiB");
    }
    // Under 1 GB and its 1/32 no floor fits at all, and the budget refuses the settings.
    assert!(at(1024).check().is_err());
}

#[test]
fn ci_takes_the_fast_cores_of_a_board_whose_cores_differ_and_all_but_two_of_one_whose_do_not() {
    let cpus = |cores: u32, fast: u32| Settings::for_headless(host(16 * GIB, cores), fast).away.cpus;
    assert_eq!(cpus(8, 4), 4, "four fast cores, four slow ones for the host");
    assert_eq!(cpus(8, 2), 2);
    // One prime core of its own design holds no machine of 2 vCPUs: the host counts as one whose cores are alike.
    assert_eq!(cpus(8, 1), 6);
    assert_eq!(cpus(3, 1), 1);
    assert_eq!(cpus(3, 2), 2);
    // All alike, or no word of which are fast: all but two.
    assert_eq!(cpus(8, 8), 6);
    assert_eq!(cpus(8, 0), 6);
    // More fast cores than the host has is no answer to trust.
    assert_eq!(cpus(8, 9), 6);
    // One or two cores alike still give CI one, and never more than there are.
    assert_eq!(cpus(2, 0), 1);
    assert_eq!(cpus(1, 0), 1);
    assert_eq!(cpus(1, 1), 1);
    // With room for a machine beside it, cf-4g gets half the CPUs, between 2 and 4, and no more than its share
    // among the three cf-4g 16 GB holds.
    let g4 = |cores: u32, fast: u32| Settings::for_headless(host(16 * GIB, cores), fast).vcpus_of(G4);
    assert_eq!(
        [g4(8, 4), g4(8, 0), g4(12, 0), g4(24, 0), g4(4, 0)],
        [2, 2, 3, 4, 2]
    );
}

proptest! {
    /// Whatever the host, the machines its cap and CPUs let in, each with its overhead, and the floor and the
    /// host's own share fit its memory; and the budget takes the settings for any host of 1.1 GB or more.
    #[test]
    fn every_machine_the_rule_lets_in_fits_with_its_overhead(
        total_mib in 1100..96 * 1024u64,
        cores in 1..64u32,
        fast in 0..72u32,
        sizes in prop::collection::vec(prop::sample::select(Size::ALL.to_vec()), 0..40),
    ) {
        let settings = Settings::for_headless(host(total_mib * MIB, cores), fast);
        prop_assert_eq!(settings.check(), Ok(()));
        let limits = settings.away;
        let mut charges: Vec<Charge> = Vec::new();
        for size in sizes {
            if admit(&settings, &limits, settings.total_bytes, &charges, size).is_ok() {
                charges.push(Charge {
                    vm_bytes: size.vm_bytes(),
                    host_bytes: settings.host_bytes(size),
                    resident_bytes: 0,
                    cpus: settings.vcpus_of(size),
                });
            }
            let held: u64 = charges.iter().map(|charge| charge.host_bytes).sum();
            // A host too small for its own share and the floor lets nothing in.
            prop_assert!(
                held == 0 || held + limits.floor_bytes + Settings::HEADLESS_HOST_USE <= settings.total_bytes,
                "{} MiB held of {} MiB", held / MIB, total_mib
            );
            prop_assert!(charges.iter().map(|charge| charge.cpus).sum::<u32>() <= limits.cpus);
            prop_assert!(limits.cpus <= cores);
        }
    }

    /// A cf-4g leaves the CPUs a machine beside it would need when the cap has room for one, and takes up to 4 of
    /// them when it has not, since no other machine could use them.
    #[test]
    fn a_cf_4g_leaves_idle_only_cpus_another_machine_could_use(
        total_mib in 1100..96 * 1024u64,
        cores in 1..64u32,
        fast in 0..72u32,
    ) {
        let settings = Settings::for_headless(host(total_mib * MIB, cores), fast);
        let limits = settings.away;
        prop_assume!(settings.can_run(G4).is_ok());
        let g4 = Charge {
            vm_bytes: G4.vm_bytes(),
            host_bytes: settings.host_bytes(G4),
            resident_bytes: 0,
            cpus: settings.vcpus_of(G4),
        };
        let beside = admit(&settings, &limits, settings.total_bytes, &[g4], G2);
        // Whether the memory has room for a cf-2g beside it, whatever CPUs the cf-4g holds.
        let room_beside = admit(&settings, &limits, settings.total_bytes, &[Charge { cpus: 0, ..g4 }], G2).is_ok();
        if room_beside && limits.cpus >= 4 {
            prop_assert_eq!(beside, Ok(()), "{} MiB, {} CPUs", total_mib, limits.cpus);
        }
        if !room_beside {
            prop_assert!(beside.is_err());
            prop_assert_eq!(settings.vcpus_of(G4), limits.cpus.min(4));
        }
    }
}

#[test]
fn a_16_gb_cloud_vm_runs_three_cf_4g_and_keeps_them_under_its_own_floor() {
    let settings = Settings::for_headless(host(16 * GIB, 8), 0);
    assert_eq!(settings.side_by_side(&settings.away, G4), 3);
    // The owner's floor lowers the cap; the cf-4g still share the CPUs by what the memory holds.
    let floor = Limits {
        floor_percent: Some(20),
        ..Limits::default()
    };
    let lowered = settings.limited(&floor).expect("a floor in range");
    assert_eq!(
        lowered.away.cap_bytes,
        16 * GIB - cf_budget::percent_of(16 * GIB, 20)
    );
    assert_eq!(lowered.vcpus, [2, 2, 4, 4]);
    assert_eq!(lowered.side_by_side(&lowered.away, G4), 3);
}

#[test]
fn the_owner_sets_a_sizes_vcpus_and_the_machines_side_by_side_follow() {
    let settings = Settings::for_headless(host(16 * GIB, 8), 0);
    assert_eq!(settings.away.cpus, 6);
    assert_eq!(
        (
            settings.side_by_side(&settings.away, G4),
            settings.side_by_side(&settings.away, G6)
        ),
        (3, 1)
    );
    let three = |sizes: &[Size]| {
        let mut limits = Limits::default();
        for size in sizes {
            limits.vcpus[size.index()] = Some(3);
        }
        settings.clone().limited(&limits)
    };
    let set = three(&[G4, G6]).expect("3 of 6 CPUs");
    assert_eq!(set.vcpus, [2, 3, 3, 4], "only the sizes named change");
    assert_eq!(
        set.side_by_side(&set.away, G4),
        2,
        "two cf-4g of 3, not three of 2"
    );
    assert_eq!(
        set.side_by_side(&set.away, G6),
        2,
        "two cf-6g of 3 fit the memory and the CPUs"
    );
    let granted = grants(set, &[G4, G4, G4]);
    assert_eq!(granted[..2], [Ok(3), Ok(3)], "each cf-4g gets its 3 vCPUs");
    assert!(granted[2].is_err(), "a third cf-4g of 3 vCPUs on 6");
    // A floor set beside them recomputes the defaults, not what the owner set.
    let mut limits = Limits {
        floor_percent: Some(20),
        ..Limits::default()
    };
    limits.vcpus[G4.index()] = Some(3);
    assert_eq!(settings.clone().limited(&limits).unwrap().vcpus[G4.index()], 3);
}

#[test]
fn a_sizes_vcpus_are_at_least_one_and_no_more_than_ci_may_use() {
    let settings = Settings::for_headless(host(16 * GIB, 8), 0);
    for (vcpus, ok) in [(0, false), (1, true), (6, true), (7, false), (u32::MAX, false)] {
        let mut limits = Limits::default();
        limits.vcpus[G4.index()] = Some(vcpus);
        assert_eq!(settings.clone().limited(&limits).is_ok(), ok, "{vcpus} vCPUs");
    }
}
