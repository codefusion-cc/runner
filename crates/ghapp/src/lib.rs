//! The GitHub App client: its signed token, installation tokens, the repositories it may serve, their workflow files
//! and variables, and runner registration tokens.
//!
//! Nothing here panics on what GitHub or the network does: every call returns a [`Result`]. The App's private key
//! is read by [`AppCredentials::load`], lives in an [`AppKey`] whose `Debug` shows nothing, and appears in no error.

mod client;
mod credentials;
mod error;
mod http;
mod jwt;

pub use client::{API, GitHubApp, Installation, MAX_WORKFLOW_FILE_BYTES, Repo};
pub use credentials::{AppCredentials, AppKey, KeyError};
pub use error::{Error, StatusError};
pub use http::{Http, MAX_BODY_BYTES, Response, RetryPolicy, backoff, is_safe_address, same_origin};
pub use jwt::{Clock, SystemClock};
