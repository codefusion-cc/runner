//! Stand-ins for the edges of the daemon: the vm-hosts, the Mac's readings, and where reports go. For tests.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cf_budget::{GIB, HostFacts, Millis, Mode, Settings, Size};
use cf_ghapp::Clock;
use cf_guest_proto::{GuestEnd, OomEvent, Process, StopReason, Summary, Usage};
use cf_host_macos::FakeHost;
use cf_vmhost::{End, Ended, JobRequest, JobState, Stage};

use crate::machines::{JobReport, MachineConfig, MachineParts, Machines, Reports};
use crate::records::Store;
use crate::roster::Roster;
use crate::traits::{Events, JobEvent};
use crate::vms::{Seen, VmError, Vms};

#[derive(Debug, Clone)]
struct FakeVm {
    state: JobState,
    /// Whether the vm-host's process is there, and the machine's memory with it.
    alive: bool,
    silent: bool,
    files: bool,
    unkillable: bool,
}

#[derive(Debug, Default)]
struct VmState {
    vms: BTreeMap<String, FakeVm>,
    started: Vec<JobRequest>,
    stopped: Vec<(String, StopReason, Duration)>,
    killed: Vec<String>,
    cleaned: Vec<String>,
    fail_start: usize,
    slow_boot: bool,
    ignore_stops: bool,
    next_pid: u32,
}

/// The vm-hosts of a test: machines that start, boot, end, die, hang and refuse as a test says. By default a
/// machine is ready as soon as it is started and ends at once when it is asked to stop.
#[derive(Debug, Clone, Default)]
pub struct FakeVms(Arc<Mutex<VmState>>);

impl FakeVms {
    fn lock(&self) -> std::sync::MutexGuard<'_, VmState> {
        self.0.lock().expect("vms")
    }

    fn with(&self, job: &str, change: impl FnOnce(&mut FakeVm)) {
        if let Some(vm) = self.lock().vms.get_mut(job) {
            change(vm);
        }
    }

    /// The next `n` starts fail.
    pub fn fail_start(&self, n: usize) {
        self.lock().fail_start = n;
    }

    /// Machines stay booting until [`FakeVms::ready`].
    pub fn slow_boot(&self, slow: bool) {
        self.lock().slow_boot = slow;
    }

    /// Machines take a stop and go on.
    pub fn ignore_stops(&self, ignore: bool) {
        self.lock().ignore_stops = ignore;
    }

    /// The guest reports `ready` and the workload starts.
    pub fn ready(&self, job: &str) {
        self.with(job, |vm| {
            vm.state.stage = Stage::Running;
            vm.state.boot_ms = Some(1200);
        });
    }

    /// The job holds this much memory and disk now.
    pub fn usage(&self, job: &str, memory_bytes: u64, disk_bytes: u64) {
        self.with(job, |vm| {
            let peak = vm.state.usage.as_ref().map_or(0, |u| u.memory_peak_bytes);
            vm.state.usage = Some(Usage {
                memory_bytes,
                memory_peak_bytes: peak.max(memory_bytes),
                cpu_usec: 1_000_000,
                disk_bytes,
                pids: 3,
            });
        });
    }

    /// The kernel stops a process of the job for memory.
    pub fn out_of_memory(&self, job: &str) {
        self.with(job, |vm| {
            vm.state.oom = Some(OomEvent {
                process: Process {
                    pid: 77,
                    name: "cc1plus".into(),
                    memory_bytes: 900 << 20,
                },
                limit_bytes: vm.state.limit_bytes,
                largest: vec![],
            });
        });
    }

    /// The vm-host writes this end and exits; the machine is gone.
    pub fn end(&self, job: &str, end: End) {
        self.with(job, |vm| finish(vm, end));
    }

    /// The vm-host is killed from outside: its state stays as it was, without an end.
    pub fn die(&self, job: &str) {
        self.with(job, |vm| vm.alive = false);
    }

    /// Nothing of the job is there, as when the daemon died before its vm-host existed.
    pub fn forget(&self, job: &str) {
        self.lock().vms.remove(job);
    }

    /// The vm-host stops answering.
    pub fn hang(&self, job: &str) {
        self.with(job, |vm| vm.silent = true);
    }

    /// The vm-host survives a SIGKILL.
    pub fn unkillable(&self, job: &str, unkillable: bool) {
        self.with(job, |vm| vm.unkillable = unkillable);
    }

    /// The jobs whose vm-host is there, holding its machine's memory.
    pub fn standing(&self) -> Vec<String> {
        self.lock()
            .vms
            .iter()
            .filter(|(_, vm)| vm.alive)
            .map(|(job, _)| job.clone())
            .collect()
    }

    /// The memory of the machines that stand.
    pub fn standing_bytes(&self) -> u64 {
        self.lock()
            .vms
            .values()
            .filter(|vm| vm.alive)
            .map(|vm| vm.state.vm_bytes)
            .sum()
    }

    /// Every request a vm-host was started with, in order.
    pub fn started(&self) -> Vec<JobRequest> {
        self.lock().started.clone()
    }

    /// Every stop a vm-host was asked for.
    pub fn stopped(&self) -> Vec<(String, StopReason, Duration)> {
        self.lock().stopped.clone()
    }

    /// Every job a kill was sent for.
    pub fn killed(&self) -> Vec<String> {
        self.lock().killed.clone()
    }

    /// Every job whose files were removed.
    pub fn cleaned(&self) -> Vec<String> {
        self.lock().cleaned.clone()
    }

    /// The jobs that have files in the state folder.
    pub fn files(&self) -> Vec<String> {
        self.jobs()
    }
}

