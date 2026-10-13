//! The text formats: what the kernel writes comes back as the numbers it wrote, anything else as no answer, and no
//! input makes a parser panic.

mod common;

use cf_budget::{Pressure, Thermal};
use cf_host_linux::parse::{
    self, CpuTicks, Meminfo, Psi, boot_id, btime, counter, cpu_list, cpu_ticks, loadavg, meminfo,
    millidegrees, psi, start_ticks, text,
};
use cf_host_linux::{Trip, TripKind, Zone, cpu_percent, pressure_of, thermal_of};
use proptest::prelude::*;

/// A CPU list as the kernel writes it: runs of two or more as ranges.
fn written(cpus: &[u32]) -> String {
    let mut parts = Vec::new();
    let mut at = 0;
    while at < cpus.len() {
        let mut end = at;
        while end + 1 < cpus.len() && cpus[end + 1] == cpus[end] + 1 {
            end += 1;
        }
        parts.push(if end == at {
            cpus[at].to_string()
        } else {
            format!("{}-{}", cpus[at], cpus[end])
        });
        at = end + 1;
    }
    parts.join(",") + "\n"
}

#[test]
fn a_counter_is_read_by_its_whole_name_and_only_once() {
    let events = "low 0\nhigh 0\nmax 12\noom 1\noom_kill 3\noom_group_kill 1\n";
    assert_eq!(counter(events, "oom_kill"), Some(3));
    assert_eq!(counter(events, "oom"), Some(1));
    assert_eq!(counter(events, "max"), Some(12));
    assert_eq!(counter(events, "kill"), None);
    assert_eq!(counter("populated x\n", "populated"), None);
    assert_eq!(counter("populated -1\n", "populated"), None);
    assert_eq!(counter("", "populated"), None);
    assert_eq!(
        counter("pswpout 1\npswpout 2\n", "pswpout"),
        None,
        "twice is no answer"
    );
    assert_eq!(
        counter("pswpout 18446744073709551615\n", "pswpout"),
        Some(u64::MAX)
    );
    assert_eq!(counter("pswpout 18446744073709551616\n", "pswpout"), None);
}

#[test]
fn psi_shares_are_read_to_the_hundredth() {
    let line = |some: &str, full: &str| {
        psi(&format!(
            "some avg10={some} avg60=1.00 avg300=2.00 total=9\nfull avg10={full} avg60=0.00 avg300=0.00 total=3\n"
        ))
    };
    assert_eq!(line("12.34", "0.05"), Ok(Psi { some: 1234, full: 5 }));
    assert_eq!(
        line("100.00", "0.00"),
        Ok(Psi {
            some: 10_000,
            full: 0
        })
    );
    assert_eq!(line("7.5", "3"), Ok(Psi { some: 750, full: 300 }));
    for bad in [
        "100.01", "1000.00", "-1.00", "1.234", "1.", ".5", "", "nan", "1e2",
    ] {
        assert!(line(bad, "0.00").is_err(), "{bad:?}");
        assert!(line("0.00", bad).is_err(), "{bad:?}");
    }
    // Only the cpu file of an old kernel has no full line; memory always has both.
    assert!(psi("some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n").is_err());
    assert!(psi("").is_err());
    // A key the next kernel adds is passed over.
    assert_eq!(
        psi("some avg5=9.00 avg10=1.00\nfull avg10=2.00 extra=x\n"),
        Ok(Psi { some: 100, full: 200 })
    );
}

#[test]
fn cpu_ticks_count_the_guests_once_and_waiting_for_io_as_idle() {
    // user nice system idle iowait irq softirq steal guest guest_nice
    assert_eq!(
        cpu_ticks("cpu  10 20 30 40 50 60 70 80 90 100\n"),
        Some(CpuTicks {
            busy: 10 + 20 + 30 + 60 + 70 + 80,
            idle: 40 + 50
        })
    );
    // A kernel of before iowait: four numbers.
    assert_eq!(cpu_ticks("cpu  1 2 3 4\n"), Some(CpuTicks { busy: 6, idle: 4 }));
    for bad in [
        "cpu  1 2 3\n",
        "cpu0 1 2 3 4\n",
        "cpu  1 2 x 4\n",
        "",
        "cpu  -1 2 3 4\n",
    ] {
        assert_eq!(cpu_ticks(bad), None, "{bad:?}");
    }
    let max = u64::MAX;
    assert_eq!(cpu_ticks(&format!("cpu  {max} 1 0 0\n")), None, "a sum past u64");
}

