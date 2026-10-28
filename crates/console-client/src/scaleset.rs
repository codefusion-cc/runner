//! Scale sets through the console (protocol 1, `/agent/runners/github/scaleset/*`): the console alone holds the
//! Runners App's key, the registration tokens and the Actions service's admin token, and asks the service for the
//! machine. The machine gets only what is its own: its scale sets, a session's message queue token and address, the
//! service's address (for `acquirejobs`, sent with the session token), and one Actions runner's configuration at a
//! time.
//!
//! What the console would refuse is refused here unsent ([`Error::Protocol`]): a repository it does not take, a scale
//! set not named `<machine>--…`, labels, an Actions runner not named `<machine>-j` and seven hex digits, a session ID or
//! a scale set ID of the wrong shape. Its own refusals keep their status and code ([`Error::Refused`]); a 429 or 503
//! keeps its `Retry-After` ([`Error::Busy`]). Every answer is checked against the contract's shape: a token is visible
//! ASCII of at most [`TOKEN_MAX`], an ID is positive, an address is there.
//!
//! `session/open` and `jit` are sent once: GitHub may have made a session or an Actions runner although the answer
//! was lost. The rest are safe to send again and are tried again as the client's [`cf_ghapp::Http`] says.

use cf_secret::Secret;
use serde::Deserialize;

use crate::client::{Client, body_of, is_slug, read};
use crate::error::Error;
use crate::github::{TOKEN_MAX, is_label, is_repo_name, is_runner_of, refuse, token_of};

/// The console's code for a scale set, session or Actions runner that is not the calling machine's (403).
pub const NOT_YOURS: &str = "not_yours";
/// The console's code for a scale set that already has a session (409).
pub const SESSION_EXISTS: &str = "session_exists";
/// The console's code for a session GitHub no longer has (404).
pub const SESSION_LOST: &str = "session_lost";
/// The console's code for an Actions runner name GitHub already has (409).
pub const NAME_TAKEN: &str = "name_taken";
/// The console's code for an Actions runner GitHub does not remove under its job (422).
pub const RUNNER_BUSY: &str = "runner_busy";
/// The console's code for a machine over its rate in the window (429).
pub const RATE_LIMITED: &str = "rate_limited";
/// The longest scale set name the console takes.
pub const SET_NAME_MAX: usize = 100;
/// The most labels one scale set carries.
pub const SET_LABELS_MAX: usize = 20;
/// The longest session ID the console takes.
pub const SESSION_ID_MAX: usize = 100;
/// The longest address taken from the console.
const URL_MAX: usize = 2048;
/// The largest ID the console takes (JavaScript's largest safe integer).
const ID_MAX: i64 = 9_007_199_254_740_991;

/// A scale set of the machine, as the console sends it: labels are names only, lowercased.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaleSetEntry {
    /// The service's ID for it.
    pub id: i64,
    /// `<machine>--…`.
    pub name: String,
    /// The labels jobs ask for.
    pub labels: Vec<String>,
}

/// A scale set's job counts when its session opened.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCounts {
    pub total_available_jobs: i64,
    pub total_acquired_jobs: i64,
    pub total_assigned_jobs: i64,
    pub total_running_jobs: i64,
    pub total_registered_runners: i64,
    pub total_busy_runners: i64,
    pub total_idle_runners: i64,
}

/// A message session the console opened for the machine.
#[derive(Debug, Clone)]
pub struct OpenedSession {
    /// GitHub's ID for it.
    pub session_id: String,
    /// Where its messages are read.
    pub queue_url: String,
    /// What they are read with. A credential, for GitHub's queue only.
    pub queue_token: Secret<String>,
    /// The Actions service's address, where `acquirejobs` goes with [`OpenedSession::queue_token`].
    pub service_url: String,
    /// The scale set's counts when it opened.
    pub counts: SessionCounts,
}

/// A session's new token.
#[derive(Debug, Clone)]
pub struct RefreshedSession {
    /// Where its messages are read now; none when GitHub did not say, and the session keeps its own.
    pub queue_url: Option<String>,
    /// The new token. A credential, for GitHub's queue only.
    pub queue_token: Secret<String>,
    /// The Actions service's address.
    pub service_url: String,
}

/// Which Actions runner `scaleset/remove` removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal<'a> {
    /// By GitHub's ID: the console reads its name first, and removes only one of the machine's.
    Id(i64),
    /// By name: `<machine>-j` and seven hex digits.
    Name(&'a str),
}

