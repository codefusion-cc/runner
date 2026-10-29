//! The scale set administrator's calls of a host that still holds the Runners App's key: the key's installation token
//! buys a registration token, which buys the Actions service's own admin token, and the host makes the calls itself.
//!
//! Only until the console's `github/scaleset/*` answered the host (rollout step 3 of the console's `docs/RUNNERS.md`):
//! the Runner then deletes the key and makes every such call through the console. This goes with the last host that
//! held a key.

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cf_console_client::{OpenedSession, RefreshedSession, SessionCounts};
use cf_ghapp::{DEFAULT_RUNNER_GROUP as RUNNER_GROUP, Error as ApiError, KeyTokens, Response};
use cf_secret::Secret;
use reqwest::Method;
use reqwest::header::AUTHORIZATION;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::{API_VERSION, ScaleSetClient, decode, encode};
use crate::error::Error;
use crate::types::{Label, RunnerRef, ScaleSet};

/// The Actions service's token is replaced this long before it expires.
const ADMIN_MARGIN_SECS: i64 = 120;
/// How long a token is trusted when its expiry cannot be read.
const UNKNOWN_EXPIRY_SECS: i64 = 600;

/// The Actions service's address and the token that administers scale sets on it.
struct Admin {
    url: String,
    token: Secret<String>,
    expires: i64,
}

/// The key a host holds and the admin token it bought, for one repository (`config_url` is its page).
pub(crate) struct KeyAdmin {
    tokens: Arc<KeyTokens>,
    config_url: String,
    admin: tokio::sync::Mutex<Option<Admin>>,
}

#[derive(Deserialize)]
struct Registration {
    url: String,
    token: String,
}

#[derive(Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
struct Page<T> {
    #[serde(default = "Vec::new", deserialize_with = "crate::types::null_default")]
    value: Vec<T>,
}

/// What the service sends when a message session opens, and what a refresh returns.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionInfo {
    session_id: String,
    message_queue_url: String,
    message_queue_access_token: String,
    #[serde(default)]
    statistics: Option<SessionCounts>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JitResponse {
    runner: Option<RunnerRef>,
    #[serde(rename = "encodedJITConfig")]
    encoded_jit_config: String,
}

/// The `exp` claim of a token, without checking anything: the service decides whether the token is good.
fn expiry_of(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()?.get("exp")?.as_i64()
}

/// Whether a token that expires at `expires` is still good to use at `now`, whatever number the token named.
fn is_fresh(expires: i64, now: i64) -> bool {
    now < expires.saturating_sub(ADMIN_MARGIN_SECS)
}

fn scale_set_body(name: &str, labels: &[String]) -> Value {
    let labels: Vec<Label> = labels
        .iter()
        .map(|l| Label {
            kind: "System".into(),
            name: l.clone(),
        })
        .collect();
    // The Actions runner is rebuilt with each release; one updating itself in a one-job machine would only download
    // the update again for every job.
    json!({"name": name, "runnerGroupId": RUNNER_GROUP, "labels": labels, "RunnerSetting": {"disableUpdate": true}, "enabled": true})
}

/// Whether the service answered that what a call names is not there.
fn gone<T>(result: Result<T, Error>) -> Result<bool, Error> {
    match result {
        Ok(_) => Ok(true),
        Err(e) if e.is_not_found() => Ok(false),
        Err(e) => Err(e),
    }
}

impl KeyAdmin {
    pub(crate) fn new(tokens: Arc<KeyTokens>, config_url: &str) -> KeyAdmin {
        KeyAdmin {
            tokens,
            config_url: config_url.to_string(),
            admin: tokio::sync::Mutex::new(None),
        }
    }

    /// Trades the App's token for the service's.
    async fn register(&self, client: &ScaleSetClient) -> Result<Admin, Error> {
        let reg = self
            .tokens
            .registration_token(client.installation(), client.repo())
            .await?;
        let url = format!("{}/actions/runner-registration", client.app().api());
        let auth = format!("RemoteAuth {}", reg.expose());
        let body = json!({"url": self.config_url, "runner_event": "register"}).to_string();
        let response = client
            .app()
            .http()
            .send(|c| {
                c.post(&url)
                    .header(AUTHORIZATION, &auth)
                    .header("Content-Type", "application/json")
                    .body(body.clone())
            })
            .await?;
        let parsed: Registration = decode("Actions runner registration", &response)?;
        if parsed.token.is_empty() || parsed.url.is_empty() {
            return Err(Error::Api(ApiError::Decode {
                what: "Actions runner registration".into(),
                why: "the answer has no service address or no token".into(),
            }));
        }
        client.check_address("Actions runner registration", &parsed.url)?;
        let expires = expiry_of(&parsed.token).unwrap_or(client.app().now_epoch() + UNKNOWN_EXPIRY_SECS);
        Ok(Admin {
            url: parsed.url.trim_end_matches('/').to_string(),
            token: Secret::new(parsed.token),
            expires,
        })
    }

