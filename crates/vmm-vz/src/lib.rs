//! The virtual machine monitor on Apple's Virtualization.framework: [`VzVmm`] implements `cf-vmm`'s `Vmm`.
//!
//! Each machine gets a dispatch queue of its own and every call to the framework runs on it, so the methods may be
//! called from any thread and the process needs no main run loop. The process needs the
//! `com.apple.security.virtualization` entitlement; without it [`VzVmm::new`] or the first start says so.
//!
//! The machine itself lives in a process the framework starts (`com.apple.Virtualization.VirtualMachine`), which
//! ends when the machine stops, when its `Vm` is dropped and when this process dies. [`Holder`] finds that process
//! by the machine's files and reads what the Mac holds for it.

#[cfg(target_os = "macos")]
mod holder;
#[cfg(target_os = "macos")]
mod vz;

#[cfg(target_os = "macos")]
pub use holder::Holder;
#[cfg(target_os = "macos")]
pub use vz::VzVmm;
