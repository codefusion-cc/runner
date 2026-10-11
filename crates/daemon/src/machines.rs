//! The join: the budget, the lifecycle and the vm-hosts as one thing the scalers ask for slots and machines.
//!
//! One lock holds everything a decision reads, so a grant, the machine started on it and the record of both never
//! disagree. Every path a machine can end by goes through the lifecycle's effects, in their order: the slot's
//! memory goes back only after the vm-host is gone, the registration at GitHub is queued for removal, and the job
//! is reported once.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cf_budget::{
    Assigned, Budget, Grant, Hold, Host, HostReading, Level, Millis, Now, Settings, Size, SlotId, Status,
    Usage,
};
use cf_ghapp::Clock;
use cf_guest_proto::{StopReason, Workload};
use cf_lifecycle::{BreakerLimits, Effect, End, Event, Limits, Phase, Slot, StartBreaker};
use cf_vmhost::{JobRequest, JobState, OutOfMemory};

use crate::ends::{EndKind, machine_end};
use crate::names::job_name;
use crate::records::{Ending, GithubJob, Record, Store};
use crate::roster::{JobInfo, Roster, Runner};
use crate::traits::{
    Claim, EventKind, Events, HostError, Hosts, JobEvent, Refusal, RunnerSpec, Slots, Ticket,
};
use crate::vms::{Seen, Vms};

/// How long a machine that is gone waits for GitHub to say how its job ended before the job is reported with what
/// the Mac knows.
pub const RESULT_WAIT: Duration = Duration::from_secs(120);

/// What every machine of this Mac is started with.
#[derive(Debug, Clone)]
pub struct MachineConfig {
    /// What the Mac is called, for what a job is told.
    pub host: String,
    /// The version of the installed image jobs run in.
    pub image: String,
    /// The size of a job's disk.
    pub scratch_bytes: u64,
    /// The longest a job may run.
    pub deadline: Duration,
    /// The lifecycle's waits. One vm-host boots a machine twice by itself, so `starts` is 1 and `start` covers
    /// both boots.
    pub limits: Limits,
    /// When the Mac started, in seconds since 1970: a record from another start has no machine.
    pub boot: i64,
    /// What runs in place of GitHub's Actions runner: the integration suite's command. None in service.
    pub stand_in: Option<Workload>,
}

impl MachineConfig {
    /// The Runner's own waits for jobs of at most `deadline`: the vm-host keeps the deadline itself, so the
    /// lifecycle's own limit is a backstop past the vm-host's longest run.
    pub fn new(host: &str, image: &str, boot: i64, deadline: Duration) -> MachineConfig {
        let vm = cf_vmhost::Limits::for_deadline(deadline);
        let millis = |d: Duration| Millis(u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        MachineConfig {
            host: host.to_string(),
            image: image.to_string(),
            scratch_bytes: 40 << 30,
            deadline,
            limits: Limits {
                // Two boots of the vm-host's own, each with its forced stop, and the disk's copy before them.
                start: millis((vm.ready + vm.kill) * vm.boots + Duration::from_secs(30)),
                starts: 1,
                idle: Millis::mins(10),
                // The vm-host answers every look; one that is silent this long hangs.
                heartbeat: Millis::secs(60),
                job: millis(vm.longest_life() + Duration::from_secs(60)),
                finish: millis(vm.done + vm.kill + Duration::from_secs(10)),
                grace: millis(vm.deadline_grace),
                stop: millis(vm.stop_slack + vm.kill + Duration::from_secs(5)),
                kill_again: Millis::secs(10),
                long_sleep: Millis::mins(5),
            },
            boot,
            stand_in: None,
        }
    }
}

/// A job's start or end, for the console.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobReport {
    /// Names this report among all of this Mac's: the Actions runner and `started` or `completed`. A report that
    /// arrives twice has the same one.
    pub id: String,
    /// Whether the job started or is over.
    pub kind: EventKind,
    /// The Actions runner.
    pub runner: String,
    /// GitHub's job.
    pub github: GithubJob,
    /// How it ended; none for a start.
    pub end: Option<EndKind>,
    /// Whether a process of the job was stopped for memory.
    pub out_of_memory: bool,
    /// When this happened, in seconds since 1970.
    pub at: i64,
    /// The machine.
    pub machine: MachineFacts,
}

/// The machine a job ran in, and what was measured of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MachineFacts {
    /// The size's label, `cf-4g`.
    pub size: String,
    /// The machine's memory.
    pub memory_bytes: u64,
    /// The job's share of it.
    pub limit_bytes: u64,
    /// The machine's CPUs.
    pub cpus: u32,
    /// How the budget granted it: `claimed`, `assigned`, `assigned-in-use`, `adopted`.
    pub origin: String,
    /// From the vm-host's start to the workload's, in milliseconds.
    pub boot_ms: Option<u64>,
    /// The most memory the job held.
    pub memory_peak_bytes: Option<u64>,
    /// The most the job had on its disk.
    pub disk_peak_bytes: Option<u64>,
    /// CPU time, in microseconds.
    pub cpu_usec: Option<u64>,
    /// From the workload's start to its end, in milliseconds.
    pub wall_ms: Option<u64>,
}

/// Where job reports go: the console's queue.
pub trait Reports: Send + Sync {
    /// A job started or is over. Called once per job and kind; a daemon that died in between may call again with
    /// the same [`JobReport::id`], which the queue then drops.
    fn job(&self, report: JobReport);
}

