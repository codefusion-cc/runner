//! The readers against folders that stand in for `/proc` and `/sys`: the board as it is, and every way its files
//! can be missing, cut short, malformed, too large, or change between two reads.

mod common;

use std::path::Path;

use cf_budget::{Battery, Host, HostError, HostFacts, Millis, Pressure, Thermal};
use cf_host_linux::{
    LinuxHost, Process, Psi, battery, boot_session, boot_time, fast_cpus, host_name, load, model, process,
    program_of,
};
use common::*;

fn host(tree: &Tree) -> LinuxHost {
    LinuxHost::under(tree.root(), tree.root()).expect("a host")
}

fn read(tree: &Tree) -> Result<cf_budget::HostReading, HostError> {
    host(tree).read()
}

#[test]
fn the_board_reads_as_its_files_say() {
    let tree = Tree::board();
    let mut host = host(&tree);
    assert_eq!(
        host.facts(),
        Ok(HostFacts {
            total_bytes: 8_129_820 * 1024,
            cores: 8
        })
    );
    let raw = host.raw().unwrap();
    assert_eq!(raw.memory.total_bytes, 8_129_820 * 1024);
    assert_eq!(raw.memory.swap_total_bytes, 4_064_908 * 1024);
    assert_eq!(host.swap_total_bytes(), Some(4_064_908 * 1024));
    assert_eq!(raw.memory_pressure, Some(Psi { some: 0, full: 0 }));
    assert_eq!(raw.zones.len(), 7);
    assert_eq!(raw.zones[0].name, "soc-thermal");
    assert_eq!(raw.zones[0].trips.len(), 3);
    assert_eq!(raw.supplies.len(), 1);
    let reading = host.read().unwrap();
    // MemAvailable, not MemFree (6265048 kB).
    assert_eq!(reading.available_bytes, 7_712_136 * 1024);
    assert_eq!(reading.swap_used_bytes, 0);
    assert_eq!(reading.swapouts, 0);
    assert_eq!(reading.pressure, Pressure::Normal);
    assert_eq!(reading.thermal, Thermal::Nominal);
    assert!(!reading.on_battery, "a USB-C supply and no battery");
    assert_eq!(reading.idle, Millis::ZERO);
    assert!(reading.disk_free_bytes > 0);
    assert!(
        host.disk_total_bytes()
            .is_some_and(|total| total >= reading.disk_free_bytes)
    );

    assert_eq!(fast_cpus(tree.root()), Some(vec![4, 5, 6, 7]));
    assert_eq!(
        boot_session(tree.root()).as_deref(),
        Some("575472c0-34c1-450a-8d2a-4b90fc2982ed")
    );
    assert_eq!(boot_time(tree.root()), Some(1_791_890_324));
    assert_eq!(model(tree.root()).as_deref(), Some("Generic RK3588 Board"));
    assert_eq!(host_name(tree.root()).as_deref(), Some("board"));
    assert_eq!(load(tree.root()), [0.13, 0.09, 0.03]);
    assert_eq!(battery(tree.root()), None, "no battery is none, not 0%");
    let ticks = host.cpu_ticks().unwrap();
    assert_eq!(
        (ticks.busy, ticks.idle),
        (3959 + 558 + 9313 + 4452, 3_345_824 + 2887)
    );
    // 487823 ticks of 1/100 s after btime.
    assert_eq!(
        process(tree.root(), PID),
        Some(Process {
            program: PROGRAM.into(),
            started_at: 1_791_890_324 + 4878,
            started_ticks: 487_823,
        })
    );
}

#[test]
fn a_file_every_reading_needs_that_is_missing_is_an_error() {
    for path in ["/proc/meminfo", "/proc/vmstat"] {
        let tree = Tree::board();
        tree.remove(path);
        assert!(
            matches!(read(&tree), Err(HostError::Call { what, code: 2 }) if what == path),
            "{path}: {:?}",
            read(&tree)
        );
    }
    let tree = Tree::board();
    tree.remove("/sys/devices/system/cpu/online");
    assert!(host(&tree).facts().is_err());
    assert_eq!(fast_cpus(tree.root()), None);
}

