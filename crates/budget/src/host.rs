use thiserror::Error;

/// A span or a point of time in milliseconds. Points count from an arbitrary start; only differences matter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Millis(pub u64);

impl Millis {
    /// No time at all.
    pub const ZERO: Millis = Millis(0);

    /// So many seconds.
    pub const fn secs(seconds: u64) -> Millis {
        Millis(seconds.saturating_mul(1000))
    }

    /// So many minutes.
    pub const fn mins(minutes: u64) -> Millis {
        Millis::secs(minutes.saturating_mul(60))
    }

    /// The time from `earlier` to this point; zero when `earlier` is the later one.
    pub const fn since(self, earlier: Millis) -> Millis {
        Millis(self.0.saturating_sub(earlier.0))
    }

    /// This point moved later by `span`.
    pub const fn plus(self, span: Millis) -> Millis {
        Millis(self.0.saturating_add(span.0))
    }
}

/// Two clocks read together. Neither is the wall clock, so setting the Mac's date moves neither.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Now {
    /// Time the Mac was awake. It stands still during sleep.
    pub awake: Millis,
    /// Time that passed, sleep included. Every duration in the budget is measured on this one.
    pub elapsed: Millis,
}

/// macOS's memory pressure level (`kern.memorystatus_vm_pressure_level`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Pressure {
    /// Level 1.
    Normal,
    /// Level 2: macOS is compressing and swapping to keep up.
    Warning,
    /// Level 4: macOS is about to stop processes.
    Critical,
}

/// macOS's thermal state, as `NSProcessInfo.thermalState` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Thermal {
    /// No throttling.
    Nominal,
    /// Slightly raised; fans may run.
    Fair,
    /// Performance is being held back.
    Serious,
    /// The Mac needs to cool down now.
    Critical,
}

/// What does not change while the Mac is up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostFacts {
    /// Installed memory (`hw.memsize`).
    pub total_bytes: u64,
    /// Logical CPU cores (`hw.ncpu`).
    pub cores: u32,
}

/// One reading of the host, taken about once a second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostReading {
    /// When it was taken.
    pub at: Now,
    /// Memory macOS can hand out without compressing or swapping anyone's pages: free, file cache and purgeable.
    /// This is what Activity Monitor shows as physical memory minus "Memory Used". It is not
    /// `kern.memorystatus_level`, which also counts every program's own pages that are not compressed yet.
    pub available_bytes: u64,
    /// The memory pressure level.
    pub pressure: Pressure,
    /// Swap in use.
    pub swap_used_bytes: u64,
    /// Pages swapped out since boot. Only that it changed matters.
    pub swapouts: u64,
    /// Time since the person last used the keyboard or the mouse.
    pub idle: Millis,
    /// The thermal state.
    pub thermal: Thermal,
    /// Whether the Mac runs on its battery.
    pub on_battery: bool,
    /// Free space on the volume that holds the job disks.
    pub disk_free_bytes: u64,
}

/// The host's battery, for the console's page: the budget itself reads only [`HostReading::on_battery`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Battery {
    /// Whether the host draws from its battery and not from a charger.
    pub on_battery: bool,
    /// The charge, 0 to 100.
    pub percent: u32,
    /// Whether it is charging.
    pub charging: bool,
}

/// A reading that could not be taken. The budget treats every one of these the same way: the Mac is closed.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum HostError {
    /// The system refused a call.
    #[error("{what} failed with code {code}")]
    Call { what: &'static str, code: i64 },
    /// The system answered with something that cannot be right.
    #[error("{what} gave a value that cannot be right: {value}")]
    Implausible { what: &'static str, value: String },
}

/// The machine the Runner runs on, as far as the budget needs it. Another OS implements this trait; tests script it.
pub trait Host {
    /// What does not change while the Mac is up. An error here is a reason not to start.
    fn facts(&self) -> Result<HostFacts, HostError>;
    /// Both clocks, now.
    fn now(&self) -> Now;
    /// One reading. An error is a result, not a panic: hand `None` to `Budget::observe` and the Mac closes.
    fn read(&mut self) -> Result<HostReading, HostError>;
}