#[test]
fn the_cpu_share_is_the_busy_ticks_of_all_that_passed() {
    let ticks = |busy, idle| CpuTicks { busy, idle };
    assert_eq!(cpu_percent(ticks(100, 100), ticks(140, 160)), Some(40.0));
    assert_eq!(cpu_percent(ticks(0, 0), ticks(0, 50)), Some(0.0));
    assert_eq!(cpu_percent(ticks(0, 0), ticks(25, 0)), Some(100.0));
    assert_eq!(cpu_percent(ticks(5, 5), ticks(5, 5)), None, "no time passed");
    // The host started again in between.
    assert_eq!(cpu_percent(ticks(1000, 1000), ticks(10, 2000)), None);
    assert_eq!(cpu_percent(ticks(1000, 1000), ticks(2000, 10)), None);
}

#[test]
fn names_are_cut_at_their_nul_and_at_128_characters() {
    assert_eq!(
        text(b"Generic RK3588 Board\0"),
        Some("Generic RK3588 Board".into())
    );
    assert_eq!(text(b"one\0two"), Some("one".into()));
    assert_eq!(
        text("ü".repeat(200).as_bytes()).map(|name| name.chars().count()),
        Some(128)
    );
    assert_eq!(text(b"tab\there"), None);
    assert_eq!(text(b"\0rest"), None);
}

#[test]
fn temperatures_past_any_sensor_are_none() {
    assert_eq!(millidegrees("44384\n"), Some(44_384));
    assert_eq!(millidegrees("-20000"), Some(-20_000));
    assert_eq!(millidegrees("300000"), Some(300_000));
    for bad in ["300001", "-100001", "-274000", "2147483648", "", "40.5", "warm"] {
        assert_eq!(millidegrees(bad), None, "{bad:?}");
    }
}

fn zone(millidegrees: i32, trips: &[(TripKind, i32)]) -> Zone {
    Zone {
        name: "zone".into(),
        millidegrees,
        trips: trips
            .iter()
            .map(|(kind, millidegrees)| Trip {
                kind: *kind,
                millidegrees: *millidegrees,
            })
            .collect(),
    }
}

#[test]
fn a_zone_with_one_passive_trip_goes_from_nominal_to_serious_and_a_fan_makes_it_fair() {
    use TripKind::{Active, Critical, Hot, Passive};
    let one = [(Passive, 80_000), (Critical, 100_000)];
    assert_eq!(thermal_of(&[zone(79_999, &one)]), Thermal::Nominal);
    assert_eq!(thermal_of(&[zone(80_000, &one)]), Thermal::Serious);
    let fan = [
        (Active, 50_000),
        (Passive, 80_000),
        (Hot, 95_000),
        (Critical, 100_000),
    ];
    assert_eq!(thermal_of(&[zone(50_000, &fan)]), Thermal::Fair);
    assert_eq!(
        thermal_of(&[zone(95_000, &fan)]),
        Thermal::Critical,
        "hot is the last warning"
    );
    // Without a passive trip there is no serious level, only the end.
    assert_eq!(
        thermal_of(&[zone(99_000, &[(Critical, 100_000)])]),
        Thermal::Nominal
    );
    assert_eq!(thermal_of(&[zone(150_000, &[])]), Thermal::Nominal);
    assert_eq!(thermal_of(&[]), Thermal::Nominal);
}

#[test]
fn psi_maps_to_the_budgets_levels_at_its_two_thresholds() {
    let at = |some, full| pressure_of(Some(Psi { some, full }));
    assert_eq!(pressure_of(None), Pressure::Normal);
    assert_eq!(at(999, 1999), Pressure::Normal);
    assert_eq!(at(1000, 0), Pressure::Warning);
    assert_eq!(at(0, 2000), Pressure::Critical, "full stalls are some stalls too");
}

fn trip() -> impl Strategy<Value = (TripKind, i32)> {
    (
        prop::sample::select(vec![
            TripKind::Active,
            TripKind::Passive,
            TripKind::Hot,
            TripKind::Critical,
        ]),
        -1000..130_000i32,
    )
}

