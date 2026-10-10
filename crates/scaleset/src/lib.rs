//! GitHub's scale set client: one repository's scale sets, their message sessions, the long poll, acquiring jobs
//! and the one-job configuration of an Actions runner.
//!
//! [`ScaleSetClient`] is the administrator's side (scale sets, sessions, runners); [`MessageSession`] is one
//! session's queue; [`listen`] polls it into a [`Scaler`] and [`supervise`] opens it again, with a backoff, when it
//! ends. The GitHub App behind it is `cf-ghapp`'s, and so are its HTTP retries and its [`cf_ghapp::Error`].
//!
//! Nothing here panics on what GitHub or the network does: every call returns a [`Result`]. Tokens and runner
//! configurations are [`cf_secret::Secret`]s and appear in no error and no log.

mod client;
mod error;
mod listener;
mod session;
mod types;

pub use client::{ACTIONS_HOST, ScaleSetClient};
pub use error::Error;
pub use listener::{
    Backoff, MAX_RETRY_AFTER, Scaler, Shutdown, ShutdownTrigger, listen, shutdown, supervise,
};
pub use session::MessageSession;
pub use types::{
    JitConfig, JobAvailable, JobBase, JobCompleted, JobStarted, Label, Message, RunnerRef, ScaleSet,
    Statistics,
};
