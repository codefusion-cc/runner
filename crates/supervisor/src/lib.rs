//! The supervisor's decisions, apart from the system calls that carry them out, so they are tested on any machine:
//! the firewall's rules and their netlink form, the DHCP exchange, reading the kernel's out-of-memory report,
//! the options of a job's root, finding the processes of a step, how a container ended, what a job is told, and
//! the ids and commands of a job's Docker sandbox, and the trusted class's gate before the Actions runner's Worker.
//!
//! The program itself (`src/main.rs`, `src/linux`) runs only as PID 1 of the guest.

pub mod dhcp;
pub mod disks;
pub mod docker;
pub mod environment;
pub mod firewall;
pub mod gate;
pub mod oom;
pub mod outcome;
pub mod processes;
pub mod tell;
