//! A public repository's releases, read with no token: the Runner's own, which a host that took it from a release
//! updates from. What a release lists and where its files are come from GitHub's API; a file is fetched from the
//! address the release names, which GitHub sends on to its storage, and is hashed as it is written, never held whole.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reqwest::{StatusCode, Url};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::error::{Error, StatusError};
use crate::http::{Http, RetryPolicy, file_client, is_safe_address, retry_after, server_time};

/// How long a download may go without a byte before it counts as dropped.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// How often a download that waits, for its next bytes or to try again, looks whether it was stopped.
const STOP_LOOK: Duration = Duration::from_millis(200);

/// How many releases one page lists: a new release is among the first, and an image release lists many files.
const RELEASES_PAGE: u32 = 30;

/// A release as GitHub's API lists it, with the fields an update reads.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Release {
    /// Its tag, such as `runner-0.1.0-446125c0a1b2`.
    pub tag_name: String,
    /// The commit its tag was made at, as the release names it: the whole name when the workflow gave one.
    pub target_commitish: String,
    /// A draft is no release yet.
    #[serde(default)]
    pub draft: bool,
    /// Who made it: the release workflow is `github-actions[bot]`.
    #[serde(default)]
    pub author: Option<Author>,
    /// Its files.
    #[serde(default)]
    pub assets: Vec<Asset>,
}

/// Who made a release.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Author {
    /// The account's login.
    pub login: String,
}

/// A file of a release.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Asset {
    /// Its name.
    pub name: String,
    /// Its size in bytes.
    pub size: u64,
    /// Where it is fetched from.
    pub browser_download_url: String,
}

impl Release {
    /// The file named `name`, when the release has one.
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.name == name)
    }
}

/// How two commits stand in a branch's history, as GitHub's compare says from `base` to `head`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compared {
    /// `head` has `base` in its history, and more.
    Ahead,
    /// `base` has `head` in its history, and more.
    Behind,
    /// The same commit.
    Identical,
    /// Each has commits the other does not.
    Diverged,
}

/// What a download wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downloaded {
    /// How many bytes.
    pub bytes: u64,
    /// Their SHA-256, in lowercase hex.
    pub sha256: String,
}

/// Where a download writes: a sink that can be emptied to begin again when a try breaks off.
pub trait Sink: Write {
    /// Empties the sink, so the next try writes from the first byte.
    fn restart(&mut self) -> std::io::Result<()>;
}

impl Sink for std::fs::File {
    fn restart(&mut self) -> std::io::Result<()> {
        use std::io::Seek as _;
        self.set_len(0)?;
        self.rewind().map(|_| ())
    }
}

impl Sink for Vec<u8> {
    fn restart(&mut self) -> std::io::Result<()> {
        self.clear();
        Ok(())
    }
}

/// The releases of one public repository, read without a token.
#[derive(Clone, Debug)]
pub struct Releases {
    http: Http,
    files: reqwest::Client,
    retry: RetryPolicy,
    api: Url,
    repo: String,
}

