use cf_budget::{HostError, HostFacts, HostReading, Millis, Now, Pressure, Thermal};

/// What macOS answered, before anything is made of it. Page counts are in pages of [`Raw::page_bytes`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Raw {
    /// The size of a page (`sysconf(_SC_PAGESIZE)`): 16384 on Apple silicon.
    pub page_bytes: u64,
    /// `free_count`. It includes the speculative pages; `vm_stat` prints it without them as "Pages free".
    pub free_pages: u64,
    /// `speculative_count`: file pages read ahead that nobody asked for yet.
    pub speculative_pages: u64,
    /// `external_page_count`: pages with a file behind them, the speculative ones included.
    pub file_backed_pages: u64,
    /// `purgeable_count`: memory its owner said may be dropped.
    pub purgeable_pages: u64,
    /// `active_count`.
    pub active_pages: u64,
    /// `inactive_count`.
    pub inactive_pages: u64,
    /// `wire_count`: memory that can never be paged out.
    pub wired_pages: u64,
    /// `internal_page_count`: programs' own (anonymous) pages that are not compressed.
    pub anonymous_pages: u64,
    /// `compressor_page_count`: pages the compressor holds.
    pub compressor_pages: u64,
    /// `swapouts`: pages written to swap since boot.
    pub swapouts: u64,
    /// `kern.memorystatus_vm_pressure_level`: 1 normal, 2 warning, 4 critical.
    pub pressure_level: i32,
    /// `vm.swapusage`, in use.
    pub swap_used_bytes: u64,
    /// `vm.swapusage`, the size of the swap files now.
    pub swap_total_bytes: u64,
    /// `HIDIdleTime` of the HID system in nanoseconds; `None` when macOS has none to give.
    pub idle_nanos: Option<u64>,
    /// The state of `com.apple.system.thermalpressurelevel`: 0 nominal, 1 moderate, 2 heavy, 3 trapping,
    /// 4 sleeping.
    pub thermal_level: u64,
    /// `IOPSGetTimeRemainingEstimate`: -2 with unlimited power (the mains), -1 on a battery whose time is not
    /// known yet, else the seconds the battery lasts.
    pub power_remaining_secs: f64,
    /// `statfs` of the volume with the job disks: blocks an unprivileged process may use, in bytes.
    pub disk_free_bytes: u64,
}

/// `IOPSGetTimeRemainingEstimate`'s answer for a Mac on the mains (`kIOPSTimeRemainingUnlimited`).
const POWER_UNLIMITED: f64 = -2.0;

/// The budget's reading from macOS's answers. An answer that cannot be right is an error and closes the Mac; it
/// is never guessed at.
///
/// Two answers are read towards caution instead: with no idle time the person counts as there, and a power answer
/// other than "unlimited" counts as battery.
pub fn reading(facts: HostFacts, at: Now, raw: &Raw) -> Result<HostReading, HostError> {
    if !raw.page_bytes.is_power_of_two() {
        return Err(HostError::Implausible {
            what: "the page size",
            value: raw.page_bytes.to_string(),
        });
    }
    let pressure = match raw.pressure_level {
        1 => Pressure::Normal,
        2 => Pressure::Warning,
        4 => Pressure::Critical,
        other => {
            return Err(HostError::Implausible {
                what: "the memory pressure level",
                value: other.to_string(),
            });
        }
    };
    let thermal = match raw.thermal_level {
        0 => Thermal::Nominal,
        1 => Thermal::Fair,
        2 => Thermal::Serious,
        _ => Thermal::Critical,
    };

    // Speculative pages are in both counts: free and file-backed.
    let pages = raw
        .free_pages
        .saturating_sub(raw.speculative_pages)
        .saturating_add(raw.file_backed_pages)
        .saturating_add(raw.purgeable_pages);
    let available_bytes = pages
        .checked_mul(raw.page_bytes)
        .filter(|bytes| *bytes <= facts.total_bytes);
    let Some(available_bytes) = available_bytes else {
        return Err(HostError::Implausible {
            what: "the available memory",
            value: format!(
                "{pages} pages of {} bytes with {} installed",
                raw.page_bytes, facts.total_bytes
            ),
        });
    };

    Ok(HostReading {
        at,
        available_bytes,
        pressure,
        swap_used_bytes: raw.swap_used_bytes,
        swapouts: raw.swapouts,
        idle: Millis(raw.idle_nanos.unwrap_or(0) / 1_000_000),
        thermal,
        on_battery: raw.power_remaining_secs != POWER_UNLIMITED,
        disk_free_bytes: raw.disk_free_bytes,
    })
}
