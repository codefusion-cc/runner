//! The scaler against the fake GitHub behind the fake console, with real sockets, the real GitHub App client and the
//! real scale set client, on the real budget and lifecycle. Only the vm-hosts and the Mac's readings are stand-ins.
#![allow(dead_code)]

pub mod world;

/// The `i`th of these, round and round: how a generated number picks among what is there; `None` when nothing is.
pub fn pick<T: Clone>(from: &[T], i: u8) -> Option<T> {
    (!from.is_empty()).then(|| from[usize::from(i) % from.len()].clone())
}

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cf_budget::{Size, Turn};
use cf_daemon::names::job_name;
use cf_daemon::testing::{Both, EventLog, FakeVms, ReportLog, TestMachines, TestParts, pass, settings_for};
use cf_daemon::{
    Claim, HostError, Hosts, MemoryStore, Refusal, Roster, RunnerSpec, Scaler, ScalerConfig, SetRegistry,
    Slots, Ticket,
};
use cf_fake_github::{FakeGithub, Options};
pub use cf_harness::{REPO, TestClock};
use cf_harness::{fast, harness_as as github};
use cf_scaleset::{Message, MessageSession, ScaleSet, ScaleSetClient, Statistics};
use cf_vmhost::OutOfMemory;

pub const MACHINE: &str = "mac-b";

/// The log of each thread that keeps one, behind the one subscriber of the test's process. A subscriber of one
/// thread alone would miss lines: `tracing` decides once for all threads whether a line is of interest, and may
/// decide it on a thread that has no log.
#[derive(Default)]
struct Logs(Mutex<Vec<(std::thread::ThreadId, Arc<cf_daemon::Lines>)>>);

static LOGS: std::sync::LazyLock<Arc<Logs>> = std::sync::LazyLock::new(|| {
    let logs = Arc::new(Logs::default());
    tracing::subscriber::set_global_default(logs.clone()).expect("the only subscriber of the tests");
    logs
});

impl tracing::Subscriber for Logs {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() <= tracing::Level::INFO
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let me = std::thread::current().id();
        let lines = self
            .0
            .lock()
            .expect("logs")
            .iter()
            .find(|(thread, _)| *thread == me)
            .map(|(_, lines)| lines.clone());
        if let Some(lines) = lines {
            lines.event(event);
        }
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

/// What this thread writes to the Runner's log for as long as this lives, in the log file's own form. For a test
/// that runs on one thread.
pub struct Log {
    folder: tempfile::TempDir,
}

impl Log {
    pub fn start() -> Log {
        let folder = tempfile::tempdir().expect("folder");
        let lines = cf_daemon::Lines::new(&folder.path().join("runner.log"), cf_daemon::ROLL_AT);
        LOGS.0
            .lock()
            .expect("logs")
            .push((std::thread::current().id(), Arc::new(lines)));
        Log { folder }
    }

