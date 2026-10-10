//! The scaler against the fake GitHub, with real sockets, the real GitHub App client and the real scale set client.
//! Only the machine's slots and virtual machines are stand-ins.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use cf_daemon::testing::{EventLog, FakeHosts, FakeSlots};
use cf_daemon::{Roster, Scaler, ScalerConfig, SetRegistry};
pub use cf_fake_github::harness::{REPO, TestClock};
use cf_fake_github::harness::{fast, harness_with as github};
use cf_fake_github::{FakeGithub, Options};
use cf_scaleset::{Message, MessageSession, ScaleSet, ScaleSetClient, Statistics};

pub const MACHINE: &str = "mac-b";

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

/// A machine with room for `gb` Actions runners of 1 GB, serving one scale set `cf-1g`.
pub async fn harness(gb: u32) -> Harness {
    harness_with(gb, Options::default()).await
}

pub async fn harness_with(gb: u32, options: Options) -> Harness {
    let cf_fake_github::harness::Harness {
        fake, client, clock, ..
    } = github(options, fast(), Duration::from_secs(10)).await;
    let labels = vec!["self-hosted".to_string(), "cf-1g".to_string()];
    let set = client
        .ensure_scale_set("mac-b--cf-1g", &labels)
        .await
        .expect("scale set");
    let session = client.open_session(set.id, MACHINE).await.expect("session");
    // The budget's own promises outlast the scaler's, so what a test sees go back, the scaler gave back.
    let slots = FakeSlots::new(clock.clone(), gb, Duration::from_secs(3600));
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

/// The next message the service has for the scale set, with room for `capacity` Actions runners.
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