/// A registration at GitHub that has to go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    /// The Actions runner's name.
    pub runner: String,
    /// GitHub's number for it, when the registration got as far as having one.
    pub runner_id: Option<i64>,
    /// The scale set.
    pub scale_set_id: i64,
    /// `owner/name`.
    pub repo: String,
}

/// One machine, for a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineStatus {
    /// The Actions runner.
    pub runner: String,
    /// The scale set.
    pub scale_set_id: i64,
    /// The lifecycle's phase, in a word.
    pub state: &'static str,
    /// When it was asked for, in seconds since 1970.
    pub started: i64,
    /// The machine's memory.
    pub memory_bytes: u64,
    /// The memory the job holds now.
    pub used_bytes: Option<u64>,
    /// GitHub's job, once it took one.
    pub github: Option<GithubJob>,
}

/// What the machines' side knows at one moment.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// The budget's account.
    pub budget: Status,
    /// Whether new jobs are taken, and why not.
    pub open: bool,
    /// The reason in the budget's word, empty while open.
    pub reason: String,
    /// The machines that stand.
    pub machines: Vec<MachineStatus>,
    /// Why the Mac could not be read, when it could not.
    pub host_error: Option<String>,
    /// Machines whose vm-host does not die.
    pub stuck: Vec<String>,
    /// Jobs that wait in line for memory.
    pub waiting: usize,
}

struct Job {
    record: Record,
    /// The budget's slot while the machine holds memory.
    slot: Option<SlotId>,
    life: Slot,
    /// The last state the vm-host gave.
    last: Option<JobState>,
    /// When the machine was gone, on the Mac's own clock.
    ended_at: Option<Millis>,
    /// Whether the registration is to be removed.
    unregister: bool,
    /// The end a restored slot is reported with, when its machine was not there any more.
    restored_end: Option<EndKind>,
}

struct Inner<H> {
    host: H,
    budget: Budget,
    breaker: StartBreaker,
    grants: BTreeMap<SlotId, Grant>,
    /// Places in line.
    waiting: BTreeSet<SlotId>,
    /// Places in line whose turn came and whose Actions runner has not started.
    ready: BTreeSet<SlotId>,
    jobs: BTreeMap<String, Job>,
    /// Registrations that are only intended, or left over, by Actions runner name.
    intents: BTreeMap<String, (Record, bool)>,
    paused: bool,
    draining: bool,
    host_error: Option<String>,
    stuck: BTreeSet<String>,
    /// Actions runners that lost a process for memory and are gone.
    recent_oom: VecDeque<String>,
    /// How long the Mac had slept in all when the machines were last told of its sleep.
    asleep: Millis,
}

/// The Mac's slots and machines. The scalers see it as [`Slots`], [`Hosts`] and [`Events`]; the daemon's loop
/// calls [`Machines::tick`] every second.
pub struct Machines<H, V> {
    inner: Mutex<Inner<H>>,
    vms: V,
    config: MachineConfig,
    roster: Arc<Roster>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    reports: Arc<dyn Reports>,
    nudge: Box<dyn Fn() + Send + Sync>,
}

/// Everything [`Machines`] is made of.
pub struct MachineParts<H, V> {
    /// Reads the Mac.
    pub host: H,
    /// The vm-hosts.
    pub vms: V,
    /// The budget's numbers.
    pub settings: Settings,
    /// What machines are started with.
    pub config: MachineConfig,
    /// The Actions runners, shared with the scalers.
    pub roster: Arc<Roster>,
    /// Where records are kept.
    pub store: Arc<dyn Store>,
    /// The wall clock.
    pub clock: Arc<dyn Clock>,
    /// Where job reports go.
    pub reports: Arc<dyn Reports>,
    /// Called when a slot opened or a waiting job may start: the scalers ask again.
    pub nudge: Box<dyn Fn() + Send + Sync>,
}

fn secs(at: SystemTime) -> i64 {
    at.duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// How long the Mac has slept in all, on its two clocks.
const fn asleep(now: Now) -> Millis {
    Millis(now.elapsed.0.saturating_sub(now.awake.0))
}

fn duration(span: Millis) -> Duration {
    Duration::from_millis(span.0)
}

const fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Reserved => "reserved",
        Phase::Starting | Phase::Retrying => "starting",
        Phase::Idle => "idle",
        Phase::Running => "running",
        Phase::Finishing => "finishing",
        Phase::Stopping | Phase::Killing => "stopping",
        Phase::Released => "ended",
    }
}

impl<H: Host + Send, V: Vms> Machines<H, V> {
    /// The machines of a daemon that starts: what the one before left is adopted or ended.
    ///
    /// The rule: a machine whose vm-host answers on its socket, from the process its state names, in the Mac's
    /// same boot, is adopted; its memory is counted first, oldest machine first, and its job goes on. Every other
    /// record has no machine any more and is ended: with the end its vm-host wrote if it wrote one, as
    /// `host-shutdown` when the Mac started again since, as `daemon-restart` when its machine was not up yet, and
    /// as `guest-lost` otherwise. A vm-host that is there and silent is killed first. What is in the state folder
    /// without a record is nobody's job: killed and removed.
    pub fn restore(parts: MachineParts<H, V>) -> Result<Machines<H, V>, cf_budget::SettingsError> {
        let MachineParts {
            host,
            vms,
            settings,
            config,
            roster,
            store,
            clock,
            reports,
            nudge,
        } = parts;
        let asleep = asleep(host.now());
        let machines = Machines {
            inner: Mutex::new(Inner {
                asleep,
                host,
                budget: Budget::new(settings)?,
                breaker: StartBreaker::new(BreakerLimits::default()),
                grants: BTreeMap::new(),
                waiting: BTreeSet::new(),
                ready: BTreeSet::new(),
                jobs: BTreeMap::new(),
                intents: BTreeMap::new(),
                paused: false,
                draining: false,
                host_error: None,
                stuck: BTreeSet::new(),
                recent_oom: VecDeque::new(),
            }),
            vms,
            config,
            roster,
            store,
            clock,
            reports,
            nudge,
        };
        machines.adopt();
        Ok(machines)
    }

