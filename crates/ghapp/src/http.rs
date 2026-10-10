use std::net::IpAddr;
use std::time::Duration;

use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::redirect::Policy;
use reqwest::{RequestBuilder, StatusCode, Url};

use crate::error::{Error, StatusError};

/// The largest response body that is read. GitHub's answers to these calls are a few kilobytes, a workflow file at
/// most [`crate::MAX_WORKFLOW_FILE_BYTES`] before its encoding; anything longer is refused instead of held in memory.
pub const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// How many redirects on the same host are followed (a renamed repository answers with one).
const MAX_REDIRECTS: usize = 5;

/// Whether a token may go to this address: over HTTPS, or to this machine itself (the tests' fake GitHub).
pub fn is_safe_address(url: &Url) -> bool {
    if url.scheme() == "https" {
        return true;
    }
    let host = url.host_str().unwrap_or("");
    url.scheme() == "http"
        && (host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback()))
}

/// Whether two addresses are on the same scheme, host and port.
pub fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// The client every call goes through: a user agent, a connect timeout, a whole-request timeout, and redirects
/// followed only to the same scheme, host and port, so no answer can send a request and its token elsewhere.
fn client(timeout: Duration) -> reqwest::Client {
    let redirects = Policy::custom(|attempt| {
        let stays = attempt
            .previous()
            .last()
            .is_some_and(|from| same_origin(from, attempt.url()));
        if stays && attempt.previous().len() <= MAX_REDIRECTS {
            attempt.follow()
        } else {
            attempt.stop()
        }
    });
    reqwest::Client::builder()
        .user_agent(concat!("codefusion-runner/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout)
        .redirect(redirects)
        .build()
        .expect("a client with these settings builds")
}

/// How calls are tried again.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Tries after the first.
    pub retries: u32,
    /// The wait before the first retry; it doubles each time.
    pub base: Duration,
    /// The longest wait between tries.
    pub cap: Duration,
    /// The longest `Retry-After` that is waited out here. Asked for more, the call fails with the error, which
    /// carries the wait, and the caller decides.
    pub max_retry_after: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            retries: 3,
            base: Duration::from_millis(500),
            cap: Duration::from_secs(10),
            max_retry_after: Duration::from_secs(60),
        }
    }
}

/// A complete response: status, headers and the whole body.
#[derive(Debug)]
pub struct Response {
    /// The status.
    pub status: StatusCode,
    /// The headers.
    pub headers: HeaderMap,
    /// The body, read in full.
    pub body: bytes::Bytes,
}

/// Sends requests and tries them again when GitHub or the network is the problem.
///
/// A request is sent only over HTTPS (or to this machine itself), a redirect is followed only to the same scheme,
/// host and port, every request has a time limit, and no more than [`MAX_BODY_BYTES`] of an answer are read.
#[derive(Clone)]
pub struct Http {
    client: reqwest::Client,
    retry: RetryPolicy,
    max_body: usize,
}

impl Http {
    /// A client whose requests take at most `timeout` each (a single request may ask for longer: the long poll).
    pub fn new(timeout: Duration, retry: RetryPolicy) -> Http {
        Http {
            client: client(timeout),
            retry,
            max_body: MAX_BODY_BYTES,
        }
    }

    /// Reads at most this much of an answer instead of [`MAX_BODY_BYTES`].
    pub fn with_max_body(mut self, bytes: usize) -> Http {
        self.max_body = bytes;
        self
    }

    /// Sends the request `build` makes. With `retry`, a network failure, a 429, a 5xx, and a 403 that carries
    /// `Retry-After` are tried again after a wait with jitter (the header's wait when there is one); a status
    /// outside 2xx that is not retried comes back as [`Error::Status`]. A body that ends early is a
    /// [`Error::Transport`]; an address a token may not go to and a body over the limit are [`Error::Refused`].
    pub async fn send(
        &self,
        build: impl Fn(&reqwest::Client) -> RequestBuilder,
        retry: bool,
    ) -> Result<Response, Error> {
        let mut attempt = 0u32;
        loop {
            let result = self.once(build(&self.client)).await;
            let error = match result {
                Ok(response) => return Ok(response),
                Err(error) => error,
            };
            if !retry || attempt >= self.retry.retries || !error.is_transient() {
                return Err(error);
            }
            let wait = match error.retry_after() {
                Some(asked) if asked > self.retry.max_retry_after => return Err(error),
                Some(asked) => asked,
                None => backoff(self.retry.base, self.retry.cap, attempt),
            };
            tracing::debug!(attempt, ?wait, "trying a GitHub call again");
            tokio::time::sleep(wait).await;
            attempt += 1;
        }
    }

    async fn once(&self, request: RequestBuilder) -> Result<Response, Error> {
        let request = request
            .build()
            .map_err(|e| Error::Transport(e.without_url().to_string()))?;
        let method = request.method().to_string();
        let path = request.url().path().to_string();
        if !is_safe_address(request.url()) {
            return Err(Error::Refused(format!(
                "{method} {path}: not sent, {} is not an HTTPS address",
                request.url().host_str().unwrap_or("the address")
            )));
        }
        let mut response = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::Transport(e.without_url().to_string()))?;
        let status = response.status();
        let headers = response.headers().clone();
        let too_long = || {
            Error::Refused(format!(
                "{method} {path}: the answer is over {} bytes",
                self.max_body
            ))
        };
        if response
            .content_length()
            .is_some_and(|n| n > self.max_body as u64)
        {
            return Err(too_long());
        }
        let mut body = bytes::BytesMut::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| Error::Transport(format!("the response body ended early: {}", e.without_url())))?
        {
            if body.len() + chunk.len() > self.max_body {
                return Err(too_long());
            }
            body.extend_from_slice(&chunk);
        }
        let body = body.freeze();
        if status.is_success() {
            return Ok(Response {
                status,
                headers,
                body,
            });
        }
        let text = String::from_utf8_lossy(&body[..body.len().min(512)])
            .trim()
            .to_string();
        Err(Error::Status(StatusError {
            method,
            path,
            status: status.as_u16(),
            body: text,
            retry_after: retry_after(&headers),
            server_time: server_time(&headers),
        }))
    }
}

/// The wait `Retry-After` asks for, in whole seconds. (GitHub sends seconds; a date is not read.)
pub(crate) fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// An exponential wait with jitter: between half and the whole of `base * 2^attempt`, at most `cap`.
pub fn backoff(base: Duration, cap: Duration, attempt: u32) -> Duration {
    let full = base
        .saturating_mul(1u32.checked_shl(attempt.min(20)).unwrap_or(u32::MAX))
        .min(cap);
    let jitter = rand::random::<f64>();
    full.mul_f64(0.5 + jitter * 0.5)
}

impl std::fmt::Debug for Http {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Http")
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

/// GitHub's clock from the `Date` header (`Tue, 15 Nov 1994 08:12:31 GMT`), in seconds since the epoch.
pub(crate) fn server_time(headers: &HeaderMap) -> Option<i64> {
    let text = headers.get(reqwest::header::DATE)?.to_str().ok()?;
    parse_http_date(text)
}

pub(crate) fn parse_http_date(text: &str) -> Option<i64> {
    let format = time::format_description::parse_borrowed::<2>(
        "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT",
    )
    .ok()?;
    time::PrimitiveDateTime::parse(text.trim(), &format)
        .ok()
        .map(|t| t.assume_utc().unix_timestamp())
}
