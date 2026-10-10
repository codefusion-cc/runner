//! The supervisor, PID 1 of the guest. In M0 it mounts the pseudo file systems, loads the kernel modules listed in
//! `/etc/modules`, says `cf-supervisor: ready` on the console and powers the machine off when the host sends a line
//! (or after two minutes, so a lost host never leaves a VM running). M1 turns it into the job supervisor.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the supervisor runs in the Linux guest only");
    std::process::exit(2);
}
