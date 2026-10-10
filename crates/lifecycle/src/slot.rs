use cf_budget::{EvictionCause, Millis};

/// Where a slot is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Phase {
    /// Memory is reserved and GitHub was asked for the job. No VM.
    Reserved,
    /// The job is ours; the VM boots. Left by the supervisor's `ready`, or after [`Limits::start`].
    Starting,
    /// The first VM did not get ready and was killed; the second starts once its process is gone.
    Retrying,
    /// The Actions runner in the VM is up and waits for its job. Left after [`Limits::idle`] without one.
    Idle,
    /// The job runs.
    Running,
    /// The job is over; the guest saves what it keeps and powers off. Left after [`Limits::finish`].
    Finishing,
    /// The vm-host was asked to stop the VM. Left after the grace time and [`Limits::stop`].
    Stopping,
    /// The vm-host was sent SIGKILL. Left only by its death.
    Killing,
    /// Over: no VM, the memory given back.
    Released,
}

/// How a slot ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum End {
    /// The job ran to its end; whether it passed is GitHub's to say.
    Ok,
    /// The job ran to its end and a process of it was stopped for going over the job's memory limit.
    Oom,
    /// Stopped because the Mac ran short of memory.
    EvictedMemory,
    /// Stopped because the Mac's disk was nearly full.
    EvictedDisk,
    /// Stopped after [`Limits::job`].
    Timeout,
    /// The VM died or went silent.
    GuestLost,
    /// The Mac slept for longer than [`Limits::long_sleep`].
    HostSlept,
    /// Stopped from the console or the tray, or because the Runner shuts down.
    Stopped,
    /// No job came for the reservation: another Mac got it, or it ran out.
    Lost,
    /// The VM did not get ready, twice.
    StartFailed,
    /// The Actions runner was up and no job came.
    NoJob,
}

impl End {
    /// The name the console knows this end by.
    pub const fn name(self) -> &'static str {
        match self {
            End::Ok => "ok",
            End::Oom => "oom",
            End::EvictedMemory => "evicted-memory",
            End::EvictedDisk => "evicted-disk",
            End::Timeout => "timeout",
            End::GuestLost => "guest-lost",
            End::HostSlept => "host-slept",
            End::Stopped => "stopped",
            End::Lost => "lost",
            End::StartFailed => "start-failed",
            End::NoJob => "no-job",
        }
    }
}

/// Something that happened to a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Event {
    /// GitHub gave the job to this Mac and the budget confirmed the reservation.
    Assigned,
    /// Another Mac got the job.
    Lost,
    /// The budget gave the reservation back by itself (it ran out): there is no memory left to release.
    Expired,
    /// The guest's supervisor reported `ready`.
    VmReady,
    /// The Actions runner took the job.
    JobStarted,
    /// The supervisor's heartbeat arrived.
    Heartbeat,
    /// The supervisor saw the kernel stop a process of the job for memory.
    OutOfMemory,
    /// The job is over, whatever its result.
    JobCompleted,
    /// The vm-host's process is gone, and the VM with it.
    VmExited,
    /// The budget says this job has to stop for the Mac's sake.
    Evict(EvictionCause),
    /// The console, the tray or a shutdown asks for this slot to stop.
    Stop,
    /// The Mac slept for this long.
    Slept(Millis),
    /// Time passed. Send it at least every second; the time itself comes with every event.
    Tick,
}

/// What a job is told before it is stopped. The words are the guest's, where they are printed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Notice {
    /// The Mac needs its memory or its disk back; the job did nothing wrong.
    Evicted(EvictionCause),
    /// The job ran for longer than a job may.
    TimedOut,
    /// Someone stopped it.
    Stopped,
}

