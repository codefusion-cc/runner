//! The process that made a machine, as a sweep finds it again: its number, when it started and the boot it started
//! in. A process number alone says nothing once the kernel gave it to another process, after a wrap or a reboot: a
//! sweep that took any process of that number for the machine's vm-host would keep what it left for ever.

use std::fmt;
use std::sync::OnceLock;

/// A process, told apart from every other that had or will have its number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Owner {
    /// Its process id.
    pub pid: u32,
    /// When it started: on Linux the clock ticks since boot (`/proc/<pid>/stat`), on macOS microseconds since 1970.
    pub start: u64,
    /// The boot it runs in: the first 32 bits of the kernel's boot id.
    pub boot: u32,
}

impl Owner {
    /// This process, read once. `None` where the system does not say when a process started.
    pub fn this() -> Option<Owner> {
        static THIS: OnceLock<Option<Owner>> = OnceLock::new();
        *THIS.get_or_init(|| Owner::of(std::process::id()))
    }

    /// The process that has this number now, if any and the system says when it started.
    pub fn of(pid: u32) -> Option<Owner> {
        Some(Owner {
            pid,
            start: platform::start(pid)?,
            boot: platform::boot()?,
        })
    }

    /// Whether this very process still runs: its number is taken, by a process that started when it did, in this
    /// boot. A zombie that nobody reaped yet still counts.
    pub fn alive(&self) -> bool {
        Owner::of(self.pid).as_ref() == Some(self)
    }

    /// The owner as a label or a file holds it: `<pid>-<start>-<boot, 8 hex digits>`.
    pub fn label(&self) -> String {
        self.to_string()
    }

    /// An owner from its [`Owner::label`]; anything else is `None`.
    pub fn parse(text: &str) -> Option<Owner> {
        let mut parts = text.split('-');
        let (pid, start, boot) = (parts.next()?, parts.next()?, parts.next()?);
        // Written one way only: no sign, no leading zero.
        let digits = |text: &str, most: usize| {
            !text.is_empty()
                && text.len() <= most
                && text.bytes().all(|byte| byte.is_ascii_digit())
                && (text == "0" || !text.starts_with('0'))
        };
        if parts.next().is_some()
            || !digits(pid, 10)
            || !digits(start, 20)
            || boot.len() != 8
            || !boot.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return None;
        }
        Some(Owner {
            pid: pid.parse().ok()?,
            start: start.parse().ok()?,
            boot: u32::from_str_radix(boot, 16).ok()?,
        })
    }
}

impl fmt::Display for Owner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}-{}-{:08x}", self.pid, self.start, self.boot)
    }
}

/// When a process started, from its `/proc/<pid>/stat`: the 22nd field, counted after the program's name, which
/// is in parentheses and may hold spaces and parentheses itself.
pub fn start_of_stat(stat: &str) -> Option<u64> {
    let (_, after) = stat.rsplit_once(')')?;
    after.split_ascii_whitespace().nth(19)?.parse().ok()
}

/// The first 32 bits of a boot id as the kernel gives it (`/proc/sys/kernel/random/boot_id`, macOS's
/// `kern.bootsessionuuid`): a UUID in hex, with or without dashes.
pub fn boot_of(id: &str) -> Option<u32> {
    let hex: String = id
        .trim()
        .chars()
        .filter(|c| *c != '-')
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if hex.len() != 32 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(&hex[..8], 16).ok()
}

#[cfg(target_os = "linux")]
mod platform {
    pub fn start(pid: u32) -> Option<u64> {
        super::start_of_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
    }

