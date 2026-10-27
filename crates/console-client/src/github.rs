//! GitHub through the console (protocol 1, `/agent/runners/github/{read,registration,jit}`): the console holds the
//! Runners App's key and hands a machine the tokens it needs, each with how long it lasts. The console answers 403
//! with the code `broker_off` ([`Error::GithubOff`]) while the owner has not turned GitHub tokens on for the machine;
//! any other 403 is a refusal of the call itself ([`Error::Refused`]).

use std::collections::HashSet;
use std::time::Duration;

use cf_secret::Secret;
use serde::Deserialize;

use crate::client::{Client, body_of, is_slug, read};
use crate::error::Error;

/// The longest repository name the console takes.
pub const REPO_MAX: usize = 140;
/// The most labels one JIT runner carries.
pub const JIT_LABELS_MAX: usize = 20;
/// The longest label.
pub const LABEL_MAX: usize = 100;
/// The longest Actions runner name the console makes.
pub const JIT_NAME_MAX: usize = 64;
/// The most installations one read answer may list; more is not the console's.
pub const READ_INSTALLATIONS_MAX: usize = 1000;
/// The longest token taken from the console, and the longest refused one a read names (`githubReadSchema`). GitHub
/// documents no fixed length, and its installation tokens grew past 255 characters.
pub const TOKEN_MAX: usize = 4096;
/// The most refused tokens one read names (`githubReadSchema`).
pub const REFUSED_MAX: usize = 20;
/// A token given with less than this left is not taken: it would end before a call made with it does.
pub const MIN_TOKEN_LIFE: Duration = Duration::from_secs(120);
/// The longest a token is held for, whatever the console says: GitHub's tokens last an hour.
pub const MAX_TOKEN_LIFE: Duration = Duration::from_secs(3600);

/// A token that reads the repositories of one installation of the console's Runners App.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallationToken {
    /// GitHub's installation ID.
    pub id: i64,
    /// The account it is installed on (`owner` of `owner/name`); none when GitHub named none.
    pub account: Option<String>,
    /// The token.
    pub token: Secret<String>,
    /// How long it lasts from when the answer was asked for, at most [`MAX_TOKEN_LIFE`].
    pub expires_in: Duration,
}

/// A token that registers Actions runners with one repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationToken {
    /// The token.
    pub token: Secret<String>,
    /// How long it lasts, at most [`MAX_TOKEN_LIFE`].
    pub expires_in: Duration,
}

/// An Actions runner registered for one job.
#[derive(Debug)]
pub struct JitRunner {
    /// GitHub's ID for it.
    pub runner_id: i64,
    /// What the Actions runner starts with. A credential.
    pub encoded: Secret<String>,
}

/// Whether `repo` is a repository name as the console takes one (`REPO_NAME` in its `protocol.ts`): an owner of a
/// letter or digit and up to 38 more letters, digits or dashes, a slash, and a name of 1 to 100 letters, digits, dots,
/// dashes and underscores that is not `.` or `..`.
pub fn is_repo_name(repo: &str) -> bool {
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    let owner_ok = owner.len() <= 39
        && owner.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && owner.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let name_ok = (1..=100).contains(&name.len())
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    repo.len() <= REPO_MAX && owner_ok && name_ok
}

