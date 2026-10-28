//! One Mac with its machines and GitHub's list of its Actions runners, without a GitHub: for tests of the machines'
//! side alone.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use cf_budget::Size;
use cf_daemon::names::job_name;
use cf_daemon::testing::{FakeVms, ReportLog, TestMachines, TestParts, pass, settings_for};
use cf_daemon::{
    Answer, Claim, EndKind, EventKind, Events, HostError, Hosts, JobEvent, JobReport, MemoryStore,
    RESULT_WAIT, Roster, RunnerSpec, Seen, Slots, Store, Ticket, Vms,
};
use cf_fake_github::harness::TestClock;
use cf_secret::Secret;
use time::OffsetDateTime;

pub const SIZE: Size = Size::G2;
pub const REPO: &str = "org/app";
pub const SET: i64 = 7;

/// The Mac mini of 2026-10-13: 16 GB and 8 cores, given to CI. A cap of 12 GiB, a floor of 2 GiB, 6 vCPUs: a cf-2g
/// machine takes 2 of them, a cf-4g 3, a cf-6g or cf-8g 4.
pub fn mini() -> cf_budget::Settings {
    cf_budget::Settings::for_host(
        cf_budget::HostFacts {
            total_bytes: 16 * cf_budget::GIB,
            cores: 8,
        },
        cf_budget::Mode::Dedicated,
    )
}

/// The peak of the job on the mini's first cf-4g machine, and what the Mac held for that machine.
pub const MINI_PEAK: u64 = 1577 * cf_budget::MIB;
pub const MINI_HELD: u64 = 4109 * cf_budget::MIB;

/// The mini with its first machine running, a cf-4g one, its job's peak and the vm-host's reading of the Mac as given.
pub async fn mini_with_one(peak: u64, held: Option<u64>) -> (World, String) {
    let mut w = World::on(mini());
    let runner = w.running_sized(Size::G4).await;
    w.parts.vms.usage(job_name(&runner), peak, 0);
    w.parts.vms.resident(job_name(&runner), held);
    pass(&w.m, 1);
    (w, runner)
}

/// One Mac with its machines, and GitHub's list of the Actions runners registered from it.
pub struct World {
    pub parts: TestParts,
    /// The wall clock, `parts.clock`.
    pub clock: Arc<TestClock>,
    pub m: Arc<TestMachines>,
    /// The Actions runners GitHub has registered.
    pub registered: BTreeSet<String>,
    next: u32,
}

