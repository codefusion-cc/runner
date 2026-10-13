//! How a machine ended, from what is known once its monitor's process has: how the monitor ended as the sandbox
//! reports it, the exit status of the machine's first process, whether the host asked for the stop, whether the
//! kernel killed in its cgroup for want of memory, whether the guest reset it, and the last lines the processes
//! wrote.

use cf_vmm::VmExit;

/// The exit code of `codefusion-runner vm-sandbox` when it could not set the machine's sandbox up: no monitor ran.
pub const SANDBOX_FAILED: i32 = 125;

/// How a process ended, as `waitpid` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// It exited with this code.
    Code(i32),
    /// A signal ended it.
    Signal(i32),
}

impl From<std::process::ExitStatus> for Status {
    fn from(status: std::process::ExitStatus) -> Self {
        use std::os::unix::process::ExitStatusExt as _;
        match status.code() {
            Some(code) => Status::Code(code),
            None => Status::Signal(status.signal().unwrap_or(0)),
        }
    }
}

impl Status {
    /// The exit code a shell gives a process that ended so: its own, or 128 and the signal.
    pub fn exit_code(self) -> i32 {
        match self {
            Status::Code(code) => code,
            Status::Signal(signal) => 128 + signal,
        }
    }
}

/// What the sandbox writes last, once the monitor has ended, to say how it did. The exit status of the machine's
/// first process does not say it on its own: pasta of before 2024 exits 0 when the program it runs dies of a signal.
const REPORT: &str = "vm-sandbox: the monitor ended with ";

/// The line the sandbox writes of a monitor that ended so.
pub fn report(monitor: Status) -> String {
    match monitor {
        Status::Code(code) => format!("{REPORT}code {code}"),
        Status::Signal(signal) => format!("{REPORT}signal {signal}"),
    }
}

/// How the monitor ended, as the last report in `output` says, if there is one. The sandbox writes its report
/// after the monitor and everything in its PID namespace are gone, so no line the monitor wrote comes after it.
pub fn reported(output: &str) -> Option<Status> {
    let said = output.lines().rev().find_map(|line| line.strip_prefix(REPORT))?;
    let (kind, number) = said.split_once(' ')?;
    let number: i32 = number.parse().ok()?;
    match kind {
        "code" => Some(Status::Code(number)),
        "signal" => Some(Status::Signal(number)),
        _ => None,
    }
}

/// What is known of a machine that ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ending<'a> {
    /// The host asked for its end ([`cf_vmm::Vm::stop`] or [`cf_vmm::Vm::kill`]).
    pub stop_asked: bool,
    /// How the machine's first process (pasta, or the sandbox without a network) ended.
    pub status: Status,
    /// How often the kernel killed in its cgroup for want of memory.
    pub oom_kills: u64,
    /// The guest reset the machine, which the host then ended.
    pub reset: bool,
    /// The machine's memory and what its cgroup allowed beyond it, for the message of an out-of-memory end.
    pub memory_max: u64,
    /// The last of what the monitor, pasta and the sandbox wrote, the sandbox's report ([`report`]) among it.
    pub output: &'a str,
}

/// The machine's end. The host's stop comes first: whatever the machine did meanwhile, it was asked to end.
/// Only a monitor that the sandbox saw exit 0 of its own was a guest's power-off; every other end is an error
/// that says why.
pub fn exit_of(ending: &Ending<'_>) -> VmExit {
    if ending.stop_asked {
        return VmExit::Stopped;
    }
    let said = tail(&without_reports(ending.output), 8);
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
    match (reported(ending.output), ending.status) {
        (Some(Status::Code(0)), _) => VmExit::Guest,
        (Some(Status::Code(code)), _) => {
            VmExit::Error(format!("the monitor exited with code {code}{and_said}"))
        }
        (Some(Status::Signal(signal)), _) => {
            VmExit::Error(format!("the monitor was ended by signal {signal}{and_said}"))
        }
        (None, Status::Code(SANDBOX_FAILED)) => {
            VmExit::Error(format!("the machine's sandbox could not be set up{and_said}"))
        }
        (None, Status::Code(code)) => VmExit::Error(format!(
            "the machine ended without its sandbox saying how the monitor did (exit code {code}){and_said}"
        )),
        (None, Status::Signal(signal)) => VmExit::Error(format!(
            "the machine ended without its sandbox saying how the monitor did (signal {signal}){and_said}"
        )),
    }
}

