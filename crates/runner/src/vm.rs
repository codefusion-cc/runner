//! The commands that start virtual machines: `vm`, the vm-host of one job, and `boot-proof`.

use cf_guest_proto::Task;
use cf_vmhost::task::run_task;
use cf_vmhost::{Guest, JobRequest, Layout, Limits};
use cf_vmm::{Network, VmSpec};
use std::io::{Read, Write};
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Set by a signal that asks the vm-host to end its job.
static CANCEL: AtomicBool = AtomicBool::new(false);

extern "C" fn cancel(_signal: libc::c_int) {
    CANCEL.store(true, Ordering::Relaxed);
}

/// What the host chooses for its machines.
#[derive(Debug, Default)]
pub struct Host {
    /// `--host-cpus`: the host CPUs the machines run on, a kernel CPU list such as `4-7`; by default the fastest
    /// this process may use. Linux only.
    pub cpus: Option<String>,
}

/// The monitor of this host's machines: Virtualization.framework.
#[cfg(target_os = "macos")]
fn monitor(_machines: &Path, _programs: &Path, host: &Host) -> Result<cf_vmm_vz::VzVmm, String> {
    if host.cpus.is_some() {
        return Err("--host-cpus is for Linux".into());
    }
    cf_vmm_vz::VzVmm::new().map_err(|error| error.to_string())
}

/// The monitor of this host's machines: cloud-hypervisor as this binary carries it, unpacked into `programs`,
/// with the system's pasta, each machine with a folder of its own in `machines`, its sandbox this same binary.
#[cfg(target_os = "linux")]
fn monitor(machines: &Path, programs: &Path, host: &Host) -> Result<cf_vmm_ch::ChVmm, String> {
    let host_cpus = match host.cpus.as_deref() {
        Some(text) => {
            let cpus = cf_vmm_ch::cpus::parse_list(text).map_err(|error| format!("--host-cpus: {error}"))?;
            if cpus.is_empty() {
                return Err("--host-cpus names no CPU".into());
            }
            Some(cpus)
        }
        None => None,
    };
    let cloud_hypervisor = cf_vmm_ch::programs::unpack(programs)?;
    let sandbox = std::env::current_exe().map_err(|error| format!("this program's path: {error}"))?;
    cf_vmm_ch::ChVmm::new(cf_vmm_ch::Config {
        dir: machines.to_path_buf(),
        cloud_hypervisor,
        pasta: None,
        sandbox,
        host_cpus,
        host_networks: cf_vmhost::host_networks(),
    })
    .map_err(|error| error.to_string())
}

/// The guest this binary carries, verified before a byte of it reaches the hypervisor.
fn guest() -> Result<Guest<'static>, String> {
    let verified = cf_image::verify_embedded()
        .map_err(|error| format!("the embedded guest image is refused: {error}"))?;
    let bundle = cf_image::embedded_bundle().ok_or("no embedded guest image")?;
    let file = |name: &str, what: &str| {
        bundle
            .file(name)
            .ok_or(format!("the embedded image has no {what}"))
    };
    Ok(Guest {
        kernel: file(&verified.manifest.kernel.name, "kernel")?,
        initramfs: file(&verified.manifest.initramfs.name, "initramfs")?,
    })
}

/// The vm-host of one job. The job's request comes on standard input, since it holds the job's registration with
/// GitHub; the job's last state goes to standard output as one line of JSON.
pub fn host(state_dir: &str, host: &Host) -> Result<(), String> {
    // A session of its own: the job does not end with the daemon that started it.
    // SAFETY: setsid takes no arguments; it fails, harmlessly, when a shell started this process as a group's leader.
    unsafe { libc::setsid() };
    let mut json = String::new();
    std::io::stdin()
        .take(1 << 20)
        .read_to_string(&mut json)
        .map_err(|error| format!("the request: {error}"))?;
    let request = JobRequest::parse(&json).map_err(|error| error.to_string())?;
    drop(json);
    let guest = guest()?;
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: the handler only stores into an atomic, which a signal handler may do.
        unsafe { libc::signal(signal, cancel as extern "C" fn(libc::c_int) as libc::sighandler_t) };
    }
    let layout = Layout::new(state_dir);
    let vmm = monitor(&layout.machines(), &layout.programs(), host)?;
    let limits = Limits::for_deadline(Duration::from_secs(request.deadline_s));
    let state =
        cf_vmhost::run(&vmm, &layout, guest, request, limits, &CANCEL).map_err(|error| error.to_string())?;
    // The daemon may be gone by now; the state file has the same.
    let _ = writeln!(
        std::io::stdout(),
        "{}",
        serde_json::to_string(&state).unwrap_or_default()
    );
    Ok(())
}

