//! The supervisor's decisions, apart from the system calls that carry them out, so they are tested on any machine:
//! the firewall's rules and their netlink form, the DHCP exchange, reading the kernel's out-of-memory report,
//! finding the processes of a step, how a container ended, and what a job is told.
//!
//! The program itself (`src/main.rs`, `src/linux`) runs only as PID 1 of the guest.

pub mod dhcp;
pub mod environment;
pub mod firewall;
pub mod oom;
pub mod outcome;
pub mod processes;
pub mod tell;
