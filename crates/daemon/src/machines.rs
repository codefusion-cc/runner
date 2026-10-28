//! The join: the budget, the lifecycle and the vm-hosts as one thing the scalers ask for slots and machines.
//!
//! One lock holds everything a decision reads, so a grant, the machine started on it and the record of both never
//! disagree. It is not held while a vm-host is looked at: what a look showed is told to the machine afterwards, as
//! of the phase the machine is in then. Every path a machine can end by goes through the lifecycle's effects, in
//! their order: the slot's memory goes back only after the vm-host is gone, the registration at GitHub is queued
//! for removal, and the job is reported once.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use cf_budget::{
    Assigned, Budget, Grant, Hold, Host, HostReading, Level, Millis, Now, Settings, Size, SlotId, Status,
    Turn, Usage,
};
use cf_ghapp::Clock;
use cf_guest_proto::{Pressure, Psi, StopReason, Workload};
use cf_lifecycle::{BreakerLimits, Effect, End, Event, Limits, Phase, Slot, StartBreaker};
use cf_vmhost::{JobRequest, JobState, OutOfMemory, Stage};

use crate::docker_check::{self, DockerCheck};
use crate::ends::{EndKind, machine_end};
use crate::making::{Making, Offered, theirs};
use crate::names::job_name;
use crate::records::{Disagrees, Ending, GithubJob, RULES, Record, Store, TestRunFacts, Upgrade};
use crate::roster::{JobInfo, Roster, Runner};
use crate::test_run::{self, StartError, TestRun};
use crate::traits::{
    Claim, EventKind, Events, HostError, Hosts, JobEvent, Refusal, RunnerSpec, Slots, Ticket,
};
use crate::vms::{Seen, Vms};
use crate::{secs, time_of};

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
    /// This start of the Mac, by a name that is another after every start and the same until then (not the time
    /// of the start, which moves with the clock): a record from another start has no machine.
    pub boot: String,
    /// What runs in place of GitHub's Actions runner: the integration suite's command. None in service.
    pub stand_in: Option<Workload>,
    /// The most Docker jobs (`cf-docker`) at once: a daemon and its builds take the Mac's processors more than
    /// other jobs do.
    pub docker_max_jobs: usize,
}

impl MachineConfig {
    /// The Runner's own waits for jobs of at most `deadline`: the vm-host keeps the deadline itself, so the
    /// lifecycle's own limit is a backstop past the vm-host's longest run.
    pub fn new(host: &str, image: &str, boot: &str, deadline: Duration) -> MachineConfig {
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
            boot: boot.to_string(),
            stand_in: None,
            docker_max_jobs: 1,
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
    /// The worst of each stall the job had inside its machine.
    pub pressure_peak: Option<Pressure>,
    /// The worst wait of the machine's vCPUs for the host's CPUs.
    pub host_cpu_peak: Option<Psi>,
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

/// What GitHub answered about an Actions runner's registration ([`Machines::answered`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// 204 to the Runner's removal: GitHub removed it, which it does only for one that holds no job.
    Removed,
    /// 404 to the removal, or to the REST view: it was gone already.
    Gone,
    /// 422 to the removal: GitHub refused it, for the job it holds.
    Refused,
    /// The REST view says it is busy: it holds a job.
    Busy,
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
    /// Its size; none for a record that names none the Runner knows.
    pub size: Option<Size>,
    /// The machine's memory.
    pub memory_bytes: u64,
    /// What the budget's cap counts it with while it holds a slot: the size of that slot. The vm-host's overhead is
    /// taken out of the cap already, so these add up to the cap on a full host.
    pub counted_bytes: Option<u64>,
    /// The memory the job holds now.
    pub used_bytes: Option<u64>,
    /// The part of it that is page cache.
    pub file_bytes: Option<u64>,
    /// The share of its CPUs the job kept busy over about the last 10 s, in percent.
    pub cpu_percent: Option<u32>,
    /// What the job stalled on over the last 10 s, inside its machine.
    pub pressure: Option<Pressure>,
    /// How long the machine's vCPUs waited for the host's CPUs over the last 10 s.
    pub host_cpu: Option<Psi>,
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
    /// What one more machine of each size meets, in the order of [`Size::ALL`].
    pub rooms: [SizeRoom; 4],
}

impl Snapshot {
    /// What one more machine of this size meets.
    pub fn room(&self, size: Size) -> &SizeRoom {
        &self.rooms[size.index()]
    }

    /// What keeps one more machine of this size from starting although the Mac takes jobs and has slots left, in
    /// the budget's word: `memory`, when what else runs on the Mac leaves too little. Nothing while a machine fits,
    /// while the Mac is closed (the budget then names why it is closed: a Mac that must stop a job is also under
    /// pressure or short of disk), and while its own machines use up its slots or the memory a further one needs
    /// (they are there to see).
    pub fn short_of(&self, size: Size) -> Option<&'static str> {
        self.room(size).why.filter(|why| *why == "memory")
    }
}

/// What one more machine of a size meets on the Mac now ([`Machines::rooms`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizeRoom {
    /// The size.
    pub size: Size,
    /// How many more fit now; `None` for a size these settings never run.
    pub free: Option<u32>,
    /// Why none fits, in the budget's word, while none does.
    pub why: Option<&'static str>,
    /// For `memory`, and `full` for memory the Mac's own machines hold (not the cap): what the Mac must have
    /// available for one more, and how much of that it lacks.
    pub need_bytes: Option<u64>,
    /// See `need_bytes`.
    pub short_bytes: Option<u64>,
}

/// How soon after a look told the listeners that the Mac's room changed a look tells them again. Room that comes
/// and goes with the Mac's memory ends every poll in flight each time; this is how often at most.
const TELL_AGAIN_AFTER: Millis = Millis::secs(5);

/// How soon after the log said that no machine of a size fits it may say so again: memory that comes and goes
/// writes a line a minute at most.
const SAY_AGAIN_AFTER: Millis = Millis::mins(1);

/// What [`Offer::short`] holds while a size is offered a place in line instead of a machine.
const PLACE: &str = "place in line";

