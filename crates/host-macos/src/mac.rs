use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io::Read as _;
use std::mem::{MaybeUninit, offset_of, size_of};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::ptr;
use std::time::{Duration, Instant};

use cf_budget::{Host, HostError, HostFacts, HostReading, Now};

use crate::{Battery, Raw, reading};

/// A Core Foundation object.
type CfRef = *const c_void;

/// `kCFStringEncodingUTF8`.
const CF_UTF8: u32 = 0x0800_0100;
/// `kCFNumberSInt64Type`.
const CF_NUMBER_SINT64: isize = 4;
/// `NOTIFY_STATUS_OK`.
const NOTIFY_OK: u32 = 0;
/// `kOSThermalNotificationPressureLevelName`.
const THERMAL_NOTIFICATION: &CStr = c"com.apple.system.thermalpressurelevel";

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CfRef;
    fn IOServiceGetMatchingService(main_port: u32, matching: CfRef) -> u32;
    fn IORegistryEntryCreateCFProperty(entry: u32, key: CfRef, allocator: CfRef, options: u32) -> CfRef;
    fn IOObjectRelease(object: u32) -> c_int;
    safe fn IOPSGetTimeRemainingEstimate() -> f64;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(allocator: CfRef, text: *const c_char, encoding: u32) -> CfRef;
    fn CFGetTypeID(value: CfRef) -> usize;
    safe fn CFNumberGetTypeID() -> usize;
    fn CFNumberGetValue(number: CfRef, kind: isize, value: *mut c_void) -> u8;
    fn CFRelease(value: CfRef);
}

unsafe extern "C" {
    safe fn mach_host_self() -> u32;
    fn notify_register_check(name: *const c_char, token: *mut c_int) -> u32;
    fn notify_get_state(token: c_int, state: *mut u64) -> u32;
    fn notify_cancel(token: c_int) -> u32;
}

/// A type `sysctl` may fill in: integers only, so every pattern of bits is a value.
///
/// # Safety
///
/// The type has no padding and no bit pattern that is not a value of it.
unsafe trait Plain: Copy {}
// SAFETY: integers, and a struct of three u64 and two 32-bit integers with no padding between or after them.
unsafe impl Plain for i32 {}
// SAFETY: as above.
unsafe impl Plain for u64 {}
// SAFETY: as above.
unsafe impl Plain for libc::xsw_usage {}

fn errno() -> i64 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0).into()
}

fn sysctl<T: Plain>(name: &'static CStr) -> Result<T, HostError> {
    let what = name.to_str().unwrap_or("sysctl");
    let mut value = MaybeUninit::<T>::zeroed();
    let mut len = size_of::<T>();
    // SAFETY: the name ends with a NUL; `value` is `len` writable bytes, and the kernel writes at most `len` and
    // stores how many it wrote; nothing is set (no new value).
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            &mut len,
            ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return Err(HostError::Call { what, code: errno() });
    }
    if len != size_of::<T>() {
        return Err(HostError::Implausible {
            what,
            value: format!("{len} bytes instead of {}", size_of::<T>()),
        });
    }
    // SAFETY: every byte was written (zeroed, then filled by the kernel) and `T: Plain` takes any bytes.
    Ok(unsafe { value.assume_init() })
}

/// The HID system's idle time in nanoseconds; `None` when macOS has no HID system (no window server yet) or it
/// gives no such number. The entry is looked up for every reading, so a window server that started again is
/// found again.
fn idle_nanos() -> Option<u64> {
    // SAFETY: the name ends with a NUL. The dictionary `IOServiceMatching` makes is consumed by
    // `IOServiceGetMatchingService`, which is not called with a null one. Port 0 is the default main port.
    let hid = unsafe {
        let matching = IOServiceMatching(c"IOHIDSystem".as_ptr());
        if matching.is_null() {
            return None;
        }
        IOServiceGetMatchingService(0, matching)
    };
    if hid == 0 {
        return None;
    }
    // SAFETY: the text ends with a NUL; a null allocator is the default one. `hid` is the entry found above and
    // `key`, when there is one, a string made here: both are used only up to their release below, once each.
    let value = unsafe {
        let key = CFStringCreateWithCString(ptr::null(), c"HIDIdleTime".as_ptr(), CF_UTF8);
        let value = if key.is_null() {
            ptr::null()
        } else {
            let value = IORegistryEntryCreateCFProperty(hid, key, ptr::null(), 0);
            CFRelease(key);
            value
        };
        IOObjectRelease(hid);
        value
    };
    if value.is_null() {
        return None;
    }
    let mut nanos: i64 = 0;
    // SAFETY: `value` is an object this call owns (it was created for it). It is read as a number only after
    // its type was checked, into an i64, which is what the SInt64 type writes. It is released once.
    let read = unsafe {
        let read = CFGetTypeID(value) == CFNumberGetTypeID()
            && CFNumberGetValue(value, CF_NUMBER_SINT64, (&raw mut nanos).cast()) != 0;
        CFRelease(value);
        read
    };
    if read { u64::try_from(nanos).ok() } else { None }
}

