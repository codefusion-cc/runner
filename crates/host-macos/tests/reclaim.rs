//! A calm Mac counts what compressing its programs' inactive pages frees; a Mac under pressure does not. Numbers
//! from an idle 16 GB Mac mini that refused 4 GB jobs for hours with 5.3 GiB available (`vm_stat`, `sysctl`).

use cf_budget::{
    Budget, GIB, HostError, HostFacts, HostReading, Millis, Mode, Now, Pressure, Refusal, Settings, Size,
    Trouble, admit,
};
use cf_host_macos::{CALM_LEVEL, Raw, reading};
use proptest::prelude::*;

const PAGE: u64 = 16384;
const FACTS: HostFacts = HostFacts {
    total_bytes: 16 * GIB,
    cores: 8,
};

/// Free (61972), file-backed (268905) and purgeable (18337) pages: 349214, 5.33 GiB.
const READY_PAGES: u64 = 349_214;

/// The idle Mac mini: `kern.memorystatus_level` 76, pressure normal, 247 MiB of swap that does not grow.
fn idle_mini() -> Raw {
    Raw {
        page_bytes: PAGE,
        free_pages: 61_972 + 16_322,
        speculative_pages: 16_322,
        file_backed_pages: 268_905,
        purgeable_pages: 18_337,
        active_pages: 354_828,
        inactive_pages: 339_413,
        wired_pages: 173_753,
        anonymous_pages: 441_658,
        compressor_pages: 68_977,
        compressed_pages: 202_918,
        swapouts: 4_210_118,
        pressure_level: 1,
        memorystatus_level: Some(76),
        swap_used_bytes: 259_391_488,
        swap_total_bytes: GIB,
        idle_nanos: None,
        thermal_level: 0,
        power_remaining_secs: -2.0,
        disk_free_bytes: 75 * GIB,
        disk_important_bytes: None,
    }
}

fn at(second: u64) -> Now {
    Now {
        awake: Millis::secs(second),
        elapsed: Millis::secs(second),
    }
}

fn read(raw: &Raw) -> Result<HostReading, HostError> {
    reading(FACTS, at(0), raw)
}

fn available(raw: &Raw) -> u64 {
    read(raw).unwrap().available_bytes
}

/// What a dedicated 16 GB Mac needs free for one more 4 GB machine: its size, its overhead and the floor.
fn needed_for_a_4g() -> u64 {
    let settings = Settings::for_host(FACTS, Mode::Dedicated);
    settings.host_bytes(Size::G4) + settings.away.floor_bytes
}

fn admits_a_4g(available_bytes: u64) -> Result<(), Refusal> {
    let settings = Settings::for_host(FACTS, Mode::Dedicated);
    admit(&settings, &settings.away, available_bytes, &[], Size::G4)
}

/// A dedicated Mac's budget after a minute and a second of readings, one a second, and whether it starts a 4 GB
/// machine then.
fn budget_after_a_minute(raw_at: impl Fn(u64) -> Raw) -> Result<(), Refusal> {
    let mut budget = Budget::new(Settings::for_host(FACTS, Mode::Dedicated)).unwrap();
    for second in 0..=61 {
        let now = at(second);
        budget.observe(now, Some(reading(FACTS, now, &raw_at(second)).unwrap()));
    }
    budget.fits(at(61), Size::G4)
}

#[test]
fn the_idle_mini_counts_its_inactive_program_pages_at_what_compressing_them_frees() {
    // Inactive pages split as file pages share the active and inactive ones: 252583 file pages that are not
    // speculative, of 694241, puts 123487 file pages and 215926 program pages in the 339413 inactive ones. The
    // compressor stores 202918 pages in 68977: compressing frees 133941 of every 202918, 142527 pages.
    assert_eq!(available(&idle_mini()), (READY_PAGES + 142_527) * PAGE);
    assert_eq!(needed_for_a_4g(), 405_504 * PAGE, "6.19 GiB");
    assert_eq!(admits_a_4g(available(&idle_mini())), Ok(()));
    assert_eq!(budget_after_a_minute(|_| idle_mini()), Ok(()));
}

#[test]
fn without_its_inactive_program_pages_the_mini_refuses_a_4g_as_it_did() {
    let strict = READY_PAGES * PAGE;
    assert_eq!(
        admits_a_4g(strict),
        Err(Refusal::Memory {
            short_bytes: (405_504 - READY_PAGES) * PAGE
        })
    );
}

