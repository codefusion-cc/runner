use std::time::Duration;

use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::{RequestBuilder, StatusCode};

use crate::error::{Error, StatusError};

/// A reqwest client with the settings GitHub's APIs need: a user agent, a connect timeout and a whole-request
/// timeout. The long poll needs a longer `timeout` than the REST calls.
pub fn http_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("codefusion-runner/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout)
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
#[derive(Clone)]
pub struct Http {
    client: reqwest::Client,
    retry: RetryPolicy,
}

impl Http {
    /// Wraps a client.
    pub fn new(client: reqwest::Client, retry: RetryPolicy) -> Http {
        Http { client, retry }
    }

    /// The underlying client, for building requests.
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Sends the request `build` makes. With `retry`, a network failure, a 429, a 5xx, and a 403 that carries
    /// `Retry-After` are tried again after a wait with jitter (the header's wait when there is one); a status
    /// outside 2xx that is not retried comes back as [`Error::Status`]. A body that ends early is a
    /// [`Error::Transport`].
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
        let response = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::Transport(e.without_url().to_string()))?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .bytes()
            .await
            .map_err(|e| Error::Transport(format!("the response body ended early: {}", e.without_url())))?;
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