/// `kern.boottime`: a `timeval`, with the four bytes the C struct leaves empty named, so that it has no padding.
#[derive(Clone, Copy)]
#[repr(C)]
struct BootTime {
    secs: i64,
    micros: i32,
    unused: i32,
}
// SAFETY: an i64 and two i32, sixteen bytes with nothing between or after them.
unsafe impl Plain for BootTime {}

/// When the Mac started, in seconds since 1970; `None` when macOS will not say.
pub fn boot_time() -> Option<i64> {
    sysctl::<BootTime>(c"kern.boottime")
        .ok()
        .map(|at| at.secs)
        .filter(|secs| *secs > 0)
}

/// The text up to the first NUL, when it is one.
fn text_of(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
    let text = std::str::from_utf8(&bytes[..end]).ok()?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// A text macOS keeps under a name.
fn sysctl_text(name: &std::ffi::CStr) -> Option<String> {
    let mut text = [0u8; 128];
    let mut len = text.len();
    // SAFETY: the name ends with a NUL; `text` is `len` writable bytes, and the kernel writes at most `len` and
    // stores how many it wrote; nothing is set (no new value).
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            text.as_mut_ptr().cast(),
            &mut len,
            ptr::null_mut(),
            0,
        )
    };
    (status == 0).then(|| text_of(&text[..len.min(text.len())]))?
}

/// The Mac's chip and model as macOS names them (`machdep.cpu.brand_string` and `hw.model`), such as
/// `Apple M5 (Mac17,3)`; `None` when macOS will say neither.
pub fn model() -> Option<String> {
    crate::model_name(
        sysctl_text(c"machdep.cpu.brand_string").as_deref(),
        sysctl_text(c"hw.model").as_deref(),
    )
}

/// How long `pmset` may take to answer. It needs about 10 ms.
const PMSET_ANSWER: Duration = Duration::from_secs(1);

/// The Mac's battery now, as `pmset -g batt` says it; `None` on a Mac without one, and when `pmset` does not
/// answer within a second (it is stopped then). The caller waits for the answer.
pub fn battery() -> Option<Battery> {
    let mut pmset = Command::new("/usr/bin/pmset")
        .args(["-g", "batt"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let asked = Instant::now();
    // Its few lines fit the pipe, so it ends without anybody reading, and what it wrote is read after.
    let ended = loop {
        match pmset.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if asked.elapsed() < PMSET_ANSWER => std::thread::sleep(Duration::from_millis(2)),
            _ => {
                let _ = pmset.kill();
                let _ = pmset.wait();
                break false;
            }
        }
    };
    let mut text = String::new();
    pmset.stdout.take()?.read_to_string(&mut text).ok()?;
    ended.then(|| crate::battery_in(&text))?
}

/// The name macOS gives this start of the Mac (`kern.bootsessionuuid`): another one after every start, and the
/// same until the next, whatever the clock is set to meanwhile. [`boot_time`] is not that: macOS works it out
/// from the clock, so it moves when the clock is set. `None` when macOS will not say.
pub fn boot_session() -> Option<String> {
    sysctl_text(c"kern.bootsessionuuid")
}

/// The program a process runs, by its full path; `None` once the process is gone.
pub fn program_of(pid: c_int) -> Option<PathBuf> {
    let mut buffer = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer is this function's own and its true size is passed; the call writes at most that much.
    let length = unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    let length = usize::try_from(length).ok().filter(|&length| length > 0)?;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buffer[..length])))
}

