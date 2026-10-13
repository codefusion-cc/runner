//! The life of one job slot, from the reservation of its memory to the moment the memory is given back.
//!
//! Pure: [`Slot::on`] takes the time and one event and returns what to do ([`Effect`]). The daemon does it: starts
//! the vm-host, asks it to stop, kills it, talks to GitHub, and gives the memory back to the budget.
//!
//! ```text
//! Reserved ──Assigned──▶ Starting ──VmReady──▶ Idle ──JobStarted──▶ Running ──JobCompleted──▶ Finishing
//!    │                    │   ▲                 │                     ▲  │                       │
//!    │ Lost, Expired      │   └── Retrying      │ idle limit, Retire  │  │ evicted, 6 h, stop    │ 2 min
//!    │                    │   20 s without      ▼                     │  │                       │
//!    │                    │   ready (once)    Asking ──JobStarted─────┘  │                       │
//!    ▼                    ▼                     │ GitHub's 204, 6 h      ▼                       ▼
//! Released ◀──VmExited── Killing ◀── 10 s ── Stopping ◀─────────────────┴───────────────────────┘
//! ```
//!
//! # What holds, on every path
//!
//! - **No state without an exit.** Every state but `Released` has a watchdog, so time alone moves a slot on; the
//!   only thing waited for without a limit is the death of a process that was sent SIGKILL, which is asked for
//!   again every 10 s.
//! - **Memory is given back exactly once**, by [`Effect::Release`], and never while the VM's process may be
//!   alive: a slot that started a VM reaches `Released` only through [`Event::VmExited`].
//! - **One end.** [`Effect::Ended`] comes once, as the last effect of a slot, with the reason.
//! - **One VM at a time.** A second start (the one retry) happens only after the first VM's process is gone.
//!
//! [`StartBreaker`] is what keeps a Mac whose VMs do not start from trying again and again: the Go manager did
//! that 2,400 times in a day in October 2026, until macOS had no processes left.

mod breaker;
mod slot;

pub use breaker::{BreakerLimits, StartBreaker};
pub use slot::{Effect, End, Event, Limits, Notice, Phase, Saved, Slot, Timer};
