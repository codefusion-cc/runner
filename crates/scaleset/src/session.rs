use std::sync::{Arc, Mutex};
use std::time::Duration;

use cf_ghapp::Response;
use cf_secret::Secret;
use reqwest::Method;
use serde::Deserialize;
use serde_json::Value;

use crate::client::{API_VERSION, ScaleSetClient, decode, encode};
use crate::error::Error;
use crate::types::{Message, RawMessage, SessionInfo, Statistics};

struct State {
    session_id: String,
    queue_url: String,
    token: Secret<String>,
}

struct Inner {
    client: ScaleSetClient,
    scale_set_id: i64,
    owner: String,
    initial: Statistics,
    state: Mutex<State>,
}

/// A message session of one scale set: the service's queue of what happened to its jobs. Cheap to clone; the
/// listener polls it and the scaler acquires jobs through it.
#[derive(Clone)]
pub struct MessageSession {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for MessageSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageSession")
            .field("scale_set_id", &self.inner.scale_set_id)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct Acquired {
    #[serde(default, deserialize_with = "crate::types::null_default")]
    value: Vec<i64>,
}

/// A queue the service no longer has means the session is gone: the caller opens a new one.
fn lost(error: Error) -> Error {
    if error.status() == Some(404) {
        Error::SessionLost("the service has no queue for the session".into())
    } else {
        error
    }
}

impl MessageSession {
    pub(crate) fn new(
        client: ScaleSetClient,
        scale_set_id: i64,
        owner: &str,
        info: SessionInfo,
    ) -> MessageSession {
        MessageSession {
            inner: Arc::new(Inner {
                client,
                scale_set_id,
                owner: owner.to_string(),
                initial: info.statistics.unwrap_or_default(),
                state: Mutex::new(State {
                    session_id: info.session_id,
                    queue_url: info.message_queue_url,
                    token: Secret::new(info.message_queue_access_token),
                }),
            }),
        }
    }

    /// The scale set's counts at the moment the session opened.
    pub fn initial_statistics(&self) -> Statistics {
        self.inner.initial
    }

    /// The scale set the session belongs to.
    pub fn scale_set_id(&self) -> i64 {
        self.inner.scale_set_id
    }

    fn snapshot(&self) -> (String, String, Secret<String>) {
        let s = self.inner.state.lock().expect("session state");
        (s.session_id.clone(), s.queue_url.clone(), s.token.clone())
    }

    async fn refresh(&self) -> Result<Option<Statistics>, Error> {
        let (session_id, _, _) = self.snapshot();
        let info = self
            .inner
            .client
            .refresh_session(self.inner.scale_set_id, &session_id)
            .await?;
        let mut s = self.inner.state.lock().expect("session state");
        s.token = Secret::new(info.message_queue_access_token);
        if !info.message_queue_url.is_empty() {
            s.queue_url = info.message_queue_url;
        }
        Ok(info.statistics)
    }

    /// The scale set's counts now, when the service gives them. They otherwise come only with a message: a scale
    /// set the service sends nothing keeps the counts of its last one, and a job another machine took since is still
    /// counted. The session and its queue stay; its token is a new one.
    pub async fn recount(&self) -> Result<Option<Statistics>, Error> {
        self.refresh().await
    }

    /// Sends the request `url` names for the session as it is now. A token the service refuses is replaced once and
    /// the request made again; a second refusal is the caller's error.
    async fn call(
        &self,
        method: Method,
        url: impl Fn(&str, &str) -> String,
        body: Option<&Value>,
        timeout: Option<Duration>,
        capacity: Option<usize>,
    ) -> Result<Response, Error> {
        let mut refreshed = false;
        loop {
            let (session_id, queue_url, token) = self.snapshot();
            let url = url(&session_id, &queue_url);
            let result = self
                .inner
                .client
                .send(method.clone(), &url, &token, body, timeout, capacity)
                .await;
            match result {
                Err(e) if e.status() == Some(401) && !refreshed => {
                    refreshed = true;
                    self.refresh().await?;
                }
                other => return other,
            }
        }
    }