/// The Mac's host name; `None` when macOS will not say.
pub fn host_name() -> Option<String> {
    let mut text = [0u8; 256];
    // SAFETY: `text` is its own length of writable bytes; one less is passed, so the last byte stays the NUL it is.
    let status = unsafe { libc::gethostname(text.as_mut_ptr().cast(), text.len() - 1) };
    (status == 0).then(|| text_of(&text))?
}

/// The load averages of 1, 5 and 15 minutes; zeros when macOS will not say.
pub fn load() -> [f64; 3] {
    let mut load = [0f64; 3];
    // SAFETY: `load` holds the three numbers asked for.
    let read = unsafe { libc::getloadavg(load.as_mut_ptr(), 3) };
    if read == 3 { load } else { [0.0; 3] }
}

/// The share of the CPUs' time that was not idle between two counts of [`MacHost::cpu_ticks`], in percent; `None`
/// when no time passed between them or a count ran over.
pub fn cpu_percent(before: [u32; 4], now: [u32; 4]) -> Option<f64> {
    let passed = |state: usize| u64::from(now[state].wrapping_sub(before[state]));
    let idle = passed(libc::CPU_STATE_IDLE as usize);
    let all: u64 = (0..4).map(passed).sum();
    // The counts are 32-bit and wrap after more than a year of ticks; a pair that makes no sense says nothing.
    if all == 0 || all > u64::from(u32::MAX / 2) {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    Some((all - idle) as f64 / all as f64 * 100.0)
}

/// Whether an answer of `words` 32-bit words reaches past the last field read here. An older macOS knows fewer
/// fields and writes fewer.
fn has_every_field_read(words: u32) -> bool {
    let needed = offset_of!(libc::vm_statistics64, internal_page_count) + size_of::<libc::natural_t>();
    (words as usize) * size_of::<libc::integer_t>() >= needed
}

/// This Mac. It only reads: nothing it calls changes a setting, a file or another process.
#[derive(Debug)]
pub struct MacHost {
    host_port: u32,
    disk: CString,
    thermal_token: c_int,
}

impl MacHost {
    /// Opens what the readings need. `disk` is a directory on the volume that holds the job disks; it must exist.
    pub fn new(disk: &Path) -> Result<MacHost, HostError> {
        let disk = CString::new(disk.as_os_str().as_bytes()).map_err(|_| HostError::Implausible {
            what: "the path of the job disks",
            value: disk.display().to_string(),
        })?;
        let mut thermal_token: c_int = 0;
        // SAFETY: the name ends with a NUL and the token is a c_int this call may write.
        let status = unsafe { notify_register_check(THERMAL_NOTIFICATION.as_ptr(), &mut thermal_token) };
        if status != NOTIFY_OK {
            return Err(HostError::Call {
                what: "notify_register_check",
                code: status.into(),
            });
        }
        // The port is the process's one right to its host; it is kept for as long as the process lives.
        Ok(MacHost {
            host_port: mach_host_self(),
            disk,
            thermal_token,
        })
    }

    /// What macOS answers now, before anything is made of it.
    pub fn raw(&self) -> Result<Raw, HostError> {
        let memory = self.memory()?;
        let swap: libc::xsw_usage = sysctl(c"vm.swapusage")?;
        let pressure_level: i32 = sysctl(c"kern.memorystatus_vm_pressure_level")?;
        // SAFETY: no arguments; it answers with the page size or -1.
        let page_bytes = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page_bytes = u64::try_from(page_bytes).map_err(|_| HostError::Call {
            what: "sysconf(_SC_PAGESIZE)",
            code: errno(),
        })?;
        Ok(Raw {
            page_bytes,
            free_pages: memory.free_count.into(),
            speculative_pages: memory.speculative_count.into(),
            file_backed_pages: memory.external_page_count.into(),
            purgeable_pages: memory.purgeable_count.into(),
            active_pages: memory.active_count.into(),
            inactive_pages: memory.inactive_count.into(),
            wired_pages: memory.wire_count.into(),
            anonymous_pages: memory.internal_page_count.into(),
            compressor_pages: memory.compressor_page_count.into(),
            swapouts: memory.swapouts,
            pressure_level,
            swap_used_bytes: swap.xsu_used,
            swap_total_bytes: swap.xsu_total,
            idle_nanos: idle_nanos(),
            thermal_level: self.thermal_level()?,
            power_remaining_secs: IOPSGetTimeRemainingEstimate(),
            disk_free_bytes: self.disk_free_bytes()?,
        })
    }

    /// The size of the volume with the job disks, in bytes; `None` when macOS will not say.
    pub fn disk_total_bytes(&self) -> Option<u64> {
        let stats = self.statfs().ok()?;
        Some(stats.f_blocks.saturating_mul(stats.f_bsize.into()))
    }

    /// The time all CPUs spent since the Mac started, in ticks: user, system, idle, nice (`CPU_STATE_*`). Two
    /// counts make a share, see [`cpu_percent`]. `None` when macOS will not say.
    pub fn cpu_ticks(&self) -> Option<[u32; 4]> {
        let mut load = MaybeUninit::<libc::host_cpu_load_info>::zeroed();
        let mut words = libc::HOST_CPU_LOAD_INFO_COUNT;
        // SAFETY: `load` is `words` 32-bit words long, which is what the count says; the kernel writes at most
        // that many and stores how many it wrote.
        let status = unsafe {
            libc::host_statistics(
                self.host_port,
                libc::HOST_CPU_LOAD_INFO,
                load.as_mut_ptr().cast(),
                &mut words,
            )
        };
        if status != 0 || words != libc::HOST_CPU_LOAD_INFO_COUNT {
            return None;
        }
        // SAFETY: the struct is four integers, zeroed and then written in full by the kernel.
        Some(unsafe { load.assume_init() }.cpu_ticks)
    }

    fn memory(&self) -> Result<libc::vm_statistics64, HostError> {
        let mut stats = MaybeUninit::<libc::vm_statistics64>::zeroed();
        let mut words = libc::HOST_VM_INFO64_COUNT;
        // SAFETY: `stats` is `words` 32-bit words long, which is what the count says; the kernel writes at most
        // that many and stores how many it wrote.
        let status = unsafe {
            libc::host_statistics64(
                self.host_port,
                libc::HOST_VM_INFO64,
                stats.as_mut_ptr().cast(),
                &mut words,
            )
        };
        if status != 0 {
            return Err(HostError::Call {
                what: "host_statistics64",
                code: status.into(),
            });
        }
        if !has_every_field_read(words) {
            return Err(HostError::Implausible {
                what: "host_statistics64",
                value: format!("{words} words"),
            });
        }
        // SAFETY: the struct is integers only and was zeroed before the kernel wrote into it.
        Ok(unsafe { stats.assume_init() })
    }

    fn thermal_level(&self) -> Result<u64, HostError> {
        let mut level: u64 = 0;
        // SAFETY: the token came from `notify_register_check` and is cancelled only on drop; `level` is a u64
        // this call may write.
        let status = unsafe { notify_get_state(self.thermal_token, &mut level) };
        if status != NOTIFY_OK {
            return Err(HostError::Call {
                what: "notify_get_state",
                code: status.into(),
            });
        }
        Ok(level)
    }

    /// What macOS says of the volume with the job disks.
    fn statfs(&self) -> Result<libc::statfs, HostError> {
        let mut stats = MaybeUninit::<libc::statfs>::zeroed();
        // SAFETY: the path ends with a NUL and `stats` is a statfs this call may write.
        let status = unsafe { libc::statfs(self.disk.as_ptr(), stats.as_mut_ptr()) };
        if status != 0 {
            return Err(HostError::Call {
                what: "statfs",
                code: errno(),
            });
        }
        // SAFETY: the call succeeded, so it filled the struct in.
        Ok(unsafe { stats.assume_init() })
    }

    fn disk_free_bytes(&self) -> Result<u64, HostError> {
        let stats = self.statfs()?;
        Ok(stats.f_bavail.saturating_mul(stats.f_bsize.into()))
    }
}

impl Host for MacHost {
    fn facts(&self) -> Result<HostFacts, HostError> {
        let total_bytes: u64 = sysctl(c"hw.memsize")?;
        let cores: i32 = sysctl(c"hw.ncpu")?;
        match (total_bytes, u32::try_from(cores)) {
            (1.., Ok(cores @ 1..)) => Ok(HostFacts { total_bytes, cores }),
            _ => Err(HostError::Implausible {
                what: "the Mac's memory and cores",
                value: format!("{total_bytes} bytes, {cores} cores"),
            }),
        }
    }

    /// `CLOCK_UPTIME_RAW` stands still while the Mac sleeps, `CLOCK_MONOTONIC_RAW` does not ([`cf_clock::now`]).
    fn now(&self) -> Now {
        cf_clock::now()
    }

    fn read(&mut self) -> Result<HostReading, HostError> {
        let facts = self.facts()?;
        let at = self.now();
        let raw = self.raw()?;
        reading(facts, at, &raw)
    }
}

impl Drop for MacHost {
    fn drop(&mut self) {
        // SAFETY: the token was opened in `new` and is not used after this.
        unsafe {
            notify_cancel(self.thermal_token);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        boot_session, boot_time, cpu_percent, has_every_field_read, host_name, load, model, text_of,
    };
    use crate::MacHost;

    #[test]
    fn the_mac_says_who_it_is_and_since_when_it_runs() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock")
            .as_secs() as i64;
        let boot = boot_time().expect("a boot time");
        // After 2020 and before now: a number read in the wrong unit or at the wrong offset is neither.
        assert!(boot > 1_577_836_800 && boot <= now, "{boot}");
        assert!(model().is_some_and(|model| model.is_ascii() && model.len() >= 3));
        let session = boot_session().expect("a name for this start");
        // A UUID, whole: one cut short by a buffer too small would name two starts alike.
        assert_eq!(session.len(), 36, "{session}");
        assert!(
            session.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
            "{session}"
        );
        assert_eq!(boot_session(), Some(session));
        assert!(host_name().is_some());
        assert!(load().iter().all(|load| load.is_finite() && *load >= 0.0));
        let host = MacHost::new(std::path::Path::new("/")).expect("a host");
        assert!(host.disk_total_bytes().is_some_and(|bytes| bytes > 1 << 30));
        let ticks = host.cpu_ticks().expect("ticks");
        assert!(ticks[libc::CPU_STATE_IDLE as usize] > 0, "{ticks:?}");
    }

    #[test]
    fn text_ends_at_its_first_nul_and_is_none_when_empty_or_no_text() {
        assert_eq!(text_of(b"Mac16,12\0rest"), Some("Mac16,12".to_string()));
        assert_eq!(text_of(b"no-nul"), Some("no-nul".to_string()));
        assert_eq!(text_of(b"\0"), None);
        assert_eq!(text_of(b""), None);
        assert_eq!(text_of(&[0xff, 0xfe, 0]), None);
    }

    #[test]
    fn the_cpu_in_use_is_the_share_of_ticks_that_were_not_idle() {
        // user, system, idle, nice
        assert_eq!(cpu_percent([100, 100, 100, 0], [130, 110, 160, 0]), Some(40.0));
        assert_eq!(cpu_percent([0, 0, 0, 0], [0, 0, 50, 0]), Some(0.0));
        assert_eq!(cpu_percent([0, 0, 0, 0], [10, 10, 0, 5]), Some(100.0));
        assert_eq!(cpu_percent([5, 5, 5, 5], [5, 5, 5, 5]), None, "no time passed");
        // A count that wrapped around is still the time that passed.
        assert_eq!(cpu_percent([u32::MAX - 9, 0, 0, 0], [10, 0, 20, 0]), Some(50.0));
        // The first count after a restart of the Mac is below the one before: no share is made of that.
        assert_eq!(cpu_percent([1000, 1000, 1000, 0], [10, 10, 10, 0]), None);
    }

    #[test]
    fn an_answer_that_ends_before_the_last_field_read_is_told_apart() {
        // <mach/vm_statistics.h>: `internal_page_count` is the 36th 32-bit word of `vm_statistics64`, and macOS
        // writes 38 today.
        assert!(!has_every_field_read(0));
        assert!(!has_every_field_read(35));
        assert!(has_every_field_read(36));
        assert!(has_every_field_read(libc::HOST_VM_INFO64_COUNT));
    }
}
