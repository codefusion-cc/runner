use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::mem::{MaybeUninit, offset_of, size_of};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use cf_budget::{Host, HostError, HostFacts, HostReading, Millis, Now};

use crate::{Raw, reading};

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

fn sysctl<T: Plain>(name: &'static CStr, what: &'static str) -> Result<T, HostError> {
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

fn clock(id: libc::clockid_t) -> Option<Millis> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `time` is a timespec this call may write.
    let status = unsafe { libc::clock_gettime(id, &mut time) };
    let secs = u64::try_from(time.tv_sec).ok()?;
    let nanos = u64::try_from(time.tv_nsec).ok()?;
    (status == 0).then(|| Millis(secs.saturating_mul(1000).saturating_add(nanos / 1_000_000)))
}

fn hid_system() -> u32 {
    // SAFETY: the name ends with a NUL. The dictionary `IOServiceMatching` makes is consumed by
    // `IOServiceGetMatchingService`, which is not called with a null one. Port 0 is the default main port.
    unsafe {
        let matching = IOServiceMatching(c"IOHIDSystem".as_ptr());
        if matching.is_null() {
            0
        } else {
            IOServiceGetMatchingService(0, matching)
        }
    }
}

/// This Mac. It only reads: nothing it calls changes a setting, a file or another process.
#[derive(Debug)]
pub struct MacHost {
    host_port: u32,
    disk: CString,
    thermal_token: c_int,
    /// The HID system's registry entry, 0 when macOS has none (no window server yet).
    hid: u32,
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
            hid: hid_system(),
        })
    }

    /// What macOS answers now, before anything is made of it.
    pub fn raw(&mut self) -> Result<Raw, HostError> {
        let memory = self.memory()?;
        let swap: libc::xsw_usage = sysctl(c"vm.swapusage", "vm.swapusage")?;
        let pressure_level: i32 = sysctl(
            c"kern.memorystatus_vm_pressure_level",
            "kern.memorystatus_vm_pressure_level",
        )?;
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
            idle_nanos: self.idle_nanos(),
            thermal_level: self.thermal_level()?,
            power_remaining_secs: IOPSGetTimeRemainingEstimate(),
            disk_free_bytes: self.disk_free_bytes()?,
        })
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
        // An older macOS knows fewer fields and writes fewer; the last one read here must be among them.
        let needed = offset_of!(libc::vm_statistics64, internal_page_count) + size_of::<libc::natural_t>();
        if (words as usize) * size_of::<libc::integer_t>() < needed {
            return Err(HostError::Implausible {
                what: "host_statistics64",
                value: format!("{words} words"),
            });
        }
        // SAFETY: the struct is integers only and was zeroed before the kernel wrote into it.
        Ok(unsafe { stats.assume_init() })
    }

    fn idle_nanos(&mut self) -> Option<u64> {
        if self.hid == 0 {
            self.hid = hid_system();
        }
        if self.hid == 0 {
            return None;
        }
        // SAFETY: the text ends with a NUL; a null allocator is the default one.
        let key = unsafe { CFStringCreateWithCString(ptr::null(), c"HIDIdleTime".as_ptr(), CF_UTF8) };
        if key.is_null() {
            return None;
        }
        // SAFETY: `hid` is a registry entry this struct holds, `key` a string that lives until the release below.
        let value = unsafe { IORegistryEntryCreateCFProperty(self.hid, key, ptr::null(), 0) };
        // SAFETY: `key` was made above, is owned here and is not used again.
        unsafe { CFRelease(key) };
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

    fn disk_free_bytes(&self) -> Result<u64, HostError> {
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
        let stats = unsafe { stats.assume_init() };
        Ok(stats.f_bavail.saturating_mul(stats.f_bsize.into()))
    }
}

impl Host for MacHost {
    fn facts(&self) -> Result<HostFacts, HostError> {
        let total_bytes: u64 = sysctl(c"hw.memsize", "hw.memsize")?;
        let cores: i32 = sysctl(c"hw.ncpu", "hw.ncpu")?;
        let cores = u32::try_from(cores).ok().filter(|cores| *cores > 0);
        match (total_bytes, cores) {
            (1.., Some(cores)) => Ok(HostFacts { total_bytes, cores }),
            _ => Err(HostError::Implausible {
                what: "the Mac's memory and cores",
                value: format!("{total_bytes} bytes, {cores:?} cores"),
            }),
        }
    }

    /// Both clocks: `CLOCK_UPTIME_RAW` stands still while the Mac sleeps, `CLOCK_MONOTONIC_RAW` does not. A clock
    /// that cannot be read answers zero, which the budget takes as time gone backwards and closes the Mac.
    fn now(&self) -> Now {
        match (clock(libc::CLOCK_UPTIME_RAW), clock(libc::CLOCK_MONOTONIC_RAW)) {
            (Some(awake), Some(elapsed)) => Now { awake, elapsed },
            _ => Now::default(),
        }
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
        // SAFETY: the token and the registry entry were opened in `new` (or on a later read) and are not used
        // after this.
        unsafe {
            notify_cancel(self.thermal_token);
            if self.hid != 0 {
                IOObjectRelease(self.hid);
            }
        }
    }
}
