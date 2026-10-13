//! Schedules of what GitHub, the machines, time and crashes do, against a GitHub that keeps the truth: which Actions
//! runner is registered and which ran a job. Whatever the schedule, and whichever older format the records were left
//! in, these hold (`docs/job-state.md`):
//!
//! - a machine whose job runs is never stopped;
//! - a job that ran is reported started and ended, each once (by its report's id);
//! - nothing stays forever: no machine, slot, registration or record outlives the jobs;
//! - the stored facts agree after every single write ([`Record::agrees`]);
//! - reading a record again changes nothing ([`Record::upgrade`] is idempotent).
//!
//! A job is reported that did not run only where GitHub's answer can't tell: its 204 to a removal was lost (the
//! answer, or the daemon with it), so the next removal meets 404, which is what a job over also leaves.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use cf_daemon::names::job_name;
use cf_daemon::testing::{CrashingStore, pass, settings_for};
use cf_daemon::{Answer, Claim, EventKind, Hosts, RESULT_WAIT, Record, Slots, Upgrade};
use cf_lifecycle::{End, Phase};
use proptest::prelude::*;
use support::pick;
use support::world::{REPO, SET, SIZE, World};

/// What GitHub says to one removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reply {
    /// It answers by the truth: 204, 404 or 422.
    Truth,
    /// A 5xx: nothing changed at GitHub.
    ServerError,
    /// No answer in time, and nothing changed.
    Timeout,
    /// GitHub did what was asked, and the answer was lost on the way.
    Lost,
}

const REPLIES: [Reply; 4] = [Reply::Truth, Reply::ServerError, Reply::Timeout, Reply::Lost];

/// An older Runner's record format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    /// main: no rules, no stored facts, `unregistered` maybe a belief.
    Main,
    /// Rules 1: an idle slot with an end of "no job" for one asking.
    Rules1,
}

/// One thing that happens. A number picks among what is there at that moment.
#[derive(Debug, Clone, Copy)]
enum Op {
    /// GitHub assigns a job and there is room: a registration and a machine.
    Assign,
    /// A machine's guest comes up.
    Boots(u8),
    /// GitHub hands a job to an Actions runner that is up; its message comes, or goes with a session.
    JobStarts(u8, bool),
    /// The job is over: its Actions runner removes its own registration and its machine ends.
    JobCompletes(u8, bool),
    /// A message of a job comes again, or late.
    Again(u8),
    /// The service's sweep; each removal gets one of [`REPLIES`], from this number on.
    Sweep(u8),
    /// GitHub's REST view at a session's opening names a busy Actions runner.
    Busy(u8),
    /// The scaler finds the idle ones surplus.
    Retire(u8),
    /// Time passes.
    Tick(u8),
    /// The daemon dies at its `n`th write from now, which is kept or not.
    CrashAt(u8, bool),
    /// The daemon dies and starts again.
    Restart,
    /// A record is left as an older Runner wrote it, and the daemon starts again.
    Downgrade(u8, Format),
}

fn op() -> impl Strategy<Value = Op> {
    let n = any::<u8>;
    prop_oneof![
        5 => Just(Op::Assign),
        4 => n().prop_map(Op::Boots),
        5 => (n(), any::<bool>()).prop_map(|(i, heard)| Op::JobStarts(i, heard)),
        4 => (n(), any::<bool>()).prop_map(|(i, heard)| Op::JobCompletes(i, heard)),
        2 => n().prop_map(Op::Again),
        6 => n().prop_map(Op::Sweep),
        1 => n().prop_map(Op::Busy),
        2 => n().prop_map(Op::Retire),
        8 => n().prop_map(Op::Tick),
        3 => (1u8..6, any::<bool>()).prop_map(|(n, kept)| Op::CrashAt(n, kept)),
        1 => Just(Op::Restart),
        2 => (n(), prop_oneof![Just(Format::Main), Just(Format::Rules1)]).prop_map(|(i, f)| Op::Downgrade(i, f)),
    ]
}

/// What GitHub knows of one Actions runner: the truth.
#[derive(Debug, Default, Clone)]
struct Truth {
    registered: bool,
    /// The job it took, and whether it is over.
    job: Option<bool>,
    /// Stops and kills of its machine when its job started.
    stops_at_start: usize,
    /// GitHub removed its registration and the Runner may not know: the next removal meets 404.
    unknown_204: bool,
}