proptest! {
    #[test]
    fn meminfo_gives_back_what_the_kernel_wrote(
        total in 1..u64::MAX / 1024,
        available in any::<u64>(),
        swap_total in 0..u64::MAX / 1024,
        swap_free in any::<u64>(),
        noise in prop::collection::vec("[A-Za-z_()]{1,16}: {0,9}[0-9]{1,12}( kB)?", 0..12),
    ) {
        let (available, swap_free) = (available % total, swap_free % (swap_total + 1));
        let mut lines = noise;
        lines.push(format!("MemTotal:  {total} kB"));
        lines.push(format!("MemAvailable:  {available} kB"));
        lines.push(format!("SwapTotal:  {swap_total} kB"));
        lines.push(format!("SwapFree:  {swap_free} kB"));
        lines.reverse();
        let names = ["MemTotal", "MemAvailable", "SwapTotal", "SwapFree"];
        prop_assume!(lines.iter().filter(|line| names.iter().any(|name| line.starts_with(&format!("{name}:")))).count() == 4);
        prop_assert_eq!(
            meminfo(&lines.join("\n")),
            Ok(Meminfo {
                total_bytes: total * 1024,
                available_bytes: available * 1024,
                swap_total_bytes: swap_total * 1024,
                swap_free_bytes: swap_free * 1024,
            })
        );
    }

    #[test]
    fn a_cpu_list_reads_back_as_the_set_it_names(set in prop::collection::btree_set(0..5000u32, 0..300)) {
        let cpus: Vec<u32> = set.into_iter().collect();
        prop_assert_eq!(cpu_list(&written(&cpus)), Some(cpus));
    }

    #[test]
    fn psi_reads_back_every_share_from_0_to_100(some in 0..=10_000u32, full in 0..=10_000u32) {
        let text = format!(
            "some avg10={}.{:02} avg60=0.00 avg300=0.00 total=1\nfull avg10={}.{:02} avg60=0.00 avg300=0.00 total=1\n",
            some / 100, some % 100, full / 100, full % 100
        );
        prop_assert_eq!(psi(&text), Ok(Psi { some, full }));
    }

    #[test]
    fn a_process_start_is_found_whatever_its_name(name in "\\PC{0,40}", ticks in any::<u64>(), pid in 1..u32::MAX) {
        prop_assert_eq!(start_ticks(&common::stat_of(pid, &name, ticks)), Some(ticks));
    }

    #[test]
    fn the_cpu_share_stays_between_0_and_100(
        busy in 0..u64::MAX / 4, idle in 0..u64::MAX / 4, more_busy in 0..u64::MAX / 4, more_idle in 0..u64::MAX / 4,
    ) {
        let before = CpuTicks { busy, idle };
        let now = CpuTicks { busy: busy + more_busy, idle: idle + more_idle };
        match cpu_percent(before, now) {
            None => prop_assert_eq!(more_busy + more_idle, 0),
            Some(share) => prop_assert!((0.0..=100.0).contains(&share)),
        }
    }

    #[test]
    fn a_warmer_zone_is_never_a_lower_level(
        trips in prop::collection::vec(trip(), 0..6),
        others in prop::collection::vec((-1000..130_000i32, prop::collection::vec(trip(), 0..4)), 0..4),
        cooler in -1000..130_000i32,
        warmer_by in 0..50_000i32,
    ) {
        let rest: Vec<Zone> = others.iter().map(|(temp, trips)| zone(*temp, trips)).collect();
        let level = |temp: i32| {
            let mut zones = rest.clone();
            zones.push(zone(temp, &trips));
            thermal_of(&zones)
        };
        prop_assert!(level(cooler) <= level(cooler + warmer_by));
        // And a zone only adds heat to what the others say.
        prop_assert!(thermal_of(&rest) <= level(cooler));
    }

    #[test]
    fn more_pressure_is_never_a_lower_level(some in 0..=10_000u32, full in 0..=10_000u32, more in 0..=10_000u32) {
        let at = |some: u32, full: u32| pressure_of(Some(Psi { some: some.min(10_000), full: full.min(10_000) }));
        prop_assert!(at(some, full) <= at(some + more, full));
        prop_assert!(at(some, full) <= at(some, full + more));
    }

    #[test]
    fn no_text_makes_a_parser_panic(text_in in "\\PC{0,200}", bytes in prop::collection::vec(any::<u8>(), 0..200)) {
        let _ = meminfo(&text_in);
        let _ = counter(&text_in, "pswpout");
        let _ = psi(&text_in);
        let _ = cpu_list(&text_in);
        let _ = btime(&text_in);
        let _ = cpu_ticks(&text_in);
        let _ = start_ticks(&text_in);
        let _ = loadavg(&text_in);
        let _ = boot_id(&text_in);
        let _ = millidegrees(&text_in);
        let _ = parse::count(&text_in);
        let _ = text(&bytes);
    }
}
