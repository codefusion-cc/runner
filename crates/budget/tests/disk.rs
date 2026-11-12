//! The disk limits a host's settings may give: the free disk under which no job starts (the hold), the one under
//! which a job is stopped, and the size of each job's disk. A small Linux host that runs containers lowers them; every
//! other host keeps the defaults.

mod common;

use cf_budget::{
    DiskShort, Eviction, EvictionCause, GIB, HostFacts, Limits, Refusal, Sandbox, Settings, SettingsError,
    Size, Trouble, Usage,
};
use common::*;

/// heimforge's dal1: 11 GiB, 12 cores alike, no KVM, jobs in containers.
const DAL1: HostFacts = HostFacts {
    total_bytes: 11 * GIB,
    cores: 12,
};

fn dal1() -> Settings {
    Settings::for_headless_in(DAL1, 0, Sandbox::Container)
}

fn disk(hold: Option<u64>, evict: Option<u64>, job: Option<u64>) -> Limits {
    Limits {
        disk_hold_bytes: hold,
        disk_evict_bytes: evict,
        job_disk_bytes: job,
        ..Limits::default()
    }
}

#[test]
fn every_kind_of_host_keeps_the_disk_defaults() {
    let hosts = [
        dedicated(),
        workstation(),
        Settings::for_headless(DAL1, 0),
        dal1(),
        Settings::for_headless(DAL1, 0).one_at_a_time(0),
    ];
    for settings in hosts {
        assert_eq!(
            (
                settings.disk_hold_bytes,
                settings.disk_evict_bytes,
                settings.job_disk_bytes
            ),
            (25 * GIB, 5 * GIB, 40 * GIB)
        );
        let limited = settings.clone().limited(&Limits::default()).unwrap();
        assert_eq!(limited, settings, "no limits change nothing");
    }
    assert_eq!(
        (Settings::DISK_HOLD, Settings::DISK_EVICT, Settings::JOB_DISK),
        (25 * GIB, 5 * GIB, 40 * GIB)
    );
}

#[test]
fn a_hold_alone_lowers_the_stop_to_it_and_never_raises_it() {
    let low = dal1().limited(&disk(Some(3 * GIB), None, None)).unwrap();
    assert_eq!((low.disk_hold_bytes, low.disk_evict_bytes), (3 * GIB, 3 * GIB));
    let high = dal1().limited(&disk(Some(10 * GIB), None, None)).unwrap();
    assert_eq!((high.disk_hold_bytes, high.disk_evict_bytes), (10 * GIB, 5 * GIB));
    let both = dal1()
        .limited(&disk(Some(3 * GIB), Some(GIB), Some(8 * GIB)))
        .unwrap();
    assert_eq!(
        (both.disk_hold_bytes, both.disk_evict_bytes, both.job_disk_bytes),
        (3 * GIB, GIB, 8 * GIB)
    );
    // A stop set alone keeps the default hold.
    let stop = dal1().limited(&disk(None, Some(2 * GIB), None)).unwrap();
    assert_eq!((stop.disk_hold_bytes, stop.disk_evict_bytes), (25 * GIB, 2 * GIB));
}