fn finish(vm: &mut FakeVm, end: End) {
    let usage = vm.state.usage.unwrap_or(Usage {
        memory_bytes: 0,
        memory_peak_bytes: 300 << 20,
        cpu_usec: 2_000_000,
        disk_bytes: 0,
        pids: 0,
    });
    let heard = !matches!(end, End::GuestLost | End::BootFailed);
    vm.state.end = Some(Ended {
        end,
        message: None,
        summary: heard.then(|| Summary {
            end: GuestEnd::Exited,
            exit_code: Some(i32::from(end == End::Failed)),
            signal: None,
            usage,
            disk_peak_bytes: 5 << 20,
            oom: vm.state.oom.clone(),
            wall_ms: 4200,
        }),
        at: 0,
    });
    vm.state.stage = Stage::Ended;
    vm.alive = false;
}

impl Vms for FakeVms {
    fn start(&self, request: &JobRequest) -> Result<(), VmError> {
        let mut state = self.lock();
        if state.fail_start > 0 {
            state.fail_start -= 1;
            return Err(VmError("the vm-host did not start".into()));
        }
        request.check().map_err(|error| VmError(error.to_string()))?;
        state.next_pid += 1;
        let ready = !state.slow_boot;
        let vm = FakeVm {
            state: JobState {
                job: request.job.clone(),
                pid: 1000 + state.next_pid,
                started_at: 0,
                stage: if ready { Stage::Running } else { Stage::Booting },
                vm_bytes: request.memory_bytes,
                limit_bytes: request.limits().memory_bytes,
                cpus: request.cpus,
                image: request.image.clone(),
                boots: 1,
                boot_ms: ready.then_some(1200),
                usage: None,
                oom: None,
                end: None,
            },
            alive: true,
            silent: false,
            files: true,
            unkillable: false,
        };
        state.vms.insert(request.job.clone(), vm);
        state.started.push(request.clone());
        Ok(())
    }

    fn look(&self, job: &str) -> Seen {
        match self.lock().vms.get(job) {
            Some(vm) if !vm.files => Seen::Nothing,
            Some(vm) if !vm.alive => Seen::Gone(vm.state.clone()),
            Some(vm) if vm.silent => Seen::Silent(vm.state.clone()),
            Some(vm) => Seen::Alive(vm.state.clone()),
            None => Seen::Nothing,
        }
    }

    fn stop(&self, job: &str, reason: StopReason, grace: Duration) -> bool {
        let mut state = self.lock();
        state.stopped.push((job.to_string(), reason, grace));
        let ignore = state.ignore_stops;
        match state.vms.get_mut(job) {
            Some(vm) if vm.alive && !vm.silent => {
                if !ignore {
                    finish(vm, End::of_stop(reason));
                }
                true
            }
            _ => false,
        }
    }

    fn kill(&self, job: &str) {
        let mut state = self.lock();
        state.killed.push(job.to_string());
        if let Some(vm) = state.vms.get_mut(job)
            && !vm.unkillable
        {
            vm.alive = false;
        }
    }

    fn jobs(&self) -> Vec<String> {
        self.lock()
            .vms
            .iter()
            .filter(|(_, vm)| vm.files)
            .map(|(job, _)| job.clone())
            .collect()
    }

    fn clean(&self, job: &str) {
        let mut state = self.lock();
        state.cleaned.push(job.to_string());
        let gone = match state.vms.get_mut(job) {
            Some(vm) => {
                vm.files = false;
                !vm.alive
            }
            None => false,
        };
        if gone {
            state.vms.remove(job);
        }
    }
}

/// A Mac for tests: 64 GB and 16 cores that nobody works on, with room for `machines` of the smallest size at
/// once and one vCPU a machine, so that the count alone decides.
pub fn settings_for(machines: u32) -> Settings {
    let mut settings = Settings::for_host(TEST_MAC, Mode::Dedicated);
    settings.away.cap_bytes = u64::from(machines) * Size::G2.vm_bytes();
    settings.in_use = settings.away;
    settings.vcpus = [1; 4];
    settings
}