/// What the polls in flight offer GitHub. A poll carries the room its scale set had when it began, for most of a
/// minute, and GitHub assigns against that number without asking again. A slot that is gone meanwhile, taken by
/// another scale set or by whatever else runs on the Mac, must end the polls that still offer it, or its job is
/// assigned here and waits in line while another Mac has room; room that came back must end the polls that offer
/// none, or its job waits at GitHub while this Mac is idle.
///
/// Many scale sets of one size each have a poll of their own, and they do not all ask at the same moment. So what
/// is kept is not the last answer one of them got, but every answer some poll may still carry: one that asks again
/// by itself and hears what is true now changes nothing for the others.
#[derive(Default)]
struct Offer {
    /// The sizes of which a listener heard that a machine fits, since every bell last rang.
    room: [bool; 4],
    /// The sizes of which a listener heard that none fits, since every bell last rang.
    none: [bool; 4],
    /// When a look last told the listeners.
    told: Option<Millis>,
    /// What the log last said keeps a machine of each size from starting on the open Mac, or [`PLACE`]; nothing
    /// while one fits.
    short: [Option<&'static str>; 4],
    /// When the log last said so, which stays when a machine fits again: it is what [`SAY_AGAIN_AFTER`] counts
    /// from.
    said: [Option<Millis>; 4],
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
    /// The end a restored slot is reported with, when its machine was not there any more.
    restored_end: Option<EndKind>,
    /// What the log said of what the machine holds.
    hold_said: HoldSaid,
}

/// What the log said of what a machine holds of the Mac's memory, so that each is said once until it changes.
#[derive(Clone, Copy, Debug, Default)]
struct HoldSaid {
    /// That it is not known.
    unknown: bool,
    /// That it is more than the machine is counted with.
    over: bool,
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
    /// An update or new settings wait for the machines to end: no new job, though the Runner goes on.
    holding: bool,
    /// Nobody listens to GitHub any more (an uninstall closed every session): how a job ended is not waited for.
    deaf: bool,
    host_error: Option<String>,
    stuck: BTreeSet<String>,
    /// Actions runners that lost a process for memory and are gone.
    recent_oom: VecDeque<String>,
    /// How long the Mac had slept in all when the machines were last told of its sleep.
    asleep: Millis,
    /// When the Mac was last looked at.
    looked: Option<Millis>,
    /// Whether the Mac took new jobs at the last look, and the budget's word for why not: a change is a line in
    /// the log.
    door: Option<(bool, &'static str)>,
    /// What the polls in flight may be offering GitHub.
    offer: Offer,
    /// For which size room is made.
    making: Making,
    /// The slots of Docker jobs; one given back leaves at the next count.
    docker: BTreeSet<SlotId>,
    /// What the last Docker check found; none before the first ended.
    docker_check: Option<DockerCheck>,
    /// When the last check's machine ended without an answer ([`DockerCheck::of_end`] said none), in seconds since
    /// 1970; none once one answered.
    docker_unanswered: Option<i64>,
}

impl<H> Inner<H> {
    /// How many slots Docker jobs hold: promised, waiting in line, or under a machine (whose grant goes when the
    /// machine is gone). Those given back are forgotten.
    fn docker_held(&mut self) -> usize {
        let Inner {
            docker,
            grants,
            waiting,
            ..
        } = self;
        docker.retain(|slot| grants.contains_key(slot) || waiting.contains(slot));
        docker.len()
    }

    /// A machine of `size` got its slot: room made for it is made.
    fn started(&mut self, size: Size) {
        self.making.started(&mut self.budget, size);
    }
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
        Phase::Idle | Phase::Asking => "idle",
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
    /// as `guest-lost` otherwise. A vm-host that is there and silent is killed first, and so is one that has written
    /// nothing yet. What is in the state folder without a record is nobody's job: killed and removed.
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
                looked: None,
                door: None,
                offer: Offer::default(),
                making: Making::default(),
                docker: BTreeSet::new(),
                // A failure the Runner before found holds until a check passes: nothing Docker comes meanwhile. A
                // pass is not kept, so Docker is unknown until this start's own check ends.
                docker_check: store.docker_failure(),
                docker_unanswered: None,
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
                holding: false,
                deaf: false,
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
                self.vms.kill(&job);
                self.vms.clean(&job);
            }
        }
        for record in records {
            let record = match record.upgrade() {
                Ok((record, Upgrade::Current)) => record,
                Ok((record, Upgrade::Upgraded)) => {
                    tracing::info!(runner = %record.runner, "a record of an older Runner was read under these rules");
                    self.store.save(&record);
                    record
                }
                // A mistake of the Runner's own: said aloud, and the record taken as it was written.
                Err(Disagrees(record)) => {
                    tracing::error!(runner = %record.runner, ?record, "a record of these rules disagrees with itself");
                    *record
                }
            };
            let Some(saved) = record.slot else {
                if record.unregistered {
                    self.store.remove(&record.job);
                } else {
                    inner.intents.insert(record.runner.clone(), (record, true));
                }
                continue;
            };
            let name = record.job.clone();
            let mut saved = saved.saved();
            if record.ended.is_some() {
                let (life, _) = Slot::restore(self.config.limits, now.elapsed, saved, false);
                self.vms.clean(&name);
                inner.jobs.insert(
                    name.clone(),
                    Job {
                        record,
                        slot: None,
                        life,
                        last: None,
                        ended_at: Some(now.elapsed),
                        restored_end: None,
                        hold_said: HoldSaid::default(),
                    },
                );
                self.settle(&mut inner, &name, now);
                continue;
            }
            let mut seen = self.vms.look(&name);
            // One that wrote nothing may still be there, hanging before its first word: it would boot a machine
            // nobody counts if it ever went on.
            if matches!(seen, Seen::Silent(_) | Seen::Nothing) {
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
                    if record.docker {
                        inner.docker.insert(grant.slot);
                    }
                    Some(grant.slot)
                }
                _ => None,
            };
            // The vm-host saw the guest up although the Runner before did not hear it.
            saved.up |= state.as_ref().is_some_and(came_up);
            let (life, effects) = Slot::restore(self.config.limits, now.elapsed, saved, alive);
            let restored_end = (!alive).then(|| match state.as_ref().and_then(|s| s.end.as_ref()) {
                Some(ended) => machine_end(saved.end.unwrap_or(End::Ok), Some(ended.end), false),
                None if !same_boot => EndKind::HostShutdown,
                None if matches!(saved.phase, Phase::Reserved | Phase::Starting | Phase::Retrying) => {
                    EndKind::DaemonRestart
                }
                None => EndKind::GuestLost,
            });
            // A registration that is gone takes no job, and its machine no longer counts.
            let done = alive && record.unregistered;
            if alive && record.local() {
                // No Actions runner, so nothing for the scalers to count: its slot is the budget's alone.
                tracing::info!(runner = %record.runner, job = %name, "a machine of the Runner's own before was adopted");
            } else if alive {
                tracing::info!(runner = %record.runner, job = %name, "a machine of the Runner before was adopted");
                self.roster.adopt(Runner {
                    name: record.runner.clone(),
                    handle: name.clone(),
                    runner_id: record.runner_id.unwrap_or(0),
                    repo: record.repo.clone(),
                    scale_set_id: record.scale_set_id,
                    ticket: slot,
                    started: time_of(record.created_at),
                    busy: record.github.is_some(),
                    adopted: true,
                    done: done.then(|| self.clock.now()),
                    job: record.github.as_ref().map(JobInfo::from),
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
                    restored_end,
                    hold_said: HoldSaid::default(),
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
        inner.budget.hold(Hold::Draining, inner.draining || inner.holding);
        inner.making.check(&mut inner.budget, now);
    }

    /// Takes new jobs or not, as the console or the person asks. The polls in flight are told, since each still
    /// offers GitHub the slot the Mac had when it began.
    pub fn pause(&self, paused: bool) {
        let changed = std::mem::replace(&mut self.lock().paused, paused) != paused;
        if changed {
            self.ring();
        }
    }

    /// Takes no new job while an update or new settings wait for the machines that stand to end, or takes them
    /// again when the wait gave up. Unlike [`Machines::drain`], a machine that ends meanwhile ends as it would
    /// otherwise. The polls in flight are told.
    pub fn hold_for_restart(&self, on: bool) {
        let changed = std::mem::replace(&mut self.lock().holding, on) != on;
        if changed {
            self.ring();
        }
    }

    /// Takes no new job from now on: the Runner is about to stop. The polls in flight are told.
    pub fn drain(&self) {
        let changed = !std::mem::replace(&mut self.lock().draining, true);
        if changed {
            self.ring();
        }
    }

    /// GitHub's word of how a job ended is not waited for from now on: an uninstall closed every session, so none
    /// can come. A job that ends is reported at once with what the Mac knows.
    pub fn hear_no_more(&self) {
        self.lock().deaf = true;
    }

    /// New numbers for the budget.
    pub fn set_settings(&self, settings: Settings) -> Result<(), cf_budget::SettingsError> {
        self.lock().budget.set_settings(settings)
    }

    /// Reads the Mac, lets the budget and every machine's lifecycle see the time that passed, and does what they
    /// say. Call it every second.
    pub fn tick(&self) {
        let mut inner = self.lock();
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
        // After the reading, never before it: the budget takes a reading from later than `now` for one of another
        // moment and closes the Mac for the calm time, and a real clock moves on while the Mac is read.
        let now = inner.host.now();
        self.holds(&mut inner, now);
        self.woke(&mut inner, now);
        // The budget forgets what it knew of a Mac nobody looked at for so long, and opens it again only after
        // a calm minute. A Mac that stays closed for that is hard to explain without this line.
        let late = inner
            .looked
            .replace(now.elapsed)
            .map(|before| now.elapsed.since(before))
            .filter(|gap| *gap > inner.budget.settings().stale_after);
        if let Some(gap) = late {
            tracing::warn!(
                gap_ms = gap.0,
                "a look at the host came late; its calm time starts again"
            );
        }
        let observed = inner.budget.observe(now, reading);
        Self::door(&mut inner, now);
        let mut nudge = false;
        // What the listeners heard of the Mac's room and what is true now are compared at every look, so nothing
        // is owed to them from a look before: room that went and came back within the wait rings no bell.
        let may_tell = inner
            .offer
            .told
            .is_none_or(|told| now.elapsed.since(told) >= TELL_AGAIN_AFTER);
        if may_tell && Self::owed(&inner, now, false) {
            inner.offer.told = Some(now.elapsed);
            nudge = true;
        }
        for slot in observed.expired {
            inner.grants.remove(&slot);
            inner.breaker.unused();
            nudge = true;
        }
        for grant in observed.started {
            inner.started(grant.size);
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
                    tracing::warn!(job = %name, cause = ?eviction.cause, "the host needs room; a job is stopped");
                    self.feed(&mut inner, &name, now, Event::Evict(eviction.cause));
                }
                // A promise or a place in line holds no machine: giving it back is all there is to stop. No start
                // was tried on it, which a Mac closed for failed starts must know.
                None => {
                    let _ = inner.budget.release(eviction.slot);
                    inner.grants.remove(&eviction.slot);
                    inner.ready.remove(&eviction.slot);
                    inner.breaker.unused();
                }
            }
        }
        // A look may wait on a vm-host's socket for as long as one that hangs lets it. The lock is given up for the
        // looks, so that the listeners, which ask for slots and bring GitHub's word, never wait with them.
        let standing: Vec<String> = names
            .iter()
            .filter(|name| {
                inner
                    .jobs
                    .get(*name)
                    .is_some_and(|job| job.life.phase() != Phase::Released)
            })
            .cloned()
            .collect();
        drop(inner);
        let mut seen: BTreeMap<String, Seen> = standing
            .into_iter()
            .map(|name| {
                let seen = self.vms.look(&name);
                (name, seen)
            })
            .collect();
        let mut inner = self.lock();
        let now = inner.host.now();
        for name in &names {
            if let Some(seen) = seen.remove(name) {
                self.watch(&mut inner, name, now, seen);
            }
            self.feed(&mut inner, name, now, Event::Tick);
            nudge |= self.settle(&mut inner, name, now);
        }
        drop(inner);
        if nudge {
            self.ring();
        }
    }

