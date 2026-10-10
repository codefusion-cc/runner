//! The vm-host: the process that owns one job's virtual machine, from the request to the end.
//!
//! The daemon starts one vm-host per job (`codefusion-runner vm --state-dir <folder>`) and writes the job's
//! [`JobRequest`] to its input. The request holds the job's registration with GitHub, so it travels on that pipe
//! only: never in an argument, the environment or a file. From then on the two share a folder, laid out by
//! [`Layout`]:
//!
//! - `run/<job>.json`: the job's [`JobState`], replaced whole on every change. This is the truth a daemon that
//!   starts again reads: a state without an `end` whose `pid` is alive is a running job to adopt, one whose `pid`
//!   is gone is a job lost with its vm-host.
//! - `run/<job>.sock`: a socket the vm-host answers on ([`control`]): one request a connection, the state back.
//! - `run/<job>/`: the machine's own files, gone with the job. `logs/<job>.console.log` stays.
//!
//! A vm-host is its own session: it does not end with the daemon. It enforces the job's deadline and every wait
//! on the guest by itself ([`Machine`]), so a job never depends on a daemon being there.
//!
//! [`run`] is the whole of it against any [`cf_vmm::Vmm`]; [`task::run_task`] runs one of the guest's small
//! tasks (a probe, formatting a disk) the same way.

pub mod control;
mod driver;
mod layout;
mod machine;
mod request;
pub mod scratch;
mod state;
pub mod task;

pub use driver::{Guest, RunError, run};
pub use layout::Layout;
pub use machine::{Action, Event, Limits, Machine};
pub use request::{JobRequest, RequestError, VM_OVERHEAD};
pub use state::{End, Ended, JobState, OutOfMemory, Stage, out_of_memory};
