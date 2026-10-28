use cf_secret::Secret;
use reqwest::Method;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, IF_NONE_MATCH, LINK};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use time::OffsetDateTime;

use crate::error::Error;
use crate::http::{Http, Response};
use crate::source::TokenSource;

const API_VERSION: &str = "2022-11-28";

/// How many pages of one listing are read: 10 000 entries at GitHub's hundred a page.
pub(crate) const MAX_PAGES: usize = 100;

pub(crate) fn decode<T: DeserializeOwned>(what: &str, response: &Response) -> Result<T, Error> {
    serde_json::from_slice(&response.body).map_err(|e| Error::not_the_json(what, &e))
}

#[derive(Deserialize)]
pub(crate) struct TokenResponse {
    pub(crate) token: String,
    #[serde(with = "time::serde::rfc3339")]
    pub(crate) expires_at: OffsetDateTime,
}

/// The token in an answer; one without a token is no answer.
pub(crate) fn token_from(what: &str, response: &Response) -> Result<TokenResponse, Error> {
    let parsed: TokenResponse = decode(what, response)?;
    if parsed.token.is_empty() {
        return Err(Error::Decode {
            what: what.into(),
            why: "GitHub returned an empty token".into(),
        });
    }
    Ok(parsed)
}

/// Whose token a call takes: an installation's from a source, and the repository the call is about, when it is
/// about one.
#[derive(Clone, Copy)]
pub(crate) struct Installed<'a> {
    pub(crate) source: &'a dyn TokenSource,
    pub(crate) installation: i64,
    pub(crate) repo: Option<&'a str>,
}

/// GitHub's REST API at one root, through one [`Http`]: what the App client and the local key's source share.
#[derive(Clone)]
pub(crate) struct Rest {
    pub(crate) http: Http,
    pub(crate) api: String,
}

impl Rest {
    pub(crate) fn new(api: &str, http: Http) -> Rest {
        Rest {
            http,
            api: api.trim_end_matches('/').to_string(),
        }
    }

    /// The same client, sending each call once: for a call GitHub may have acted on although the answer failed.
    pub(crate) fn without_retries(&self) -> Rest {
        Rest {
            http: self.http.without_retries(),
            api: self.api.clone(),
        }
    }

    pub(crate) fn url(&self, path: &str) -> String {
        if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{}{path}", self.api)
        }
    }

    /// One call; with `etag`, GitHub answers 304 (an [`Error::Status`]) when what it would send has that tag.
    pub(crate) async fn request(
        &self,
        method: Method,
        url: &str,
        auth: &Secret<String>,
        etag: Option<&str>,
        body: Option<&serde_json::Value>,
    ) -> Result<Response, Error> {
        let bearer = format!("Bearer {}", auth.expose());
        let body = body.map(|body| body.to_string());
        self.http
            .send(|c| {
                let mut request = c
                    .request(method.clone(), url)
                    .header("Accept", "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", API_VERSION)
                    .header(AUTHORIZATION, &bearer);
                if let Some(tag) = etag {
                    request = request.header(IF_NONE_MATCH, tag);
                }
                if let Some(body) = &body {
                    request = request
                        .header(CONTENT_TYPE, "application/json")
                        .body(body.clone());
                }
                request
            })
            .await
    }

    /// A call with a token of `to.source` for `to.installation` (and the repository it is about, when it is about
    /// one); a 401 tells the source that token was refused and tries once with a new one.
    pub(crate) async fn as_installation(
        &self,
        to: Installed<'_>,
        method: Method,
        path: &str,
        etag: Option<&str>,
        body: Option<&serde_json::Value>,
    ) -> Result<Response, Error> {
        let Installed {
            source,
            installation,
            repo,
        } = to;
        let url = self.url(path);
        let mut retried = false;
        loop {
            let token = source.token(installation, repo).await?;
            match self.request(method.clone(), &url, &token, etag, body).await {
                Err(Error::Status(s)) if s.status == 401 && !retried => {
                    retried = true;
                    source.refused(installation, &token).await;
                }
                other => return other,
            }
        }
    }

    /// The next page's address from a `Link` header. Only pages on the same API are followed: a header must not
    /// send the token elsewhere.
    pub(crate) fn next_page(&self, response: &Response) -> Option<String> {
        let link = response.headers.get(LINK)?.to_str().ok()?;
        for part in link.split(',') {
            let part = part.trim();
            if !part.ends_with("rel=\"next\"") {
                continue;
            }
            let url = part.strip_prefix('<')?.split('>').next()?;
            let prefix = format!("{}/", self.api);
            return url.starts_with(&prefix).then(|| url.to_string());
        }
        None
    }

    /// The refusal of a listing that names a next page past [`MAX_PAGES`]: one that names one for ever is not read for
    /// ever.
    pub(crate) fn too_many_pages(first: &str) -> Error {
        Error::Refused(format!("{first}: a listing of more than {MAX_PAGES} pages"))
    }
}
