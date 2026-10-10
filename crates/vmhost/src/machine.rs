//! A job's life as a state machine: what the vm-host does on every event, and every wait it refuses to wait
//! forever. No clock, no thread and no file here: [`crate::run`] feeds it and does what it says.

use crate::state::{End, Ended, Stage};
use cf_guest_proto::{GuestEnd, GuestMessage, OomEvent, PROTOCOL, StopReason, Summary, Usage};
use std::time::Duration;

/// How long a vm-host waits at each stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// From a machine's start to the guest's `ready`; then the machine is booted once more, then given up.
    pub ready: Duration,
    /// Between two heartbeats of the guest.
    pub heartbeat: Duration,
    /// The job's whole run, from `ready`.
    pub deadline: Duration,
    /// What a job at its deadline gets to end by itself.
    pub deadline_grace: Duration,
    /// Past a stop's own grace, for the guest to say how it ended.
    pub stop_slack: Duration,
    /// From the workload's end to the machine's.
    pub done: Duration,
    /// For a forced stop to end the machine.
    pub kill: Duration,
    /// How often a machine is booted in all.
    pub boots: u32,
    /// The least time between two state files that differ only in what the job uses.
    pub publish: Duration,
    /// The most a job's machines write to the console log on the Mac; a machine that writes more is stopped.
    pub console_bytes: u64,
}

impl Limits {
    /// The Runner's waits, for a job with this deadline.
    pub fn for_deadline(deadline: Duration) -> Self {
        Self {
            ready: Duration::from_secs(20),
            heartbeat: Duration::from_secs(60),
            deadline,
            deadline_grace: Duration::from_secs(20),
            stop_slack: Duration::from_secs(10),
            done: Duration::from_secs(120),
            kill: Duration::from_secs(10),
            boots: 2,
            publish: Duration::from_secs(10),
            console_bytes: 16 << 20,
        }
    }

    /// The longest a job runs, from its workload's start: at the deadline it is told to end and has
    /// `deadline_grace` to; a guest that has not said how it ended `stop_slack` later has its machine stopped by
    /// force. A stop from anybody else, whatever grace it asks for, ends the job no later than this.
    pub fn longest_run(&self) -> Duration {
        self.deadline + self.deadline_grace + self.stop_slack
    }

    /// The longest a vm-host lives past its workload's start: [`Limits::longest_run`], then `done` for a machine
    /// that stays up after its guest's account, then `kill` for one that ignores the forced stop.
    pub fn longest_life(&self) -> Duration {
        self.longest_run() + self.done + self.kill
    }
}

/// What happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The machine did not start.
    StartFailed(String),
    /// The channel to the guest is open.
    Connected,
    /// The guest said something.
    Guest(GuestMessage),
    /// The channel closed, or carried what is no message.
    ChannelClosed,
    /// The machine is gone, for this reason.
    VmExited(String),
    /// The daemon (or a signal) asks the job to end.
    StopAsked {
        /// Why.
        reason: StopReason,
        /// What the job gets to end by itself.
        grace: Duration,
    },
    /// The machines of this job wrote more to the console than [`Limits::console_bytes`].
    ConsoleFull,
    /// Time went by.
    Tick,
}

/// What to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Let go of any machine there is and start a fresh one.
    Boot,
    /// Send the guest its task.
    SendStart,
    /// Tell the guest to end the job.
    SendStop {
        /// Why.
        reason: StopReason,
        /// What the job gets to end by itself.
        grace: Duration,
    },
    /// Close the channel: the guest has said its last and powers off when it sees this.
    CloseChannel,
    /// Stop the machine by force.
    StopVm,
    /// Write the state file.
    Publish,
    /// The job is over: write the state, remove its files, exit.
    Finish,
}

const CONSOLE_FULL: &str = "the machine wrote more to its console than a job's log may hold";