    fn lock(&self) -> MutexGuard<'_, Inner<H>> {
        self.inner.lock().expect("machines")
    }

    fn wall(&self) -> i64 {
        secs(self.clock.now())
    }

    fn adopt(&self) {
        let mut inner = self.lock();
        let now = inner.host.now();
        let records = self.store.load();
        let known: BTreeSet<String> = records.iter().map(|r| r.job.clone()).collect();
        for job in self.vms.jobs() {
            if !known.contains(&job) {
                tracing::warn!(%job, "a machine without a record was found; it is ended and removed");
                if matches!(self.vms.look(&job), Seen::Alive(_) | Seen::Silent(_)) {
                    self.vms.kill(&job);
                }
                self.vms.clean(&job);
            }
        }
        for record in records {
            let Some(saved) = record.slot else {
                if record.unregistered {
                    self.store.remove(&record.job);
                } else {
                    inner.intents.insert(record.runner.clone(), (record, true));
                }
                continue;
            };
            let name = record.job.clone();
            if record.ended.is_some() {
                let (life, _) = Slot::restore(self.config.limits, now.elapsed, saved.saved(), false);
                self.vms.clean(&name);
                inner.jobs.insert(
                    name.clone(),
                    Job {
                        record,
                        slot: None,
                        life,
                        last: None,
                        ended_at: Some(now.elapsed),
                        unregister: true,
                        restored_end: None,
                    },
                );
                self.settle(&mut inner, &name, now);
                continue;
            }
            let mut seen = self.vms.look(&name);
            if matches!(seen, Seen::Silent(_)) {
                self.vms.kill(&name);
                seen = self.vms.look(&name);
            }
            let same_boot = record.boot == self.config.boot;
            let size = record.size();
            let state = match &seen {
                Seen::Alive(s) | Seen::Silent(s) | Seen::Gone(s) => Some(s.clone()),
                Seen::Nothing | Seen::Exited => None,
            };
            let alive = matches!(seen, Seen::Alive(_)) && same_boot && size.is_some();
            if matches!(seen, Seen::Alive(_)) && !alive {
                // It answers but cannot be this record's machine (another boot, an unknown size): end it.
                self.vms.kill(&name);
            }
            let slot = match (alive, size) {
                (true, Some(size)) => {
                    let grant = inner.budget.adopt(size);
                    inner.grants.insert(grant.slot, grant);
                    Some(grant.slot)
                }
                _ => None,
            };
            let phase = saved.saved().phase;
            let (life, effects) = Slot::restore(self.config.limits, now.elapsed, saved.saved(), alive);
            let restored_end = (!alive).then(|| match state.as_ref().and_then(|s| s.end.as_ref()) {
                Some(ended) => machine_end(saved.saved().end.unwrap_or(End::Ok), Some(ended.end), false),
                None if !same_boot => EndKind::HostShutdown,
                None if matches!(phase, Phase::Reserved | Phase::Starting | Phase::Retrying) => {
                    EndKind::DaemonRestart
                }
                None => EndKind::GuestLost,
            });
            if alive {
                tracing::info!(runner = %record.runner, job = %name, "a machine of the Runner before was adopted");
                self.roster.adopt(Runner {
                    name: record.runner.clone(),
                    handle: name.clone(),
                    runner_id: record.runner_id.unwrap_or(0),
                    scale_set_id: record.scale_set_id,
                    ticket: slot,
                    started: UNIX_EPOCH + Duration::from_secs(u64::try_from(record.created_at).unwrap_or(0)),
                    busy: record.github.is_some(),
                    adopted: true,
                    done: None,
                    job: record.github.as_ref().map(|g| JobInfo {
                        repo: g.repo.clone(),
                        name: g.job.clone(),
                        workflow: g.workflow.clone(),
                        run_id: g.run_id,
                        started: UNIX_EPOCH
                            + Duration::from_secs(u64::try_from(g.started_at.unwrap_or(0)).unwrap_or(0)),
                    }),
                });
            } else {
                tracing::warn!(
                    runner = %record.runner, job = %name, end = restored_end.map_or("", EndKind::name),
                    "a machine of the Runner before is not there any more"
                );
            }
            inner.jobs.insert(
                name.clone(),
                Job {
                    record,
                    slot,
                    life,
                    last: state,
                    ended_at: None,
                    unregister: false,
                    restored_end,
                },
            );
            self.apply(&mut inner, &name, now, effects);
        }
    }

    /// Before every decision of the budget's: the holds that are not the budget's own to know.
    fn holds(&self, inner: &mut Inner<H>, now: Now) {
        let failures = inner.breaker.holds(now.elapsed) || !inner.stuck.is_empty();
        inner.budget.hold(Hold::StartFailures, failures);
        inner.budget.hold(Hold::Paused, inner.paused);
        inner.budget.hold(Hold::Draining, inner.draining);
    }

    /// Takes new jobs or not, as the console or the person asks. The polls in flight are told, since each still
    /// offers GitHub the slot the Mac had when it began.
    pub fn pause(&self, paused: bool) {
        let changed = std::mem::replace(&mut self.lock().paused, paused) != paused;
        if changed {
            (self.nudge)();
        }
    }

    /// Takes no new job from now on: the Runner is about to stop. The polls in flight are told.
    pub fn drain(&self) {
        let changed = !std::mem::replace(&mut self.lock().draining, true);
        if changed {
            (self.nudge)();
        }
    }

    /// New numbers for the budget.
    pub fn set_settings(&self, settings: Settings) -> Result<(), cf_budget::SettingsError> {
        self.lock().budget.set_settings(settings)
    }

    /// Reads the Mac, lets the budget and every machine's lifecycle see the time that passed, and does what they
    /// say. Call it every second.
    pub fn tick(&self) {
        let mut inner = self.lock();
        let now = inner.host.now();
        let reading: Option<HostReading> = match inner.host.read() {
            Ok(reading) => {
                inner.host_error = None;
                Some(reading)
            }
            Err(error) => {
                inner.host_error = Some(error.to_string());
                None
            }
        };
        self.holds(&mut inner, now);
        self.woke(&mut inner, now);
        let observed = inner.budget.observe(now, reading);
        let mut nudge = false;
        for slot in observed.expired {
            inner.grants.remove(&slot);
            inner.breaker.unused();
            nudge = true;
        }
        for grant in observed.started {
            inner.breaker.admit(now.elapsed);
            inner.waiting.remove(&grant.slot);
            inner.ready.insert(grant.slot);
            inner.grants.insert(grant.slot, grant);
            nudge = true;
        }
        let names: Vec<String> = inner.jobs.keys().cloned().collect();
        if let Some(eviction) = observed.evict {
            let job = inner
                .jobs
                .iter()
                .find(|(_, job)| job.slot == Some(eviction.slot))
                .map(|(name, _)| name.clone());
            match job {
                Some(name) => {
                    tracing::warn!(job = %name, cause = ?eviction.cause, "the Mac needs room; a job is stopped");
                    self.feed(&mut inner, &name, now, Event::Evict(eviction.cause));
                }
                // A promise or a place in line holds no machine: giving it back is all there is to stop.
                None => {
                    let _ = inner.budget.release(eviction.slot);
                    inner.grants.remove(&eviction.slot);
                    inner.ready.remove(&eviction.slot);
                }
            }
        }
        for name in &names {
            self.watch(&mut inner, name, now);
            self.feed(&mut inner, name, now, Event::Tick);
            nudge |= self.settle(&mut inner, name, now);
        }
        drop(inner);
        if nudge {
            (self.nudge)();
        }
    }

    /// Looks at a job's vm-host and tells the lifecycle what there is to tell.
    fn watch(&self, inner: &mut Inner<H>, name: &str, now: Now) {
        let Some(job) = inner.jobs.get(name) else {
            return;
        };
        let phase = job.life.phase();
        if phase == Phase::Released {
            return;
        }
        let slot = job.slot;
        let had_oom = job.record.oom;
        let never_seen = job.last.is_none();
        let seen = self.vms.look(name);
        let mut events = Vec::new();
        match &seen {
            // The vm-host has not written its state yet; the start's own time runs. So it does when GitHub's word
            // of the job was here before the first look: then the time a vm-host may be silent runs.
            Seen::Nothing if phase == Phase::Starting || never_seen => {}
            Seen::Nothing | Seen::Exited => events.push(Event::VmExited),
            Seen::Silent(_) => {}
            Seen::Alive(state) => {
                events.push(Event::Heartbeat);
                if state.boot_ms.is_some() && phase == Phase::Starting {
                    events.push(Event::VmReady);
                }
                if state.oom.is_some() && !had_oom {
                    events.push(Event::OutOfMemory);
                }
                if let (Some(slot), Some(usage)) = (slot, state.usage.as_ref()) {
                    // What the job ever held stays with the machine; the guest's own share on top of it is not
                    // counted, so the budget never takes more off "still to come" than the Mac has given.
                    let _ = inner.budget.usage(
                        now,
                        slot,
                        Usage {
                            resident_bytes: usage.memory_peak_bytes.min(state.vm_bytes),
                            scratch_bytes: usage.disk_bytes,
                        },
                    );
                }
            }
            Seen::Gone(state) => {
                if let Some(ended) = &state.end {
                    use cf_vmhost::End as VmEnd;
                    let ran = matches!(ended.end, VmEnd::Ok | VmEnd::Oom | VmEnd::Failed);
                    if ran && phase == Phase::Starting {
                        events.push(Event::VmReady);
                    }
                    if state.oom.is_some() && !had_oom {
                        events.push(Event::OutOfMemory);
                    }
                    // The workload ran to its end, so the job is over whether GitHub's word of it came or not.
                    if ran {
                        events.push(Event::JobCompleted);
                    }
                }
                events.push(Event::VmExited);
            }
        }
        if let Some(job) = inner.jobs.get_mut(name) {
            match seen {
                Seen::Alive(state) | Seen::Gone(state) | Seen::Silent(state) => {
                    job.record.oom |= state.oom.is_some();
                    job.last = Some(state);
                }
                Seen::Nothing | Seen::Exited => {}
            }
        }
        for event in events {
            self.feed(inner, name, now, event);
        }
    }

    /// Tells every machine of the time the Mac slept since they were last told, before anything else happens to
    /// one: a wait that starts now is not put off by a sleep that is over.
    fn woke(&self, inner: &mut Inner<H>, now: Now) {
        let total = asleep(now);
        let slept = Millis(total.0.saturating_sub(inner.asleep.0));
        inner.asleep = total;
        if slept > Millis::ZERO {
            let names: Vec<String> = inner.jobs.keys().cloned().collect();
            for name in names {
                self.step(inner, &name, now, Event::Slept(slept));
            }
        }
    }

    fn feed(&self, inner: &mut Inner<H>, name: &str, now: Now, event: Event) {
        self.woke(inner, now);
        self.step(inner, name, now, event);
    }

    fn step(&self, inner: &mut Inner<H>, name: &str, now: Now, event: Event) {
        let Some(job) = inner.jobs.get_mut(name) else {
            return;
        };
        let before = job.life.saved();
        let effects = job.life.on(now.elapsed, event);
        let changed = job.life.saved() != before;
        self.apply(inner, name, now, effects);
        if changed && let Some(job) = inner.jobs.get_mut(name) {
            job.record.slot = Some(job.life.saved().into());
            self.store.save(&job.record);
        }
    }

    /// Does what the lifecycle says, in its order.
    fn apply(&self, inner: &mut Inner<H>, name: &str, now: Now, effects: Vec<Effect>) {
        for effect in effects {
            let Some(job) = inner.jobs.get_mut(name) else {
                return;
            };
            match effect {
                // A machine is started where the scaler asked for it ([`Hosts::start`]); `starts` is 1, so the
                // lifecycle never asks for a second.
                Effect::StartVm | Effect::Tell(_) => {}
                Effect::StopVm { grace } => {
                    let reason = match job.life.saved().end {
                        Some(End::Timeout) => StopReason::Timeout,
                        Some(End::EvictedMemory) => StopReason::EvictedMemory,
                        Some(End::EvictedDisk) => StopReason::EvictedDisk,
                        Some(End::HostSlept) => StopReason::HostSlept,
                        _ => StopReason::Cancelled,
                    };
                    if !self.vms.stop(name, reason, duration(grace)) {
                        tracing::warn!(job = %name, "the vm-host took no stop; it is ended by force when its time is up");
                    }
                }
                Effect::KillVm => self.vms.kill(name),
                Effect::Unregister => job.unregister = true,
                Effect::Release => {
                    if let Some(slot) = job.slot.take() {
                        let _ = inner.budget.release(slot);
                        inner.grants.remove(&slot);
                    }
                }
                Effect::StartFailed => inner.breaker.failed(now.elapsed),
                Effect::StartSucceeded => inner.breaker.succeeded(),
                Effect::Stuck => {
                    if inner.stuck.insert(name.to_string()) {
                        tracing::error!(job = %name, "a vm-host does not die; its memory stays counted and no new job is taken");
                    }
                }
                Effect::Ended { end, ran_job } => self.ended(inner, name, now, end, ran_job),
            }
        }
    }

    /// The machine is gone: what it left is removed and how it ended is written down.
    fn ended(&self, inner: &mut Inner<H>, name: &str, now: Now, end: End, ran_job: bool) {
        let draining = inner.draining;
        inner.stuck.remove(name);
        let Some(job) = inner.jobs.get_mut(name) else {
            return;
        };
        let vm_end = job.last.as_ref().and_then(|s| s.end.as_ref());
        let kind = job
            .restored_end
            .take()
            .unwrap_or_else(|| machine_end(end, vm_end.map(|e| e.end), draining));
        let summary = vm_end.and_then(|e| e.summary.as_ref());
        let usage = summary
            .map(|s| &s.usage)
            .or(job.last.as_ref().and_then(|s| s.usage.as_ref()));
        job.record.ended = Some(Ending {
            kind: kind.name().to_string(),
            at: self.wall(),
            boot_ms: job.last.as_ref().and_then(|s| s.boot_ms),
            memory_peak_bytes: usage.map(|u| u.memory_peak_bytes),
            disk_peak_bytes: summary.map(|s| s.disk_peak_bytes),
            cpu_usec: usage.map(|u| u.cpu_usec),
            wall_ms: summary.map(|s| s.wall_ms),
            message: vm_end.and_then(|e| e.message.clone()),
        });
        job.record.slot = Some(job.life.saved().into());
        job.ended_at = Some(now.elapsed);
        job.unregister = true;
        let oom = job.record.oom;
        let runner = job.record.runner.clone();
        tracing::info!(%runner, job = %name, end = kind.name(), ran_job, "machine gone");
        self.store.save(&job.record);
        self.vms.clean(name);
        self.roster.remove(&runner);
        if oom {
            inner.recent_oom.push_back(runner);
            if inner.recent_oom.len() > 64 {
                inner.recent_oom.pop_front();
            }
        }
    }

    /// Reports a job whose machine is gone once GitHub said how it ended or the wait for that is over, and
    /// forgets an Actions runner nothing is left of. Says whether a slot opened.
    fn settle(&self, inner: &mut Inner<H>, name: &str, now: Now) -> bool {
        let Some(job) = inner.jobs.get_mut(name) else {
            return false;
        };
        let Some(ending) = job.record.ended.clone() else {
            return false;
        };
        let mut opened = false;
        if !job.record.end_reported {
            let waited = job.ended_at.map_or(Millis(u64::MAX), |at| now.elapsed.since(at));
            let result = job.record.github.as_ref().and_then(|g| g.result.clone());
            if result.is_none() && duration(waited) < RESULT_WAIT {
                return false;
            }
            if let Some(github) = job.record.github.clone() {
                let base = EndKind::from_name(&ending.kind).unwrap_or(EndKind::GuestLost);
                self.reports.job(JobReport {
                    id: format!("{}/completed", job.record.runner),
                    kind: EventKind::Completed,
                    runner: job.record.runner.clone(),
                    github,
                    end: Some(base.with_result(result.as_deref())),
                    out_of_memory: job.record.oom,
                    at: ending.at,
                    machine: facts(&job.record),
                });
            }
            job.record.end_reported = true;
            self.store.save(&job.record);
            opened = true;
        }
        if job.record.finished() {
            self.store.remove(name);
            inner.jobs.remove(name);
        }
        opened
    }

    /// The registrations at GitHub that have to go: of machines that are gone, and of Actions runners that were
    /// registered and never got a machine. Whoever holds the scale set clients removes them and says so with
    /// [`Machines::unregistered`].
    pub fn registrations_to_remove(&self) -> Vec<Registration> {
        let inner = self.lock();
        let of = |record: &Record| Registration {
            runner: record.runner.clone(),
            runner_id: record.runner_id,
            scale_set_id: record.scale_set_id,
            repo: record.repo.clone(),
        };
        inner
            .jobs
            .values()
            .filter(|job| job.unregister && !job.record.unregistered)
            .map(|job| of(&job.record))
            .chain(
                inner
                    .intents
                    .values()
                    .filter(|(_, stale)| *stale)
                    .map(|(record, _)| of(record)),
            )
            .collect()
    }

    /// GitHub no longer has a registration of this name.
    pub fn unregistered(&self, runner: &str) {
        let mut inner = self.lock();
        let now = inner.host.now();
        if let Some((record, _)) = inner.intents.remove(runner) {
            self.store.remove(&record.job);
        }
        let name = job_name(runner).to_string();
        if let Some(job) = inner.jobs.get_mut(&name)
            && job.record.runner == runner
        {
            job.record.unregistered = true;
            self.store.save(&job.record);
            self.settle(&mut inner, &name, now);
        }
    }

    /// Whether an Actions runner that is gone had lost a process for memory.
    pub fn was_out_of_memory(&self, runner: &str) -> bool {
        self.lock().recent_oom.iter().any(|name| name == runner)
    }

    /// Asks every machine to stop, as the console's stop or an uninstall does.
    pub fn stop_all(&self) {
        let mut inner = self.lock();
        let now = inner.host.now();
        let names: Vec<String> = inner.jobs.keys().cloned().collect();
        for name in names {
            self.feed(&mut inner, &name, now, Event::Stop);
        }
    }

    /// How many Actions runners the Runner still has something to do for: a machine that stands, an end to
    /// report, a registration to remove.
    pub fn unsettled(&self) -> usize {
        let inner = self.lock();
        inner.jobs.len() + inner.intents.len()
    }

    /// What there is to report.
    pub fn snapshot(&self) -> Snapshot {
        let mut inner = self.lock();
        let now = inner.host.now();
        self.holds(&mut inner, now);
        let budget = inner.budget.status(now);
        let open = budget.level == Level::Open;
        let reason = budget.closed_by.map_or("", |r| r.name()).to_string();
        Snapshot {
            open,
            reason,
            machines: inner
                .jobs
                .values()
                .filter(|job| job.record.ended.is_none())
                .map(|job| MachineStatus {
                    runner: job.record.runner.clone(),
                    scale_set_id: job.record.scale_set_id,
                    state: phase_name(job.life.phase()),
                    started: job.record.created_at,
                    memory_bytes: job.record.memory_bytes,
                    used_bytes: job
                        .last
                        .as_ref()
                        .and_then(|s| s.usage.as_ref())
                        .map(|u| u.memory_bytes),
                    github: job.record.github.clone(),
                })
                .collect(),
            host_error: inner.host_error.clone(),
            stuck: inner.stuck.iter().cloned().collect(),
            waiting: inner.waiting.len(),
            budget,
        }
    }

    /// The budget's numbers.
    pub fn settings(&self) -> Settings {
        self.lock().budget.settings().clone()
    }

    /// The Mac's own clock.
    pub fn now(&self) -> Now {
        self.lock().host.now()
    }

    /// Runs `with` on the host, as a test changes what the Mac reads.
    #[doc(hidden)]
    pub fn with_host<T>(&self, with: impl FnOnce(&mut H) -> T) -> T {
        with(&mut self.lock().host)
    }

    /// The vm-hosts.
    #[doc(hidden)]
    pub fn vms(&self) -> &V {
        &self.vms
    }
}

