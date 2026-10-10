//! A scale set client against the fake GitHub, with real sockets and the real GitHub App client in between.
#![allow(dead_code)]

pub mod recorder;

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use cf_fake_github::{FakeGithub, Options, test_key};
use cf_ghapp::{AppKey, Clock, GitHubApp, Http, RetryPolicy, http_client};
use cf_scaleset::{MessageSession, ScaleSet, ScaleSetClient};
use cf_secret::Secret;

pub const REPO: &str = "org/app";

/// The real time plus whatever a test moved it by.
pub struct TestClock(Mutex<Duration>);

impl TestClock {
    pub fn advance(&self, d: Duration) {
        *self.0.lock().unwrap() += d;
    }
}

impl Clock for TestClock {
    fn now(&self) -> SystemTime {
        SystemTime::now() + *self.0.lock().unwrap()
    }
}

pub struct Harness {
    pub fake: FakeGithub,
    pub client: ScaleSetClient,
    pub clock: Arc<TestClock>,
}

pub fn fast() -> RetryPolicy {
    RetryPolicy {
        retries: 2,
        base: Duration::from_millis(5),
        cap: Duration::from_millis(20),
        max_retry_after: Duration::from_secs(2),
    }
}

pub async fn harness_with(options: Options, retry: RetryPolicy, timeout: Duration) -> Harness {
    let fake = FakeGithub::start(options).await;
    fake.add_repo(REPO, true);
    let clock = Arc::new(TestClock(Mutex::new(Duration::ZERO)));
    let key = AppKey::from_pem(&Secret::new(test_key().private_pem.clone())).expect("key");
    let app = GitHubApp::new(
        "Iv23test",
        key,
        &fake.url(),
        Http::new(http_client(timeout), retry),
        clock.clone(),
    );
    let client = ScaleSetClient::new(
        app,
        1,
        REPO,
        &format!("{}/{REPO}", fake.url()),
        clock.clone(),
        timeout,
    );
    Harness { fake, client, clock }
}

pub async fn harness() -> Harness {
    harness_with(Options::default(), fast(), Duration::from_secs(10)).await
}

pub fn labels(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| s.to_string()).collect()
}

/// A scale set on `cf-2g` with an open session.
pub async fn with_session(h: &Harness) -> (ScaleSet, MessageSession) {
    let set = h
        .client
        .ensure_scale_set("air--cf-2g", &labels(&["self-hosted", "cf-2g"]))
        .await
        .expect("scale set");
    let session = h.client.open_session(set.id, "air").await.expect("session");
    (set, session)
}
