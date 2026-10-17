use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use cf_ghapp::Clock;
use cf_scaleset::{Error, JobBase, Message, Statistics};
use time::OffsetDateTime;

use crate::machines::Answer;
use crate::names::runner_name;
use crate::roster::{JobInfo, Roster, Runner};
use crate::traits::{Claim, EventKind, Events, Hosts, JobEvent, Jobs, Registry, RunnerSpec, Slots, Ticket};
use crate::turns::turn_of;
use cf_budget::{DEFER_LIMIT, Size, Turn};
use cf_vmhost::OutOfMemory;

/// How long a new Actions runner may wait for its job before it counts as surplus.
pub const IDLE_GRACE: Duration = Duration::from_secs(2 * 60);

/// How long a slot stays promised to an acquired job the service has not assigned. The Go manager's reservations
/// ran out after the same two minutes.
pub const PROMISE_TTL: Duration = Duration::from_secs(2 * 60);

/// How often an Actions runner whose name is taken is registered again under another.
const NAME_TRIES: usize = 3;

/// How many offered jobs the scaler remembers first seeing, for [`DEFER_LIMIT`].
const OFFERS_KEPT: usize = 512;

/// What one scale set's scaler needs to know about itself.
#[derive(Debug, Clone)]
pub struct ScalerConfig {
    /// The scale set.
    pub scale_set_id: i64,
    /// Its repository, `owner/name`.
    pub repo: String,
    /// This machine's name, the start of every Actions runner's name.
    pub machine: String,
    /// The size of the scale set's machines.
    pub size: Size,
    /// Whether its jobs get a Docker daemon (`cf-docker`).
    pub docker: bool,
}

/// A slot promised to a job this scaler acquired.
#[derive(Debug, Clone, Copy)]
struct Promise {
    ticket: Ticket,
    since: SystemTime,
}

/// Asks whether an Actions runner whose machine is gone had lost a process for memory, once per Actions runner.
pub type WasOutOfMemory = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Turns one scale set's messages into Actions runners: it takes jobs while the machine has room, starts one Actions
/// runner per assigned job, and retires Actions runners whose job went away. [`cf_scaleset::listen`] calls it one
/// message at a time.
///
/// A message may arrive again after a failure: acquiring a job twice does nothing, and Actions runners follow the
/// service's counts, not the number of messages.
///
/// The slots it promised and no Actions runner took go back by themselves: after [`PROMISE_TTL`] without an assignment,
/// and when the scaler is dropped.
pub struct Scaler<J, R, H> {
    config: ScalerConfig,
    jobs: J,
    registry: R,
    hosts: H,
    slots: Arc<dyn Slots>,
    roster: Arc<Roster>,
    clock: Arc<dyn Clock>,
    events: Option<Arc<dyn Events>>,
    was_out_of_memory: Option<WasOutOfMemory>,
    /// Slots promised to jobs this scaler acquired and whose Actions runners have not started, oldest first.
    promised: Vec<Promise>,
    /// Places in line of assigned jobs the machine has no room for yet, oldest first.
    waiting: Vec<Ticket>,
    /// Jobs GitHub assigned to the scale set that no Actions runner here has started, oldest first: what a new place
    /// in line stands for. GitHub hands a scale set's jobs to its Actions runners itself, so this is the scaler's
    /// best guess of which job a machine started next will run, assuming the order they were assigned in.
    pending: Vec<JobBase>,
    /// Offered jobs by request ID, with when they were first offered: one passed over goes first after
    /// [`DEFER_LIMIT`].
    offered: Vec<(i64, SystemTime)>,
    /// Why the scale set's jobs can never run on this machine, when its settings rule the size out.
    never: Arc<Mutex<Option<String>>>,
    /// The counts as the service last reported them.
    last: Arc<Mutex<Option<Statistics>>>,
    /// How many Actions runners the scale set's assigned jobs need now: the service's count, less the jobs that ended
    /// since without a new one. None before the first report.
    wanted: Option<usize>,
}

