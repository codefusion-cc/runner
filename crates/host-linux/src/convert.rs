use cf_budget::{Battery, HostError, HostReading, Millis, Now, Pressure, Thermal};

use crate::parse::{CpuTicks, Psi};

/// What Linux answered, before anything is made of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Raw {
    /// `MemTotal` of `/proc/meminfo`.
    pub total_bytes: u64,
    /// `MemAvailable` of `/proc/meminfo`.
    pub available_bytes: u64,
    /// `SwapTotal` of `/proc/meminfo`. zram counts as swap.
    pub swap_total_bytes: u64,
    /// `SwapFree` of `/proc/meminfo`.
    pub swap_free_bytes: u64,
    /// `pswpout` of `/proc/vmstat`: pages written to swap since the host started.
    pub swapouts: u64,
    /// `/proc/pressure/memory`; `None` on a kernel without pressure stall information (built without it, or with
    /// it off by default and no `psi=1`). Never zeros in its place: an absent signal is not a calm one.
    pub memory_pressure: Option<Psi>,
    /// The thermal zones that are on, with what the kernel says of their temperatures.
    pub zones: Vec<Zone>,
    /// The power supplies the kernel knows (`/sys/class/power_supply`).
    pub supplies: Vec<Supply>,
    /// `statvfs` of the state folder: blocks an unprivileged process may use, in bytes.
    pub disk_free_bytes: u64,
}

/// One thermal zone (`/sys/class/thermal/thermal_zone*`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Zone {
    /// Its `type`, such as `soc-thermal`.
    pub name: String,
    /// Its temperature, in thousandths of a degree Celsius.
    pub millidegrees: i32,
    /// Its trip points: where the kernel starts a fan, holds the CPUs back, or shuts the host down.
    pub trips: Vec<Trip>,
}

/// A trip point of a thermal zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trip {
    /// What the kernel does there.
    pub kind: TripKind,
    /// Where, in thousandths of a degree Celsius.
    pub millidegrees: i32,
}

/// What the kernel does at a trip point (`trip_point_*_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TripKind {
    /// `active`: a fan runs.
    Active,
    /// `passive`: the CPUs are held back. An RK3588 has one at 85 C, and one at 60 C where its power allocator
    /// starts to watch.
    Passive,
    /// `hot`: the platform is told it is about to overheat.
    Hot,
    /// `critical`: the kernel shuts the host down.
    Critical,
}

impl TripKind {
    /// The kind a `trip_point_*_type` file names.
    pub fn named(name: &str) -> Option<TripKind> {
        match name.trim() {
            "active" => Some(TripKind::Active),
            "passive" => Some(TripKind::Passive),
            "hot" => Some(TripKind::Hot),
            "critical" => Some(TripKind::Critical),
            _ => None,
        }
    }
}

/// One power supply (`/sys/class/power_supply/*`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Supply {
    /// Its folder's name, such as `BAT0` or `AC`.
    pub name: String,
    /// Whether its `type` is `Battery`. Every other type (`Mains`, `USB`, `UPS`, ...) powers the host from outside.
    pub battery: bool,
    /// Whether it powers the host and not a device: `scope` is not `Device`, as it is for a wireless mouse's battery.
    pub system: bool,
    /// `online` of an outside supply: whether it powers the host now.
    pub online: Option<bool>,
    /// `present` of a battery: `false` for an empty battery bay.
    pub present: Option<bool>,
    /// `status` of a battery: `Charging`, `Discharging`, `Not charging`, `Full` or `Unknown`.
    pub status: Option<String>,
    /// `capacity` of a battery: its charge, 0 to 100.
    pub capacity: Option<u32>,
}

/// From this share of time with some task stalled on memory over 10 s (PSI `some avg10`, in hundredths of a
/// percent), memory counts as under pressure: the kernel reclaims or swaps in in the way of programs a tenth of
/// the time. Like macOS's warning, it closes new slots, and with swap growing fast it stops a job.
pub const PRESSURE_WARNING: u32 = 10_00;

/// From this share of time with every task stalled on memory at once (PSI `full avg10`), memory counts as
/// critical: the host does a fifth less work than it could. Like macOS's critical level, it stops a job when it
/// lasts ([`Settings::critical_for`](cf_budget::Settings::critical_for)). Well under the 50% to 60% at which
/// systemd-oomd stops whole programs.
pub const PRESSURE_CRITICAL: u32 = 20_00;

/// The budget's pressure level from PSI. Without PSI it is [`Pressure::Normal`]: the budget then decides from
/// `MemAvailable` against its floor and from swap-outs, which close new slots alone, and stops no job for memory.
pub fn pressure_of(psi: Option<Psi>) -> Pressure {
    match psi {
        Some(psi) if psi.full >= PRESSURE_CRITICAL => Pressure::Critical,
        Some(psi) if psi.some >= PRESSURE_WARNING => Pressure::Warning,
        _ => Pressure::Normal,
    }
}

