use std::collections::{BTreeSet, VecDeque};

use crate::{HostReading, Millis, Mode, Now, Pressure, Profile, Settings, Thermal};

/// What the Mac is doing about new slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// Slots open up to the budget.
    Open,
    /// No new slot; running jobs go on. Memory is short, the person is there, or the calm time is not over.
    Tight,
    /// No new slot for a reason that is not memory: no reading, heat, battery, disk, or a hold someone set.
    Hold,
    /// The Mac is out of memory or disk: a job of ours stops if stopping one gives the Mac something back
    /// ([`Observed::evict`](crate::Observed::evict) names it).
    Evict,
}

/// Something a reading shows that closes the Mac. Slots open again 60 s after the last one is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Trouble {
    /// No reading, an old one, or a gap in time before it.
    Blind,
    /// Memory pressure at warning or critical.
    Pressure,
    /// macOS swapped pages out within the last 30 s.
    Swapping,
    /// The Mac is under its floor, or was and is not yet clear of it by the margin.
    Short,
    /// The thermal state is serious or critical.
    Hot,
    /// The Mac runs on its battery.
    Battery,
    /// The disk for job disks is nearly full.
    DiskLow,
}

impl Trouble {
    /// A short fixed name for reports and logs.
    pub const fn name(self) -> &'static str {
        match self {
            Trouble::Blind => "no-reading",
            Trouble::Pressure => "memory-pressure",
            Trouble::Swapping => "swapping",
            Trouble::Short => "under-floor",
            Trouble::Hot => "thermal",
            Trouble::Battery => "battery",
            Trouble::DiskLow => "disk",
        }
    }

    /// Whether this is a memory signal ([`Level::Tight`]) or another reason to wait ([`Level::Hold`]).
    pub(crate) const fn is_memory(self) -> bool {
        matches!(self, Trouble::Pressure | Trouble::Swapping | Trouble::Short)
    }
}

/// A reason to open no slot that someone set, not a reading. It ends when it is lifted, with no calm time after.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Hold {
    /// Paused from the tray or the console.
    Paused,
    /// A quiet hour.
    QuietHour,
    /// VMs failed to start several times in a row.
    StartFailures,
    /// GitHub refuses the Actions runner's version, or there is no usable image.
    NoImage,
    /// The Runner is shutting down or updating.
    Draining,
}

impl Hold {
    /// A short fixed name for reports and logs.
    pub const fn name(self) -> &'static str {
        match self {
            Hold::Paused => "paused",
            Hold::QuietHour => "quiet-hour",
            Hold::StartFailures => "start-failures",
            Hold::NoImage => "no-image",
            Hold::Draining => "draining",
        }
    }
}

/// What the readings so far say: the part of the budget that remembers.
#[derive(Clone, Debug, Default)]
pub(crate) struct Watch {
    /// The newest reading that could be used.
    reading: Option<HostReading>,
    /// When `observe` last ran.
    observed: Option<Now>,
    /// Since when nothing has been wrong.
    calm_since: Option<Millis>,
    /// What is wrong now.
    troubles: BTreeSet<Trouble>,
    /// Since when pressure has been at warning or above, and at critical.
    warning_since: Option<Millis>,
    critical_since: Option<Millis>,
    /// The swap-out counter of the reading before, and when it last moved.
    swapouts: Option<u64>,
    last_swapout: Option<Millis>,
    /// Swap in use over the growth window, oldest first, at most one sample a second.
    swap_used: VecDeque<(Millis, u64)>,
    /// Whether the Mac went under its floor and has not cleared the margin since.
    short: bool,
}

impl Watch {
    /// Takes one reading (or the lack of one) at `now`. Returns how long the Mac slept since the reading before.
    pub(crate) fn observe(&mut self, settings: &Settings, now: Now, reading: Option<HostReading>) -> Millis {
        let mut slept = Millis::ZERO;
        if let Some(before) = self.observed {
            let passed = now.elapsed.since(before.elapsed);
            slept = passed.since(now.awake.since(before.awake));
            // A gap is time nobody watched: whatever was known before it proves nothing about now.
            if now.elapsed < before.elapsed || now.awake < before.awake || passed > settings.stale_after {
                self.forget();
            }
        }
        self.observed = Some(now);

        // A reading from another moment than this one is no reading.
        let reading = reading.filter(|r| {
            r.at.elapsed <= now.elapsed && now.elapsed.since(r.at.elapsed) <= settings.stale_after
        });
        self.reading = reading;
        self.troubles.clear();
        match reading {
            None => {
                self.troubles.insert(Trouble::Blind);
                // Without a reading there is no evidence that pressure went on.
                self.warning_since = None;
                self.critical_since = None;
                self.swapouts = None;
                self.swap_used.clear();
            }
            Some(reading) => self.judge(settings, now.elapsed, &reading),
        }
        if self.troubles.is_empty() {
            self.calm_since.get_or_insert(now.elapsed);
        } else {
            self.calm_since = None;
        }
        slept
    }