#[test]
fn a_kernel_without_psi_reads_as_no_pressure_signal_not_as_zero() {
    let tree = Tree::board();
    tree.remove("/proc/pressure");
    let raw = host(&tree).raw().unwrap();
    assert_eq!(raw.memory_pressure, None);
    assert_eq!(read(&tree).unwrap().pressure, Pressure::Normal);
    // A pressure file that is there but says something else is not taken for a missing one.
    let tree = Tree::board();
    tree.file("/proc/pressure/memory", "some avg10=oops\n");
    assert!(matches!(read(&tree), Err(HostError::Implausible { .. })));
    // So is one that is not text, or longer than the kernel writes.
    tree.bytes("/proc/pressure/memory", b"some avg10=\xff\n");
    assert!(matches!(read(&tree), Err(HostError::Call { .. })));
    tree.file(
        "/proc/pressure/memory",
        &"x".repeat(cf_host_linux::parse::MAX_FILE_BYTES + 1),
    );
    assert!(matches!(read(&tree), Err(HostError::Call { .. })));
    // A pressure file a folder stands in for cannot be read: no signal.
    let tree = Tree::board();
    tree.remove("/proc/pressure/memory");
    tree.file("/proc/pressure/memory/inside", "");
    assert_eq!(host(&tree).raw().unwrap().memory_pressure, None);
}

#[test]
fn pressure_reads_as_the_budget_levels() {
    let tree = Tree::board();
    let at = |some: &str, full: &str| {
        tree.file(
            "/proc/pressure/memory",
            &format!(
                "some avg10={some} avg60=0.00 avg300=0.00 total=1\nfull avg10={full} avg60=0.00 avg300=0.00 total=1\n"
            ),
        );
        read(&tree).unwrap().pressure
    };
    assert_eq!(at("9.99", "9.99"), Pressure::Normal);
    assert_eq!(at("10.00", "0.00"), Pressure::Warning);
    assert_eq!(at("55.10", "19.99"), Pressure::Warning);
    assert_eq!(at("55.10", "20.00"), Pressure::Critical);
    assert_eq!(at("100.00", "100.00"), Pressure::Critical);
}

#[test]
fn malformed_and_cut_short_meminfo_is_refused() {
    for meminfo in [
        "",
        "MemTotal:        8129820 kB\n",
        "MemTotal:        8129820 kB\nMemAvailable:    7712",
        "MemTotal:        8129820 kB\nMemAvailable:    7712136\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
        "MemTotal: 0 kB\nMemAvailable: 0 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
        "MemTotal: -1 kB\nMemAvailable: 0 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
        // Past what a u64 holds in bytes.
        "MemTotal: 18014398509481984 kB\nMemAvailable: 0 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
        "MemTotal: 99999999999999999999999 kB\nMemAvailable: 0 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
        // Two answers to one question.
        "MemTotal: 8 kB\nMemAvailable: 1 kB\nMemAvailable: 2 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
        // More available than there is, more swap free than there is.
        "MemTotal: 8 kB\nMemAvailable: 9 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
        "MemTotal: 8 kB\nMemAvailable: 1 kB\nSwapTotal: 1 kB\nSwapFree: 2 kB\n",
    ] {
        let tree = Tree::board();
        tree.file("/proc/meminfo", meminfo);
        assert!(
            matches!(read(&tree), Err(HostError::Implausible { .. })),
            "{meminfo:?}: {:?}",
            read(&tree)
        );
    }
    // The largest that fits, and a meminfo of the next kernel with lines it does not know yet.
    let tree = Tree::board();
    tree.file(
        "/proc/meminfo",
        "Future:  1 GB\nMemTotal: 18014398509481983 kB\nMemAvailable: 18014398509481983 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
    );
    assert_eq!(
        read(&tree).unwrap().available_bytes,
        18_014_398_509_481_983 * 1024
    );
}

#[test]
fn a_file_larger_than_the_kernel_writes_or_not_text_is_not_read_in_part() {
    let tree = Tree::board();
    let mut huge = MEMINFO.to_string();
    huge.push_str(&"Padding: 1 kB\n".repeat(80_000));
    tree.file("/proc/meminfo", &huge);
    assert!(matches!(read(&tree), Err(HostError::Call { .. })));
    tree.bytes("/proc/meminfo", b"MemTotal: \xff\xfe kB\n");
    assert!(matches!(read(&tree), Err(HostError::Call { .. })));
}

