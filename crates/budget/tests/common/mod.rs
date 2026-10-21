//! Two Macs to test with, and readings of a Mac where nothing is wrong.
#![allow(dead_code)]

use cf_budget::{
    Assigned, Budget, GIB, Grant, HostFacts, HostReading, Millis, Mode, Now, Pressure, Refusal, Settings,
    SlotId, Thermal,
};

/// A 16 GB Mac with 10 cores that nobody works on.
pub const DEDICATED: HostFacts = HostFacts {
    total_bytes: 16 * GIB,
    cores: 10,
};
/// A 16 GB Mac with 8 cores that is someone's own.
pub const WORKSTATION: HostFacts = HostFacts {
    total_bytes: 16 * GIB,
    cores: 8,
};

pub fn dedicated() -> Settings {
    Settings::for_host(DEDICATED, Mode::Dedicated)
}

pub fn workstation() -> Settings {
    Settings::for_host(WORKSTATION, Mode::Workstation)
}

/// The same settings with one vCPU per VM, for tests about memory that the CPU cap would otherwise decide.
pub fn one_vcpu(mut settings: Settings) -> Settings {
    settings.vcpus = [1; 4];
    settings
}

pub fn ms(millis: u64) -> Now {
    Now {
        awake: Millis(millis),
        elapsed: Millis(millis),
    }
}

pub fn secs(seconds: u64) -> Now {
    ms(seconds * 1000)
}

/// A reading with nothing wrong: all 16 GB available, so that the caps decide, and the person away for an hour.
pub fn calm(at: Now) -> HostReading {
    HostReading {
        at,
        available_bytes: 16 * GIB,
        pressure: Pressure::Normal,
        swap_used_bytes: 0,
        swapouts: 100,
        idle: Millis::mins(60),
        thermal: Thermal::Nominal,
        on_battery: false,
        disk_free_bytes: 100 * GIB,
        disk_reclaimable_bytes: 0,
    }
}

/// One reading a second from `from` to `to` seconds, both included, each changed by `change`; the assigned jobs
/// that started, with the second.
pub fn watch(
    budget: &mut Budget,
    from: u64,
    to: u64,
    change: impl Fn(&mut HostReading),
) -> Vec<(u64, Grant)> {
    let mut started = Vec::new();
    for second in from..=to {
        let mut reading = calm(secs(second));
        change(&mut reading);
        let observed = budget.observe(secs(second), Some(reading));
        started.extend(observed.started.into_iter().map(|grant| (second, grant)));
    }
    started
}

/// A budget that has seen 60 s of calm readings: it is open at second 60.
pub fn open(settings: Settings) -> Budget {
    let mut budget = Budget::new(settings).unwrap();
    watch(&mut budget, 0, 60, |_| {});
    budget
}

/// The slot of an assigned job that waits, and why.
pub fn waits(assigned: Assigned) -> (SlotId, Refusal) {
    match assigned {
        Assigned::Waits { slot, why } => (slot, why),
        other => panic!("expected a job that waits, got {other:?}"),
    }
}

/// The grant of an assigned job that starts.
pub fn starts(assigned: Assigned) -> Grant {
    match assigned {
        Assigned::Start(grant) => grant,
        other => panic!("expected a job that starts, got {other:?}"),
    }
}
