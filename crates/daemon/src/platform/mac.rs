//! macOS: [`MacHost`] for the budget, Virtualization.framework through the vm-hosts, Time Machine for the state
//! folder, and the entitlement without which macOS starts no virtual machine.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cf_budget::{HostError, HostFacts, Mode, Settings};
use cf_console_client::Os;
use cf_host_macos::{
    MacHost, battery, boot_session, boot_time, cpu_percent, host_name, load, model, program_of,
};

use super::{Platform, Process};
use crate::{HostExtras, VmHosts};

/// The entitlement without which macOS starts no virtual machine for a program.
const VIRTUALIZATION: &str = "com.apple.security.virtualization";

/// How often the battery is read.
const BATTERY_EVERY: Duration = Duration::from_secs(30);

/// macOS on Apple silicon.
#[derive(Debug, Clone, Copy)]
pub struct Mac;

impl Platform for Mac {
    type Host = MacHost;
    const OS: Os = Os::Macos;
    const SYSTEM: &'static str = "macOS";
    const MODEL: &'static str = "Mac";
    const SERVICE_LOG: &'static str = "launchd.log";

    fn host(state_dir: &Path) -> Result<MacHost, HostError> {
        MacHost::new(state_dir)
    }

    fn settings(facts: HostFacts, mode: Mode) -> Settings {
        Settings::for_host(facts, mode)
    }

    fn boot_session() -> Option<String> {
        boot_session()
    }

    fn host_name() -> Option<String> {
        host_name()
    }

    fn model() -> Option<String> {
        model()
    }

    fn extras(state_dir: &Path) -> Result<Box<dyn Fn() -> HostExtras + Send + Sync>, HostError> {
        // A reader of its own for what only the console's page shows; the machines keep theirs.
        let extras = MacHost::new(state_dir)?;
        let ticks_before = Mutex::new(extras.cpu_ticks());
        // `pmset` is a program that has to run: a report takes the battery as it was read last.
        let battery = watched(BATTERY_EVERY, battery);
        Ok(Box::new(move || {
            let now = extras.cpu_ticks();
            let before = std::mem::replace(&mut *ticks_before.lock().expect("ticks"), now);
            HostExtras {
                load: load(),
                disk_total_bytes: extras.disk_total_bytes().unwrap_or(0),
                swap_total_bytes: extras.raw().map_or(0, |raw| raw.swap_total_bytes),
                cpu_percent: before
                    .zip(now)
                    .and_then(|(before, now)| cpu_percent(before, now))
                    .unwrap_or(-1.0),
                boot: boot_time(),
                battery: *battery.lock().expect("battery"),
            }
        }))
    }

    /// Asks Time Machine to skip the folder: a backup of gigabytes that change with every job helps nobody.
    fn keep_state(state_dir: &Path) -> Option<String> {
        cf_host_macos::exclude_from_backup(state_dir)
            .err()
            .map(|error| format!("{} stays in Time Machine's backups: {error}", state_dir.display()))
    }

    /// Whether the program carries the entitlement macOS asks of a program that starts virtual machines.
    fn fit(program: &Path) -> Result<(), String> {
        let output = std::process::Command::new("/usr/bin/codesign")
            .args(["-d", "--entitlements", "-", "--xml"])
            .arg(program)
            .output()
            .map_err(|error| format!("codesign: {error}"))?;
        if output.status.success() && String::from_utf8_lossy(&output.stdout).contains(VIRTUALIZATION) {
            Ok(())
        } else {
            Err(format!(
                "{} is not signed with the {VIRTUALIZATION} entitlement; build it with `cargo xtask build`",
                program.display()
            ))
        }
    }

    fn process(pid: u32) -> Option<Process> {
        let pid = libc::c_int::try_from(pid).ok()?;
        let started_at = started_at(pid)?;
        let program = program_of(pid)?;
        // The number may have gone to another process while the program was read.
        // macOS keeps the start in seconds as the clock said then, which no later step moves.
        (self::started_at(pid)? == started_at).then_some(Process {
            program,
            started_at,
            mark: None,
        })
    }

    fn vm_hosts(program: &Path, state_dir: &Path) -> VmHosts {
        VmHosts::new(program, state_dir)
    }
}

/// When a process started, in seconds since 1970.
fn started_at(pid: libc::c_int) -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: `info` is `size` writable bytes, which is what is passed; the call writes at most that many and says
    // how many it wrote.
    let written =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), size) };
    if written != size {
        return None;
    }
    // SAFETY: the call wrote the whole struct, which is integers and bytes and was zeroed before.
    Some(unsafe { info.assume_init() }.pbi_start_tvsec)
}

/// What `read` answers: read now, and again every `every` on a thread of its own for as long as someone keeps the
/// answer. For what takes too long to ask on the way to a report, such as a program that has to run.
fn watched<T: Send + 'static>(every: Duration, read: impl Fn() -> T + Send + 'static) -> Arc<Mutex<T>> {
    let seen = Arc::new(Mutex::new(read()));
    let kept = Arc::downgrade(&seen);
    let started = std::thread::Builder::new().name("watched".into()).spawn(move || {
        loop {
            std::thread::sleep(every);
            let Some(seen) = kept.upgrade() else { break };
            let now = read();
            *seen.lock().expect("watched") = now;
        }
    });
    if let Err(error) = started {
        tracing::warn!(%error, "no thread to read again on: what was read at the start stays");
    }
    seen
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    use super::*;

    fn until(what: &str, done: impl Fn() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < Duration::from_secs(5), "{what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn what_is_watched_is_there_at_once_read_again_and_no_longer_once_nobody_keeps_it() {
        let reads = Arc::new(AtomicU32::new(0));
        let seen = {
            let reads = reads.clone();
            watched(Duration::from_millis(5), move || {
                reads.fetch_add(1, Ordering::SeqCst) + 1
            })
        };
        assert!(
            *seen.lock().unwrap() >= 1,
            "the first answer is there when `watched` returns"
        );
        until("a third read", || *seen.lock().unwrap() >= 3);
        drop(seen);
        // A read that was on its way may still end; after it nothing is read any more.
        std::thread::sleep(Duration::from_millis(100));
        let after = reads.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(reads.load(Ordering::SeqCst), after);
    }

    #[test]
    fn a_process_is_its_program_and_its_start_and_a_number_nobody_has_is_none() {
        let me = Mac::process(std::process::id()).expect("this process");
        assert_eq!(me.program, std::env::current_exe().expect("this program"));
        let now = u64::try_from(crate::secs(std::time::SystemTime::now())).expect("a time");
        assert!(me.started_at <= now && me.started_at + 3600 > now, "{me:?}");
        assert_eq!(Mac::process(u32::MAX), None);
    }

    #[test]
    fn a_program_without_the_entitlement_is_not_fit_and_the_error_says_so() {
        let folder = tempfile::tempdir().expect("folder");
        let unsigned = folder.path().join("program");
        std::fs::write(&unsigned, "#!/bin/sh\n").expect("a program");
        let error = Mac::fit(&unsigned).expect_err("unsigned");
        assert!(error.contains(VIRTUALIZATION), "{error}");
    }
}