#[derive(Default)]
struct Model {
    runners: BTreeMap<String, Truth>,
}

fn stops(w: &World, runner: &str) -> usize {
    let job = job_name(runner);
    let stopped = w.parts.vms.stopped().iter().filter(|(j, ..)| j == job).count();
    stopped + w.parts.vms.killed().iter().filter(|j| *j == job).count()
}

/// GitHub's answer to removing this Actions runner's registration, by the truth, and what it changes there.
fn remove(model: &mut Model, runner: &str) -> Answer {
    let truth = model.runners.entry(runner.to_string()).or_default();
    match (truth.registered, truth.job) {
        (true, Some(false)) => Answer::Refused,
        (true, _) => {
            truth.registered = false;
            Answer::Removed
        }
        (false, _) => Answer::Gone,
    }
}

fn sweep(w: &mut World, model: &mut Model, crash: &CrashingStore, seed: u8) {
    for (index, registration) in w.m.registrations_to_remove().into_iter().enumerate() {
        let reply = REPLIES[(usize::from(seed) + index) % REPLIES.len()];
        let runner = registration.runner;
        if matches!(reply, Reply::ServerError | Reply::Timeout) {
            continue;
        }
        let answer = remove(model, &runner);
        if answer == Answer::Removed {
            w.registered.remove(&runner);
        }
        if reply == Reply::Lost {
            if answer == Answer::Removed {
                model.runners.get_mut(&runner).expect("known").unknown_204 = true;
            }
            continue;
        }
        let crashed_before = crash.crashed();
        w.m.answered(&runner, answer);
        // The daemon died as it wrote the answer down: GitHub's change stands, the Runner's word of it may not.
        if !crashed_before && crash.crashed() && answer == Answer::Removed {
            model.runners.get_mut(&runner).expect("known").unknown_204 = true;
        }
    }
}

/// Writes a record as an older Runner would have left the same state.
fn downgrade(mut record: Record, format: Format) -> Record {
    let Some(slot) = record.slot else { return record };
    let mut saved = slot.saved();
    saved.up = false;
    saved.completed = false;
    if saved.phase == Phase::Asking {
        saved.phase = Phase::Idle;
        saved.end = (format == Format::Rules1).then_some(End::NoJob);
    }
    record.slot = Some(saved.into());
    if let Some(github) = record.github.as_mut() {
        github.inferred = false;
    }
    record.rules = match format {
        Format::Main => 0,
        Format::Rules1 => 1,
    };
    record
}

