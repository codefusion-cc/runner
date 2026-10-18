//! The wire protocol between a vm-host and the supervisor in the guest.
//!
//! One connection per virtual machine, opened by the host to the supervisor's vsock port [`PORT`]. Each side sends
//! frames: a 4 byte big-endian length, then that many bytes of JSON. The supervisor speaks first ([`GuestMessage::Hello`]),
//! the host answers with [`HostMessage::Start`], and the supervisor reports until [`GuestMessage::Exited`], after
//! which it powers the machine off.
//!
//! Both ends come out of one build (the supervisor is embedded in the Runner), so the two never differ in version;
//! [`PROTOCOL`] is checked all the same and a mismatch ends the boot.
//!
//! Everything a peer sends is input: a frame that is too large, cut short or not a message is a [`ProtoError`], never
//! a panic. The host reads a guest with [`read_guest_message`], which also bounds what a frame holds.

mod frame;
mod message;

/// A value that must not reach a log: `Debug` prints no part of it. A one-job configuration of the Actions runner
/// travels in one, as a plain string on the wire, since the other end needs it.
pub use cf_secret::Secret;
pub use frame::{
    MAX_FRAME, MAX_GUEST_FRAME, ProtoError, encode, read_guest_message, read_message, read_message_within,
    write_message,
};
pub use message::{
    CacheEnd, CacheFound, GuestEnd, GuestMessage, HostMessage, Job, LARGEST_PROCESSES, Limits, MESSAGE_CHARS,
    NAME_CHARS, OomEvent, Process, STAGE_CHARS, Start, Stop, StopReason, Summary, Task, Usage, Workload, clip,
    secret_env,
};

/// The protocol's version. A supervisor that hears another one refuses to start a task.
pub const PROTOCOL: u32 = 1;

/// The vsock port the supervisor listens on.
pub const PORT: u32 = 6000;

/// The serial of the disk with the image's root file system, which the guest finds it by.
pub const IMAGE_DISK: &str = "cf-image";

/// The serial of the job's own disk.
pub const SCRATCH_DISK: &str = "cf-scratch";

/// The serial of a Docker job's cache disk: its repository's Docker data, a copy the job may write.
pub const CACHE_DISK: &str = "cf-docker-cache";
