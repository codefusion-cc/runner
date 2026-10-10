//! The arithmetic between macOS's answers and the budget's reading, on numbers taken from three real 16 GB Macs
//! (`vm_stat`, `sysctl`, `ioreg`, `pmset`), and on answers that cannot be right.

use cf_budget::{GIB, HostError, HostFacts, HostReading, Millis, Now, Pressure, Thermal};
use cf_host_macos::{Raw, reading};

const FACTS: HostFacts = HostFacts {
    total_bytes: 16 * GIB,
    cores: 8,
};
const AT: Now = Now {
    awake: Millis(5000),
    elapsed: Millis(9000),
};

/// A laptop in use: 8.35 of 9.2 GB of swap used, pressure at warning, a key pressed a third of a second ago.
fn laptop() -> Raw {
    Raw {
        page_bytes: 16384,
        free_pages: 7372 + 4508,
        speculative_pages: 4508,
        file_backed_pages: 123_334,
        purgeable_pages: 84,
        active_pages: 177_832,
        inactive_pages: 172_763,
        wired_pages: 212_844,
        anonymous_pages: 231_769,
        compressor_pages: 433_623,
        swapouts: 40_584_774,
        pressure_level: 2,
        swap_used_bytes: 8_756_264_960,
        swap_total_bytes: 9_663_676_416,
        idle_nanos: Some(320_528_083),
        thermal_level: 0,
        power_remaining_secs: -2.0,
        disk_free_bytes: 1_893_123_923_968,
    }
}

fn read(raw: &Raw) -> Result<HostReading, HostError> {
    reading(FACTS, AT, raw)
}

#[test]
fn a_laptop_under_pressure_reads_as_vm_stat_and_sysctl_show_it() {
    let reading = read(&laptop()).unwrap();
    assert_eq!(
        reading,
        HostReading {
            at: AT,
            // vm_stat: (7372 free + 123334 file-backed + 84 purgeable) pages of 16384 bytes.
            available_bytes: 130_790 * 16384,
            pressure: Pressure::Warning,
            swap_used_bytes: 8_756_264_960,
            swapouts: 40_584_774,
            idle: Millis(320),
            thermal: Thermal::Nominal,
            on_battery: false,
            disk_free_bytes: 1_893_123_923_968,
        }
    );
}

#[test]
fn a_ci_mac_with_a_ten_gigabyte_vm_has_under_three_gigabytes_available() {
    // The dedicated Mac while the old 8 GB container VM ran: vm_stat showed 5933 free, 2648 speculative,
    // 163248 file-backed and 6614 purgeable pages, and kern.memorystatus_level said 52% "free".
    let raw = Raw {
        free_pages: 5933 + 2648,
        speculative_pages: 2648,
        file_backed_pages: 163_248,
        purgeable_pages: 6614,
        pressure_level: 1,
        idle_nanos: Some(2_461_339_384_000),
        ..laptop()
    };
    let reading = read(&raw).unwrap();
    assert_eq!(reading.available_bytes, 175_795 * 16384);
    assert!(reading.available_bytes < 3 * GIB, "52% of 16 GB would be 8.3 GB");
    assert_eq!(reading.pressure, Pressure::Normal);
    assert_eq!(reading.idle, Millis(2_461_339));
}

#[test]
fn a_desktop_nobody_touched_for_seven_hours() {
    let raw = Raw {
        free_pages: 4130 + 16_883,
        speculative_pages: 16_883,
        file_backed_pages: 223_899,
        purgeable_pages: 222,
        pressure_level: 1,
        idle_nanos: Some(25_404_751_221_958),
        ..laptop()
    };
    let reading = read(&raw).unwrap();
    assert_eq!(reading.available_bytes, 228_251 * 16384);
    assert_eq!(reading.idle, Millis(25_404_751));
}

#[test]
fn speculative_pages_are_counted_once() {
    // They are file pages read ahead: in the free count and in the file-backed count.
    let with = Raw {
        free_pages: 1000,
        speculative_pages: 400,
        file_backed_pages: 5000,
        purgeable_pages: 0,
        ..laptop()
    };
    assert_eq!(read(&with).unwrap().available_bytes, (600 + 5000) * 16384);
    // More speculative than free cannot be, and must not wrap into a huge number.
    let odd = Raw {
        free_pages: 100,
        speculative_pages: 400,
        ..with
    };
    assert_eq!(read(&odd).unwrap().available_bytes, 5000 * 16384);
}

#[test]
fn programs_own_pages_and_the_compressor_are_not_available() {
    let base = read(&laptop()).unwrap().available_bytes;
    let busier = Raw {
        active_pages: 400_000,
        inactive_pages: 300_000,
        anonymous_pages: 600_000,
        compressor_pages: 10,
        wired_pages: 10,
        ..laptop()
    };
    assert_eq!(read(&busier).unwrap().available_bytes, base);
}