impl<J: Jobs, R: Registry, H: Hosts> Scaler<J, R, H> {
    /// A scaler for one scale set.
    pub fn new(
        config: ScalerConfig,
        jobs: J,
        registry: R,
        hosts: H,
        slots: Arc<dyn Slots>,
        roster: Arc<Roster>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Scaler {
            config,
            jobs,
            registry,
            hosts,
            slots,
            roster,
            clock,
            events: None,
            was_out_of_memory: None,
            promised: vec![],
            waiting: vec![],
            pending: vec![],
            offered: vec![],
            never: Arc::new(Mutex::new(None)),
            last: Arc::new(Mutex::new(None)),
            wanted: None,
        }
    }

    /// Where job events go.
    pub fn with_events(mut self, events: Arc<dyn Events>) -> Self {
        self.events = Some(events);
        self
    }

    /// Who knows whether an Actions runner whose machine is already gone had lost a process for memory.
    pub fn with_out_of_memory_check(mut self, was: WasOutOfMemory) -> Self {
        self.was_out_of_memory = Some(was);
        self
    }

    /// The scale set's job counts as the service last reported them, or none before its first report. The handle
    /// stays valid while the scaler is lent to the listener.
    pub fn statistics_handle(&self) -> Arc<Mutex<Option<Statistics>>> {
        self.last.clone()
    }

