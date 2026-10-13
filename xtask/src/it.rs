//! `cargo xtask it`: the Runner against real virtual machines on this Mac, one small machine at a time.
//!
//! GitHub's hosted Macs boot no machines, so this suite runs on a Mac by hand. It needs the built binary
//! (`cargo xtask build`) and a signed image release in a folder (`gh release download <tag> -R <repository> -D
//! target/it/release`), which it installs through the verifying store. Each scenario starts `codefusion-runner vm` the
//! way the daemon does, with a shell script in place of the Actions runner, and judges the job's last state and what
//! the job wrote to the machine's console. The numbers go to `target/it/report.txt`.
//!
//! `service` and `commands` (`it/service.rs`) run the whole service and the binary's own commands against the
//! repository's fakes of GitHub and the console. A scenario that needs more memory than the Mac has free says
//! `SKIPPED` and why.

use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
mod service;
/// The service runs on macOS only; the suite is built wherever the other tasks are.
#[cfg(not(target_os = "macos"))]
mod service {
    use super::{Bench, Fallible};

    pub fn service(_bench: &Bench) -> Fallible<Vec<String>> {
        Err("the service runs on macOS only".into())
    }

    pub fn commands(_bench: &Bench) -> Fallible<Vec<String>> {
        Err("the service runs on macOS only".into())
    }
}

type Fallible<T> = Result<T, Box<dyn Error>>;
/// A scenario answers with the lines of its report.
type Scenario = fn(&Bench) -> Fallible<Vec<String>>;

const MB: u64 = 1 << 20;
const SCRATCH_BYTES: u64 = 30 << 30;
/// The job's share of a machine of 1024 MB: the machine less what its kernel and supervisor keep.
const LIMIT_OF_1024: &str = "805306368";
/// Takes all the memory a job gets but 200 MB, in pieces it writes into, and prints what the guest has touched by
/// then: all its memory but what is free. It holds on for six seconds, so that the Mac's count is read meanwhile.
const HOLDS_ALL_IT_GETS: &str = r#"node -e 'const fs = require("fs"); const kb = (name) => Number(fs.readFileSync("/proc/meminfo", "utf8").match(new RegExp(name + ":\\s+(\\d+)"))[1]); const limit = Number(fs.readFileSync("/sys/fs/cgroup/memory.max", "utf8")); const held = []; for (let left = limit - 200 * 1048576; left > 0; left -= 268435456) held.push(Buffer.alloc(Math.min(left, 268435456), 1)); console.log("touched_kb=" + (kb("MemTotal") - kb("MemFree")) + " total_kb=" + kb("MemTotal") + " limit_kb=" + limit / 1024); setTimeout(() => console.log("held=" + held.length), 6000)'"#;
const MACHINE_PROCESS: &str = "com.apple.Virtualization.VirtualMachine";
const AGENT: &str = "cc.codefusion.runner.it";
const WAITS: &str = "trap '' TERM; echo waiting; sleep 600; echo woke";
/// Holds 700 MB for five seconds and prints what the guest has touched by then: all its memory but what is free.
const HOLDS_700_MB: &str = r#"node -e 'const held = Buffer.alloc(700 << 20, 1); const text = require("fs").readFileSync("/proc/meminfo", "utf8"); const kb = (name) => Number(text.match(new RegExp(name + ":\\s+(\\d+)"))[1]); console.log("touched_kb=" + (kb("MemTotal") - kb("MemFree")) + " total_kb=" + kb("MemTotal")); setTimeout(() => console.log("held=" + held[5]), 5000)'"#;
/// What a job's root tries against the machine's network with nothing but the kernel's own calls, so that the
/// answer does not depend on which tools the image has: a packet socket and a raw socket (either would send what
/// no firewall rule sees), and the firewall's table deleted over netlink, as `nft delete table` would.
const NETWORK_PROBES: &str = r#"
sudo -n perl -e 'print "packet=", (socket(S, 17, 3, 768) ? "0" : "refused"), "\n"'
sudo -n perl -e 'print "rawip=", (socket(S, 2, 3, 1) ? "0" : "refused"), "\n"'
sudo -n perl -e '
  $SIG{ALRM} = sub { print "nfnetlink=silent\n"; exit };
  alarm 5;
  socket(S, 16, 3, 12) or do { print "nfnetlink=refused(socket)\n"; exit };
  sub message { my ($kind, $flags, $family, $resource, $body) = @_; my $rest = pack("CCn", $family, 0, $resource) . $body; pack("LSSLL", 16 + length($rest), $kind, $flags, 0, 0) . $rest }
  my $name = "codefusion\0";
  my $table = pack("SS", 4 + length($name), 1) . $name;
  $table .= "\0" x ((4 - length($table) % 4) % 4);
  my $batch = message(0x10, 1, 0, 10, "") . message((10 << 8) | 2, 1 | 4, 1, 0, $table) . message(0x11, 1, 0, 10, "");
  send(S, $batch, 0, pack("SSLL", 16, 0, 0, 0)) or do { print "nfnetlink=refused(send)\n"; exit };
  recv(S, my $answer, 4096, 0);
  my ($length, $kind, $flags, $sequence, $port, $error) = unpack("LSSLLl", $answer);
  print "nfnetlink=", ($kind == 2 && $error == 0 ? "0" : "refused($error)"), "\n";
'
timeout 3 bash -c "</dev/tcp/2606:4700:4700::1111/443" 2>/dev/null; echo ipv6=$?
sudo -n sh -c 'echo 0 > /proc/sys/net/ipv6/conf/all/disable_ipv6' 2>/dev/null; echo ipv6-on=$?
sudo -n mount -o remount,rw /proc/sys 2>/dev/null; echo remount=$?
try 10.1.2.3 80 private-again; try $gw 22 gateway-again"#;