    /// Whether a poll in flight may offer what is not true any more: a slot of a size of which no machine fits now
    /// and that has no place in line, or, unless only room that is `gone` is asked for (which is all that taking a
    /// slot can change), none of a size of which one fits or that has one. Whoever hears yes rings the bells
    /// ([`Machines::ring`]).
    fn owed(inner: &Inner<H>, now: Now, gone: bool) -> bool {
        Size::ALL.into_iter().any(|size| {
            let at = size.index();
            let fits = match inner.budget.fits(now, size) {
                Ok(()) => true,
                Err(why) => inner.making.place(&inner.budget, now, size, why),
            };
            (inner.offer.room[at] && !fits) || (!gone && inner.offer.none[at] && fits)
        })
    }

    /// Rings every listener's bell: each gives up its poll and asks again. What they heard before counts no more
    /// from here on, and is forgotten before the bells ring, so that an answer one of them gets right after is
    /// kept.
    fn ring(&self) {
        {
            let mut inner = self.lock();
            inner.offer.room = [false; 4];
            inner.offer.none = [false; 4];
        }
        (self.nudge)();
    }

    /// What one more machine of this size meets now: how many more fit, and when none does, why in the budget's
    /// word. A size these settings never run is `never`; memory the Mac's own machines hold, or have still to take,
    /// is `full`, not `memory`: they are there to see.
    fn room(inner: &Inner<H>, now: Now, status: &Status, size: Size) -> SizeRoom {
        let settings = inner.budget.settings();
        let mut room = SizeRoom {
            size,
            free: Some(status.free.of(size)),
            why: None,
            need_bytes: None,
            short_bytes: None,
        };
        if settings.can_run(size).is_err() {
            room.free = None;
            room.why = Some("never");
            return room;
        }
        match inner.budget.fits(now, size) {
            Ok(()) => {}
            Err(refusal @ cf_budget::Refusal::Memory { short_bytes }) => {
                room.why = Some(if theirs(settings, status, size) {
                    "full"
                } else {
                    refusal.name()
                });
                room.need_bytes = status
                    .reading
                    .map(|reading| reading.available_bytes.saturating_add(short_bytes));
                room.short_bytes = Some(short_bytes);
            }
            Err(refusal) => room.why = Some(refusal.name()),
        }
        room
    }