#[test]
fn swap_outs_are_the_counter_and_a_missing_one_is_an_error() {
    let tree = Tree::board();
    tree.file("/proc/vmstat", "pswpin 3\npswpout 18446744073709551615\n");
    assert_eq!(read(&tree).unwrap().swapouts, u64::MAX);
    for vmstat in [
        "pswpin 3\n",
        "pswpout\n",
        "pswpout -1\n",
        "pswpout 1\npswpout 2\n",
        "pswpouts 4\n",
    ] {
        tree.file("/proc/vmstat", vmstat);
        assert!(
            matches!(read(&tree), Err(HostError::Implausible { .. })),
            "{vmstat:?}"
        );
    }
}

#[test]
fn heat_reads_by_each_zones_own_trip_points() {
    let soc = |millidegrees: i32| {
        let tree = Tree::board();
        tree.zone(
            0,
            "soc-thermal",
            millidegrees,
            &[("passive", 60000), ("passive", 85000), ("critical", 115000)],
        );
        read(&tree).unwrap().thermal
    };
    assert_eq!(soc(59_999), Thermal::Nominal);
    assert_eq!(soc(60_000), Thermal::Fair);
    assert_eq!(soc(84_999), Thermal::Fair);
    assert_eq!(
        soc(85_000),
        Thermal::Serious,
        "where an RK3588 holds its cores back"
    );
    assert_eq!(soc(114_999), Thermal::Serious);
    assert_eq!(soc(115_000), Thermal::Critical);

    // The hottest zone decides, whichever it is.
    let tree = Tree::board();
    tree.zone(6, "npu-thermal", 116_000, &[("critical", 115000)]);
    assert_eq!(read(&tree).unwrap().thermal, Thermal::Critical);
}

#[test]
fn zones_that_say_nothing_or_cannot_be_read_are_left_out() {
    // No thermal zones at all, as in a virtual machine.
    let tree = Tree::board();
    tree.remove("/sys/class/thermal");
    assert_eq!(host(&tree).raw().unwrap().zones, []);
    assert_eq!(read(&tree).unwrap().thermal, Thermal::Nominal);

    let tree = Tree::board();
    // Off, hot as it may be.
    tree.zone(1, "off", 200_000, &[("critical", 100_000)]);
    tree.file("/sys/class/thermal/thermal_zone1/mode", "disabled\n");
    // A sensor that cannot be read, one with the kernel's invalid mark, one with text.
    tree.zone(2, "asleep", 0, &[("critical", 100_000)]);
    tree.remove("/sys/class/thermal/thermal_zone2/temp");
    tree.zone(3, "invalid", -274_000, &[("critical", 100_000)]);
    tree.zone(4, "garbled", 0, &[("critical", 100_000)]);
    tree.file("/sys/class/thermal/thermal_zone4/temp", "hot\n");
    // A trip firmware does not use (0 C, or the invalid mark), one of a kind nobody knows, one without a
    // temperature: none of them counts, and the trips after them still do.
    tree.zone(
        5,
        "odd trips",
        70_000,
        &[
            ("critical", 0),
            ("passive", -274_000),
            ("strange", 1),
            ("passive", 90_000),
        ],
    );
    tree.remove("/sys/class/thermal/thermal_zone5/trip_point_3_temp");
    tree.file("/sys/class/thermal/thermal_zone5/trip_point_4_type", "passive\n");
    tree.file("/sys/class/thermal/thermal_zone5/trip_point_4_temp", "69000\n");
    let raw = host(&tree).raw().unwrap();
    let names: Vec<&str> = raw.zones.iter().map(|zone| zone.name.as_str()).collect();
    assert_eq!(names, ["soc-thermal", "odd trips", "npu-thermal"]);
    assert_eq!(
        read(&tree).unwrap().thermal,
        Thermal::Serious,
        "70 C over the passive trip at 69 C"
    );
}

