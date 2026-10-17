//! The client of the CodeFusion Console's runner protocol (protocol 1): a Mac enrols with a one-time code, reports what
//! it does and which jobs started and ended, hears of commands and pauses in the answer, and leaves when uninstalled.
//!
//! [`enroll`] trades the code for a token; [`Client`] makes the other calls with it. Job events wait in an [`Outbox`]
//! until a report that carries them is answered, so a console that is away for a while, a lost answer and a restart of
//! the Runner lose none of them and send none twice that the console took.
//!
//! Nothing here panics on what the network can do: every call is a result ([`Error`]). A token is a
//! [`cf_secret::Secret`] and appears in no error or `Debug` output.

mod client;
mod error;
mod outbox;
mod wire;

pub use client::{
    Client, MACHINE_NAME_MAX, MAX_BODY_BYTES, MAX_JOB_EVENTS, MAX_RESULTS, MAX_RUNNERS, MAX_SCALE_SETS,
    RESULT_ID_MAX, RUNNER_NAME_MAX, enroll, is_machine_name, units,
};
pub use error::Error;
pub use outbox::{EVENT_ID_MAX, OUTBOX_CAPACITY, Outbox, REPORT_BATCH, SENT_MEMORY};
pub use wire::*;
