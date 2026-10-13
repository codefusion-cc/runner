//! The files of `/proc` and `/sys`, read under a root: `/` on a real host, a folder of fixtures in tests.

use std::fs::File;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use cf_budget::{Battery, HostError};

use crate::convert::{Supply, Trip, TripKind, Zone, battery_of};
use crate::parse::{self, CpuTicks, MAX_FILE_BYTES, Meminfo, Psi};

/// A file under the root, whole. One longer than [`MAX_FILE_BYTES`] or not UTF-8 is an error, never read in part.
pub(crate) fn text(root: &Path, path: &str) -> io::Result<String> {
    let bytes = bytes(root, path)?;
    String::from_utf8(bytes).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))
}

fn bytes(root: &Path, path: &str) -> io::Result<Vec<u8>> {
    // What the kernel writes here is a few kB at most, and its files give no size to start from.
    let mut bytes = Vec::with_capacity(4096);
    File::open(under(root, path))?
        .take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(io::Error::from(io::ErrorKind::FileTooLarge));
    }
    Ok(bytes)
}

/// `path` (absolute, as on a real host) under `root`.
pub(crate) fn under(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches('/'))
}

pub(crate) fn call(what: &'static str, error: &io::Error) -> HostError {
    HostError::Call {
        what,
        code: error.raw_os_error().map_or(-1, i64::from),
    }
}

/// A file every reading needs.
fn needed(root: &Path, path: &'static str) -> Result<String, HostError> {
    text(root, path).map_err(|error| call(path, &error))
}

pub(crate) fn meminfo(root: &Path) -> Result<Meminfo, HostError> {
    parse::meminfo(&needed(root, "/proc/meminfo")?)
}

pub(crate) fn swapouts(root: &Path) -> Result<u64, HostError> {
    parse::vmstat(&needed(root, "/proc/vmstat")?, "pswpout")
        .ok_or_else(|| parse::implausible("pswpout of /proc/vmstat", "missing"))
}

/// `/proc/pressure/memory`; `None` when the kernel has no such file or refuses to read it (PSI built in but off).
/// A file that is there and says something else, text or not, is an error.
pub(crate) fn memory_pressure(root: &Path) -> Result<Option<Psi>, HostError> {
    let path = "/proc/pressure/memory";
    match text(root, path) {
        Ok(text) => parse::psi(&text).map(Some),
        Err(error) if error.raw_os_error().is_some() => Ok(None),
        Err(error) => Err(call(path, &error)),
    }
}

/// The names in a folder that start with `prefix`, sorted; none when the folder is not there.
fn entries(root: &Path, folder: &str, prefix: &str) -> Vec<String> {
    let Ok(listing) = std::fs::read_dir(under(root, folder)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = listing
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with(prefix))
        .collect();
    names.sort();
    names
}

/// The thermal zones that are on and say their temperature. A zone that cannot be read is left out: some
/// drivers refuse while their device sleeps (a Wi-Fi card that is down), and the kernel's own protection holds
/// for every zone anyway.
pub(crate) fn zones(root: &Path) -> Vec<Zone> {
    let folder = "/sys/class/thermal";
    entries(root, folder, "thermal_zone")
        .into_iter()
        .filter_map(|zone| {
            let file = |name: &str| text(root, &format!("{folder}/{zone}/{name}")).ok();
            if file("mode").is_some_and(|mode| mode.trim() == "disabled") {
                return None;
            }
            let millidegrees = parse::millidegrees(&file("temp")?)?;
            // Trip points are numbered from 0 with no gaps.
            let trips = (0..)
                .map_while(|index| {
                    let kind = file(&format!("trip_point_{index}_type"))?;
                    let temp = file(&format!("trip_point_{index}_temp"));
                    Some(TripKind::named(&kind).zip(temp.as_deref().and_then(parse::millidegrees)))
                })
                .flatten()
                .map(|(kind, millidegrees)| Trip { kind, millidegrees })
                .collect();
            Some(Zone {
                name: file("type").map_or_else(|| zone.clone(), |name| name.trim().to_string()),
                millidegrees,
                trips,
            })
        })
        .collect()
}

