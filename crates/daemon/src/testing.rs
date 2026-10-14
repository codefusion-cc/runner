//! Stand-ins for the edges of the daemon: the vm-hosts, the Mac's readings, and where reports go. For tests.

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use cf_budget::FakeHost;
use cf_budget::{GIB, HostFacts, Millis, Mode, Settings, Size};
use cf_ghapp::Clock;
use cf_guest_proto::{CacheEnd, CacheFound, GuestEnd, OomEvent, Process, StopReason, Summary, Usage};
use cf_vmhost::{End, Ended, JobRequest, JobState, Stage};

use crate::machines::{JobReport, MachineConfig, MachineParts, Machines, Reports};
use crate::records::{MemoryStore, Record, Store};
use crate::roster::Roster;
use crate::traits::{Events, JobEvent};
use crate::vms::{Seen, VmError, Vms};

#[derive(Debug, Clone)]
struct FakeVm {
    state: JobState,
    /// Whether the vm-host's process is there, and the machine's memory with it.
    alive: bool,
    silent: bool,
    files: bool,
    unkillable: bool,
    /// What its guest says of a Docker cache disk at the end, when it has one.
    cache: Option<CacheEnd>,
}

#[derive(Debug, Default)]
struct VmState {
    vms: BTreeMap<String, FakeVm>,
    started: Vec<JobRequest>,
    stopped: Vec<(String, StopReason, Duration)>,
    killed: Vec<String>,
    cleaned: Vec<String>,
    /// Machines that went before they said they were up.
    died_booting: usize,
    fail_start: usize,
    slow_boot: bool,
    late_state: bool,
    ignore_stops: bool,
    /// The daemon that drives them died: nothing it asks of them is done.
    frozen: bool,
    next_pid: u32,
    held: Option<Arc<HeldLooks>>,
    /// What the guests of machines started from now on say of their cache disk; a fresh one let go cleanly unless a
    /// test says otherwise.
    cache_end: Option<CacheEnd>,
}

/// Looks at vm-hosts that wait, as a look at a vm-host that does not answer does, until a test lets them go.
#[derive(Debug, Default)]
pub struct HeldLooks {
    /// How many looks wait, and whether they may go on.
    state: Mutex<(usize, bool)>,
    moved: Condvar,
}

impl HeldLooks {
    fn pass(&self) {
        let mut state = self.state.lock().expect("looks");
        state.0 += 1;
        self.moved.notify_all();
        while !state.1 {
            state = self.moved.wait(state).expect("looks");
        }
    }

    /// Waits until a look waits; `false` when none came within `limit`.
    pub fn waiting(&self, limit: Duration) -> bool {
        let state = self.state.lock().expect("looks");
        let (state, _) = self
            .moved
            .wait_timeout_while(state, limit, |state| state.0 == 0)
            .expect("looks");
        state.0 > 0
    }

    /// Lets every look go on, those that wait and those to come.
    pub fn let_go(&self) {
        self.state.lock().expect("looks").1 = true;
        self.moved.notify_all();
    }
}

/// The vm-hosts of a test: machines that start, boot, end, die, hang and refuse as a test says. By default a
/// machine is ready as soon as it is started and ends at once when it is asked to stop.
#[derive(Debug, Clone, Default)]
pub struct FakeVms(Arc<Mutex<VmState>>);

