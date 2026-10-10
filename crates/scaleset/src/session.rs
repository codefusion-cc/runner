use std::sync::Arc;
use std::sync::Mutex;

use cf_ghapp::Error as ApiError;
use cf_secret::Secret;
use reqwest::Method;
use serde::Deserialize;
use serde_json::Value;

use crate::client::ScaleSetClient;
use crate::error::Error;
use crate::types::{Message, RawMessage, SessionInfo, Statistics};

const API_VERSION: &str = "6.0-preview";

struct State {
    session_id: String,
    queue_url: String,
    token: Secret<String>,
}

struct Inner {
    client: ScaleSetClient,
    scale_set_id: i64,
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
    #[serde(default)]
    value: Vec<i64>,
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

impl MessageSession {
    pub(crate) fn new(client: ScaleSetClient, scale_set_id: i64, info: SessionInfo) -> MessageSession {
        MessageSession {
            inner: Arc::new(Inner {
                client,
                scale_set_id,
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
        (
            s.session_id.clone(),
            s.queue_url.clone(),
            Secret::new(s.token.expose().clone()),
        )
    }

    async fn refresh(&self) -> Result<(), Error> {
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
        Ok(())
    }

    /// Waits for the next message after `after`, for about a minute. `Ok(None)` is a quiet end of the wait. A token
    /// the service refuses is replaced once and the wait goes on; a session the service no longer has is
    /// [`Error::SessionLost`]. `max_capacity` is how many runners the machine would take now (the service offers no
    /// more jobs than that).
    pub async fn poll(&self, after: i64, max_capacity: usize) -> Result<Option<Message>, Error> {
        let mut refreshed = false;
        loop {
            let (session_id, queue_url, token) = self.snapshot();
            let mut url = format!(
                "{queue_url}?sessionId={}&api-version={API_VERSION}",
                encode(&session_id)
            );
            if after > 0 {
                url.push_str(&format!("&lastMessageId={after}"));
            }
            let result = self
                .inner
                .client
                .session_call(
                    Method::GET,
                    &url,
                    &token,
                    None,
                    Some(self.inner.client.poll_timeout()),
                    Some(max_capacity),
                )
                .await;
            match result {
                Ok(r) if r.status.as_u16() == 202 || r.body.is_empty() => return Ok(None),
                Ok(r) => {
                    let raw: RawMessage = serde_json::from_slice(&r.body).map_err(|e| {
                        Error::Api(ApiError::Decode {
                            what: "message".into(),
                            why: e.to_string(),
                        })
                    })?;
                    return Ok(Some(raw.into_message()));
                }
                Err(e) if e.status() == Some(401) && !refreshed => {
                    refreshed = true;
                    self.refresh().await?;
                }
                Err(e) if e.status() == Some(404) => {
                    return Err(Error::SessionLost(
                        "the service has no queue for the session".into(),
                    ));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Tells the service the message was handled, so it is not sent again.
    pub async fn acknowledge(&self, message_id: i64) -> Result<(), Error> {
        let mut refreshed = false;
        loop {
            let (session_id, queue_url, token) = self.snapshot();
            let url = format!(
                "{queue_url}/{message_id}?sessionId={}&api-version={API_VERSION}",
                encode(&session_id)
            );
            match self
                .inner
                .client
                .session_call(Method::DELETE, &url, &token, None, None, None)
                .await
            {
                Ok(_) => return Ok(()),
                Err(e) if e.status() == Some(401) && !refreshed => {
                    refreshed = true;
                    self.refresh().await?;
                }
                Err(e) if e.status() == Some(404) => {
                    return Err(Error::SessionLost(
                        "the service has no queue for the session".into(),
                    ));
                }
                Err(e) => return Err(e),
            }
        }
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
        let mut refreshed = false;
        loop {
            let (_, _, token) = self.snapshot();
            match self
                .inner
                .client
                .session_call(Method::POST, &url, &token, Some(&body), None, None)
                .await
            {
                Ok(r) => {
                    let parsed: Acquired = serde_json::from_slice(&r.body).map_err(|e| {
                        Error::Api(ApiError::Decode {
                            what: "acquired jobs".into(),
                            why: e.to_string(),
                        })
                    })?;
                    // Only what was asked for counts: the service has no business handing out other IDs.
                    return Ok(parsed
                        .value
                        .into_iter()
                        .filter(|id| request_ids.contains(id))
                        .collect());
                }
                Err(e) if e.status() == Some(401) && !refreshed => {
                    refreshed = true;
                    self.refresh().await?;
                }
                Err(e) => return Err(e),
            }
        }
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