/// What a job's root tries against its own machine from inside the container: the pipe its end is reported on
/// and the process that holds it, the kernel's settings and what is mounted read-only over them, a lower score
/// for the kernel's choice under memory pressure, swap, and the socket the supervisor talks to the Mac on.
/// `control` is the same kind of try on something a job may do, so a refusal is not just a `sudo` that is missing.
const INSIDE_PROBES: &str = r#"
sudo -n sh -c 'echo x > /tmp/control' 2>/dev/null; echo control=$?
sudo -n sh -c 'echo x > /proc/1/fd/100' 2>/dev/null; echo status-pipe=$?
sudo -n sh -c 'cat /proc/1/environ > /dev/null' 2>/dev/null; echo first-environ=$?
sudo -n sh -c 'echo -1000 > /proc/self/oom_score_adj' 2>/dev/null; echo oom-floor=$?
sudo -n sh -c 'echo "|/tmp/x" > /proc/sys/kernel/core_pattern' 2>/dev/null; echo core-pattern=$?
sudo -n sh -c 'echo x > /run/codefusion/probe' 2>/dev/null; echo write-shared=$?
sudo -n mount -o remount,rw /sys 2>/dev/null; echo remount-sys=$?
sudo -n mount -o remount,rw /run/codefusion 2>/dev/null; echo remount-shared=$?
sudo -n umount -l /proc/sys 2>/dev/null; echo umount-settings=$?
sudo -n umount -l /proc/kcore 2>/dev/null; echo umount-memory=$?
sudo -n umount -l /run/codefusion 2>/dev/null; echo umount-shared=$?
mkdir -p /tmp/at
sudo -n unshare -m -p -f sh -c 'mount -t proc proc /tmp/at' 2>/dev/null; echo fresh-proc=$?
sudo -n unshare -m -n sh -c 'mount -t sysfs sysfs /tmp/at 2>/dev/null && grep -q " /tmp/at sysfs rw" /proc/mounts'; echo fresh-sys=$?
sudo -n sh -c 'dd if=/dev/zero of=/tmp/swap bs=1M count=8 2>/dev/null && chmod 600 /tmp/swap && mkswap /tmp/swap >/dev/null 2>&1 && swapon /tmp/swap' 2>/dev/null; echo swap=$?
echo swaps=$(tail -n +2 /proc/swaps | wc -l)
grep -Eq '^overlay / overlay [^ ]*nodev' /proc/mounts; echo root-nodev=$?
sudo -n perl -e '
  $SIG{ALRM} = sub { print "vsock-host=silent\n"; exit };
  alarm 5;
  socket(S, 40, 1, 0) or do { print "vsock-host=refused(socket)\n"; exit };
  print "vsock-host=", (connect(S, pack("SSLLx4", 40, 0, 6000, 2)) ? "0" : "refused"), "\n"'
sudo -n perl -e '
  socket(S, 40, 1, 0) or do { print "vsock-port=refused(socket)\n"; exit };
  print "vsock-port=", (bind(S, pack("SSLLx4", 40, 0, 6000, 0xFFFFFFFF)) ? "0" : "refused"), "\n"'
exit 7"#;

struct Bench {
    binary: PathBuf,
    state: PathBuf,
    work: PathBuf,
    image: String,
    release: PathBuf,
}

struct Running {
    child: Child,
    job: String,
    began: Instant,
    machines_before: BTreeSet<u32>,
    available_before: u64,
}

struct Finished {
    state: Value,
    console: String,
    took: Duration,
    machines: BTreeSet<u32>,
    machine_peak: u64,
    host_peak: u64,
    /// The most the vm-host said its machine held of the Mac's memory (`resident_bytes` in its state).
    resident_peak: u64,
    available_before: u64,
    available_least: u64,
}

/// Runs the scenarios named after `--only` (all of them otherwise) and fails when one did.
pub fn run(root: &Path, args: &[String]) -> Fallible<()> {
    let mut release = root.join("target/it/release");
    let mut only: Option<Vec<String>> = None;
    let mut options = args.iter();
    while let Some(option) = options.next() {
        let value = options.next().ok_or_else(|| format!("{option} needs a value"))?;
        match option.as_str() {
            "--release" => release = PathBuf::from(value),
            "--only" => only = Some(value.split(',').map(str::to_string).collect()),
            _ => return Err(format!("unknown option {option}").into()),
        }
    }
    let bench = prepare(root, &release)?;
    let scenarios: [(&str, Scenario); 15] = [
        ("boot", boot),
        ("lifecycle", lifecycle),
        ("exit-code", exit_code),
        ("oom", oom),
        ("full-memory", full_memory),
        ("service", service::service),
        ("commands", service::commands),
        ("timeout", timeout),
        ("stop", stop),
        ("sigterm", sigterm),
        ("sigkill", sigkill),
        ("firewall", firewall),
        ("inside", inside),
        ("launchd", launchd),
        ("timers", timers),
    ];
    if let Some(unknown) = only
        .iter()
        .flatten()
        .find(|name| !scenarios.iter().any(|(known, _)| known == name))
    {
        return Err(format!("no scenario {unknown}").into());
    }
    let machines_before = machines();
    let mut report = String::new();
    let mut failed = Vec::new();
    for (name, scenario) in scenarios {
        if only
            .as_ref()
            .is_some_and(|only| !only.iter().any(|wanted| wanted == name))
        {
            continue;
        }
        let began = Instant::now();
        let lines = match scenario(&bench) {
            Ok(lines) => format!(
                "{name}: ok in {:.1} s\n{}",
                began.elapsed().as_secs_f32(),
                indent(&lines)
            ),
            Err(error) => {
                failed.push(name);
                format!("{name}: FAILED: {error}\n")
            }
        };
        print!("{lines}");
        report.push_str(&lines);
        // A scenario that failed half way leaves no machine to the next one.
        within(Duration::from_secs(20), || machines().is_subset(&machines_before));
    }
    let left: Vec<u32> = machines().difference(&machines_before).copied().collect();
    if !left.is_empty() {
        failed.push("a machine is still running");
        report.push_str(&format!("machines left running: {left:?}\n"));
    }
    fs::write(root.join("target/it/report.txt"), &report)?;
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("failed: {}", failed.join(", ")).into())
    }
}

fn prepare(root: &Path, release: &Path) -> Fallible<Bench> {
    let binary = root.join("target/release/codefusion-runner");
    if !binary.exists() {
        return Err("no target/release/codefusion-runner: run `cargo xtask build` first".into());
    }
    let manifest = fs::read_to_string(release.join("image.json")).map_err(|_| {
        format!(
            "no image release in {}: download one with `gh release download <tag> -D <folder>`",
            release.display()
        )
    })?;
    let image = serde_json::from_str::<Value>(&manifest)?["version"]
        .as_str()
        .ok_or("image.json names no version")?
        .to_string();
    let state = root.join("target/it/state");
    let work = root.join("target/it/work");
    fs::create_dir_all(&work)?;
    install(&binary, release, &image, &state)?;
    Ok(Bench {
        binary,
        state,
        work,
        image,
        release: release.to_path_buf(),
    })
}

/// Installs the release into this state folder through the verifying store, unless it is there already.
fn install(binary: &Path, release: &Path, image: &str, state: &Path) -> Fallible<()> {
    if state.join("images").join(image).join("rootfs.img").exists() {
        return Ok(());
    }
    let installed = Command::new(binary)
        .args(["image", "install"])
        .arg(release)
        .args(["--version", image, "--state-dir"])
        .arg(state)
        .status()?;
    if installed.success() {
        Ok(())
    } else {
        Err("the image release was refused".into())
    }
}

fn indent(lines: &[String]) -> String {
    lines.iter().map(|line| format!("  {line}\n")).collect()
}

fn expect(holds: bool, what: impl Into<String>) -> Fallible<()> {
    if holds { Ok(()) } else { Err(what.into().into()) }
}

/// Every line a job's console must show, or the first one it lacks.
fn shows(console: &str, wanted: &[&str]) -> Fallible<()> {
    match wanted.iter().find(|line| !console.contains(**line)) {
        None => Ok(()),
        Some(missing) => Err(format!("the console lacks `{missing}`; its end: {}", tail(console)).into()),
    }
}

fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(12)..].join(" | ")
}

fn within(limit: Duration, mut holds: impl FnMut() -> bool) -> bool {
    let began = Instant::now();
    while began.elapsed() < limit {
        if holds() {
            return true;
        }
        sleep(Duration::from_millis(200));
    }
    holds()
}

fn output(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default()
}

/// The processes that each hold one machine's memory, whoever started them.
fn machines() -> BTreeSet<u32> {
    output("pgrep", &["-f", MACHINE_PROCESS])
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn signal(pid: u32, name: &str) {
    let _ = Command::new("kill")
        .args([&format!("-{name}"), &pid.to_string()])
        .status();
}

/// The memory a process holds, in bytes, as the system charges it: its footprint, or what `ps` says when that tool
/// answers nothing.
fn memory_of(pid: u32) -> u64 {
    let report = output("footprint", &[&pid.to_string()]);
    let footprint = report.split("Footprint: ").nth(1).and_then(|rest| {
        let mut words = rest.split_whitespace();
        let number: f64 = words.next()?.parse().ok()?;
        let unit = match words.next()? {
            "KB" => 1u64 << 10,
            "MB" => 1 << 20,
            "GB" => 1 << 30,
            _ => 1,
        };
        Some((number * unit as f64) as u64)
    });
    footprint.unwrap_or_else(|| {
        output("ps", &["-o", "rss=", "-p", &pid.to_string()])
            .trim()
            .parse::<u64>()
            .unwrap_or(0)
            << 10
    })
}

/// The Mac's memory that is free or given back at once when asked for: free, inactive, speculative, purgeable.
fn available() -> u64 {
    let report = output("vm_stat", &[]);
    let page: u64 = report
        .split("page size of ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next()?.parse().ok())
        .unwrap_or(16384);
    let pages: u64 = report
        .lines()
        .filter(|line| {
            [
                "Pages free",
                "Pages inactive",
                "Pages speculative",
                "Pages purgeable",
            ]
            .iter()
            .any(|name| line.starts_with(name))
        })
        .filter_map(|line| line.rsplit(' ').next()?.trim_end_matches('.').parse::<u64>().ok())
        .sum();
    pages * page
}

fn mb(bytes: u64) -> String {
    format!("{} MB", bytes / MB)
}

fn request(bench: &Bench, job: &str, memory_mb: u64, deadline_s: u64, script: &str) -> Value {
    json!({
        "job": job,
        "host": "the-test-mac",
        "memory_bytes": memory_mb * MB,
        "cpus": 2,
        "scratch_bytes": SCRATCH_BYTES,
        "image": bench.image,
        "workload": { "kind": "command", "argv": ["/bin/bash", "-c", script] },
        "env": [["CI", "true"]],
        "deadline_s": deadline_s,
    })
}

fn console_of(bench: &Bench, job: &str) -> String {
    fs::read(bench.state.join("logs").join(format!("{job}.console.log")))
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

fn state_of(bench: &Bench, job: &str) -> Value {
    fs::read_to_string(bench.state.join("run").join(format!("{job}.json")))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null)
}

/// Removes what an earlier run of this job left.
fn forget(bench: &Bench, job: &str) {
    let run = bench.state.join("run");
    let _ = fs::remove_file(run.join(format!("{job}.json")));
    let _ = fs::remove_file(run.join(format!("{job}.sock")));
    let _ = fs::remove_dir_all(run.join(job));
    let _ = fs::remove_file(bench.state.join("logs").join(format!("{job}.console.log")));
}

/// Starts a vm-host for this job, behind `wrap` when the scenario runs it under another command.
fn start(
    bench: &Bench,
    job: &str,
    memory_mb: u64,
    deadline_s: u64,
    script: &str,
    wrap: &[&str],
) -> Fallible<Running> {
    forget(bench, job);
    let machines_before = machines();
    let available_before = available();
    let mut command = match wrap.split_first() {
        None => Command::new(&bench.binary),
        Some((program, rest)) => {
            let mut command = Command::new(program);
            command.args(rest).arg(&bench.binary);
            command
        }
    };
    let mut child = command
        .args(["vm", "--state-dir"])
        .arg(&bench.state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(fs::File::create(bench.work.join(format!("{job}.stderr")))?)
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("no standard input")?;
    stdin.write_all(
        request(bench, job, memory_mb, deadline_s, script)
            .to_string()
            .as_bytes(),
    )?;
    drop(stdin);
    Ok(Running {
        child,
        job: job.to_string(),
        began: Instant::now(),
        machines_before,
        available_before,
    })
}

/// A job that was not over when its scenario stopped waiting; its vm-host was killed.
#[derive(Debug)]
struct StillRunning(String);

impl std::fmt::Display for StillRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for StillRunning {}

/// Watches a job to its end, measuring its machine meanwhile. `during` sees the console so far and the job's
/// machine processes on every look.
fn finish(
    bench: &Bench,
    mut running: Running,
    limit: Duration,
    during: &mut dyn FnMut(&str, &BTreeSet<u32>) -> Fallible<()>,
) -> Fallible<Finished> {
    let mut seen = BTreeSet::new();
    let (mut machine_peak, mut host_peak, mut available_least) = (0, 0, running.available_before);
    let mut resident_peak = 0;
    let exited = loop {
        if running.child.try_wait()?.is_some() {
            break true;
        }
        if running.began.elapsed() > limit {
            break false;
        }
        let now: BTreeSet<u32> = machines().difference(&running.machines_before).copied().collect();
        machine_peak = machine_peak.max(now.iter().map(|pid| memory_of(*pid)).sum());
        host_peak = host_peak.max(memory_of(running.child.id()));
        let said = state_of(bench, &running.job)["resident_bytes"].as_u64();
        resident_peak = resident_peak.max(said.unwrap_or(0));
        available_least = available_least.min(available());
        seen.extend(now.iter().copied());
        if let Err(error) = during(&console_of(bench, &running.job), &now) {
            let _ = running.child.kill();
            let _ = running.child.wait();
            return Err(error);
        }
        sleep(Duration::from_millis(300));
    };
    if !exited {
        let _ = running.child.kill();
        let _ = running.child.wait();
        return Err(StillRunning(format!(
            "{} was still running after {limit:?}; its console's end: {}",
            running.job,
            tail(&console_of(bench, &running.job))
        ))
        .into());
    }
    let took = running.began.elapsed();
    let mut printed = String::new();
    if let Some(mut stdout) = running.child.stdout.take() {
        stdout.read_to_string(&mut printed)?;
    }
    let state = printed
        .lines()
        .last()
        .and_then(|line| serde_json::from_str(line).ok())
        .unwrap_or(Value::Null);
    Ok(Finished {
        state,
        console: console_of(bench, &running.job),
        took,
        machines: seen,
        machine_peak,
        host_peak,
        resident_peak,
        available_before: running.available_before,
        available_least,
    })
}

fn job(bench: &Bench, name: &str, memory_mb: u64, deadline_s: u64, script: &str) -> Fallible<Finished> {
    let running = start(bench, name, memory_mb, deadline_s, script, &[])?;
    finish(
        bench,
        running,
        Duration::from_secs(deadline_s + 60),
        &mut |_, _| Ok(()),
    )
}

fn end(finished: &Finished) -> &str {
    finished.state["end"]["end"].as_str().unwrap_or("no end")
}

fn ended(finished: &Finished, wanted: &str) -> Fallible<()> {
    expect(
        end(finished) == wanted,
        format!(
            "the job ended `{}`, not `{wanted}`: {}; the console's end: {}",
            end(finished),
            finished.state["end"],
            tail(&finished.console)
        ),
    )
}

/// What the job leaves in the run folder: its state, and neither its socket nor its machine's files.
fn tidy(bench: &Bench, job: &str) -> Fallible<()> {
    let run = bench.state.join("run");
    expect(
        !run.join(format!("{job}.sock")).exists() && !run.join(job).exists(),
        "the job's socket or its machine's files stayed",
    )?;
    expect(
        state_of(bench, job)["stage"] == "ended",
        "the state file does not say the job ended",
    )
}

fn number_after(text: &str, before: &str) -> Option<f64> {
    let rest = text.split(before).nth(1)?;
    let digits: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    digits.parse().ok()
}

/// The milliseconds of a duration printed by Rust's `{:?}` (`271.00ms`, `1.52s`) after `before`.
fn ms_after(text: &str, before: &str) -> Option<f64> {
    let number = number_after(text, before)?;
    let unit: String = text
        .split(before)
        .nth(1)?
        .chars()
        .skip_while(|c| c.is_ascii_digit() || *c == '.')
        .take_while(|c| c.is_alphabetic())
        .collect();
    match unit.as_str() {
        "s" => Some(number * 1000.0),
        "ms" => Some(number),
        "µs" => Some(number / 1000.0),
        "ns" => Some(number / 1_000_000.0),
        _ => None,
    }
}

/// The Mac's load average over the last minute, and how many CPUs it has.
fn load() -> (f64, usize) {
    let printed = Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    let average = number_after(&printed, "{ ").unwrap_or(0.0);
    let cpus = std::thread::available_parallelism().map_or(1, usize::from);
    (average, cpus)
}

fn boot(bench: &Bench) -> Fallible<Vec<String>> {
    let mut ready = Vec::new();
    for _ in 0..5 {
        let printed = Command::new(&bench.binary)
            .args(["boot-proof", "--memory-mb", "512", "--timeout-s", "30"])
            .output()?;
        let text =
            String::from_utf8_lossy(&printed.stdout).into_owned() + &String::from_utf8_lossy(&printed.stderr);
        expect(printed.status.success(), format!("boot-proof failed: {text}"))?;
        ready.push(ms_after(&text, "ready after ").ok_or(format!("no time in `{text}`"))?);
    }
    ready.sort_by(f64::total_cmp);
    Ok(vec![format!(
        "kernel to the supervisor's answer, 512 MB, no disks, 5 boots: least {:.0} ms, median {:.0} ms, most {:.0} ms",
        ready[0], ready[2], ready[4]
    )])
}

fn lifecycle(bench: &Bench) -> Fallible<Vec<String>> {
    let script = r#"id; echo sudo=$(sudo -n id -u); echo cpus=$(nproc); echo limit=$(cat /sys/fs/cgroup/memory.max)
touch /tmp/x "$HOME/x" && echo writable=$HOME
echo x 2>/dev/null > /proc/sys/kernel/hostname || echo procsys=ro
sudo -n sh -c 'echo 1 > /proc/sys/net/ipv4/ip_forward' 2>/dev/null || echo procsys-root=ro
sudo -n sh -c 'echo b > /proc/sysrq-trigger' 2>/dev/null; echo sysrq=tried
"$ACTIONS_RUNNER_HOOK_JOB_STARTED"
echo github=$(curl -s -o /dev/null -w '%{http_code}' --max-time 15 https://github.com)
echo ci=$CI; sleep 5"#;
    let finished = job(bench, "it-lifecycle", 1024, 120, script)?;
    ended(&finished, "ok")?;
    expect(
        finished.state["end"]["summary"]["exit_code"] == 0,
        "the exit code is not 0",
    )?;
    shows(
        &finished.console,
        &[
            "uid=1001(runner)",
            "sudo=0",
            "cpus=2",
            &format!("limit={LIMIT_OF_1024}"),
            "writable=/home/runner",
            "procsys=ro",
            "procsys-root=ro",
            "sysrq=tried",
            "This job runs on the-test-mac with 768 MB memory, 2 CPUs, 30 GB disk.",
            "github=200",
            "ci=true",
        ],
    )?;
    tidy(bench, "it-lifecycle")?;
    expect(
        finished.machines.len() == 1,
        format!("the job had {} machines", finished.machines.len()),
    )?;
    Ok(vec![
        format!(
            "boot to the workload's start: {} ms as the vm-host saw it ({} boots)",
            finished.state["boot_ms"], finished.state["boots"]
        ),
        finished
            .console
            .lines()
            .find(|line| line.contains("workload started"))
            .unwrap_or("no timing line")
            .trim()
            .to_string(),
        format!(
            "the whole job, a 5 s sleep in it: {:.1} s",
            finished.took.as_secs_f32()
        ),
        format!(
            "an idle 1024 MB machine holds {} on the host, its vm-host {}",
            mb(finished.machine_peak),
            mb(finished.host_peak)
        ),
    ])
}

fn exit_code(bench: &Bench) -> Fallible<Vec<String>> {
    let finished = job(bench, "it-exit", 512, 60, "echo failing; exit 7")?;
    ended(&finished, "failed")?;
    expect(
        finished.state["end"]["summary"]["exit_code"] == 7,
        format!(
            "the exit code is {}",
            finished.state["end"]["summary"]["exit_code"]
        ),
    )?;
    tidy(bench, "it-exit")?;
    Ok(vec![
        "exit 7 in a 512 MB machine ends `failed` with code 7".to_string(),
    ])
}

fn oom(bench: &Bench) -> Fallible<Vec<String>> {
    // The job's root first tries to lift its own limit, then a step takes all the memory it gets. The outer
    // shell stands in for the Actions runner and the inner one for a step that goes on after the kill: the step is
    // stopped with all it started once its time is over, and the Actions runner's stand-in sees it fail.
    let script = r#"sudo -n sh -c 'echo max > /sys/fs/cgroup/memory.max' 2>/dev/null; echo lift=$? now=$(cat /sys/fs/cgroup/memory.max)
bash -c 'tail /dev/zero; echo hog=$?; sleep 120; echo step-went-on'
echo step=$?"#;
    let finished = job(bench, "it-oom", 1024, 180, script)?;
    ended(&finished, "oom")?;
    expect(
        finished.took < Duration::from_secs(60),
        format!("the step after the kill ran on for {:?}", finished.took),
    )?;
    shows(&finished.console, &["hog=137", "step=137"])?;
    expect(
        // The console also shows the script, in the shell's word of the kill: only a line of its own counts.
        !finished.console.lines().any(|line| line.trim() == "step-went-on"),
        "the step outlived its time after the kill",
    )?;
    let killed = &finished.state["oom"];
    expect(
        killed.is_object(),
        format!("the state names no kill: {}", finished.state),
    )?;
    let over = finished.machine_peak.saturating_sub(1024 * MB);
    expect(
        finished.machine_peak > 512 * MB,
        format!(
            "the machine was measured at {} only: the measure is wrong",
            mb(finished.machine_peak)
        ),
    )?;
    expect(
        over < 256 * MB,
        format!(
            "the machine's process held {} for a 1024 MB machine",
            mb(finished.machine_peak)
        ),
    )?;
    tidy(bench, "it-oom")?;

    // The workload is itself the process the kernel stops, and the job ends in the same moment: no later look
    // at the kernel's log or counter is there to say why.
    let alone = job(bench, "it-oom-alone", 512, 120, "exec tail /dev/zero")?;
    ended(&alone, "oom")?;
    expect(
        alone.state["oom"].is_object(),
        format!("the state names no kill: {}", alone.state),
    )?;
    tidy(bench, "it-oom-alone")?;

    // What the machine's process holds beyond the memory its guest has touched: the hypervisor's own share.
    let holds = job(bench, "it-memory", 1024, 120, HOLDS_700_MB)?;
    ended(&holds, "ok")?;
    tidy(bench, "it-memory")?;
    let touched =
        number_after(&holds.console, "touched_kb=").ok_or("the job printed no memory")? as u64 * 1024;
    let total = number_after(&holds.console, "total_kb=").ok_or("the job printed no memory")? as u64 * 1024;
    expect(
        touched > 700 * MB && holds.machine_peak > 700 * MB,
        format!(
            "the guest touched {} and its machine held {}: the measure is wrong",
            mb(touched),
            mb(holds.machine_peak)
        ),
    )?;
    Ok(vec![
        finished
            .console
            .lines()
            .find(|line| line.contains("lift="))
            .unwrap_or("no lift line")
            .trim()
            .to_string(),
        format!("the kill as reported: {killed}"),
        format!(
            "a workload that is itself the process stopped (`exec tail /dev/zero`, 512 MB): ended `oom` after {:.1} s",
            alone.took.as_secs_f32()
        ),
        format!(
            "a 1024 MB machine whose job holds 700 MB: the guest had touched {} of the {} its kernel counts, the machine's process held {} at its most ({} beyond what was touched), its vm-host {}",
            mb(touched),
            mb(total),
            mb(holds.machine_peak),
            mb(holds.machine_peak.saturating_sub(touched)),
            mb(holds.host_peak)
        ),
        format!(
            "the machine's process at its most: {} for a 1024 MB machine ({} beyond its size), its vm-host {}",
            mb(finished.machine_peak),
            mb(over),
            mb(finished.host_peak)
        ),
        format!(
            "the Mac's available memory: {} before, {} at its least, {} after",
            mb(finished.available_before),
            mb(finished.available_least),
            mb(available())
        ),
        format!("ended {:.1} s after the start", finished.took.as_secs_f32()),
    ])
}

/// A machine whose job took all it got, as the Mac and the guest counted it.
struct Filled {
    size: u64,
    limit: u64,
    /// What the guest's kernel counts as its memory, and what of it was not free at the job's most.
    total: u64,
    touched: u64,
    machine_peak: u64,
    host_peak: u64,
    /// What the vm-host read of the machine's process at its most: the number the budget counts the machine by.
    resident_peak: u64,
}

impl Filled {
    /// The memory of the machine the guest left free.
    fn free(&self) -> u64 {
        self.total.saturating_sub(self.touched)
    }

    /// What the machine's process and the vm-host hold beyond the machine's size if every page the guest left free
    /// were touched too, and the guest's kernel had touched all it keeps out of its own count (its map of the
    /// memory, its image): the least a full machine costs beyond its size.
    fn at_least(&self) -> u64 {
        (self.machine_peak + self.host_peak + self.free()).saturating_sub(self.size)
    }

    /// The same if the kernel had touched nothing of what it keeps out of its count: no full machine costs more
    /// beyond its size. The kernel's share grows with the machine, so the smallest machine says it closest.
    fn at_most(&self) -> u64 {
        self.machine_peak.saturating_sub(self.touched) + self.host_peak
    }

    fn line(&self) -> String {
        format!(
            "a {} machine whose job takes all it gets (limit {}): the guest had touched {} of the {} its kernel counts; the machine's process held {} at its most (its vm-host read {}), the vm-host itself {}; a full one costs between {} and {} beyond its size",
            mb(self.size),
            mb(self.limit),
            mb(self.touched),
            mb(self.total),
            mb(self.machine_peak),
            mb(self.resident_peak),
            mb(self.host_peak),
            mb(self.at_least()),
            mb(self.at_most())
        )
    }
}

fn filled(bench: &Bench, job_name: &str, size_mb: u64) -> Fallible<Filled> {
    let finished = job(bench, job_name, size_mb, 180, HOLDS_ALL_IT_GETS)?;
    ended(&finished, "ok")?;
    tidy(bench, job_name)?;
    let kb = |name: &str| number_after(&finished.console, name).map(|kb| kb as u64 * 1024);
    let filled = Filled {
        size: size_mb * MB,
        limit: kb("limit_kb=").ok_or("the job printed no limit")?,
        total: kb("total_kb=").ok_or("the job printed no memory")?,
        touched: kb("touched_kb=").ok_or("the job printed no memory")?,
        machine_peak: finished.machine_peak,
        host_peak: finished.host_peak,
        resident_peak: finished.resident_peak,
    };
    let least = filled.limit.saturating_sub(300 * MB);
    expect(
        filled.touched > least && filled.machine_peak > least,
        format!("the measure is wrong: {}", filled.line()),
    )?;
    // The budget counts a machine by what its vm-host reads of the machine's process. A vm-host that finds no such
    // process, or reads it wrong, leaves the budget with the job's peak only: safe, but one machine at a time.
    expect(
        filled.resident_peak > least && filled.resident_peak <= filled.machine_peak + 64 * MB,
        format!(
            "the vm-host read {} of its machine's process, which held {}: {}",
            mb(filled.resident_peak),
            mb(filled.machine_peak),
            filled.line()
        ),
    )?;
    Ok(filled)
}

/// What a machine costs the Mac beyond its size when its job takes all the memory it gets: the number the budget
/// counts every machine with (`vm_host_overhead_bytes`). A `cf-4g`, the size most jobs run in, and a machine of
/// 1024 MB, which bounds the cost from above most closely.
fn full_memory(bench: &Bench) -> Fallible<Vec<String>> {
    const SIZE_MB: u64 = 4096;
    let needed = (SIZE_MB + 2048) * MB;
    if available() < needed {
        return Ok(vec![format!(
            "SKIPPED: {} of memory are available on this Mac and a cf-4g machine that fills up needs {} with the Mac's floor",
            mb(available()),
            mb(needed)
        )]);
    }
    let available_before = available();
    let (large, small) = (
        filled(bench, "it-full", SIZE_MB)?,
        filled(bench, "it-full-small", 1024)?,
    );
    let counted = cf_budget::Settings::VM_HOST_OVERHEAD;
    let lines = vec![
        large.line(),
        small.line(),
        format!(
            "the budget counts {} for a machine beyond its size; the Mac's available memory was {} before",
            mb(counted),
            mb(available_before)
        ),
    ];
    expect(
        large.at_least() <= counted && small.at_least() <= counted && small.at_most() <= counted,
        format!(
            "a machine costs the Mac more beyond its size than the budget counts:\n{}",
            indent(&lines)
        ),
    )?;
    Ok(lines)
}

fn timeout(bench: &Bench) -> Fallible<Vec<String>> {
    let finished = job(bench, "it-timeout", 512, 5, WAITS)?;
    ended(&finished, "timeout")?;
    expect(!finished.console.contains("woke"), "the job went on")?;
    // The deadline (5 s) and its grace (20 s) count from the workload's start. The guest then ends the job
    // itself and says so; only a guest that does not is stopped by the host, 10 s later (`Limits::longest_run`).
    // The job's time is the guest's own count: the vm-host's also holds what the Mac took to start the machine,
    // which is seconds when the scenario before left it short of memory.
    let ran = finished.state["end"]["summary"]["wall_ms"]
        .as_f64()
        .unwrap_or(0.0)
        / 1000.0;
    let took = finished.took.as_secs_f32();
    expect(
        (25.0..35.0).contains(&ran) && f64::from(took) >= ran,
        format!(
            "a 5 s deadline and 20 s of grace: the job ended {ran:.1} s after its start, its vm-host after {took:.1} s"
        ),
    )?;
    expect(
        finished.state["end"]["summary"]["end"]["reason"] == "timeout",
        format!("the guest gave no account of the stop: {}", finished.state["end"]),
    )?;
    tidy(bench, "it-timeout")?;
    Ok(vec![format!(
        "a job that ignores SIGTERM, deadline 5 s: ended `timeout` {ran:.1} s after its start by the guest's count, its vm-host {took:.1} s after its own: 5 s, 20 s of grace, then the guest's kill, its account and the power-off"
    )])
}

fn ask(bench: &Bench, job: &str, request: &Value) -> Fallible<Value> {
    let mut stream = UnixStream::connect(bench.state.join("run").join(format!("{job}.sock")))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(format!("{request}\n").as_bytes())?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

fn stop(bench: &Bench) -> Fallible<Vec<String>> {
    let running = start(bench, "it-stop", 512, 300, WAITS, &[])?;
    let mut asked: Option<Instant> = None;
    let mut answers = Vec::new();
    let finished = finish(bench, running, Duration::from_secs(90), &mut |console, _| {
        if asked.is_none() && console.contains("waiting") {
            answers.push(ask(bench, "it-stop", &json!({ "cmd": "state" }))?);
            answers.push(ask(
                bench,
                "it-stop",
                &json!({ "cmd": "stop", "reason": "evicted-memory", "grace_ms": 3000 }),
            )?);
            asked = Some(Instant::now());
        }
        Ok(())
    })?;
    let asked = asked.ok_or("the job never got to its wait")?;
    ended(&finished, "evicted-memory")?;
    expect(
        answers[0].to_string().contains("\"running\""),
        format!("the state asked for over the socket: {}", answers[0]),
    )?;
    let after = asked.elapsed().as_secs_f32();
    expect(
        (2.5..15.0).contains(&after),
        format!("3 s of grace took {after:.1} s"),
    )?;
    tidy(bench, "it-stop")?;
    Ok(vec![format!(
        "stop over the socket, 3 s of grace, a job that ignores SIGTERM: ended `evicted-memory` {after:.1} s later"
    )])
}

fn sigterm(bench: &Bench) -> Fallible<Vec<String>> {
    let running = start(bench, "it-term", 512, 300, WAITS, &[])?;
    let host = running.child.id();
    let mut sent: Option<Instant> = None;
    let finished = finish(bench, running, Duration::from_secs(90), &mut |console, _| {
        if sent.is_none() && console.contains("waiting") {
            signal(host, "TERM");
            sent = Some(Instant::now());
        }
        Ok(())
    })?;
    let after = sent
        .ok_or("the job never got to its wait")?
        .elapsed()
        .as_secs_f32();
    ended(&finished, "cancelled")?;
    tidy(bench, "it-term")?;
    expect(
        within(Duration::from_secs(10), || {
            finished.machines.iter().all(|pid| !alive(*pid))
        }),
        "the machine outlived its vm-host",
    )?;
    Ok(vec![format!(
        "SIGTERM to the vm-host: the job ended `cancelled` and the machine was gone {after:.1} s later"
    )])
}

fn sigkill(bench: &Bench) -> Fallible<Vec<String>> {
    let mut running = start(bench, "it-kill", 512, 300, WAITS, &[])?;
    let mut machine = BTreeSet::new();
    let ready = within(Duration::from_secs(60), || {
        machine = machines().difference(&running.machines_before).copied().collect();
        console_of(bench, "it-kill").contains("waiting")
    });
    if !ready || machine.len() != 1 {
        let _ = running.child.kill();
        let _ = running.child.wait();
        return Err(format!("the job never got to its wait with one machine ({machine:?})").into());
    }
    signal(running.child.id(), "KILL");
    let killed = Instant::now();
    let _ = running.child.wait();
    let gone = within(Duration::from_secs(15), || machine.iter().all(|pid| !alive(*pid)));
    let after = killed.elapsed().as_millis();
    let stage = state_of(bench, "it-kill")["stage"].clone();
    let answered = ask(bench, "it-kill", &json!({ "cmd": "state" })).is_ok();
    forget(bench, "it-kill");
    expect(gone, "the machine's process outlived a killed vm-host by 15 s")?;
    expect(!answered, "something answered on a killed vm-host's socket")?;
    Ok(vec![format!(
        "SIGKILL to the vm-host: the machine's process was gone within {after} ms; the state file still said {stage} and nothing answered on the socket, which is how the daemon tells"
    )])
}

fn firewall(bench: &Bench) -> Fallible<Vec<String>> {
    let lan = ["en0", "en1"]
        .iter()
        .map(|port| output("ipconfig", &["getifaddr", port]).trim().to_string())
        .find(|address| !address.is_empty());
    let tailnet = output("ifconfig", &[])
        .lines()
        .filter_map(|line| line.trim().strip_prefix("inet 100."))
        .find_map(|rest| {
            let second: u8 = rest.split('.').next()?.parse().ok()?;
            let address = rest.split_whitespace().next()?;
            (64..128).contains(&second).then(|| format!("100.{address}"))
        });
    // Another job's machine, listening, for the first to reach for.
    let other = start(
        bench,
        "it-other",
        512,
        120,
        r#"node -e 'require("http").createServer((q, s) => s.end("x")).listen(8000, "0.0.0.0")' & echo github=$(curl -s -o /dev/null -w '%{http_code}' --max-time 15 https://github.com); sleep 45"#,
        &[],
    )?;
    let mut address = None;
    within(Duration::from_secs(40), || {
        address = console_of(bench, "it-other")
            .split("address ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next().map(str::to_string));
        address.is_some()
    });
    let script = format!(
        r#"gw=$(awk '/nameserver/{{print $2; exit}}' /etc/resolv.conf)
try() {{ timeout 3 bash -c "</dev/tcp/$1/$2" 2>/dev/null; echo "$3=$?"; }}
try $gw 22 gateway-ssh; try $gw 445 gateway-files; try 10.1.2.3 80 private; try 169.254.169.254 80 metadata
try {lan} 22 lan; try {tailnet} 22 tailnet; try {other} 8000 other-machine
echo names=$(getent hosts github.com | wc -l)
echo github=$(curl -s -o /dev/null -w '%{{http_code}}' --max-time 15 https://github.com)
echo ip=$(command -v ip)
sudo -n ip route add 10.0.0.0/8 via $gw 2>/dev/null; echo route=$?
sudo -n ip link set lo down 2>/dev/null; echo link=$?
echo tools=$(command -v ip iptables nft ifconfig | tr '\n' ' ')
sudo -n iptables -P OUTPUT ACCEPT 2>/dev/null; echo iptables=$?
sudo -n nft flush ruleset 2>/dev/null; echo nft=$?
sudo -n hostname other 2>/dev/null; echo hostname=$?
sudo -n date -s 2001-01-01 >/dev/null 2>&1; echo clock=$?
sudo -n mknod /tmp/disk b 254 0 2>/dev/null; echo mknod=$?"#,
        lan = lan.as_deref().unwrap_or("192.168.1.1"),
        tailnet = tailnet.as_deref().unwrap_or("100.100.100.100"),
        other = address.as_deref().unwrap_or("192.168.64.250"),
    ) + NETWORK_PROBES;
    let first = start(bench, "it-firewall", 512, 120, &script, &[]).and_then(|running| {
        finish(bench, running, Duration::from_secs(120), &mut |_, machines| {
            // The other job's machine is not this one's.
            expect(machines.len() <= 1, "two machines for one job")
        })
    });
    let other = finish(bench, other, Duration::from_secs(120), &mut |_, _| Ok(()))?;
    let first = first?;
    let address = address.ok_or("the other machine got no address")?;
    ended(&first, "ok")?;
    ended(&other, "ok")?;
    // 124 is `timeout` giving up: the packet was dropped. A refusal would be 1 and a connection 0.
    shows(
        &first.console,
        &[
            "gateway-ssh=124",
            "gateway-files=124",
            "private=124",
            "metadata=124",
            "lan=124",
            "tailnet=124",
            "other-machine=124",
            "names=1",
            "github=200",
            "packet=refused",
            "rawip=refused",
            "nfnetlink=refused",
            "private-again=124",
            "gateway-again=124",
        ],
    )?;
    shows(&other.console, &["github=200"])?;
    expect(
        [
            "route=0",
            "link=0",
            "iptables=0",
            "nft=0",
            "hostname=0",
            "clock=0",
            "mknod=0",
            "ipv6=0",
            "ipv6-on=0",
            "remount=0",
        ]
        .iter()
        .all(|done| !first.console.contains(done)),
        format!(
            "the job's root changed the machine itself: {}",
            tail(&first.console)
        ),
    )?;
    let own = first
        .console
        .split("address ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or("none")
        .to_string();
    expect(
        own != address && own != "none",
        "the two machines share an address",
    )?;
    Ok(vec![
        "two machines at once under NAT, each with its own address, both reached github.com".to_string(),
        format!(
            "dropped (timeout's 124): the gateway's ports 22 and 445, 10.1.2.3, 169.254.169.254, the Mac's LAN address ({}), its tailnet address ({}), the other machine's port 8000",
            if lan.is_some() {
                "real"
            } else {
                "none here, a stand-in"
            },
            if tailnet.is_some() {
                "real"
            } else {
                "none here, a stand-in"
            }
        ),
        first
            .console
            .lines()
            .filter(|line| {
                [
                    "tools=",
                    "route=",
                    "link=",
                    "iptables=",
                    "nft=",
                    "hostname=",
                    "clock=",
                    "mknod=",
                    "packet=",
                    "rawip=",
                    "nfnetlink=",
                    "ipv6=",
                    "ipv6-on=",
                    "remount=",
                ]
                .iter()
                .any(|name| line.starts_with(name))
            })
            .collect::<Vec<_>>()
            .join(" "),
    ])
}

/// The last value a job printed as `name=value`.
fn printed<'a>(console: &'a str, name: &str) -> Option<&'a str> {
    console
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix(name)?.strip_prefix('='))
}

fn inside(bench: &Bench) -> Fallible<Vec<String>> {
    const REFUSED: [&str; 16] = [
        "status-pipe",
        "first-environ",
        "oom-floor",
        "core-pattern",
        "write-shared",
        "remount-sys",
        "remount-shared",
        "umount-settings",
        "umount-memory",
        "umount-shared",
        "fresh-proc",
        "fresh-sys",
        "swap",
        "vsock-host",
        "vsock-port",
        "root-nodev",
    ];
    let finished = job(bench, "it-inside", 512, 120, INSIDE_PROBES)?;
    let value = |name: &str| printed(&finished.console, name).unwrap_or("not tried");
    let line = REFUSED
        .iter()
        .chain(&["control", "swaps"])
        .map(|name| format!("{name}={}", value(name)))
        .collect::<Vec<_>>()
        .join(" ");
    expect(
        value("control") == "0",
        format!("the job's root could not even write a file of its own: {line}"),
    )?;
    // `root-nodev` is a question, the others are tries: the root has no device files when the answer is 0.
    let open: Vec<&str> = REFUSED
        .iter()
        .copied()
        .filter(|name| match (*name, value(name)) {
            (_, "not tried") => true,
            ("root-nodev", answer) => answer != "0",
            (_, answer) => answer == "0",
        })
        .collect();
    expect(
        open.is_empty() && value("swaps") == "0",
        format!(
            "the job's root got at its own machine ({}): {line}",
            open.join(", ")
        ),
    )?;
    // What the job wrote at the pipe its end is reported on changed nothing: its end is its own exit code.
    ended(&finished, "failed")?;
    expect(
        finished.state["end"]["summary"]["exit_code"] == 7,
        format!("the job's end is not its own: {}", finished.state["end"]),
    )?;
    tidy(bench, "it-inside")?;
    Ok(vec![
        "refused to the job's root, whose job then ended with its own exit code 7:".to_string(),
        line,
    ])
}

fn launchd(bench: &Bench) -> Fallible<Vec<String>> {
    let job = "it-launchd";
    let uid = output("id", &["-u"]).trim().to_string();
    // An agent touches no other volume without the owner's consent, and waits for it for good. So all it touches
    // is in the temporary folder: the binary, its own files and a state folder with the image.
    let folder = std::env::temp_dir().join(AGENT);
    let _ = fs::remove_dir_all(&folder);
    fs::create_dir_all(&folder)?;
    let path = |name: &str| folder.join(name).display().to_string();
    let binary = folder.join("codefusion-runner");
    fs::copy(&bench.binary, &binary)?;
    let bench = &Bench {
        binary: binary.clone(),
        state: folder.join("state"),
        work: folder.clone(),
        image: bench.image.clone(),
        release: bench.release.clone(),
    };
    install(&binary, &bench.release, &bench.image, &bench.state)?;
    fs::write(
        path("launchd.json"),
        request(bench, job, 512, 120, "echo waiting; sleep 15; echo survived").to_string(),
    )?;
    // The daemon's stand-in: it starts a vm-host and is gone two seconds later.
    fs::write(
        path("launchd.sh"),
        format!(
            "#!/bin/sh\n'{}' vm --state-dir '{}' < '{}' > '{}' 2> '{}' &\nsleep 2\n",
            path("codefusion-runner"),
            bench.state.display(),
            path("launchd.json"),
            path("launchd.out"),
            path("launchd.err")
        ),
    )?;
    fs::write(
        path("launchd.plist"),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{AGENT}</string>
<key>ProgramArguments</key><array><string>/bin/sh</string><string>{}</string></array>
<key>RunAtLoad</key><true/>
</dict></plist>
"#,
            path("launchd.sh")
        ),
    )?;
    let domain = format!("gui/{uid}");
    let loaded = Command::new("launchctl")
        .args(["bootstrap", &domain, &path("launchd.plist")])
        .status()?;
    expect(loaded.success(), "launchctl bootstrap refused the test's agent")?;
    let outcome = (|| {
        // The first job in a state folder makes its disk's template first.
        let started = within(Duration::from_secs(90), || {
            console_of(bench, job).contains("waiting")
        });
        let said =
            fs::read_to_string(path("launchd.err")).unwrap_or_else(|error| format!("no output: {error}"));
        expect(
            started,
            format!("the job never started under launchd: {}", tail(&said)),
        )?;
        let gone = within(Duration::from_secs(10), || {
            !output("launchctl", &["print", &format!("{domain}/{AGENT}")]).contains("state = running")
        });
        expect(gone, "the agent's own process did not end")?;
        let host = state_of(bench, job)["pid"]
            .as_u64()
            .ok_or("the state file names no pid")? as u32;
        sleep(Duration::from_secs(3));
        expect(
            alive(host),
            "launchd took the vm-host with the agent that started it",
        )?;
        expect(
            within(Duration::from_secs(60), || {
                state_of(bench, job)["stage"] == "ended"
            }),
            "the job did not end",
        )?;
        let state = state_of(bench, job);
        expect(
            state["end"]["end"] == "ok" && console_of(bench, job).contains("survived"),
            format!("the job ended {}", state["end"]),
        )?;
        expect(
            within(Duration::from_secs(10), || !alive(host)),
            "the vm-host stayed after its job",
        )
    })();
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{AGENT}")])
        .stderr(Stdio::null())
        .status();
    // A vm-host that is still there has failed the scenario: it is not left to run on.
    let hosts = || output("pgrep", &["-f", &path("codefusion-runner")]);
    for host in hosts().lines().filter_map(|line| line.trim().parse().ok()) {
        signal(host, "KILL");
    }
    within(Duration::from_secs(20), || hosts().trim().is_empty());
    let _ = fs::remove_dir_all(&folder);
    outcome?;
    Ok(vec![
        "a vm-host started by a launchd agent that ended 2 s later ran its job to `ok`, 15 s on".to_string(),
    ])
}

fn timers(bench: &Bench) -> Fallible<Vec<String>> {
    // 1500 timers of 2 ms, how late each fired, then how many times a loop spins in a second.
    let script = r#"node -e '
const d = []; let n = 0;
function step() { const t = performance.now(); setTimeout(() => { d.push(performance.now() - t - 2); if (++n < 1500) step(); else done(); }, 2); }
function done() { d.sort((a, b) => a - b); let spins = 0; const until = performance.now() + 1000; while (performance.now() < until) spins++;
  console.log("late-us p50=" + Math.round(d[750] * 1000) + " p99=" + Math.round(d[1485] * 1000) + " most=" + Math.round(d[1499] * 1000) + " spins=" + spins); }
step();'"#;
    let mut lines = Vec::new();
    for (name, wrap) in [
        ("as started", &[][..]),
        ("taskpolicy -c utility", &["taskpolicy", "-c", "utility"][..]),
        (
            "taskpolicy -c background",
            &["taskpolicy", "-c", "background"][..],
        ),
    ] {
        let running = start(bench, "it-timers", 1024, 120, script, wrap)?;
        let finished = match finish(bench, running, Duration::from_secs(150), &mut |_, _| Ok(())) {
            Ok(finished) => finished,
            // A clamped tier gets what the Mac's other work leaves. With more work than CPUs that can be nothing,
            // which is what this scenario is here to show, so it is a number and no failure. Any other end is one.
            Err(error) => {
                let (average, cpus) = load();
                if wrap.is_empty() || !error.is::<StillRunning>() || average <= cpus as f64 {
                    return Err(error);
                }
                lines.push(format!(
                    "{name}: not done on a Mac with a load average of {average:.0} on {cpus} CPUs: {error}"
                ));
                continue;
            }
        };
        ended(&finished, "ok")?;
        let line = finished
            .console
            .lines()
            .find(|line| line.contains("late-us"))
            .ok_or("the job printed no timing")?;
        lines.push(format!(
            "{name}: {} (boot {} ms)",
            line.trim(),
            finished.state["boot_ms"]
        ));
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::ms_after;

    #[test]
    fn a_boot_time_is_read_in_its_own_unit() {
        let line = |time: &str| format!("kernel 6.12: ready after {time}, ended after 2.10s (power-off)");
        assert_eq!(ms_after(&line("271.00ms"), "ready after "), Some(271.0));
        assert_eq!(ms_after(&line("1.52s"), "ready after "), Some(1520.0));
        assert_eq!(ms_after(&line("900.00µs"), "ready after "), Some(0.9));
        assert_eq!(ms_after(&line("soon"), "ready after "), None);
        assert_eq!(ms_after("nothing", "ready after "), None);
    }
}
