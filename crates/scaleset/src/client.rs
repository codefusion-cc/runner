use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cf_ghapp::{Clock, Error as ApiError, GitHubApp, Response};
use cf_secret::Secret;
use reqwest::Method;
use reqwest::header::AUTHORIZATION;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::error::Error;
use crate::session::MessageSession;
use crate::types::{JitConfig, JitResponse, Label, RunnerRef, ScaleSet, SessionInfo};

const API_VERSION: &str = "6.0-preview";
const RUNNER_GROUP: i64 = 1;
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

/// One repository's scale sets on the Actions service, reached as the GitHub App that serves it.
///
/// The App's installation token buys a registration token, which buys the service's own admin token. That token
/// is kept until two minutes before it expires, and replaced once when the service refuses it. Every call returns
/// a `Result`; nothing here panics for what the service or the network does.
#[derive(Clone)]
pub struct ScaleSetClient {
    inner: Arc<Inner>,
}

struct Inner {
    app: GitHubApp,
    installation: i64,
    repo: String,
    config_url: String,
    clock: Arc<dyn Clock>,
    admin: tokio::sync::Mutex<Option<Admin>>,
    poll_timeout: Duration,
}

impl std::fmt::Debug for ScaleSetClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScaleSetClient")
            .field("repo", &self.inner.repo)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct Registration {
    url: String,
    token: String,
}

#[derive(Deserialize)]
struct Page<T> {
    #[serde(default = "Vec::new")]
    value: Vec<T>,
}

fn decode<T: DeserializeOwned>(what: &str, response: &Response) -> Result<T, Error> {
    serde_json::from_slice(&response.body).map_err(|e| {
        Error::Api(ApiError::Decode {
            what: what.into(),
            why: e.to_string(),
        })
    })
}

/// The `exp` claim of a token, without checking anything: the service decides whether the token is good.
fn expiry_of(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()?.get("exp")?.as_i64()
}

impl ScaleSetClient {
    /// A client for the scale sets of `repo` (`owner/name`, whose page is `config_url`), which the App serves through
    /// `installation`. `poll_timeout` is how long a long poll may take in all; the service ends it after about a
    /// minute.
    pub fn new(
        app: GitHubApp,
        installation: i64,
        repo: &str,
        config_url: &str,
        clock: Arc<dyn Clock>,
        poll_timeout: Duration,
    ) -> ScaleSetClient {
        ScaleSetClient {
            inner: Arc::new(Inner {
                app,
                installation,
                repo: repo.to_string(),
                config_url: config_url.to_string(),
                clock,
                admin: tokio::sync::Mutex::new(None),
                poll_timeout,
            }),
        }
    }

    pub(crate) fn poll_timeout(&self) -> Duration {
        self.inner.poll_timeout
    }

    fn now(&self) -> i64 {
        self.inner
            .clock
            .now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs() as i64
    }

    /// Trades the App's token for the service's.
    async fn register(&self) -> Result<Admin, Error> {
        let reg = self
            .inner
            .app
            .registration_token(self.inner.installation, &self.inner.repo)
            .await?;
        let url = format!("{}/actions/runner-registration", self.inner.app.api());
        let auth = format!("RemoteAuth {}", reg.expose());
        let body = json!({"url": self.inner.config_url, "runner_event": "register"}).to_string();
        let response = self
            .inner
            .app
            .http()
            .send(
                |c| {
                    c.post(&url)
                        .header(AUTHORIZATION, &auth)
                        .header("Content-Type", "application/json")
                        .body(body.clone())
                },
                true,
            )
            .await?;
        let parsed: Registration = decode("runner registration", &response)?;
        if parsed.token.is_empty() || parsed.url.is_empty() {
            return Err(Error::Api(ApiError::Decode {
                what: "runner registration".into(),
                why: "the answer has no service address or no token".into(),
            }));
        }
        let expires = expiry_of(&parsed.token).unwrap_or(self.now() + UNKNOWN_EXPIRY_SECS);
        Ok(Admin {
            url: parsed.url.trim_end_matches('/').to_string(),
            token: Secret::new(parsed.token),
            expires,
        })
    }