impl World {
    /// How often the listeners' bells have rung.
    pub fn nudges(&self) -> usize {
        self.parts.nudges.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// A Mac with room for `machines` machines of the test's size.
    pub fn new(machines: u32) -> World {
        World::on(settings_for(machines))
    }

    /// A Mac with these numbers.
    pub fn on(settings: cf_budget::Settings) -> World {
        World::keeping(settings, Arc::new(MemoryStore::default()))
    }

    /// A Mac with these numbers whose records are kept there.
    pub fn keeping(settings: cf_budget::Settings, store: Arc<dyn Store>) -> World {
        let clock = Arc::new(TestClock::default());
        let parts = TestParts {
            vms: FakeVms::default(),
            store,
            roster: Arc::new(Roster::new()),
            clock: clock.clone(),
            reports: Arc::new(ReportLog::default()),
            settings,
            nudges: Arc::new(AtomicUsize::new(0)),
            boot: "start-1".into(),
            bell: Arc::default(),
        };
        let m = parts.machines(None);
        World {
            parts,
            clock,
            m,
            registered: BTreeSet::new(),
            next: 0,
        }
    }

    /// The daemon dies and starts again: the Mac's clock goes on, the records and the vm-hosts are what they were.
    pub fn restart(&mut self) {
        let host = self.m.with_host(|host| host.clone());
        self.parts.roster = Arc::new(Roster::new());
        self.m = self.parts.restored(host);
    }

    /// The Mac itself starts again: no process of before is left.
    pub fn reboot(&mut self) {
        for job in self.parts.vms.standing() {
            self.parts.vms.die(&job);
        }
        self.parts.boot.push_str("-and-one-more");
        self.restart();
    }

    pub fn name(&mut self) -> String {
        self.next += 1;
        format!("mac-b-j{:07}", self.next)
    }

    /// A job is assigned and there is room: a slot, a registration at GitHub, a machine.
    pub async fn start(&mut self) -> String {
        self.start_sized(SIZE).await
    }

    /// [`World::start`] with a machine of this size.
    pub async fn start_sized(&mut self, size: Size) -> String {
        let Claim::Start(ticket) = self.m.claim(size, None) else {
            panic!("no room for the job");
        };
        self.start_as(ticket, size).await.expect("the machine starts")
    }

    pub async fn start_on(&mut self, ticket: Ticket) -> Result<String, HostError> {
        self.start_as(ticket, SIZE).await
    }

    /// A machine of this size on a slot.
    pub async fn start_as(&mut self, ticket: Ticket, size: Size) -> Result<String, HostError> {
        let runner = self.register();
        self.start_registered(&runner, ticket, size).await.map(|_| runner)
    }

    /// A new Actions runner's intent is written, and GitHub registers it.
    pub fn register(&mut self) -> String {
        let runner = self.name();
        self.m.registering(&runner, REPO, SET);
        self.registered.insert(runner.clone());
        runner
    }

    /// The machine of an Actions runner [`World::register`] registered, on a slot.
    pub async fn start_registered(
        &mut self,
        runner: &str,
        ticket: Ticket,
        size: Size,
    ) -> Result<(), HostError> {
        self.m
            .start(RunnerSpec {
                name: runner.to_string(),
                repo: REPO.into(),
                scale_set_id: SET,
                runner_id: i64::from(self.next),
                size,
                ticket,
                jit_config: Secret::new("jit".to_string()),
            })
            .await
            .map(|_| ())
    }

    /// Whether this Actions runner's guest is up.
    pub fn up(&self, runner: &str) -> bool {
        matches!(self.parts.vms.look(job_name(runner)), Seen::Alive(state) if state.boot_ms.is_some())
    }

    /// GitHub says a job started on the Actions runner, or completed with this result.
    pub fn github(&self, kind: EventKind, runner: &str, result: &str) {
        self.m.job(&JobEvent {
            kind,
            runner: runner.into(),
            repo: REPO.into(),
            job: "build".into(),
            workflow: "ci.yml".into(),
            run_id: 42,
            event: "push".into(),
            queued_at: Some(OffsetDateTime::UNIX_EPOCH),
            started_at: Some(OffsetDateTime::UNIX_EPOCH),
            at: OffsetDateTime::UNIX_EPOCH,
            result: result.into(),
            out_of_memory: false,
        });
    }

    /// A job started on a new machine.
    pub async fn running(&mut self) -> String {
        self.running_sized(SIZE).await
    }

    /// [`World::running`] on a machine of this size.
    pub async fn running_sized(&mut self, size: Size) -> String {
        let runner = self.start_sized(size).await;
        self.github(EventKind::Started, &runner, "");
        runner
    }

    /// What the service does every few seconds: GitHub removes the registrations the machines name.
    pub fn sweep(&mut self) {
        for registration in self.m.registrations_to_remove() {
            // GitHub answers a removal of what it has; one gone already is a 404.
            let answer = if self.registered.remove(&registration.runner) {
                Answer::Removed
            } else {
                Answer::Gone
            };
            self.m.answered(&registration.runner, answer);
        }
    }

    /// The scaler finds this idle machine surplus.
    pub fn retire(&self, runner: &str) {
        self.m.retire(REPO, SET, 1, &[runner.to_string()]);
    }

    pub fn reports(&self) -> Vec<JobReport> {
        self.parts.reports.all()
    }

    /// The end kinds reported, by Actions runner.
    pub fn ends(&self) -> Vec<(String, EndKind)> {
        self.reports()
            .into_iter()
            .filter_map(|r| r.end.map(|end| (r.runner, end)))
            .collect()
    }

    pub fn slots(&self) -> usize {
        self.m.snapshot().budget.slots.len()
    }

    /// Nothing is left of any job: no machine, no file, no memory held, no registration, no record. A record is
    /// kept for as long as GitHub's word of a job may still come.
    pub fn assert_nothing_left(&mut self) {
        self.sweep();
        pass(&self.m, RESULT_WAIT.as_secs() + 2);
        self.sweep();
        assert_eq!(
            self.parts.vms.standing(),
            Vec::<String>::new(),
            "a machine stands"
        );
        assert_eq!(self.parts.vms.files(), Vec::<String>::new(), "files are left");
        assert_eq!(self.slots(), 0, "memory is held");
        assert_eq!(self.registered, BTreeSet::new(), "registered at GitHub");
        assert_eq!(self.parts.store.load(), [], "records are left");
        assert_eq!(self.m.snapshot().machines, [], "a machine is listed");
    }
}

pub fn ids(reports: &[JobReport]) -> Vec<String> {
    reports.iter().map(|r| r.id.clone()).collect()
}
