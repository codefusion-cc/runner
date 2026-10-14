use cf_ghapp::{Error as HttpError, Http, Response};
use cf_secret::Secret;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::Error;
use crate::outbox::{Outbox, REPORT_BATCH};
use crate::wire::{Answer, Enroll, Enrolled, PROTOCOL, Report};

/// The longest body the console reads, 512 KiB. A report over it is refused here before it is sent.
pub const MAX_BODY_BYTES: usize = 512 * 1024;
/// The most job events one report carries.
pub const MAX_JOB_EVENTS: usize = 500;
/// The most command results one report carries.
pub const MAX_RESULTS: usize = 50;
/// The most Actions runners one status lists.
pub const MAX_RUNNERS: usize = 64;
/// The most Docker caches one status lists.
pub const MAX_DOCKER_CACHES: usize = 64;
/// The most Docker cache events one status carries.
pub const MAX_DOCKER_CACHE_EVENTS: usize = 32;
/// The most scale sets one status lists.
pub const MAX_SCALE_SETS: usize = 500;
/// The longest Mac name.
pub const MACHINE_NAME_MAX: usize = 24;
/// The longest Actions runner name in a job event.
pub const RUNNER_NAME_MAX: usize = 100;
/// The longest command id in a result.
pub const RESULT_ID_MAX: usize = 64;

