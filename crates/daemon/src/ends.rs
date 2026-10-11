//! How a job on this Mac ended, in one word for the console and the log.

use cf_lifecycle::End;
use cf_vmhost::End as VmEnd;

/// How a job ended. The first four are the job's own ends, by GitHub's word; the rest are what the Mac did or what
/// happened to the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EndKind {
    /// The job ran to its end and passed.
    Success,
    /// The job ran to its end and failed.
    Failure,
    /// The job was cancelled at GitHub.
    Cancelled,
    /// The job ran to its end, and GitHub did not say how.
    Finished,
    /// Stopped at its deadline, after the grace time.
    Timeout,
    /// The machine died or went silent.
    GuestLost,
    /// Stopped because the Mac ran short of memory.
    EvictedMemory,
    /// Stopped because the Mac's disk was nearly full.
    EvictedDisk,
    /// Stopped because the Mac slept through it.
    HostSlept,
    /// The Mac shut down or its user logged out.
    HostShutdown,
    /// The Runner started again before the machine was up, and what it needed to register was gone with the old one.
    DaemonRestart,
    /// Stopped on the Mac: from the console, or by whoever ended its vm-host.
    Stopped,
    /// The machine did not get ready.
    StartFailed,
    /// The Actions runner was up and no job came.
    NoJob,
    /// Another Mac got the job before a machine was started.
    Lost,
}

impl EndKind {
    /// Every end.
    pub const ALL: [EndKind; 15] = [
        EndKind::Success,
        EndKind::Failure,
        EndKind::Cancelled,
        EndKind::Finished,
        EndKind::Timeout,
        EndKind::GuestLost,
        EndKind::EvictedMemory,
        EndKind::EvictedDisk,
        EndKind::HostSlept,
        EndKind::HostShutdown,
        EndKind::DaemonRestart,
        EndKind::Stopped,
        EndKind::StartFailed,
        EndKind::NoJob,
        EndKind::Lost,
    ];

    /// The name the console gets, at most 30 characters.
    pub const fn name(self) -> &'static str {
        match self {
            EndKind::Success => "success",
            EndKind::Failure => "failure",
            EndKind::Cancelled => "cancelled",
            EndKind::Finished => "finished",
            EndKind::Timeout => "timeout",
            EndKind::GuestLost => "guest-lost",
            EndKind::EvictedMemory => "evicted-memory",
            EndKind::EvictedDisk => "evicted-disk",
            EndKind::HostSlept => "host-slept",
            EndKind::HostShutdown => "host-shutdown",
            EndKind::DaemonRestart => "daemon-restart",
            EndKind::Stopped => "stopped",
            EndKind::StartFailed => "start-failed",
            EndKind::NoJob => "no-job",
            EndKind::Lost => "lost",
        }
    }

    /// The end of this name.
    pub fn from_name(name: &str) -> Option<EndKind> {
        EndKind::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// The end once GitHub said how the job went. Only a job that ran to its end takes GitHub's word: for one the
    /// Mac stopped or lost, GitHub's "failed" says less than what the Mac knows.
    pub fn with_result(self, result: Option<&str>) -> EndKind {
        if self != EndKind::Finished {
            return self;
        }
        match result.map(str::to_ascii_lowercase).as_deref() {
            Some("succeeded" | "success") => EndKind::Success,
            Some("failed" | "failure") => EndKind::Failure,
            Some("canceled" | "cancelled") => EndKind::Cancelled,
            _ => EndKind::Finished,
        }
    }
}