#[test]
fn power_reads_as_the_mains_unless_a_battery_powers_the_host() {
    // A laptop on its charger, then unplugged.
    let tree = Tree::board();
    tree.remove("/sys/class/power_supply");
    tree.supply("AC", &[("type", "Mains"), ("online", "1")]);
    tree.supply(
        "BAT0",
        &[
            ("type", "Battery"),
            ("present", "1"),
            ("status", "Charging"),
            ("capacity", "57"),
        ],
    );
    assert!(!read(&tree).unwrap().on_battery);
    assert_eq!(
        battery(tree.root()),
        Some(Battery {
            on_battery: false,
            percent: 57,
            charging: true
        })
    );
    tree.supply("AC", &[("online", "0")]);
    tree.supply("BAT0", &[("status", "Discharging")]);
    assert!(read(&tree).unwrap().on_battery);
    assert_eq!(
        battery(tree.root()),
        Some(Battery {
            on_battery: true,
            percent: 57,
            charging: false
        })
    );
    // Just unplugged, the battery still says it is full: the charger's word wins.
    tree.supply("BAT0", &[("status", "Full")]);
    assert!(read(&tree).unwrap().on_battery);
    // Plugged into a charger too weak for the load, the battery drains and says so: the host still runs on the
    // mains, and the battery is not charging.
    tree.supply("AC", &[("online", "1")]);
    tree.supply("BAT0", &[("status", "Discharging")]);
    assert!(!read(&tree).unwrap().on_battery);
    assert_eq!(
        battery(tree.root()),
        Some(Battery {
            on_battery: false,
            percent: 57,
            charging: false
        })
    );

    // A wireless mouse's battery powers the mouse, not the host.
    let tree = Tree::board();
    tree.supply(
        "hid-mouse-battery",
        &[
            ("type", "Battery"),
            ("scope", "Device"),
            ("status", "Discharging"),
            ("capacity", "12"),
        ],
    );
    assert!(!read(&tree).unwrap().on_battery);
    assert_eq!(battery(tree.root()), None);
    // An empty battery bay.
    let tree = Tree::board();
    tree.remove("/sys/class/power_supply");
    tree.supply(
        "BAT1",
        &[("type", "Battery"), ("present", "0"), ("status", "Unknown")],
    );
    assert!(!read(&tree).unwrap().on_battery);
    assert_eq!(battery(tree.root()), None);
}

#[test]
fn a_battery_with_no_word_from_a_charger_is_read_by_its_state_and_cautiously() {
    let tree = Tree::board();
    for (status, on_battery) in [
        (Some("Charging"), false),
        (Some("Full"), false),
        (Some("Not charging"), false),
        (Some("Discharging"), true),
        (Some("Unknown"), true),
        (Some("Exploding"), true),
        (None, true),
    ] {
        tree.remove("/sys/class/power_supply");
        tree.supply("BAT0", &[("type", "Battery"), ("capacity", "101")]);
        if let Some(status) = status {
            tree.supply("BAT0", &[("status", status)]);
        }
        assert_eq!(read(&tree).unwrap().on_battery, on_battery, "{status:?}");
        // A charge over 100% is none at all.
        assert_eq!(battery(tree.root()), None);
    }
    // Two batteries: the page shows the first by name that says its charge.
    tree.remove("/sys/class/power_supply");
    tree.supply(
        "BAT1",
        &[("type", "Battery"), ("status", "Full"), ("capacity", "90")],
    );
    tree.supply(
        "BAT0",
        &[("type", "Battery"), ("status", "Charging"), ("capacity", "40")],
    );
    let first = Battery {
        on_battery: false,
        percent: 40,
        charging: true,
    };
    assert_eq!(battery(tree.root()), Some(first));
    tree.supply("BAT0", &[("capacity", "")]);
    assert_eq!(
        battery(tree.root()),
        Some(Battery {
            on_battery: false,
            percent: 90,
            charging: false
        })
    );
    // No supplies at all, as in a virtual machine.
    tree.remove("/sys/class/power_supply");
    assert!(!read(&tree).unwrap().on_battery);
}

#[test]
fn the_fast_cpus_are_those_of_the_highest_capacity_among_the_online_ones() {
    let tree = Tree::board();
    // CPU 7 offline: the fast ones are 4 to 6.
    tree.file("/sys/devices/system/cpu/online", "0-6\n");
    assert_eq!(fast_cpus(tree.root()), Some(vec![4, 5, 6]));
    assert_eq!(host(&tree).facts().unwrap().cores, 7);
    // All alike.
    for cpu in 0..8 {
        tree.capacity(cpu, "1024\n");
    }
    assert_eq!(fast_cpus(tree.root()), Some((0..7).collect()));
    // One that says nothing of its capacity, or nonsense: no word, so all of them.
    tree.remove("/sys/devices/system/cpu/cpu2/cpu_capacity");
    assert_eq!(fast_cpus(tree.root()), Some((0..7).collect()));
    tree.file("/sys/devices/system/cpu/cpu2/cpu_capacity", "-5\n");
    assert_eq!(fast_cpus(tree.root()), Some((0..7).collect()));
    // An online list that cannot be right.
    for online in ["", "7-0", "0-3,2", "0-4294967295", "x"] {
        tree.file("/sys/devices/system/cpu/online", online);
        assert_eq!(fast_cpus(tree.root()), None, "{online:?}");
        assert!(host(&tree).facts().is_err(), "{online:?}");
    }
}