    /// Why this scale set's assigned jobs can never run here, or none while they can. The handle stays valid while
    /// the scaler is lent to the listener.
    pub fn never_handle(&self) -> Arc<Mutex<Option<String>>> {
        self.never.clone()
    }

    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::from(self.clock.now())
    }

    fn event(&self, kind: EventKind, runner: &str, base: &JobBase, result: &str) -> JobEvent {
        JobEvent {
            kind,
            runner: runner.to_string(),
            repo: base.repository(),
            job: base.job_display_name.clone(),
            workflow: base.job_workflow_ref.clone(),
            run_id: base.workflow_run_id,
            event: base.event_name.clone(),
            queued_at: base.queue_time,
            started_at: base.runner_assign_time,
            at: self.now(),
            result: result.to_string(),
            out_of_memory: false,
        }
    }

    /// Whether the Actions runner's machine lost a process to the kernel for memory. The completion usually comes while
    /// the machine still stands and has lost none so far; for that one, for one already gone and for one whose vm-host
    /// cannot say, whoever watches machines go knows. Only a vm-host that cannot say is worth a warning.
    async fn out_of_memory(&self, runner: &str) -> bool {
        if runner.is_empty() {
            return false;
        }
        let recorded = || self.was_out_of_memory.as_ref().is_some_and(|was| was(runner));
        match self.roster.get(runner) {
            Some(r) if !r.handle.is_empty() => match self.hosts.out_of_memory(&r.handle).await {
                OutOfMemory::Yes(_) => true,
                OutOfMemory::No => false,
                OutOfMemory::NotSoFar => recorded(),
                OutOfMemory::Unknown { reason } => {
                    let recorded = recorded();
                    tracing::warn!(
                        runner = %runner, %reason, recorded,
                        "the job's vm-host could not say whether the job ran out of memory; what the Runner recorded of its machine is used"
                    );
                    recorded
                }
            },
            _ => recorded(),
        }
    }

    async fn handle(&mut self, message: &Message) -> Result<(), Error> {
        self.acquire(message).await?;
        for job in &message.assigned {
            // A message that comes again after a failure names the same jobs again.
            if !self.pending.iter().any(|p| same_job(p, job)) {
                self.pending.push(job.clone());
            }
        }
        let ended = message.started.iter().map(|s| &s.job);
        for job in ended.chain(message.completed.iter().map(|c| &c.job)) {
            if let Some(at) = self.pending.iter().position(|p| same_job(p, job)) {
                self.pending.remove(at);
            }
        }
        for started in &message.started {
            let event = self.event(EventKind::Started, &started.runner_name, &started.job, "");
            self.roster.mark_busy(
                &self.config.repo,
                self.config.scale_set_id,
                &started.runner_name,
                JobInfo {
                    repo: event.repo.clone(),
                    name: event.job.clone(),
                    workflow: event.workflow.clone(),
                    run_id: event.run_id,
                    started: self.clock.now(),
                },
            );
            // GitHub says so with its next message, which may be most of a minute after the start: the line names
            // the start by GitHub's own time and how long its word took.
            let (at, heard_after_s) = heard(event.started_at, event.at);
            tracing::info!(
                runner = %started.runner_name, job = %started.job.job_display_name, %at, heard_after_s,
                "job started"
            );
            if let (Some(events), false) = (&self.events, started.runner_name.is_empty()) {
                events.job(&event);
            }
        }
        let mut finished = 0;
        for done in &message.completed {
            let mut event = self.event(EventKind::Completed, &done.runner_name, &done.job, &done.result);
            event.out_of_memory = self.out_of_memory(&done.runner_name).await;
            if self.roster.mark_done(
                &self.config.repo,
                self.config.scale_set_id,
                &done.runner_name,
                self.clock.now(),
            ) {
                finished += 1;
            }
            tracing::info!(runner = %done.runner_name, job = %done.job.job_display_name, result = %done.result, "job completed");
            if event.out_of_memory {
                tracing::warn!(
                    runner = %done.runner_name, job = %done.job.job_display_name, run = done.job.workflow_run_id,
                    result = %done.result,
                    "job ran out of memory: it used up its memory and swap, and the kernel killed processes of it"
                );
                if let Some(events) = &self.events {
                    events.out_of_memory(&event);
                }
            }
            // A job cancelled before any Actions runner took it completes without one: nothing ran here.
            if let (Some(events), false) = (&self.events, done.runner_name.is_empty()) {
                events.job(&event);
            }
        }
        match message.statistics {
            Some(stats) => {
                // A start or an end GitHub's word did not tie to its assignment leaves no job behind for long.
                let gone = self.pending.len().saturating_sub(stats.unstarted());
                self.pending.drain(..gone);
                self.wanted = Some(stats.assigned());
                *self.last.lock().expect("statistics") = Some(stats);
            }
            // Without new counts the old ones still name the jobs that just ended, and an Actions runner would start
            // for each. Every job that ended on an Actions runner of this scale set is one Actions runner fewer to
            // have; only the service's own counts raise the number again.
            None => self.wanted = self.wanted.map(|wanted| wanted.saturating_sub(finished)),
        }
        Ok(())
    }

    /// Takes the offered jobs that fit. A slot is promised to each before it is acquired, so no more is acquired
    /// than the machine can run; jobs another machine took first give their slots back.
    ///
    /// When not all fit, the jobs offered for longer than [`DEFER_LIMIT`] go first, then those of the main branch and
    /// deploys ([`turn_of`]), then the rest; the others stay offered at GitHub.
    async fn acquire(&mut self, message: &Message) -> Result<(), Error> {
        let now = self.clock.now();
        let mut offers: Vec<(u8, SystemTime, i64)> = vec![];
        for offer in &message.available {
            let id = offer.runner_request_id;
            if offers.iter().any(|&(_, _, seen)| seen == id) {
                continue;
            }
            let since = match self.offered.iter().find(|(seen, _)| *seen == id) {
                Some(&(_, since)) => since,
                None => {
                    self.offered.push((id, now));
                    now
                }
            };
            let waited = now.duration_since(since).unwrap_or(Duration::ZERO);
            let kind = if waited >= Duration::from_millis(DEFER_LIMIT.0) {
                0
            } else if turn_of(offer).urgent {
                1
            } else {
                2
            };
            offers.push((kind, since, id));
        }
        // Past the bound, those not offered now are forgotten, the newest of them first: they lose the least age.
        let mut excess = self.offered.len().saturating_sub(OFFERS_KEPT);
        let mut at = self.offered.len();
        while excess > 0 && at > 0 {
            at -= 1;
            let id = self.offered[at].0;
            if !offers.iter().any(|&(_, _, offered)| offered == id) {
                self.offered.remove(at);
                excess -= 1;
            }
        }
        if offers.is_empty() {
            return Ok(());
        }
        offers.sort_by_key(|&(kind, since, _)| (kind, since));
        let mut ids: Vec<i64> = offers.into_iter().map(|(_, _, id)| id).collect();
        // The promises are the scaler's as soon as they are made: were this call dropped while it waits for the
        // service, they would still run out or go back with the scaler.
        let first = self.promised.len();
        let mut refusal = None;
        for _ in 0..ids.len() {
            match self.slots.reserve_for(self.config.size, self.config.docker) {
                Ok(ticket) => self.promised.push(Promise {
                    ticket,
                    since: self.clock.now(),
                }),
                Err(why) => {
                    refusal = Some(why);
                    break;
                }
            }
        }
        let reserved = self.promised.len() - first;
        if reserved == 0 {
            let (open, reason) = self.slots.state();
            tracing::info!(offered = ids.len(), open, reason = %reason, why = ?refusal, "jobs offered; no free slot");
            return Ok(());
        }
        ids.truncate(reserved);
        let got = match self.jobs.acquire(&ids).await {
            Ok(got) => got,
            Err(e) => {
                self.give_back(first..self.promised.len());
                return Err(e);
            }
        };
        // A job acquired, or gone to another machine, is offered no more.
        self.offered.retain(|(id, _)| !ids.contains(id));
        // Jobs another machine took first give their slots back (the oldest of these promises, as the Go manager
        // did).
        let lost = reserved.saturating_sub(got.len());
        self.give_back(first..first + lost);
        tracing::info!(
            offered = message.available.len(),
            tried = ids.len(),
            acquired = got.len(),
            "jobs offered"
        );
        Ok(())
    }

    /// Gives back the slots promised to jobs the service did not assign within [`PROMISE_TTL`]: cancelled, or taken
    /// by an Actions runner elsewhere. A job assigned later still gets a slot, a free one ([`Slots::claim`]).
    fn expire_promises(&mut self) {
        let now = self.clock.now();
        let slots = &self.slots;
        self.promised.retain(|promise| {
            let waited = now.duration_since(promise.since).unwrap_or(Duration::ZERO);
            if waited < PROMISE_TTL {
                return true;
            }
            slots.release(promise.ticket);
            tracing::info!(waited = ?waited, "an acquired job was not assigned; its slot is free again");
            false
        });
    }

    /// Starts Actions runners for the jobs assigned to the scale set that have none, keeps a place in line for
    /// those the machine has no room for yet, and retires the surplus.
    async fn converge(&mut self) -> Result<(), Error> {
        let Some(assigned) = self.wanted else {
            return Ok(());
        };
        let active = self.active();
        // Jobs that waited in line and went away give their place back, the newest first.
        while active + self.waiting.len() > assigned
            && let Some(ticket) = self.waiting.pop()
        {
            self.slots.withdraw(ticket);
        }
        // Then those whose turn came.
        let mut at = 0;
        while at < self.waiting.len() {
            let ticket = self.waiting[at];
            if !self.slots.ready(ticket) {
                at += 1;
                continue;
            }
            self.waiting.remove(at);
            if let Err(e) = self.start(ticket).await {
                self.slots.withdraw(ticket);
                return Err(e);
            }
        }
        let active = self.active();
        // The jobs the Actions runners that wait for one take first, then those the places in line stand for.
        let taken = self.idle() + self.waiting.len();
        for n in 0..assigned.saturating_sub(active + self.waiting.len()) {
            let promised = (!self.promised.is_empty()).then(|| self.promised.remove(0).ticket);
            let turn = self.turn_for(taken + n);
            let claim = self.slots.claim_in_turn(self.config.size, promised, turn);
            if self.config.docker
                && let Claim::Start(ticket) | Claim::Waits { ticket, .. } = &claim
            {
                self.slots.mark_docker(*ticket);
            }
            match claim {
                Claim::Start(ticket) => {
                    if let Err(e) = self.start(ticket).await {
                        self.slots.release(ticket);
                        return Err(e);
                    }
                }
                Claim::Waits { ticket, why } => {
                    tracing::info!(%why, "an assigned job waits in line for room");
                    self.waiting.push(ticket);
                }
                Claim::Never(why) => {
                    let mut never = self.never.lock().expect("never");
                    if never.as_deref() != Some(why.as_str()) {
                        tracing::error!(
                            size = self.config.size.label(), %why,
                            "an assigned job can never run on this machine: its settings rule the size out"
                        );
                        *never = Some(why);
                    }
                    return Ok(());
                }
            }
        }
        *self.never.lock().expect("never") = None;
        self.retune();
        if active > assigned {
            self.retire_surplus(active - assigned);
        }
        Ok(())
    }

    async fn start(&mut self, ticket: Ticket) -> Result<(), Error> {
        let at = Start {
            machine: &self.config.machine,
            repo: &self.config.repo,
            scale_set_id: self.config.scale_set_id,
            size: self.config.size,
        };
        start_runner(
            &self.registry,
            &self.hosts,
            &self.roster,
            &*self.clock,
            at,
            ticket,
        )
        .await?;
        Ok(())
    }

    /// Retires up to `n` Actions runners that waited past [`IDLE_GRACE`] for a job that never came, such as one
    /// cancelled before it started. Busy, finished and adopted Actions runners are left alone, and those already
    /// asking for their removal count ([`Hosts::retire`]).
    ///
    /// The scaler only asks: the lifecycle asks for the registration's removal, the sweep sends it, and GitHub's
    /// answer decides (`docs/job-state.md`).
    fn retire_surplus(&mut self, n: usize) {
        let (repo, set) = (self.config.repo.clone(), self.config.scale_set_id);
        let now = self.clock.now();
        let idle: Vec<String> = self
            .roster
            .runners(&repo, set)
            .into_iter()
            .filter(|runner| {
                let age = now.duration_since(runner.started).unwrap_or(Duration::ZERO);
                !(runner.busy || runner.adopted || runner.done.is_some() || age < IDLE_GRACE)
            })
            .map(|runner| runner.name)
            .collect();
        self.hosts.retire(&repo, set, n, &idle);
    }

    /// The turn of the `at`th job GitHub assigned that no Actions runner here started.
    fn turn_for(&self, at: usize) -> Turn {
        Turn {
            repo: self.config.repo.clone(),
            ..self.pending.get(at).map(turn_of).unwrap_or_default()
        }
    }

    /// Gives each place in line the turn of the job it stands for now: jobs started, ended or went away since it was
    /// taken, and the places that went were the newest, whatever job went.
    fn retune(&self) {
        let idle = self.idle();
        for (k, &ticket) in self.waiting.iter().enumerate() {
            self.slots.set_turn(ticket, self.turn_for(idle + k));
        }
    }

    /// The Actions runners of this scale set that wait for a job here.
    fn idle(&self) -> usize {
        let (repo, id) = (self.config.repo.as_str(), self.config.scale_set_id);
        self.roster
            .runners(repo, id)
            .iter()
            .filter(|runner| runner.waits())
            .count()
    }

    /// The Actions runners GitHub counts for this scale set: those with a machine, and those whose machine is gone
    /// under a job GitHub has not let go of.
    fn active(&self) -> usize {
        let (repo, id) = (self.config.repo.as_str(), self.config.scale_set_id);
        self.roster.active(repo, id) + self.hosts.lost_with_job(repo, id)
    }

    /// How many Actions runners this scale set may have at once right now, which every wait carries to the service:
    /// those it has, the jobs that wait in line, the slots promised to it, and one more while the machine has a free slot. The service may fill
    /// every number it is given before any reports back, so offering all free slots to every scale set would let it
    /// assign this machine several times the jobs it can run while other machines stay idle. One at a time, a burst
    /// still fills the machine within a few polls: each assignment ends the wait and the next one offers the next slot.
    pub fn capacity(&self) -> usize {
        self.active()
            + self.waiting.len()
            + self.promised.len()
            + self
                .slots
                .open_slots(self.config.size)
                .min(1)
                .min(if self.config.docker {
                    self.slots.docker_room()
                } else {
                    usize::MAX
                })
    }
}

