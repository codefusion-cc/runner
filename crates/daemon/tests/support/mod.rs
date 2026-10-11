//! The scaler against the fake GitHub, with real sockets, the real GitHub App client and the real scale set client,
//! on the real budget and lifecycle. Only the vm-hosts and the Mac's readings are stand-ins.
#![allow(dead_code)]

pub mod world;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cf_budget::Size;
use cf_daemon::names::job_name;
use cf_daemon::testing::{Both, EventLog, FakeVms, ReportLog, TestMachines, TestParts, pass, settings_for};
use cf_daemon::{
    Claim, HostError, Hosts, MemoryStore, Refusal, Roster, RunnerSpec, Scaler, ScalerConfig, SetRegistry,
    Slots, Ticket,
};
pub use cf_fake_github::harness::{REPO, TestClock};
use cf_fake_github::harness::{fast, harness_with as github};
use cf_fake_github::{FakeGithub, Options};
use cf_scaleset::{Message, MessageSession, ScaleSet, ScaleSetClient, Statistics};
use cf_vmhost::OutOfMemory;

pub const MACHINE: &str = "mac-b";

/// The size every test's scale set has.
pub const SIZE: Size = Size::G2;

pub type TestScaler = Scaler<MessageSession, SetRegistry, Spy>;

#[derive(Default)]
struct Seen {
    released: Vec<Ticket>,
    withdrawn: Vec<Ticket>,
    claimed: usize,
    started: Vec<(String, i64, Ticket)>,
    stopped: Vec<String>,
    fail_stop: usize,
    unanswered: bool,
}

/// The real machines, with what the scaler asked of them written down.
#[derive(Clone)]
pub struct Spy {
    pub machines: Arc<TestMachines>,
    seen: Arc<Mutex<Seen>>,
}

impl Spy {
    pub fn new(machines: Arc<TestMachines>) -> Spy {
        Spy {
            machines,
            seen: Arc::default(),
        }
    }

    fn seen(&self) -> std::sync::MutexGuard<'_, Seen> {
        self.seen.lock().expect("spy")
    }

    /// The slots the budget holds: promises and machines.
    pub fn held(&self) -> Vec<Ticket> {
        self.machines
            .snapshot()
            .budget
            .slots
            .iter()
            .map(|held| held.slot)
            .collect()
    }

    /// How many slots the budget holds.
    pub fn used(&self) -> usize {
        self.held().len()
    }

    /// The slots the scaler gave back.
    pub fn released(&self) -> Vec<Ticket> {
        self.seen().released.clone()
    }

    /// The places in line the scaler gave back.
    pub fn withdrawn(&self) -> Vec<Ticket> {
        self.seen().withdrawn.clone()
    }

    /// How often the scaler got a slot to start in.
    pub fn claimed(&self) -> usize {
        self.seen().claimed
    }

    /// Closes the machine to new jobs, or opens it.
    pub fn set_open(&self, open: bool) {
        self.machines.pause(!open);
    }

    /// The Actions runners a machine was started for: name, registration, slot.
    pub fn started(&self) -> Vec<(String, i64, Ticket)> {
        self.seen().started.clone()
    }

    /// The Actions runners whose machine was asked to stop.
    pub fn stopped(&self) -> Vec<String> {
        self.seen().stopped.clone()
    }

    pub fn fail_start(&self, n: usize) {
        self.machines.vms().fail_start(n);
    }

    pub fn fail_stop(&self, n: usize) {
        self.seen().fail_stop = n;
    }

    pub fn mark_out_of_memory(&self, runner: &str) {
        self.machines.vms().out_of_memory(job_name(runner));
    }

    pub fn leave_out_of_memory_unanswered(&self) {
        self.seen().unanswered = true;
    }

    /// The Actions runner's machine ends by itself and the machines' side sees it go.
    pub fn gone(&self, runner: &str) {
        self.machines.vms().end(job_name(runner), cf_vmhost::End::Ok);
        // One look to see it gone and give its slot back, one for the budget to hand the room on.
        pass(&self.machines, 2);
    }
}

impl Slots for Spy {
    fn reserve(&self, size: Size) -> Result<Ticket, Refusal> {
        self.machines.reserve(size)
    }

    fn release(&self, ticket: Ticket) {
        self.seen().released.push(ticket);
        self.machines.release(ticket);
    }

    fn claim(&self, size: Size, promised: Option<Ticket>) -> Claim {
        let claim = self.machines.claim(size, promised);
        if matches!(claim, Claim::Start(_)) {
            self.seen().claimed += 1;
        }
        claim
    }

    fn ready(&self, ticket: Ticket) -> bool {
        let ready = self.machines.ready(ticket);
        if ready {
            self.seen().claimed += 1;
        }
        ready
    }

    fn withdraw(&self, ticket: Ticket) {
        self.seen().withdrawn.push(ticket);
        self.machines.withdraw(ticket);
    }

    fn open_slots(&self, size: Size) -> usize {
        self.machines.open_slots(size)
    }

    fn state(&self) -> (bool, String) {
        self.machines.state()
    }
}