#[test]
fn the_fast_cpus_are_a_kind_of_core_not_a_clock_speed() {
    let tree = Tree::board();
    let capacity = |cpu, value| tree.capacity(cpu, value);
    let midr = |cpu, value| tree.midr(cpu, value);
    // The second big cluster binned to a lower top speed: still the same cores, all four for CI.
    capacity(6, "1000\n");
    capacity(7, "1000\n");
    assert_eq!(fast_cpus(tree.root()), Some(vec![4, 5, 6, 7]));
    // A later revision of the same design is the same kind.
    midr(5, "0x00000000411fd0b1\n");
    assert_eq!(fast_cpus(tree.root()), Some(vec![4, 5, 6, 7]));
    // A prime core of another design beside them is a kind of its own.
    midr(7, "0x00000000410fd440\n");
    capacity(7, "1024\n");
    capacity(4, "900\n");
    capacity(5, "900\n");
    capacity(6, "900\n");
    assert_eq!(fast_cpus(tree.root()), Some(vec![7]));
    // Where one core's design cannot be read, the kind is the capacity.
    midr(7, "0x00000000414fd0b0\n");
    capacity(7, "900\n");
    capacity(4, "1024\n");
    midr(2, "nonsense\n");
    assert_eq!(fast_cpus(tree.root()), Some(vec![4]));
    tree.remove("/sys/devices/system/cpu/cpu2/regs/identification/midr_el1");
    assert_eq!(fast_cpus(tree.root()), Some(vec![4]));
}

#[test]
fn a_process_is_told_apart_from_a_later_one_with_its_number() {
    let tree = Tree::board();
    let first = process(tree.root(), PID).unwrap();
    // The number went to a later process of the same program: the same path, a later start.
    tree.process(PID, "codefusion-run", 487_823 + 6000, PROGRAM);
    let later = process(tree.root(), PID).unwrap();
    assert_eq!(later.program, first.program);
    assert_eq!(later.started_at, first.started_at + 60);
    assert_eq!(later.started_ticks, first.started_ticks + 6000);
    // NTP steps the clock 100 s ahead, and Linux's btime with it: the time in seconds since 1970 moves, the ticks
    // since the start of the host do not.
    tree.file(
        "/proc/stat",
        &STAT.replace("btime 1791890324", "btime 1791890424"),
    );
    let stepped = process(tree.root(), PID).unwrap();
    assert_eq!(stepped.started_at, later.started_at + 100);
    assert_eq!(stepped.started_ticks, later.started_ticks);
    // Gone: its folder, or its program's link, or its stat.
    tree.remove(&format!("/proc/{PID}/exe"));
    assert_eq!(program_of(tree.root(), PID), None);
    tree.process(PID, "cat", 1, PROGRAM);
    tree.remove(&format!("/proc/{PID}/stat"));
    assert_eq!(process(tree.root(), PID), None);
    // A link with no start to check it against names no program either.
    assert_eq!(program_of(tree.root(), PID), None);
    tree.remove(&format!("/proc/{PID}"));
    assert_eq!(process(tree.root(), PID), None);
    assert_eq!(process(tree.root(), 0), None);
    // A program whose file was replaced since keeps the kernel's mark.
    tree.process(
        7,
        "vm",
        10,
        "/opt/codefusion-runner/bin/codefusion-runner (deleted)",
    );
    assert_eq!(
        program_of(tree.root(), 7).as_deref(),
        Some(Path::new(
            "/opt/codefusion-runner/bin/codefusion-runner (deleted)"
        ))
    );
    // Without btime there is no start.
    tree.file("/proc/stat", "cpu  1 2 3 4\n");
    assert_eq!(process(tree.root(), 7), None);
}

#[test]
fn a_process_whose_name_is_made_to_look_like_fields_is_read_by_its_real_fields() {
    let tree = Tree::board();
    for name in [
        "a) R 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 999999",
        "x (y) z",
        ")",
        "",
        "  ",
    ] {
        tree.process(9, name, 2500, PROGRAM);
        assert_eq!(
            process(tree.root(), 9).map(|process| process.started_at),
            Some(1_791_890_324 + 25),
            "{name:?}"
        );
    }
    for stat in [
        "",
        "9 (cut",
        "9 (vm) R 1",
        "nine (vm) R 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20",
    ] {
        tree.file("/proc/9/stat", stat);
        assert_eq!(process(tree.root(), 9), None, "{stat:?}");
    }
}

