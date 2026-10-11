//! Whatever order jobs, readings of the Mac, ends and crashes come in: no machine without a grant, no grant
//! leaked, no Actions runner left registered, every job reported exactly once.

mod support;

use std::collections::BTreeSet;

use cf_budget::{GIB, Pressure, Size};
use cf_daemon::names::job_name;
use cf_daemon::testing::pass;
use cf_daemon::{Claim, EventKind, Hosts, RESULT_WAIT, Seen, Slots, Ticket, Vms};
use cf_vmhost::End;
use proptest::prelude::*;
use support::world::World;

/// One thing that happens. A number picks among what is there at that moment.
#[derive(Debug, Clone)]
enum Op {
    /// A scaler promises GitHub a place.
    Promise(u8),
    /// A promise is given back.
    Unpromise(u8),
    /// GitHub assigns a job of this size.
    Assign(u8, bool),
    /// A job that waits in line is cancelled.
    Withdraw(u8),
    /// A scaler gone wrong starts a machine for a job whose turn has not come.
    StartEarly(u8),
    /// A scaler gone wrong gives back the slot of a machine that stands.
    GiveBackHeld(u8, bool),
    /// Seconds pass; machines whose turn came are started.
    Tick(u8),
    /// GitHub hands a job to an Actions runner that is up.
    JobStarts(u8),
    /// GitHub says a job is over.
    JobCompletes(u8, u8),
    /// A vm-host ends by itself, in one of its ways.
    VmEnds(u8, u8),
    VmDies(u8),
    VmHangs(u8),
    /// The scaler retires a machine.
    Stop(u8),
    StopAll,
    Pressure(bool),
    DiskFull(bool),
    Sleep,
    Unreadable(bool),
    Pause(bool),
    NextStartFails,
    SlowBoot(bool),
    Boots(u8),
    /// The daemon dies and starts again.
    Crash,
    /// The Mac starts again.
    Reboot,
    /// The service's sweep of registrations, with GitHub answering or not.
    Sweep(bool),
}

fn op() -> impl Strategy<Value = Op> {
    let n = any::<u8>;
    prop_oneof![
        2 => n().prop_map(Op::Promise),
        1 => n().prop_map(Op::Unpromise),
        8 => (n(), any::<bool>()).prop_map(|(size, promised)| Op::Assign(size, promised)),
        1 => n().prop_map(Op::Withdraw),
        1 => n().prop_map(Op::StartEarly),
        1 => (n(), any::<bool>()).prop_map(|(i, release)| Op::GiveBackHeld(i, release)),
        8 => (1u8..40).prop_map(Op::Tick),
        6 => n().prop_map(Op::JobStarts),
        4 => (n(), n()).prop_map(|(i, r)| Op::JobCompletes(i, r)),
        5 => (n(), n()).prop_map(|(i, e)| Op::VmEnds(i, e)),
        1 => n().prop_map(Op::VmDies),
        1 => n().prop_map(Op::VmHangs),
        2 => n().prop_map(Op::Stop),
        1 => Just(Op::StopAll),
        1 => any::<bool>().prop_map(Op::Pressure),
        1 => any::<bool>().prop_map(Op::DiskFull),
        1 => Just(Op::Sleep),
        1 => any::<bool>().prop_map(Op::Unreadable),
        1 => any::<bool>().prop_map(Op::Pause),
        1 => Just(Op::NextStartFails),
        1 => any::<bool>().prop_map(Op::SlowBoot),
        1 => n().prop_map(Op::Boots),
        3 => Just(Op::Crash),
        1 => Just(Op::Reboot),
        3 => any::<bool>().prop_map(Op::Sweep),
    ]
}

const SIZES: [Size; 4] = [Size::G2, Size::G2, Size::G4, Size::G8];
const ENDS: [End; 6] = [
    End::Ok,
    End::Failed,
    End::Oom,
    End::Timeout,
    End::GuestLost,
    End::Cancelled,
];
const RESULTS: [&str; 3] = ["succeeded", "failed", "canceled"];

fn pick<T: Clone>(from: &[T], i: u8) -> Option<T> {
    (!from.is_empty()).then(|| from[usize::from(i) % from.len()].clone())
}

/// What the test knows of the world beside the machines themselves.
#[derive(Default)]
struct Model {
    promises: Vec<Ticket>,
    waiting: Vec<(Ticket, Size)>,
    /// Actions runners GitHub handed a job, and those whose job it called over.
    job_started: BTreeSet<String>,
    job_completed: BTreeSet<String>,
    /// Every Actions runner that got a machine.
    runners: Vec<String>,
    /// The slots of the machines this daemon started.
    held: Vec<(String, Ticket)>,
}

fn up(w: &World, runner: &str) -> bool {
    matches!(w.parts.vms.look(job_name(runner)), Seen::Alive(state) if state.boot_ms.is_some())
}