    /// The service's address and a token that is good for a while, asking for a new one when needed.
    async fn admin(&self, replace: bool) -> Result<(String, Secret<String>), Error> {
        let mut slot = self.inner.admin.lock().await;
        let fresh = slot
            .as_ref()
            .is_some_and(|a| !replace && self.now() < a.expires - ADMIN_MARGIN_SECS);
        if !fresh {
            *slot = Some(self.register().await?);
        }
        let a = slot.as_ref().expect("just set");
        Ok((a.url.clone(), Secret::new(a.token.expose().clone())))
    }

    async fn send(
        &self,
        method: Method,
        url: &str,
        token: &Secret<String>,
        body: Option<&Value>,
        timeout: Option<Duration>,
    ) -> Result<Response, Error> {
        let bearer = format!("Bearer {}", token.expose());
        let payload = body.map(Value::to_string);
        Ok(self
            .inner
            .app
            .http()
            .send(
                |c| {
                    let mut r = c
                        .request(method.clone(), url)
                        .header(AUTHORIZATION, &bearer)
                        .header("Accept", "application/json");
                    if let Some(p) = &payload {
                        r = r.header("Content-Type", "application/json").body(p.clone());
                    }
                    if let Some(t) = timeout {
                        r = r.timeout(t);
                    }
                    r
                },
                true,
            )
            .await?)
    }

    /// A call to the scale set API as the administrator; a 401 gets a new admin token once.
    async fn admin_call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Response, Error> {
        let mut replace = false;
        loop {
            let (base, token) = self.admin(replace).await?;
            let sep = if path.contains('?') { '&' } else { '?' };
            let url = format!("{base}/_apis/{path}{sep}api-version={API_VERSION}");
            match self.send(method.clone(), &url, &token, body.as_ref(), None).await {
                Err(e) if e.status() == Some(401) && !replace => replace = true,
                other => return other,
            }
        }
    }

    /// Every scale set of the repository's runner group.
    pub async fn list_scale_sets(&self) -> Result<Vec<ScaleSet>, Error> {
        let response = self
            .admin_call(
                Method::GET,
                &format!("runtime/runnerscalesets?runnerGroupId={RUNNER_GROUP}"),
                None,
            )
            .await?;
        Ok(decode::<Page<ScaleSet>>("scale sets", &response)?.value)
    }

    /// The scale set of this name, if there is one.
    pub async fn find_scale_set(&self, name: &str) -> Result<Option<ScaleSet>, Error> {
        let path = format!(
            "runtime/runnerscalesets?runnerGroupId={RUNNER_GROUP}&name={}",
            encode(name)
        );
        let response = self.admin_call(Method::GET, &path, None).await?;
        Ok(decode::<Page<ScaleSet>>("scale sets", &response)?
            .value
            .into_iter()
            .find(|s| s.name == name))
    }

    fn scale_set_body(name: &str, labels: &[String]) -> Value {
        let labels: Vec<Label> = labels
            .iter()
            .map(|l| Label {
                kind: "System".into(),
                name: l.clone(),
            })
            .collect();
        // The Actions runner is rebuilt with each release; one updating itself in a one-job machine would only
        // download the update again for every job.
        json!({"name": name, "runnerGroupId": RUNNER_GROUP, "labels": labels, "RunnerSetting": {"disableUpdate": true}, "enabled": true})
    }

    /// Finds the scale set of this name with exactly these labels, creating it or setting its labels when needed.
    pub async fn ensure_scale_set(&self, name: &str, labels: &[String]) -> Result<ScaleSet, Error> {
        let mut want: Vec<String> = labels.iter().map(|l| l.to_lowercase()).collect();
        want.sort();
        want.dedup();
        let body = Self::scale_set_body(name, &want);
        match self.find_scale_set(name).await? {
            None => {
                let response = self
                    .admin_call(Method::POST, "runtime/runnerscalesets", Some(body))
                    .await?;
                decode("scale set", &response)
            }
            Some(existing) if existing.label_names() != want => {
                let response = self
                    .admin_call(
                        Method::PATCH,
                        &format!("runtime/runnerscalesets/{}", existing.id),
                        Some(body),
                    )
                    .await?;
                decode("scale set", &response)
            }
            Some(existing) => Ok(existing),
        }
    }

    /// Deletes a scale set; its queued jobs wait for another runner.
    pub async fn delete_scale_set(&self, id: i64) -> Result<(), Error> {
        self.admin_call(Method::DELETE, &format!("runtime/runnerscalesets/{id}"), None)
            .await
            .map(|_| ())
    }

