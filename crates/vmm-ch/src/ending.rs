//! How a machine ended, from what is known once its monitor's process has: the exit status, whether the host
//! asked for the stop, whether the kernel killed in its cgroup for want of memory, whether the guest reset it, and
//! the last lines the monitor wrote.

use cf_vmm::VmExit;

/// The exit code of `codefusion-runner vm-sandbox` when it could not set the machine's sandbox up: no monitor ran.
pub const SANDBOX_FAILED: i32 = 125;

/// How the first process of the machine ended, as `waitpid` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// It exited with this code. pasta exits with the code of the program it runs, and with 128 and the signal's
    /// number when a signal ended it.
    Code(i32),
    /// A signal ended it.
    Signal(i32),
}

/// What is known of a machine that ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ending<'a> {
    /// The host asked for its end ([`cf_vmm::Vm::stop`] or [`cf_vmm::Vm::kill`]).
    pub stop_asked: bool,
    /// How its first process ended.
    pub status: Status,
    /// How often the kernel killed in its cgroup for want of memory.
    pub oom_kills: u64,
    /// The guest reset the machine, which the host then ended.
    pub reset: bool,
    /// The machine's memory and what its cgroup allowed beyond it, for the message of an out-of-memory end.
    pub memory_max: u64,
    /// The last of what the monitor, pasta and the sandbox wrote.
    pub output: &'a str,
}

/// The machine's end. The host's stop comes first: whatever the machine did meanwhile, it was asked to end.
/// Only a monitor that exits 0 of its own was a guest's power-off; every other end is an error that says why.
pub fn exit_of(ending: &Ending<'_>) -> VmExit {
    if ending.stop_asked {
        return VmExit::Stopped;
    }
    let said = tail(ending.output, 8);
    let and_said = if said.is_empty() {
        String::new()
    } else {
        format!(": {said}")
    };
    if ending.oom_kills > 0 {
        return VmExit::Error(format!(
            "the machine used more than the {} MiB its cgroup allows, and the kernel ended it{and_said}",
            ending.memory_max >> 20
        ));
    }
    if ending.reset {
        return VmExit::Error(format!(
            "the guest reset its machine (a kernel panic, or a reboot), and the host ended it{and_said}"
        ));
    }
    match ending.status {
        Status::Code(0) => VmExit::Guest,
        Status::Code(SANDBOX_FAILED) => {
            VmExit::Error(format!("the machine's sandbox could not be set up{and_said}"))
        }
        Status::Code(code) if code > 128 && code < 128 + 65 => VmExit::Error(format!(
            "the monitor was ended by signal {} (exit code {code}){and_said}",
            code - 128
        )),
        Status::Code(code) => VmExit::Error(format!("the monitor exited with code {code}{and_said}")),
        Status::Signal(signal) => {
            VmExit::Error(format!("the monitor was ended by signal {signal}{and_said}"))
        }
    }
}

/// The last `lines` lines of `text` that say something, joined by ` | `, at most 1000 characters.
pub fn tail(text: &str, lines: usize) -> String {
    let mut kept: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .rev()
        .take(lines)
        .collect();
    kept.reverse();
    let joined = kept.join(" | ");
    let chars = joined.chars().count();
    if chars <= 1000 {
        return joined;
    }
    let cut: String = joined.chars().skip(chars - 999).collect();
    format!("…{cut}")
}

/// The last bytes of an output stream, at most [`Recent::KEEP`] of them: what a monitor writes is never held whole.
#[derive(Debug, Default)]
pub struct Recent {
    bytes: std::collections::VecDeque<u8>,
}

impl Recent {
    /// How many of the last bytes are kept.
    pub const KEEP: usize = 8 << 10;

    /// Adds what came next.
    pub fn push(&mut self, bytes: &[u8]) {
        let bytes = &bytes[bytes.len().saturating_sub(Self::KEEP)..];
        let over = (self.bytes.len() + bytes.len()).saturating_sub(Self::KEEP);
        self.bytes.drain(..over);
        self.bytes.extend(bytes);
    }

    /// What is kept, as text; bytes that are no UTF-8 are replaced.
    pub fn text(&self) -> String {
        let (first, second) = self.bytes.as_slices();
        String::from_utf8_lossy(&[first, second].concat()).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn ending(status: Status) -> Ending<'static> {
        Ending {
            stop_asked: false,
            status,
            oom_kills: 0,
            reset: false,
            memory_max: 2208 << 20,
            output: "",
        }
    }

    fn error(exit: VmExit) -> String {
        match exit {
            VmExit::Error(why) => why,
            other => panic!("{other:?} is no error"),
        }
    }

    #[test]
    fn a_monitor_that_exits_0_of_its_own_was_a_power_off() {
        assert_eq!(exit_of(&ending(Status::Code(0))), VmExit::Guest);
    }