fn facts(record: &Record) -> MachineFacts {
    let ended = record.ended.as_ref();
    MachineFacts {
        size: record.size.clone().unwrap_or_default(),
        memory_bytes: record.memory_bytes,
        limit_bytes: record.limit_bytes,
        cpus: record.cpus,
        origin: record.origin.clone(),
        boot_ms: ended.and_then(|e| e.boot_ms),
        memory_peak_bytes: ended.and_then(|e| e.memory_peak_bytes),
        disk_peak_bytes: ended.and_then(|e| e.disk_peak_bytes),
        cpu_usec: ended.and_then(|e| e.cpu_usec),
        wall_ms: ended.and_then(|e| e.wall_ms),
    }
}

impl<H: Host + Send, V: Vms> Slots for Machines<H, V> {
    fn reserve(&self, size: Size) -> Result<Ticket, Refusal> {
        let mut inner = self.lock();
        let now = inner.host.now();
        self.holds(&mut inner, now);
        if !inner.breaker.admit(now.elapsed) {
            return Err(Refusal(Hold::StartFailures.name().to_string()));
        }
        match inner.budget.claim(now, size) {
            Ok(grant) => {
                inner.grants.insert(grant.slot, grant);
                Ok(grant.slot)
            }
            Err(refusal) => {
                inner.breaker.unused();
                Err(Refusal(refusal.name().to_string()))
            }
        }
    }

