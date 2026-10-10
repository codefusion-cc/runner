//! The fake GitHub with the real clients in front of it: what every crate's tests start from.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use cf_ghapp::{AppKey, Clock, GitHubApp, Http, RetryPolicy};
use cf_scaleset::ScaleSetClient;
use cf_secret::Secret;

use crate::httpd::{Handler, Reply, Server};
use crate::{FakeGithub, Options, test_key};

/// The repository the harness installs the App on.
pub const REPO: &str = "org/app";

/// The real time plus whatever a test moved it by.
#[derive(Debug, Default)]
pub struct TestClock(Mutex<Duration>);

impl TestClock {
    /// Moves the clock forward.
    pub fn advance(&self, d: Duration) {
        *self.0.lock().expect("clock") += d;
    }
}

impl Clock for TestClock {
    fn now(&self) -> SystemTime {
        SystemTime::now() + *self.0.lock().expect("clock")
    }
}

/// Retries that take milliseconds, and a `Retry-After` waited out up to two seconds.
pub fn fast() -> RetryPolicy {
    RetryPolicy {
        retries: 2,
        base: Duration::from_millis(5),
        cap: Duration::from_millis(20),
        max_retry_after: Duration::from_secs(2),
    }
}

/// The real GitHub App client, with the test key, talking to `fake`.
pub fn app(fake: &FakeGithub, http: Http, clock: Arc<dyn Clock>) -> GitHubApp {
    let key = AppKey::from_pem(&Secret::new(test_key().private_pem.clone())).expect("key");
    GitHubApp::new("Iv23test", key, &fake.url(), http, clock)
}

/// A fake GitHub, the App installed on [`REPO`], and the scale set client of that repository.
pub struct Harness {
    /// The fake.
    pub fake: FakeGithub,
    /// The GitHub App client.
    pub app: GitHubApp,
    /// The scale set client of [`REPO`].
    pub client: ScaleSetClient,
    /// The clock both clients read.
    pub clock: Arc<TestClock>,
}

/// A harness whose requests give up after `timeout` and are tried again by `retry`.
pub async fn harness_with(options: Options, retry: RetryPolicy, timeout: Duration) -> Harness {
    let fake = FakeGithub::start(options).await;
    fake.add_repo(REPO, true);
    let clock = Arc::new(TestClock::default());
    let app = app(&fake, Http::new(timeout, retry), clock.clone());
    let client = ScaleSetClient::new(
        app.clone(),
        1,
        REPO,
        &format!("{}/{REPO}", fake.url()),
        clock.clone(),
        timeout,
    );
    Harness {
        fake,
        app,
        client,
        clock,
    }
}

/// A harness with [`fast`] retries and ten seconds per request.
pub async fn harness() -> Harness {
    harness_with(Options::default(), fast(), Duration::from_secs(10)).await
}

/// Owned labels from literals.
pub fn labels(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| s.to_string()).collect()
}

/// Waits (in real time) until `condition` holds, for at most eight seconds.
pub async fn until(what: &str, condition: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while !condition() {
        assert!(std::time::Instant::now() < deadline, "gave up waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Another server on this machine, which is not the fake GitHub: it answers 200 to everything and counts what
/// reaches it. Nothing with a token should.
pub async fn elsewhere() -> (Server, Arc<AtomicUsize>) {
    let visits = Arc::new(AtomicUsize::new(0));
    let count = visits.clone();
    let handler: Handler = Arc::new(move |_| {
        let count = count.clone();
        Box::pin(async move {
            count.fetch_add(1, Ordering::SeqCst);
            Reply::json(&serde_json::json!({"repositories": [], "value": [], "count": 0}))
        })
    });
    (Server::start(handler).await, visits)
}