#[test]
fn the_pressure_levels_are_one_two_and_four() {
    for (level, pressure) in [
        (1, Pressure::Normal),
        (2, Pressure::Warning),
        (4, Pressure::Critical),
    ] {
        assert_eq!(
            read(&Raw {
                pressure_level: level,
                ..laptop()
            })
            .unwrap()
            .pressure,
            pressure
        );
    }
    for level in [0, 3, 5, 8, -1, i32::MAX, i32::MIN] {
        let error = read(&Raw {
            pressure_level: level,
            ..laptop()
        })
        .unwrap_err();
        assert_eq!(
            error,
            HostError::Implausible {
                what: "the memory pressure level",
                value: level.to_string()
            }
        );
    }
}

#[test]
fn the_thermal_levels_follow_macos_and_anything_beyond_is_critical() {
    let expected = [
        (0, Thermal::Nominal),
        (1, Thermal::Fair),
        (2, Thermal::Serious),
        (3, Thermal::Critical),
        (4, Thermal::Critical),
        (5, Thermal::Critical),
        (u64::MAX, Thermal::Critical),
    ];
    for (level, thermal) in expected {
        assert_eq!(
            read(&Raw {
                thermal_level: level,
                ..laptop()
            })
            .unwrap()
            .thermal,
            thermal,
            "level {level}"
        );
    }
}

#[test]
fn only_unlimited_power_is_the_mains() {
    assert!(!read(&laptop()).unwrap().on_battery);
    // -1: on the battery, time not known yet. Then seconds left, and answers no Mac should give.
    for answer in [
        -1.0,
        0.0,
        1.0,
        5400.0,
        -2.5,
        -1.999,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ] {
        let reading = read(&Raw {
            power_remaining_secs: answer,
            ..laptop()
        })
        .unwrap();
        assert!(reading.on_battery, "{answer}");
    }
}

#[test]
fn without_an_idle_time_the_person_counts_as_there() {
    assert_eq!(
        read(&Raw {
            idle_nanos: None,
            ..laptop()
        })
        .unwrap()
        .idle,
        Millis::ZERO
    );
    assert_eq!(
        read(&Raw {
            idle_nanos: Some(0),
            ..laptop()
        })
        .unwrap()
        .idle,
        Millis::ZERO
    );
    assert_eq!(
        read(&Raw {
            idle_nanos: Some(999_999),
            ..laptop()
        })
        .unwrap()
        .idle,
        Millis(0)
    );
    assert_eq!(
        read(&Raw {
            idle_nanos: Some(1_000_000),
            ..laptop()
        })
        .unwrap()
        .idle,
        Millis(1)
    );
    assert_eq!(
        read(&Raw {
            idle_nanos: Some(u64::MAX),
            ..laptop()
        })
        .unwrap()
        .idle,
        Millis(u64::MAX / 1_000_000)
    );
}

#[test]
fn more_available_than_installed_is_an_error_not_a_number() {
    let pages_installed = 16 * GIB / 16384;
    let all = Raw {
        free_pages: pages_installed,
        speculative_pages: 0,
        file_backed_pages: 0,
        purgeable_pages: 0,
        ..laptop()
    };
    assert_eq!(read(&all).unwrap().available_bytes, 16 * GIB);
    let one_more = Raw {
        free_pages: pages_installed + 1,
        ..all
    };
    assert!(matches!(
        read(&one_more),
        Err(HostError::Implausible {
            what: "the available memory",
            ..
        })
    ));
    // Counts so large that bytes overflow, in each field.
    for raw in [
        Raw {
            free_pages: u64::MAX,
            ..all
        },
        Raw {
            file_backed_pages: u64::MAX,
            ..all
        },
        Raw {
            purgeable_pages: u64::MAX / 16384 + 1,
            free_pages: 0,
            ..all
        },
    ] {
        assert!(
            matches!(
                read(&raw),
                Err(HostError::Implausible {
                    what: "the available memory",
                    ..
                })
            ),
            "{raw:?}"
        );
    }
}

#[test]
fn a_page_size_that_is_no_power_of_two_is_an_error() {
    for page_bytes in [0, 3, 16383, 16385] {
        let error = read(&Raw {
            page_bytes,
            ..laptop()
        })
        .unwrap_err();
        assert_eq!(
            error,
            HostError::Implausible {
                what: "the page size",
                value: page_bytes.to_string()
            }
        );
    }
    // An Intel Mac's pages.
    let intel = Raw {
        page_bytes: 4096,
        ..laptop()
    };
    assert_eq!(read(&intel).unwrap().available_bytes, 130_790 * 4096);
}

#[test]
fn swap_and_disk_pass_through_as_they_are() {
    let raw = Raw {
        swap_used_bytes: 0,
        swapouts: 0,
        disk_free_bytes: 0,
        ..laptop()
    };
    let reading = read(&raw).unwrap();
    assert_eq!(
        (reading.swap_used_bytes, reading.swapouts, reading.disk_free_bytes),
        (0, 0, 0)
    );
    let raw = Raw {
        swap_used_bytes: u64::MAX,
        swapouts: u64::MAX,
        disk_free_bytes: u64::MAX,
        ..laptop()
    };
    let reading = read(&raw).unwrap();
    assert_eq!(
        (reading.swap_used_bytes, reading.swapouts, reading.disk_free_bytes),
        (u64::MAX, u64::MAX, u64::MAX)
    );
}