/// Boots the embedded guest with no disk and no network, has it answer and power off, and prints what happened.
pub fn boot_proof(memory_bytes: u64, timeout: Duration, host: &Host) -> Result<(), String> {
    let guest = guest()?;
    let scratch = Scratch::new("proof")?;
    let console = scratch.0.join("console.log");
    let spec = VmSpec {
        kernel: scratch.write("kernel", guest.kernel)?,
        initramfs: scratch.write("initramfs", guest.initramfs)?,
        memory_bytes,
        cpus: 1,
        disks: Vec::new(),
        network: Network::None,
        console: console.clone(),
    };
    let vmm = monitor(
        &scratch.path().join("machines"),
        &scratch.path().join("programs"),
        host,
    )?;
    let report = run_task(&vmm, &spec, Task::Probe, timeout, &|| false);
    print!("{}", std::fs::read_to_string(&console).unwrap_or_default());
    let report = report?;
    println!(
        "kernel {}: ready after {:.2?}, ended after {:.2?} ({}); the machine held {} of the host's memory",
        report.kernel,
        report.ready_after,
        report.ended_after,
        if report.powered_off {
            "guest powered itself off"
        } else {
            "stopped by the host"
        },
        report
            .resident
            .map_or("an amount nobody could read".into(), |bytes| format!(
                "{} MiB",
                bytes >> 20
            ))
    );
    if report.powered_off {
        Ok(())
    } else {
        Err("the guest did not power off by itself".into())
    }
}

/// A folder of this process alone under the temporary folder, `codefusion-runner-<what>-<pid>`, removed when it goes
/// out of scope, however the work ends. Nobody else can write there: what the hypervisor loads from it is what was
/// verified, and a state folder made in it holds only what this process put there.
pub(crate) struct Scratch(PathBuf);

impl Scratch {
    pub(crate) fn new(what: &str) -> Result<Self, String> {
        Self::under(&std::env::temp_dir(), what)
    }

    fn under(folder: &Path, what: &str) -> Result<Self, String> {
        let path = folder.join(format!("codefusion-runner-{what}-{}", std::process::id()));
        // Made new and private in one step. A folder already there is not ours to trust, whoever left it.
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        Ok(Self(path))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    /// Writes a file that did not exist, readable by this user only.
    fn write(&self, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
        let path = self.0.join(name);
        cf_vmhost::write_private(&path, bytes).map_err(|error| format!("{}: {error}", path.display()))?;
        Ok(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn a_scratch_folder_is_new_for_this_user_alone_and_one_already_there_is_not_taken() {
        let folder = tempfile::tempdir().expect("folder");
        let scratch = Scratch::under(folder.path(), "test").expect("made");
        let path = scratch.path().to_path_buf();
        let mode = std::fs::metadata(&path).expect("there").permissions().mode();
        assert_eq!(mode & 0o777, 0o700);

        std::fs::write(path.join("theirs"), b"left by someone").expect("written");
        let again = Scratch::under(folder.path(), "test").map(|_| ());
        let said = again.expect_err("a folder that is already there is nobody's to build on");
        assert!(said.contains("exists"), "{said}");
        assert!(path.join("theirs").exists(), "and it is left as it was");

        drop(scratch);
        assert!(!path.exists(), "gone with the work it was for");
    }
}
