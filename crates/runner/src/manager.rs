//! The seam to the system's service manager, which keeps the Runner running: launchd on a Mac
//! ([`crate::launchd::Agent`]), systemd on Linux ([`crate::systemd::UserUnit`]). Each starts the Runner again
//! whenever it ends, after a pause, for good: `restart` and an update rely on nothing else.

use std::path::{Path, PathBuf};

/// The Runner's entry in the service manager of the user it runs as.
pub trait ServiceManager {
    /// What the entry is called, for a person: `the launchd agent …`, `the systemd user unit …`.
    fn name(&self) -> String;

    /// Where the entry's file is.
    fn path(&self) -> PathBuf;

    /// The entry's file that runs `program daemon --config <config>` in `state_dir`, with what the Runner prints
    /// going to `log`. An error for paths the service manager cannot be told.
    fn definition(
        &self,
        program: &Path,
        config: &Path,
        state_dir: &Path,
        log: &Path,
    ) -> Result<String, String>;

    /// The command that shows how the entry is doing.
    fn look(&self) -> String;

    /// Ends the Runner if it runs and waits until the service manager has let go of it.
    fn stop(&self) -> Result<(), String>;

    /// Writes the entry and has the service manager start it, in place of one that ran before.
    fn start(&self, definition: &str) -> Result<(), String>;

    /// Ends the Runner and removes the entry. Nothing there is nothing to do.
    fn remove(&self) -> Result<(), String>;
}
