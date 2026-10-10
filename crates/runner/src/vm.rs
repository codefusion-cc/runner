//! The commands that start virtual machines: `vm`, the vm-host of one job, and `boot-proof`.

use cf_guest_proto::Task;
use cf_vmhost::task::run_task;
use cf_vmhost::{Guest, JobRequest, Layout, Limits};
use cf_vmm::{Network, VmSpec};
use cf_vmm_vz::VzVmm;
use std::io::{Read, Write};
use std::os::unix::fs::DirBuilderExt as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Set by a signal that asks the vm-host to end its job.
static CANCEL: AtomicBool = AtomicBool::new(false);

extern "C" fn cancel(_signal: libc::c_int) {
    CANCEL.store(true, Ordering::Relaxed);
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
pub fn host(state_dir: &str) -> Result<(), String> {
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
    let vmm = VzVmm::new().map_err(|error| error.to_string())?;
    let limits = Limits::for_deadline(Duration::from_secs(request.deadline_s));
    let state = cf_vmhost::run(&vmm, &Layout::new(state_dir), guest, request, limits, &CANCEL)
        .map_err(|error| error.to_string())?;
    // The daemon may be gone by now; the state file has the same.
    let _ = writeln!(
        std::io::stdout(),
        "{}",
        serde_json::to_string(&state).unwrap_or_default()
    );
    Ok(())
}

/// Boots the embedded guest with no disk and no network, has it answer and power off, and prints what happened.
pub fn boot_proof(memory_bytes: u64, timeout: Duration) -> Result<(), String> {
    let guest = guest()?;
    let scratch = Scratch::new()?;
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
    let vmm = VzVmm::new().map_err(|error| error.to_string())?;
    let report = run_task(&vmm, &spec, Task::Probe, timeout, &|| false);
    print!("{}", std::fs::read_to_string(&console).unwrap_or_default());
    let report = report?;
    println!(
        "kernel {}: ready after {:.2?}, ended after {:.2?} ({})",
        report.kernel,
        report.ready_after,
        report.ended_after,
        if report.powered_off {
            "guest powered itself off"
        } else {
            "stopped by the host"
        }
    );
    if report.powered_off {
        Ok(())
    } else {
        Err("the guest did not power off by itself".into())
    }
}

/// A folder of this process alone for the proof's kernel and initramfs, removed when it goes out of scope, however
/// the proof ends. What the hypervisor loads from it is what was verified: nobody else can write there.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!("codefusion-runner-proof-{}", std::process::id()));
        // Made new and private in one step. A folder already there is not ours to trust, whoever left it.
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        Ok(Self(path))
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