/// What the daemon has to do after an event, in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Effect {
    /// Start a vm-host with the slot's grant. There is no VM of this slot at this moment.
    StartVm,
    /// Tell the job why it is about to be stopped.
    Tell(Notice),
    /// Ask the vm-host to stop the VM after `grace` (time for the Actions runner to upload what it has).
    StopVm { grace: Millis },
    /// Send the vm-host's process group SIGKILL.
    KillVm,
    /// Remove the Actions runner's registration at GitHub if it is still there.
    Unregister,
    /// Give the slot's memory back to the budget and delete its scratch disk. The VM's process is gone.
    Release,
    /// A VM did not reach `ready`: tell the [`crate::StartBreaker`].
    StartFailed,
    /// A VM reached `ready`: tell the [`crate::StartBreaker`].
    StartSucceeded,
    /// A process that was sent SIGKILL is still there. Its memory stays counted; report it and hold new slots.
    Stuck,
    /// The slot is over. Always the last effect of a slot, and sent once.
    Ended { end: End, ran_job: bool },
}

/// A watchdog that is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Timer {
    /// From the VM's start to the supervisor's `ready`.
    Start,
    /// The Actions runner without a job.
    Idle,
    /// Since the last heartbeat.
    Heartbeat,
    /// The job's own time.
    Job,
    /// From the job's end to the VM's power-off.
    Finish,
    /// From asking the vm-host to stop to its death.
    Stop,
    /// From SIGKILL to the next one.
    Kill,
}

/// Every time limit of a slot. The defaults are the plan's watchdog table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Limits {
    /// From the VM's start to the supervisor's `ready`: 20 s.
    pub start: Millis,
    /// How many times a VM is started for one job: 2 (one retry).
    pub starts: u32,
    /// The Actions runner without a job: 2 minutes.
    pub idle: Millis,
    /// Silence of the supervisor's heartbeat: 60 s.
    pub heartbeat: Millis,
    /// A job: 6 hours.
    pub job: Millis,
    /// From the job's end to the VM's power-off: 2 minutes.
    pub finish: Millis,
    /// What a running job gets to upload before its VM is stopped: 20 s.
    pub grace: Millis,
    /// From the end of the grace time to SIGKILL: 10 s.
    pub stop: Millis,
    /// From one SIGKILL to the next: 10 s.
    pub kill_again: Millis,
    /// A VM that was there during a longer sleep than this is stopped: 5 minutes.
    pub long_sleep: Millis,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            start: Millis::secs(20),
            starts: 2,
            idle: Millis::mins(2),
            heartbeat: Millis::secs(60),
            job: Millis::mins(360),
            finish: Millis::mins(2),
            grace: Millis::secs(20),
            stop: Millis::secs(10),
            kill_again: Millis::secs(10),
            long_sleep: Millis::mins(5),
        }
    }
}

/// What of a slot is written to disk, so that a daemon that starts again can go on with it. The watchdogs are not
/// in it: they start again from the restart (the vm-host keeps the job's own limit itself).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Saved {
    /// Where the slot was.
    pub phase: Phase,
    /// How many VMs were started for it.
    pub starts: u32,
    /// The end that was already decided, if one was.
    pub end: Option<End>,
    /// Whether a process of the job was stopped for memory.
    pub oom: bool,
    /// Whether a job started in it.
    pub ran_job: bool,
}

/// One slot's state machine. See the crate's page for the picture and for what holds on every path.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Slot {
    limits: Limits,
    phase: Phase,
    starts: u32,
    end: Option<End>,
    oom: bool,
    ran_job: bool,
    /// Whether the budget still counts this slot.
    memory: bool,
    /// Whether this slot came back from disk: what a second VM needs to register (the Actions runner's
    /// configuration) was never saved, so a restored slot never starts one.
    restored: bool,
    /// The watchdog of the phase.
    deadline: Option<Millis>,
    heartbeat: Option<Millis>,
    job: Option<Millis>,
}

impl Slot {
    /// A slot whose memory was just reserved.
    pub fn reserved(limits: Limits) -> Slot {
        Slot {
            limits,
            phase: Phase::Reserved,
            starts: 0,
            end: None,
            oom: false,
            ran_job: false,
            memory: true,
            restored: false,
            deadline: None,
            heartbeat: None,
            job: None,
        }
    }