#[test]
fn under_a_pressure_warning_or_critical_only_the_strict_count_is_left() {
    for level in [2, 4] {
        let raw = Raw {
            pressure_level: level,
            ..idle_mini()
        };
        assert_eq!(available(&raw), READY_PAGES * PAGE, "pressure level {level}");
        assert!(admits_a_4g(available(&raw)).is_err());
        assert_eq!(
            budget_after_a_minute(|_| raw),
            Err(Refusal::Trouble(Trouble::Pressure))
        );
    }
}

#[test]
fn a_low_memorystatus_level_counts_nothing_more_and_the_calm_level_does() {
    let low = Raw {
        memorystatus_level: Some(CALM_LEVEL - 1),
        ..idle_mini()
    };
    assert_eq!(available(&low), READY_PAGES * PAGE);
    assert_eq!(
        budget_after_a_minute(|_| low),
        Err(Refusal::Memory {
            short_bytes: (405_504 - READY_PAGES) * PAGE
        })
    );
    let calm = Raw {
        memorystatus_level: Some(CALM_LEVEL),
        ..idle_mini()
    };
    assert_eq!(available(&calm), (READY_PAGES + 142_527) * PAGE);
    assert_eq!(budget_after_a_minute(|_| calm), Ok(()));
}

#[test]
fn a_compressor_with_a_quarter_of_the_memory_counts_nothing_more() {
    let quarter = FACTS.total_bytes / 4 / PAGE;
    let heavy = Raw {
        compressor_pages: quarter,
        compressed_pages: 3 * quarter,
        ..idle_mini()
    };
    assert_eq!(available(&heavy), READY_PAGES * PAGE);
    let just_under = Raw {
        compressor_pages: quarter - 1,
        ..heavy
    };
    assert!(available(&just_under) > READY_PAGES * PAGE);
}

#[test]
fn swap_that_moves_refuses_the_job_whatever_is_counted() {
    let swapping = budget_after_a_minute(|second| Raw {
        swapouts: idle_mini().swapouts + second,
        swap_used_bytes: idle_mini().swap_used_bytes + second * 4 * PAGE,
        ..idle_mini()
    });
    assert_eq!(swapping, Err(Refusal::Trouble(Trouble::Swapping)));
}

#[test]
fn without_a_memorystatus_level_nothing_more_is_counted() {
    let unknown = Raw {
        memorystatus_level: None,
        ..idle_mini()
    };
    assert_eq!(available(&unknown), READY_PAGES * PAGE);
}

#[test]
fn a_memorystatus_level_over_a_hundred_is_an_error_and_a_hundred_is_not() {
    for level in [101, u32::MAX] {
        let raw = Raw {
            memorystatus_level: Some(level),
            ..idle_mini()
        };
        assert_eq!(
            read(&raw),
            Err(HostError::Implausible {
                what: "kern.memorystatus_level",
                value: level.to_string()
            })
        );
    }
    let full = Raw {
        memorystatus_level: Some(100),
        ..idle_mini()
    };
    assert_eq!(available(&full), (READY_PAGES + 142_527) * PAGE);
}

#[test]
fn the_count_never_passes_what_memorystatus_says_holds_nothing_compressed() {
    // 50% of 16 GiB is 8 GiB: 524288 pages, of which 349214 are counted already.
    let raw = Raw {
        memorystatus_level: Some(50),
        inactive_pages: 2_000_000,
        anonymous_pages: 2_000_000,
        ..idle_mini()
    };
    assert_eq!(available(&raw), 8 * GIB);
    // A level under the strict count takes nothing from it.
    let below = Raw {
        memorystatus_level: Some(CALM_LEVEL),
        free_pages: 600_000,
        speculative_pages: 0,
        ..raw
    };
    assert_eq!(
        available(&below),
        (600_000 + 268_905 + 18_337) * PAGE,
        "more than 50% counted strictly"
    );
}

/// A Mac whose only memory to count is `free` free pages and `inactive` inactive program pages, with an empty
/// compressor.
fn plain(free: u64, inactive: u64) -> Raw {
    Raw {
        free_pages: free,
        speculative_pages: 0,
        file_backed_pages: 0,
        purgeable_pages: 0,
        active_pages: 0,
        inactive_pages: inactive,
        anonymous_pages: inactive,
        compressor_pages: 0,
        compressed_pages: 0,
        memorystatus_level: Some(100),
        ..idle_mini()
    }
}