/// The power supplies of the host; none on a host without any. A device's own battery (`scope` is `Device`) is
/// left out unread past its scope, since reading its state can wake the device. Each supply is read for what its
/// kind has: an outside supply its `online`, a battery its `present`, `status` and `capacity`.
pub(crate) fn supplies(root: &Path) -> Vec<Supply> {
    let folder = "/sys/class/power_supply";
    entries(root, folder, "")
        .into_iter()
        .filter_map(|name| {
            let file = |file: &str| {
                text(root, &format!("{folder}/{name}/{file}"))
                    .ok()
                    .map(|text| text.trim().to_string())
            };
            let flag = |file_name: &str| match file(file_name).as_deref() {
                Some("1") => Some(true),
                Some("0") => Some(false),
                _ => None,
            };
            let battery = file("type")? == "Battery";
            if file("scope").as_deref() == Some("Device") {
                return None;
            }
            Some(if battery {
                Supply {
                    battery,
                    online: None,
                    present: flag("present"),
                    status: file("status"),
                    capacity: file("capacity").and_then(|percent| percent.parse().ok()),
                    name,
                }
            } else {
                Supply {
                    battery,
                    online: flag("online"),
                    present: None,
                    status: None,
                    capacity: None,
                    name,
                }
            })
        })
        .collect()
}

/// The CPUs that are online.
pub(crate) fn online_cpus(root: &Path) -> Result<Vec<u32>, HostError> {
    let path = "/sys/devices/system/cpu/online";
    let text = needed(root, path)?;
    parse::cpu_list(&text)
        .filter(|cpus| !cpus.is_empty())
        .ok_or_else(|| parse::implausible(path, &text))
}

/// The fast CPUs of this host: of the online ones, those of the kind of core with the highest `cpu_capacity`, the
/// big cores of a big.LITTLE board (cpus 4 to 7 of an RK3588). Machines are pinned to them: a vCPU that moves
/// between two kinds of core can make KVM refuse to run it, since its emulated PMU covers one kind.
///
/// The kind is the core's design (`midr_el1`: implementer and part number), so cores of one design whose capacity
/// differs only by their clock, such as two big clusters binned to different top speeds or a prime core beside
/// others of its design, count together. Where a core's design cannot be read, the kind is its capacity. On a host
/// whose cores are alike, or that says nothing of their capacity, all online CPUs. `None` when the online CPUs
/// cannot be read.
pub fn fast_cpus(root: &Path) -> Option<Vec<u32>> {
    let online = online_cpus(root).ok()?;
    let read = |cpu: &u32, name: &str| text(root, &format!("/sys/devices/system/cpu/cpu{cpu}/{name}")).ok();
    let capacities: Option<Vec<u64>> = online
        .iter()
        .map(|cpu| parse::count(&read(cpu, "cpu_capacity")?))
        .collect();
    let Some(capacities) = capacities else {
        return Some(online);
    };
    let fastest = capacities.iter().copied().max()?;
    if capacities.iter().all(|capacity| *capacity == fastest) {
        return Some(online);
    }
    let designs: Option<Vec<u64>> = online
        .iter()
        .map(|cpu| parse::midr_design(&read(cpu, "regs/identification/midr_el1")?))
        .collect();
    let fastest_at = capacities.iter().position(|capacity| *capacity == fastest)?;
    let kinds = designs.unwrap_or(capacities);
    let kind = kinds[fastest_at];
    Some(
        online
            .into_iter()
            .zip(kinds)
            .filter(|(_, of)| *of == kind)
            .map(|(cpu, _)| cpu)
            .collect(),
    )
}

/// The name Linux gives this start of the host (`/proc/sys/kernel/random/boot_id`): another one after every start,
/// and the same until the next, whatever the clock is set to meanwhile. `None` when it cannot be read whole.
pub fn boot_session(root: &Path) -> Option<String> {
    parse::boot_id(&text(root, "/proc/sys/kernel/random/boot_id").ok()?)
}

/// When the host started, in seconds since 1970 (`btime` of `/proc/stat`); `None` when Linux will not say. Linux
/// works it out from the clock, so it moves when the clock is set: [`boot_session`] names a start.
pub fn boot_time(root: &Path) -> Option<i64> {
    parse::btime(&text(root, "/proc/stat").ok()?)
}

/// The host's model: the device tree's (`/proc/device-tree/model`, such as `Generic RK3588 Board`) on a board, else DMI's
/// vendor and product (`/sys/class/dmi/id`); `None` when neither says.
pub fn model(root: &Path) -> Option<String> {
    if let Some(model) = bytes(root, "/proc/device-tree/model")
        .ok()
        .and_then(|bytes| parse::text(&bytes))
    {
        return Some(model);
    }
    let dmi = |name: &str| {
        bytes(root, &format!("/sys/class/dmi/id/{name}"))
            .ok()
            .and_then(|bytes| parse::text(&bytes))
    };
    parse::dmi_model(dmi("sys_vendor").as_deref(), dmi("product_name").as_deref())
}

/// The host's battery now; `None` on a host without one.
pub fn battery(root: &Path) -> Option<Battery> {
    battery_of(&supplies(root))
}

