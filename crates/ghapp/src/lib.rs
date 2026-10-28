//! The GitHub App client: the repositories it may serve, their workflow files and variables, their queues, and
//! Actions runners for one job.
//!
//! Its tokens come from a [`TokenSource`]: the CodeFusion Console, which alone holds the Runners App's key
//! (`cf_console_client::ConsoleTokens`). No machine holds the key, a registration token or the Actions service's
//! admin token. Nothing here panics on what GitHub or the network does: every call returns a [`Result`].

mod client;
mod clock;
mod error;
mod http;
mod rest;
mod source;

pub use client::{
    API, DEFAULT_RUNNER_GROUP, GitHubApp, Installation, JOBS_PAGE, MAX_RUN_SETS, PlainJit, Repo,
    WAITING_RUNS, Waiting, WaitingJob,
};
pub use clock::{Clock, SystemClock};
pub use error::{Error, StatusError};
pub use http::{
    Http, MAX_BODY_BYTES, Response, RetryPolicy, is_safe_address, is_safe_address_text, same_origin,
};
pub use source::{Monotonic, Pending, SystemMonotonic, TokenSource};
