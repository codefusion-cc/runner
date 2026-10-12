use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use cf_secret::Secret;
use reqwest::Method;
use reqwest::header::{AUTHORIZATION, LINK};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use time::OffsetDateTime;

use crate::credentials::{AppCredentials, AppKey};
use crate::error::Error;
use crate::http::{Http, Response};
use crate::jwt::{Clock, Signer};

/// GitHub's REST API.
pub const API: &str = "https://api.github.com";

const API_VERSION: &str = "2022-11-28";

/// An installation token is replaced this long before it expires.
const TOKEN_MARGIN_SECS: i64 = 300;

/// How many pages of one listing are read: 10 000 entries at GitHub's hundred a page.
const MAX_PAGES: usize = 100;

/// One account the App is installed on.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Installation {
    /// The installation's ID.
    pub id: i64,
}

/// A repository the App may serve.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Repo {
    /// `owner/name`.
    pub full_name: String,
    /// Changes with every push, so workflows need reading again only when it does.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub pushed_at: Option<OffsetDateTime>,
    /// The repository's page, which the scale set client takes as its scope.
    #[serde(default)]
    pub html_url: String,
    /// Whether the repository is private.
    #[serde(default)]
    pub private: bool,
    /// Whether it is archived.
    #[serde(default)]
    pub archived: bool,
    /// Whether it is disabled.
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Deserialize)]
struct TokenResponse {
    token: String,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
}

struct CachedToken {
    value: Secret<String>,
    expires: i64,
}

/// A repository's workflow file contents by blob hash.
type Blobs = HashMap<String, Arc<Vec<u8>>>;

type TokenSlot = Arc<tokio::sync::Mutex<Option<CachedToken>>>;

struct Inner {
    signer: Signer,
    http: Http,
    api: String,
    tokens: Mutex<HashMap<i64, TokenSlot>>,
    /// Each repository's current workflow files by blob hash.
    blobs: Mutex<HashMap<String, Blobs>>,
}

/// A GitHub App's REST client. Cheap to clone; safe to share. It keeps installation tokens until shortly before
/// they expire and workflow files by their blob hash, so polling repositories costs little.
#[derive(Clone)]
pub struct GitHubApp {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for GitHubApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubApp")
            .field("api", &self.inner.api)
            .finish_non_exhaustive()
    }
}