    /// [`Snapshot::short_of`], for a look that makes no snapshot.
    fn short(inner: &Inner<H>, now: Now, size: Size) -> Option<&'static str> {
        Self::room(inner, now, &inner.budget.status(now), size)
            .why
            .filter(|why| *why == "memory")
    }

    /// Writes a line when the Mac starts or stops taking new jobs, or stops for another reason than before. Without
    /// it a job that waits leaves nothing in the log, and nobody who reads it can tell whether the Runner listens.
    fn door(inner: &mut Inner<H>, now: Now) {
        let status = inner.budget.status(now);
        let open = status.level == Level::Open;
        // Only a Mac that is out of memory or disk is closed with nothing that refuses a new job by name.
        let why = status
            .closed_by
            .map_or(if open { "" } else { "out-of-room" }, |refusal| refusal.name());
        if inner.door.replace((open, why)) == Some((open, why)) {
            return;
        }
        match status.closed_by {
            _ if open => tracing::info!("the host is open: a new job gets a machine"),
            Some(cf_budget::Refusal::Calming { left }) => tracing::info!(
                why,
                opens_in_s = left.0.div_ceil(1000),
                "the host is closed: a new job waits"
            ),
            _ => tracing::info!(why, "the host is closed: a new job waits"),
        }
    }

    /// Tells the budget what a running machine holds of the Mac's memory: what its vm-host read of the process that
    /// runs it. The guest touches memory its job never counts (its kernel, its page cache) and gives none back, so the
    /// job's peak is only the least the machine holds: it counts when the vm-host could not read the process (or is
    /// of a Runner from before), and the rest of the machine's size stays still to come. Either way the log says once
    /// when what a machine whose guest is up holds is not known, or is more than the budget counts it with.
    fn count_held(inner: &mut Inner<H>, name: &str, slot: SlotId, now: Now, state: &JobState) {
        let counted = inner.budget.host_bytes(slot).ok();
        let resident = state.resident_bytes;
        if let Some(job) = inner.jobs.get_mut(name) {
            let said = &mut job.hold_said;
            // A machine on its way down may have no process left to read before its vm-host is gone.
            let unknown = resident.is_none() && matches!(state.stage, Stage::Running | Stage::Stopping);
            if unknown && !said.unknown {
                tracing::warn!(
                    job = name,
                    "what the host holds for a machine is not known: it counts by its job's peak"
                );
            }
            said.unknown = unknown;
            if let (Some(held), Some(counted)) = (resident, counted) {
                let over = held > counted;
                if over && !said.over {
                    tracing::warn!(
                        job = name,
                        held_mib = held >> 20,
                        counted_mib = counted >> 20,
                        "a machine holds more than it is counted with"
                    );
                }
                said.over = over;
            }
        }
        let peak = state
            .usage
            .as_ref()
            .map(|usage| usage.memory_peak_bytes.min(state.vm_bytes));
        let Some(held) = resident.or(peak) else {
            return;
        };
        let _ = inner.budget.usage(
            now,
            slot,
            Usage {
                resident_bytes: held,
                scratch_bytes: state.usage.as_ref().map_or(0, |usage| usage.disk_bytes),
            },
        );
    }

    /// Tells the lifecycle what a look at a job's vm-host showed. The look was taken without the lock, so the job
    /// may have moved on since: its phase is read here, and one that is gone by now is told nothing.
    fn watch(&self, inner: &mut Inner<H>, name: &str, now: Now, seen: Seen) {
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
                if let Some(slot) = slot {
                    Self::count_held(inner, name, slot, now, state);
                }
            }
            Seen::Gone(state) => {
                if state.end.is_some() {
                    // The vm-host saw the guest up, though the Runner had not yet.
                    if came_up(state) && phase == Phase::Starting {
                        events.push(Event::VmReady);
                    }
                    if state.oom.is_some() && !had_oom {
                        events.push(Event::OutOfMemory);
                    }
                    // The workload ran to its end, so the job is over whether GitHub's word of it came or not.
                    if workload_ran(state) {
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
        self.local_up(inner, name, now);
    }

    /// A machine of the Runner's own ([`Record::local`]: a test run's, the Docker check's) is up: its job starts there
    /// and then, since no Actions runner waits for one and GitHub never says anything of it. Without this the
    /// lifecycle would wait for a job, ask GitHub to remove a registration that never was, and stop the machine as one
    /// without a job.
    fn local_up(&self, inner: &mut Inner<H>, name: &str, now: Now) {
        let Some(job) = inner.jobs.get_mut(name) else {
            return;
        };
        if !job.record.local() || job.life.phase() != Phase::Idle {
            return;
        }
        let wall = secs(self.clock.now());
        let at = match job.record.test_run.as_mut() {
            Some(facts) => *facts.started_at.get_or_insert(wall),
            None => wall,
        };
        tracing::info!(runner = %job.record.runner, job = %name, "a machine of the Runner's own is up; its work starts");
        self.hear(inner, name, now, Event::JobStarted, at, true);
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

    /// Feeds the lifecycle an event, and writes the record when the lifecycle moved. Says whether it wrote.
    fn feed(&self, inner: &mut Inner<H>, name: &str, now: Now, event: Event) -> bool {
        self.woke(inner, now);
        self.step(inner, name, now, event)
    }

    fn step(&self, inner: &mut Inner<H>, name: &str, now: Now, event: Event) -> bool {
        let Some(job) = inner.jobs.get_mut(name) else {
            return false;
        };
        let before = job.life.saved();
        let effects = job.life.on(now.elapsed, event);
        let changed = job.life.saved() != before;
        self.apply(inner, name, now, effects);
        if changed && let Some(job) = inner.jobs.get_mut(name) {
            job.record.slot = Some(job.life.saved().into());
            self.store.save(&job.record);
            return true;
        }
        false
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
            pressure_peak: job.last.as_ref().and_then(|s| s.pressure_peak),
            host_cpu_peak: job.last.as_ref().and_then(|s| s.host_cpu_peak),
            message: vm_end.and_then(|e| e.message.clone()),
        });
        job.record.slot = Some(job.life.saved().into());
        job.ended_at = Some(now.elapsed);
        if job.record.probe {
            let at = self.wall();
            let console = || self.vms.console_tail(name, docker_check::CONSOLE_TAIL);
            match DockerCheck::of_end(job.last.as_ref(), kind, console, at) {
                None => {
                    tracing::info!(
                        end = kind.name(),
                        "the Docker check's machine ended without an answer; it boots again later"
                    );
                    inner.docker_unanswered = Some(at);
                }
                Some(check) => {
                    match &check.why {
                        None => tracing::info!("Docker starts in this host's job machines"),
                        Some(why) => {
                            tracing::warn!(%why, "Docker does not start in this host's job machines")
                        }
                    }
                    self.store
                        .keep_docker_failure(Some(&check).filter(|check| !check.ok));
                    inner.docker_check = Some(check);
                    inner.docker_unanswered = None;
                }
            }
        }
        let Some(job) = inner.jobs.get_mut(name) else {
            return;
        };
        let oom = job.record.oom;
        let runner = job.record.runner.clone();
        tracing::info!(%runner, job = %name, end = kind.name(), ran_job, "machine gone");
        self.roster.remove(&runner);
        self.store.save(&job.record);
        self.vms.clean(name);
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
        let deaf = inner.deaf;
        let Some(job) = inner.jobs.get_mut(name) else {
            return false;
        };
        let Some(ending) = job.record.ended.as_ref() else {
            return false;
        };
        let (kind, at) = (EndKind::from_name(&ending.kind), ending.at);
        let mut opened = false;
        // A test run or the Docker check has no word of GitHub's to wait for: the Mac knows all there is.
        let test_run = job.record.test_run.is_some();
        let local = job.record.local();
        if !job.record.end_reported {
            let waited = job.ended_at.map_or(Millis(u64::MAX), |at| now.elapsed.since(at));
            let result = job.record.github.as_ref().and_then(|g| g.result.clone());
            if !local && result.is_none() && !deaf && duration(waited) < RESULT_WAIT {
                return false;
            }
            // Whether a job ran is GitHub's to say, by its answer to the removal.
            if !local && job.record.unheard() && !deaf {
                return false;
            }
            if test_run && !job.record.start_reported {
                // Its machine never came up: the console still hears of the run that it ended, and a completion
                // needs a start.
                job.record.start_reported = true;
                self.reports.job(JobReport {
                    id: format!("{}/started", job.record.runner),
                    kind: EventKind::Started,
                    runner: job.record.runner.clone(),
                    github: reported_job(&job.record).unwrap_or_default(),
                    end: None,
                    out_of_memory: false,
                    at: job.record.created_at,
                    machine: facts(&job.record),
                });
            }
            if let Some(github) = reported_job(&job.record) {
                let base = kind.unwrap_or(EndKind::GuestLost);
                let end = match (test_run, base) {
                    // The machine ended by itself: the agent ran the suite and told the console how it went.
                    (true, EndKind::Finished) => EndKind::Success,
                    (true, base) => base,
                    (false, base) => base.with_result(result.as_deref()),
                };
                self.reports.job(JobReport {
                    id: format!("{}/completed", job.record.runner),
                    kind: EventKind::Completed,
                    runner: job.record.runner.clone(),
                    github,
                    end: Some(end),
                    out_of_memory: job.record.oom,
                    at,
                    machine: facts(&job.record),
                });
            }
            job.record.end_reported = true;
            // A record with nothing left to do is removed below, not written first.
            if !job.record.finished() {
                self.store.save(&job.record);
            }
            opened = true;
        }
        if job.record.finished() {
            self.store.remove(name);
            inner.jobs.remove(name);
        }
        opened
    }

    /// The registrations at GitHub that have to go: of Actions runners asking for their removal, of machines that
    /// are gone, and of Actions runners that were registered and never got a machine. Whoever holds the scale set
    /// clients removes them and says what GitHub answered with [`Machines::answered`].
    pub fn registrations_to_remove(&self) -> Vec<Registration> {
        let inner = self.lock();
        let now = inner.host.now();
        let of = |record: &Record| Registration {
            runner: record.runner.clone(),
            runner_id: record.runner_id,
            scale_set_id: record.scale_set_id,
            repo: record.repo.clone(),
        };
        inner
            .jobs
            .values()
            .filter(|job| !job.record.unregistered)
            .filter(|job| match job.life.phase() {
                Phase::Asking => true,
                // GitHub's word of a job that went unheard may still come: its messages say a job started up to a
                // minute late. Until the wait is over, the removal would only stand in for it.
                Phase::Released => {
                    let waited = job.ended_at.map_or(Millis(u64::MAX), |at| now.elapsed.since(at));
                    !job.record.unheard() || inner.deaf || duration(waited) >= RESULT_WAIT
                }
                _ => false,
            })
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

    /// What GitHub answered about an Actions runner's registration: to the Runner's removal of it, or when its REST
    /// view was asked (`docs/job-state.md`). The one place GitHub's answers move a machine and its job.
    ///
    /// - [`Answer::Removed`]: no job holds it, so none ran. A machine that stands stops as one without a job.
    /// - [`Answer::Gone`]: the registration was gone already. Under a VM that came up, its one job is over, since a
    ///   one-job Actions runner removes its own once its job is over; under one that never came up, no job ran.
    /// - [`Answer::Refused`], [`Answer::Busy`]: a job holds it. The registration stays, and is asked about again.
    ///
    /// A job only these answers name is stored as such ([`GithubJob::inferred`]) and reported started and ended once.
    pub fn answered(&self, runner: &str, answer: Answer) {
        let mut inner = self.lock();
        let now = inner.host.now();
        let unregistered = matches!(answer, Answer::Removed | Answer::Gone);
        if unregistered {
            self.forget_intent(&mut inner, runner);
        }
        let Some(name) = Self::job_of(&inner, runner) else {
            return;
        };
        let job = inner.jobs.get_mut(&name).expect("just found");
        let stands = job.record.ended.is_none();
        let (repo, set) = (job.record.repo.clone(), job.record.scale_set_id);
        let word = match answer {
            Answer::Removed => Event::Unregistered,
            Answer::Gone if job.record.up() || job.record.github.is_some() => Event::JobCompleted,
            Answer::Gone => Event::Unregistered,
            Answer::Refused | Answer::Busy => Event::JobStarted,
        };
        if unregistered {
            job.record.unregistered = true;
            if stands {
                self.roster.mark_done(&repo, set, runner, self.clock.now());
            }
        }
        let opened = if word == Event::Unregistered {
            self.land(&mut inner, &name, now, word, true)
        } else {
            // A machine that stands holds the job now; one that is gone, when it went.
            let at = job.record.ended.as_ref().map_or_else(|| self.wall(), |e| e.at);
            let inferred = job.record.github.is_none();
            if inferred {
                tracing::info!(%runner, ?answer, "GitHub's answer says the Actions runner took a job nobody heard of");
                let github = job.record.github.insert(GithubJob {
                    repo: repo.clone(),
                    started_at: Some(at),
                    inferred: true,
                    ..GithubJob::default()
                });
                if stands {
                    self.roster.mark_busy(&repo, set, runner, JobInfo::from(&*github));
                }
            }
            self.hear(&mut inner, &name, now, word, at, unregistered || inferred)
        };
        drop(inner);
        if opened {
            self.ring();
        }
    }

    /// A registration that is gone, or was never made, leaves no intent behind.
    fn forget_intent(&self, inner: &mut Inner<H>, runner: &str) {
        if let Some((record, _)) = inner.intents.remove(runner) {
            self.store.remove(&record.job);
        }
    }

    /// The handle of this Actions runner's machine, while it is this Mac's.
    fn job_of(inner: &Inner<H>, runner: &str) -> Option<String> {
        let name = job_name(runner);
        inner
            .jobs
            .get(name)
            .is_some_and(|job| job.record.runner == runner)
            .then(|| name.to_string())
    }

    /// A word of the job on this machine, from GitHub's messages or its answers, once what it says of the job is in
    /// the record (`changed`: the caller wrote something new there): the start is reported once, and the record and
    /// the lifecycle step the word causes are one write. Says whether a slot opened.
    fn hear(&self, inner: &mut Inner<H>, name: &str, now: Now, word: Event, at: i64, changed: bool) -> bool {
        let Some(job) = inner.jobs.get_mut(name) else {
            return false;
        };
        // Written with the lifecycle's step, which takes the same word as a job that ran.
        let mut changed = changed | job.record.ended.as_mut().is_some_and(Ending::job_ran);
        // The Docker check is no job: the console hears nothing of it.
        if !job.record.start_reported && !job.record.probe {
            changed = true;
            job.record.start_reported = true;
            // Settled as having had no job, by a path that could not wait for GitHub (an uninstall): no completion
            // went out, since one needs a reported start, so the end is reported once from here on.
            job.record.end_reported &= job.record.ended.is_none();
            self.reports.job(JobReport {
                id: format!("{}/started", job.record.runner),
                kind: EventKind::Started,
                runner: job.record.runner.clone(),
                github: reported_job(&job.record).unwrap_or_default(),
                end: None,
                out_of_memory: false,
                at,
                machine: facts(&job.record),
            });
        }
        self.land(inner, name, now, word, changed)
    }

    /// The lifecycle's step for a word, written with what changed in the record (`changed`) in one write, then
    /// what that settles. Says whether a slot opened.
    fn land(&self, inner: &mut Inner<H>, name: &str, now: Now, word: Event, changed: bool) -> bool {
        if !self.feed(inner, name, now, word)
            && changed
            && let Some(job) = inner.jobs.get(name)
        {
            self.store.save(&job.record);
        }
        self.settle(inner, name, now)
    }

    /// A record of a machine about to start on a grant, as [`Machines::launch`] writes it.
    fn record(
        &self,
        runner: &str,
        repo: &str,
        scale_set_id: i64,
        size: Size,
        grant: &Grant,
        request: &JobRequest,
    ) -> Record {
        Record {
            job: request.job.clone(),
            runner: runner.to_string(),
            runner_id: None,
            scale_set_id,
            repo: repo.to_string(),
            size: Some(size.label().to_string()),
            docker: request.docker,
            memory_bytes: grant.vm_bytes,
            limit_bytes: grant.job_limit_bytes,
            cpus: grant.cpus,
            origin: grant.origin.name().to_string(),
            slot: None,
            created_at: self.wall(),
            boot: self.config.boot.clone(),
            unregistered: false,
            github: None,
            start_reported: false,
            ended: None,
            end_reported: false,
            oom: false,
            rules: RULES,
            test_run: None,
            probe: false,
        }
    }

    /// Starts a vm-host for `request` on the slot `ticket` grants, with `record` written first: a Runner that dies
    /// right here finds the record and no machine. After an error no machine stands and the slot is still the
    /// caller's.
    fn launch(
        &self,
        inner: &mut Inner<H>,
        now: Now,
        ticket: Ticket,
        mut record: Record,
        request: &JobRequest,
    ) -> Result<(), HostError> {
        self.woke(inner, now);
        let mut life = Slot::reserved(self.config.limits);
        let effects = life.on(now.elapsed, Event::Assigned);
        debug_assert_eq!(effects, [Effect::StartVm]);
        record.slot = Some(life.saved().into());
        // Written before the vm-host exists: a Runner that dies right here finds the record and no machine.
        self.store.save(&record);
        if let Err(error) = self.vms.start(request) {
            inner.breaker.failed(now.elapsed);
            self.vms.clean(&request.job);
            return Err(HostError(error.to_string()));
        }
        inner.ready.remove(&ticket);
        inner.jobs.insert(
            request.job.clone(),
            Job {
                record,
                slot: Some(ticket),
                life,
                last: None,
                ended_at: None,
                restored_end: None,
                hold_said: HoldSaid::default(),
            },
        );
        Ok(())
    }

    /// Starts a machine of the Runner's own ([`Record::local`]) on the slot `ticket` grants: the image CI jobs run in,
    /// `own`'s workload, and no Actions runner. Its record, which `mark` tells apart (a test run's facts, the Docker
    /// check's flag), has no registration at GitHub from the start, so the sweep never asks GitHub anything of it.
    /// After an error no machine stands, no record is left of it, and the slot is still the caller's.
    fn start_own(
        &self,
        inner: &mut Inner<H>,
        now: Now,
        ticket: Ticket,
        grant: &Grant,
        own: Own<'_>,
        mark: impl FnOnce(&mut Record),
    ) -> Result<(), HostError> {
        let request = JobRequest {
            job: job_name(own.runner).to_string(),
            host: self.config.host.clone(),
            memory_bytes: grant.vm_bytes,
            cpus: grant.cpus,
            scratch_bytes: self.config.scratch_bytes,
            image: self.config.image.clone(),
            workload: own.workload,
            env: own.env,
            deadline_s: own.deadline.as_secs(),
            docker: own.docker,
        };
        let mut record = Record {
            unregistered: true,
            ..self.record(own.runner, own.repo, test_run::SET, own.size, grant, &request)
        };
        mark(&mut record);
        self.launch(inner, now, ticket, record, &request)
            .inspect_err(|_| {
                // Nothing stands and nothing is registered: no record is left of it.
                self.store.remove(&request.job);
            })
    }

    /// Starts a test run's machine on the slot `ticket` grants ([`test_run::admit`]): the image CI jobs run in, the
    /// agent as its workload, the run's address and token in its environment, and no Actions runner. Its record has
    /// no registration at GitHub from the start, so the sweep never asks GitHub anything of it. After an error no
    /// machine stands and the slot is still the caller's.
    pub fn start_test_run(&self, ticket: Ticket, run: &TestRun) -> Result<String, StartError> {
        let mut inner = self.lock();
        let now = inner.host.now();
        let runner = run.runner();
        let name = job_name(&runner).to_string();
        // The Mac may have stopped taking jobs since the slot was given (a wait for room ends while it is paused, or
        // an update holds): a run is not a job it has to take.
        if inner.paused || inner.draining || inner.holding {
            return Err(StartError::Closed);
        }
        let failed = |why: String| Err(StartError::Failed(HostError(why)));
        if inner.jobs.contains_key(&name) {
            return failed(format!("a machine named {runner} is there already"));
        }
        let Some(grant) = inner.grants.get(&ticket).copied() else {
            return failed(format!("slot {} is not granted", ticket.0));
        };
        let own = Own {
            runner: &runner,
            repo: &run.repo,
            size: run.size,
            workload: test_run::workload(),
            env: run.env(self.config.deadline),
            deadline: run.deadline(self.config.deadline),
            docker: run.docker,
        };
        let test_run = TestRunFacts {
            label: run.label.clone(),
            started_at: None,
        };
        self.start_own(&mut inner, now, ticket, &grant, own, |record| {
            record.test_run = Some(test_run);
        })
        .map_err(StartError::Failed)?;
        // A Docker run's slot counts against the Mac's Docker jobs since `test_run::admit` took it.
        tracing::info!(%runner, size = run.size.label(), repo = %run.repo, "a test run's machine starts");
        Ok(runner)
    }

    /// Boots the Docker check's machine ([`crate::docker_check`]) in a slot of `size` the budget grants as it would a
    /// claim, so the check never takes the host's room from what it protects, nor runs while the Mac takes no new
    /// job (paused, draining, holding for an update, someone at it). Says the machine's name, or why none started.
    pub fn start_docker_probe(&self, size: Size) -> Result<String, String> {
        let ticket = self.reserve_for(size, true).map_err(|refusal| refusal.0)?;
        let mut inner = self.lock();
        let now = inner.host.now();
        let runner = format!("dk-p{:07x}", rand::random::<u32>() & 0x0fff_ffff);
        let started = match (inner.budget.confirm(ticket), inner.grants.get(&ticket).copied()) {
            (Ok(()), Some(grant)) => {
                let own = Own {
                    runner: &runner,
                    repo: "",
                    size,
                    workload: docker_check::workload(),
                    env: Vec::new(),
                    deadline: docker_check::DEADLINE,
                    docker: true,
                };
                self.start_own(&mut inner, now, ticket, &grant, own, |record| record.probe = true)
                    .map_err(|error| error.to_string())
            }
            _ => Err("its slot ran out".to_string()),
        };
        drop(inner);
        if let Err(why) = started {
            self.release(ticket);
            return Err(why);
        }
        tracing::info!(%runner, size = size.label(), "the Docker check's machine starts");
        Ok(runner)
    }

    /// What the last Docker check found; none before the first ended.
    pub fn docker_check(&self) -> Option<DockerCheck> {
        self.lock().docker_check.clone()
    }

    /// Why nothing Docker comes to this host ([`DockerCheck::refusal`]): its last Docker check failed.
    pub fn docker_refusal(&self) -> Option<String> {
        DockerCheck::refusal(self.lock().docker_check.as_ref())
    }

    /// Whether a Docker check is due at `now` (seconds since 1970): [`DockerCheck::due`], and not within
    /// [`docker_check::AGAIN_AFTER_NO_ANSWER`] of a check machine that ended without an answer.
    pub fn docker_check_due(&self, now: i64) -> bool {
        let inner = self.lock();
        DockerCheck::due(inner.docker_check.as_ref(), now)
            && inner
                .docker_unanswered
                .is_none_or(|at| docker_check::waited(at, now, docker_check::AGAIN_AFTER_NO_ANSWER))
    }

    /// Whether the Docker check's machine stands.
    pub fn probing(&self) -> bool {
        self.lock()
            .jobs
            .values()
            .any(|job| job.record.probe && job.record.ended.is_none())
    }

    /// The test run on this Mac whose machine stands, by its name; none while there is none.
    pub fn test_run_on(&self) -> Option<String> {
        self.lock()
            .jobs
            .values()
            .find(|job| job.record.test_run.is_some() && job.record.ended.is_none())
            .map(|job| job.record.runner.clone())
    }

    /// Whether this Actions runner, or test run, still has its machine here or a report to make.
    pub fn knows(&self, runner: &str) -> bool {
        Self::job_of(&self.lock(), runner).is_some()
    }

    /// Whether an Actions runner that is gone had lost a process for memory.
    pub fn was_out_of_memory(&self, runner: &str) -> bool {
        self.lock().recent_oom.iter().any(|name| name == runner)
    }

    /// Asks a machine to stop, as the console's stop does; one that is gone already is stopped.
    pub fn stop(&self, handle: &str) {
        let mut inner = self.lock();
        let now = inner.host.now();
        self.feed(&mut inner, handle, now, Event::Stop);
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

    /// How many jobs may still get a machine although no new slot is granted: one for each slot promised to GitHub,
    /// or granted, with no machine yet. A registration under way still holds its slot, so it counts here and in
    /// [`Machines::unsettled`] for those few seconds. A job that waits in line gets no machine while no slot is
    /// granted: if the Runner ends, GitHub takes it back and hands it out again, as it would anyway.
    pub fn coming(&self) -> usize {
        let inner = self.lock();
        let held: BTreeSet<SlotId> = inner.jobs.values().filter_map(|job| job.slot).collect();
        inner.grants.keys().filter(|slot| !held.contains(slot)).count()
    }

    /// What there is to report.
    pub fn snapshot(&self) -> Snapshot {
        let mut inner = self.lock();
        let now = inner.host.now();
        self.holds(&mut inner, now);
        let budget = inner.budget.status(now);
        let rooms = Size::ALL.map(|size| Self::room(&inner, now, &budget, size));
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
                    size: job.record.size(),
                    memory_bytes: job.record.memory_bytes,
                    counted_bytes: job.slot.and_then(|slot| {
                        budget
                            .slots
                            .iter()
                            .find(|held| held.slot == slot)
                            .map(|held| held.size.vm_bytes())
                    }),
                    used_bytes: job
                        .last
                        .as_ref()
                        .and_then(|s| s.usage.as_ref())
                        .map(|u| u.memory_bytes),
                    file_bytes: job
                        .last
                        .as_ref()
                        .and_then(|s| s.usage.as_ref())
                        .map(|u| u.memory_file_bytes),
                    // A load in hundredths of a CPU over the CPUs is the percent of them; a heartbeat that came late
                    // can make it a little more, which no machine has.
                    cpu_percent: job
                        .last
                        .as_ref()
                        .and_then(|s| s.cpu_load?.checked_div(s.cpus))
                        .map(|percent| percent.min(100)),
                    pressure: job.last.as_ref().and_then(|s| s.usage.as_ref()?.pressure),
                    host_cpu: job.last.as_ref().and_then(|s| s.host_cpu),
                    github: reported_job(&job.record),
                })
                .collect(),
            host_error: inner.host_error.clone(),
            stuck: inner.stuck.iter().cloned().collect(),
            waiting: inner.waiting.len(),
            rooms,
            budget,
        }
    }

    /// The longest a job may run.
    pub fn deadline(&self) -> Duration {
        self.config.deadline
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

/// Whether the vm-host says the workload ran to its end: then the guest was up, whatever else it wrote.
fn workload_ran(state: &JobState) -> bool {
    use cf_vmhost::End as VmEnd;
    state
        .end
        .as_ref()
        .is_some_and(|ended| matches!(ended.end, VmEnd::Ok | VmEnd::Oom | VmEnd::Failed))
}

/// Whether the vm-host saw the guest up: it timed the boot, or the workload ran to its end. A look and the Runner
/// that starts again read it the same way.
fn came_up(state: &JobState) -> bool {
    state.boot_ms.is_some() || workload_ran(state)
}

/// What a machine of the Runner's own ([`Machines::start_own`]) runs, and for whom.
struct Own<'a> {
    runner: &'a str,
    repo: &'a str,
    size: Size,
    workload: Workload,
    env: Vec<(String, cf_guest_proto::Secret<String>)>,
    deadline: Duration,
    docker: bool,
}

