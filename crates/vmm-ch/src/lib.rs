//! Machines on Linux: cloud-hypervisor under KVM, one process per machine.
//!
//! Each machine runs in a cgroup of its own under the part of the tree delegated to the Runner ([`cgroup`]): its
//! memory is capped at its size and the monitor's overhead, without swap; its vCPUs are held to the host's fast
//! cores ([`cpus`]); `memory.current` is what [`cf_vmm::Vm::resident`] reports, and `cgroup.kill` ends it.
//!
//! With a network the monitor runs under pasta, in a user and network namespace that pasta made and nothing of the
//! host changes for: pasta's tap device and the guest's share a bridge there, pasta answers the guest's DHCP and
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
pub mod sandbox;
pub mod vsock;

#[cfg(target_os = "linux")]
mod machine;

#[cfg(target_os = "linux")]
pub use machine::{ChVmm, Config};

/// What a machine's cgroup holds beyond the guest's memory: cloud-hypervisor's own memory and the guest memory's
/// page tables, pasta, and the sandbox.
pub fn overhead(memory_bytes: u64) -> u64 {
    (192 << 20) + memory_bytes / 64
}