impl GitHubApp {
    /// A client for the App with this client ID and key. `api` is the REST API root.
    pub fn new(client_id: &str, key: AppKey, api: &str, http: Http, clock: Arc<dyn Clock>) -> GitHubApp {
        GitHubApp {
            inner: Arc::new(Inner {
                signer: Signer::new(client_id.to_string(), key, clock),
                http,
                api: api.trim_end_matches('/').to_string(),
                tokens: Mutex::new(HashMap::new()),
                blobs: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// A client for the App in loaded credentials.
    pub fn from_credentials(
        credentials: AppCredentials,
        api: &str,
        http: Http,
        clock: Arc<dyn Clock>,
    ) -> GitHubApp {
        GitHubApp::new(&credentials.client_id, credentials.key, api, http, clock)
    }

    /// The REST API root.
    pub fn api(&self) -> &str {
        &self.inner.api
    }

    /// The HTTP client the calls go through.
    pub fn http(&self) -> &Http {
        &self.inner.http
    }

    /// GitHub's clock minus this machine's, in seconds, as learned from its responses (0 until it was refused for
    /// the time).
    pub fn clock_skew_secs(&self) -> i64 {
        self.inner.signer.skew()
    }

    /// GitHub's time as this machine knows it, in seconds since the epoch: its own clock plus that offset. What
    /// the expiry of a token GitHub issued is compared with.
    pub fn now_epoch(&self) -> i64 {
        self.inner.signer.now_epoch()
    }

    /// The App's own signed token, valid nine minutes and issued a minute in the past.
    pub fn jwt(&self) -> Result<Secret<String>, Error> {
        self.inner.signer.token()
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{}{path}", self.inner.api)
        }
    }

    async fn request(&self, method: Method, url: &str, auth: &Secret<String>) -> Result<Response, Error> {
        let url = url.to_string();
        let bearer = format!("Bearer {}", auth.expose());
        self.inner
            .http
            .send(|c| {
                c.request(method.clone(), &url)
                    .header("Accept", "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", API_VERSION)
                    .header(AUTHORIZATION, &bearer)
            })
            .await
    }

    /// A call authenticated as the App. A 401 whose `Date` header shows the machine's clock is off is tried once
    /// more with a token signed by GitHub's time.
    async fn as_app(&self, method: Method, path: &str) -> Result<Response, Error> {
        let url = self.url(path);
        let mut learned = false;
        loop {
            let jwt = self.jwt()?;
            match self.request(method.clone(), &url, &jwt).await {
                Err(Error::Status(s)) if s.status == 401 && !learned => {
                    learned = true;
                    match s.server_time {
                        Some(t) if self.inner.signer.learn_server_time(t) => continue,
                        _ => return Err(Error::Status(s)),
                    }
                }
                other => return other,
            }
        }
    }

    /// A call authenticated with an installation token; a 401 drops the cached token and tries once with a new one.
    async fn as_installation(
        &self,
        installation: i64,
        method: Method,
        path: &str,
    ) -> Result<Response, Error> {
        let url = self.url(path);
        let mut retried = false;
        loop {
            let token = self.installation_token(installation).await?;
            match self.request(method.clone(), &url, &token).await {
                Err(Error::Status(s)) if s.status == 401 && !retried => {
                    retried = true;
                    self.forget_token(installation, &token).await;
                }
                other => return other,
            }
        }
    }

    fn decode<T: DeserializeOwned>(&self, what: &str, response: &Response) -> Result<T, Error> {
        serde_json::from_slice(&response.body).map_err(|e| Error::not_the_json(what, &e))
    }

    /// The token in an answer; one without a token is no answer.
    fn token_from(&self, what: &str, response: &Response) -> Result<TokenResponse, Error> {
        let parsed: TokenResponse = self.decode(what, response)?;
        if parsed.token.is_empty() {
            return Err(Error::Decode {
                what: what.into(),
                why: "GitHub returned an empty token".into(),
            });
        }
        Ok(parsed)
    }

    /// Every page of a listing from `first` on, as the App itself or as an installation. A listing of more than
    /// [`MAX_PAGES`] pages is [`Error::Refused`]: one that names a next page for ever is not read for ever.
    async fn pages(&self, installation: Option<i64>, first: &str) -> Result<Vec<Response>, Error> {
        let mut pages = vec![];
        let mut next = Some(first.to_string());
        while let Some(page) = next {
            if pages.len() == MAX_PAGES {
                return Err(Error::Refused(format!(
                    "{first}: a listing of more than {MAX_PAGES} pages"
                )));
            }
            let response = match installation {
                Some(installation) => self.as_installation(installation, Method::GET, &page).await?,
                None => self.as_app(Method::GET, &page).await?,
            };
            next = self.next_page(&response);
            pages.push(response);
        }
        Ok(pages)
    }

    /// Lists every account the App is installed on.
    pub async fn installations(&self) -> Result<Vec<Installation>, Error> {
        let mut all = vec![];
        for page in self.pages(None, "/app/installations?per_page=100").await? {
            all.extend(self.decode::<Vec<Installation>>("installations", &page)?);
        }
        Ok(all)
    }

    fn slot(&self, installation: i64) -> TokenSlot {
        self.inner
            .tokens
            .lock()
            .expect("tokens")
            .entry(installation)
            .or_default()
            .clone()
    }

    /// Drops the cached token GitHub refused, unless another caller has replaced it already.
    async fn forget_token(&self, installation: i64, refused: &Secret<String>) {
        let slot = self.slot(installation);
        let mut cached = slot.lock().await;
        if cached.as_ref().is_some_and(|c| c.value == *refused) {
            *cached = None;
        }
    }

    /// An installation token, reused until five minutes before it expires. One call at a time per installation asks
    /// GitHub for a new one; the others wait for it.
    pub async fn installation_token(&self, installation: i64) -> Result<Secret<String>, Error> {
        let slot = self.slot(installation);
        let mut cached = slot.lock().await;
        let now = self.inner.signer.now_epoch();
        if let Some(c) = cached.as_ref().filter(|c| now < c.expires - TOKEN_MARGIN_SECS) {
            return Ok(c.value.clone());
        }
        let response = self
            .as_app(
                Method::POST,
                &format!("/app/installations/{installation}/access_tokens"),
            )
            .await?;
        let parsed = self.token_from("installation token", &response)?;
        let value = Secret::new(parsed.token);
        *cached = Some(CachedToken {
            value: value.clone(),
            expires: parsed.expires_at.unix_timestamp(),
        });
        Ok(value)
    }

    /// A token for registering Actions runners with a repository, which the scale set client trades for the Actions
    /// service's own.
    pub async fn registration_token(&self, installation: i64, repo: &str) -> Result<Secret<String>, Error> {
        let path = format!("/repos/{repo}/actions/runners/registration-token");
        let response = self.as_installation(installation, Method::POST, &path).await?;
        let parsed = self.token_from("Actions runner registration token", &response)?;
        Ok(Secret::new(parsed.token))
    }

    /// Whether a registered Actions runner of a repository has a job, by GitHub's own record. `None` when GitHub no
    /// longer has it: a one-job Actions runner unregisters itself once its job is over.
    pub async fn runner_busy(
        &self,
        installation: i64,
        repo: &str,
        runner_id: i64,
    ) -> Result<Option<bool>, Error> {
        #[derive(Deserialize)]
        struct RunnerState {
            busy: bool,
        }
        let path = format!("/repos/{repo}/actions/runners/{runner_id}");
        match self.as_installation(installation, Method::GET, &path).await {
            Ok(response) => Ok(Some(
                self.decode::<RunnerState>("Actions runner", &response)?.busy,
            )),
            Err(Error::Status(s)) if s.status == 404 => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The next page's address from a `Link` header. Only pages on the same API are followed: a header must not
    /// send the token elsewhere.
    fn next_page(&self, response: &Response) -> Option<String> {
        let link = response.headers.get(LINK)?.to_str().ok()?;
        for part in link.split(',') {
            let part = part.trim();
            if !part.ends_with("rel=\"next\"") {
                continue;
            }
            let url = part.strip_prefix('<')?.split('>').next()?;
            let prefix = format!("{}/", self.inner.api);
            return url.starts_with(&prefix).then(|| url.to_string());
        }
        None
    }

    /// Lists the repositories an installation gives the App.
    pub async fn repositories(&self, installation: i64) -> Result<Vec<Repo>, Error> {
        #[derive(Deserialize)]
        struct Page {
            repositories: Vec<Repo>,
        }
        let mut all = vec![];
        for page in self
            .pages(Some(installation), "/installation/repositories?per_page=100")
            .await?
        {
            all.extend(self.decode::<Page>("repositories", &page)?.repositories);
        }
        Ok(all)
    }

    /// The workflow files on the repository's default branch by file name; none when it has no `.github/workflows`
    /// folder. A file is fetched only when its content changed since the last call, and one over `max_file_bytes`
    /// (what the reader of the files would refuse anyway) not at all. Any other failure is an error, not "no
    /// workflows": that would drop the repository's Actions runners.
    pub async fn workflow_files(
        &self,
        installation: i64,
        repo: &str,
        max_file_bytes: u64,
    ) -> Result<BTreeMap<String, Vec<u8>>, Error> {
        #[derive(Deserialize)]
        struct Entry {
            name: String,
            #[serde(rename = "type")]
            kind: String,
            sha: String,
            #[serde(default)]
            size: u64,
        }
        #[derive(Deserialize)]
        struct Blob {
            content: String,
            encoding: String,
        }
        let listing = match self
            .as_installation(
                installation,
                Method::GET,
                &format!("/repos/{repo}/contents/.github/workflows"),
            )
            .await
        {
            Ok(r) => r,
            Err(Error::Status(s)) if s.status == 404 => {
                self.inner.blobs.lock().expect("blobs").remove(repo);
                return Ok(BTreeMap::new());
            }
            Err(e) => return Err(e),
        };
        let entries: Vec<Entry> = self.decode("workflow listing", &listing)?;
        let known = self
            .inner
            .blobs
            .lock()
            .expect("blobs")
            .get(repo)
            .cloned()
            .unwrap_or_default();
        let mut files = BTreeMap::new();
        let mut current = HashMap::new();
        for e in entries {
            let yaml = e.name.ends_with(".yml") || e.name.ends_with(".yaml");
            if e.kind != "file" || !yaml || e.size > max_file_bytes {
                continue;
            }
            let content = match known.get(&e.sha) {
                Some(c) => c.clone(),
                None => {
                    let response = self
                        .as_installation(
                            installation,
                            Method::GET,
                            &format!("/repos/{repo}/git/blobs/{}", e.sha),
                        )
                        .await?;
                    let blob: Blob = self.decode("workflow file", &response)?;
                    if blob.encoding != "base64" {
                        return Err(Error::Decode {
                            what: format!("{repo}/{}", e.name),
                            why: format!("unexpected encoding {:?}", blob.encoding),
                        });
                    }
                    let clean: String = blob.content.chars().filter(|c| !c.is_whitespace()).collect();
                    let bytes = STANDARD.decode(clean).map_err(|err| Error::Decode {
                        what: format!("{repo}/{}", e.name),
                        why: err.to_string(),
                    })?;
                    // The listing's size is a claim; what counts is what came.
                    if bytes.len() as u64 > max_file_bytes {
                        continue;
                    }
                    Arc::new(bytes)
                }
            };
            current.insert(e.sha.clone(), content.clone());
            files.insert(e.name, content.as_ref().clone());
        }
        self.inner
            .blobs
            .lock()
            .expect("blobs")
            .insert(repo.to_string(), current);
        Ok(files)
    }

    /// The repository's Actions variables, the organization's that reach it included. Needs the App's `Variables`
    /// permission: without it GitHub answers 403 (or 404), which comes back as the error so the caller can say
    /// what is missing. Organization variables are best effort: a refusal there leaves them out.
    pub async fn variables(&self, installation: i64, repo: &str) -> Result<BTreeMap<String, String>, Error> {
        #[derive(Deserialize)]
        struct Variable {
            name: String,
            value: String,
        }
        #[derive(Deserialize)]
        struct Page {
            variables: Vec<Variable>,
        }
        let mut vars = BTreeMap::new();
        for (path, required) in [
            (
                format!("/repos/{repo}/actions/organization-variables?per_page=30"),
                false,
            ),
            (format!("/repos/{repo}/actions/variables?per_page=30"), true),
        ] {
            let pages = match self.pages(Some(installation), &path).await {
                Ok(pages) => pages,
                Err(Error::Status(s)) if !required && (s.status == 403 || s.status == 404) => continue,
                Err(e) => return Err(e),
            };
            for page in &pages {
                for v in self.decode::<Page>("variables", page)?.variables {
                    vars.insert(v.name, v.value);
                }
            }
        }
        Ok(vars)
    }

    /// The App's installation that covers a repository.
    pub async fn repo_installation(&self, repo: &str) -> Result<i64, Error> {
        let response = self
            .as_app(Method::GET, &format!("/repos/{repo}/installation"))
            .await?;
        Ok(self
            .decode::<Installation>("repository installation", &response)?
            .id)
    }
}