impl<J: Jobs, R: Registry, H: Hosts> cf_scaleset::Scaler for Scaler<J, R, H> {
    async fn scale(&mut self, message: Option<&Message>) -> Result<usize, Error> {
        if let Some(message) = message {
            self.handle(message).await?;
        }
        // After the Actions runners started: a job assigned late still finds the slot promised to it.
        self.converge().await?;
        self.expire_promises();
        Ok(self.capacity())
    }
}

/// Where an Actions runner is registered and what machine it gets.
#[derive(Debug, Clone, Copy)]
pub struct Start<'a> {
    /// This machine's name, the start of every Actions runner's name.
    pub machine: &'a str,
    /// The repository, `owner/name`.
    pub repo: &'a str,
    /// The scale set, or [`crate::held::OWN`] for an Actions runner of its own.
    pub scale_set_id: i64,
    /// The machine's size.
    pub size: Size,
}

/// Registers an Actions runner with `registry` and starts its machine in the slot `ticket`, which is the machine's
/// from then on. Says the Actions runner's name. After an error the slot is still the caller's, and nothing is left
/// at GitHub that the host does not know to remove.
pub async fn start_runner<R: Registry, H: Hosts>(
    registry: &R,
    hosts: &H,
    roster: &Roster,
    clock: &dyn Clock,
    at: Start<'_>,
    ticket: Ticket,
) -> Result<String, Error> {
    let mut tries = 0;
    let (name, jit) = loop {
        tries += 1;
        // Never the name of an Actions runner this machine has: its record is kept under the name.
        let name = loop {
            let name = runner_name(at.machine, rand::random::<u32>());
            if roster.get(&name).is_none() {
                break name;
            }
        };
        hosts.registering(&name, at.repo, at.scale_set_id);
        match registry.generate_jit(&name).await {
            Ok(jit) => break (name, jit),
            Err(e) => {
                // A refusal leaves nothing at GitHub; anything else may have registered the name before it
                // failed. A taken name is no refusal: only this machine makes names that begin with its own,
                // so the name is taken by a try of this call whose answer was lost on the way.
                let refused = e
                    .status()
                    .is_some_and(|status| (400..500).contains(&status) && status != 409);
                if refused {
                    hosts.not_registered(&name);
                } else {
                    hosts.registration_unsure(&name);
                }
                // The name is taken: names are random, another try uses another.
                if e.status() == Some(409) && tries < NAME_TRIES {
                    continue;
                }
                tracing::warn!(runner = %name, error = %e, "could not register an Actions runner");
                return Err(e);
            }
        }
    };
    let runner_id = jit.runner.id;
    let spec = RunnerSpec {
        name: name.clone(),
        repo: at.repo.to_string(),
        scale_set_id: at.scale_set_id,
        runner_id,
        size: at.size,
        ticket,
        jit_config: jit.encoded,
    };
    let handle = match hosts.start(spec).await {
        Ok(handle) => handle,
        Err(e) => {
            match registry.remove_runner(runner_id).await {
                Ok(()) => hosts.not_registered(&name),
                Err(remove) if remove.is_not_found() => hosts.not_registered(&name),
                // The host keeps the name as a registration to remove.
                Err(remove) => {
                    tracing::warn!(runner = %name, error = %remove, "could not unregister an Actions runner that failed to start");
                }
            }
            return Err(Error::Scaler(format!(
                "start the machine of Actions runner {name}: {e}"
            )));
        }
    };
    roster.add(Runner {
        name: name.clone(),
        handle,
        runner_id,
        repo: at.repo.to_string(),
        scale_set_id: at.scale_set_id,
        ticket: Some(ticket),
        started: clock.now(),
        busy: false,
        adopted: false,
        done: None,
        job: None,
    });
    tracing::info!(runner = %name, "Actions runner started");
    Ok(name)
}