/// Whether `name` is a Mac name the console accepts: lower case letters and digits in groups joined by single
/// hyphens, 1 to 24 characters.
pub fn is_machine_name(name: &str) -> bool {
    name.len() <= MACHINE_NAME_MAX
        && name.split('-').all(|part| {
            !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// The number of UTF-16 units, which is how the console counts a text's length.
pub(crate) fn units(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Whether `token` has the shape the console issues: `cfr_` and 43 characters of `A-Z a-z 0-9 _ -`.
fn is_token(token: &str) -> bool {
    token.strip_prefix("cfr_").is_some_and(|rest| {
        rest.len() == 43
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

fn endpoint(base: &str, call: &str) -> Result<reqwest::Url, Error> {
    reqwest::Url::parse(&format!("{}/agent/runners/{call}", base.trim_end_matches('/')))
        .map_err(|_| Error::Protocol("the console's address is not a URL".into()))
}

/// The request body: the message with the protocol version in it.
fn body_of<T: Serialize>(message: &T) -> Result<Vec<u8>, Error> {
    let mut value = serde_json::to_value(message)
        .map_err(|_| Error::Protocol("the message cannot be written as JSON".into()))?;
    match value.as_object_mut() {
        Some(object) => object.insert("protocol".into(), PROTOCOL.into()),
        None => return Err(Error::Protocol("the message is not an object".into())),
    };
    serde_json::to_vec(&value).map_err(|_| Error::Protocol("the message cannot be written as JSON".into()))
}

/// An answer read as `T`. The error says where the answer is wrong and quotes none of it.
fn read<T: DeserializeOwned>(what: &str, response: &Response) -> Result<T, Error> {
    serde_json::from_slice(&response.body).map_err(|error| Error::from(HttpError::not_the_json(what, &error)))
}

/// Trades a one-time code for the Mac's own token.
///
/// A Mac enrolling again under its name keeps its history and gets a new token; the old one stops working. The code is
/// spent by the first request that reaches the console, so pass an [`Http`] that does not retry (`retries: 0`): a
/// retry after an answer was lost meets [`Error::Unauthorized`] and the token is gone. Checks the console makes first
/// (the name, the code's length, the CPU count) refuse here as [`Error::Protocol`]; a code for another Mac is
/// [`Error::Refused`] with status 403.
pub async fn enroll(http: &Http, base: &str, enroll: &Enroll) -> Result<Enrolled, Error> {
    if !is_machine_name(&enroll.machine) {
        return Err(Error::Protocol(
            "the host's name is not one the console accepts".into(),
        ));
    }
    if enroll.code.expose().is_empty() || units(enroll.code.expose()) > 200 {
        return Err(Error::Protocol("the code is empty or over 200 characters".into()));
    }
    if !(1..=4096).contains(&enroll.cpus) {
        return Err(Error::Protocol("the CPU count is not 1 to 4096".into()));
    }
    let mut message = serde_json::json!({
        "protocol": PROTOCOL,
        "code": enroll.code.expose(),
        "machine": enroll.machine,
        "hostname": enroll.hostname,
        "model": enroll.model,
        "version": enroll.version,
        "cpus": enroll.cpus,
        "memoryBytes": enroll.memory_bytes,
    });
    if let Some(os) = enroll.os {
        message["os"] = serde_json::json!(os);
    }
    let body = serde_json::to_vec(&message)
        .map_err(|_| Error::Protocol("the request cannot be written as JSON".into()))?;
    let url = endpoint(base, "enroll")?;
    let response = http
        .send(|client| {
            client
                .post(url.clone())
                .header("content-type", "application/json")
                .body(body.clone())
        })
        .await?;
    #[derive(serde::Deserialize)]
    struct Wire {
        token: String,
        name: String,
    }
    let wire: Wire = read("the enrolment answer", &response)?;
    if !is_token(&wire.token) {
        return Err(Error::Protocol("the token is not one the console issues".into()));
    }
    if wire.name != enroll.machine {
        return Err(Error::Protocol("the console enrolled another name".into()));
    }
    Ok(Enrolled {
        token: Secret::new(wire.token),
        name: wire.name,
    })
}

/// Checks a report against what the console refuses outright, so that nothing is sent that cannot be accepted.
fn check(report: &Report, body: &[u8]) -> Result<(), Error> {
    let refuse = |why: &str| Err(Error::Protocol(why.to_string()));
    if !is_machine_name(&report.status.machine) {
        return refuse("the host's name is not one the console accepts");
    }
    if report.jobs.len() > MAX_JOB_EVENTS {
        return refuse("more than 500 job events in one report");
    }
    if report.results.len() > MAX_RESULTS {
        return refuse("more than 50 command results in one report");
    }
    if report.status.runners.len() > MAX_RUNNERS {
        return refuse("more than 64 Actions runners in one status");
    }
    if report.status.docker_caches.len() > MAX_DOCKER_CACHES {
        return refuse("more than 64 Docker caches in one status");
    }
    if report.status.docker_cache_events.len() > MAX_DOCKER_CACHE_EVENTS {
        return refuse("more than 32 Docker cache events in one status");
    }
    if report.status.scale_sets.len() > MAX_SCALE_SETS {
        return refuse("more than 500 scale sets in one status");
    }
    if report
        .jobs
        .iter()
        .any(|e| e.runner.is_empty() || units(&e.runner) > RUNNER_NAME_MAX)
    {
        return refuse("a job event names no Actions runner, or one over 100 characters");
    }
    if report
        .results
        .iter()
        .any(|r| r.id.is_empty() || units(&r.id) > RESULT_ID_MAX)
    {
        return refuse("a command result has no id, or one over 64 characters");
    }
    if body.len() > MAX_BODY_BYTES {
        return refuse("the report is over 512 KiB");
    }
    Ok(())
}

/// The console's Runner endpoints (protocol 1), called with the Mac's own token.
///
/// The token is a [`Secret`]: it shows in no `Debug` output and no [`Error`]. Every call is a result; a refusal, a dead
/// connection and a wrong answer are errors to look at, not panics.
#[derive(Debug, Clone)]
pub struct Client {
    base: String,
    token: Secret<String>,
    http: Http,
}

impl Client {
    /// A client of the console at `base` (an HTTPS address, or this machine itself in tests) with the Mac's token.
    /// `http` sets the time limit and the retries of every call; a report is safe to send again (see [`Outbox`]).
    pub fn new(base: &str, token: Secret<String>, http: Http) -> Client {
        Client {
            base: base.to_string(),
            token,
            http,
        }
    }

    async fn post(&self, call: &str, body: Vec<u8>) -> Result<Response, Error> {
        // A token of another shape is not the console's: it would be refused, and may not even be a header.
        if !is_token(self.token.expose()) {
            return Err(Error::Unauthorized);
        }
        let url = endpoint(&self.base, call)?;
        Ok(self
            .http
            .send(|client| {
                client
                    .post(url.clone())
                    .bearer_auth(self.token.expose())
                    .header("content-type", "application/json")
                    .body(body.clone())
            })
            .await?)
    }

    /// Sends a report as it is, with the job events in it. Returns what the console wants done.
    ///
    /// Refuses as [`Error::Protocol`], without sending, what the console would refuse: a name that is not a Mac name,
    /// more than 500 job events, 50 results, 64 Actions runners or 500 scale sets, an event without an Actions runner
    /// or with one over 100 characters, a body over 512 KiB. To send events from the outbox use [`Client::deliver`].
    pub async fn report(&self, report: &Report) -> Result<Answer, Error> {
        let body = body_of(report)?;
        check(report, &body)?;
        let response = self.post("report", body).await?;
        let answer: Answer = read("the report's answer", &response)?;
        if answer.report_every == 0 {
            return Err(Error::Protocol(
                "the console asks for a report every 0 seconds".into(),
            ));
        }
        Ok(answer)
    }

    /// Sends the report with the outbox's next batch of job events (at most [`REPORT_BATCH`]) in it, replacing the
    /// report's own `jobs`. Those events leave the outbox only when the console's answer came and is the contract's:
    /// a refusal, a lost connection and a 200 that is not the console's mark nothing. An event whose report the
    /// console took but whose answer was lost goes again with the same id.
    ///
    /// Calls on one outbox run one after the other, so no event is in two reports at once.
    pub async fn deliver(&self, outbox: &Outbox, mut report: Report) -> Result<Answer, Error> {
        let _one_at_a_time = outbox.delivering().await;
        report.jobs = outbox.batch(REPORT_BATCH);
        let answer = self.report(&report).await?;
        outbox.sent(report.jobs.iter().map(|e| e.id.as_str()));
        Ok(answer)
    }

    /// Tells the console this Mac is gone (uninstalled). The console forgets the Mac and keeps its job history; the
    /// token stops working.
    pub async fn leave(&self) -> Result<(), Error> {
        let response = self.post("leave", b"{}".to_vec()).await?;
        #[derive(serde::Deserialize)]
        struct Wire {
            ok: bool,
        }
        let wire: Wire = read("the leave answer", &response)?;
        if wire.ok {
            Ok(())
        } else {
            Err(Error::Protocol("the console did not say ok".into()))
        }
    }
}