/// The job a machine's reports and status name: GitHub's, as its messages told it, or a test run's, which the Runner
/// tells itself. A test run that ended by itself is a `success` whatever its suite did: the agent tells the console
/// how the suite went, and the Runner only how the machine did.
fn reported_job(record: &Record) -> Option<GithubJob> {
    if record.probe {
        return None;
    }
    let Some(facts) = &record.test_run else {
        return record.github.clone();
    };
    let ended = record.ended.as_ref();
    Some(GithubJob {
        repo: record.repo.clone(),
        job: facts.label.clone(),
        workflow: test_run::WORKFLOW.to_string(),
        run_id: 0,
        event: test_run::EVENT.to_string(),
        queued_at: Some(record.created_at),
        started_at: Some(facts.started_at.unwrap_or(record.created_at)),
        result: ended
            .filter(|ending| EndKind::from_name(&ending.kind) == Some(EndKind::Finished))
            .map(|_| EndKind::Success.name().to_string()),
        completed_at: ended.map(|ending| ending.at),
        inferred: false,
    })
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
        pressure_peak: ended.and_then(|e| e.pressure_peak),
        host_cpu_peak: ended.and_then(|e| e.host_cpu_peak),
    }
}

/// Why a Docker job's slot is refused: as many Docker jobs as the Mac allows hold one.
pub const DOCKER_FULL: &str = "docker-full";

