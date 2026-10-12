use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use cf_ghapp::Clock;
use cf_scaleset::{Error, JobBase, Look, Message, Statistics};
use time::OffsetDateTime;

use crate::names::runner_name;
use crate::roster::{JobInfo, Roster, Runner};
use crate::traits::{Claim, EventKind, Events, Hosts, JobEvent, Jobs, Registry, RunnerSpec, Slots, Ticket};
use cf_budget::Size;
use cf_vmhost::OutOfMemory;

/// How long a new Actions runner may wait for its job before it counts as surplus.
pub const IDLE_GRACE: Duration = Duration::from_secs(2 * 60);

/// How long a slot stays promised to an acquired job the service has not assigned. The Go manager's reservations
/// ran out after the same two minutes.
pub const PROMISE_TTL: Duration = Duration::from_secs(2 * 60);

/// The least time between two openings of one scale set's session for room GitHub has not seen
/// ([`Scaler::with_renew_after`] changes it). Memory that comes and goes may open the Mac every few seconds; GitHub
/// gets to look at the queue at most this often for it.
pub const RENEW_AFTER: Duration = Duration::from_secs(15);

/// How often a scale set that has nothing on this machine while a machine of its size fits fetches GitHub's counts
/// ([`Scaler::with_recheck_after`] changes it). They otherwise come only with a message, and a scale set GitHub
/// sends nothing keeps the counts of its last one. Counts that name jobs GitHub holds back open the session again,
/// no sooner than this after it last opened: the second line behind [`RENEW_AFTER`], for whatever left room unseen.
pub const RECHECK_AFTER: Duration = Duration::from_secs(30);

/// How long the slots promised for a new session's first poll stay promised without an assignment. GitHub answers
/// that poll within a round trip; a bell that ends it first is followed by the next poll at once.
pub const OPENING_HOLD: Duration = Duration::from_secs(2);