async fn apply(w: &mut World, model: &mut Model, crash: &CrashingStore, op: Op) {
    let names: Vec<String> = model.runners.keys().cloned().collect();
    match op {
        Op::Assign => {
            let ticket = match w.m.claim(SIZE, None) {
                Claim::Start(ticket) => ticket,
                // No room now: the job goes elsewhere, and its place in line is given back.
                Claim::Waits { ticket, .. } => return w.m.withdraw(ticket),
                Claim::Never(_) => return,
            };
            let runner = w.register();
            // A daemon that died writing its intent never asked GitHub.
            if crash.crashed() {
                w.registered.remove(&runner);
            } else {
                model.runners.insert(
                    runner.clone(),
                    Truth {
                        registered: true,
                        ..Truth::default()
                    },
                );
                let _ = w.start_registered(&runner, ticket, SIZE).await;
            }
        }
        Op::Boots(i) => {
            let booting: Vec<String> = names
                .iter()
                .filter(|r| w.parts.vms.booting().iter().any(|job| job == job_name(r)))
                .cloned()
                .collect();
            if let Some(runner) = pick(&booting, i) {
                w.parts.vms.ready(job_name(&runner));
            }
        }
        Op::JobStarts(i, heard) => {
            // Only an Actions runner that runs and is registered takes a job.
            let free: Vec<String> = names
                .iter()
                .filter(|r| {
                    let truth = &model.runners[*r];
                    truth.registered && truth.job.is_none() && w.up(r)
                })
                .cloned()
                .collect();
            if let Some(runner) = pick(&free, i) {
                let at = stops(w, &runner);
                let truth = model.runners.get_mut(&runner).expect("known");
                truth.job = Some(false);
                truth.stops_at_start = at;
                if heard {
                    w.github(EventKind::Started, &runner, "");
                }
            }
        }
        Op::JobCompletes(i, heard) => {
            let running: Vec<String> = names
                .iter()
                .filter(|r| model.runners[*r].job == Some(false))
                .cloned()
                .collect();
            if let Some(runner) = pick(&running, i) {
                let truth = model.runners.get_mut(&runner).expect("known");
                truth.job = Some(true);
                truth.registered = false;
                w.registered.remove(&runner);
                if heard {
                    w.github(EventKind::Completed, &runner, "succeeded");
                }
                w.parts.vms.end(job_name(&runner), cf_vmhost::End::Ok);
            }
        }
        Op::Again(i) => {
            let ran: Vec<String> = names
                .iter()
                .filter(|r| model.runners[*r].job.is_some())
                .cloned()
                .collect();
            if let Some(runner) = pick(&ran, i) {
                w.github(EventKind::Started, &runner, "");
                if model.runners[&runner].job == Some(true) {
                    w.github(EventKind::Completed, &runner, "succeeded");
                }
            }
        }
        Op::Sweep(seed) => sweep(w, model, crash, seed),
        Op::Busy(i) => {
            let busy: Vec<String> = names
                .iter()
                .filter(|r| model.runners[*r].registered && model.runners[*r].job == Some(false))
                .cloned()
                .collect();
            if let Some(runner) = pick(&busy, i) {
                w.m.answered(&runner, Answer::Busy);
            }
        }
        Op::Retire(n) => {
            let n = usize::from(n % 3) + 1;
            w.m.retire(REPO, SET, n, &names);
        }
        Op::Tick(i) => pass(&w.m, [1, 5, 30, 120][usize::from(i) % 4]),
        Op::CrashAt(n, kept) => {
            // A dead daemon's hands are gone with it: it starts and stops nothing more.
            let vms = w.parts.vms.clone();
            crash.crash_at(usize::from(n), kept, move || vms.freeze(true));
        }
        Op::Restart => w.restart(),
        Op::Downgrade(i, format) => {
            let records = w.parts.store.load();
            if let Some(record) = pick(&records, i) {
                if format == Format::Main && record.github.is_none() && record.unregistered {
                    // main's `unregistered` may be a belief: it is dropped, and the next removal meets 404.
                    if let Some(truth) = model.runners.get_mut(&record.runner) {
                        truth.unknown_204 = true;
                    }
                }
                crash.plant(&downgrade(record, format));
                w.restart();
            }
        }
    }
    if crash.crashed() {
        crash.revive();
        w.parts.vms.freeze(false);
        w.restart();
    }
}

/// What must hold after every step.
fn check(w: &World, model: &Model, crash: &CrashingStore) -> Result<(), TestCaseError> {
    let disagreements = crash.disagreements();
    prop_assert!(
        disagreements.is_empty(),
        "a write left facts that disagree: {disagreements:?}"
    );
    for record in w.parts.store.load() {
        prop_assert_eq!(
            record.clone().upgrade(),
            Ok((record.clone(), Upgrade::Current)),
            "a record of these rules read again changed"
        );
    }
    for (runner, truth) in &model.runners {
        if truth.job == Some(false) {
            prop_assert_eq!(
                stops(w, runner),
                truth.stops_at_start,
                "{} was stopped while its job ran",
                runner
            );
        }
    }
    Ok(())
}

/// The world comes to rest: every job ends, GitHub answers truly, time passes.
fn rest(w: &mut World, model: &mut Model, crash: &CrashingStore) {
    crash.revive();
    for (runner, truth) in model.runners.iter_mut() {
        if truth.job == Some(false) {
            truth.job = Some(true);
            truth.registered = false;
            w.registered.remove(runner);
            w.github(EventKind::Completed, runner, "failed");
            w.parts.vms.end(job_name(runner), cf_vmhost::End::Ok);
        }
    }
    for _ in 0..40 {
        let left = !w.parts.vms.standing().is_empty()
            || !w.registered.is_empty()
            || !w.parts.store.load().is_empty()
            || !w.m.snapshot().budget.slots.is_empty();
        if !left {
            break;
        }
        pass(&w.m, 30);
        sweep(w, model, crash, 0);
    }
    pass(&w.m, RESULT_WAIT.as_secs() + 5);
    sweep(w, model, crash, 0);
    pass(&w.m, 2);
}