    fn release(&self, ticket: Ticket) {
        let mut inner = self.lock();
        if inner.jobs.values().any(|job| job.slot == Some(ticket)) {
            // A machine holds it: the slot goes back when the machine is gone, and not before.
            return;
        }
        inner.grants.remove(&ticket);
        inner.ready.remove(&ticket);
        if inner.budget.release(ticket).is_ok() {
            inner.breaker.unused();
            drop(inner);
            (self.nudge)();
        }
    }

    fn claim(&self, size: Size, promised: Option<Ticket>) -> Claim {
        let mut inner = self.lock();
        let now = inner.host.now();
        self.holds(&mut inner, now);
        if let Some(ticket) = promised {
            if inner.budget.confirm(ticket).is_ok() {
                return Claim::Start(ticket);
            }
            inner.grants.remove(&ticket);
        }
        match inner.budget.assign(now, size) {
            Assigned::Start(grant) => {
                inner.breaker.admit(now.elapsed);
                inner.grants.insert(grant.slot, grant);
                Claim::Start(grant.slot)
            }
            Assigned::Waits { slot, why } => {
                inner.waiting.insert(slot);
                Claim::Waits {
                    ticket: slot,
                    why: why.name().to_string(),
                }
            }
            Assigned::Never(refusal) => Claim::Never(refusal.name().to_string()),
        }
    }