impl FakeVms {
    fn lock(&self) -> std::sync::MutexGuard<'_, VmState> {
        self.0.lock().expect("vms")
    }

    fn with(&self, job: &str, change: impl FnOnce(&mut FakeVm)) {
        if let Some(vm) = self.lock().vms.get_mut(job) {
            change(vm);
        }
    }

    /// The next `n` starts fail.
    pub fn fail_start(&self, n: usize) {
        self.lock().fail_start = n;
    }

    /// Machines stay booting until [`FakeVms::ready`].
    pub fn slow_boot(&self, slow: bool) {
        self.lock().slow_boot = slow;
    }

    /// A vm-host that starts has written nothing yet, until [`FakeVms::written`].
    pub fn late_state(&self, late: bool) {
        self.lock().late_state = late;
    }

    /// The vm-host writes its first state.
    pub fn written(&self, job: &str) {
        self.with(job, |vm| vm.files = true);
    }

    /// Machines take a stop and go on.
    pub fn ignore_stops(&self, ignore: bool) {
        self.lock().ignore_stops = ignore;
    }

    /// The daemon died, and its hands with it: a start, stop, kill or cleanup it still asks for does nothing, until
    /// the daemon that starts next thaws them.
    pub fn freeze(&self, frozen: bool) {
        self.lock().frozen = frozen;
    }

    /// Every look at a vm-host waits from now on, until the test lets them go.
    pub fn hold_looks(&self) -> Arc<HeldLooks> {
        let held = Arc::new(HeldLooks::default());
        self.lock().held = Some(held.clone());
        held
    }

    /// The guest reports `ready` and the workload starts.
    pub fn ready(&self, job: &str) {
        self.with(job, |vm| {
            vm.state.stage = Stage::Running;
            vm.state.boot_ms = Some(1200);
        });
    }

    /// The job holds this much memory and disk now.
    pub fn usage(&self, job: &str, memory_bytes: u64, disk_bytes: u64) {
        self.with(job, |vm| {
            let peak = vm.state.usage.as_ref().map_or(0, |u| u.memory_peak_bytes);
            vm.state.usage = Some(Usage {
                memory_bytes,
                memory_peak_bytes: peak.max(memory_bytes),
                cpu_usec: 1_000_000,
                disk_bytes,
                pids: 3,
            });
        });
    }

    /// The job's machine holds this much of the Mac's memory now, as its vm-host read it (`None`: it could not).
    pub fn resident(&self, job: &str, bytes: Option<u64>) {
        self.with(job, |vm| vm.state.resident_bytes = bytes);
    }

    /// The job's vm-host is at this stage now.
    pub fn stage(&self, job: &str, stage: Stage) {
        self.with(job, |vm| vm.state.stage = stage);
    }

    /// The kernel stops a process of the job for memory.
    pub fn out_of_memory(&self, job: &str) {
        self.with(job, |vm| {
            vm.state.oom = Some(OomEvent {
                process: Process {
                    pid: 77,
                    name: "cc1plus".into(),
                    memory_bytes: 900 << 20,
                },
                limit_bytes: vm.state.limit_bytes,
                largest: vec![],
            });
        });
    }

    /// The vm-host writes this end and exits; the machine is gone.
    pub fn end(&self, job: &str, end: End) {
        self.with(job, |vm| finish(vm, end));
    }

    /// The vm-host is killed from outside: its state stays as it was, without an end.
    pub fn die(&self, job: &str) {
        let mut state = self.lock();
        if let Some(vm) = state.vms.get_mut(job) {
            let booting = vm.alive && vm.state.stage == Stage::Booting;
            vm.alive = false;
            state.died_booting += usize::from(booting);
        }
    }

    /// Nothing of the job is there, as when the daemon died before its vm-host existed.
    pub fn forget(&self, job: &str) {
        self.lock().vms.remove(job);
    }

    /// The vm-host stops answering.
    pub fn hang(&self, job: &str) {
        self.with(job, |vm| vm.silent = true);
    }

    /// The vm-host survives a SIGKILL.
    pub fn unkillable(&self, job: &str, unkillable: bool) {
        self.with(job, |vm| vm.unkillable = unkillable);
    }

    /// The jobs whose vm-host is there, holding its machine's memory.
    pub fn standing(&self) -> Vec<String> {
        self.lock()
            .vms
            .iter()
            .filter(|(_, vm)| vm.alive)
            .map(|(job, _)| job.clone())
            .collect()
    }

    /// The machines that stand and have not said they are up.
    pub fn booting(&self) -> Vec<String> {
        self.lock()
            .vms
            .iter()
            .filter(|(_, vm)| vm.alive && vm.state.stage != Stage::Running)
            .map(|(job, _)| job.clone())
            .collect()
    }

    /// How many machines went without ever saying they were up: killed or dead while they booted.
    pub fn died_booting(&self) -> usize {
        self.lock().died_booting
    }

    /// The memory of the machines that stand.
    pub fn standing_bytes(&self) -> u64 {
        self.lock()
            .vms
            .values()
            .filter(|vm| vm.alive)
            .map(|vm| vm.state.vm_bytes)
            .sum()
    }

    /// Every request a vm-host was started with, in order.
    pub fn started(&self) -> Vec<JobRequest> {
        self.lock().started.clone()
    }

    /// Every stop a vm-host was asked for.
    pub fn stopped(&self) -> Vec<(String, StopReason, Duration)> {
        self.lock().stopped.clone()
    }

    /// Every job a kill was sent for.
    pub fn killed(&self) -> Vec<String> {
        self.lock().killed.clone()
    }

    /// Every job whose files were removed.
    pub fn cleaned(&self) -> Vec<String> {
        self.lock().cleaned.clone()
    }

    /// The jobs that have files in the state folder.
    pub fn files(&self) -> Vec<String> {
        self.jobs()
    }

    /// What the guests of machines started from now on say of their Docker cache disk at the end.
    pub fn cache_end(&self, end: CacheEnd) {
        self.lock().cache_end = Some(end);
    }
}

