//! The GitHub App client: installation tokens, the repositories it may serve, their workflow files and variables,
//! and Actions runner registration tokens.
//!
//! Where the tokens come from is a [`TokenSource`]: the App's own key on this machine ([`KeyTokens`]), or the
//! CodeFusion Console, which holds the key (`cf_console_client::ConsoleTokens`). Nothing here panics on what GitHub or
//! the network does: every call returns a [`Result`]. The App's private key is read by [`AppCredentials::load`], lives
//! in an [`AppKey`] whose `Debug` shows nothing, and appears in no error.

mod client;
mod credentials;
mod error;
mod http;
mod jwt;
mod key;
mod rest;
mod source;

pub use client::{
    API, DEFAULT_RUNNER_GROUP, GitHubApp, Installation, JOBS_PAGE, MAX_RUN_SETS, PlainJit, RUNS_LISTED, Repo,
    WAITING_RUNS, Waiting, WaitingJob,
};
pub use credentials::{AppCredentials, AppKey, KeyError};
pub use error::{Error, StatusError};
pub use http::{
    Http, MAX_BODY_BYTES, Response, RetryPolicy, is_safe_address, is_safe_address_text, same_origin,
};
pub use jwt::{Clock, SystemClock};
pub use key::KeyTokens;
pub use source::{Monotonic, Pending, SystemMonotonic, TokenSource};