/// Whether two messages speak of the same job: by its request ID when both carry one, else by its run and name. The
/// real service sends an assigned job with no request ID; two jobs of one run that name nothing may be two.
fn same_job(a: &JobBase, b: &JobBase) -> bool {
    if a.runner_request_id > 0 && b.runner_request_id > 0 {
        return a.runner_request_id == b.runner_request_id;
    }
    a.workflow_run_id > 0
        && !a.job_display_name.is_empty()
        && a.workflow_run_id == b.workflow_run_id
        && a.job_display_name == b.job_display_name
}

/// When GitHub says a job started, as a text, and how many seconds later its word came. A start GitHub names no
/// time for counts as now.
fn heard(started_at: Option<OffsetDateTime>, now: OffsetDateTime) -> (String, i64) {
    let at = started_at.unwrap_or(now);
    (
        crate::service::iso(at.unix_timestamp()),
        (now - at).whole_seconds().max(0),
    )
}

/// Before a scale set's session opens: whatever the session before it left unread may be gone, since GitHub drops a
/// session's pending messages with it and those queued while there is none (`docs/github-protocol.md`). Each Actions
/// runner of the scale set that waits for a job here is held against GitHub's REST view of it, and one that is busy
/// took a job whose start was not heard.
///
/// It runs before the session opens, so the counts the session opens with are from after every answer acted on here:
/// a job that ends meanwhile is not in them (`docs/job-state.md`, rule 1). Only `busy: true` moves anything. An idle
/// answer is one moment's, and one that the Actions runner is gone is left to the sweep, which heard whether GitHub
/// removed it at the Runner's request.
pub async fn reconcile<R: Registry>(
    registry: &R,
    roster: &Roster,
    repo: &str,
    scale_set_id: i64,
    answered: impl Fn(&str, Answer),
) {
    for runner in roster.runners(repo, scale_set_id) {
        if runner.busy || runner.done.is_some() {
            continue;
        }
        match registry.runner_busy(runner.runner_id).await {
            Ok(Some(true)) => answered(&runner.name, Answer::Busy),
            Ok(Some(false) | None) => {}
            Err(error) => {
                tracing::warn!(
                    runner = %runner.name, %error,
                    "GitHub could not say whether an Actions runner has a job; a start it lost with a session is heard when the machine ends"
                );
            }
        }
    }
}

