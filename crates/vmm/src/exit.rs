//! How one machine ended, for a monitor whose machine is told of its end from several sides.

use crate::VmExit;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// A machine's end, set once by whoever hears of it first: a callback, a stop's answer, a look at the machine.
/// Every later word is ignored, so [`crate::Vm::wait`] answers the same for ever.
#[derive(Debug, Default)]
pub struct ExitCell {
    /// The end, and whether the host has asked for a stop.
    slot: Mutex<(Option<VmExit>, bool)>,
    changed: Condvar,
}

impl ExitCell {
    /// The machine ended this way, unless it has an end already.
    pub fn end(&self, exit: VmExit) {
        let mut slot = self.slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.0.is_none() {
            slot.0 = Some(exit);
            self.changed.notify_all();
        }
    }

    /// The host asks the machine to stop. A machine found stopped from here on was stopped by the host, whichever
    /// of the stop's answer and a look at the machine comes first.
    pub fn stop_asked(&self) {
        self.slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .1 = true;
    }

    /// The machine was found stopped and nothing said why: the host's doing after [`ExitCell::stop_asked`], the
    /// guest's own otherwise.
    pub fn found_stopped(&self) {
        let asked = self
            .slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .1;
        self.end(if asked { VmExit::Stopped } else { VmExit::Guest });
    }

    /// Waits for the end for at most `timeout`. `None` when the machine still runs.
    pub fn wait(&self, timeout: Duration) -> Option<VmExit> {
        let deadline = Instant::now().checked_add(timeout);
        let mut slot = self.slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if slot.0.is_some() {
                return slot.0.clone();
            }
            slot = match deadline {
                Some(deadline) => {
                    let left = deadline.checked_duration_since(Instant::now())?;
                    self.changed
                        .wait_timeout(slot, left)
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .0
                }
                // A wait longer than the clock can count is a wait without an end.
                None => self
                    .changed
                    .wait(slot)
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
            };
        }
    }
}
