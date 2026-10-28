//! A fake GitHub at the HTTP edge, for tests.
//!
//! It answers what the Runner's GitHub half asks, over real sockets: the GitHub App's installation tokens (made
//! in-process for the fake console), repositories with their workflow files and variables, Actions runner registration, and the Actions service with its
//! scale sets, message sessions, long poll, job acquisition and one-job Actions runner configs. Tests drive the jobs
//! ([`FakeGithub::queue_job`], [`FakeGithub::complete_job`], [`FakeGithub::cancel_job`]) and make it misbehave
//! ([`FakeGithub::fail`], [`FakeGithub::expire_session_token`], [`FakeGithub::lose_session`],
//! [`FakeGithub::set_clock_offset`]). The code under test is never mocked: it talks to this server like it talks to
//! GitHub. `cf-harness` is that setup, for every crate's tests: this fake and the fake console with the real clients
//! in front of them.

mod github;
pub mod httpd;

pub use github::{CONSOLE_MARK, FakeGithub, JobState, Logged, Options, ScaleSetInfo, TAKE_BACK_AFTER};