impl<J, R, H> Scaler<J, R, H> {
    fn give_back(&mut self, promises: std::ops::Range<usize>) {
        for promise in self.promised.drain(promises) {
            self.slots.release(promise.ticket);
        }
    }
}

impl<J, R, H> Drop for Scaler<J, R, H> {
    fn drop(&mut self) {
        self.give_back(0..self.promised.len());
        for ticket in self.waiting.drain(..) {
            self.slots.withdraw(ticket);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: i64, run: i64, name: &str) -> JobBase {
        JobBase {
            runner_request_id: id,
            workflow_run_id: run,
            job_display_name: name.into(),
            ..JobBase::default()
        }
    }

    #[test]
    fn a_job_is_known_by_its_request_id_else_by_its_run_and_name() {
        assert!(same_job(&job(5, 1, "a"), &job(5, 2, "b")));
        assert!(!same_job(&job(5, 1, "a"), &job(6, 1, "a")));
        // An assignment names no request ID; its start does.
        assert!(same_job(&job(0, 1, "check"), &job(5, 1, "check")));
        assert!(!same_job(&job(0, 1, "check"), &job(5, 1, "deploy")));
        assert!(!same_job(&job(0, 1, "check"), &job(0, 2, "check")));
        // Jobs that name neither run nor name may be any jobs.
        assert!(!same_job(&job(0, 0, "check"), &job(0, 0, "check")));
        assert!(!same_job(&job(0, 1, ""), &job(0, 1, "")));
    }
}