    /// Waits for the next message after `after`, for about a minute. `Ok(None)` is a quiet end of the wait. A token the
    /// service refuses is replaced once and the wait goes on; a session the service no longer has is
    /// [`Error::SessionLost`]. `max_capacity` is how many Actions runners the machine would take now (the service
    /// offers no more jobs than that).
    pub async fn poll(&self, after: i64, max_capacity: usize) -> Result<Option<Message>, Error> {
        let url = |session_id: &str, queue_url: &str| {
            let mut url = format!(
                "{queue_url}?sessionId={}&api-version={API_VERSION}",
                encode(session_id)
            );
            if after > 0 {
                url.push_str(&format!("&lastMessageId={after}"));
            }
            url
        };
        let timeout = Some(self.inner.client.poll_timeout());
        let response = self
            .call(Method::GET, url, None, timeout, Some(max_capacity))
            .await
            .map_err(lost)?;
        if response.status.as_u16() == 202 || response.body.is_empty() {
            return Ok(None);
        }
        let raw: RawMessage = decode("message", &response)?;
        Ok(Some(raw.into_message()))
    }

    /// Tells the service the message was handled, so it is not sent again.
    pub async fn acknowledge(&self, message_id: i64) -> Result<(), Error> {
        let url = |session_id: &str, queue_url: &str| {
            format!(
                "{queue_url}/{message_id}?sessionId={}&api-version={API_VERSION}",
                encode(session_id)
            )
        };
        self.call(Method::DELETE, url, None, None, None)
            .await
            .map_err(lost)?;
        Ok(())
    }

    /// Acquires jobs by their request IDs and returns the ones this scale set got; the others went to another
    /// machine first (or are gone).
    pub async fn acquire_jobs(&self, request_ids: &[i64]) -> Result<Vec<i64>, Error> {
        if request_ids.is_empty() {
            return Ok(vec![]);
        }
        let base = self.inner.client.service_url().await?;
        let url = format!(
            "{base}/_apis/runtime/runnerscalesets/{}/acquirejobs?api-version={API_VERSION}",
            self.inner.scale_set_id
        );
        let body = Value::from(request_ids.to_vec());
        let response = self
            .call(Method::POST, |_, _| url.clone(), Some(&body), None, None)
            .await?;
        let acquired: Acquired = decode("acquired jobs", &response)?;
        // Only what was asked for counts, and once: the service has no business handing out other IDs, and a job
        // named twice is still one job.
        let mut got: Vec<i64> = vec![];
        for id in acquired.value {
            if request_ids.contains(&id) && !got.contains(&id) {
                got.push(id);
            }
        }
        Ok(got)
    }

    /// Ends the session at the service and opens a new one in its place, which every clone uses from then on.
    /// Returns the scale set's counts at the new session's start. The service looks at its queued jobs when a session
    /// opens, and not when a poll offers more room than the one before, so this is how a scale set that got room
    /// while nothing happened at GitHub gets its queued jobs. What the old session's queue still held is lost: the
    /// counts say what is assigned. A session the service no longer has is no error here.
    pub async fn reopen(&self) -> Result<Statistics, Error> {
        match self.close().await {
            Ok(()) => {}
            Err(error) if error.status() == Some(404) => {}
            Err(error) => return Err(error),
        }
        let fresh = self
            .inner
            .client
            .open_session(self.inner.scale_set_id, &self.inner.owner)
            .await?;
        let (session_id, queue_url, token) = fresh.snapshot();
        *self.inner.state.lock().expect("session state") = State {
            session_id,
            queue_url,
            token,
        };
        Ok(fresh.initial_statistics())
    }

    /// Ends the session on the service, so the next one of this machine opens without waiting for this one to
    /// expire.
    pub async fn close(&self) -> Result<(), Error> {
        let (session_id, _, _) = self.snapshot();
        self.inner
            .client
            .delete_session(self.inner.scale_set_id, &session_id)
            .await
    }
}