impl Releases {
    /// The releases of `repo` (`owner/name`) at the API `api` (`https://api.github.com`, or this machine's fake),
    /// each call tried again as `retry` says.
    pub fn new(api: &str, repo: &str, retry: RetryPolicy) -> Result<Releases, Error> {
        let api = Url::parse(api.trim_end_matches('/'))
            .map_err(|error| Error::Refused(format!("{api} is no address: {error}")))?;
        if !is_safe_address(&api) {
            return Err(Error::Refused(format!("{api} is not an HTTPS address")));
        }
        if !repo
            .split_once('/')
            .is_some_and(|(owner, name)| plain(owner) && plain(name))
        {
            return Err(Error::Refused(format!("{repo} is no owner/name")));
        }
        Ok(Releases {
            http: Http::new(Duration::from_secs(30), retry),
            files: file_client(READ_TIMEOUT),
            retry,
            api,
            repo: repo.to_string(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/repos/{}/{path}",
            self.api.as_str().trim_end_matches('/'),
            self.repo
        )
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, what: &str, path: &str) -> Result<T, Error> {
        let url = self.url(path);
        let response = self
            .http
            .send(|client| {
                client
                    .get(&url)
                    .header("Accept", "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28")
            })
            .await?;
        serde_json::from_slice(&response.body).map_err(|error| Error::not_the_json(what, &error))
    }

    /// The release of `tag`, or `None` when there is none (GitHub answers 404).
    pub async fn by_tag(&self, tag: &str) -> Result<Option<Release>, Error> {
        if !plain(tag) {
            return Err(Error::Refused(format!("{tag} is no plain tag")));
        }
        match self.get("the release", &format!("releases/tags/{tag}")).await {
            Ok(release) => Ok(Some(release)),
            Err(error) if error.status() == Some(404) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// One page of the releases, the newest first (`page` from 1).
    pub async fn page(&self, page: u32) -> Result<Vec<Release>, Error> {
        self.get(
            "the releases",
            &format!("releases?per_page={RELEASES_PAGE}&page={page}"),
        )
        .await
    }

    /// The commit a branch is at: its whole name, read from the branch's ref, which no tag of the same name stands
    /// for.
    pub async fn branch_head(&self, branch: &str) -> Result<String, Error> {
        #[derive(Deserialize)]
        struct Ref {
            object: Object,
        }
        #[derive(Deserialize)]
        struct Object {
            sha: String,
        }
        let what = format!("the branch {branch}");
        let found: Ref = self.get(&what, &format!("git/ref/heads/{branch}")).await?;
        if !is_whole_commit(&found.object.sha) {
            return Err(Error::Decode {
                what,
                why: "the answer names no commit".into(),
            });
        }
        Ok(found.object.sha)
    }

    /// How `head` stands to `base` in the repository's history.
    pub async fn compare(&self, base: &str, head: &str) -> Result<Compared, Error> {
        #[derive(Deserialize)]
        struct Comparison {
            status: String,
        }
        let what = format!("the compare of {base} and {head}");
        // One commit a page: only the status is read.
        let found: Comparison = self
            .get(&what, &format!("compare/{base}...{head}?per_page=1"))
            .await?;
        match found.status.as_str() {
            "ahead" => Ok(Compared::Ahead),
            "behind" => Ok(Compared::Behind),
            "identical" => Ok(Compared::Identical),
            "diverged" => Ok(Compared::Diverged),
            _ => Err(Error::Decode {
                what,
                why: "the answer's status is none of ahead, behind, identical and diverged".into(),
            }),
        }
    }

    /// Fetches the file at `url` into `sink`, at most `max` bytes, hashed as it is written. A try that breaks off
    /// (no connection, a body that ends early, a 429 or a 5xx) begins again from the first byte, as the retry
    /// policy says. A sink that cannot be written, a file over `max`, an address that is not HTTPS and `stop` set
    /// end it at once.
    pub async fn download(
        &self,
        url: &str,
        sink: &mut dyn Sink,
        max: u64,
        stop: &AtomicBool,
    ) -> Result<Downloaded, Error> {
        let url = Url::parse(url).map_err(|_| Error::Refused("a release's file has no address".into()))?;
        if !is_safe_address(&url) {
            return Err(Error::Refused(format!(
                "{} is not an HTTPS address",
                url.host_str().unwrap_or("a release's file")
            )));
        }
        let mut attempt = 0u32;
        loop {
            if attempt > 0 {
                sink.restart().map_err(|error| local(&error))?;
            }
            let error = match self.download_once(&url, sink, max, stop).await {
                Ok(downloaded) => return Ok(downloaded),
                Err(error) => error,
            };
            if stop.load(Ordering::SeqCst) {
                return Err(error);
            }
            let Some(wait) = self.retry.wait_after(&error, attempt) else {
                return Err(error);
            };
            tracing::debug!(attempt, ?wait, "fetching a release's file again");
            // A Retry-After may ask for minutes.
            if !sleep_unless(stop, wait).await {
                return Err(stopped());
            }
            attempt += 1;
        }
    }

    async fn download_once(
        &self,
        url: &Url,
        sink: &mut dyn Sink,
        max: u64,
        stop: &AtomicBool,
    ) -> Result<Downloaded, Error> {
        let path = url.path().to_string();
        let mut response = self
            .files
            .get(url.clone())
            .send()
            .await
            .map_err(|e| Error::Transport(e.without_url().to_string()))?;
        let status = response.status();
        if status != StatusCode::OK {
            let headers = response.headers().clone();
            return Err(Error::Status(StatusError {
                method: "GET".into(),
                path,
                status: status.as_u16(),
                body: String::new(),
                retry_after: retry_after(&headers),
                server_time: server_time(&headers),
            }));
        }
        let too_long = || Error::Refused(format!("GET {path}: the file is over {max} bytes"));
        if response.content_length().is_some_and(|n| n > max) {
            return Err(too_long());
        }
        let mut hash = Sha256::new();
        let mut bytes = 0u64;
        loop {
            if stop.load(Ordering::SeqCst) {
                return Err(stopped());
            }
            // A connection that stalls is looked at for `stop` while it waits for its next bytes.
            let Ok(next) = tokio::time::timeout(STOP_LOOK, response.chunk()).await else {
                continue;
            };
            let Some(chunk) =
                next.map_err(|e| Error::Transport(format!("the file ended early: {}", e.without_url())))?
            else {
                break;
            };
            bytes += chunk.len() as u64;
            if bytes > max {
                return Err(too_long());
            }
            sink.write_all(&chunk).map_err(|error| local(&error))?;
            hash.update(&chunk);
        }
        sink.flush().map_err(|error| local(&error))?;
        Ok(Downloaded {
            bytes,
            sha256: format!("{:x}", hash.finalize()),
        })
    }
}

/// Sleeps for `time`, looking at `stop` meanwhile: `false` when it was set before the time was up.
pub async fn sleep_unless(stop: &AtomicBool, time: Duration) -> bool {
    let until = tokio::time::Instant::now() + time;
    loop {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        let now = tokio::time::Instant::now();
        if now >= until {
            return true;
        }
        tokio::time::sleep_until(until.min(now + STOP_LOOK)).await;
    }
}

fn stopped() -> Error {
    Error::Local("the download was stopped".into())
}

/// A write on this host that failed: no room on its disk, most often.
fn local(error: &std::io::Error) -> Error {
    if error.kind() == std::io::ErrorKind::StorageFull {
        Error::Local(format!("no room on this host's disk: {error}"))
    } else {
        Error::Local(format!("the file could not be written: {error}"))
    }
}

/// Whether `name` is a plain name in a GitHub address: letters, digits, `-`, `_` and `.`, and not empty.
fn plain(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Whether `name` is a whole commit's name: 40 lowercase hex characters.
pub fn is_whole_commit(name: &str) -> bool {
    name.len() == 40 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