/// One job's state machine. Times are since the vm-host started.
#[derive(Debug)]
pub struct Machine {
    limits: Limits,
    stage: Stage,
    /// When the stage began, and for a boot, when this machine started.
    since: Duration,
    boots: u32,
    ready_at: Option<Duration>,
    heard: Duration,
    published: Duration,
    /// The stop in force: its reason and when the guest has had its grace and slack.
    stop: Option<(StopReason, Duration)>,
    boot_ms: Option<u64>,
    usage: Option<Usage>,
    oom: Option<OomEvent>,
    end: Option<(End, Option<String>, Option<Summary>)>,
}

impl Machine {
    /// A job at its start, and the first thing to do.
    pub fn new(limits: Limits, now: Duration) -> (Self, Vec<Action>) {
        let machine = Self {
            limits,
            stage: Stage::Booting,
            since: now,
            boots: 1,
            ready_at: None,
            heard: now,
            published: now,
            stop: None,
            boot_ms: None,
            usage: None,
            oom: None,
            end: None,
        };
        (machine, vec![Action::Publish, Action::Boot])
    }

    /// Where the job is.
    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// How often a machine was booted.
    pub fn boots(&self) -> u32 {
        self.boots
    }

    /// Milliseconds from the last machine's start to the workload's.
    pub fn boot_ms(&self) -> Option<u64> {
        self.boot_ms
    }

    /// What the job used at the guest's last heartbeat.
    pub fn usage(&self) -> Option<&Usage> {
        self.usage.as_ref()
    }

    /// The first out-of-memory stop the guest reported.
    pub fn oom(&self) -> Option<&OomEvent> {
        self.oom.as_ref()
    }

    /// The end, once it is known, as the state file has it.
    pub fn ended(&self, at: u64) -> Option<Ended> {
        self.end.as_ref().map(|(end, message, summary)| Ended {
            end: *end,
            message: message.clone(),
            summary: summary.clone(),
            at,
        })
    }

    fn enter(&mut self, stage: Stage, now: Duration) {
        self.stage = stage;
        self.since = now;
    }

    /// Ends the job for this reason, unless it has an end already, and stops the machine by force.
    fn give_up(&mut self, end: End, message: impl Into<Option<String>>, now: Duration) -> Vec<Action> {
        self.end.get_or_insert((end, message.into(), None));
        self.kill(now)
    }

    /// Stops the machine by force and waits for it to be gone.
    fn kill(&mut self, now: Duration) -> Vec<Action> {
        self.enter(Stage::Killing, now);
        vec![Action::StopVm, Action::Publish]
    }

    /// The machine is gone: nothing is left to wait for.
    fn finish(&mut self, end: End, message: impl Into<Option<String>>, now: Duration) -> Vec<Action> {
        self.end.get_or_insert((end, message.into(), None));
        self.enter(Stage::Ended, now);
        vec![Action::Finish]
    }

    /// A boot that came to nothing: once more, or the job's end.
    fn boot_failed(&mut self, why: String, now: Duration) -> Vec<Action> {
        if self.boots < self.limits.boots {
            self.boots += 1;
            self.since = now;
            vec![Action::Boot, Action::Publish]
        } else {
            self.give_up(End::BootFailed, why, now)
        }
    }

    /// The end a machine lost mid-job has: the stop that was under way, else a lost guest.
    fn lost(&self) -> End {
        self.stop
            .map_or(End::GuestLost, |(reason, _)| End::of_stop(reason))
    }

    fn stop(&mut self, reason: StopReason, grace: Duration, now: Duration) -> Vec<Action> {
        // No stop gives a job longer than its deadline would have: whoever asks, the longest run holds. (The
        // deadline's own stop comes with the first look at the clock past it, and has its whole grace from there.)
        let deadline = self.ready_at.unwrap_or(now) + self.limits.deadline;
        let latest = now.max(deadline) + self.limits.deadline_grace + self.limits.stop_slack;
        let until = (now + grace + self.limits.stop_slack).min(latest);
        let grace = grace.min(until.saturating_sub(now + self.limits.stop_slack));
        // The first reason is the job's; a later stop can only make it sooner, and one that does not is not sent.
        let sooner = self.stop.is_none_or(|(_, existing)| until < existing);
        self.stop = Some(match self.stop {
            Some((first, existing)) => (first, existing.min(until)),
            None => (reason, until),
        });
        self.enter(Stage::Stopping, now);
        if sooner {
            vec![Action::SendStop { reason, grace }, Action::Publish]
        } else {
            Vec::new()
        }
    }

