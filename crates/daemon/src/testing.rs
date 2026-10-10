//! Stand-ins for the two things the scaler does not own: the machine's slots and its virtual machines. For tests.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use cf_ghapp::Clock;

use crate::traits::{Events, HostError, Hosts, JobEvent, Refusal, RunnerSpec, Slots, Ticket};

/// The real time plus whatever a test moved it by.
#[derive(Debug, Default)]
pub struct TestClock(Mutex<Duration>);

impl TestClock {
    /// Moves the clock forward.
    pub fn advance(&self, d: Duration) {
        *self.0.lock().expect("clock") += d;
    }
}

impl Clock for TestClock {
    fn now(&self) -> SystemTime {
        SystemTime::now() + *self.0.lock().expect("clock")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    Reserved(SystemTime),
    Claimed,
}

#[derive(Debug)]
struct SlotState {
    total_gb: u32,
    open: bool,
    reason: String,
    next: u64,
    held: BTreeMap<Ticket, (u32, Held)>,
    released: Vec<Ticket>,
}

/// A memory budget as the real one behaves: reservations that run out, a gate for new jobs, claims that ignore it.
pub struct FakeSlots {
    clock: Arc<TestClock>,
    ttl: Duration,
    state: Mutex<SlotState>,
}

impl FakeSlots {
    /// A machine with `total_gb` of memory for runners, open for new jobs.
    pub fn new(clock: Arc<TestClock>, total_gb: u32, ttl: Duration) -> Arc<FakeSlots> {
        Arc::new(FakeSlots {
            clock,
            ttl,
            state: Mutex::new(SlotState {
                total_gb,
                open: true,
                reason: String::new(),
                next: 1,
                held: BTreeMap::new(),
                released: vec![],
            }),
        })
    }

    /// Opens or closes the machine for new jobs.
    pub fn set_open(&self, open: bool, reason: &str) {
        let mut st = self.state.lock().expect("slots");
        st.open = open;
        st.reason = reason.to_string();
    }

    /// How many GB are held by reservations or claims.
    pub fn used_gb(&self) -> u32 {
        let mut st = self.state.lock().expect("slots");
        self.purge(&mut st);
        st.held.values().map(|(gb, _)| gb).sum()
    }

    /// How many slots are claimed by runners.
    pub fn claimed(&self) -> usize {
        self.state
            .lock()
            .expect("slots")
            .held
            .values()
            .filter(|(_, h)| *h == Held::Claimed)
            .count()
    }

    /// Every ticket given back, in order, so a double release shows.
    pub fn released(&self) -> Vec<Ticket> {
        self.state.lock().expect("slots").released.clone()
    }

    fn purge(&self, st: &mut SlotState) {
        let now = self.clock.now();
        st.held
            .retain(|_, (_, h)| !matches!(h, Held::Reserved(until) if *until <= now));
    }

    fn free(st: &SlotState) -> u32 {
        st.total_gb
            .saturating_sub(st.held.values().map(|(gb, _)| gb).sum())
    }

    fn take(&self, st: &mut SlotState, gb: u32, held: Held) -> Ticket {
        let ticket = Ticket(st.next);
        st.next += 1;
        st.held.insert(ticket, (gb, held));
        ticket
    }
}

impl Slots for FakeSlots {
    fn reserve(&self, size_gb: u32) -> Result<Ticket, Refusal> {
        let mut st = self.state.lock().expect("slots");
        self.purge(&mut st);
        if !st.open {
            return Err(Refusal(st.reason.clone()));
        }
        if Self::free(&st) < size_gb {
            return Err(Refusal("full".into()));
        }
        let until = self.clock.now() + self.ttl;
        Ok(self.take(&mut st, size_gb, Held::Reserved(until)))
    }

    fn release(&self, ticket: Ticket) {
        let mut st = self.state.lock().expect("slots");
        if st.held.remove(&ticket).is_some() {
            st.released.push(ticket);
        }
    }

    fn claim(&self, size_gb: u32, promised: Option<Ticket>) -> Option<Ticket> {
        let mut st = self.state.lock().expect("slots");
        self.purge(&mut st);
        if let Some(ticket) = promised
            && let Some((_, held)) = st.held.get_mut(&ticket)
        {
            *held = Held::Claimed;
            return Some(ticket);
        }
        if Self::free(&st) < size_gb {
            return None;
        }
        Some(self.take(&mut st, size_gb, Held::Claimed))
    }

    fn open_slots(&self, size_gb: u32) -> usize {
        let mut st = self.state.lock().expect("slots");
        self.purge(&mut st);
        if !st.open || size_gb == 0 {
            return 0;
        }
        (Self::free(&st) / size_gb) as usize
    }

    fn state(&self) -> (bool, String) {
        let st = self.state.lock().expect("slots");
        (st.open, st.reason.clone())
    }
}

#[derive(Debug, Default)]
struct HostState {
    started: Vec<(String, i64, Ticket)>,
    stopped: Vec<String>,
    fail_start: usize,
    oom: Vec<String>,
    oom_unanswered: bool,
}

/// Virtual machines that exist as names.
#[derive(Debug, Clone, Default)]
pub struct FakeHosts(Arc<Mutex<HostState>>);

impl FakeHosts {
    /// The next `n` starts fail.
    pub fn fail_start(&self, n: usize) {
        self.0.lock().expect("hosts").fail_start = n;
    }

    /// The machine of this runner lost a process for memory.
    pub fn mark_out_of_memory(&self, runner: &str) {
        self.0.lock().expect("hosts").oom.push(runner.to_string());
    }

    /// The machine does not answer whether it lost a process for memory.
    pub fn leave_out_of_memory_unanswered(&self) {
        self.0.lock().expect("hosts").oom_unanswered = true;
    }

    /// The runners started, as `(name, service runner id, slot)`.
    pub fn started(&self) -> Vec<(String, i64, Ticket)> {
        self.0.lock().expect("hosts").started.clone()
    }

    /// The names of the machines stopped.
    pub fn stopped(&self) -> Vec<String> {
        self.0.lock().expect("hosts").stopped.clone()
    }
}

impl Hosts for FakeHosts {
    async fn start(&self, spec: RunnerSpec) -> Result<String, HostError> {
        let mut st = self.0.lock().expect("hosts");
        if st.fail_start > 0 {
            st.fail_start -= 1;
            return Err(HostError("no machine".into()));
        }
        assert!(
            !spec.jit_config.expose().is_empty(),
            "a runner starts with its configuration"
        );
        st.started.push((spec.name.clone(), spec.runner_id, spec.ticket));
        Ok(spec.name)
    }

    async fn stop(&self, handle: &str) -> Result<(), HostError> {
        self.0.lock().expect("hosts").stopped.push(handle.to_string());
        Ok(())
    }

    async fn out_of_memory(&self, handle: &str) -> Result<bool, HostError> {
        let st = self.0.lock().expect("hosts");
        if st.oom_unanswered {
            return Err(HostError("the VM monitor does not answer".into()));
        }
        Ok(st.oom.iter().any(|n| n == handle))
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