    /// A slot found on disk after a restart, and what to do about it.
    ///
    /// `vm_alive`: whether its vm-host's process is there. A live VM must have been adopted into the new budget
    /// before this call, so its memory is counted; a dead one must not have been, and nothing is released for it.
    /// A VM that was still booting goes on booting. One that was waiting to be started a second time is not
    /// started again: what the Actions runner needs to register was never on disk.
    pub fn restore(limits: Limits, now: Millis, saved: Saved, vm_alive: bool) -> (Slot, Vec<Effect>) {
        let mut slot = Slot::reserved(limits);
        slot.starts = saved.starts;
        slot.end = saved.end;
        slot.oom = saved.oom;
        slot.ran_job = saved.ran_job;
        slot.memory = vm_alive;
        slot.restored = true;
        let mut effects = Vec::new();
        if saved.phase == Phase::Released {
            slot.memory = false;
            slot.phase = Phase::Released;
            return (slot, effects);
        }
        if !vm_alive {
            // The budget is a new one and never counted this slot.
            let end = match saved.phase {
                Phase::Reserved => End::Lost,
                Phase::Starting => {
                    effects.push(Effect::StartFailed);
                    End::StartFailed
                }
                Phase::Retrying => End::StartFailed,
                _ => saved.end.unwrap_or(End::GuestLost),
            };
            slot.starts = slot.starts.max(u32::from(saved.phase != Phase::Reserved));
            slot.release(end, &mut effects);
            return (slot, effects);
        }
        slot.starts = slot.starts.max(1);
        match saved.phase {
            Phase::Starting => {
                slot.phase = Phase::Starting;
                slot.deadline = Some(now.plus(limits.start));
            }
            Phase::Idle => slot.idle(now),
            Phase::Running => slot.run(now),
            Phase::Finishing => {
                slot.phase = Phase::Finishing;
                slot.end = Some(saved.end.unwrap_or(End::Ok));
                slot.deadline = Some(now.plus(limits.finish));
            }
            Phase::Stopping => slot.stop(now, saved.end.unwrap_or(End::Stopped), Millis::ZERO, &mut effects),
            // A VM where there cannot be one (a reservation), one that was to be replaced, or one being killed.
            Phase::Reserved | Phase::Retrying | Phase::Killing | Phase::Released => {
                let end = match saved.phase {
                    Phase::Retrying => End::StartFailed,
                    _ => saved.end.unwrap_or(End::GuestLost),
                };
                slot.kill(now, end, &mut effects);
            }
        }
        (slot, effects)
    }

    /// What to write to disk for [`Slot::restore`].
    pub fn saved(&self) -> Saved {
        Saved {
            phase: self.phase,
            starts: self.starts,
            end: self.end,
            oom: self.oom,
            ran_job: self.ran_job,
        }
    }

    /// Where the slot is.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Whether the budget still counts this slot's memory.
    pub fn holds_memory(&self) -> bool {
        self.memory
    }

    /// The watchdogs that are running, with the time each has left at `now`.
    pub fn timers(&self, now: Millis) -> Vec<(Timer, Millis)> {
        let phase_timer = match self.phase {
            Phase::Starting => Some(Timer::Start),
            Phase::Idle => Some(Timer::Idle),
            Phase::Finishing => Some(Timer::Finish),
            Phase::Stopping => Some(Timer::Stop),
            Phase::Retrying | Phase::Killing => Some(Timer::Kill),
            Phase::Reserved | Phase::Running | Phase::Released => None,
        };
        let left = |at: Millis| at.since(now);
        let mut timers = Vec::new();
        timers.extend(
            phase_timer
                .zip(self.deadline)
                .map(|(timer, at)| (timer, left(at))),
        );
        timers.extend(self.heartbeat.map(|at| (Timer::Heartbeat, left(at))));
        timers.extend(self.job.map(|at| (Timer::Job, left(at))));
        timers
    }

    /// When the next watchdog is due; `None` when none runs (a reservation, whose time the budget keeps, and a
    /// released slot).
    pub fn next_deadline(&self) -> Option<Millis> {
        [self.deadline, self.heartbeat, self.job]
            .into_iter()
            .flatten()
            .min()
    }