/// How a machine ended, from the lifecycle's end and the vm-host's own. What the daemon decided (a stop for memory,
/// for the deadline, for a sleep) stands; where the lifecycle only saw the machine go, the vm-host's word says why.
/// `draining`: the Runner itself was told to end, so a vm-host that was stopped by somebody else went down with the
/// Mac.
pub fn machine_end(life: End, vm: Option<VmEnd>, draining: bool) -> EndKind {
    match life {
        End::EvictedMemory => EndKind::EvictedMemory,
        End::EvictedDisk => EndKind::EvictedDisk,
        End::Timeout => EndKind::Timeout,
        End::HostSlept => EndKind::HostSlept,
        End::Stopped => EndKind::Stopped,
        End::StartFailed => EndKind::StartFailed,
        End::NoJob => EndKind::NoJob,
        End::Lost => EndKind::Lost,
        End::Ok | End::Oom | End::GuestLost => match vm {
            Some(VmEnd::Ok | VmEnd::Oom | VmEnd::Failed) => EndKind::Finished,
            Some(VmEnd::Timeout) => EndKind::Timeout,
            Some(VmEnd::BootFailed) => EndKind::StartFailed,
            Some(VmEnd::EvictedMemory) => EndKind::EvictedMemory,
            Some(VmEnd::EvictedDisk) => EndKind::EvictedDisk,
            Some(VmEnd::HostSlept) => EndKind::HostSlept,
            // The job was over before the machine was asked to go: how the machine was put away says nothing of it.
            Some(VmEnd::Cancelled) if life != End::GuestLost => EndKind::Finished,
            Some(VmEnd::Cancelled) if draining => EndKind::HostShutdown,
            Some(VmEnd::Cancelled) => EndKind::Stopped,
            Some(VmEnd::GuestLost) => EndKind::GuestLost,
            None if life == End::GuestLost => EndKind::GuestLost,
            // GitHub said the job is over and the vm-host was killed before it wrote how the machine went.
            None => EndKind::Finished,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_fits_the_console_and_reads_back() {
        for kind in EndKind::ALL {
            assert!(kind.name().len() <= 30, "{}", kind.name());
            assert_eq!(EndKind::from_name(kind.name()), Some(kind));
        }
        assert_eq!(EndKind::from_name("ok"), None);
    }

    #[test]
    fn githubs_word_decides_only_a_job_that_ran_to_its_end() {
        assert_eq!(EndKind::Finished.with_result(Some("succeeded")), EndKind::Success);
        assert_eq!(EndKind::Finished.with_result(Some("Failed")), EndKind::Failure);
        assert_eq!(
            EndKind::Finished.with_result(Some("canceled")),
            EndKind::Cancelled
        );
        assert_eq!(EndKind::Finished.with_result(Some("skipped")), EndKind::Finished);
        assert_eq!(EndKind::Finished.with_result(None), EndKind::Finished);
        for kind in EndKind::ALL {
            if kind != EndKind::Finished {
                assert_eq!(kind.with_result(Some("failed")), kind, "{}", kind.name());
            }
        }
    }

    #[test]
    fn the_daemons_own_decision_stands_and_the_vm_host_explains_the_rest() {
        assert_eq!(
            machine_end(End::EvictedMemory, Some(VmEnd::EvictedMemory), false),
            EndKind::EvictedMemory
        );
        // The guest ended by itself in the grace time: it was still stopped for the Mac's memory.
        assert_eq!(
            machine_end(End::EvictedMemory, Some(VmEnd::Ok), false),
            EndKind::EvictedMemory
        );
        assert_eq!(machine_end(End::Timeout, None, false), EndKind::Timeout);
        // The vm-host kept the deadline before the daemon's own backstop.
        assert_eq!(
            machine_end(End::GuestLost, Some(VmEnd::Timeout), false),
            EndKind::Timeout
        );
        assert_eq!(
            machine_end(End::Ok, Some(VmEnd::Timeout), false),
            EndKind::Timeout
        );
        assert_eq!(
            machine_end(End::GuestLost, Some(VmEnd::BootFailed), false),
            EndKind::StartFailed
        );
        assert_eq!(
            machine_end(End::GuestLost, Some(VmEnd::Cancelled), false),
            EndKind::Stopped
        );
        assert_eq!(
            machine_end(End::GuestLost, Some(VmEnd::Cancelled), true),
            EndKind::HostShutdown
        );
        assert_eq!(machine_end(End::GuestLost, None, false), EndKind::GuestLost);
        assert_eq!(
            machine_end(End::GuestLost, Some(VmEnd::GuestLost), true),
            EndKind::GuestLost
        );
        assert_eq!(
            machine_end(End::Ok, Some(VmEnd::Failed), false),
            EndKind::Finished
        );
        assert_eq!(machine_end(End::Oom, Some(VmEnd::Oom), false), EndKind::Finished);
        assert_eq!(machine_end(End::Ok, None, false), EndKind::Finished);
    }
}
