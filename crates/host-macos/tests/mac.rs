//! The real reader on the Mac the tests run on, against what macOS's own tools print at the same moment.
#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use cf_budget::{Host, HostError};
use cf_host_macos::{MacHost, Raw, battery, battery_in, model, performance_cores, reading};

fn output(program: &str, args: &[&str]) -> String {
    let output = Command::new(program).args(args).output().unwrap();
    assert!(output.status.success(), "{program} {args:?}");
    String::from_utf8(output.stdout).unwrap()
}

fn sysctl(name: &str) -> String {
    output("/usr/sbin/sysctl", &["-n", name]).trim().to_string()
}

/// One count of a raw reading.
type Field = fn(&Raw) -> u64;

/// `vm_stat`'s lines by name, in pages.
fn vm_stat() -> HashMap<String, u64> {
    output("/usr/bin/vm_stat", &[])
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            let value = value.trim().trim_end_matches('.').parse().ok()?;
            Some((name.trim_matches('"').to_string(), value))
        })
        .collect()
}

#[test]
fn the_facts_are_what_sysctl_prints() {
    let host = MacHost::new(Path::new("/")).unwrap();
    let facts = host.facts().unwrap();
    assert_eq!(facts.total_bytes.to_string(), sysctl("hw.memsize"));
    assert_eq!(facts.cores.to_string(), sysctl("hw.ncpu"));
}

#[test]
fn the_performance_cores_are_level_0_of_a_chip_with_two_kinds_and_none_of_one_with_one() {
    let levels: u32 = sysctl("hw.nperflevels").parse().unwrap();
    let expected = if levels > 1 {
        sysctl("hw.perflevel0.logicalcpu").parse().unwrap()
    } else {
        0
    };
    assert_eq!(performance_cores(), expected);
    let cores: u32 = sysctl("hw.ncpu").parse().unwrap();
    assert!(expected < cores, "{expected} performance cores of {cores}");
}

#[test]
fn the_mac_is_named_by_the_chip_and_the_model_sysctl_prints() {
    let (chip, name) = (sysctl("machdep.cpu.brand_string"), sysctl("hw.model"));
    assert!(!chip.is_empty() && !name.is_empty(), "{chip:?} {name:?}");
    assert_eq!(model(), Some(format!("{chip} ({name})")));
}

#[test]
fn the_battery_is_the_one_pmset_prints_and_none_on_a_mac_without_one() {
    let before = battery_in(&output("/usr/bin/pmset", &["-g", "batt"]));
    let read = battery();
    let after = battery_in(&output("/usr/bin/pmset", &["-g", "batt"]));
    // The charge may move between the looks; whether there is a battery does not.
    assert_eq!(read.is_some(), before.is_some());
    assert!(
        read == before || read == after,
        "{read:?} between {before:?} and {after:?}"
    );
    assert_eq!(
        before.is_some(),
        output("/usr/bin/pmset", &["-g", "batt"]).contains("InternalBattery")
    );
}

/// One look at the page counts beside `vm_stat`'s: what is off, if anything.
fn page_counts_beside_vm_stat(host: &MacHost) -> Result<(), String> {
    let before = host.raw().unwrap();
    let total_pages = host.facts().unwrap().total_bytes / before.page_bytes;
    // Memory moves between two looks at it, but a field read from the wrong place is off by far more than this.
    let slack = total_pages / 32;
    let tool = vm_stat();
    let after = host.raw().unwrap();
    assert_eq!(before.page_bytes.to_string(), sysctl("hw.pagesize"));

    let fields: [(&str, Field); 9] = [
        ("Pages free", |raw| raw.free_pages - raw.speculative_pages),
        ("Pages speculative", |raw| raw.speculative_pages),
        ("Pages active", |raw| raw.active_pages),
        ("Pages inactive", |raw| raw.inactive_pages),
        ("Pages wired down", |raw| raw.wired_pages),
        ("Pages purgeable", |raw| raw.purgeable_pages),
        ("File-backed pages", |raw| raw.file_backed_pages),
        ("Anonymous pages", |raw| raw.anonymous_pages),
        ("Pages occupied by compressor", |raw| raw.compressor_pages),
    ];
    for (name, field) in fields {
        let (low, high) = (
            field(&before).min(field(&after)),
            field(&before).max(field(&after)),
        );
        let printed = tool[name];
        if printed + slack < low || printed > high + slack {
            return Err(format!("{name}: vm_stat {printed}, read {low} to {high}"));
        }
    }
    // A counter: what the tool printed lies between the two reads.
    assert!(
        before.swapouts <= tool["Swapouts"] && tool["Swapouts"] <= after.swapouts,
        "swapouts"
    );
    // The pages macOS accounts for are most of the Mac and not more than it.
    let counted = after.free_pages
        + after.active_pages
        + after.inactive_pages
        + after.wired_pages
        + after.compressor_pages;
    assert!(
        counted <= total_pages && counted >= total_pages / 2,
        "{counted} of {total_pages} pages"
    );
    Ok(())
}

