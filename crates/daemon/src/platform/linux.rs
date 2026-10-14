//! Linux: [`LinuxHost`] for the budget with the settings of a host nobody sits at, cloud-hypervisor through the
//! vm-hosts, each started in a systemd scope of its own whose cgroup is delegated to it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cf_budget::{HostError, HostFacts, Mode, Settings};
use cf_console_client::Os;
use cf_host_linux::{
    LinuxHost, battery, boot_session, boot_time, cpu_percent, fast_cpus, host_name, load, model,
};

use super::{Platform, Process};
use crate::{HostExtras, VmHosts};

/// The real file system's root, which the readers of `cf_host_linux` take.
const ROOT: &str = "/";

/// systemd's tool that starts a vm-host in a scope of its own.
const SYSTEMD_RUN: &str = "/usr/bin/systemd-run";

/// Where systemd makes each user's runtime folder, `<RUNTIME>/<uid>`, while the user's manager runs.
pub const RUNTIME: &str = "/run/user";

/// How long the program check may take.
const FIT_LIMIT: Duration = Duration::from_secs(10);

/// Linux with KVM and cgroup v2, under systemd.
#[derive(Debug, Clone, Copy)]
pub struct Linux;

impl Platform for Linux {
    type Host = LinuxHost;
    const OS: Os = Os::Linux;
    const SYSTEM: &'static str = "Linux";
    const MODEL: &'static str = "Linux host";
    const SERVICE_LOG: &'static str = "systemd.log";

    fn host(state_dir: &Path) -> Result<LinuxHost, HostError> {
        LinuxHost::new(state_dir)
    }

    /// The settings of a host nobody works at ([`Settings::for_headless`]), with machines on its fast cores. `mode`
    /// is not used: nobody sits at it.
    fn settings(facts: HostFacts, _mode: Mode) -> Settings {
        let fast = fast_cpus(Path::new(ROOT)).map_or(0, |cpus| u32::try_from(cpus.len()).unwrap_or(0));
        Settings::for_headless(facts, fast)
    }

    fn boot_session() -> Option<String> {
        boot_session(Path::new(ROOT))
    }

    fn host_name() -> Option<String> {
        host_name(Path::new(ROOT))
    }

    fn model() -> Option<String> {
        model(Path::new(ROOT))
    }

    fn extras(state_dir: &Path) -> Result<Box<dyn Fn() -> HostExtras + Send + Sync>, HostError> {
        let host = LinuxHost::new(state_dir)?;
        let ticks_before = Mutex::new(host.cpu_ticks());
        Ok(Box::new(move || {
            let root = Path::new(ROOT);
            let now = host.cpu_ticks();
            let before = std::mem::replace(&mut *ticks_before.lock().expect("ticks"), now);
            HostExtras {
                load: load(root),
                disk_total_bytes: host.disk_total_bytes().unwrap_or(0),
                swap_total_bytes: host.raw().map_or(0, |raw| raw.memory.swap_total_bytes),
                cpu_percent: before
                    .zip(now)
                    .and_then(|(before, now)| cpu_percent(before, now))
                    .unwrap_or(-1.0),
                boot: boot_time(root),
                battery: battery(root),
            }
        }))
    }

    /// Nothing: no backup on Linux knows of the Runner.
    fn keep_state(_state_dir: &Path) -> Option<String> {
        None
    }

    /// Whether the program carries cloud-hypervisor, by asking it (`codefusion-runner vmm`): a build without it
    /// starts no machine.
    fn fit(program: &Path) -> Result<(), String> {
        let not = |why: String| {
            format!(
                "{} carries no cloud-hypervisor ({why}); build it with `cargo xtask build` on Linux for arm64",
                program.display()
            )
        };
        let mut child = Command::new(program)
            .arg("vmm")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| not(error.to_string()))?;
        let began = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(status)) => return Err(not(status.to_string())),
                Ok(None) if began.elapsed() > FIT_LIMIT => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(not("it did not answer".into()));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(error) => return Err(not(error.to_string())),
            }
        }
    }

    fn process(pid: u32) -> Option<Process> {
        let process = cf_host_linux::process(Path::new(ROOT), pid)?;
        Some(Process {
            program: process.program,
            started_at: process.started_at,
        })
    }

    /// Each vm-host in a scope of its own (`systemd-run --user --scope -p Delegate=yes`): its cgroup is delegated
    /// to it, so it can give each machine a cgroup with limits, and the scope is no part of the Runner's own
    /// service, which systemd starts again however often it ends.
    fn vm_hosts(program: &Path, state_dir: &Path) -> VmHosts {
        VmHosts::new(program, state_dir).in_scopes(SYSTEMD_RUN)
    }
}