    fn ready(&self, ticket: Ticket) -> bool {
        self.lock().ready.contains(&ticket)
    }

    fn withdraw(&self, ticket: Ticket) {
        let mut inner = self.lock();
        if inner.jobs.values().any(|job| job.slot == Some(ticket)) {
            return;
        }
        if inner.waiting.remove(&ticket) {
            let _ = inner.budget.withdraw(ticket);
        } else if inner.ready.remove(&ticket) {
            inner.grants.remove(&ticket);
            let _ = inner.budget.release(ticket);
            inner.breaker.unused();
            drop(inner);
            (self.nudge)();
        }
    }

    fn open_slots(&self, size: Size) -> usize {
        let mut inner = self.lock();
        let now = inner.host.now();
        self.holds(&mut inner, now);
        inner.budget.free(now).of(size) as usize
    }

    fn state(&self) -> (bool, String) {
        let mut inner = self.lock();
        let now = inner.host.now();
        self.holds(&mut inner, now);
        let status = inner.budget.status(now);
        (
            status.level == Level::Open,
            status.closed_by.map_or("", |r| r.name()).to_string(),
        )
    }
}

impl<H: Host + Send, V: Vms> Hosts for Machines<H, V> {
    fn registering(&self, name: &str, repo: &str, scale_set_id: i64) {
        let record = Record {
            job: job_name(name).to_string(),
            runner: name.to_string(),
            runner_id: None,
            scale_set_id,
            repo: repo.to_string(),
            size: None,
            memory_bytes: 0,
            limit_bytes: 0,
            cpus: 0,
            origin: String::new(),
            slot: None,
            created_at: self.wall(),
            boot: self.config.boot,
            unregistered: false,
            github: None,
            start_reported: false,
            ended: None,
            end_reported: true,
            oom: false,
        };
        self.store.save(&record);
        self.lock().intents.insert(name.to_string(), (record, false));
    }

