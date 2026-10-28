//! For tests: the fake GitHub behind the fake console, with the Runner's real clients in front of them, as a machine
//! has them. The console holds the Runners App's key and the Actions service's admin token; the machine gets its
//! tokens and its scale sets through the console ([`ConsoleTokens`], [`ScaleSetClient`]). What every crate's tests
//! start from.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use cf_console_client::{Client, ConsoleTokens, Enroll, enroll};
use cf_fake_console::FakeConsole;
use cf_fake_github::httpd::{Handler, Reply, Server};
use cf_fake_github::{FakeGithub, Options};
use cf_ghapp::{Clock, GitHubApp, Http, Monotonic, RetryPolicy};
use cf_scaleset::ScaleSetClient;
use cf_secret::Secret;

/// The repository the harness installs the App on.
pub const REPO: &str = "org/app";
/// The machine the harness enrols with the console.
pub const MACHINE: &str = "mac-a";

/// The real time plus whatever a test moved it by, as the wall clock ([`Clock`]) and as the monotonic one
/// ([`Monotonic`]), so a token's lifetime and GitHub's expiry move together.
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

impl Monotonic for TestClock {
    fn now(&self) -> Instant {
        Instant::now() + *self.0.lock().expect("clock")
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

/// A machine enrolled with `console` as `machine`, with GitHub through the console turned on for it: its console
/// client.
pub async fn enrolled(console: &FakeConsole, machine: &str, http: Http) -> Client {
    let code = format!("code-{machine}");
    console.add_code_for(&code, machine);
    let hello = Enroll {
        code: Secret::new(code),
        machine: machine.to_string(),
        hostname: format!("{machine}.local"),
        model: "Mac16,10".into(),
        version: "0.1.0".into(),
        cpus: 10,
        memory_bytes: 32 << 30,
        os: None,
    };
    let token = enroll(&http.without_retries(), &console.url(), &hello)
        .await
        .expect("enrol")
        .token;
    assert!(console.set_broker(machine, true), "the machine is enrolled");
    Client::new(&console.url(), token, http)
}

/// The real GitHub App client of the machine `console` serves as `machine`, its tokens from the console.
pub fn app(
    fake: &FakeGithub,
    console: &Client,
    machine: &str,
    http: Http,
    clock: Arc<TestClock>,
) -> GitHubApp {
    app_on(fake, console, machine, http, clock.clone(), clock)
}

/// [`app`] on a wall clock and a monotonic one of the caller's.
pub fn app_on(
    fake: &FakeGithub,
    console: &Client,
    machine: &str,
    http: Http,
    clock: Arc<dyn Clock>,
    ticks: Arc<dyn Monotonic>,
) -> GitHubApp {
    let tokens = ConsoleTokens::new(console.clone(), machine, clock, ticks);
    GitHubApp::with_source(Arc::new(tokens), &fake.url(), http)
}

/// The monotonic clock that moves with a wall clock a test moves: now, plus however far `clock` is ahead of the real
/// time.
pub struct Ticks(pub Arc<dyn Clock>);

impl Monotonic for Ticks {
    fn now(&self) -> Instant {
        let ahead = self.0.now().duration_since(SystemTime::now()).unwrap_or_default();
        Instant::now() + ahead
    }
}

/// A fake GitHub with the App installed on [`REPO`], a fake console that holds the App and serves it, the machine
/// [`MACHINE`] enrolled there with GitHub through the console on, and the machine's clients.
pub struct Harness {
    /// The fake GitHub.
    pub fake: FakeGithub,
    /// The fake console.
    pub console: FakeConsole,
    /// The machine's console client.
    pub console_client: Client,
    /// The GitHub App client, its tokens from the console.
    pub app: GitHubApp,
    /// The scale set client of [`REPO`], through the console.
    pub client: ScaleSetClient,
    /// The clock the clients read.
    pub clock: Arc<TestClock>,
    /// The machine's name at the console.
    pub machine: String,
}

impl Harness {
    /// Another scale set client of `repo` for this machine, whose calls give up after `timeout`.
    pub fn scale_sets_of(&self, repo: &str, timeout: Duration) -> ScaleSetClient {
        ScaleSetClient::new(
            self.app.clone(),
            self.console_client.clone(),
            &self.machine,
            1,
            repo,
            timeout,
        )
    }

    /// Another scale set client of [`REPO`] for this machine, whose calls give up after `timeout`.
    pub fn scale_sets(&self, timeout: Duration) -> ScaleSetClient {
        self.scale_sets_of(REPO, timeout)
    }
}

/// A harness of the machine [`MACHINE`] whose requests give up after `timeout` and are tried again by `retry`.
pub async fn harness_with(options: Options, retry: RetryPolicy, timeout: Duration) -> Harness {
    harness_as(MACHINE, options, retry, timeout).await
}

/// A harness of the machine `machine`, whose requests give up after `timeout` and are tried again by `retry`.
pub async fn harness_as(machine: &str, options: Options, retry: RetryPolicy, timeout: Duration) -> Harness {
    let fake = FakeGithub::start(options).await;
    fake.add_repo(REPO, true);
    let console = FakeConsole::start().await;
    console.serve_github(fake.clone());
    let http = Http::new(timeout, retry);
    let console_client = enrolled(&console, machine, http.clone()).await;
    let clock = Arc::new(TestClock::default());
    let app = app(&fake, &console_client, machine, http, clock.clone());
    let client = ScaleSetClient::new(app.clone(), console_client.clone(), machine, 1, REPO, timeout);
    Harness {
        fake,
        console,
        console_client,
        app,
        client,
        clock,
        machine: machine.to_string(),
    }
}

/// A harness with [`fast`] retries and ten seconds per request.
pub async fn harness() -> Harness {
    harness_with(Options::default(), fast(), Duration::from_secs(10)).await
}

/// Whether any request that reached `fake` asked for a registration token or traded one for the Actions service's
/// admin token: none may, as the console makes both in-process and no machine holds them.
pub fn registrations(fake: &FakeGithub) -> usize {
    fake.count("/registration-token") + fake.count("/runner-registration")
}

/// Requests that reached `fake` with a still-good admin token of the Actions service and did not come from the
/// console: none may, as no machine holds one.
pub fn admin_requests_from_machines(fake: &FakeGithub) -> Vec<String> {
    fake.requests()
        .into_iter()
        .filter(|r| r.admin && !r.from_console)
        .map(|r| r.line)
        .collect()
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