/// What a program that talks to this user's systemd needs in its environment and may not have, as under
/// `sudo -iu <user>`: the user's runtime folder (`XDG_RUNTIME_DIR`) and the bus in it
/// (`DBUS_SESSION_BUS_ADDRESS`). `get` reads the environment; what it has is kept. Empty when both are there.
///
/// An error, which names the root command that fixes it, when the runtime folder is not there: no systemd user
/// manager runs for the user, because lingering is off and nobody is logged in.
pub fn user_bus_env(
    uid: u32,
    user: &str,
    get: impl Fn(&str) -> Option<OsString>,
    runtime: &Path,
) -> Result<Vec<(&'static str, OsString)>, String> {
    let mut env = Vec::new();
    let folder = match get("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => {
            let dir = runtime.join(uid.to_string());
            env.push(("XDG_RUNTIME_DIR", dir.clone().into_os_string()));
            dir
        }
    };
    if !folder.is_dir() {
        return Err(format!(
            "{} is not there: no systemd user manager runs for {user}. Turn lingering on, as root: `sudo loginctl \
             enable-linger {user}`",
            folder.display()
        ));
    }
    if get("DBUS_SESSION_BUS_ADDRESS").is_none_or(|bus| bus.is_empty()) {
        let mut bus = OsString::from("unix:path=");
        bus.push(folder.join("bus"));
        env.push(("DBUS_SESSION_BUS_ADDRESS", bus));
    }
    Ok(env)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: BTreeMap<String, OsString> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), OsString::from(value)))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn without_a_session_the_users_runtime_folder_and_its_bus_are_named() {
        let runtime = tempfile::tempdir().expect("folder");
        std::fs::create_dir(runtime.path().join("1001")).expect("folder");
        let folder = runtime.path().join("1001");
        let mut bus = OsString::from("unix:path=");
        bus.push(folder.join("bus"));
        assert_eq!(
            user_bus_env(1001, "runner", env(&[]), runtime.path()),
            Ok(vec![
                ("XDG_RUNTIME_DIR", folder.clone().into_os_string()),
                ("DBUS_SESSION_BUS_ADDRESS", bus.clone()),
            ])
        );
        // An empty value is none.
        assert_eq!(
            user_bus_env(
                1001,
                "runner",
                env(&[("XDG_RUNTIME_DIR", ""), ("DBUS_SESSION_BUS_ADDRESS", "")]),
                runtime.path()
            ),
            Ok(vec![
                ("XDG_RUNTIME_DIR", folder.clone().into_os_string()),
                ("DBUS_SESSION_BUS_ADDRESS", bus),
            ])
        );
    }

    #[test]
    fn what_the_environment_has_is_kept() {
        let runtime = tempfile::tempdir().expect("folder");
        let own = runtime.path().join("elsewhere");
        std::fs::create_dir(&own).expect("folder");
        let own_text = own.to_str().expect("a path");
        assert_eq!(
            user_bus_env(
                1001,
                "runner",
                env(&[
                    ("XDG_RUNTIME_DIR", own_text),
                    ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/x/bus")
                ]),
                runtime.path()
            ),
            Ok(vec![])
        );
        let mut bus = OsString::from("unix:path=");
        bus.push(own.join("bus"));
        assert_eq!(
            user_bus_env(
                1001,
                "runner",
                env(&[("XDG_RUNTIME_DIR", own_text)]),
                runtime.path()
            ),
            Ok(vec![("DBUS_SESSION_BUS_ADDRESS", bus)]),
            "the bus is the one in the folder the environment names"
        );
    }

    #[test]
    fn no_runtime_folder_is_lingering_off_and_says_the_command_that_turns_it_on() {
        let runtime = tempfile::tempdir().expect("folder");
        let error =
            user_bus_env(1001, "codefusion-runner", env(&[]), runtime.path()).expect_err("no manager");
        assert_eq!(
            error,
            format!(
                "{}/1001 is not there: no systemd user manager runs for codefusion-runner. Turn lingering on, as \
                 root: `sudo loginctl enable-linger codefusion-runner`",
                runtime.path().display()
            )
        );
    }

    #[test]
    fn a_process_is_its_program_and_its_start_and_a_number_nobody_has_is_none() {
        let me = Linux::process(std::process::id()).expect("this process");
        assert_eq!(me.program, std::env::current_exe().expect("this program"));
        let now = u64::try_from(crate::secs(std::time::SystemTime::now())).expect("a time");
        assert!(me.started_at <= now && me.started_at + 3600 > now, "{me:?}");
        assert_eq!(Linux::process(u32::MAX), None);
    }

    #[test]
    fn a_program_that_carries_no_cloud_hypervisor_is_not_fit_and_the_error_says_how_to_build_one() {
        let folder = tempfile::tempdir().expect("folder");
        let program = |name: &str, body: &str| {
            use std::os::unix::fs::PermissionsExt as _;
            let path = folder.path().join(name);
            std::fs::write(&path, body).expect("a program");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("a mode");
            path
        };
        let without = program("without", "#!/bin/sh\n[ \"$1\" = vmm ] && exit 1\nexit 0\n");
        let error = Linux::fit(&without).expect_err("refused");
        assert!(
            error.contains("carries no cloud-hypervisor") && error.contains("cargo xtask build"),
            "{error}"
        );
        let with = program("with", "#!/bin/sh\n[ \"$1\" = vmm ] && exit 0\nexit 1\n");
        assert_eq!(Linux::fit(&with), Ok(()));
        assert!(Linux::fit(&folder.path().join("missing")).is_err());
    }
}