/// How often an Actions runner whose name is taken is registered again under another.
const NAME_TRIES: usize = 3;

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
    /// Why the scale set's jobs can never run on this machine, when its settings rule the size out.
    never: Arc<Mutex<Option<String>>>,
    /// The counts as the service last reported them.
    last: Arc<Mutex<Option<Statistics>>>,
    /// How many Actions runners the scale set's assigned jobs need now: the service's count, less the jobs that ended
    /// since without a new one. None before the first report.
    wanted: Option<usize>,
    /// Whether the last capacity carried room for one more job; none before the first.
    offered_room: Option<bool>,
    /// Room came while the scale set had nothing on this machine, and GitHub has not looked at its queue since: it
    /// looks when a job is queued or ends and when a session opens, not when a poll offers more room.
    unseen_room: bool,
    /// The scale set has nothing on this machine.
    idle: bool,
    /// And a machine of its size fits.
    idle_with_room: bool,
    /// And GitHub's last counts name jobs it holds for the scale set.
    held_back: bool,
    /// When the session last opened, by the clock.
    opened: SystemTime,
    /// When GitHub's counts last came, by the clock.
    counted: SystemTime,
    /// The counts asked for last came by a count, not by a session that opened.
    counting: bool,
    /// The slots promised to held-back jobs for a new session's first poll, and since when.
    opening: Vec<Ticket>,
    opening_since: SystemTime,
    renew_after: Duration,
    recheck_after: Duration,
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
            events: None,
            was_out_of_memory: None,
            promised: vec![],
            waiting: vec![],
            never: Arc::new(Mutex::new(None)),
            last: Arc::new(Mutex::new(None)),
            wanted: None,
            offered_room: None,
            unseen_room: false,
            idle: true,
            idle_with_room: false,
            held_back: false,
            opened: clock.now(),
            counted: clock.now(),
            counting: false,
            opening: vec![],
            opening_since: clock.now(),
            renew_after: RENEW_AFTER,
            recheck_after: RECHECK_AFTER,
            clock,
        }
    }

    /// The least time between two openings of the session for room GitHub has not seen; [`RENEW_AFTER`] unless
    /// set.
    pub fn with_renew_after(mut self, after: Duration) -> Self {
        self.renew_after = after;
        self
    }

    /// How often counts are fetched while the scale set has nothing here and one fits, and the least time between
    /// two openings of the session while they name jobs GitHub holds back; [`RECHECK_AFTER`] unless set.
    pub fn with_recheck_after(mut self, after: Duration) -> Self {
        self.recheck_after = after;
        self
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
            match self.slots.reserve(self.config.size) {
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
        for _ in active + self.waiting.len()..assigned {
            let promised = (!self.promised.is_empty()).then(|| self.promised.remove(0).ticket);
            match self.slots.claim(self.config.size, promised) {
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
        if active > assigned {
            self.retire_surplus(active - assigned).await;
        }
        Ok(())
    }

    async fn start(&mut self, ticket: Ticket) -> Result<(), Error> {
        let mut tries = 0;
        let (name, jit) = loop {
            tries += 1;
            // Never the name of an Actions runner this machine has: its record is kept under the name.
            let name = loop {
                let name = runner_name(&self.config.machine, rand::random::<u32>());
                if self.roster.get(&name).is_none() {
                    break name;
                }
            };
            self.hosts
                .registering(&name, &self.config.repo, self.config.scale_set_id);
            match self.registry.generate_jit(&name).await {
                Ok(jit) => break (name, jit),
                Err(e) => {
                    // A refusal leaves nothing at GitHub; anything else may have registered the name before it
                    // failed. A taken name is no refusal: only this machine makes names that begin with its own,
                    // so the name is taken by a try of this call whose answer was lost on the way.
                    let refused = e
                        .status()
                        .is_some_and(|status| (400..500).contains(&status) && status != 409);
                    if refused {
                        self.hosts.not_registered(&name);
                    } else {
                        self.hosts.registration_unsure(&name);
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
            repo: self.config.repo.clone(),
            scale_set_id: self.config.scale_set_id,
            runner_id,
            size: self.config.size,
            ticket,
            jit_config: jit.encoded,
        };
        let handle = match self.hosts.start(spec).await {
            Ok(handle) => handle,
            Err(e) => {
                match self.registry.remove_runner(runner_id).await {
                    Ok(()) => self.hosts.not_registered(&name),
                    Err(remove) if remove.is_not_found() => self.hosts.not_registered(&name),
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
        self.roster.add(Runner {
            name: name.clone(),
            handle,
            runner_id,
            repo: self.config.repo.clone(),
            scale_set_id: self.config.scale_set_id,
            ticket: Some(ticket),
            started: self.clock.now(),
            busy: false,
            adopted: false,
            done: None,
            job: None,
        });
        tracing::info!(runner = %name, "Actions runner started");
        Ok(())
    }

    /// Removes up to `n` Actions runners that waited past [`IDLE_GRACE`] for a job that never came, such as one
    /// cancelled before it started. Busy, finished and adopted Actions runners are left alone.
    async fn retire_surplus(&mut self, mut n: usize) {
        for runner in self.roster.runners(&self.config.repo, self.config.scale_set_id) {
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
                    tracing::info!(runner = %runner.name, error = %e, "surplus Actions runner not removed");
                    continue;
                }
            }
            n -= 1;
            // Its registration is removed, so it takes no job and no longer counts. The machine keeps its slot
            // until whoever watches the machines sees it gone.
            self.roster.mark_done(
                &self.config.repo,
                self.config.scale_set_id,
                &runner.name,
                self.clock.now(),
            );
            match self.hosts.stop(&runner.handle).await {
                Ok(()) => tracing::info!(runner = %runner.name, "surplus Actions runner removed"),
                Err(e) => {
                    tracing::warn!(runner = %runner.name, error = %e, "could not stop a surplus Actions runner's machine");
                }
            }
        }
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
        self.offer().0
    }

    /// [`Scaler::capacity`], and whether it carries room for one more job.
    fn offer(&self) -> (usize, bool) {
        let room = self.slots.open_slots(self.config.size) > 0;
        (
            self.active() + self.waiting.len() + self.promised.len() + usize::from(room),
            room,
        )
    }

    /// Keeps track of room GitHub has not seen. Its polls offered none, and now one fits: GitHub gave the scale
    /// set's queued jobs out when they were queued, against the room offered then, and does not look again until
    /// something happens at GitHub. Nothing may happen there for a long time (2026-10-12: 13 minutes of an idle Mac
    /// with 7 of its jobs queued; a job that runs here may run for an hour).
    fn watch_room(&mut self, room: bool) {
        let idle = self.active() == 0 && self.waiting.is_empty() && self.promised.is_empty();
        if !room {
            self.unseen_room = false;
        } else if self.offered_room == Some(false) {
            self.unseen_room = true;
        }
        self.idle = idle;
        self.offered_room = Some(room);
        let available = self
            .last
            .lock()
            .expect("statistics")
            .as_ref()
            .is_some_and(|stats| stats.total_available_jobs > 0);
        self.idle_with_room = room && idle;
        self.held_back = room && available && self.promised.is_empty();
    }
}

impl<J: Jobs, R: Registry, H: Hosts> cf_scaleset::Scaler for Scaler<J, R, H> {
    async fn scale(&mut self, message: Option<&Message>) -> Result<usize, Error> {
        let counting = std::mem::take(&mut self.counting);
        if let Some(message) = message {
            if message.statistics.is_some() {
                self.counted = self.clock.now();
            }
            self.handle(message).await?;
            if let (Some(stats), false) = (&message.statistics, counting)
                && message.id == 0
                && message.assigned.is_empty()
            {
                // A session opened, and GitHub looks at the queue against its first poll.
                self.unseen_room = false;
                self.held_back = false;
                self.opened = self.clock.now();
                self.promise_held(stats);
            }
        }
        // After the Actions runners started: a job assigned late still finds the slot promised to it.
        self.converge().await?;
        self.give_back_unopened();
        self.expire_promises();
        let (capacity, room) = self.offer();
        self.watch_room(room);
        Ok(capacity)
    }

    fn renew(&mut self) -> Option<(Duration, Look)> {
        // Slots promised for a first poll go back in their time, whether or not a poll ends then.
        let (after, since, look) = if !self.opening.is_empty() {
            (OPENING_HOLD, self.opening_since, Look::Recount)
        } else if self.unseen_room {
            (self.renew_after, self.opened, self.reopen())
        } else if self.held_back {
            (self.recheck_after, self.opened, self.reopen())
        } else if self.idle_with_room {
            (self.recheck_after, self.counted, Look::Recount)
        } else {
            return None;
        };
        let now = self.clock.now();
        let age = now.duration_since(since).unwrap_or(Duration::ZERO);
        if age < after {
            return Some((after - age, look));
        }
        // A session opened again says so with its counts.
        if look == Look::Recount {
            self.counting = true;
            self.counted = now;
        }
        Some((Duration::ZERO, look))
    }
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

impl<J, R, H> Scaler<J, R, H> {
    /// A session opened: GitHub hands out the queued jobs it holds for the scale set against its first poll, and only
    /// then. A slot is promised to each the counts name that fits, so that poll shows GitHub all of them and no other
    /// scale set offers the same slots. One more slot a poll would get one job each time something happens at
    /// GitHub (2026-10-12 tests: two slots free after memory came back, two jobs queued; one ran, one waited).
    fn promise_held(&mut self, stats: &Statistics) {
        let held = usize::try_from(stats.total_available_jobs).unwrap_or(0);
        for _ in 0..held {
            let Ok(ticket) = self.slots.reserve(self.config.size) else {
                break;
            };
            self.promised.push(Promise {
                ticket,
                since: self.clock.now(),
            });
            self.opening.push(ticket);
        }
        self.opening_since = self.clock.now();
    }

    /// The slots promised for a session's first poll that no assignment took within [`OPENING_HOLD`] are free
    /// again, for this scale set's next poll and every other one's.
    fn give_back_unopened(&mut self) {
        let promised = &self.promised;
        self.opening
            .retain(|ticket| promised.iter().any(|promise| promise.ticket == *ticket));
        let held = self
            .clock
            .now()
            .duration_since(self.opening_since)
            .unwrap_or(Duration::ZERO);
        if self.opening.is_empty() || held < OPENING_HOLD {
            return;
        }
        let (slots, opening) = (&self.slots, std::mem::take(&mut self.opening));
        self.promised.retain(|promise| {
            let unused = opening.contains(&promise.ticket);
            if unused {
                slots.release(promise.ticket);
            }
            !unused
        });
    }

    /// How the session is opened again: at once while the scale set has nothing here, between messages while a job
    /// runs, so that its start and its end are heard.
    fn reopen(&self) -> Look {
        if self.idle {
            Look::Reopen
        } else {
            Look::ReopenBetweenMessages
        }
    }

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