fn run(ops: &[Op]) -> Result<(), TestCaseError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let crash = Arc::new(CrashingStore::default());
        let mut w = World::keeping(settings_for(3), crash.clone());
        let mut model = Model::default();
        for op in ops {
            apply(&mut w, &mut model, &crash, *op).await;
            check(&w, &model, &crash)?;
        }
        rest(&mut w, &mut model, &crash);
        check(&w, &model, &crash)?;

        prop_assert_eq!(w.parts.vms.standing(), Vec::<String>::new(), "a machine stands");
        prop_assert_eq!(w.m.snapshot().budget.slots.len(), 0, "a slot is held");
        prop_assert_eq!(
            &w.registered,
            &BTreeSet::new(),
            "an Actions runner is left registered"
        );
        prop_assert_eq!(w.parts.store.load(), vec![], "records are left");

        let reported: BTreeSet<String> = w.reports().into_iter().map(|r| r.id).collect();
        for (runner, truth) in &model.runners {
            let both = BTreeSet::from([format!("{runner}/completed"), format!("{runner}/started")]);
            let mine: BTreeSet<String> = reported
                .iter()
                .filter(|id| id.starts_with(&format!("{runner}/")))
                .cloned()
                .collect();
            if truth.job.is_some() {
                prop_assert_eq!(
                    &mine,
                    &both,
                    "the job of {} is reported started and ended",
                    runner
                );
            } else if truth.unknown_204 {
                prop_assert!(
                    mine.is_empty() || mine == both,
                    "half a job reported for {runner}: {mine:?}"
                );
            } else {
                prop_assert!(
                    mine.is_empty(),
                    "a job reported that did not run on {runner}: {mine:?}"
                );
            }
        }
        Ok(())
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, max_shrink_iters: 4000, ..ProptestConfig::default() })]

    #[test]
    fn no_schedule_stops_a_running_job_loses_one_or_leaves_anything(ops in proptest::collection::vec(op(), 1..80)) {
        run(&ops)?;
    }
}

/// The schedules of earlier findings, and those this test found, kept as they are.
#[test]
fn pinned_schedules() {
    use Op::*;
    let pinned: [(&str, &[Op]); 6] = [
        (
            "a job starts unheard while its removal is asked: GitHub refuses, the job runs on",
            &[
                Assign,
                Boots(0),
                Retire(0),
                JobStarts(0, false),
                Sweep(0),
                Tick(3),
                Tick(3),
            ],
        ),
        (
            "a job starts and ends unheard: the 404 under a machine that came up is that job",
            &[
                Assign,
                Boots(0),
                JobStarts(0, false),
                JobCompletes(0, false),
                Retire(0),
                Sweep(0),
            ],
        ),
        (
            "the daemon dies as it writes a job's start",
            &[
                Assign,
                Boots(0),
                CrashAt(1, false),
                JobStarts(0, true),
                Tick(3),
                Tick(3),
            ],
        ),
        (
            "a removal answered with 204 while the daemon dies at that write",
            &[Assign, Boots(0), Retire(0), CrashAt(1, false), Sweep(0)],
        ),
        (
            "an idle machine rules 1 wrote as asking, whose job starts unheard",
            &[
                Assign,
                Boots(0),
                Retire(0),
                Downgrade(0, Format::Rules1),
                JobStarts(0, false),
                Sweep(0),
            ],
        ),
        (
            "main's record of a standing machine whose job only GitHub's REST view named",
            &[
                Assign,
                Boots(0),
                JobStarts(0, false),
                Busy(0),
                Downgrade(0, Format::Main),
                Sweep(0),
            ],
        ),
    ];
    for (name, ops) in pinned {
        if let Err(failure) = run(ops) {
            panic!("{name}: {failure}");
        }
    }
}