impl<H: Host + Send, V: Vms> Slots for Machines<H, V> {
    fn reserve(&self, size: Size) -> Result<Ticket, Refusal> {
        self.reserve_for(size, false)
    }

    fn reserve_docker(&self, size: Size) -> Result<Ticket, Refusal> {
        self.reserve_for(size, true)
    }

    fn reserve_for(&self, size: Size, docker: bool) -> Result<Ticket, Refusal> {
        let mut inner = self.lock();
        if docker && inner.docker_held() >= self.config.docker_max_jobs {
            return Err(Refusal(DOCKER_FULL.to_string()));
        }
        let now = inner.host.now();
        self.holds(&mut inner, now);
        if !inner.breaker.admit(now.elapsed) {
            return Err(Refusal(Hold::StartFailures.name().to_string()));
        }
        match inner.budget.claim(now, size) {
            Ok(grant) => {
                inner.started(size);
                inner.grants.insert(grant.slot, grant);
                if docker {
                    inner.docker.insert(grant.slot);
                }
                let told = Self::owed(&inner, now, true);
                drop(inner);
                if told {
                    self.ring();
                }
                Ok(grant.slot)
            }
            Err(refusal) => {
                inner.breaker.unused();
                Err(Refusal(refusal.name().to_string()))
            }
        }
    }