    fn not_registered(&self, name: &str) {
        if let Some((record, _)) = self.lock().intents.remove(name) {
            self.store.remove(&record.job);
        }
    }

    fn registration_unsure(&self, name: &str) {
        if let Some((_, stale)) = self.lock().intents.get_mut(name) {
            *stale = true;
        }
    }

    fn lost_with_job(&self, scale_set_id: i64) -> usize {
        self.lock()
            .jobs
            .values()
            .filter(|job| {
                let record = &job.record;
                record.scale_set_id == scale_set_id
                    && record.ended.is_some()
                    && !record.unregistered
                    && record
                        .github
                        .as_ref()
                        .is_some_and(|github| github.result.is_none())
            })
            .count()
    }

    async fn start(&self, spec: RunnerSpec) -> Result<String, HostError> {
        let mut inner = self.lock();
        let now = inner.host.now();
        let name = job_name(&spec.name).to_string();
        let refuse = |inner: &mut Inner<H>, why: String| {
            // The registration exists and has no machine: it is left as one to remove.
            if let Some((record, stale)) = inner.intents.get_mut(&spec.name) {
                record.runner_id = Some(spec.runner_id);
                *stale = true;
                self.store.save(record);
            }
            Err(HostError(why))
        };
        if inner.jobs.contains_key(&name) {
            return refuse(&mut inner, format!("a machine named {name} is there already"));
        }
        let Some(grant) = inner.grants.get(&spec.ticket).copied() else {
            return refuse(&mut inner, format!("slot {} is not granted", spec.ticket.0));
        };
        let request = JobRequest {
            job: name.clone(),
            host: self.config.host.clone(),
            memory_bytes: grant.vm_bytes,
            cpus: grant.cpus,
            scratch_bytes: self.config.scratch_bytes,
            image: self.config.image.clone(),
            workload: self.config.stand_in.clone().unwrap_or(Workload::ActionsRunner {
                jit_config: spec.jit_config,
            }),
            env: Vec::new(),
            deadline_s: self.config.deadline.as_secs(),
        };
        self.woke(&mut inner, now);
        let mut life = Slot::reserved(self.config.limits);
        let effects = life.on(now.elapsed, Event::Assigned);
        debug_assert_eq!(effects, [Effect::StartVm]);
        let record = Record {
            job: name.clone(),
            runner: spec.name.clone(),
            runner_id: Some(spec.runner_id),
            scale_set_id: spec.scale_set_id,
            repo: spec.repo.clone(),
            size: Some(spec.size.label().to_string()),
            memory_bytes: grant.vm_bytes,
            limit_bytes: grant.job_limit_bytes,
            cpus: grant.cpus,
            origin: grant.origin.name().to_string(),
            slot: Some(life.saved().into()),
            created_at: self.wall(),
            boot: self.config.boot,
            unregistered: false,
            github: None,
            start_reported: false,
            ended: None,
            end_reported: false,
            oom: false,
        };
        // Written before the vm-host exists: a Runner that dies right here finds the record and no machine.
        self.store.save(&record);
        if let Err(error) = self.vms.start(&request) {
            inner.breaker.failed(now.elapsed);
            self.vms.clean(&name);
            // Back to a registration without a machine, which the caller removes; the slot stays the caller's.
            let mut intent = record;
            intent.slot = None;
            intent.end_reported = true;
            self.store.save(&intent);
            inner.intents.insert(spec.name.clone(), (intent, true));
            return Err(HostError(error.to_string()));
        }
        inner.intents.remove(&spec.name);
        inner.ready.remove(&spec.ticket);
        inner.jobs.insert(
            name.clone(),
            Job {
                record,
                slot: Some(spec.ticket),
                life,
                last: None,
                ended_at: None,
                unregister: false,
                restored_end: None,
            },
        );
        Ok(name)
    }

