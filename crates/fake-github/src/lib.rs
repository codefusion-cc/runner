//! A fake GitHub at the HTTP edge, for tests.
//!
//! It answers what the Runner's GitHub half asks, over real sockets: the GitHub App's installations and tokens,
//! repositories with their workflow files and variables, runner registration, and the Actions service with its scale
//! sets, message sessions, long poll, job acquisition and one-job runner configs. Tests drive the jobs
//! ([`FakeGithub::queue_job`], [`FakeGithub::complete_job`], [`FakeGithub::cancel_job`]) and make it misbehave
//! ([`FakeGithub::fail`], [`FakeGithub::expire_session_token`], [`FakeGithub::lose_session`],
//! [`FakeGithub::set_clock_offset`]). The code under test is never mocked: it talks to this server like it talks to
//! GitHub. [`harness`] is that setup, for every crate's tests: the fake, the real clients and a clock to move.

mod github;
pub mod harness;
pub mod httpd;
mod keys;

pub use github::{FakeGithub, JobState, Logged, Options, ScaleSetInfo, Stats};
pub use keys::{TestKey, test_key};