/// The host's name (`/proc/sys/kernel/hostname`, what `gethostname` says); `None` when Linux will not say.
pub fn host_name(root: &Path) -> Option<String> {
    parse::text(&bytes(root, "/proc/sys/kernel/hostname").ok()?)
}

/// The load averages of 1, 5 and 15 minutes (`/proc/loadavg`); zeros when Linux will not say.
pub fn load(root: &Path) -> [f64; 3] {
    text(root, "/proc/loadavg")
        .ok()
        .and_then(|text| parse::loadavg(&text))
        .unwrap_or([0.0; 3])
}

pub(crate) fn cpu_ticks(root: &Path) -> Option<CpuTicks> {
    parse::cpu_ticks(&text(root, "/proc/stat").ok()?)
}

/// A process as far as the Runner tells one from another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Process {
    /// The program it runs, by its full path (`/proc/<pid>/exe`). A program whose file was removed or replaced
    /// since reads with the kernel's ` (deleted)` at its end, so it never passes for the file now at that path.
    pub program: PathBuf,
    /// When it started, in seconds since 1970: [`started_ticks`](Self::started_ticks) after `btime`, as macOS
    /// answers it. A number that went to a later process has a later time. Linux works `btime` out from the wall
    /// clock at each read, so a step of the clock (a board without a battery-backed clock that meets NTP) moves
    /// this time for processes that started before it.
    pub started_at: u64,
    /// When it started, in clock ticks since the host started (`USER_HZ`, field 22 of `/proc/<pid>/stat`). No step
    /// of the wall clock moves it: with [`boot_session`], it names this process for good.
    pub started_ticks: u64,
}

/// Clock ticks a second in what `/proc` shows (`USER_HZ`): 100 on every architecture the Runner is built for.
fn ticks_a_second() -> u64 {
    // SAFETY: no arguments; it answers with the number or -1.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    u64::try_from(ticks)
        .ok()
        .filter(|ticks| *ticks > 0)
        .unwrap_or(100)
}

/// The process with this number, its program and when it started; `None` once it is gone, for a kernel thread,
/// and for a process of another user whose program this one may not see. The start is read before and after the
/// program, and a number that went to another process in between is `None`, so a program and a start never come
/// from two processes.
pub fn process(root: &Path, pid: u32) -> Option<Process> {
    let (program, started_ticks) = program_and_start(root, pid)?;
    let started_at = u64::try_from(boot_time(root)?)
        .ok()?
        .checked_add(started_ticks / ticks_a_second())?;
    Some(Process {
        program,
        started_at,
        started_ticks,
    })
}

/// The program a process runs, by its full path; `None` once the process is gone. [`process`] says when it
/// started too, which tells a number that went to another process apart.
pub fn program_of(root: &Path, pid: u32) -> Option<PathBuf> {
    program_and_start(root, pid).map(|(program, _)| program)
}

fn program_and_start(root: &Path, pid: u32) -> Option<(PathBuf, u64)> {
    let stat = format!("/proc/{pid}/stat");
    program_between(
        || text(root, &stat).ok(),
        || std::fs::read_link(under(root, &format!("/proc/{pid}/exe"))).ok(),
    )
}

/// The program and the start in ticks, from reads of the start before and after the program; `None` when the two
/// starts differ, since the program may then be either process's.
fn program_between(
    mut stat: impl FnMut() -> Option<String>,
    program: impl FnOnce() -> Option<PathBuf>,
) -> Option<(PathBuf, u64)> {
    let mut start = || parse::start_ticks(&stat()?);
    let before = start()?;
    let program = program()?;
    (start()? == before).then_some((program, before))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::program_between;

    const PROGRAM: &str = "/opt/codefusion-runner/bin/codefusion-runner";

    fn stat(start: u64) -> String {
        format!("42 (vm) S 1 42 42 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 3 0 {start} 123 456\n")
    }

    /// The number goes to another process between the two reads of its start: a program read in between may be
    /// either's, so there is no answer. A test of files cannot stop a read half way, so the reads are scripted.
    #[test]
    fn a_number_that_changes_hands_between_two_reads_names_no_process() {
        let answer = |starts: [u64; 2]| {
            let mut starts = starts.into_iter();
            program_between(|| starts.next().map(stat), || Some(PathBuf::from(PROGRAM)))
        };
        assert_eq!(answer([100, 900]), None);
        assert_eq!(answer([900, 100]), None);
        assert_eq!(answer([250, 250]), Some((PathBuf::from(PROGRAM), 250)));
        // Gone before the second read.
        let mut once = Some(stat(250));
        assert_eq!(program_between(|| once.take(), || Some(PROGRAM.into())), None);
    }
}