/// Whether `label` is one the console takes: 1 to 100 letters, digits, dots, dashes and underscores.
pub fn is_label(label: &str) -> bool {
    (1..=LABEL_MAX).contains(&label.len())
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// Whether `name` is an Actions runner of `machine` as the console makes them (`isRunnerOf`): exactly `<machine>-j`
/// and seven lowercase hex digits, so `mac` never names a runner of `mac-mini`.
pub fn is_runner_of(machine: &str, name: &str) -> bool {
    let slug = is_slug(machine);
    let suffix = name
        .strip_prefix(machine)
        .and_then(|rest| rest.strip_prefix("-j"))
        .is_some_and(|hex| {
            hex.len() == 7
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
    slug && suffix && name.len() <= JIT_NAME_MAX
}

/// A token as the console sends it: non-empty, at most [`TOKEN_MAX`], visible ASCII only (it becomes a header).
fn token_of(what: &str, token: String) -> Result<Secret<String>, Error> {
    if token.is_empty() || token.len() > TOKEN_MAX || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(Error::Protocol(format!("{what}: not a token")));
    }
    Ok(Secret::new(token))
}

/// A lifetime as the console sends it, in whole seconds: one under [`MIN_TOKEN_LIFE`] is refused, one over
/// [`MAX_TOKEN_LIFE`] held for that long only.
fn life_of(what: &str, seconds: u64) -> Result<Duration, Error> {
    let life = Duration::from_secs(seconds);
    if life < MIN_TOKEN_LIFE {
        return Err(Error::Protocol(format!(
            "{what}: a token that ends in under {} s",
            MIN_TOKEN_LIFE.as_secs()
        )));
    }
    Ok(life.min(MAX_TOKEN_LIFE))
}

fn refuse(why: &str) -> Error {
    Error::Protocol(why.to_string())
}

impl Client {
    /// `github/read`: per installation of the console's Runners App, a token that only reads (runners, Actions runs,
    /// variables, contents). `refused` names tokens of the console's that GitHub refused (401): the console hands out
    /// none of them again, makes a new one in its place and revokes it. At most [`REFUSED_MAX`] are named, the last
    /// ones of `refused`, and none over [`TOKEN_MAX`], which the console would refuse the whole call for.
    /// Tried again on a dropped connection, a 429 or a 5xx, as the client's [`cf_ghapp::Http`] says.
    /// [`Error::GithubOff`] is the console's switch for this machine being off.
    pub async fn github_read(&self, refused: &[Secret<String>]) -> Result<Vec<InstallationToken>, Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            id: i64,
            #[serde(default)]
            account: Option<String>,
            token: String,
            expires_in_s: u64,
        }
        #[derive(Deserialize)]
        struct Answer {
            installations: Vec<Wire>,
        }
        let named: Vec<&str> = refused
            .iter()
            .map(|t| t.expose().as_str())
            .filter(|t| (1..=TOKEN_MAX).contains(&t.len()))
            .collect();
        let named = &named[named.len().saturating_sub(REFUSED_MAX)..];
        let message = if named.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::json!({ "refused": named })
        };
        let body = body_of(&message)?;
        let response = self.post("github/read", body).await?;
        let answer: Answer = read("the read tokens' answer", &response)?;
        if answer.installations.len() > READ_INSTALLATIONS_MAX {
            return Err(refuse("the read tokens' answer: more than 1000 installations"));
        }
        let mut seen = HashSet::new();
        answer
            .installations
            .into_iter()
            .map(|wire| {
                if wire.id <= 0 || !seen.insert(wire.id) {
                    return Err(refuse(
                        "the read tokens' answer: an installation ID that is not one, or twice",
                    ));
                }
                Ok(InstallationToken {
                    id: wire.id,
                    account: wire.account,
                    token: token_of("the read tokens' answer", wire.token)?,
                    expires_in: life_of("the read tokens' answer", wire.expires_in_s)?,
                })
            })
            .collect()
    }

    /// `github/registration`: a token that registers Actions runners with `repo` (`owner/name`). A name the console
    /// would refuse is refused here as [`Error::Protocol`], unsent. Tried again as [`Client::github_read`] is.
    pub async fn github_registration(&self, repo: &str) -> Result<RegistrationToken, Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            token: String,
            expires_in_s: u64,
        }
        if !is_repo_name(repo) {
            return Err(refuse("not a repository the console takes"));
        }
        let body = body_of(&serde_json::json!({ "repo": repo }))?;
        let response = self.post("github/registration", body).await?;
        let wire: Wire = read("the registration token's answer", &response)?;
        Ok(RegistrationToken {
            token: token_of("the registration token's answer", wire.token)?,
            expires_in: life_of("the registration token's answer", wire.expires_in_s)?,
        })
    }

    /// `github/jit`: one Actions runner `name` for one job of `repo`, with exactly `labels`. Sent once and never again
    /// by itself: GitHub may have registered it although the answer failed. What the console would refuse (a name
    /// that is not `<machine>-j` and seven hex digits, no labels or more than 20, a label it does not take) is refused
    /// here as [`Error::Protocol`], unsent. Over the console's limits it answers 429 with the wait ([`Error::Busy`]),
    /// having asked GitHub nothing; a name the console does not make for this machine is a 403 ([`Error::Refused`]).
    pub async fn github_jit(&self, repo: &str, name: &str, labels: &[String]) -> Result<JitRunner, Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            runner_id: i64,
            encoded_jit_config: String,
        }
        if !is_repo_name(repo) {
            return Err(refuse("not a repository the console takes"));
        }
        let machine_shaped = name.is_ascii() && name.len() > 9 && is_runner_of(&name[..name.len() - 9], name);
        if !machine_shaped {
            return Err(refuse("not an Actions runner name the console makes"));
        }
        if labels.is_empty() || labels.len() > JIT_LABELS_MAX || !labels.iter().all(|l| is_label(l)) {
            return Err(refuse(
                "1 to 20 labels of letters, digits, dots, dashes and underscores",
            ));
        }
        let body = body_of(&serde_json::json!({ "repo": repo, "name": name, "labels": labels }))?;
        let response = self
            .post_with(&self.http().without_retries(), "github/jit", body)
            .await?;
        let wire: Wire = read("the runner configuration's answer", &response)?;
        if wire.runner_id <= 0 {
            return Err(refuse("the runner configuration's answer: no runner ID"));
        }
        if wire.encoded_jit_config.is_empty() {
            return Err(refuse("the runner configuration's answer: no configuration"));
        }
        Ok(JitRunner {
            runner_id: wire.runner_id,
            encoded: Secret::new(wire.encoded_jit_config),
        })
    }
}
