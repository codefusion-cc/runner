//! The virtual machine monitor on Apple's Virtualization.framework. M0 holds only the boot proof: start the guest
//! image with a console, wait for the supervisor to say `ready`, and power it off. M1 grows this into the `Vmm`
//! implementation behind `cf-vmm`.
//!
//! Virtualization.framework works on the main queue: [`boot_to_ready`] must be called on the process's main thread,
//! which it keeps running the main run loop while the guest boots. The process needs the
//! `com.apple.security.virtualization` entitlement.

#[cfg(target_os = "macos")]
mod proof;

#[cfg(target_os = "macos")]
pub use proof::{BootError, BootReport, BootSpec, boot_to_ready};
