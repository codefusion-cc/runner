use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use cf_secret::Secret;
use reqwest::Method;
use serde::Deserialize;

use crate::client::{DEFAULT_RUNNER_GROUP, Installation, PlainJit};
use crate::credentials::{AppCredentials, AppKey};
use crate::error::Error;
use crate::http::{Http, Response};
use crate::jwt::{Clock, Signer};
use crate::rest::{Installed, MAX_PAGES, Rest, decode, token_from};
use crate::source::{Pending, TokenSource};

/// An installation token is replaced this long before it expires.
const TOKEN_MARGIN_SECS: i64 = 300;

struct CachedToken {
    value: Secret<String>,
    expires: i64,
}

type TokenSlot = Arc<tokio::sync::Mutex<Option<CachedToken>>>;

/// GitHub tokens signed for with the App's own key on this machine: the App's JWT (iat a minute back, nine minutes,
/// GitHub's time learned from its `Date` header), installation tokens with every permission of the App (cached until
/// five minutes before they expire, one request at a time per installation), and the registration tokens and
/// one-job Actions runners those make.
pub struct KeyTokens {
    signer: Signer,
    rest: Rest,
    tokens: Mutex<HashMap<i64, TokenSlot>>,
}

impl std::fmt::Debug for KeyTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyTokens")
            .field("api", &self.rest.api)
            .finish_non_exhaustive()
    }
}

impl KeyTokens {
    /// The tokens of the App with this client ID and key, from GitHub's REST API at `api`.
    pub fn new(client_id: &str, key: AppKey, api: &str, http: Http, clock: Arc<dyn Clock>) -> KeyTokens {
        KeyTokens {
            signer: Signer::new(client_id.to_string(), key, clock),
            rest: Rest::new(api, http),
            tokens: Mutex::new(HashMap::new()),
        }
    }

    /// The tokens of the App in loaded credentials.
    pub fn from_credentials(
        credentials: AppCredentials,
        api: &str,
        http: Http,
        clock: Arc<dyn Clock>,
    ) -> KeyTokens {
        KeyTokens::new(&credentials.client_id, credentials.key, api, http, clock)
    }

    /// The App's own signed token, valid nine minutes and issued a minute in the past.
    pub fn jwt(&self) -> Result<Secret<String>, Error> {
        self.signer.token()
    }

    /// A call authenticated as the App. A 401 whose `Date` header shows the machine's clock is off is tried once
    /// more with a token signed by GitHub's time.
    async fn as_app(&self, method: Method, path: &str) -> Result<Response, Error> {
        let url = self.rest.url(path);
        let mut learned = false;
        loop {
            let jwt = self.jwt()?;
            match self.rest.request(method.clone(), &url, &jwt, None, None).await {
                Err(Error::Status(s)) if s.status == 401 && !learned => {
                    learned = true;
                    match s.server_time {
                        Some(t) if self.signer.learn_server_time(t) => continue,
                        _ => return Err(Error::Status(s)),
                    }
                }
                other => return other,
            }
        }
    }

    fn slot(&self, installation: i64) -> TokenSlot {
        self.tokens
            .lock()
            .expect("tokens")
            .entry(installation)
            .or_default()
            .clone()
    }

    /// Every account the App is installed on.
    async fn list_installations(&self) -> Result<Vec<Installation>, Error> {
        let first = "/app/installations?per_page=100";
        let mut all = vec![];
        let mut next = Some(first.to_string());
        let mut pages = 0;
        while let Some(page) = next {
            if pages == MAX_PAGES {
                return Err(Rest::too_many_pages(first));
            }
            let response = self.as_app(Method::GET, &page).await?;
            next = self.rest.next_page(&response);
            pages += 1;
            all.extend(decode::<Vec<Installation>>("installations", &response)?);
        }
        Ok(all)
    }