/// The budget's thermal state from the zones: the hottest zone's, by its own trip points.
///
/// - Critical: at or over a `hot` or `critical` trip, where the kernel is about to shut the host down.
/// - Serious: at or over the zone's highest `passive` trip, where the kernel holds the CPUs back. This closes
///   new slots.
/// - Fair: at or over an `active` trip or a lower `passive` one: a fan runs, or the kernel starts to watch.
/// - Nominal: under all of them, and for a zone without trips. A host without thermal zones (a virtual machine,
///   some boards) is Nominal: the kernel's own protection still holds there.
///
/// Trips at or under 0 C are left out: firmware writes those for a trip it does not use.
pub fn thermal_of(zones: &[Zone]) -> Thermal {
    zones
        .iter()
        .map(|zone| {
            let at = |kinds: &'static [TripKind]| {
                zone.trips
                    .iter()
                    .filter(|trip| trip.millidegrees > 0 && kinds.contains(&trip.kind))
                    .map(|trip| trip.millidegrees)
            };
            let reached = |threshold: Option<i32>| threshold.is_some_and(|at| zone.millidegrees >= at);
            if reached(at(&[TripKind::Hot, TripKind::Critical]).min()) {
                Thermal::Critical
            } else if reached(at(&[TripKind::Passive]).max()) {
                Thermal::Serious
            } else if reached(at(&[TripKind::Active, TripKind::Passive]).min()) {
                Thermal::Fair
            } else {
                Thermal::Nominal
            }
        })
        .max()
        .unwrap_or(Thermal::Nominal)
}

/// The batteries that power the host, by name.
fn batteries(supplies: &[Supply]) -> impl Iterator<Item = &Supply> {
    supplies
        .iter()
        .filter(|supply| supply.battery && supply.system && supply.present != Some(false))
}

/// Whether the host runs on its battery.
///
/// A host without a battery runs on the mains, whatever its supplies say: a board on a USB-C supply has no
/// battery to run on. With a battery, an outside supply that is online means the mains; outside supplies that are
/// all offline mean the battery. With no word from an outside supply, a battery that charges, is full or is not
/// charging means the mains, and any other state (discharging, unknown, none) the battery, the cautious side.
pub fn on_battery_of(supplies: &[Supply]) -> bool {
    if batteries(supplies).next().is_none() {
        return false;
    }
    let outside: Vec<Option<bool>> = supplies
        .iter()
        .filter(|supply| !supply.battery && supply.system)
        .map(|supply| supply.online)
        .collect();
    if outside.contains(&Some(true)) {
        return false;
    }
    if outside.contains(&Some(false)) {
        return true;
    }
    let mut states = batteries(supplies).map(|battery| battery.status.as_deref());
    !states.all(|status| matches!(status, Some("Charging" | "Full" | "Not charging")))
}

/// The host's battery for the console's page: the first battery by name that says its charge. `None` on a host
/// without a battery, which is not a battery at 0%.
pub fn battery_of(supplies: &[Supply]) -> Option<Battery> {
    let mut batteries: Vec<&Supply> = batteries(supplies).collect();
    batteries.sort_by(|one, other| one.name.cmp(&other.name));
    let battery = batteries
        .into_iter()
        .find(|battery| battery.capacity.is_some_and(|percent| percent <= 100))?;
    Some(Battery {
        on_battery: on_battery_of(supplies),
        percent: battery.capacity?,
        charging: battery.status.as_deref() == Some("Charging"),
    })
}

/// The budget's reading from Linux's answers. An answer that cannot be right is an error and closes the host; it
/// is never guessed at.
///
/// Every field, and what it means on Linux:
///
/// - `available_bytes`: `MemAvailable`, never more than `MemTotal`.
/// - `pressure`: PSI's memory pressure ([`pressure_of`]), Normal on a kernel without PSI.
/// - `swap_used_bytes`: `SwapTotal` less `SwapFree`.
/// - `swapouts`: `pswpout`. The budget looks at how it moves between two readings, its rate, and counts the host
///   as swapping for 30 s after it moved.
/// - `idle`: zero. Linux has no person's idle time for a service to read, and a headless host has no person;
///   such a host is [`Mode::Dedicated`](cf_budget::Mode::Dedicated), where the budget never reads it. On a
///   workstation zero counts the person as there, the cautious side, as on a Mac whose idle time is not known.
/// - `thermal`: the hottest zone by its trip points ([`thermal_of`]).
/// - `on_battery`: [`on_battery_of`]; false on a host without a battery.
/// - `disk_free_bytes`: `statvfs` of the state folder.
pub fn reading(at: Now, raw: &Raw) -> Result<HostReading, HostError> {
    if raw.available_bytes > raw.total_bytes {
        return Err(HostError::Implausible {
            what: "MemAvailable",
            value: format!("{} bytes of {}", raw.available_bytes, raw.total_bytes),
        });
    }
    let Some(swap_used_bytes) = raw.swap_total_bytes.checked_sub(raw.swap_free_bytes) else {
        return Err(HostError::Implausible {
            what: "SwapFree",
            value: format!("{} bytes of {}", raw.swap_free_bytes, raw.swap_total_bytes),
        });
    };
    Ok(HostReading {
        at,
        available_bytes: raw.available_bytes,
        pressure: pressure_of(raw.memory_pressure),
        swap_used_bytes,
        swapouts: raw.swapouts,
        idle: Millis::ZERO,
        thermal: thermal_of(&raw.zones),
        on_battery: on_battery_of(&raw.supplies),
        disk_free_bytes: raw.disk_free_bytes,
    })
}

/// The share of the CPUs' time that was not idle between two counts of
/// [`LinuxHost::cpu_ticks`](crate::LinuxHost::cpu_ticks), in percent; `None` when no time passed between them or a
/// count went back (the host started again).
pub fn cpu_percent(before: CpuTicks, now: CpuTicks) -> Option<f64> {
    let busy = now.busy.checked_sub(before.busy)?;
    let idle = now.idle.checked_sub(before.idle)?;
    let all = busy.checked_add(idle).filter(|all| *all > 0)?;
    #[allow(clippy::cast_precision_loss)]
    Some(busy as f64 / all as f64 * 100.0)
}