fn standing(w: &World, model: &Model) -> Vec<String> {
    let standing = w.parts.vms.standing();
    model
        .runners
        .iter()
        .filter(|r| standing.iter().any(|job| job == job_name(r)))
        .cloned()
        .collect()
}

async fn start(w: &mut World, model: &mut Model, ticket: Ticket, size: Size) {
    match w.start_as(ticket, size).await {
        Ok(runner) => {
            model.held.push((runner.clone(), ticket));
            model.runners.push(runner);
        }
        // As the scaler does: the slot goes back; the registration is the sweep's.
        Err(_) => w.m.release(ticket),
    }
}

async fn apply(w: &mut World, model: &mut Model, op: Op) {
    match op {
        Op::Promise(size) => {
            if let Ok(ticket) = w.m.reserve(SIZES[usize::from(size) % 2]) {
                model.promises.push(ticket);
            }
        }
        Op::Unpromise(i) => {
            if let Some(ticket) = pick(&model.promises, i) {
                model.promises.retain(|t| *t != ticket);
                w.m.release(ticket);
            }
        }
        Op::Assign(size, promised) => {
            let size = SIZES[usize::from(size) % SIZES.len()];
            let promise = if promised { model.promises.pop() } else { None };
            match w.m.claim(size, promise) {
                Claim::Start(ticket) => start(w, model, ticket, size).await,
                Claim::Waits { ticket, .. } => model.waiting.push((ticket, size)),
                Claim::Never(_) => {}
            }
        }
        Op::Withdraw(i) => {
            if let Some((ticket, size)) = pick(&model.waiting, i) {
                model.waiting.retain(|w| *w != (ticket, size));
                w.m.withdraw(ticket);
            }
        }
        Op::StartEarly(i) => {
            let early: Vec<(Ticket, Size)> = model
                .waiting
                .iter()
                .copied()
                .filter(|(t, _)| !w.m.ready(*t))
                .collect();
            if let Some((ticket, size)) = pick(&early, i) {
                let refused = w.start_as(ticket, size).await;
                assert!(refused.is_err(), "a machine was started without a grant");
            }
        }
        Op::GiveBackHeld(i, release) => {
            let standing = standing(w, model);
            let held: Vec<Ticket> = model
                .held
                .iter()
                .filter(|(r, _)| standing.contains(r))
                .map(|(_, t)| *t)
                .collect();
            if let Some(ticket) = pick(&held, i) {
                if release {
                    w.m.release(ticket);
                } else {
                    w.m.withdraw(ticket);
                }
            }
        }
        Op::Tick(seconds) => {
            for _ in 0..seconds {
                pass(&w.m, 1);
                for (ticket, size) in model.waiting.clone() {
                    if w.m.ready(ticket) {
                        model.waiting.retain(|w| w.0 != ticket);
                        start(w, model, ticket, size).await;
                    }
                }
            }
        }
        Op::JobStarts(i) => {
            let free: Vec<String> = standing(w, model)
                .into_iter()
                .filter(|r| !model.job_started.contains(r) && up(w, r))
                .collect();
            if let Some(runner) = pick(&free, i) {
                w.github(EventKind::Started, &runner, "");
                model.job_started.insert(runner);
            }
        }
        Op::JobCompletes(i, result) => {
            let open: Vec<String> = model
                .job_started
                .difference(&model.job_completed)
                .cloned()
                .collect();
            if let Some(runner) = pick(&open, i) {
                w.github(
                    EventKind::Completed,
                    &runner,
                    RESULTS[usize::from(result) % RESULTS.len()],
                );
                model.job_completed.insert(runner);
            }
        }
        Op::VmEnds(i, end) => {
            if let Some(runner) = pick(&standing(w, model), i) {
                w.parts
                    .vms
                    .end(job_name(&runner), ENDS[usize::from(end) % ENDS.len()]);
            }
        }
        Op::VmDies(i) => {
            if let Some(runner) = pick(&standing(w, model), i) {
                w.parts.vms.die(job_name(&runner));
            }
        }
        Op::VmHangs(i) => {
            if let Some(runner) = pick(&standing(w, model), i) {
                w.parts.vms.hang(job_name(&runner));
            }
        }
        Op::Stop(i) => {
            if let Some(runner) = pick(&standing(w, model), i) {
                let _ = w.m.stop(job_name(&runner)).await;
            }
        }
        Op::StopAll => w.m.stop_all(),
        Op::Pressure(on) => w.m.with_host(|host| {
            host.set(|r| r.pressure = if on { Pressure::Critical } else { Pressure::Normal });
        }),
        Op::DiskFull(on) => w.m.with_host(|host| {
            host.set(|r| r.disk_free_bytes = if on { GIB } else { 1024 * GIB });
        }),
        Op::Sleep => w.m.with_host(|host| host.sleep(cf_budget::Millis::mins(20))),
        Op::Unreadable(on) => w.m.with_host(|host| {
            host.fail(on.then_some(cf_budget::HostError::Call {
                what: "host_statistics64",
                code: 5,
            }));
        }),
        Op::Pause(on) => w.m.pause(on),
        Op::NextStartFails => w.parts.vms.fail_start(1),
        Op::SlowBoot(on) => w.parts.vms.slow_boot(on),
        Op::Boots(i) => {
            if let Some(runner) = pick(&standing(w, model), i) {
                w.parts.vms.ready(job_name(&runner));
            }
        }
        Op::Crash => {
            // Promises and places in line are the daemon's own: GitHub assigns again what still waits.
            model.promises.clear();
            model.waiting.clear();
            model.held.clear();
            w.restart();
        }
        Op::Reboot => {
            model.promises.clear();
            model.waiting.clear();
            model.held.clear();
            w.reboot();
        }
        Op::Sweep(github_answers) => {
            if github_answers {
                w.sweep();
            }
        }
    }
}