    pub fn boot() -> Option<u32> {
        super::boot_of(&std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    pub fn start(pid: u32) -> Option<u64> {
        let pid = libc::c_int::try_from(pid).ok().filter(|pid| *pid > 0)?;
        // SAFETY: an all-zero proc_bsdinfo is a valid value of the plain struct the call fills.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: the call writes at most `size` bytes into `info`, which we own.
        let filled =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
        (filled == size).then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
    }

    pub fn boot() -> Option<u32> {
        let mut buffer = [0u8; 64];
        let mut length = buffer.len();
        // SAFETY: the name is a NUL-ended string; the call writes at most `length` bytes into the buffer we own.
        let read = unsafe {
            libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                buffer.as_mut_ptr().cast(),
                &raw mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        if read != 0 {
            return None;
        }
        let text = std::str::from_utf8(&buffer[..length.min(buffer.len())]).ok()?;
        super::boot_of(text.trim_end_matches('\0'))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    pub fn start(_pid: u32) -> Option<u64> {
        None
    }

    pub fn boot() -> Option<u32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn this_process_is_alive_and_one_of_another_start_or_boot_is_not() {
        let this = Owner::this().expect("the system says when this process started");
        assert_eq!(this.pid, std::process::id());
        assert!(this.alive());
        assert!(
            !Owner {
                start: this.start + 1,
                ..this
            }
            .alive(),
            "another start"
        );
        assert!(
            !Owner {
                boot: this.boot ^ 1,
                ..this
            }
            .alive(),
            "another boot"
        );
    }

    /// After a reboot or a wrap a vm-host's number is some other process's: pid 1 is init or launchd, never a
    /// vm-host, and it must not count as the live owner of what a vm-host of that number left.
    #[test]
    fn a_pid_that_is_now_another_process_is_not_a_live_vm_host() {
        let this = Owner::this().expect("this process");
        // macOS tells another user's process's start to root only.
        let first = Owner::of(1).unwrap_or(Owner {
            pid: 1,
            start: 1,
            boot: this.boot,
        });
        for left in [
            Owner {
                pid: 1,
                start: 0,
                boot: this.boot,
            },
            Owner {
                pid: 1,
                start: first.start + 1,
                boot: this.boot,
            },
            Owner {
                boot: this.boot ^ 0x8000_0000,
                ..first
            },
        ] {
            assert!(!left.alive(), "{left}");
        }
        assert!(!Owner { pid: 0, ..this }.alive());
        assert!(
            !Owner {
                pid: u32::MAX,
                ..this
            }
            .alive()
        );
    }

    #[test]
    fn a_label_is_read_back_as_written() {
        let owner = Owner {
            pid: 4321,
            start: 98_765,
            boot: 0x0a1b_2c3d,
        };
        assert_eq!(owner.label(), "4321-98765-0a1b2c3d");
        assert_eq!(Owner::parse("4321-98765-0a1b2c3d"), Some(owner));
        for refused in [
            "",
            "4321",
            "4321-98765",
            "4321-98765-0a1b2c3",
            "4321-98765-0a1b2c3d-1",
            "4321-98765-0A1B2C3D",
            "-98765-0a1b2c3d",
            "4321--0a1b2c3d",
            "+4321-98765-0a1b2c3d",
            "04321-98765-0a1b2c3d",
            "4321-98765-0a1b2c3g",
            "99999999999-1-00000000",
            "1-999999999999999999999-00000000",
            "1-1-00000000\n",
        ] {
            assert_eq!(Owner::parse(refused), None, "{refused:?}");
        }
    }

    #[test]
    fn the_start_is_the_22nd_field_whatever_the_programs_name_holds() {
        let stat = |name: &str| {
            format!(
                "4321 ({name}) S 1 4321 4321 0 -1 4194560 2041 0 0 0 7 3 0 0 20 0 1 0 1234567 12345678 900 \
                 18446744073709551615 1 1 0 0 0 0 0 4096 0 0 0 17 3 0 0 0 0 0"
            )
        };
        assert_eq!(start_of_stat(&stat("codefusion-runner")), Some(1_234_567));
        assert_eq!(start_of_stat(&stat("a) S 1 2 (b")), Some(1_234_567));
        assert_eq!(start_of_stat(&stat("with space")), Some(1_234_567));
        assert_eq!(start_of_stat("4321 (x) S 1"), None, "cut short");
        assert_eq!(start_of_stat("no parentheses"), None);
        assert_eq!(start_of_stat(""), None);
    }

    #[test]
    fn a_boot_id_gives_its_first_32_bits() {
        assert_eq!(
            boot_of("0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9\n"),
            Some(0x0a1b_2c3d)
        );
        assert_eq!(boot_of("0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9"), Some(0x0a1b_2c3d));
        assert_eq!(boot_of("0a1b2c3d4e5f60718293a4b5c6d7e8f9"), Some(0x0a1b_2c3d));
        for refused in ["", "0a1b2c3d", "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8fz", "ż"] {
            assert_eq!(boot_of(refused), None, "{refused:?}");
        }
    }

    proptest! {
        #[test]
        fn any_owner_is_read_back_from_its_label(pid in any::<u32>(), start in any::<u64>(), boot in any::<u32>()) {
            let owner = Owner { pid, start, boot };
            prop_assert_eq!(Owner::parse(&owner.label()), Some(owner));
        }

        #[test]
        fn any_text_is_an_owner_only_as_its_label(text in "\\PC{0,48}") {
            if let Some(owner) = Owner::parse(&text) {
                prop_assert_eq!(owner.label(), text);
            }
        }

        #[test]
        fn any_stat_line_is_read_without_a_panic(text in "\\PC{0,200}") {
            let _ = start_of_stat(&text);
            let _ = boot_of(&text);
        }
    }
}
