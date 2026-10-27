//! The client of the CodeFusion Console's runner protocol (protocol 1): a Mac enrols with a one-time code, reports what
//! it does and which jobs started and ended, hears of commands and pauses in the answer, and leaves when uninstalled.
//!
//! [`enroll`] trades the code for a token; [`Client`] makes the other calls with it, GitHub's tokens through the
//! console among them, which [`ConsoleTokens`] keeps and renews for the App client. Job events wait in an [`Outbox`]
//! until a report that carries them is answered, so a console that is away for a while, a lost answer and a restart of
//! the Runner lose none of them and send none twice that the console took.
//!
//! Nothing here panics on what the network can do: every call is a result ([`Error`]). A token is a
//! [`cf_secret::Secret`] and appears in no error or `Debug` output.

mod client;
mod error;
mod github;
mod outbox;
mod tokens;
mod wire;

pub use client::{
    Client, MACHINE_NAME_MAX, MAX_BODY_BYTES, MAX_JOB_CACHE_EVENTS, MAX_JOB_CACHES, MAX_JOB_EVENTS,
    MAX_RESULTS, MAX_RUNNERS, MAX_SCALE_SETS, RESULT_ID_MAX, RUNNER_NAME_MAX, enroll, is_machine_name, units,
};
pub use error::{BROKER_OFF, Error, RETRY_AFTER_MAX};
pub use github::{
    InstallationToken, JIT_LABELS_MAX, JIT_NAME_MAX, JitRunner, LABEL_MAX, MAX_TOKEN_LIFE, MIN_TOKEN_LIFE,
    READ_INSTALLATIONS_MAX, REFUSED_MAX, REFUSED_TOKEN_MAX, REPO_MAX, RegistrationToken, TOKEN_MAX, is_label,
    is_repo_name, is_runner_of,
};
pub use outbox::{EVENT_ID_MAX, OUTBOX_CAPACITY, Outbox, REPORT_BATCH, SENT_MEMORY};
pub use tokens::{
    ConsoleTokens, EMPTY_LIFE, END_MARGIN, FAILED_WAIT, FAILED_WAIT_MAX, GATE_RECHECK, GATE_RECHECK_ALONE,
};
pub use wire::*;