fn finish(vm: &mut FakeVm, end: End) {
    let usage = vm.state.usage.unwrap_or(Usage {
        memory_bytes: 0,
        memory_peak_bytes: 300 << 20,
        cpu_usec: 2_000_000,
        disk_bytes: 0,
        pids: 0,
    });
    let heard = !matches!(end, End::GuestLost | End::BootFailed);
    vm.state.end = Some(Ended {
        end,
        message: None,
        summary: heard.then(|| Summary {
            end: GuestEnd::Exited,
            exit_code: Some(i32::from(end == End::Failed)),
            signal: None,
            usage,
            disk_peak_bytes: 5 << 20,
            oom: vm.state.oom.clone(),
            wall_ms: 4200,
            docker_cache: vm.cache,
        }),
        at: 0,
    });
    vm.state.stage = Stage::Ended;
    vm.alive = false;
}

impl Vms for FakeVms {
    fn start(&self, request: &JobRequest) -> Result<(), VmError> {
        let mut state = self.lock();
        if state.frozen {
            return Err(VmError("the daemon that asked is dead".into()));
        }
        if state.fail_start > 0 {
            state.fail_start -= 1;
            return Err(VmError("the vm-host did not start".into()));
        }
        request.check().map_err(|error| VmError(error.to_string()))?;
        state.next_pid += 1;
        let ready = !state.slow_boot;
        let files = !state.late_state;
        let vm = FakeVm {
            state: JobState {
                job: request.job.clone(),
                pid: 1000 + state.next_pid,
                started_at: 0,
                stage: if ready { Stage::Running } else { Stage::Booting },
                vm_bytes: request.memory_bytes,
                limit_bytes: request.limits().memory_bytes,
                cpus: request.cpus,
                image: request.image.clone(),
                boots: 1,
                boot_ms: ready.then_some(1200),
                usage: None,
                resident_bytes: None,
                oom: None,
                end: None,
            },
            alive: true,
            silent: false,
            files,
            unkillable: false,
            cache: request.docker_cache.as_ref().map(|_| {
                state.cache_end.unwrap_or(CacheEnd {
                    found: CacheFound::Fresh,
                    clean: true,
                    daemon: true,
                })
            }),
        };
        state.vms.insert(request.job.clone(), vm);
        state.started.push(request.clone());
        Ok(())
    }

    fn look(&self, job: &str) -> Seen {
        let held = self.lock().held.clone();
        if let Some(held) = held {
            held.pass();
        }
        match self.lock().vms.get(job) {
            // It wrote nothing: there while its process is, and known to have exited after.
            Some(vm) if !vm.files && vm.alive => Seen::Nothing,
            Some(vm) if !vm.files => Seen::Exited,
            Some(vm) if !vm.alive => Seen::Gone(vm.state.clone()),
            Some(vm) if vm.silent => Seen::Silent(vm.state.clone()),
            Some(vm) => Seen::Alive(vm.state.clone()),
            None => Seen::Nothing,
        }
    }

