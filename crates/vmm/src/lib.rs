//! The trait the Runner talks to a virtual machine monitor through.
//!
//! A [`Vmm`] starts machines; a [`Vm`] is one of them. Nothing here knows macOS: the implementation on
//! Virtualization.framework is `cf-vmm-vz`, and another operating system gets its own. [`testing`] has a machine
//! whose guest is a closure, so everything above this trait is tested without a hypervisor.
//!
//! Every method may be called from any thread. A failure to start or reach a machine is a [`VmmError`]; nothing here
//! panics for what a host or a guest does.

mod exit;
mod spec;
pub mod testing;

use std::io::{Read, Write};
use std::time::Duration;

/// A pressure file's share of the last 10 s ([`Vm::cpu_pressure`]).
pub use cf_guest_proto::Psi;
pub use exit::ExitCell;
pub use spec::{Disk, MIN_MEMORY, Network, SpecError, VmSpec};

/// The guest kernel's command line, the same under every monitor of an architecture. `console=hvc0` is the virtio
/// console the monitor writes to the console file. `quiet` keeps the kernel's own lines off the console; `panic=-1`
/// restarts a guest whose kernel panics, which closes the channel, and the host takes a closed channel for a lost
/// guest at once (a monitor that would boot the guest again ends the machine instead). `arm64.nosme` hides SME from
/// the guest's programs (the kernel clears the SME and SME2 feature registers, so `/proc/cpuinfo` and `getauxval`
/// stop listing them): on an M5 the guest's CPU reports it, but Chromium's code that uses it dies with SIGILL (a
/// camera in Playwright's Chromium crashed its video capture). The guest reports no SVE, so `arm64.nosve` is not
/// needed. The option means nothing to an x86_64 kernel, so its line leaves it out. `psi=1` turns on the pressure
/// stall accounts the supervisor reports a job's stalls from: Alpine's `linux-virt` builds them off by default.
#[cfg(target_arch = "aarch64")]
pub const COMMAND_LINE: &str = "console=hvc0 quiet panic=-1 psi=1 arm64.nosme";
#[cfg(not(target_arch = "aarch64"))]
pub const COMMAND_LINE: &str = "console=hvc0 quiet panic=-1 psi=1";

/// The longest a write to the guest's connection waits for the guest to read, under either monitor.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Why the monitor could not do what was asked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VmmError {
    /// This machine cannot run virtual machines (no hardware support, or the program lacks the right to).
    #[error("virtual machines are not supported here: {0}")]
    Unsupported(String),
    /// The specification cannot be a machine.
    #[error("the machine's specification is refused: {0}")]
    Spec(String),
    /// The machine did not start.
    #[error("the machine did not start: {0}")]
    Start(String),
    /// The guest's port could not be reached (nothing listens yet, or the machine is gone).
    #[error("the guest's port {port} is not reachable: {reason}")]
    Connect {
        /// The port asked for.
        port: u32,
        /// What the monitor said.
        reason: String,
    },
    /// The machine did not obey a stop.
    #[error("the machine could not be stopped: {0}")]
    Stop(String),
}

impl From<SpecError> for VmmError {
    fn from(error: SpecError) -> Self {
        VmmError::Spec(error.to_string())
    }
}

/// How a machine came to its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmExit {
    /// The guest powered itself off.
    Guest,
    /// The host stopped it ([`Vm::stop`]).
    Stopped,
    /// The monitor lost it: the guest crashed the machine, or the monitor's own process died.
    Error(String),
}

/// A virtual machine monitor.
pub trait Vmm: Send + Sync {
    /// Starts a machine and returns once it runs. The machine holds its full memory size from here on.
    fn start(&self, spec: &VmSpec) -> Result<Box<dyn Vm>, VmmError>;
}

/// One running (or ended) virtual machine. Dropping it ends the machine.
pub trait Vm: Send + Sync {
    /// Opens a connection to a port the guest listens on. One attempt: while the guest is not listening yet this
    /// is an error, and the caller tries again.
    fn channel(&self, port: u32) -> Result<Box<dyn Channel>, VmmError>;

    /// Waits for the machine's end for at most `timeout`. `None` when it still runs. After the end every call
    /// answers at once with the same exit.
    fn wait(&self, timeout: Duration) -> Option<VmExit>;

    /// Stops the machine as pulling its plug would: the guest gets no say. Returns once the stop is asked for;
    /// [`Vm::wait`] tells when it happened. An error means the machine takes no stop: it has ended, which
    /// [`Vm::wait`] tells, or it never will, and [`Vm::kill`] is what is left.
    fn stop(&self) -> Result<(), VmmError>;

    /// Lets go of the machine without waiting for anything: the last resort when [`Vm::stop`] does not end it.
    fn kill(&self);

    /// What of the host's memory the machine holds now, in bytes: the guest's memory as far as the guest has touched
    /// it, and the monitor's own. On a Mac the physical footprint of the monitor's process less what macOS compressed
    /// or swapped out of it; on Linux what the machine's cgroup holds less the page cache the host takes back at will.
    /// `None` when that cannot be told apart or read.
    fn resident(&self) -> Option<u64>;

    /// How long the machine's vCPUs waited for one of the host's CPUs over the last 10 s: the machine's cgroup's
    /// `cpu.pressure` on Linux. `None` where the host keeps no such account, as on a Mac.
    fn cpu_pressure(&self) -> Option<Psi> {
        None
    }
}

/// A byte stream between the host and one port of the guest.
pub trait Channel: Read + Write + Send {
    /// A second handle to the same stream, so one thread reads while another writes.
    fn try_clone(&self) -> std::io::Result<Box<dyn Channel>>;

    /// Closes the stream for both handles and wakes a blocked reader.
    fn shutdown(&self);
}
