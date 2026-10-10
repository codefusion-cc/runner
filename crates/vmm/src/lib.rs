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

pub use exit::ExitCell;
pub use spec::{Disk, MIN_MEMORY, Network, SpecError, VmSpec};

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
}

/// A byte stream between the host and one port of the guest.
pub trait Channel: Read + Write + Send {
    /// A second handle to the same stream, so one thread reads while another writes.
    fn try_clone(&self) -> std::io::Result<Box<dyn Channel>>;

    /// Closes the stream for both handles and wakes a blocked reader.
    fn shutdown(&self);
}
