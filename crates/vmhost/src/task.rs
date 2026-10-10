//! One of the guest's small tasks, run from the machine's start to its end: a probe, formatting a disk. A job
//! goes through [`crate::run`], which survives more.

use cf_guest_proto::{
    GuestMessage, HostMessage, PORT, PROTOCOL, Start, Summary, Task, read_message, write_message,
};
use cf_vmm::{Channel, Vm, VmExit, VmSpec, Vmm};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// What a task's machine did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskReport {
    /// The guest's own account of the task.
    pub summary: Summary,
    /// The guest's kernel.
    pub kernel: String,
    /// From the machine's start to the guest's `ready`.
    pub ready_after: Duration,
    /// From the machine's start to its end.
    pub ended_after: Duration,
    /// Whether the guest powered itself off (`false`: the host had to stop it).
    pub powered_off: bool,
}

/// The time to hand a guest, in nanoseconds since 1970.
pub(crate) fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() as u64)
}

/// Opens the channel to a guest that may still be booting: tries until it listens, the machine ends or the time
/// is over.
pub fn connect(vm: &dyn Vm, within: Duration) -> Result<Box<dyn Channel>, String> {
    let deadline = Instant::now() + within;
    loop {
        match vm.channel(PORT) {
            Ok(channel) => return Ok(channel),
            Err(error) if Instant::now() >= deadline => return Err(error.to_string()),
            Err(_) => {}
        }
        if let Some(exit) = vm.wait(Duration::from_millis(25)) {
            return Err(format!(
                "the machine ended before its guest listened ({})",
                describe(&exit)
            ));
        }
    }
}

/// A machine's end in words.
pub(crate) fn describe(exit: &VmExit) -> String {
    match exit {
        VmExit::Guest => "the guest powered off".into(),
        VmExit::Stopped => "stopped by the host".into(),
        VmExit::Error(error) => error.clone(),
    }
}

/// Ends a machine that should be gone: a stop, a moment for it, then letting go.
pub(crate) fn put_down(vm: &dyn Vm, within: Duration) -> bool {
    if vm.wait(Duration::ZERO).is_some() {
        return true;
    }
    if vm.stop().is_ok() && vm.wait(within).is_some() {
        return true;
    }
    vm.kill();
    false
}

/// Boots a machine, gives its guest one task and waits for the end, all within `within`. An error says in words
/// what went wrong; the machine is gone either way.
pub fn run_task(vmm: &dyn Vmm, spec: &VmSpec, task: Task, within: Duration) -> Result<TaskReport, String> {
    let started = Instant::now();
    let left = |what: &str| {
        within
            .checked_sub(started.elapsed())
            .ok_or(format!("{what} took longer than {} s", within.as_secs()))
    };
    let vm = vmm.start(spec).map_err(|error| error.to_string())?;
    let result = (|| {
        let mut channel = connect(vm.as_ref(), left("the boot")?)?;
        let mut reader = channel
            .try_clone()
            .map_err(|error| format!("the channel: {error}"))?;
        let (sender, messages) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(Some(message)) = read_message::<GuestMessage>(&mut reader) {
                if sender.send(message).is_err() {
                    return;
                }
            }
        });
        let next = |what: &str| {
            messages
                .recv_timeout(left(what)?)
                .map_err(|_| format!("the guest went silent during {what}"))
        };
        let kernel = match next("the boot")? {
            GuestMessage::Hello {
                protocol: PROTOCOL,
                kernel,
            } => kernel,
            GuestMessage::Hello { protocol, .. } => {
                return Err(format!(
                    "the guest speaks protocol {protocol}, the host {PROTOCOL}"
                ));
            }
            _ => return Err("the guest did not say hello first".into()),
        };
        write_message(
            &mut channel,
            &HostMessage::Start(Start {
                protocol: PROTOCOL,
                time_ns: now_ns(),
                task,
            }),
        )
        .map_err(|error| error.to_string())?;
        let mut ready_after = None;
        let summary = loop {
            match next("the task")? {
                GuestMessage::Ready { .. } => ready_after = Some(started.elapsed()),
                GuestMessage::Exited(summary) => break summary,
                GuestMessage::Failed { stage, message } => {
                    return Err(format!("the guest failed at {stage}: {message}"));
                }
                _ => {}
            }
        };
        channel.shutdown();
        let powered_off = vm.wait(
            left("the power-off")
                .unwrap_or_default()
                .min(Duration::from_secs(10)),
        ) == Some(VmExit::Guest);
        Ok(TaskReport {
            summary,
            kernel,
            ready_after: ready_after.unwrap_or_else(|| started.elapsed()),
            ended_after: started.elapsed(),
            powered_off,
        })
    })();
    put_down(vm.as_ref(), Duration::from_secs(5));
    result
}