impl Hosts for Spy {
    fn registering(&self, name: &str, repo: &str, scale_set_id: i64) {
        self.machines.registering(name, repo, scale_set_id);
    }

    fn not_registered(&self, name: &str) {
        self.machines.not_registered(name);
    }

    fn registration_unsure(&self, name: &str) {
        self.machines.registration_unsure(name);
    }

    async fn start(&self, spec: RunnerSpec) -> Result<String, HostError> {
        let seen = (spec.name.clone(), spec.runner_id, spec.ticket);
        let handle = self.machines.start(spec).await?;
        self.seen().started.push(seen);
        Ok(handle)
    }

    async fn stop(&self, handle: &str) -> Result<(), HostError> {
        let name = self
            .seen()
            .started
            .iter()
            .find(|(name, _, _)| job_name(name) == handle)
            .map_or(handle.to_string(), |(name, _, _)| name.clone());
        {
            let mut seen = self.seen();
            if seen.fail_stop > 0 {
                seen.fail_stop -= 1;
                return Err(HostError("the machine did not stop".into()));
            }
            seen.stopped.push(name);
        }
        self.machines.stop(handle).await
    }

    async fn out_of_memory(&self, handle: &str) -> OutOfMemory {
        if self.seen().unanswered {
            return OutOfMemory::Unknown {
                reason: "the vm-host does not answer".into(),
            };
        }
        self.machines.out_of_memory(handle).await
    }
}

pub struct Harness {
    pub fake: FakeGithub,
    pub client: ScaleSetClient,
    pub clock: Arc<TestClock>,
    pub set: ScaleSet,
    pub session: MessageSession,
    /// The machines as the scaler's slots.
    pub slots: Spy,
    /// The same machines as the scaler's hosts.
    pub hosts: Spy,
    pub machines: Arc<TestMachines>,
    pub parts: TestParts,
    pub roster: Arc<Roster>,
    pub events: Arc<EventLog>,
    pub scaler: TestScaler,
}

/// A machine with room for `machines` Actions runners of the smallest size, serving one scale set `cf-2g`.
pub async fn harness(machines: u32) -> Harness {
    harness_with(machines, Options::default()).await
}

pub async fn harness_with(machines: u32, options: Options) -> Harness {
    harness_on(settings_for(machines), options).await
}

/// A machine with these budget settings.
pub async fn harness_on(settings: cf_budget::Settings, options: Options) -> Harness {
    let cf_fake_github::harness::Harness {
        fake, client, clock, ..
    } = github(options, fast(), Duration::from_secs(10)).await;
    let labels = vec!["self-hosted".to_string(), SIZE.label().to_string()];
    let set = client
        .ensure_scale_set("mac-b--cf-2g", &labels)
        .await
        .expect("scale set");
    let session = client.open_session(set.id, MACHINE).await.expect("session");
    let roster = Arc::new(Roster::new());
    let parts = TestParts {
        vms: FakeVms::default(),
        store: Arc::new(MemoryStore::default()),
        roster: roster.clone(),
        clock: clock.clone(),
        reports: Arc::new(ReportLog::default()),
        settings,
        nudges: Arc::new(AtomicUsize::new(0)),
        boot: 1,
        bell: Arc::default(),
    };
    let machines = parts.machines(None);
    parts.nudges.store(0, Ordering::Relaxed);
    let spy = Spy::new(machines.clone());
    let events = Arc::new(EventLog::default());
    let scaler = scaler_of(&Parts {
        client: &client,
        set: &set,
        session: &session,
        slots: &spy,
        hosts: &spy,
        roster: &roster,
        clock: &clock,
        events: &events,
    });
    Harness {
        fake,
        client,
        clock,
        set,
        session,
        slots: spy.clone(),
        hosts: spy,
        machines,
        parts,
        roster,
        events,
        scaler,
    }
}

/// What a scaler is made of. A test that needs a second scaler for the same scale set, as the daemon has after it
/// started again, names the harness's own parts here.
pub struct Parts<'a> {
    pub client: &'a ScaleSetClient,
    pub set: &'a ScaleSet,
    pub session: &'a MessageSession,
    pub slots: &'a Spy,
    pub hosts: &'a Spy,
    pub roster: &'a Arc<Roster>,
    pub clock: &'a Arc<TestClock>,
    pub events: &'a Arc<EventLog>,
}

pub fn scaler_of(parts: &Parts<'_>) -> TestScaler {
    Scaler::new(
        ScalerConfig {
            scale_set_id: parts.set.id,
            repo: REPO.into(),
            machine: MACHINE.into(),
            size: SIZE,
        },
        parts.session.clone(),
        SetRegistry {
            client: parts.client.clone(),
            scale_set_id: parts.set.id,
        },
        parts.hosts.clone(),
        Arc::new(parts.slots.clone()),
        parts.roster.clone(),
        parts.clock.clone(),
    )
    .with_events(Arc::new(Both(parts.events.clone(), parts.slots.machines.clone())))
    .with_out_of_memory_check({
        let machines = parts.slots.machines.clone();
        Arc::new(move |runner| machines.was_out_of_memory(runner))
    })
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