    fn summary(&mut self, summary: Summary, now: Duration) -> Vec<Action> {
        self.usage = Some(summary.usage);
        if self.oom.is_none() {
            self.oom = summary.oom.clone();
        }
        // Why a job was stopped is the host's to say: the guest only says that it was. One that names a stop
        // nobody asked for is not believed.
        let (end, message) = match (summary.end, self.stop) {
            (GuestEnd::Stopped(_), Some((reason, _))) => (End::of_stop(reason), None),
            (GuestEnd::Stopped(_), None) => (
                End::GuestLost,
                Some("the guest said the job was stopped, which nobody had asked for".to_string()),
            ),
            (GuestEnd::Exited, _) => (End::of_summary(&summary), None),
        };
        self.end.get_or_insert((end, message, Some(summary)));
        self.enter(Stage::Done, now);
        vec![Action::CloseChannel, Action::Publish]
    }

    /// Takes one event at time `now` and says what to do. Every event is also a look at the clock, so no wait
    /// depends on a [`Event::Tick`] finding room between a guest's messages.
    pub fn handle(&mut self, event: Event, now: Duration) -> Vec<Action> {
        let tick = event == Event::Tick;
        let mut actions = self.take(event, now);
        if !tick {
            actions.extend(self.tick(now));
        }
        actions
    }

    fn take(&mut self, event: Event, now: Duration) -> Vec<Action> {
        match (self.stage, event) {
            (Stage::Ended, _) => Vec::new(),
            // A machine that is gone ends every stage but a boot, which may have another try.
            (Stage::Booting, Event::VmExited(why)) if self.boots < self.limits.boots => {
                self.boot_failed(why, now)
            }
            // At the last try the machine is already gone: nothing to stop and nothing to wait for.
            (Stage::Booting, Event::VmExited(why)) => self.finish(End::BootFailed, why, now),
            // No machine came of it: at the last try there is nothing to stop and nothing to wait for.
            (Stage::Booting, Event::StartFailed(why)) if self.boots < self.limits.boots => {
                self.boot_failed(why, now)
            }
            (Stage::Booting, Event::ChannelClosed) => {
                self.boot_failed("the guest closed the channel before it was ready".into(), now)
            }
            (Stage::Running | Stage::Stopping, Event::VmExited(why)) => self.finish(
                self.lost(),
                format!("the machine went without a word: {why}"),
                now,
            ),
            (Stage::Done | Stage::Killing, Event::VmExited(_)) => self.finish(End::GuestLost, None, now),
            (_, Event::StartFailed(why)) => self.finish(End::BootFailed, why, now),

            (Stage::Booting, Event::Connected) => vec![Action::SendStart],
            (_, Event::Connected) => Vec::new(),

            (Stage::Booting, Event::StopAsked { reason, .. }) => {
                self.give_up(End::of_stop(reason), None, now)
            }
            (Stage::Running | Stage::Stopping, Event::StopAsked { reason, grace }) => {
                self.stop(reason, grace, now)
            }
            // The job has its end; what a stop is still for is the machine and its memory, now.
            (Stage::Done, Event::StopAsked { .. }) => self.kill(now),
            (Stage::Killing, Event::StopAsked { .. }) => Vec::new(),

            (Stage::Booting | Stage::Running | Stage::Stopping, Event::Guest(message)) => {
                self.guest(message, now)
            }
            (Stage::Done | Stage::Killing, Event::Guest(_)) => Vec::new(),

            (Stage::Running | Stage::Stopping, Event::ChannelClosed) => self.give_up(
                self.lost(),
                "the guest closed the channel before it said how the job ended".to_string(),
                now,
            ),
            (Stage::Done | Stage::Killing, Event::ChannelClosed) => Vec::new(),

            // Another boot would only write more.
            (Stage::Booting, Event::ConsoleFull) => {
                self.give_up(End::BootFailed, CONSOLE_FULL.to_string(), now)
            }
            (Stage::Running | Stage::Stopping, Event::ConsoleFull) => {
                self.give_up(self.lost(), CONSOLE_FULL.to_string(), now)
            }
            (Stage::Done, Event::ConsoleFull) => self.kill(now),
            (Stage::Killing, Event::ConsoleFull) => Vec::new(),

            (_, Event::Tick) => self.tick(now),
        }
    }