/// What must hold at every moment.
fn check(w: &World, model: &Model) -> Result<(), TestCaseError> {
    let snapshot = w.m.snapshot();
    let listed: BTreeSet<String> = snapshot
        .machines
        .iter()
        .map(|m| job_name(&m.runner).to_string())
        .collect();
    for job in w.parts.vms.standing() {
        prop_assert!(
            listed.contains(&job),
            "machine {job} stands and the daemon does not know it"
        );
    }
    // Every machine the daemon knows holds a slot, and so does every job whose turn came.
    let ready = model.waiting.iter().filter(|(t, _)| w.m.ready(*t)).count();
    let slots = snapshot.budget.slots.len();
    prop_assert!(
        listed.len() + ready <= slots,
        "{} machines and {ready} turns on {slots} slots",
        listed.len()
    );
    prop_assert!(
        slots <= listed.len() + ready + model.promises.len(),
        "{slots} slots for {} machines, {ready} turns and {} promises: a grant leaked",
        listed.len(),
        model.promises.len()
    );
    let cap = w.m.settings().away.cap_bytes;
    prop_assert!(
        w.parts.vms.standing_bytes() <= cap,
        "machines of {} bytes on a Mac that gives {cap}",
        w.parts.vms.standing_bytes()
    );
    let reports = w.reports();
    let ids: BTreeSet<&str> = reports.iter().map(|r| r.id.as_str()).collect();
    prop_assert_eq!(ids.len(), reports.len(), "a job was reported twice");
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 300, max_shrink_iters: 4000, ..ProptestConfig::default() })]

    #[test]
    fn no_order_of_events_leaves_a_machine_a_grant_a_registration_or_an_unreported_job(
        ops in proptest::collection::vec(op(), 1..120)
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread().build().expect("runtime");
        runtime.block_on(async {
            let mut w = World::new(3);
            let mut model = Model::default();
            for op in ops {
                apply(&mut w, &mut model, op).await;
                check(&w, &model)?;
            }

            // The world comes to rest: the Mac is calm, GitHub answers, every job is over.
            for op in [Op::Pressure(false), Op::DiskFull(false), Op::Unreadable(false), Op::Pause(false), Op::SlowBoot(false)] {
                apply(&mut w, &mut model, op).await;
            }
            for (ticket, _) in std::mem::take(&mut model.waiting) {
                w.m.withdraw(ticket);
            }
            for ticket in std::mem::take(&mut model.promises) {
                w.m.release(ticket);
            }
            for runner in standing(&w, &model) {
                // A vm-host that hangs ends only when the daemon ends it.
                if !matches!(w.parts.vms.look(job_name(&runner)), Seen::Silent(_)) {
                    w.parts.vms.end(job_name(&runner), End::Ok);
                }
            }
            for runner in model.job_started.difference(&model.job_completed) {
                w.github(EventKind::Completed, runner, "failed");
            }
            pass(&w.m, 90);
            check(&w, &model)?;
            w.sweep();
            pass(&w.m, RESULT_WAIT.as_secs() + 5);
            w.sweep();
            pass(&w.m, 2);

            prop_assert_eq!(w.parts.vms.standing(), Vec::<String>::new(), "a machine stands");
            prop_assert_eq!(w.parts.vms.files(), Vec::<String>::new(), "files are left");
            prop_assert_eq!(w.m.snapshot().budget.slots.len(), 0, "a grant leaked");
            prop_assert_eq!(w.m.snapshot().waiting, 0);
            prop_assert_eq!(&w.registered, &BTreeSet::new(), "an Actions runner is left registered");
            prop_assert_eq!(w.parts.store.load(), vec![], "records are left");
            let mut reported: Vec<String> = w.reports().into_iter().map(|r| r.id).collect();
            reported.sort();
            let mut expected: Vec<String> = model
                .job_started
                .iter()
                .flat_map(|r| [format!("{r}/completed"), format!("{r}/started")])
                .collect();
            expected.sort();
            prop_assert_eq!(reported, expected, "every job once: its start and its end");
            Ok(())
        })?;
    }
}