    /// Takes one event at `now` and returns what to do, in order. An event that means nothing where the slot is
    /// (a message that came twice or late) does nothing.
    pub fn on(&mut self, now: Millis, event: Event) -> Vec<Effect> {
        let mut effects = Vec::new();
        if self.phase == Phase::Released {
            return effects;
        }
        let out = &mut effects;
        let due = |at: Option<Millis>| at.is_some_and(|at| now >= at);
        let long_sleep = matches!(event, Event::Slept(span) if span > self.limits.long_sleep);
        // A guest sleeps with its host, so no watchdog runs while the Mac sleeps: each is due that much later.
        if let Event::Slept(span) = event {
            for timer in [&mut self.deadline, &mut self.heartbeat, &mut self.job] {
                *timer = timer.map(|at| at.plus(span));
            }
        }
        match (self.phase, event) {
            (Phase::Reserved, Event::Assigned) => self.start(now, out),
            (Phase::Reserved, Event::Lost | Event::Stop | Event::Evict(_)) => self.release(End::Lost, out),
            (Phase::Reserved, Event::Expired) => {
                self.memory = false;
                self.release(End::Lost, out);
            }

            // The VM's process is gone, wherever the slot was.
            (Phase::Starting, Event::VmExited) => {
                out.push(Effect::StartFailed);
                if self.may_start_again() {
                    self.start(now, out);
                } else {
                    self.release(End::StartFailed, out);
                }
            }
            (Phase::Retrying, Event::VmExited) => self.start(now, out),
            (Phase::Idle | Phase::Running, Event::VmExited) => self.release(End::GuestLost, out),
            (Phase::Finishing | Phase::Stopping | Phase::Killing, Event::VmExited) => {
                self.release(self.end.unwrap_or(End::GuestLost), out);
            }

            (Phase::Starting, Event::VmReady) => {
                out.push(Effect::StartSucceeded);
                self.idle(now);
            }
            // The job started, so the VM was ready: its `ready` was lost on the way.
            (Phase::Starting, Event::JobStarted) => {
                out.push(Effect::StartSucceeded);
                self.run(now);
            }
            (Phase::Starting, Event::Tick) if due(self.deadline) => {
                out.push(Effect::StartFailed);
                out.push(Effect::KillVm);
                if self.may_start_again() {
                    self.phase = Phase::Retrying;
                } else {
                    self.phase = Phase::Killing;
                    self.end = Some(End::StartFailed);
                }
                self.deadline = Some(now.plus(self.limits.kill_again));
            }
            (Phase::Starting, Event::Stop) => self.kill(now, End::Stopped, out),
            (Phase::Starting, Event::Evict(cause)) => self.kill(now, evicted(cause), out),
            (Phase::Starting, Event::Slept(_)) if long_sleep => self.kill(now, End::HostSlept, out),

            // The kill was sent already; only the reason for it changes.
            (Phase::Retrying, Event::Stop) => self.give_up_retry(End::Stopped),
            (Phase::Retrying, Event::Evict(cause)) => self.give_up_retry(evicted(cause)),
            (Phase::Retrying, Event::Slept(_)) if long_sleep => self.give_up_retry(End::HostSlept),
            (Phase::Retrying | Phase::Killing, Event::Tick) if due(self.deadline) => {
                out.push(Effect::KillVm);
                out.push(Effect::Stuck);
                self.deadline = Some(now.plus(self.limits.kill_again));
            }

            (Phase::Idle, Event::JobStarted) => self.run(now),
            // The job is over, so it had started: its start was lost on the way.
            (Phase::Idle, Event::JobCompleted) => {
                self.ran_job = true;
                self.finish(now);
            }
            (Phase::Idle | Phase::Running, Event::Heartbeat) => {
                self.heartbeat = Some(now.plus(self.limits.heartbeat));
            }
            (Phase::Idle | Phase::Running, Event::Tick) if due(self.heartbeat) => {
                self.kill(now, End::GuestLost, out)
            }
            (Phase::Idle, Event::Tick) if due(self.deadline) => self.stop(now, End::NoJob, Millis::ZERO, out),
            (Phase::Idle, Event::Stop) => self.stop(now, End::Stopped, Millis::ZERO, out),
            (Phase::Idle, Event::Evict(cause)) => self.stop(now, evicted(cause), Millis::ZERO, out),
            (Phase::Idle | Phase::Running, Event::Slept(_)) if long_sleep => {
                self.stop(now, End::HostSlept, Millis::ZERO, out);
            }

            (Phase::Idle | Phase::Running, Event::OutOfMemory) => self.oom = true,
            (Phase::Running, Event::JobCompleted) => self.finish(now),
            (Phase::Running, Event::Tick) if due(self.job) => {
                out.push(Effect::Tell(Notice::TimedOut));
                self.stop(now, End::Timeout, self.limits.grace, out);
            }
            (Phase::Running, Event::Evict(cause)) => {
                out.push(Effect::Tell(Notice::Evicted(cause)));
                self.stop(now, evicted(cause), self.limits.grace, out);
            }
            (Phase::Running, Event::Stop) => {
                out.push(Effect::Tell(Notice::Stopped));
                self.stop(now, End::Stopped, self.limits.grace, out);
            }

            // The job is over and its end is decided; all that is left is to get the VM gone sooner.
            (Phase::Finishing, Event::Tick) if due(self.deadline) => self.stop_finished(now, out),
            (Phase::Finishing, Event::Stop | Event::Evict(_)) => self.stop_finished(now, out),
            (Phase::Finishing, Event::Slept(_)) if long_sleep => self.stop_finished(now, out),

            (Phase::Stopping, Event::Tick) if due(self.deadline) => {
                out.push(Effect::KillVm);
                self.phase = Phase::Killing;
                self.deadline = Some(now.plus(self.limits.kill_again));
            }

            _ => {}
        }
        effects
    }