#[test]
fn an_empty_compressor_counts_half_of_each_inactive_program_page() {
    assert_eq!(available(&plain(1000, 2000)), 2000 * PAGE);
    assert_eq!(available(&plain(1000, 1)), 1000 * PAGE, "half a page is none");
}

#[test]
fn at_exactly_the_floor_a_4g_starts_and_a_page_under_it_does_not() {
    // 305504 free pages and half of 200000 inactive program pages: 405504, the 6.19 GiB a 4 GB machine needs.
    let exactly = available(&plain(305_504, 200_000));
    assert_eq!(exactly, needed_for_a_4g());
    assert_eq!(admits_a_4g(exactly), Ok(()));
    let under = available(&plain(305_503, 200_000));
    assert_eq!(admits_a_4g(under), Err(Refusal::Memory { short_bytes: PAGE }));
    assert_eq!(budget_after_a_minute(|_| plain(305_504, 200_000)), Ok(()));
}

#[test]
fn zero_counts_give_zero_and_no_error() {
    let nothing = Raw {
        free_pages: 0,
        speculative_pages: 0,
        file_backed_pages: 0,
        purgeable_pages: 0,
        active_pages: 0,
        inactive_pages: 0,
        anonymous_pages: 0,
        compressor_pages: 0,
        compressed_pages: 0,
        memorystatus_level: Some(0),
        ..idle_mini()
    };
    assert_eq!(available(&nothing), 0);
    let no_queues = Raw {
        memorystatus_level: Some(100),
        free_pages: 10,
        ..nothing
    };
    assert_eq!(available(&no_queues), 10 * PAGE);
}

#[test]
fn a_compressor_that_saves_nothing_or_less_than_nothing_frees_nothing() {
    for (stored, occupied) in [(100_000, 100_000), (50_000, 60_000)] {
        let raw = Raw {
            compressed_pages: stored,
            compressor_pages: occupied,
            ..idle_mini()
        };
        assert_eq!(available(&raw), READY_PAGES * PAGE, "{stored} in {occupied}");
    }
}

#[test]
fn a_compressor_counts_at_most_three_quarters_of_a_page() {
    // Zero pages compress to almost nothing: 1000000 pages in 1000 would say every page frees all of itself.
    let raw = Raw {
        compressed_pages: 1_000_000,
        compressor_pages: 1000,
        ..plain(0, 4000)
    };
    assert_eq!(available(&raw), 3000 * PAGE);
}

#[test]
fn inactive_pages_beyond_the_programs_own_are_not_counted_as_theirs() {
    // All 4000 inactive pages, but only 1000 program pages, 400 of them purgeable and counted already.
    let raw = Raw {
        anonymous_pages: 1000,
        purgeable_pages: 400,
        ..plain(0, 4000)
    };
    assert_eq!(available(&raw), (400 + 300) * PAGE);
}

fn any_raw() -> impl Strategy<Value = Raw> {
    let pages = 0u64..2_000_000;
    (
        (pages.clone(), pages.clone(), pages.clone(), pages.clone()),
        (pages.clone(), pages.clone(), pages.clone(), pages.clone(), pages),
        (
            prop::option::of(0u32..=100),
            prop_oneof![Just(1), Just(2), Just(4)],
        ),
    )
        .prop_map(
            |(
                (free, speculative, file, purgeable),
                (active, inactive, anonymous, occupied, stored),
                (level, pressure),
            )| Raw {
                free_pages: free / 8,
                speculative_pages: speculative / 8,
                file_backed_pages: file / 8,
                purgeable_pages: purgeable / 8,
                active_pages: active,
                inactive_pages: inactive,
                anonymous_pages: anonymous,
                compressor_pages: occupied,
                compressed_pages: stored,
                memorystatus_level: level,
                pressure_level: pressure,
                ..idle_mini()
            },
        )
}

proptest! {
    #[test]
    fn the_count_is_never_under_the_strict_one_and_never_over_the_mac(raw in any_raw()) {
        let strict = Raw { pressure_level: 2, ..raw };
        if let (Ok(counted), Ok(strict)) = (read(&raw), read(&strict)) {
            prop_assert!(counted.available_bytes >= strict.available_bytes);
            prop_assert!(counted.available_bytes <= FACTS.total_bytes);
            let calm = raw.pressure_level == 1 && raw.memorystatus_level.is_some_and(|level| level >= CALM_LEVEL);
            if !calm {
                prop_assert_eq!(counted.available_bytes, strict.available_bytes);
            }
            prop_assert_eq!(counted.pressure == Pressure::Normal, raw.pressure_level == 1);
        }
    }
}
