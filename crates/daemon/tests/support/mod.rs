//! The scaler against the fake GitHub, with real sockets, the real GitHub App client and the real scale set client.
//! Only the machine's slots and virtual machines are stand-ins.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use cf_daemon::testing::{EventLog, FakeHosts, FakeSlots, TestClock};
use cf_daemon::{Roster, Scaler, ScalerConfig, SetRegistry};
use cf_fake_github::{FakeGithub, Options, test_key};
use cf_ghapp::{AppKey, GitHubApp, Http, RetryPolicy, http_client};
use cf_scaleset::{Message, MessageSession, ScaleSet, ScaleSetClient, Statistics};
use cf_secret::Secret;

pub const REPO: &str = "org/app";
pub const MACHINE: &str = "macmini";

pub type TestScaler = Scaler<MessageSession, SetRegistry, FakeHosts>;

pub struct Harness {
    pub fake: FakeGithub,
    pub client: ScaleSetClient,
    pub clock: Arc<TestClock>,
    pub set: ScaleSet,
    pub session: MessageSession,
    pub slots: Arc<FakeSlots>,
    pub hosts: FakeHosts,
    pub roster: Arc<Roster>,
    pub events: Arc<EventLog>,
    pub scaler: TestScaler,
}

fn fast() -> RetryPolicy {
    RetryPolicy {
        retries: 2,
        base: Duration::from_millis(5),
        cap: Duration::from_millis(20),
        max_retry_after: Duration::from_secs(2),
    }
}

/// A machine with room for `gb` runners of 1 GB, serving one scale set `cf-1g`.
pub async fn harness(gb: u32) -> Harness {
    harness_with(gb, Options::default()).await
}

pub async fn harness_with(gb: u32, options: Options) -> Harness {
    let fake = FakeGithub::start(options).await;
    fake.add_repo(REPO, true);
    let clock = Arc::new(TestClock::default());
    let key = AppKey::from_pem(&Secret::new(test_key().private_pem.clone())).expect("key");
    let timeout = Duration::from_secs(10);
    let app = GitHubApp::new(
        "Iv23test",
        key,
        &fake.url(),
        Http::new(http_client(timeout), fast()),
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
    let labels = vec!["self-hosted".to_string(), "cf-1g".to_string()];
    let set = client
        .ensure_scale_set("macmini--cf-1g", &labels)
        .await
        .expect("scale set");
    let session = client.open_session(set.id, MACHINE).await.expect("session");
    let slots = FakeSlots::new(clock.clone(), gb, Duration::from_secs(60));
    let hosts = FakeHosts::default();
    let roster = Arc::new(Roster::new());
    let events = Arc::new(EventLog::default());
    let scaler = Scaler::new(
        ScalerConfig {
            scale_set_id: set.id,
            repo: REPO.into(),
            machine: MACHINE.into(),
            size_gb: 1,
        },
        session.clone(),
        SetRegistry {
            client: client.clone(),
            scale_set_id: set.id,
        },
        hosts.clone(),
        slots.clone(),
        roster.clone(),
        clock.clone(),
    )
    .with_events(events.clone());
    Harness {
        fake,
        client,
        clock,
        set,
        session,
        slots,
        hosts,
        roster,
        events,
        scaler,
    }
}

impl Harness {
    pub fn clock_now(&self) -> std::time::SystemTime {
        use cf_ghapp::Clock;
        self.clock.now()
    }
}

/// A message that carries only the scale set's counts, like the first one every session starts with.
pub fn stats(assigned: i64) -> Message {
    Message::initial(Statistics {
        total_assigned_jobs: assigned,
        ..Statistics::default()
    })
}

/// The next message the service has for the scale set, with room for `capacity` runners.
pub async fn next_message(h: &Harness, capacity: usize) -> Message {
    h.session
        .poll(0, capacity)
        .await
        .expect("poll")
        .expect("a message")
}

/// The next `n` messages merged into one, as a batch the service sent together would be.
pub async fn next_messages(h: &Harness, n: usize, capacity: usize) -> Message {
    let mut merged = Message::default();
    for _ in 0..n {
        let m = h
            .session
            .poll(merged.id, capacity)
            .await
            .expect("poll")
            .expect("a message");
        merged.id = m.id;
        merged.statistics = m.statistics;
        merged.available.extend(m.available);
        merged.assigned.extend(m.assigned);
        merged.started.extend(m.started);
        merged.completed.extend(m.completed);
    }
    merged
}