/// Whether `name` is a scale set name of `machine` as the console takes one: `<machine>--` then one or more lowercase
/// letters, digits and dashes, at most [`SET_NAME_MAX`] characters.
pub fn is_set_name_of(machine: &str, name: &str) -> bool {
    let rest = name.strip_prefix(machine).and_then(|r| r.strip_prefix("--"));
    is_slug(machine)
        && !machine.contains("--")
        && name.len() <= SET_NAME_MAX
        && rest.is_some_and(|r| {
            !r.is_empty()
                && r.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// Whether `id` is a session ID the console takes: 1 to 100 letters, digits and dashes.
pub fn is_session_id(id: &str) -> bool {
    (1..=SESSION_ID_MAX).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn check_repo(repo: &str) -> Result<(), Error> {
    if is_repo_name(repo) {
        Ok(())
    } else {
        Err(refuse("not a repository the console takes"))
    }
}

fn check_set(set_id: i64) -> Result<(), Error> {
    if (1..=ID_MAX).contains(&set_id) {
        Ok(())
    } else {
        Err(refuse("not a scale set ID"))
    }
}

fn check_session(session_id: &str) -> Result<(), Error> {
    if is_session_id(session_id) {
        Ok(())
    } else {
        Err(refuse("not a session ID the console takes"))
    }
}

/// An address as the console sends it: there, visible ASCII, not too long. Where it may lead is the caller's check.
fn url_of(what: &str, url: String) -> Result<String, Error> {
    if url.is_empty() || url.len() > URL_MAX || !url.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(Error::Protocol(format!("{what}: not an address")));
    }
    Ok(url)
}

#[derive(Deserialize)]
struct WireSet {
    id: i64,
    name: String,
    labels: Vec<String>,
}

fn entry_of(what: &str, wire: WireSet) -> Result<ScaleSetEntry, Error> {
    if wire.id <= 0 {
        return Err(Error::Protocol(format!("{what}: a scale set ID that is not one")));
    }
    if wire.name.is_empty() || wire.name.len() > SET_NAME_MAX {
        return Err(Error::Protocol(format!(
            "{what}: a scale set name that is not one"
        )));
    }
    if wire.labels.len() > SET_LABELS_MAX * 5 || !wire.labels.iter().all(|l| l.len() <= 1000) {
        return Err(Error::Protocol(format!(
            "{what}: labels that are not a scale set's"
        )));
    }
    Ok(ScaleSetEntry {
        id: wire.id,
        name: wire.name,
        labels: wire.labels,
    })
}

impl Client {
    async fn scaleset(&self, route: &str, message: serde_json::Value) -> Result<cf_ghapp::Response, Error> {
        self.post(&format!("github/scaleset/{route}"), body_of(&message)?)
            .await
    }

    async fn scaleset_once(
        &self,
        route: &str,
        message: serde_json::Value,
    ) -> Result<cf_ghapp::Response, Error> {
        self.post_with(
            &self.http().without_retries(),
            &format!("github/scaleset/{route}"),
            body_of(&message)?,
        )
        .await
    }

    /// `scaleset/list`: the machine's scale sets of `repo`, those of the Default runner group named `<machine>--…`.
    pub async fn scaleset_list(&self, repo: &str) -> Result<Vec<ScaleSetEntry>, Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            scale_sets: Vec<WireSet>,
        }
        check_repo(repo)?;
        let response = self.scaleset("list", serde_json::json!({ "repo": repo })).await?;
        let wire: Wire = read("the scale sets' answer", &response)?;
        wire.scale_sets
            .into_iter()
            .map(|set| entry_of("the scale sets' answer", set))
            .collect()
    }

    /// `scaleset/create`: the scale set `name` of `repo` with exactly `labels`, made, or changed to them, or as it is.
    /// `machine` is this machine's name at the console: a name not `<machine>--…` is refused here unsent.
    pub async fn scaleset_create(
        &self,
        machine: &str,
        repo: &str,
        name: &str,
        labels: &[String],
    ) -> Result<ScaleSetEntry, Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            scale_set: WireSet,
        }
        check_repo(repo)?;
        if !is_set_name_of(machine, name) {
            return Err(refuse("not a scale set name of this machine the console takes"));
        }
        if labels.is_empty() || labels.len() > SET_LABELS_MAX || !labels.iter().all(|l| is_label(l)) {
            return Err(refuse(
                "1 to 20 labels of letters, digits, dots, dashes and underscores",
            ));
        }
        let message = serde_json::json!({ "repo": repo, "name": name, "labels": labels });
        let response = self.scaleset("create", message).await?;
        let wire: Wire = read("the scale set's answer", &response)?;
        let entry = entry_of("the scale set's answer", wire.scale_set)?;
        if entry.name != name {
            return Err(Error::Protocol(
                "the scale set's answer: another scale set".into(),
            ));
        }
        Ok(entry)
    }

    /// `scaleset/delete`: whether the scale set was there to delete.
    pub async fn scaleset_delete(&self, repo: &str, set_id: i64) -> Result<bool, Error> {
        #[derive(Deserialize)]
        struct Wire {
            deleted: bool,
        }
        check_repo(repo)?;
        check_set(set_id)?;
        let response = self
            .scaleset("delete", serde_json::json!({ "repo": repo, "setId": set_id }))
            .await?;
        Ok(read::<Wire>("the deletion's answer", &response)?.deleted)
    }

    /// `scaleset/session/open`, sent once: a session of the scale set, owned by this machine. A scale set that has one
    /// is a 409 with the code [`SESSION_EXISTS`].
    pub async fn scaleset_session_open(&self, repo: &str, set_id: i64) -> Result<OpenedSession, Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Wire {
            session_id: String,
            message_queue_url: String,
            message_queue_access_token: String,
            service_url: String,
            statistics: SessionCounts,
        }
        check_repo(repo)?;
        check_set(set_id)?;
        let response = self
            .scaleset_once(
                "session/open",
                serde_json::json!({ "repo": repo, "setId": set_id }),
            )
            .await?;
        let what = "the session's answer";
        let wire: Wire = read(what, &response)?;
        if !is_session_id(&wire.session_id) {
            return Err(Error::Protocol(format!("{what}: a session ID that is not one")));
        }
        Ok(OpenedSession {
            session_id: wire.session_id,
            queue_url: url_of(what, wire.message_queue_url)?,
            queue_token: token_of(what, wire.message_queue_access_token)?,
            service_url: url_of(what, wire.service_url)?,
            counts: wire.statistics,
        })
    }

    /// `scaleset/session/refresh`: a new token for the session. A session GitHub no longer has is a 404 with the code
    /// [`SESSION_LOST`].
    pub async fn scaleset_session_refresh(
        &self,
        repo: &str,
        set_id: i64,
        session_id: &str,
    ) -> Result<RefreshedSession, Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            message_queue_url: String,
            message_queue_access_token: String,
            service_url: String,
        }
        check_repo(repo)?;
        check_set(set_id)?;
        check_session(session_id)?;
        let message = serde_json::json!({ "repo": repo, "setId": set_id, "sessionId": session_id });
        let response = self.scaleset("session/refresh", message).await?;
        let what = "the session's new token";
        let wire: Wire = read(what, &response)?;
        Ok(RefreshedSession {
            queue_url: match wire.message_queue_url.is_empty() {
                true => None,
                false => Some(url_of(what, wire.message_queue_url)?),
            },
            queue_token: token_of(what, wire.message_queue_access_token)?,
            service_url: url_of(what, wire.service_url)?,
        })
    }

    /// `scaleset/session/close`: whether the session was there to close.
    pub async fn scaleset_session_close(
        &self,
        repo: &str,
        set_id: i64,
        session_id: &str,
    ) -> Result<bool, Error> {
        #[derive(Deserialize)]
        struct Wire {
            closed: bool,
        }
        check_repo(repo)?;
        check_set(set_id)?;
        check_session(session_id)?;
        let message = serde_json::json!({ "repo": repo, "setId": set_id, "sessionId": session_id });
        let response = self.scaleset("session/close", message).await?;
        Ok(read::<Wire>("the session's closing", &response)?.closed)
    }

    /// `scaleset/jit`, sent once: the Actions runner `name` of the scale set, for one job. `machine` is this machine's
    /// name at the console: a name not `<machine>-j` and seven hex digits is refused here unsent. A name GitHub has is
    /// a 409 with the code [`NAME_TAKEN`].
    pub async fn scaleset_jit(
        &self,
        machine: &str,
        repo: &str,
        set_id: i64,
        name: &str,
    ) -> Result<crate::JitRunner, Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            runner_id: i64,
            encoded_jit_config: String,
        }
        check_repo(repo)?;
        check_set(set_id)?;
        if !is_runner_of(machine, name) {
            return Err(refuse(
                "not an Actions runner name of this machine the console makes",
            ));
        }
        let message = serde_json::json!({ "repo": repo, "setId": set_id, "name": name });
        let response = self.scaleset_once("jit", message).await?;
        let what = "the runner configuration's answer";
        let wire: Wire = read(what, &response)?;
        if wire.runner_id <= 0 {
            return Err(Error::Protocol(format!("{what}: no runner ID")));
        }
        if wire.encoded_jit_config.is_empty() || wire.encoded_jit_config.len() > 64 * 1024 {
            return Err(Error::Protocol(format!("{what}: no configuration")));
        }
        Ok(crate::JitRunner {
            runner_id: wire.runner_id,
            encoded: Secret::new(wire.encoded_jit_config),
        })
    }

    /// `scaleset/remove`: whether the Actions runner was there to remove. One with a job is a 422 with the code
    /// [`RUNNER_BUSY`]; one not of this machine a 403 [`NOT_YOURS`]. `machine` is this machine's name at the console.
    pub async fn scaleset_remove(
        &self,
        machine: &str,
        repo: &str,
        runner: Removal<'_>,
    ) -> Result<bool, Error> {
        #[derive(Deserialize)]
        struct Wire {
            removed: bool,
        }
        check_repo(repo)?;
        let message = match runner {
            Removal::Id(id) if (1..=ID_MAX).contains(&id) => {
                serde_json::json!({ "repo": repo, "runnerId": id })
            }
            Removal::Id(_) => return Err(refuse("not an Actions runner ID")),
            Removal::Name(name) if is_runner_of(machine, name) => {
                serde_json::json!({ "repo": repo, "name": name })
            }
            Removal::Name(_) => {
                return Err(refuse(
                    "not an Actions runner name of this machine the console makes",
                ));
            }
        };
        let response = self.scaleset("remove", message).await?;
        Ok(read::<Wire>("the removal's answer", &response)?.removed)
    }
}

const _: () = assert!(TOKEN_MAX >= 1024);