    /// The service's address and a token that is good for a while, asking for a new one when needed. `refused` is a
    /// token the service just turned down: it is replaced, unless another caller has replaced it already. One caller
    /// at a time asks; the others wait for its answer.
    async fn admin(
        &self,
        client: &ScaleSetClient,
        refused: Option<&Secret<String>>,
    ) -> Result<(String, Secret<String>), Error> {
        let mut slot = self.admin.lock().await;
        let fresh = slot
            .as_ref()
            .is_some_and(|a| refused != Some(&a.token) && is_fresh(a.expires, client.app().now_epoch()));
        if !fresh {
            *slot = Some(self.register(client).await?);
        }
        let a = slot.as_ref().expect("just set");
        Ok((a.url.clone(), a.token.clone()))
    }

    /// A call to the scale set API as the administrator; a 401 gets a new admin token once.
    async fn call(
        &self,
        client: &ScaleSetClient,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Response, Error> {
        let mut refused: Option<Secret<String>> = None;
        loop {
            let (base, token) = self.admin(client, refused.as_ref()).await?;
            let sep = if path.contains('?') { '&' } else { '?' };
            let url = format!("{base}/_apis/{path}{sep}api-version={API_VERSION}");
            match client
                .send(method.clone(), &url, &token, body.as_ref(), None, None)
                .await
            {
                Err(e) if e.status() == Some(401) && refused.is_none() => refused = Some(token),
                other => return other,
            }
        }
    }

    /// Every scale set of the repository's Actions runner group.
    pub(crate) async fn list(&self, client: &ScaleSetClient) -> Result<Vec<ScaleSet>, Error> {
        let path = format!("runtime/runnerscalesets?runnerGroupId={RUNNER_GROUP}");
        let response = self.call(client, Method::GET, &path, None).await?;
        Ok(decode::<Page<ScaleSet>>("scale sets", &response)?.value)
    }

    /// Finds the scale set of this name with exactly these labels, creating it or setting its labels when needed.
    pub(crate) async fn ensure(
        &self,
        client: &ScaleSetClient,
        name: &str,
        labels: &[String],
    ) -> Result<ScaleSet, Error> {
        let mut want: Vec<String> = labels.iter().map(|l| l.to_lowercase()).collect();
        want.sort();
        want.dedup();
        let body = scale_set_body(name, &want);
        let path = format!(
            "runtime/runnerscalesets?runnerGroupId={RUNNER_GROUP}&name={}",
            encode(name)
        );
        let response = self.call(client, Method::GET, &path, None).await?;
        let found = decode::<Page<ScaleSet>>("scale sets", &response)?
            .value
            .into_iter()
            .find(|s| s.name == name);
        match found {
            None => {
                let response = self
                    .call(client, Method::POST, "runtime/runnerscalesets", Some(body))
                    .await?;
                decode("scale set", &response)
            }
            Some(existing) if existing.label_names() != want => {
                let path = format!("runtime/runnerscalesets/{}", existing.id);
                let response = self.call(client, Method::PATCH, &path, Some(body)).await?;
                decode("scale set", &response)
            }
            Some(existing) => Ok(existing),
        }
    }

    /// Deletes a scale set: whether it was there to delete.
    pub(crate) async fn delete(&self, client: &ScaleSetClient, id: i64) -> Result<bool, Error> {
        let path = format!("runtime/runnerscalesets/{id}");
        gone(self.call(client, Method::DELETE, &path, None).await)
    }

    /// Opens the message session of a scale set for `owner` (the machine's name).
    pub(crate) async fn open_session(
        &self,
        client: &ScaleSetClient,
        scale_set_id: i64,
        owner: &str,
    ) -> Result<OpenedSession, Error> {
        let path = format!("runtime/runnerscalesets/{scale_set_id}/sessions");
        let response = self
            .call(client, Method::POST, &path, Some(json!({"ownerName": owner})))
            .await
            .map_err(|e| match e.status() {
                Some(409) => Error::SessionExists,
                _ => e,
            })?;
        let info: SessionInfo = decode("message session", &response)?;
        let (service_url, _) = self.admin(client, None).await?;
        Ok(OpenedSession {
            session_id: info.session_id,
            queue_url: info.message_queue_url,
            queue_token: Secret::new(info.message_queue_access_token),
            service_url,
            counts: info.statistics.unwrap_or_default(),
        })
    }

    /// Asks the service for a new token for a session.
    pub(crate) async fn refresh_session(
        &self,
        client: &ScaleSetClient,
        scale_set_id: i64,
        session_id: &str,
    ) -> Result<RefreshedSession, Error> {
        let path = format!(
            "runtime/runnerscalesets/{scale_set_id}/sessions/{}",
            encode(session_id)
        );
        let response = self
            .call(client, Method::PATCH, &path, None)
            .await
            .map_err(|e| match e.status() {
                Some(404) => Error::SessionLost("the service no longer has the session".into()),
                _ => e,
            })?;
        let info: SessionInfo = decode("message session", &response)?;
        let (service_url, _) = self.admin(client, None).await?;
        Ok(RefreshedSession {
            queue_url: (!info.message_queue_url.is_empty()).then_some(info.message_queue_url),
            queue_token: Secret::new(info.message_queue_access_token),
            service_url,
        })
    }

    /// Ends a session.
    pub(crate) async fn delete_session(
        &self,
        client: &ScaleSetClient,
        scale_set_id: i64,
        session_id: &str,
    ) -> Result<(), Error> {
        let path = format!(
            "runtime/runnerscalesets/{scale_set_id}/sessions/{}",
            encode(session_id)
        );
        self.call(client, Method::DELETE, &path, None).await.map(|_| ())
    }

    /// Registers an Actions runner of the scale set, sent as the service's own client sends it; the error says
    /// whether the call may have registered one anyway (a name taken, or an answer that never came).
    pub(crate) async fn generate_jit(
        &self,
        client: &ScaleSetClient,
        scale_set_id: i64,
        name: &str,
    ) -> Result<(Option<RunnerRef>, Secret<String>), (Error, bool)> {
        let path = format!("runtime/runnerscalesets/{scale_set_id}/generatejitconfig");
        let body = json!({"name": name, "workFolder": "_work"});
        match self.call(client, Method::POST, &path, Some(body)).await {
            Ok(response) => {
                let parsed: JitResponse =
                    decode("Actions runner configuration", &response).map_err(|e| (e, true))?;
                Ok((parsed.runner, Secret::new(parsed.encoded_jit_config)))
            }
            Err(e) => {
                let leftover = e.status() == Some(409) || (e.is_transient() && e.status() != Some(429));
                Err((e, leftover))
            }
        }
    }

    /// The registered Actions runner of this name, if there is one.
    async fn runner_by_name(&self, client: &ScaleSetClient, name: &str) -> Result<Option<RunnerRef>, Error> {
        let path = format!("distributedtask/pools/0/agents?agentName={}", encode(name));
        let response = self.call(client, Method::GET, &path, None).await?;
        Ok(decode::<Page<RunnerRef>>("Actions runners", &response)?
            .value
            .into_iter()
            .find(|r| r.name == name))
    }

    /// Removes a registered Actions runner by GitHub's ID: whether it was there to remove. One with a job refuses
    /// (422).
    pub(crate) async fn remove_runner(&self, client: &ScaleSetClient, runner_id: i64) -> Result<bool, Error> {
        let path = format!("distributedtask/pools/0/agents/{runner_id}");
        gone(self.call(client, Method::DELETE, &path, None).await)
    }

    /// Removes a registered Actions runner by name, as [`KeyAdmin::remove_runner`] does.
    pub(crate) async fn remove_runner_named(
        &self,
        client: &ScaleSetClient,
        name: &str,
    ) -> Result<bool, Error> {
        match self.runner_by_name(client, name).await? {
            Some(runner) => self.remove_runner(client, runner.id).await,
            None => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_fresh_until_two_minutes_before_it_runs_out_whatever_it_says() {
        assert!(is_fresh(1000, 879));
        assert!(!is_fresh(1000, 880));
        assert!(!is_fresh(1000, 1000));
        assert!(!is_fresh(i64::MIN, 0));
        assert!(is_fresh(i64::MAX, 0));
    }
}
