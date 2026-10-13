//! Machines on Linux: cloud-hypervisor under KVM, one process per machine.
//!
//! Each machine runs in a cgroup of its own under the part of the tree delegated to the Runner ([`cgroup`]): its
//! memory is capped at its size and the monitor's overhead, without swap; its vCPUs are held to the host's fast
//! cores ([`cpus`]); `memory.current` is what [`cf_vmm::Vm::resident`] reports, and `cgroup.kill` ends it.
//!
//! With a network the monitor runs under pasta (the system's, [`programs`]), in a user and network namespace that
//! pasta made and nothing of the host changes for: pasta's tap device and the guest's share a bridge there, pasta answers the guest's DHCP and
//! DNS and carries its connections out through ordinary sockets of the host, and the guest's firewall on the tap
//! ([`firewall`]) drops what the guest sends to private and special ranges and to the host itself before pasta
//! sees it. The sandbox ([`sandbox`], the hidden `vm-sandbox` subcommand of the program that hosts this crate)
//! sets that up and starts the monitor without capabilities, in a PID and cgroup namespace of its own.
//!
//! The monitor tells of its machine through `--event-monitor` ([`events`]); its exit status and the cgroup's
//! out-of-memory count say how the machine ended ([`ending`]); the guest's vsock ports are reached through a Unix
//! socket ([`vsock`]).

pub mod cgroup;
pub mod command;
pub mod cpus;
pub mod ending;
pub mod events;
pub mod firewall;
pub mod link;
/// The pinned cloud-hypervisor.
pub mod pins;
pub mod programs;
pub mod sandbox;
pub mod vsock;

#[cfg(target_os = "linux")]
mod machine;

#[cfg(target_os = "linux")]
pub use machine::{ChVmm, Config};

/// What a machine's cgroup holds beyond the guest's memory: cloud-hypervisor's own memory and the guest memory's
/// page tables, pasta, and the sandbox. Too little and a guest that fills its memory ends as an OOM it did not
/// cause; too much and a machine may take more of the host than the budget gave it.
///
/// Measured with `cargo xtask it --only full-memory` on an RK3588 board (8 GB, Debian bookworm, kernel 6.1,
/// cloud-hypervisor v53.0, 2026-10-13): a job takes all the memory its guest gets, and what the cgroup held at its
/// most less what the guest had touched bounds what it holds beyond the guest's memory. 79 MiB for a 1 GiB machine,
/// 102 MiB for 2 GiB, 187 MiB for 4 GiB: about 43 MiB and 36 MiB a GiB, the guest kernel's own tables (1/64 of its
/// memory) counted in. This gives 160, 192 and 256 MiB, and 384 MiB to an 8 GiB machine, for which the line above
/// says 331.
pub fn overhead(memory_bytes: u64) -> u64 {
    (128 << 20) + memory_bytes / 32
}

#[cfg(test)]
mod tests {
    use super::overhead;

    const MIB: u64 = 1 << 20;

    #[test]
    fn the_overhead_covers_what_full_machines_were_measured_to_hold_with_room_to_spare() {
        // (size, the most measured beyond it), from the board run named at `overhead`.
        for (size, measured) in [
            (1024 * MIB, 79 * MIB),
            (2048 * MIB, 102 * MIB),
            (4096 * MIB, 187 * MIB),
        ] {
            let given = overhead(size);
            assert!(
                given >= measured + 64 * MIB,
                "{} MiB for a {} MiB machine",
                given / MIB,
                size / MIB
            );
            // Not so much that a monitor grown to three times its size would go unnoticed.
            assert!(
                given <= measured * 3,
                "{} MiB for a {} MiB machine",
                given / MIB,
                size / MIB
            );
        }
    }

    #[test]
    fn a_larger_machine_gets_more_and_the_largest_no_overflow() {
        let mut last = 0;
        for gib in 1..=64 {
            let given = overhead(gib << 30);
            assert!(given > last);
            last = given;
        }
        assert!(overhead(u64::MAX) > overhead(64 << 30));
    }
}
