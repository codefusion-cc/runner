//! The readers against the Linux they run on, compared with what `free`, `df` and `getconf` say. Memory moves
//! between two reads, so those numbers agree within a margin.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cf_budget::Host;
use cf_host_linux::{
    LinuxHost, boot_session, boot_time, cpu_percent, fast_cpus, host_name, load, model, process, program_of,
};

/// How far memory may move between this crate's read and `free`'s.
const MARGIN: u64 = 128 << 20;

fn root() -> &'static Path {
    Path::new("/")
}

fn output(program: &str, args: &[&str]) -> String {
    let output = Command::new(program).args(args).output().expect(program);
    assert!(output.status.success(), "{program} {args:?}");
    String::from_utf8(output.stdout).expect("text")
}

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn near(one: u64, other: u64) -> bool {
    one.abs_diff(other) <= MARGIN
}

#[test]
fn memory_and_swap_agree_with_free() {
    let mut host = LinuxHost::new(root()).expect("this host");
    let reading = host.read().expect("a reading");
    let raw = host.raw().expect("raw");
    let free = output("free", &["-b"]);
    let row = |name: &str| -> Vec<u64> {
        let line = free.lines().find(|line| line.starts_with(name)).expect(name);
        line.split_whitespace()
            .skip(1)
            .map(|n| n.parse().unwrap())
            .collect()
    };
    let (memory, swap) = (row("Mem:"), row("Swap:"));
    // Mem: total used free shared buff/cache available; Swap: total used free.
    assert_eq!(raw.memory.total_bytes, memory[0]);
    assert_eq!(host.facts().unwrap().total_bytes, memory[0]);
    assert!(
        near(reading.available_bytes, memory[5]),
        "{} against {}",
        reading.available_bytes,
        memory[5]
    );
    assert_eq!(raw.memory.swap_total_bytes, swap[0]);
    assert!(
        near(reading.swap_used_bytes, swap[1]),
        "{} against {}",
        reading.swap_used_bytes,
        swap[1]
    );
    assert!(reading.available_bytes <= raw.memory.total_bytes);
    // PSI is there exactly when the kernel offers the file.
    assert_eq!(
        raw.memory_pressure.is_some(),
        std::fs::read_to_string("/proc/pressure/memory").is_ok()
    );
}

#[test]
fn cores_and_disk_agree_with_the_system() {
    let host = LinuxHost::new(root()).expect("this host");
    let online: u32 = output("getconf", &["_NPROCESSORS_ONLN"]).trim().parse().unwrap();
    assert_eq!(host.facts().unwrap().cores, online);
    let fast = fast_cpus(root()).expect("the fast CPUs");
    assert!(!fast.is_empty() && fast.len() <= online as usize, "{fast:?}");

    let df = output("df", &["-B1", "--output=size,avail", "/"]);
    let numbers: Vec<u64> = df
        .lines()
        .nth(1)
        .unwrap()
        .split_whitespace()
        .map(|n| n.parse().unwrap())
        .collect();
    assert_eq!(host.disk_total_bytes(), Some(numbers[0]));
    let free = host.raw().unwrap().disk_free_bytes;
    assert!(
        free.abs_diff(numbers[1]) <= 256 << 20,
        "{free} against {}",
        numbers[1]
    );
}

#[test]
fn the_host_says_who_it_is_and_since_when_it_runs() {
    let boot = boot_time(root()).expect("a boot time");
    let uptime: f64 = std::fs::read_to_string("/proc/uptime")
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    // btime is now less the time since boot, give or take the second each is rounded to.
    let expected = now_s() as f64 - uptime;
    assert!((boot as f64 - expected).abs() <= 2.0, "{boot} against {expected}");
    let session = boot_session(root()).expect("a name for this start");
    assert_eq!(boot_session(root()), Some(session));
    assert!(host_name(root()).is_some());
    assert!(model(root()).is_none_or(|model| model.len() >= 2));
    assert!(load(root()).iter().all(|load| load.is_finite() && *load >= 0.0));

    let host = LinuxHost::new(root()).unwrap();
    let before = host.cpu_ticks().expect("ticks");
    let spin = std::time::Instant::now();
    while spin.elapsed() < Duration::from_millis(300) {}
    let after = host.cpu_ticks().expect("ticks");
    let share = cpu_percent(before, after).expect("time passed");
    assert!(share > 0.0 && share <= 100.0, "{share}");
}

#[test]
fn a_process_is_known_by_its_program_and_start_until_it_is_gone() {
    let me = process(root(), std::process::id()).expect("this process");
    assert_eq!(me.program, std::env::current_exe().unwrap());
    let started = now_s();
    let mut child = Command::new("sleep").arg("30").spawn().expect("sleep");
    let pid = child.id();
    // The child replaces itself with `sleep` on exec; wait until it did.
    let sleep = std::fs::canonicalize(output("sh", &["-c", "command -v sleep"]).trim()).unwrap();
    let mut seen = None;
    for _ in 0..100 {
        seen = process(root(), pid).filter(|process| process.program == sleep);
        if seen.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let seen = seen.expect("the child runs sleep");
    assert!(
        seen.started_at + 2 >= started && seen.started_at <= now_s(),
        "{seen:?} against {started}"
    );
    assert!(seen.started_at >= me.started_at);
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(process(root(), pid), None, "a process that is gone and reaped");
    assert_eq!(program_of(root(), pid), None);
    // A kernel thread runs no program.
    assert_eq!(program_of(root(), 2), None);
}