    fn stop(&self, job: &str, reason: StopReason, grace: Duration) -> bool {
        let mut state = self.lock();
        if state.frozen {
            return false;
        }
        state.stopped.push((job.to_string(), reason, grace));
        let ignore = state.ignore_stops;
        match state.vms.get_mut(job) {
            Some(vm) if vm.alive && !vm.silent => {
                if !ignore {
                    finish(vm, End::of_stop(reason));
                }
                true
            }
            _ => false,
        }
    }

    fn kill(&self, job: &str) {
        let mut state = self.lock();
        if state.frozen {
            return;
        }
        state.killed.push(job.to_string());
        if let Some(vm) = state.vms.get_mut(job)
            && !vm.unkillable
        {
            let booting = vm.alive && vm.state.stage == Stage::Booting;
            vm.alive = false;
            state.died_booting += usize::from(booting);
        }
    }

    fn jobs(&self) -> Vec<String> {
        self.lock()
            .vms
            .iter()
            .filter(|(_, vm)| vm.files)
            .map(|(job, _)| job.clone())
            .collect()
    }

    fn clean(&self, job: &str) {
        let mut state = self.lock();
        if state.frozen {
            return;
        }
        state.cleaned.push(job.to_string());
        let gone = match state.vms.get_mut(job) {
            Some(vm) => {
                vm.files = false;
                !vm.alive
            }
            None => false,
        };
        if gone {
            state.vms.remove(job);
        }
    }
}

/// A Mac for tests: 64 GB and 16 cores that nobody works on, with room for `machines` of the smallest size at
/// once and one vCPU a machine, so that the count alone decides.
pub fn settings_for(machines: u32) -> Settings {
    let mut settings = Settings::for_host(TEST_MAC, Mode::Dedicated);
    settings.away.cap_bytes = u64::from(machines) * Size::G2.vm_bytes();
    settings.in_use = settings.away;
    settings.vcpus = [1; 4];
    settings
}

/// The Mac [`settings_for`] is for.
pub const TEST_MAC: HostFacts = HostFacts {
    total_bytes: 64 * GIB,
    cores: 16,
};

/// The machines of a test.
pub type TestMachines = Machines<FakeHost, FakeVms>;

/// What a test's machines are made of, kept so that a test can make them again as a daemon that starts again does.
#[derive(Clone)]
pub struct TestParts {
    /// The vm-hosts, which outlive a daemon.
    pub vms: FakeVms,
    /// The records, which outlive a daemon.
    pub store: Arc<dyn Store>,
    /// The Actions runners.
    pub roster: Arc<Roster>,
    /// The wall clock.
    pub clock: Arc<dyn Clock>,
    /// Where job reports go.
    pub reports: Arc<ReportLog>,
    /// The budget's numbers.
    pub settings: Settings,
    /// How often the machines nudged the scalers.
    pub nudges: Arc<std::sync::atomic::AtomicUsize>,
    /// The bell the machines ring, for a test with listeners.
    pub bell: Arc<crate::service::Nudge>,
    /// The Mac's boot.
    pub boot: String,
    /// Where Docker jobs' caches are kept, and their budget; none keeps none.
    pub docker_cache: Option<(std::path::PathBuf, u64)>,
}

impl TestParts {
    /// Machines on a calm Mac that was watched for a minute, so slots are open. `host` carries the Mac's clock on
    /// from the daemon before.
    pub fn machines(&self, host: Option<FakeHost>) -> Arc<TestMachines> {
        let machines = self.restored(host.unwrap_or_else(|| FakeHost::new(TEST_MAC)));
        pass(&machines, 61);
        machines
    }

    /// Machines as a daemon that just started has them, before its first look at the Mac.
    pub fn restored(&self, host: FakeHost) -> Arc<TestMachines> {
        let (nudges, bell) = (self.nudges.clone(), self.bell.clone());
        let machines = Machines::restore(MachineParts {
            host,
            vms: self.vms.clone(),
            settings: self.settings.clone(),
            config: MachineConfig {
                docker_cache: self.docker_cache.clone(),
                ..MachineConfig::new("mac-b", "2026.10.1", &self.boot, Duration::from_secs(6 * 3600))
            },
            roster: self.roster.clone(),
            store: self.store.clone(),
            clock: self.clock.clone(),
            reports: self.reports.clone(),
            nudge: Box::new(move || {
                nudges.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                bell.fire();
            }),
        })
        .expect("settings");
        Arc::new(machines)
    }
}