#[test]
fn the_disk_limits_at_their_edges() {
    let too_low = Err(SettingsError::DiskEvictTooLow { least_bytes: GIB });
    let hold_too_low = Err(SettingsError::DiskHoldTooLow { least_bytes: GIB });
    // A hold of nothing, or of a byte, is said of the hold, though it takes the stop down with it.
    assert_eq!(dal1().limited(&disk(Some(0), None, None)), hold_too_low);
    assert_eq!(dal1().limited(&disk(Some(1), None, None)), hold_too_low);
    assert_eq!(dal1().limited(&disk(Some(GIB - 1), None, None)), hold_too_low);
    assert_eq!(
        dal1().limited(&disk(Some(GIB - 1), Some(GIB - 1), None)),
        hold_too_low
    );
    assert_eq!(dal1().limited(&disk(None, Some(0), None)), too_low);
    assert_eq!(dal1().limited(&disk(None, Some(GIB - 1), None)), too_low);
    // The least of both, equal.
    let least = dal1().limited(&disk(Some(GIB), Some(GIB), None)).unwrap();
    assert_eq!((least.disk_hold_bytes, least.disk_evict_bytes), (GIB, GIB));
    // A stop above the hold, by a byte.
    assert_eq!(
        dal1().limited(&disk(Some(3 * GIB), Some(3 * GIB + 1), None)),
        Err(SettingsError::DiskLimitsCrossed {
            evict_bytes: 3 * GIB + 1,
            hold_bytes: 3 * GIB
        })
    );
    // A stop set alone above the default hold.
    assert_eq!(
        dal1().limited(&disk(None, Some(26 * GIB), None)),
        Err(SettingsError::DiskLimitsCrossed {
            evict_bytes: 26 * GIB,
            hold_bytes: 25 * GIB
        })
    );
    // The largest numbers do not overflow anything.
    assert_eq!(
        dal1()
            .limited(&disk(Some(u64::MAX), Some(u64::MAX), None))
            .map(|s| s.disk_hold_bytes),
        Ok(u64::MAX)
    );
}

#[test]
fn a_jobs_disk_is_between_four_and_512_gib() {
    let job = |bytes| dal1().limited(&disk(None, None, Some(bytes)));
    let refused = |bytes| {
        Err(SettingsError::JobDisk {
            bytes,
            least_bytes: 4 * GIB,
            most_bytes: 512 * GIB,
        })
    };
    for bytes in [0, 1, 4 * GIB - 1, 512 * GIB + 1, u64::MAX] {
        assert_eq!(job(bytes), refused(bytes), "{bytes}");
    }
    for bytes in [4 * GIB, 8 * GIB, 512 * GIB] {
        assert_eq!(job(bytes).map(|s| s.job_disk_bytes), Ok(bytes), "{bytes}");
    }
}

#[test]
fn a_hold_that_no_file_system_of_this_size_meets_is_refused() {
    let limits = disk(Some(12 * GIB), Some(GIB), Some(4 * GIB));
    let settings = dal1().limited(&limits).unwrap();
    let check = |disk_bytes| settings.check_disk(&limits, Sandbox::Container, disk_bytes);
    let over = |disk_bytes| {
        Err(SettingsError::DiskHoldOverDisk {
            hold_bytes: 12 * GIB,
            disk_bytes,
        })
    };
    // Larger than the disk, and exactly the disk: no file system is ever all free.
    assert_eq!(check(11 * GIB), over(11 * GIB));
    assert_eq!(check(12 * GIB), over(12 * GIB));
    assert!(
        over(12 * GIB)
            .unwrap_err()
            .to_string()
            .contains("no job would ever start")
    );
    // A byte more disk than the hold is not refused, only short of a job's disk.
    assert_eq!(
        check(12 * GIB + 1),
        Ok(Some(DiskShort {
            hold_bytes: 12 * GIB,
            job_bytes: 4 * GIB,
            disk_bytes: 12 * GIB + 1
        }))
    );
    // A system that does not say its disk's size: nothing is checked.
    assert_eq!(check(0), Ok(None));
}

#[test]
fn a_jobs_disk_larger_than_the_file_system_is_refused() {
    let limits = disk(Some(2 * GIB), None, Some(16 * GIB));
    let settings = dal1().limited(&limits).unwrap();
    assert_eq!(
        settings.check_disk(&limits, Sandbox::Container, 16 * GIB - 1),
        Err(SettingsError::JobDiskOverDisk {
            job_bytes: 16 * GIB,
            disk_bytes: 16 * GIB - 1
        })
    );
    assert_eq!(
        settings.check_disk(&limits, Sandbox::Vm, 16 * GIB - 1),
        Err(SettingsError::JobDiskOverDisk {
            job_bytes: 16 * GIB,
            disk_bytes: 16 * GIB - 1
        }),
        "a machine's disk is a file on it too"
    );
}