fn without_reports(output: &str) -> String {
    output
        .lines()
        .filter(|line| !line.starts_with(REPORT))
        .collect::<Vec<_>>()
        .join("\n")
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

    #[test]
    fn an_exit_status_is_told_as_a_shell_tells_it() {
        use std::os::unix::process::ExitStatusExt as _;
        let exited = std::process::ExitStatus::from_raw(7 << 8);
        let killed = std::process::ExitStatus::from_raw(libc::SIGKILL);
        assert_eq!(Status::from(exited), Status::Code(7));
        assert_eq!(Status::from(killed), Status::Signal(9));
        assert_eq!(Status::Code(7).exit_code(), 7);
        assert_eq!(Status::Signal(9).exit_code(), 137);
        assert_eq!(Status::Code(0).exit_code(), 0);
    }
    use proptest::prelude::*;

    const POWERED_OFF: &str = "vm-sandbox: the monitor ended with code 0\n";

    fn ending(status: Status, output: &'static str) -> Ending<'static> {
        Ending {
            stop_asked: false,
            status,
            oom_kills: 0,
            reset: false,
            memory_max: 2208 << 20,
            output,
        }
    }

    impl<'a> Ending<'a> {
        fn with(self, output: &'a str) -> Ending<'a> {
            Ending { output, ..self }
        }
    }

    fn error(exit: VmExit) -> String {
        match exit {
            VmExit::Error(why) => why,
            other => panic!("{other:?} is no error"),
        }
    }

    #[test]
    fn a_monitor_the_sandbox_saw_exit_0_was_a_power_off() {
        assert_eq!(exit_of(&ending(Status::Code(0), POWERED_OFF)), VmExit::Guest);
        // pasta's own status does not change what the sandbox saw.
        assert_eq!(exit_of(&ending(Status::Code(3), POWERED_OFF)), VmExit::Guest);
    }

    #[test]
    fn a_monitor_killed_under_an_old_pasta_that_exits_0_is_an_error() {
        let why = error(exit_of(&ending(
            Status::Code(0),
            "vm-sandbox: the monitor ended with signal 9\n",
        )));
        assert_eq!(why, "the monitor was ended by signal 9");
    }

    #[test]
    fn an_exit_0_without_the_sandbox_saying_so_is_no_power_off() {
        let why = error(exit_of(&ending(Status::Code(0), "")));
        assert_eq!(
            why,
            "the machine ended without its sandbox saying how the monitor did (exit code 0)"
        );
        let why = error(exit_of(&ending(Status::Signal(9), "pasta: no tap\n")));
        assert_eq!(
            why,
            "the machine ended without its sandbox saying how the monitor did (signal 9): pasta: no tap"
        );
    }

    #[test]
    fn the_last_report_counts_and_a_broken_one_is_none() {
        let output =
            "vm-sandbox: the monitor ended with code 0\nnoise\nvm-sandbox: the monitor ended with code 2\n";
        assert_eq!(reported(output), Some(Status::Code(2)));
        for broken in [
            "vm-sandbox: the monitor ended with code\n",
            "vm-sandbox: the monitor ended with code x\n",
            "vm-sandbox: the monitor ended with exit 0\n",
            " vm-sandbox: the monitor ended with code 0\n",
            "vm-sandbox: the monitor ended with code 99999999999\n",
        ] {
            assert_eq!(reported(broken), None, "{broken}");
        }
        assert_eq!(reported(&report(Status::Signal(11))), Some(Status::Signal(11)));
        assert_eq!(reported(&report(Status::Code(-1))), Some(Status::Code(-1)));
    }

    #[test]
    fn a_stop_the_host_asked_for_is_stopped_however_the_monitor_ended() {
        for (status, output) in [
            (Status::Code(0), POWERED_OFF),
            (Status::Code(1), ""),
            (Status::Signal(9), ""),
            (Status::Code(137), "vm-sandbox: the monitor ended with signal 9"),
        ] {
            let stopped = Ending {
                stop_asked: true,
                oom_kills: 1,
                reset: true,
                ..ending(status, output)
            };
            assert_eq!(exit_of(&stopped), VmExit::Stopped, "{status:?}");
        }
    }

    #[test]
    fn a_machine_out_of_memory_says_so_with_its_limit() {
        let why = error(exit_of(&Ending {
            oom_kills: 1,
            ..ending(Status::Signal(9), "x\n")
        }));
        assert_eq!(
            why,
            "the machine used more than the 2208 MiB its cgroup allows, and the kernel ended it: x"
        );
        // Even when the monitor seemed to end well before the kill took the rest.
        assert!(
            error(exit_of(&Ending {
                oom_kills: 2,
                ..ending(Status::Code(0), POWERED_OFF)
            }))
            .contains("MiB")
        );
    }

    #[test]
    fn a_reset_is_an_error_even_though_the_monitor_was_killed_for_it() {
        for (status, output) in [(Status::Signal(9), ""), (Status::Code(0), POWERED_OFF)] {
            let why = error(exit_of(&Ending {
                reset: true,
                ..ending(status, output)
            }));
            assert!(why.starts_with("the guest reset its machine"), "{why}");
        }
    }

    #[test]
    fn a_crash_or_a_kill_from_elsewhere_is_an_error_with_the_last_lines() {
        let crashed = ending(
            Status::Code(1),
            "\nError booting VM: no kernel\n\nvm-sandbox: the monitor ended with code 1\n",
        );
        assert_eq!(
            exit_of(&crashed),
            VmExit::Error("the monitor exited with code 1: Error booting VM: no kernel".into())
        );
        assert_eq!(
            exit_of(&ending(
                Status::Code(139),
                "vm-sandbox: the monitor ended with signal 11\n"
            )),
            VmExit::Error("the monitor was ended by signal 11".into())
        );
    }

    #[test]
    fn a_sandbox_that_could_not_be_set_up_says_so() {
        let why = error(exit_of(&ending(Status::Code(SANDBOX_FAILED), "bridge: refused")));
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
        fn only_a_reported_exit_0_with_nothing_else_wrong_is_a_power_off(
            first in any::<i32>(),
            reports in proptest::collection::vec(any::<i32>(), 0..3),
            signal in 1i32..65,
            oom in 0u64..3,
            reset: bool,
        ) {
            let output: String = reports.iter().map(|code| report(Status::Code(*code)) + "\n").collect();
            for status in [Status::Code(first), Status::Signal(signal)] {
                let exit = exit_of(&Ending { oom_kills: oom, reset, ..ending(status, "") }.with(&output));
                let clean = reports.last() == Some(&0) && oom == 0 && !reset;
                prop_assert_eq!(exit == VmExit::Guest, clean);
                prop_assert!(exit != VmExit::Stopped);
            }
        }
    }
}