/// Lets `seconds` pass on the Mac, with a reading and a tick each second.
pub fn pass(machines: &TestMachines, seconds: u64) {
    for _ in 0..seconds {
        machines.with_host(|host| host.pass(Millis::secs(1)));
        machines.tick();
    }
}

/// Records in memory at which a test can make the daemon die between any two writes: the `n`th write from
/// [`CrashingStore::crash_at`] on is the daemon's last, kept or lost with it, and no write after it is kept until
/// [`CrashingStore::revive`]. `on_crash` runs at that moment, while the writer waits: whatever else the dead daemon
/// could still do (talk to GitHub, queue a report) is for the test to stop there.
#[derive(Default)]
pub struct CrashingStore {
    records: MemoryStore,
    crash: Mutex<Crash>,
    /// Every write of a record whose facts disagree ([`Record::agrees`]), as it was written.
    disagreements: Mutex<Vec<String>>,
}

#[derive(Default)]
struct Crash {
    /// Writes left before the crash, and whether the last one is kept.
    armed: Option<(usize, bool)>,
    dead: bool,
    on_crash: Option<Box<dyn Fn() + Send + Sync>>,
}

impl CrashingStore {
    /// The `writes`th write from now is the last; `kept`: whether it is written before the daemon dies.
    pub fn crash_at(&self, writes: usize, kept: bool, on_crash: impl Fn() + Send + Sync + 'static) {
        let mut crash = self.crash.lock().expect("crash");
        if crash.armed.is_none() && !crash.dead {
            crash.armed = Some((writes.max(1), kept));
            crash.on_crash = Some(Box::new(on_crash));
        }
    }

    /// Whether a crash is set and has not come yet.
    pub fn armed(&self) -> bool {
        self.crash.lock().expect("crash").armed.is_some()
    }

    /// Whether the daemon died at a write.
    pub fn crashed(&self) -> bool {
        self.crash.lock().expect("crash").dead
    }

    /// The writes of a record whose facts disagreed, at the moment of each: what a daemon that died right after it
    /// would have found.
    pub fn disagreements(&self) -> Vec<String> {
        self.disagreements.lock().expect("disagreements").clone()
    }

    /// Writes a record as another Runner left it: an older one's write is not this one's, and is not checked.
    pub fn plant(&self, record: &Record) {
        self.records.save(record);
    }

    /// No crash any more: the daemon that starts next writes again, and one that was set and did not come is off.
    pub fn revive(&self) {
        *self.crash.lock().expect("crash") = Crash::default();
    }

    /// Writes with `apply` unless the daemon is dead, counting the write towards the crash. The crash holds the
    /// lock until `on_crash` returned, so a write of another thread meanwhile finds the daemon dead.
    fn write(&self, apply: impl FnOnce()) {
        let mut crash = self.crash.lock().expect("crash");
        match crash.armed {
            _ if crash.dead => {}
            None => apply(),
            Some((left, kept)) if left > 1 => {
                crash.armed = Some((left - 1, kept));
                apply();
            }
            Some((_, kept)) => {
                crash.armed = None;
                crash.dead = true;
                if kept {
                    apply();
                }
                if let Some(on_crash) = &crash.on_crash {
                    on_crash();
                }
            }
        }
    }
}

impl Store for CrashingStore {
    fn save(&self, record: &Record) {
        if !record.agrees() {
            let slot = record.slot.map(|slot| slot.saved());
            self.disagreements.lock().expect("disagreements").push(format!(
                "{}: {slot:?} beside {:?}, unregistered: {}",
                record.runner, record.github, record.unregistered
            ));
        }
        self.write(|| self.records.save(record));
    }

    fn remove(&self, job: &str) {
        self.write(|| self.records.remove(job));
    }

    fn load(&self) -> Vec<Record> {
        self.records.load()
    }
}