    fn guest(&mut self, message: GuestMessage, now: Duration) -> Vec<Action> {
        self.heard = now;
        match message {
            // A supervisor greets once, before its task. Whatever greets a job under way is not ours any more.
            GuestMessage::Hello { .. } if self.stage != Stage::Booting => self.give_up(
                self.lost(),
                "the guest greeted again in the middle of its job".to_string(),
                now,
            ),
            GuestMessage::Hello { protocol, .. } if protocol != PROTOCOL => {
                // The guest is the one this binary carries; another try would meet the same guest.
                self.give_up(
                    End::BootFailed,
                    format!("the guest speaks protocol {protocol}, the host {PROTOCOL}"),
                    now,
                )
            }
            GuestMessage::Hello { .. } => Vec::new(),
            GuestMessage::Ready { .. } if self.stage == Stage::Booting => {
                self.boot_ms = Some(now.saturating_sub(self.since).as_millis() as u64);
                self.ready_at = Some(now);
                self.enter(Stage::Running, now);
                vec![Action::Publish]
            }
            GuestMessage::Ready { .. } => Vec::new(),
            GuestMessage::Heartbeat(usage) => {
                self.usage = Some(usage);
                if now.saturating_sub(self.published) >= self.limits.publish {
                    self.published = now;
                    vec![Action::Publish]
                } else {
                    Vec::new()
                }
            }
            GuestMessage::Oom(event) => {
                if self.oom.is_some() {
                    return Vec::new();
                }
                self.oom = Some(event);
                vec![Action::Publish]
            }
            GuestMessage::Exited(summary) => self.summary(summary, now),
            GuestMessage::Failed { stage, message } => {
                let end = if self.stage == Stage::Booting {
                    End::BootFailed
                } else {
                    End::Failed
                };
                self.give_up(end, format!("the guest failed at {stage}: {message}"), now)
            }
        }
    }

    fn tick(&mut self, now: Duration) -> Vec<Action> {
        let waited = now.saturating_sub(self.since);
        match self.stage {
            Stage::Booting if waited >= self.limits.ready => self.boot_failed(
                format!("the guest was not ready within {} s", self.limits.ready.as_secs()),
                now,
            ),
            Stage::Running | Stage::Stopping if now.saturating_sub(self.heard) >= self.limits.heartbeat => {
                self.give_up(
                    self.lost(),
                    format!("the guest was silent for {} s", self.limits.heartbeat.as_secs()),
                    now,
                )
            }
            Stage::Stopping if self.stop.is_some_and(|(_, until)| now >= until) => self.give_up(
                self.lost(),
                "the guest did not end the job in the time it had".to_string(),
                now,
            ),
            Stage::Running
                if self
                    .ready_at
                    .is_some_and(|ready| now.saturating_sub(ready) >= self.limits.deadline) =>
            {
                self.stop(StopReason::Timeout, self.limits.deadline_grace, now)
            }
            Stage::Done if waited >= self.limits.done => self.kill(now),
            // A machine that does not obey a forced stop goes with this process.
            Stage::Killing if waited >= self.limits.kill => {
                self.enter(Stage::Ended, now);
                vec![Action::Finish]
            }
            _ => Vec::new(),
        }
    }
}