    fn judge(&mut self, settings: &Settings, now: Millis, reading: &HostReading) {
        if reading.pressure >= Pressure::Warning {
            self.warning_since.get_or_insert(now);
            self.troubles.insert(Trouble::Pressure);
        } else {
            self.warning_since = None;
        }
        if reading.pressure == Pressure::Critical {
            self.critical_since.get_or_insert(now);
        } else {
            self.critical_since = None;
        }

        // The counter only ever rises; any other value than the last one means pages went out (or the count
        // started again, which is no better).
        if self.swapouts.is_some_and(|before| before != reading.swapouts) {
            self.last_swapout = Some(now);
        }
        self.swapouts = Some(reading.swapouts);
        if self
            .last_swapout
            .is_some_and(|at| now.since(at) < settings.swap_quiet)
        {
            self.troubles.insert(Trouble::Swapping);
        }

        if self
            .swap_used
            .back()
            .is_none_or(|(at, _)| now.since(*at) >= Millis::secs(1))
        {
            self.swap_used.push_back((now, reading.swap_used_bytes));
        }
        // Keep one sample at or before the start of the window: growth is measured from it.
        while self
            .swap_used
            .get(1)
            .is_some_and(|(at, _)| now.since(*at) >= settings.swap_growth_window)
        {
            self.swap_used.pop_front();
        }

        let floor = profile_for(settings, Some(reading)).floor_bytes;
        if reading.available_bytes < floor {
            self.short = true;
        } else if reading.available_bytes >= floor.saturating_add(settings.recover_margin_bytes) {
            self.short = false;
        }
        if self.short {
            self.troubles.insert(Trouble::Short);
        }

        if reading.thermal >= Thermal::Serious {
            self.troubles.insert(Trouble::Hot);
        }
        if reading.on_battery {
            self.troubles.insert(Trouble::Battery);
        }
        if reading.disk_free_bytes < settings.disk_hold_bytes {
            self.troubles.insert(Trouble::DiskLow);
        }
    }

    /// Drops everything that was learned over time. What a single reading shows is learned again from the next.
    fn forget(&mut self) {
        self.calm_since = None;
        self.warning_since = None;
        self.critical_since = None;
        self.swapouts = None;
        self.last_swapout = None;
        self.swap_used.clear();
    }

    /// When `observe` last ran.
    pub(crate) fn observed(&self) -> Option<Now> {
        self.observed
    }

    /// The newest reading, if it was taken no longer than [`Settings::stale_after`] before `now`.
    pub(crate) fn fresh(&self, settings: &Settings, now: Now) -> Option<&HostReading> {
        let observed = self.observed?;
        let reading = self.reading.as_ref()?;
        let in_order = now.elapsed >= observed.elapsed && now.awake >= observed.awake;
        (in_order && now.elapsed.since(reading.at.elapsed) <= settings.stale_after).then_some(reading)
    }

    /// What is wrong now, worst first.
    pub(crate) fn troubles(&self) -> impl Iterator<Item = Trouble> + '_ {
        self.troubles.iter().copied()
    }

    /// How much longer nothing may go wrong before slots open; zero when the calm time is over. `None` while
    /// something is wrong.
    ///
    /// The calm time is counted to the last look at the Mac, not to the moment of asking: what came after that
    /// look is time nobody watched, and it proves nothing.
    pub(crate) fn calm_left(&self, settings: &Settings) -> Option<Millis> {
        let since = self.calm_since?;
        let watched_to = self.observed?.elapsed;
        Some(settings.calm_for.since(watched_to.since(since)))
    }

    /// Whether memory is so short that a job has to stop: critical pressure that lasted, or pressure with swap
    /// growing fast through the whole window.
    pub(crate) fn memory_emergency(&self, settings: &Settings, now: Now) -> bool {
        let now = now.elapsed;
        if self
            .critical_since
            .is_some_and(|since| now.since(since) >= settings.critical_for)
        {
            return true;
        }
        let window = settings.swap_growth_window;
        if !self.warning_since.is_some_and(|since| now.since(since) >= window) {
            return false;
        }
        let Some((_, used_now)) = self.swap_used.back() else {
            return false;
        };
        // The newest sample that is at least a window old.
        let start = self
            .swap_used
            .iter()
            .rev()
            .find(|(at, _)| now.since(*at) >= window);
        start.is_some_and(|(_, used_then)| used_now.saturating_sub(*used_then) >= settings.swap_growth_bytes)
    }
}

/// The limits in force for a reading. Without one, a workstation counts as in use.
pub(crate) fn profile_for<'a>(settings: &'a Settings, reading: Option<&HostReading>) -> &'a Profile {
    if person_there(settings, reading) {
        &settings.in_use
    } else {
        &settings.away
    }
}

/// Whether the person counts as there. Never on a dedicated Mac; on a workstation unless a reading shows the
/// keyboard and the mouse at rest for long enough.
pub(crate) fn person_there(settings: &Settings, reading: Option<&HostReading>) -> bool {
    match settings.mode {
        Mode::Dedicated => false,
        Mode::Workstation => !reading.is_some_and(|r| r.idle >= settings.idle_after),
    }
}