/// Collects job reports.
#[derive(Default)]
pub struct ReportLog(Mutex<Vec<JobReport>>, Mutex<Option<Arc<dyn Reports>>>);

impl ReportLog {
    /// Every report, in order.
    pub fn all(&self) -> Vec<JobReport> {
        self.0.lock().expect("reports").clone()
    }

    /// Every report from now on goes there too.
    pub fn forward_to(&self, reports: Arc<dyn Reports>) {
        *self.1.lock().expect("reports") = Some(reports);
    }
}

impl Reports for ReportLog {
    fn job(&self, report: JobReport) {
        self.0.lock().expect("reports").push(report.clone());
        if let Some(also) = self.1.lock().expect("reports").as_ref() {
            also.job(report);
        }
    }
}

/// Sends job events to two listeners: a test's log and the machines.
pub struct Both<A, B>(pub Arc<A>, pub Arc<B>);

impl<A: Events, B: Events> Events for Both<A, B> {
    fn job(&self, event: &JobEvent) {
        self.0.job(event);
        self.1.job(event);
    }

    fn out_of_memory(&self, event: &JobEvent) {
        self.0.out_of_memory(event);
        self.1.out_of_memory(event);
    }
}

/// Collects the job events.
#[derive(Debug, Default)]
pub struct EventLog {
    jobs: Mutex<Vec<JobEvent>>,
    oom: Mutex<Vec<JobEvent>>,
}

impl EventLog {
    /// Started and completed events, in order.
    pub fn jobs(&self) -> Vec<JobEvent> {
        self.jobs.lock().expect("events").clone()
    }

    /// Events of jobs that used up their memory.
    pub fn out_of_memory(&self) -> Vec<JobEvent> {
        self.oom.lock().expect("events").clone()
    }
}

impl Events for EventLog {
    fn job(&self, event: &JobEvent) {
        self.jobs.lock().expect("events").push(event.clone());
    }

    fn out_of_memory(&self, event: &JobEvent) {
        self.oom.lock().expect("events").push(event.clone());
    }
}

/// An update a test scripts: what each build and each install answers, how long a build takes, and what was
/// installed.
#[derive(Debug, Default)]
pub struct FakeUpdate(Mutex<UpdateScript>);

#[derive(Debug, Default)]
struct UpdateScript {
    builds: std::collections::VecDeque<Result<String, String>>,
    installs: std::collections::VecDeque<Result<(), String>>,
    build_takes: Duration,
    built: usize,
    installed: Vec<crate::update::Pending>,
}

impl FakeUpdate {
    /// The next build answers `answer`.
    pub fn build_gives(&self, answer: Result<&str, &str>) {
        let answer = answer.map(str::to_string).map_err(str::to_string);
        self.0.lock().expect("update").builds.push_back(answer);
    }

    /// The next install answers `answer`.
    pub fn install_gives(&self, answer: Result<(), &str>) {
        let answer = answer.map_err(str::to_string);
        self.0.lock().expect("update").installs.push_back(answer);
    }

    /// Every build takes this long.
    pub fn build_takes(&self, time: Duration) {
        self.0.lock().expect("update").build_takes = time;
    }

    /// How many builds ran.
    pub fn built(&self) -> usize {
        self.0.lock().expect("update").built
    }

    /// What was installed, in order.
    pub fn installed(&self) -> Vec<crate::update::Pending> {
        self.0.lock().expect("update").installed.clone()
    }
}

impl crate::update::SelfUpdate for FakeUpdate {
    fn source(&self) -> String {
        "/src/runner main".into()
    }

    fn build(&self) -> Result<String, String> {
        let (takes, answer) = {
            let mut script = self.0.lock().expect("update");
            script.built += 1;
            let answer = script
                .builds
                .pop_front()
                .unwrap_or_else(|| Err("no build scripted".into()));
            (script.build_takes, answer)
        };
        std::thread::sleep(takes);
        answer
    }

    fn install(&self, pending: &crate::update::Pending) -> Result<(), String> {
        let mut script = self.0.lock().expect("update");
        let answer = script.installs.pop_front().unwrap_or(Ok(()));
        if answer.is_ok() {
            script.installed.push(pending.clone());
        }
        answer
    }
}