    fn mark_docker(&self, ticket: Ticket) {
        let mut inner = self.lock();
        inner.docker.insert(ticket);
        if inner.docker_held() > self.config.docker_max_jobs {
            tracing::info!(
                held = inner.docker.len(),
                "GitHub assigned a Docker job past the cap; it runs as any assigned job"
            );
        }
    }

    fn docker_room(&self) -> usize {
        self.config
            .docker_max_jobs
            .saturating_sub(self.lock().docker_held())
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
            self.ring();
        }
    }

    fn claim_in_turn(&self, size: Size, promised: Option<Ticket>, turn: Turn) -> Claim {
        let mut inner = self.lock();
        let now = inner.host.now();
        self.holds(&mut inner, now);
        if let Some(ticket) = promised {
            if inner.budget.confirm_in_turn(ticket, &turn).is_ok() {
                return Claim::Start(ticket);
            }
            inner.grants.remove(&ticket);
        }
        let claim = match inner.budget.assign_in_turn(now, size, turn) {
            Assigned::Start(grant) => {
                inner.started(size);
                inner.breaker.admit(now.elapsed);
                inner.grants.insert(grant.slot, grant);
                Claim::Start(grant.slot)
            }
            Assigned::Waits { slot, why } => {
                inner.waiting.insert(slot);
                // Room is made for it, if it is one to make room for, before another job asks.
                self.holds(&mut inner, now);
                Claim::Waits {
                    ticket: slot,
                    why: why.name().to_string(),
                }
            }
            Assigned::Never(refusal) => return Claim::Never(refusal.name().to_string()),
        };
        // The slot this job took, or the next one that opens and is now the waiting job's, is one the other scale
        // sets' polls may still offer.
        let told = Self::owed(&inner, now, true);
        drop(inner);
        if told {
            self.ring();
        }
        claim
    }

    fn set_turn(&self, ticket: Ticket, turn: Turn) {
        let _ = self.lock().budget.set_turn(ticket, turn);
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
            // Room made for its size is now for a job GitHub took back: from now on, not from the next look.
            let now = inner.host.now();
            self.holds(&mut inner, now);
        } else if inner.ready.remove(&ticket) {
            inner.grants.remove(&ticket);
            let _ = inner.budget.release(ticket);
            inner.breaker.unused();
            drop(inner);
            self.ring();
        }
    }

    fn open_slots(&self, size: Size) -> usize {
        let mut inner = self.lock();
        let now = inner.host.now();
        self.holds(&mut inner, now);
        let Inner { making, budget, .. } = &mut *inner;
        let offered = making.offer(budget, now, size);
        let at = size.index();
        match offered {
            Offered::Free(free) => {
                inner.offer.room[at] = true;
                if inner.offer.short[at].take().is_some() {
                    tracing::info!(size = size.label(), "a machine of this size fits again");
                }
                return free as usize;
            }
            Offered::Place => {
                inner.offer.room[at] = true;
                if inner.offer.short[at].replace(PLACE) != Some(PLACE) {
                    tracing::info!(
                        size = size.label(),
                        "no machine of this size fits now, but a job of it gets a place in line: the smaller ones \
                         wait for it"
                    );
                }
                return 1;
            }
            Offered::Nothing => {}
        }
        inner.offer.none[at] = true;
        // A closed Mac said why when it closed. An open one that has no room for this size takes none of its jobs
        // and would say nothing: GitHub keeps them for another Mac, or until there is room here.
        let may_say = inner.offer.said[at].is_none_or(|said| now.elapsed.since(said) >= SAY_AGAIN_AFTER);
        if may_say
            && let Some(why) = Self::short(&inner, now, size)
            && inner.offer.short[at] != Some(why)
        {
            inner.offer.short[at] = Some(why);
            inner.offer.said[at] = Some(now.elapsed);
            tracing::info!(
                size = size.label(),
                why,
                "the host is open, but no machine of this size fits now: its jobs stay at GitHub"
            );
        }
        0
    }

    fn held(&self) -> bool {
        let inner = self.lock();
        inner.paused || inner.draining || inner.holding
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
            docker: false,
            memory_bytes: 0,
            limit_bytes: 0,
            cpus: 0,
            origin: String::new(),
            slot: None,
            created_at: self.wall(),
            boot: self.config.boot.clone(),
            unregistered: false,
            github: None,
            start_reported: false,
            ended: None,
            end_reported: true,
            oom: false,
            rules: RULES,
            test_run: None,
            probe: false,
        };
        self.store.save(&record);
        self.lock().intents.insert(name.to_string(), (record, false));
    }

    fn not_registered(&self, name: &str) {
        self.forget_intent(&mut self.lock(), name);
    }

    fn registration_unsure(&self, name: &str) {
        if let Some((_, stale)) = self.lock().intents.get_mut(name) {
            *stale = true;
        }
    }

    fn retire(&self, repo: &str, scale_set_id: i64, n: usize, idle: &[String]) {
        let mut inner = self.lock();
        let asking = inner
            .jobs
            .values()
            .filter(|job| job.record.is_of(repo, scale_set_id) && job.life.phase() == Phase::Asking)
            .count();
        let more = n.saturating_sub(asking);
        if more == 0 {
            return;
        }
        let now = inner.host.now();
        let names: Vec<String> = idle
            .iter()
            .filter(|runner| {
                inner.jobs.get(job_name(runner)).is_some_and(|job| {
                    job.record.is_of(repo, scale_set_id)
                        && job.record.runner == **runner
                        && job.life.phase() == Phase::Idle
                })
            })
            .take(more)
            .map(|runner| job_name(runner).to_string())
            .collect();
        for name in names {
            tracing::info!(job = %name, "surplus Actions runner retired: its removal is asked for");
            self.feed(&mut inner, &name, now, Event::Retire);
        }
    }

    fn lost_with_job(&self, repo: &str, scale_set_id: i64) -> usize {
        self.lock()
            .jobs
            .values()
            .filter(|job| {
                let record = &job.record;
                record.is_of(repo, scale_set_id)
                    && record.ended.is_some()
                    && !record.unregistered
                    && match &record.github {
                        Some(github) => github.result.is_none(),
                        // Its start may have gone unheard: GitHub may count its job as assigned until it answered.
                        None => record.up(),
                    }
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
            docker: inner.docker.contains(&spec.ticket),
        };
        let record = self.record(
            &spec.name,
            &spec.repo,
            spec.scale_set_id,
            spec.size,
            &grant,
            &request,
        );
        let record = Record {
            runner_id: Some(spec.runner_id),
            ..record
        };
        if let Err(error) = self.launch(&mut inner, now, spec.ticket, record.clone(), &request) {
            // Back to a registration without a machine, which the caller removes; the slot stays the caller's.
            let mut intent = record;
            intent.slot = None;
            intent.end_reported = true;
            self.store.save(&intent);
            inner.intents.insert(spec.name.clone(), (intent, true));
            return Err(error);
        }
        inner.intents.remove(&spec.name);
        Ok(name)
    }

    async fn out_of_memory(&self, handle: &str) -> OutOfMemory {
        let Some(last) = self.lock().jobs.get(handle).map(|job| job.last.clone()) else {
            return OutOfMemory::Unknown {
                reason: "the machine is gone".into(),
            };
        };
        // Without the lock: the look may wait on the vm-host's socket.
        match self.vms.look(handle) {
            // A machine that stands has not said its last word: that is no answer nobody could give, only an early one.
            Seen::Alive(state) if state.end.is_none() && state.oom.is_none() => OutOfMemory::NotSoFar,
            Seen::Alive(state) | Seen::Gone(state) => state.out_of_memory(),
            Seen::Silent(_) => OutOfMemory::Unknown {
                reason: "the vm-host does not answer".into(),
            },
            Seen::Nothing | Seen::Exited => match &last {
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
        let Some(name) = Self::job_of(&inner, &event.runner) else {
            tracing::debug!(runner = %event.runner, "a job event for an Actions runner that is not this host's any more");
            return;
        };
        let job = inner.jobs.get_mut(&name).expect("just found");
        let github = job.record.github.get_or_insert_with(GithubJob::default);
        github.repo.clone_from(&event.repo);
        github.job.clone_from(&event.job);
        github.workflow.clone_from(&event.workflow);
        github.run_id = event.run_id;
        github.event.clone_from(&event.event);
        github.queued_at = event.queued_at.map(|t| t.unix_timestamp());
        github.started_at = event.started_at.map(|t| t.unix_timestamp()).or(github.started_at);
        github.inferred = false;
        if event.kind == EventKind::Completed {
            github.result = Some(event.result.clone());
            github.completed_at = Some(event.at.unix_timestamp());
            job.record.oom |= event.out_of_memory;
        }
        let word = match event.kind {
            EventKind::Started => Event::JobStarted,
            EventKind::Completed => Event::JobCompleted,
        };
        let at = event.started_at.unwrap_or(event.at).unix_timestamp();
        let opened = self.hear(&mut inner, &name, now, word, at, true);
        drop(inner);
        if opened {
            self.ring();
        }
    }

    fn out_of_memory(&self, _event: &JobEvent) {}
}
