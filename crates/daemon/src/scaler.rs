use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use cf_ghapp::Clock;
use cf_scaleset::{Error, JobBase, Message, Statistics};
use time::OffsetDateTime;

use crate::names::runner_name;
use crate::roster::{JobInfo, Roster, Runner};
use crate::traits::{EventKind, Events, Hosts, JobEvent, Jobs, Registry, RunnerSpec, Slots, Ticket};

/// How long a new runner may wait for its job before it counts as surplus.
pub const IDLE_GRACE: Duration = Duration::from_secs(2 * 60);

/// How long a slot stays promised to an acquired job the service has not assigned. The Go manager's reservations
/// ran out after the same two minutes.
pub const PROMISE_TTL: Duration = Duration::from_secs(2 * 60);

/// How often a runner whose name is taken is registered again under another.
const NAME_TRIES: usize = 3;

/// What one scale set's scaler needs to know about itself.
#[derive(Debug, Clone)]
pub struct ScalerConfig {
    /// The scale set.
    pub scale_set_id: i64,
    /// Its repository, `owner/name`.
    pub repo: String,
    /// This machine's name, the start of every runner's name.
    pub machine: String,
    /// The memory size class of the scale set's runners in GB.
    pub size_gb: u32,
}

/// A slot promised to a job this scaler acquired.
#[derive(Debug, Clone, Copy)]
struct Promise {
    ticket: Ticket,
    since: SystemTime,
}

