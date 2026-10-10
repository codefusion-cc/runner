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
//! a panic.

mod frame;
mod message;
mod secret;

pub use frame::{Decoder, MAX_FRAME, ProtoError, encode, read_message, write_message};
pub use message::{
    GuestEnd, GuestMessage, HostMessage, Job, Limits, OomEvent, Process, Start, Stop, StopReason, Summary,
    Task, Usage, Workload,
};
pub use secret::Secret;

/// The protocol's version. A supervisor that hears another one refuses to start a task.
pub const PROTOCOL: u32 = 1;

/// The vsock port the supervisor listens on.
pub const PORT: u32 = 6000;