#[test]
fn the_page_counts_are_what_vm_stat_prints() {
    let host = MacHost::new(Path::new("/")).unwrap();
    // A Mac short of memory hands out gigabytes within the moment `vm_stat` takes to start, so a count may move past
    // the slack once; a field read from the wrong place is off at every look.
    let mut looks = Vec::new();
    for _ in 0..5 {
        match page_counts_beside_vm_stat(&host) {
            Ok(()) => return,
            Err(off) => looks.push(off),
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    panic!("off at every look: {looks:#?}");
}

#[test]
fn pressure_and_swap_are_what_sysctl_prints() {
    let host = MacHost::new(Path::new("/")).unwrap();
    let before = host.raw().unwrap();
    let level: i32 = sysctl("kern.memorystatus_vm_pressure_level").parse().unwrap();
    let after = host.raw().unwrap();
    assert!(
        level == before.pressure_level || level == after.pressure_level,
        "{level}"
    );
    assert!([1, 2, 4].contains(&after.pressure_level));

    // "total = 2048.00M  used = 852.81M  free = 1195.19M  (encrypted)"
    let printed = sysctl("vm.swapusage");
    let megabytes = |name: &str| -> f64 {
        let rest = printed.split(&format!("{name} = ")).nth(1).unwrap();
        rest.split('M').next().unwrap().trim().parse().unwrap()
    };
    let total = after.swap_total_bytes as f64 / 1_048_576.0;
    let used = after.swap_used_bytes as f64 / 1_048_576.0;
    // Swap files come and go in steps of a gigabyte; what is in use moves less between two looks.
    assert!(
        (megabytes("total") - total).abs() <= 2048.0,
        "{printed} against {total}"
    );
    assert!(
        (megabytes("used") - used).abs() <= 512.0,
        "{printed} against {used}"
    );
    assert!(after.swap_used_bytes <= after.swap_total_bytes);
}

#[test]
fn a_reading_is_made_of_the_raw_answers_and_fits_this_mac() {
    let mut host = MacHost::new(Path::new("/")).unwrap();
    let facts = host.facts().unwrap();
    let first = host.read().unwrap();
    let raw = host.raw().unwrap();
    let second = host.read().unwrap();

    assert!(first.available_bytes > 0 && first.available_bytes < facts.total_bytes);
    assert!(first.disk_free_bytes > 0);
    assert!(second.swapouts >= first.swapouts);
    // Time goes on, on both clocks, and the awake clock never runs ahead of the other.
    assert!(second.at.elapsed >= first.at.elapsed && second.at.awake >= first.at.awake);
    assert!(first.at.elapsed >= first.at.awake);
    assert!(first.at.elapsed.0 > 0);
    // The same answers give the same reading: `read` adds nothing of its own.
    let again = reading(facts, second.at, &raw).unwrap();
    assert_eq!(
        (again.pressure, again.thermal, again.on_battery),
        (second.pressure, second.thermal, second.on_battery)
    );
}

#[test]
fn both_clocks_count_milliseconds() {
    let host = MacHost::new(Path::new("/")).unwrap();
    let before = host.now();
    // Longer than a second, so that both the seconds and the fraction of the clock's answer move.
    std::thread::sleep(std::time::Duration::from_millis(1200));
    let after = host.now();
    for (name, passed) in [
        ("elapsed", after.elapsed.0.checked_sub(before.elapsed.0)),
        ("awake", after.awake.0.checked_sub(before.awake.0)),
    ] {
        let passed = passed.unwrap_or_else(|| panic!("the {name} clock went back"));
        assert!(
            (1200..5000).contains(&passed),
            "{name}: {passed} ms for a sleep of 1200 ms"
        );
    }
}

#[test]
fn the_idle_time_is_what_the_hid_system_says() {
    let host = MacHost::new(Path::new("/")).unwrap();
    // `    | |     "HIDIdleTime" = 891834416`, in nanoseconds.
    let printed = || -> Option<u64> {
        output("/usr/sbin/ioreg", &["-c", "IOHIDSystem", "-d", "4"])
            .lines()
            .find(|line| line.contains("\"HIDIdleTime\""))
            .and_then(|line| line.rsplit('=').next()?.trim().parse().ok())
    };
    // A key pressed between two looks starts the time again, so ours before and after must enclose ioreg's in
    // one of a few tries.
    let enclosed = (0..20).any(|_| {
        let before = host.raw().unwrap().idle_nanos;
        let between = printed();
        let after = host.raw().unwrap().idle_nanos;
        match (before, between, after) {
            // A Mac with no HID system to ask: then we say so too.
            (None, None, None) => true,
            (Some(before), Some(between), Some(after)) => before <= between && between <= after,
            _ => false,
        }
    });
    assert!(
        enclosed,
        "ioreg says {:?}, we read {:?}",
        printed(),
        host.raw().unwrap().idle_nanos
    );
}

#[test]
fn the_thermal_level_is_the_notification_state() {
    let host = MacHost::new(Path::new("/")).unwrap();
    // "com.apple.system.thermalpressurelevel 0"
    let printed = || -> u64 {
        output(
            "/usr/bin/notifyutil",
            &["-g", "com.apple.system.thermalpressurelevel"],
        )
        .split_whitespace()
        .last()
        .unwrap()
        .parse()
        .unwrap()
    };
    let before = host.raw().unwrap().thermal_level;
    let level = printed();
    let after = host.raw().unwrap().thermal_level;
    assert!(
        level == before || level == after,
        "{level}, read {before} and {after}"
    );
}

#[test]
fn the_disk_is_the_one_the_path_names() {
    let host = MacHost::new(Path::new("/")).unwrap();
    // `df -k /`: the fourth column of the last line is what is available, in kilobytes.
    let printed = output("/bin/df", &["-k", "/"]);
    let available: u64 = printed
        .lines()
        .last()
        .unwrap()
        .split_whitespace()
        .nth(3)
        .unwrap()
        .parse()
        .unwrap();
    let read = host.raw().unwrap().disk_free_bytes;
    let difference = read.abs_diff(available * 1024);
    assert!(difference < 1 << 30, "df {available} KB, read {read} bytes");
}

#[test]
fn the_capacity_for_important_use_is_what_foundation_says_and_never_below_the_free_space_by_much() {
    let host = MacHost::new(Path::new("/tmp")).unwrap();
    let raw = host.raw().unwrap();
    let important = raw
        .disk_important_bytes
        .expect("macOS says the capacity of a volume it mounts");
    // The free space and the capacity are read a moment apart, and the capacity counts the free space.
    assert!(
        important + (1 << 30) >= raw.disk_free_bytes,
        "{important} for important use, {} free",
        raw.disk_free_bytes
    );
    let script = r#"ObjC.import("Foundation"); var r = Ref(); $.NSURL.fileURLWithPath("/tmp").getResourceValueForKeyError(r, $("NSURLVolumeAvailableCapacityForImportantUsageKey"), null); String(ObjC.unwrap(r[0]))"#;
    let foundation: u64 = output("/usr/bin/osascript", &["-l", "JavaScript", "-e", script])
        .trim()
        .parse()
        .unwrap();
    assert!(
        important.abs_diff(foundation) < 1 << 30,
        "read {important}, Foundation {foundation}"
    );
}

#[test]
fn the_capacity_for_important_use_is_asked_for_once_in_thirty_seconds() {
    // One answer takes about 23 ms and the Runner reads the Mac once a second; the rest of a reading 0.13 ms.
    let host = MacHost::new(Path::new("/tmp")).unwrap();
    assert!(host.raw().unwrap().disk_important_bytes.is_some());
    let started = std::time::Instant::now();
    for _ in 0..50 {
        let raw = host.raw().unwrap();
        assert!(raw.disk_important_bytes.unwrap() >= raw.disk_free_bytes);
    }
    let took = started.elapsed();
    assert!(
        took < std::time::Duration::from_millis(500),
        "50 readings took {took:?}"
    );
}

#[test]
fn a_path_that_is_not_there_is_an_error_and_closes_the_mac() {
    let mut host = MacHost::new(Path::new("/nonexistent/cf-runner-test")).unwrap();
    assert_eq!(
        host.read(),
        Err(HostError::Call {
            what: "statfs",
            code: 2
        })
    );
    let error = MacHost::new(Path::new("/tmp/a\0b")).unwrap_err();
    assert!(matches!(
        error,
        HostError::Implausible {
            what: "the path of the job disks",
            ..
        }
    ));
}

#[test]
fn a_thousand_readings_and_many_hosts_leave_nothing_open() {
    let mut host = MacHost::new(Path::new("/")).unwrap();
    for _ in 0..1000 {
        host.read().unwrap();
    }
    // Each host opens a notification token and a registry entry and gives both back.
    for _ in 0..2000 {
        MacHost::new(Path::new("/")).unwrap().read().unwrap();
    }
}