#[test]
fn the_page_facts_say_nothing_rather_than_something_wrong() {
    let tree = Tree::empty();
    assert_eq!(boot_session(tree.root()), None);
    assert_eq!(boot_time(tree.root()), None);
    assert_eq!(model(tree.root()), None);
    assert_eq!(host_name(tree.root()), None);
    assert_eq!(load(tree.root()), [0.0; 3]);
    assert_eq!(battery(tree.root()), None);

    let tree = Tree::board();
    for id in [
        "575472c0-34c1-450a-8d2a-4b90fc2982e",
        "575472c0-34c1-450a-8d2a-4b90fc2982edd",
        "575472c034c1-450a-8d2a-4b90fc2982ed0",
        "575472C0-34C1-450A-8D2A-4B90FC2982ED",
        "zzzzzzzz-34c1-450a-8d2a-4b90fc2982ed",
        "",
    ] {
        tree.file("/proc/sys/kernel/random/boot_id", id);
        assert_eq!(boot_session(tree.root()), None, "{id:?}");
    }
    for stat in [
        "btime 0\n",
        "btime -5\n",
        "btime\n",
        "btime 99999999999999999999\n",
        "cpu  1 2 3 4\n",
    ] {
        tree.file("/proc/stat", stat);
        assert_eq!(boot_time(tree.root()), None, "{stat:?}");
    }
    for loadavg in ["", "0.1 0.2", "0.1 -0.2 0.3", "NaN 1 1", "inf 1 1", "a b c"] {
        tree.file("/proc/loadavg", loadavg);
        assert_eq!(load(tree.root()), [0.0; 3], "{loadavg:?}");
    }
    for name in [&b"\0"[..], b"", b"  \n", b"bad\x07name", b"\xff\xfe"] {
        tree.bytes("/proc/sys/kernel/hostname", name);
        assert_eq!(host_name(tree.root()), None, "{name:?}");
    }
    tree.file("/proc/sys/kernel/hostname", &"h".repeat(300));
    assert_eq!(host_name(tree.root()).map(|name| name.len()), Some(128));
}

#[test]
fn the_model_comes_from_the_device_tree_else_from_dmi() {
    let tree = Tree::board();
    tree.remove("/proc/device-tree");
    assert_eq!(model(tree.root()), None);
    tree.file("/sys/class/dmi/id/sys_vendor", "LENOVO\n");
    tree.file("/sys/class/dmi/id/product_name", "ThinkCentre M75q\n");
    assert_eq!(model(tree.root()).as_deref(), Some("LENOVO ThinkCentre M75q"));
    tree.file("/sys/class/dmi/id/product_name", "To Be Filled By O.E.M.\n");
    assert_eq!(model(tree.root()).as_deref(), Some("LENOVO"));
    tree.file("/sys/class/dmi/id/sys_vendor", "System manufacturer\n");
    assert_eq!(model(tree.root()), None);
    tree.file("/sys/class/dmi/id/sys_vendor", "Hetzner\n");
    tree.file("/sys/class/dmi/id/product_name", "Hetzner vServer\n");
    assert_eq!(model(tree.root()).as_deref(), Some("Hetzner vServer"));
    // An empty device-tree model leaves the word to DMI.
    tree.bytes("/proc/device-tree/model", b"\0");
    assert_eq!(model(tree.root()).as_deref(), Some("Hetzner vServer"));
}

#[test]
fn a_host_whose_disk_folder_is_not_there_does_not_start() {
    let tree = Tree::board();
    assert!(matches!(
        LinuxHost::under(tree.root(), &tree.path("/no/such/folder")),
        Err(HostError::Call { what: "statvfs", .. })
    ));
    assert!(matches!(
        LinuxHost::under(tree.root(), Path::new("/a\0b")),
        Err(HostError::Implausible { .. })
    ));
}

#[test]
fn readings_take_their_time_from_the_hosts_clocks() {
    // What the clocks do is cf-clock's to test; a reading must carry them, not a time of zero.
    let tree = Tree::board();
    let at = host(&tree).read().unwrap().at;
    assert!(at.elapsed.0 > 0 && at.awake.0 > 0, "{at:?}");
}