    /// Opens the message session of a scale set for `owner` (the machine's name). A session of the same owner that
    /// is still open makes this [`Error::SessionExists`]: it expires, and the call can be made again.
    pub async fn open_session(&self, scale_set_id: i64, owner: &str) -> Result<MessageSession, Error> {
        let body = json!({"ownerName": owner});
        let response = self
            .admin_call(
                Method::POST,
                &format!("runtime/runnerscalesets/{scale_set_id}/sessions"),
                Some(body),
            )
            .await
            .map_err(|e| match e.status() {
                Some(409) => Error::SessionExists,
                _ => e,
            })?;
        let info: SessionInfo = decode("message session", &response)?;
        Ok(MessageSession::new(self.clone(), scale_set_id, info))
    }

    /// Asks the service for a new token for a session.
    pub(crate) async fn refresh_session(
        &self,
        scale_set_id: i64,
        session_id: &str,
    ) -> Result<SessionInfo, Error> {
        let path = format!(
            "runtime/runnerscalesets/{scale_set_id}/sessions/{}",
            encode(session_id)
        );
        let response = self
            .admin_call(Method::PATCH, &path, None)
            .await
            .map_err(|e| match e.status() {
                Some(404) => Error::SessionLost("the service no longer has the session".into()),
                _ => e,
            })?;
        decode("message session", &response)
    }

    /// Ends a session.
    pub(crate) async fn delete_session(&self, scale_set_id: i64, session_id: &str) -> Result<(), Error> {
        let path = format!(
            "runtime/runnerscalesets/{scale_set_id}/sessions/{}",
            encode(session_id)
        );
        self.admin_call(Method::DELETE, &path, None).await.map(|_| ())
    }

    /// A call with a session's token (not the administrator's), to the session's own address.
    pub(crate) async fn session_call(
        &self,
        method: Method,
        url: &str,
        token: &Secret<String>,
        body: Option<&Value>,
        timeout: Option<Duration>,
        max_capacity: Option<usize>,
    ) -> Result<Response, Error> {
        let bearer = format!("Bearer {}", token.expose());
        let payload = body.map(Value::to_string);
        Ok(self
            .inner
            .app
            .http()
            .send(
                |c| {
                    let mut r = c
                        .request(method.clone(), url)
                        .header(AUTHORIZATION, &bearer)
                        .header("Accept", "application/json");
                    if let Some(n) = max_capacity {
                        r = r.header("X-ScaleSetMaxCapacity", n.to_string());
                    }
                    if let Some(p) = &payload {
                        r = r.header("Content-Type", "application/json").body(p.clone());
                    }
                    if let Some(t) = timeout {
                        r = r.timeout(t);
                    }
                    r
                },
                true,
            )
            .await?)
    }

    /// The Actions service's address for this repository.
    pub(crate) async fn service_url(&self) -> Result<String, Error> {
        Ok(self.admin(false).await?.0)
    }

    /// Registers a runner that takes one job and unregisters itself after it. A runner of this name that exists
    /// already is a conflict (409 in the error): names are random, so another try uses another name.
    pub async fn generate_jit_config(
        &self,
        scale_set_id: i64,
        name: &str,
        work_folder: &str,
    ) -> Result<JitConfig, Error> {
        let body = json!({"name": name, "workFolder": work_folder});
        let response = self
            .admin_call(
                Method::POST,
                &format!("runtime/runnerscalesets/{scale_set_id}/generatejitconfig"),
                Some(body),
            )
            .await?;
        let parsed: JitResponse = decode("runner configuration", &response)?;
        let runner = parsed.runner.unwrap_or(RunnerRef {
            id: 0,
            name: name.to_string(),
        });
        if parsed.encoded_jit_config.is_empty() {
            return Err(Error::Api(ApiError::Decode {
                what: "runner configuration".into(),
                why: "the service sent no configuration".into(),
            }));
        }
        Ok(JitConfig {
            runner,
            encoded: Secret::new(parsed.encoded_jit_config),
        })
    }

    /// Removes a registered runner. A runner with a job refuses (the service answers 422); one that is gone is
    /// [`Error::is_not_found`].
    pub async fn remove_runner(&self, runner_id: i64) -> Result<(), Error> {
        self.admin_call(
            Method::DELETE,
            &format!("distributedtask/pools/0/agents/{runner_id}"),
            None,
        )
        .await
        .map(|_| ())
    }
}

/// Percent-encodes a value for a query or a path segment.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