    /// The lines so far that have `words` in them, each from its message on.
    pub fn lines(&self, words: &str) -> Vec<String> {
        std::fs::read_to_string(self.folder.path().join("runner.log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains(words))
            .map(|line| line.splitn(5, ' ').last().unwrap_or_default().to_string())
            .collect()
    }
}

impl Drop for Log {
    fn drop(&mut self) {
        let me = std::thread::current().id();
        LOGS.0.lock().expect("logs").retain(|(thread, _)| *thread != me);
    }
}

/// The size every test's scale set has.
pub const SIZE: Size = Size::G2;

pub type TestScaler = Scaler<MessageSession, SetRegistry, Spy>;

#[derive(Default)]
struct Seen {
    released: Vec<Ticket>,
    withdrawn: Vec<Ticket>,
    claimed: usize,
    started: Vec<(String, i64, Ticket)>,
    /// The repository of each of those.
    started_repos: Vec<String>,
    retired: Vec<String>,
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

    /// The repository of each Actions runner a machine was started for, in the order they started.
    pub fn started_repos(&self) -> Vec<String> {
        self.seen().started_repos.clone()
    }

    /// The Actions runners the scaler retired as surplus.
    pub fn retired(&self) -> Vec<String> {
        self.seen().retired.clone()
    }

    pub fn fail_start(&self, n: usize) {
        self.machines.vms().fail_start(n);
    }

    pub fn mark_out_of_memory(&self, runner: &str) {
        self.machines.vms().out_of_memory(job_name(runner));
    }

    pub fn leave_out_of_memory_unanswered(&self) {
        self.seen().unanswered = true;
    }

    /// Every machine started so far comes up: its Actions runner waits for a job.
    pub fn boot_all(&self) {
        for (runner, ..) in self.started() {
            self.machines.vms().ready(job_name(&runner));
        }
        pass(&self.machines, 1);
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

    fn claim_in_turn(&self, size: Size, promised: Option<Ticket>, turn: Turn) -> Claim {
        let claim = self.machines.claim_in_turn(size, promised, turn);
        if matches!(claim, Claim::Start(_)) {
            self.seen().claimed += 1;
        }
        claim
    }

    fn set_turn(&self, ticket: Ticket, turn: Turn) {
        self.machines.set_turn(ticket, turn);
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

    fn offer_room(&self, set: &cf_daemon::SetKey, size: Size) -> usize {
        self.machines.offer_room(set, size)
    }

    fn withdraw_offer(&self, set: &cf_daemon::SetKey) {
        self.machines.withdraw_offer(set);
    }

    fn offer_not_sent(&self, set: &cf_daemon::SetKey) {
        self.machines.offer_not_sent(set);
    }

    fn state(&self) -> (bool, String) {
        self.machines.state()
    }

    fn held(&self) -> bool {
        self.machines.held()
    }

    fn reserve_docker(&self, size: Size) -> Result<Ticket, Refusal> {
        self.machines.reserve_docker(size)
    }

    fn mark_docker(&self, ticket: Ticket) {
        self.machines.mark_docker(ticket);
    }

    fn docker_room(&self) -> usize {
        self.machines.docker_room()
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

    fn retire(&self, repo: &str, scale_set_id: i64, n: usize, idle: &[String]) {
        // A machine that asks for its removal names its registration to the sweep.
        let asking = || -> Vec<String> {
            let names = self.machines.registrations_to_remove().into_iter();
            names.map(|registration| registration.runner).collect()
        };
        let before = asking();
        self.machines.retire(repo, scale_set_id, n, idle);
        let asked = asking().into_iter().filter(|runner| !before.contains(runner));
        self.seen().retired.extend(asked);
    }

    async fn start(&self, spec: RunnerSpec) -> Result<String, HostError> {
        let seen = (spec.name.clone(), spec.runner_id, spec.ticket);
        let repo = spec.repo.clone();
        let handle = self.machines.start(spec).await?;
        let mut spied = self.seen();
        spied.started.push(seen);
        spied.started_repos.push(repo);
        Ok(handle)
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
    /// The fake console, which holds the App and asks GitHub for the machine.
    pub console: cf_fake_console::FakeConsole,
    /// The machine's console client.
    pub console_client: cf_console_client::Client,
    pub app: cf_ghapp::GitHubApp,
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
    let cf_harness::Harness {
        fake,
        console,
        console_client,
        app,
        client,
        clock,
        ..
    } = github(MACHINE, options, fast(), Duration::from_secs(10)).await;
    let labels = vec!["self-hosted".to_string(), SIZE.label().to_string()];
    let set = client
        .ensure_scale_set("mac-b--cf-2g", &labels)
        .await
        .expect("scale set");
    let session = client.open_session(set.id).await.expect("session");
    let roster = Arc::new(Roster::new());
    let parts = TestParts {
        vms: FakeVms::default(),
        store: Arc::new(MemoryStore::default()),
        roster: roster.clone(),
        clock: clock.clone(),
        reports: Arc::new(ReportLog::default()),
        settings,
        nudges: Arc::new(AtomicUsize::new(0)),
        boot: "start-1".into(),
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
        console,
        console_client,
        app,
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
    scaler_for(parts, REPO)
}

/// A scaler of `repo`'s scale set on the parts' machines.
pub fn scaler_for(parts: &Parts<'_>, repo: &str) -> TestScaler {
    scaler_with(parts, repo, false)
}

/// A scaler of `repo`'s scale set on the parts' machines, whose jobs get Docker when `docker` says so.
pub fn scaler_with(parts: &Parts<'_>, repo: &str, docker: bool) -> TestScaler {
    Scaler::new(
        ScalerConfig {
            scale_set_id: parts.set.id,
            repo: repo.into(),
            machine: MACHINE.into(),
            size: SIZE,
            docker,
        },
        parts.session.clone(),
        SetRegistry::of_set(parts.client.clone(), parts.set.id),
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

    /// The scaler of another repository's `cf-2g` scale set at the fake, on this harness's machines.
    pub async fn another_repository(&self, repo: &str) -> TestScaler {
        self.fake.add_repo(repo, true);
        let client = ScaleSetClient::new(
            self.app.clone(),
            self.console_client.clone(),
            MACHINE,
            1,
            repo,
            Duration::from_secs(10),
        );
        let labels = vec!["self-hosted".to_string(), SIZE.label().to_string()];
        let set = client
            .ensure_scale_set(&format!("mac-b--cf-2g--{}", repo.replace('/', "-")), &labels)
            .await
            .expect("scale set");
        let session = client.open_session(set.id).await.expect("session");
        scaler_for(
            &Parts {
                client: &client,
                set: &set,
                session: &session,
                slots: &self.slots,
                hosts: &self.hosts,
                roster: &self.roster,
                clock: &self.clock,
                events: &self.events,
            },
            repo,
        )
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