/// Asks whether a runner whose machine is gone had lost a process for memory, once per runner.
pub type WasOutOfMemory = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Turns one scale set's messages into runners: it takes jobs while the machine has room, starts one runner per
/// assigned job, and retires runners whose job went away. [`cf_scaleset::listen`] calls it one message at a time.
///
/// A message may arrive again after a failure: acquiring a job twice does nothing, and runners follow the service's
/// counts, not the number of messages.
///
/// The slots it promised and no runner took go back by themselves: after [`PROMISE_TTL`] without an assignment, and
/// when the scaler is dropped.
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
    /// Slots promised to jobs this scaler acquired and whose runners have not started, oldest first.
    promised: Vec<Promise>,
    last: Arc<Mutex<Option<Statistics>>>,
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
            last: Arc::new(Mutex::new(None)),
        }
    }

    /// Where job events go.
    pub fn with_events(mut self, events: Arc<dyn Events>) -> Self {
        self.events = Some(events);
        self
    }

    /// Who knows whether a runner whose machine is already gone had lost a process for memory.
    pub fn with_out_of_memory_check(mut self, was: WasOutOfMemory) -> Self {
        self.was_out_of_memory = Some(was);
        self
    }

    /// The scale set's job counts as the service last reported them, or none before its first report. The handle
    /// stays valid while the scaler is lent to the listener.
    pub fn statistics_handle(&self) -> Arc<Mutex<Option<Statistics>>> {
        self.last.clone()
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

    /// Whether the runner's machine lost a process to the kernel for memory. The completion usually comes while the
    /// machine still stands; for one already gone, or one whose host does not answer, whoever removed it knows.
    async fn out_of_memory(&self, runner: &str) -> bool {
        if runner.is_empty() {
            return false;
        }
        let recorded = || self.was_out_of_memory.as_ref().is_some_and(|was| was(runner));
        match self.roster.get(runner) {
            Some(r) if !r.handle.is_empty() => match self.hosts.out_of_memory(&r.handle).await {
                Ok(answer) => answer,
                Err(e) => {
                    tracing::warn!(runner = %runner, error = %e, "the host did not say whether the job ran out of memory");
                    recorded()
                }
            },
            _ => recorded(),
        }
    }

    async fn handle(&mut self, message: &Message) -> Result<(), Error> {
        self.acquire(message).await?;
        for started in &message.started {
            let event = self.event(EventKind::Started, &started.runner_name, &started.job, "");
            self.roster.mark_busy(
                &started.runner_name,
                JobInfo {
                    repo: event.repo.clone(),
                    name: event.job.clone(),
                    workflow: event.workflow.clone(),
                    run_id: event.run_id,
                    started: self.clock.now(),
                },
            );
            tracing::info!(runner = %started.runner_name, job = %started.job.job_display_name, "job started");
            if let (Some(events), false) = (&self.events, started.runner_name.is_empty()) {
                events.job(&event);
            }
        }
        let mut finished = 0;
        for done in &message.completed {
            let mut event = self.event(EventKind::Completed, &done.runner_name, &done.job, &done.result);
            event.out_of_memory = self.out_of_memory(&done.runner_name).await;
            if self.roster.mark_done(&done.runner_name, self.clock.now()) {
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
            // A job cancelled before any runner took it completes without one: nothing ran here.
            if let (Some(events), false) = (&self.events, done.runner_name.is_empty()) {
                events.job(&event);
            }
        }
        let mut last = self.last.lock().expect("statistics");
        match message.statistics {
            Some(stats) => *last = Some(stats),
            // Without new counts the old ones still name the jobs that just ended, and a runner would start for
            // each. Every job that ended on a runner of this machine is one assigned job fewer; only the service's
            // own counts raise the number again.
            None => {
                if let Some(kept) = last.as_mut() {
                    kept.total_assigned_jobs = (kept.total_assigned_jobs - finished).max(0);
                }
            }
        }
        Ok(())
    }

    /// Takes the offered jobs that fit. A slot is promised to each before it is acquired, so no more is acquired
    /// than the machine can run; jobs another machine took first give their slots back.
    async fn acquire(&mut self, message: &Message) -> Result<(), Error> {
        let mut ids: Vec<i64> = vec![];
        for offer in &message.available {
            if !ids.contains(&offer.runner_request_id) {
                ids.push(offer.runner_request_id);
            }
        }
        if ids.is_empty() {
            return Ok(());
        }
        // The promises are the scaler's as soon as they are made: were this call dropped while it waits for the
        // service, they would still run out or go back with the scaler.
        let first = self.promised.len();
        let mut refusal = None;
        for _ in 0..ids.len() {
            match self.slots.reserve(self.config.size_gb) {
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

    fn give_back(&mut self, promises: std::ops::Range<usize>) {
        for promise in self.promised.drain(promises) {
            self.slots.release(promise.ticket);
        }
    }

    /// Gives back the slots promised to jobs the service did not assign within [`PROMISE_TTL`]: cancelled, or taken
    /// by a runner elsewhere. A job assigned later still gets a slot, a free one ([`Slots::claim`]).
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

    /// Starts runners for the jobs assigned to the scale set that have none, and retires the surplus.
    async fn converge(&mut self) -> Result<(), Error> {
        let Some(last) = *self.last.lock().expect("statistics") else {
            return Ok(());
        };
        let active = self.roster.active(self.config.scale_set_id);
        let assigned = last.assigned();
        for _ in active..assigned {
            let promised = (!self.promised.is_empty()).then(|| self.promised.remove(0).ticket);
            let Some(ticket) = self.slots.claim(self.config.size_gb, promised) else {
                tracing::warn!(waiting = assigned - active, "assigned jobs wait for a free slot");
                return Ok(());
            };
            if let Err(e) = self.start(ticket).await {
                self.slots.release(ticket);
                return Err(e);
            }
        }
        if active > assigned {
            self.retire_surplus(active - assigned).await;
        }
        Ok(())
    }

    async fn start(&mut self, ticket: Ticket) -> Result<(), Error> {
        let (mut name, mut jit) = (String::new(), None);
        for attempt in 0..NAME_TRIES {
            name = runner_name(&self.config.machine, rand::random::<u32>());
            match self.registry.generate_jit(&name).await {
                Ok(config) => {
                    jit = Some(config);
                    break;
                }
                // The name is taken: names are random, another try uses another.
                Err(e) if e.status() == Some(409) && attempt + 1 < NAME_TRIES => continue,
                Err(e) => {
                    tracing::warn!(runner = %name, error = %e, "could not register a runner");
                    return Err(e);
                }
            }
        }
        let jit = jit.expect("the loop returns or sets it");
        let runner_id = jit.runner.id;
        let spec = RunnerSpec {
            name: name.clone(),
            repo: self.config.repo.clone(),
            scale_set_id: self.config.scale_set_id,
            runner_id,
            size_gb: self.config.size_gb,
            ticket,
            jit_config: jit.encoded,
        };
        let handle = match self.hosts.start(spec).await {
            Ok(handle) => handle,
            Err(e) => {
                if runner_id != 0
                    && let Err(remove) = self.registry.remove_runner(runner_id).await
                {
                    tracing::warn!(runner = %name, error = %remove, "could not unregister a runner that failed to start");
                }
                return Err(Error::Scaler(format!("start the machine of runner {name}: {e}")));
            }
        };
        self.roster.add(Runner {
            name: name.clone(),
            handle,
            runner_id,
            scale_set_id: self.config.scale_set_id,
            ticket: Some(ticket),
            started: self.clock.now(),
            busy: false,
            adopted: false,
            done: None,
            job: None,
        });
        tracing::info!(runner = %name, "runner started");
        Ok(())
    }

    /// Removes up to `n` runners that waited past [`IDLE_GRACE`] for a job that never came, such as one cancelled
    /// before it started. Busy, finished and adopted runners are left alone.
    async fn retire_surplus(&mut self, mut n: usize) {
        for runner in self.roster.runners(self.config.scale_set_id) {
            if n == 0 {
                return;
            }
            let age = self
                .clock
                .now()
                .duration_since(runner.started)
                .unwrap_or(Duration::ZERO);
            if runner.busy || runner.adopted || runner.done.is_some() || age < IDLE_GRACE {
                continue;
            }
            match self.registry.remove_runner(runner.runner_id).await {
                Ok(()) => {}
                // Gone from the service already: only its machine is left to remove.
                Err(e) if e.is_not_found() => {}
                Err(e) => {
                    // It may have just taken a job; try again on a later poll.
                    tracing::info!(runner = %runner.name, error = %e, "surplus runner not removed");
                    continue;
                }
            }
            n -= 1;
            if let Err(e) = self.hosts.stop(&runner.handle).await {
                // The machine may still stand and use its memory: it keeps its slot until whoever watches the
                // machines sees it gone. Its registration is removed, so it takes no job and no longer counts.
                tracing::warn!(runner = %runner.name, error = %e, "could not remove a surplus runner's machine");
                self.roster.mark_done(&runner.name, self.clock.now());
                continue;
            }
            self.roster.exited(&runner.name, &*self.slots);
            tracing::info!(runner = %runner.name, "surplus runner removed");
        }
    }

    /// How many runners this scale set may have at once right now, which every wait carries to the service: those it
    /// has, the slots promised to it, and one more while the machine has a free slot. The service may fill every
    /// number it is given before any reports back, so offering all free slots to every scale set would let it assign
    /// this machine several times the jobs it can run while other machines stay idle. One at a time, a burst still
    /// fills the machine within a few polls: each assignment ends the wait and the next one offers the next slot.
    pub fn capacity(&self) -> usize {
        self.roster.active(self.config.scale_set_id)
            + self.promised.len()
            + self.slots.open_slots(self.config.size_gb).min(1)
    }
}

impl<J: Jobs, R: Registry, H: Hosts> cf_scaleset::Scaler for Scaler<J, R, H> {
    async fn scale(&mut self, message: Option<&Message>) -> Result<usize, Error> {
        if let Some(message) = message {
            self.handle(message).await?;
        }
        // After the runners started: a job assigned late still finds the slot promised to it.
        self.converge().await?;
        self.expire_promises();
        Ok(self.capacity())
    }
}

impl<J, R, H> Drop for Scaler<J, R, H> {
    fn drop(&mut self) {
        for promise in self.promised.drain(..) {
            self.slots.release(promise.ticket);
        }
    }
}