    #[test]
    fn a_stop_the_host_asked_for_is_stopped_however_the_monitor_ended() {
        for status in [
            Status::Code(0),
            Status::Code(1),
            Status::Signal(9),
            Status::Code(137),
        ] {
            let stopped = Ending {
                stop_asked: true,
                oom_kills: 1,
                reset: true,
                ..ending(status)
            };
            assert_eq!(exit_of(&stopped), VmExit::Stopped, "{status:?}");
        }
    }

    #[test]
    fn a_machine_out_of_memory_says_so_with_its_limit() {
        let why = error(exit_of(&Ending {
            oom_kills: 1,
            output: "x\n",
            ..ending(Status::Signal(9))
        }));
        assert!(why.contains("2208 MiB"), "{why}");
        assert!(why.ends_with(": x"), "{why}");
        // Even when the kill took pasta first and it exited with the code of a signal.
        assert!(
            error(exit_of(&Ending {
                oom_kills: 2,
                ..ending(Status::Code(137))
            }))
            .contains("MiB")
        );
    }

    #[test]
    fn a_reset_is_an_error_even_though_the_monitor_was_killed_for_it() {
        let why = error(exit_of(&Ending {
            reset: true,
            ..ending(Status::Signal(9))
        }));
        assert!(why.contains("reset"), "{why}");
        let why = error(exit_of(&Ending {
            reset: true,
            ..ending(Status::Code(0))
        }));
        assert!(why.contains("reset"), "{why}");
    }

    #[test]
    fn a_crash_or_a_kill_from_elsewhere_is_an_error_with_the_last_lines() {
        let crashed = Ending {
            output: "\nError booting VM: no kernel\n\n",
            ..ending(Status::Code(1))
        };
        assert_eq!(
            exit_of(&crashed),
            VmExit::Error("the monitor exited with code 1: Error booting VM: no kernel".into())
        );
        assert_eq!(
            exit_of(&ending(Status::Signal(11))),
            VmExit::Error("the monitor was ended by signal 11".into())
        );
        assert_eq!(
            exit_of(&ending(Status::Code(137))),
            VmExit::Error("the monitor was ended by signal 9 (exit code 137)".into())
        );
        for code in [128, 255, -1] {
            assert!(error(exit_of(&ending(Status::Code(code)))).contains(&format!("code {code}")));
        }
    }

    #[test]
    fn a_sandbox_that_could_not_be_set_up_says_so() {
        let why = error(exit_of(&Ending {
            output: "bridge: refused",
            ..ending(Status::Code(SANDBOX_FAILED))
        }));
        assert_eq!(why, "the machine's sandbox could not be set up: bridge: refused");
    }

    #[test]
    fn the_tail_is_the_last_lines_that_say_something_and_bounded() {
        assert_eq!(tail("a\n\n b \nc\n", 2), "b | c");
        assert_eq!(tail("", 8), "");
        assert_eq!(tail("\n \n", 8), "");
        let long = "é".repeat(5000);
        let cut = tail(&long, 8);
        assert_eq!(cut.chars().count(), 1000);
        assert!(cut.starts_with('…') && cut.ends_with('é'));
    }

    #[test]
    fn recent_output_keeps_the_end_of_a_flood() {
        let mut recent = Recent::default();
        recent.push(&vec![b'a'; Recent::KEEP * 3]);
        recent.push(b"the end");
        let text = recent.text();
        assert_eq!(text.len(), Recent::KEEP);
        assert!(text.ends_with("the end"));
        recent.push(&[0xff, b'\n']);
        assert!(recent.text().ends_with("the end\u{fffd}\n"));
    }

    proptest! {
        #[test]
        fn recent_output_is_the_end_of_all_that_came(pieces in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..3000), 0..20)) {
            let mut recent = Recent::default();
            let mut all = Vec::new();
            for piece in &pieces {
                recent.push(piece);
                all.extend_from_slice(piece);
            }
            let kept: Vec<u8> = recent.bytes.iter().copied().collect();
            prop_assert_eq!(&kept[..], &all[all.len().saturating_sub(Recent::KEEP)..]);
        }

        #[test]
        fn every_end_that_is_not_a_clean_exit_is_an_error(code in any::<i32>(), signal in 1i32..65, oom in 0u64..3, reset: bool) {
            for status in [Status::Code(code), Status::Signal(signal)] {
                let exit = exit_of(&Ending { oom_kills: oom, reset, ..ending(status) });
                let clean = status == Status::Code(0) && oom == 0 && !reset;
                prop_assert_eq!(exit == VmExit::Guest, clean);
                prop_assert!(exit != VmExit::Stopped);
            }
        }
    }
}