    /// An installation token, reused until five minutes before it expires. One call at a time per installation asks
    /// GitHub for a new one; the others wait for it.
    async fn installation_token(&self, installation: i64) -> Result<Secret<String>, Error> {
        let slot = self.slot(installation);
        let mut cached = slot.lock().await;
        let now = self.signer.now_epoch();
        if let Some(c) = cached.as_ref().filter(|c| now < c.expires - TOKEN_MARGIN_SECS) {
            return Ok(c.value.clone());
        }
        let response = self
            .as_app(
                Method::POST,
                &format!("/app/installations/{installation}/access_tokens"),
            )
            .await?;
        let parsed = token_from("installation token", &response)?;
        let value = Secret::new(parsed.token);
        *cached = Some(CachedToken {
            value: value.clone(),
            expires: parsed.expires_at.unix_timestamp(),
        });
        Ok(value)
    }

    /// Drops the cached token GitHub refused, unless another caller has replaced it already.
    async fn forget_token(&self, installation: i64, refused: &Secret<String>) {
        let slot = self.slot(installation);
        let mut cached = slot.lock().await;
        if cached.as_ref().is_some_and(|c| c.value == *refused) {
            *cached = None;
        }
    }

    async fn find_installation(&self, repo: &str) -> Result<i64, Error> {
        let response = self
            .as_app(Method::GET, &format!("/repos/{repo}/installation"))
            .await?;
        Ok(decode::<Installation>("repository installation", &response)?.id)
    }

    fn installed<'a>(&'a self, installation: i64, repo: &'a str) -> Installed<'a> {
        Installed {
            source: self,
            installation,
            repo: Some(repo),
        }
    }

    async fn registration(&self, installation: i64, repo: &str) -> Result<Secret<String>, Error> {
        let path = format!("/repos/{repo}/actions/runners/registration-token");
        let response = self
            .rest
            .as_installation(
                self.installed(installation, repo),
                Method::POST,
                &path,
                None,
                None,
            )
            .await?;
        let parsed = token_from("Actions runner registration token", &response)?;
        Ok(Secret::new(parsed.token))
    }

    async fn plain_jit(
        &self,
        installation: i64,
        repo: &str,
        name: &str,
        labels: &[String],
    ) -> Result<PlainJit, Error> {
        #[derive(Deserialize)]
        struct Registered {
            id: i64,
        }
        #[derive(Deserialize)]
        struct JitAnswer {
            runner: Registered,
            encoded_jit_config: String,
        }
        let body = serde_json::json!({
            "name": name, "runner_group_id": DEFAULT_RUNNER_GROUP, "labels": labels, "work_folder": "_work",
        });
        let path = format!("/repos/{repo}/actions/runners/generate-jitconfig");
        let response = self
            .rest
            .as_installation(
                self.installed(installation, repo),
                Method::POST,
                &path,
                None,
                Some(&body),
            )
            .await?;
        let answer: JitAnswer = decode("Actions runner configuration", &response)?;
        Ok(PlainJit {
            id: answer.runner.id,
            encoded: Secret::new(answer.encoded_jit_config),
        })
    }
}

impl TokenSource for KeyTokens {
    fn installations(&self) -> Pending<'_, Vec<Installation>> {
        Box::pin(self.list_installations())
    }

    fn token<'a>(&'a self, installation: i64, _repo: Option<&'a str>) -> Pending<'a, Secret<String>> {
        Box::pin(self.installation_token(installation))
    }

    fn refused<'a>(
        &'a self,
        installation: i64,
        token: &'a Secret<String>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(self.forget_token(installation, token))
    }

    fn repo_installation<'a>(&'a self, repo: &'a str) -> Pending<'a, i64> {
        Box::pin(self.find_installation(repo))
    }

    fn registration_token<'a>(&'a self, installation: i64, repo: &'a str) -> Pending<'a, Secret<String>> {
        Box::pin(self.registration(installation, repo))
    }

    fn generate_jit_config<'a>(
        &'a self,
        installation: i64,
        repo: &'a str,
        name: &'a str,
        labels: &'a [String],
    ) -> Pending<'a, PlainJit> {
        Box::pin(self.plain_jit(installation, repo, name, labels))
    }

    fn now_epoch(&self) -> i64 {
        self.signer.now_epoch()
    }

    fn clock_skew_secs(&self) -> i64 {
        self.signer.skew()
    }
}