/// The Mac [`settings_for`] is for.
pub const TEST_MAC: HostFacts = HostFacts {
    total_bytes: 64 * GIB,
    cores: 16,
};

/// The machines of a test.
pub type TestMachines = Machines<FakeHost, FakeVms>;

/// What a test's machines are made of, kept so that a test can make them again as a daemon that starts again does.
#[derive(Clone)]
pub struct TestParts {
    /// The vm-hosts, which outlive a daemon.
    pub vms: FakeVms,
    /// The records, which outlive a daemon.
    pub store: Arc<dyn Store>,
    /// The Actions runners.
    pub roster: Arc<Roster>,
    /// The wall clock.
    pub clock: Arc<dyn Clock>,
    /// Where job reports go.
    pub reports: Arc<ReportLog>,
    /// The budget's numbers.
    pub settings: Settings,
    /// How often the machines nudged the scalers.
    pub nudges: Arc<std::sync::atomic::AtomicUsize>,
    /// The bell the machines ring, for a test with listeners.
    pub bell: Arc<crate::service::Nudge>,
    /// The Mac's boot.
    pub boot: i64,
}

impl TestParts {
    /// Machines on a calm Mac that was watched for a minute, so slots are open. `host` carries the Mac's clock on
    /// from the daemon before.
    pub fn machines(&self, host: Option<FakeHost>) -> Arc<TestMachines> {
        let machines = self.restored(host.unwrap_or_else(|| FakeHost::new(TEST_MAC)));
        pass(&machines, 61);
        machines
    }

    /// Machines as a daemon that just started has them, before its first look at the Mac.
    pub fn restored(&self, host: FakeHost) -> Arc<TestMachines> {
        let (nudges, bell) = (self.nudges.clone(), self.bell.clone());
        let machines = Machines::restore(MachineParts {
            host,
            vms: self.vms.clone(),
            settings: self.settings.clone(),
            config: MachineConfig::new("mac-b", "2026.10.1", self.boot, Duration::from_secs(6 * 3600)),
            roster: self.roster.clone(),
            store: self.store.clone(),
            clock: self.clock.clone(),
            reports: self.reports.clone(),
            nudge: Box::new(move || {
                nudges.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                bell.fire();
            }),
        })
        .expect("settings");
        Arc::new(machines)
    }
}

/// Lets `seconds` pass on the Mac, with a reading and a tick each second.
pub fn pass(machines: &TestMachines, seconds: u64) {
    for _ in 0..seconds {
        machines.with_host(|host| host.pass(Millis::secs(1)));
        machines.tick();
    }
}

/// Collects job reports.
#[derive(Default)]
pub struct ReportLog(Mutex<Vec<JobReport>>, Mutex<Option<Arc<dyn Reports>>>);

impl ReportLog {
    /// Every report, in order.
    pub fn all(&self) -> Vec<JobReport> {
        self.0.lock().expect("reports").clone()
    }

    /// Every report from now on goes there too.
    pub fn forward_to(&self, reports: Arc<dyn Reports>) {
        *self.1.lock().expect("reports") = Some(reports);
    }
}

impl Reports for ReportLog {
    fn job(&self, report: JobReport) {
        self.0.lock().expect("reports").push(report.clone());
        if let Some(also) = self.1.lock().expect("reports").as_ref() {
            also.job(report);
        }
    }
}

/// Sends job events to two listeners: a test's log and the machines.
pub struct Both<A, B>(pub Arc<A>, pub Arc<B>);

impl<A: Events, B: Events> Events for Both<A, B> {
    fn job(&self, event: &JobEvent) {
        self.0.job(event);
        self.1.job(event);
    }

    fn out_of_memory(&self, event: &JobEvent) {
        self.0.out_of_memory(event);
        self.1.out_of_memory(event);
    }
}

/// Collects the job events.
#[derive(Debug, Default)]
pub struct EventLog {
    jobs: Mutex<Vec<JobEvent>>,
    oom: Mutex<Vec<JobEvent>>,
}

impl EventLog {
    /// Started and completed events, in order.
    pub fn jobs(&self) -> Vec<JobEvent> {
        self.jobs.lock().expect("events").clone()
    }

    /// Events of jobs that used up their memory.
    pub fn out_of_memory(&self) -> Vec<JobEvent> {
        self.oom.lock().expect("events").clone()
    }
}

impl Events for EventLog {
    fn job(&self, event: &JobEvent) {
        self.jobs.lock().expect("events").push(event.clone());
    }

    fn out_of_memory(&self, event: &JobEvent) {
        self.oom.lock().expect("events").push(event.clone());
    }
}