    async fn stop(&self, handle: &str) -> Result<(), HostError> {
        let mut inner = self.lock();
        let now = inner.host.now();
        self.feed(&mut inner, handle, now, Event::Stop);
        Ok(())
    }

    async fn out_of_memory(&self, handle: &str) -> OutOfMemory {
        let inner = self.lock();
        let Some(job) = inner.jobs.get(handle) else {
            return OutOfMemory::Unknown {
                reason: "the machine is gone".into(),
            };
        };
        match self.vms.look(handle) {
            Seen::Alive(state) | Seen::Gone(state) => state.out_of_memory(),
            Seen::Silent(_) => OutOfMemory::Unknown {
                reason: "the vm-host does not answer".into(),
            },
            Seen::Nothing | Seen::Exited => match &job.last {
                Some(state) => state.out_of_memory(),
                None => OutOfMemory::Unknown {
                    reason: "the vm-host has said nothing yet".into(),
                },
            },
        }
    }
}

impl<H: Host + Send, V: Vms> Events for Machines<H, V> {
    fn job(&self, event: &JobEvent) {
        let mut inner = self.lock();
        let now = inner.host.now();
        let name = job_name(&event.runner).to_string();
        let Some(job) = inner
            .jobs
            .get_mut(&name)
            .filter(|job| job.record.runner == event.runner)
        else {
            tracing::debug!(runner = %event.runner, "a job event for an Actions runner that is not this Mac's any more");
            return;
        };
        let github = job.record.github.get_or_insert_with(GithubJob::default);
        github.repo.clone_from(&event.repo);
        github.job.clone_from(&event.job);
        github.workflow.clone_from(&event.workflow);
        github.run_id = event.run_id;
        github.event.clone_from(&event.event);
        github.queued_at = event.queued_at.map(|t| t.unix_timestamp());
        github.started_at = event.started_at.map(|t| t.unix_timestamp()).or(github.started_at);
        if event.kind == EventKind::Completed {
            github.result = Some(event.result.clone());
            github.completed_at = Some(event.at.unix_timestamp());
            job.record.oom |= event.out_of_memory;
        }
        if !job.record.start_reported {
            job.record.start_reported = true;
            self.reports.job(JobReport {
                id: format!("{}/started", job.record.runner),
                kind: EventKind::Started,
                runner: job.record.runner.clone(),
                github: job.record.github.clone().unwrap_or_default(),
                end: None,
                out_of_memory: false,
                at: event.started_at.unwrap_or(event.at).unix_timestamp(),
                machine: facts(&job.record),
            });
        }
        self.store.save(&job.record);
        let step = match event.kind {
            EventKind::Started => Event::JobStarted,
            EventKind::Completed => Event::JobCompleted,
        };
        self.feed(&mut inner, &name, now, step);
        let opened = self.settle(&mut inner, &name, now);
        drop(inner);
        if opened {
            (self.nudge)();
        }
    }

    fn out_of_memory(&self, _event: &JobEvent) {}
}