#[test]
fn a_container_host_whose_hold_and_one_jobs_disk_do_not_fit_starts_and_says_so() {
    let limits = disk(Some(4 * GIB), Some(GIB), Some(8 * GIB));
    let settings = dal1().limited(&limits).unwrap();
    // To the byte: 12 GiB holds both.
    assert_eq!(
        settings.check_disk(&limits, Sandbox::Container, 12 * GIB),
        Ok(None)
    );
    let short = settings.check_disk(&limits, Sandbox::Container, 12 * GIB - 1);
    assert_eq!(
        short,
        Ok(Some(DiskShort {
            hold_bytes: 4 * GIB,
            job_bytes: 8 * GIB,
            disk_bytes: 12 * GIB - 1
        }))
    );
    assert!(
        short
            .unwrap()
            .unwrap()
            .to_string()
            .contains("do not fit the file system"),
        "it says what is short"
    );
    // Machines are left as they are.
    let vm = Settings::for_headless(DAL1, 0).limited(&limits).unwrap();
    assert_eq!(vm.check_disk(&limits, Sandbox::Vm, 12 * GIB - 1), Ok(None));
}

#[test]
fn the_defaults_are_never_refused_for_a_small_disk() {
    // A host on the defaults starts on any disk as it did; a container host on them is told why it may take nothing.
    for settings in [dedicated(), Settings::for_headless(DAL1, 0)] {
        assert_eq!(
            settings.check_disk(&Limits::default(), Sandbox::Vm, 20 * GIB),
            Ok(None)
        );
    }
    assert_eq!(
        dal1().check_disk(&Limits::default(), Sandbox::Container, 20 * GIB),
        Ok(Some(DiskShort {
            hold_bytes: 25 * GIB,
            job_bytes: 40 * GIB,
            disk_bytes: 20 * GIB
        }))
    );
}

/// A budget that has seen a minute of readings with this much free disk.
fn watched_with_free(settings: Settings, free: u64) -> cf_budget::Budget {
    let mut budget = cf_budget::Budget::new(settings).unwrap();
    watch(&mut budget, 0, 60, |r| {
        r.available_bytes = 10 * GIB;
        r.disk_free_bytes = free;
    });
    budget
}

#[test]
fn a_small_disk_takes_a_job_once_its_hold_is_lowered_and_holds_under_it() {
    // dal1's home: about 12 GiB, 4 GiB of it free.
    let free = 4 * GIB;
    let mut defaults = watched_with_free(dal1(), free);
    assert_eq!(
        defaults.claim(secs(60), Size::G2).map(|_| ()),
        Err(Refusal::Trouble(Trouble::DiskLow)),
        "today's 25 GiB hold takes nothing"
    );
    let lowered = dal1()
        .limited(&disk(Some(3 * GIB), Some(GIB), Some(6 * GIB)))
        .unwrap();
    let mut budget = watched_with_free(lowered.clone(), free);
    let grant = budget.claim(secs(60), Size::G2).expect("a job starts");
    budget.confirm(grant.slot).unwrap();
    // To the byte: at the hold a job still starts, a byte under it none does.
    let mut at_hold = watched_with_free(lowered.clone(), 3 * GIB);
    assert!(at_hold.claim(secs(60), Size::G2).is_ok());
    let mut reading = calm(secs(61));
    reading.available_bytes = 10 * GIB;
    reading.disk_free_bytes = 3 * GIB - 1;
    let observed = budget.observe(secs(61), Some(reading));
    assert_eq!(observed.evict, None, "above the stop, the job goes on");
    assert_eq!(
        budget.claim(secs(61), Size::G2).map(|_| ()),
        Err(Refusal::Trouble(Trouble::DiskLow))
    );
    // Under the stop, the job that wrote is stopped.
    budget
        .usage(
            secs(62),
            grant.slot,
            Usage {
                resident_bytes: 0,
                scratch_bytes: 5 * GIB,
            },
        )
        .unwrap();
    reading.at = secs(62);
    reading.disk_free_bytes = GIB - 1;
    assert_eq!(
        budget.observe(secs(62), Some(reading)).evict,
        Some(Eviction {
            slot: grant.slot,
            cause: EvictionCause::Disk
        })
    );
}