    fn may_start_again(&self) -> bool {
        !self.restored && self.starts < self.limits.starts
    }

    fn start(&mut self, now: Millis, out: &mut Vec<Effect>) {
        self.starts += 1;
        self.phase = Phase::Starting;
        self.deadline = Some(now.plus(self.limits.start));
        out.push(Effect::StartVm);
    }

    fn idle(&mut self, now: Millis) {
        self.phase = Phase::Idle;
        self.deadline = Some(now.plus(self.limits.idle));
        self.heartbeat = Some(now.plus(self.limits.heartbeat));
    }

    fn run(&mut self, now: Millis) {
        self.phase = Phase::Running;
        self.ran_job = true;
        self.deadline = None;
        self.heartbeat = Some(now.plus(self.limits.heartbeat));
        self.job = Some(now.plus(self.limits.job));
    }

    fn finish(&mut self, now: Millis) {
        self.phase = Phase::Finishing;
        self.end = Some(if self.oom { End::Oom } else { End::Ok });
        self.deadline = Some(now.plus(self.limits.finish));
        self.heartbeat = None;
        self.job = None;
    }

    fn stop(&mut self, now: Millis, end: End, grace: Millis, out: &mut Vec<Effect>) {
        self.phase = Phase::Stopping;
        self.end = Some(end);
        self.deadline = Some(now.plus(grace).plus(self.limits.stop));
        self.heartbeat = None;
        self.job = None;
        out.push(Effect::StopVm { grace });
    }

    fn stop_finished(&mut self, now: Millis, out: &mut Vec<Effect>) {
        self.stop(now, self.end.unwrap_or(End::Ok), Millis::ZERO, out);
    }

    fn kill(&mut self, now: Millis, end: End, out: &mut Vec<Effect>) {
        self.phase = Phase::Killing;
        self.end = Some(end);
        self.deadline = Some(now.plus(self.limits.kill_again));
        self.heartbeat = None;
        self.job = None;
        out.push(Effect::KillVm);
    }

    fn give_up_retry(&mut self, end: End) {
        self.phase = Phase::Killing;
        self.end = Some(end);
    }

    fn release(&mut self, end: End, out: &mut Vec<Effect>) {
        if self.starts > 0 {
            out.push(Effect::Unregister);
        }
        if self.memory {
            out.push(Effect::Release);
            self.memory = false;
        }
        out.push(Effect::Ended {
            end,
            ran_job: self.ran_job,
        });
        self.phase = Phase::Released;
        self.end = Some(end);
        self.deadline = None;
        self.heartbeat = None;
        self.job = None;
    }
}

fn evicted(cause: EvictionCause) -> End {
    match cause {
        EvictionCause::Memory => End::EvictedMemory,
        EvictionCause::Disk => End::EvictedDisk,
    }
}
