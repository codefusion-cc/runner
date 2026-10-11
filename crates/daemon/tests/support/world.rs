//! One Mac with its machines and GitHub's list of its Actions runners, without a GitHub: for tests of the machines'
//! side alone.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use cf_budget::Size;
use cf_daemon::testing::{FakeVms, ReportLog, TestMachines, TestParts, pass, settings_for};
use cf_daemon::{
    Claim, EndKind, EventKind, Events, HostError, Hosts, JobEvent, JobReport, MemoryStore, RESULT_WAIT,
    Roster, RunnerSpec, Slots, Ticket,
};
use cf_fake_github::harness::TestClock;
use cf_secret::Secret;
use time::OffsetDateTime;

pub const SIZE: Size = Size::G2;
pub const REPO: &str = "org/app";
pub const SET: i64 = 7;

/// One Mac with its machines, and GitHub's list of the Actions runners registered from it.
pub struct World {
    pub parts: TestParts,
    pub m: Arc<TestMachines>,
    /// The Actions runners GitHub has registered.
    pub registered: BTreeSet<String>,
    next: u32,
}

impl World {
    /// A Mac with room for `machines` machines of the test's size.
    pub fn new(machines: u32) -> World {
        World::on(settings_for(machines))
    }

    /// A Mac with these numbers.
    pub fn on(settings: cf_budget::Settings) -> World {
        let parts = TestParts {
            vms: FakeVms::default(),
            store: Arc::new(MemoryStore::default()),
            roster: Arc::new(Roster::new()),
            clock: Arc::new(TestClock::default()),
            reports: Arc::new(ReportLog::default()),
            settings,
            nudges: Arc::new(AtomicUsize::new(0)),
            boot: 1,
            bell: Arc::default(),
        };
        let m = parts.machines(None);
        World {
            parts,
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
        self.parts.boot += 1;
        self.restart();
    }

    pub fn name(&mut self) -> String {
        self.next += 1;
        format!("mac-b-j{:07}", self.next)
    }

    /// A job is assigned and there is room: a slot, a registration at GitHub, a machine.
    pub async fn start(&mut self) -> String {
        let Claim::Start(ticket) = self.m.claim(SIZE, None) else {
            panic!("no room for the job");
        };
        self.start_on(ticket).await.expect("the machine starts")
    }

    pub async fn start_on(&mut self, ticket: Ticket) -> Result<String, HostError> {
        self.start_as(ticket, SIZE).await
    }

    /// A machine of this size on a slot.
    pub async fn start_as(&mut self, ticket: Ticket, size: Size) -> Result<String, HostError> {
        let runner = self.name();
        self.m.registering(&runner, REPO, SET);
        self.registered.insert(runner.clone());
        let id = i64::from(self.next);
        self.m
            .start(RunnerSpec {
                name: runner.clone(),
                repo: REPO.into(),
                scale_set_id: SET,
                runner_id: id,
                size,
                ticket,
                jit_config: Secret::new("jit".to_string()),
            })
            .await
            .map(|_| runner)
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
        let runner = self.start().await;
        self.github(EventKind::Started, &runner, "");
        runner
    }

    /// What the service does every few seconds: GitHub removes the registrations the machines name.
    pub fn sweep(&mut self) {
        for registration in self.m.registrations_to_remove() {
            self.registered.remove(&registration.runner);
            self.m.unregistered(&registration.runner);
        }
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
