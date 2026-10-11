use cf_budget::{GIB, Host, HostError, HostFacts, HostReading, Millis, Now, Pressure, Thermal};

/// A host a test scripts. It starts as a calm Mac on the mains with all its memory available and its person just
/// gone; a test moves the clocks and changes what the next readings say.
#[derive(Clone, Debug)]
pub struct FakeHost {
    facts: Result<HostFacts, HostError>,
    now: Now,
    reading: HostReading,
    failure: Option<HostError>,
    reads: u64,
    read_takes: (Millis, Millis),
}

impl FakeHost {
    /// A calm Mac with this much memory and so many cores.
    pub fn new(facts: HostFacts) -> FakeHost {
        let now = Now {
            awake: Millis::secs(1000),
            elapsed: Millis::secs(1000),
        };
        FakeHost {
            facts: Ok(facts),
            now,
            reading: HostReading {
                at: now,
                available_bytes: facts.total_bytes,
                pressure: Pressure::Normal,
                swap_used_bytes: 0,
                swapouts: 0,
                idle: Millis::ZERO,
                thermal: Thermal::Nominal,
                on_battery: false,
                disk_free_bytes: 1024 * GIB,
            },
            failure: None,
            reads: 0,
            read_takes: (Millis::ZERO, Millis::ZERO),
        }
    }

    /// Time passes with the Mac awake and nobody at it: both clocks and the idle time move on.
    pub fn pass(&mut self, span: Millis) {
        self.now.awake = self.now.awake.plus(span);
        self.now.elapsed = self.now.elapsed.plus(span);
        self.reading.idle = self.reading.idle.plus(span);
    }

    /// Both clocks move on and nothing else does: time too short for the person's idle time to matter.
    fn move_on(&mut self, span: Millis) {
        self.now.awake = self.now.awake.plus(span);
        self.now.elapsed = self.now.elapsed.plus(span);
    }

    /// The Mac sleeps: only the clock that counts sleep moves on.
    pub fn sleep(&mut self, span: Millis) {
        self.now.elapsed = self.now.elapsed.plus(span);
    }

    /// The person touches the keyboard or the mouse.
    pub fn touch(&mut self) {
        self.reading.idle = Millis::ZERO;
    }

    /// Sets both clocks, also backwards: what a broken clock or a restored snapshot would show.
    pub fn set_now(&mut self, now: Now) {
        self.now = now;
    }

    /// Changes what the readings say from now on. The time of a reading is always the host's own.
    pub fn set(&mut self, change: impl FnOnce(&mut HostReading)) {
        change(&mut self.reading);
    }

    /// Every read fails with this error until `None` is set.
    pub fn fail(&mut self, error: Option<HostError>) {
        self.failure = error;
    }

    /// What `facts` answers from now on.
    pub fn set_facts(&mut self, facts: Result<HostFacts, HostError>) {
        self.facts = facts;
    }

    /// A reading takes time from now on, as a real Mac's does: both clocks move on by `before` until the reading
    /// gets its time, and by `after` until `read` answers.
    pub fn read_takes(&mut self, before: Millis, after: Millis) {
        self.read_takes = (before, after);
    }

    /// How often `read` was called.
    pub fn reads(&self) -> u64 {
        self.reads
    }
}

impl Host for FakeHost {
    fn facts(&self) -> Result<HostFacts, HostError> {
        self.facts.clone()
    }

    fn now(&self) -> Now {
        self.now
    }

    fn read(&mut self) -> Result<HostReading, HostError> {
        self.reads += 1;
        let (before, after) = self.read_takes;
        self.move_on(before);
        let at = self.now;
        self.move_on(after);
        match &self.failure {
            Some(error) => Err(error.clone()),
            None => Ok(HostReading { at, ..self.reading }),
        }
    }
}
