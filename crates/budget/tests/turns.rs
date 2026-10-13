//! Who goes first among the assigned jobs that wait for room: jobs of the main branch and deploys, repositories in
//! turn, and none passed over for longer than the defer limit.

mod common;

use cf_budget::{Assigned, Budget, DEFER_LIMIT, Millis, Size, SlotId, Turn};
use common::*;
use proptest::prelude::*;

fn pull(repo: &str) -> Turn {
    Turn {
        urgent: false,
        repo: repo.into(),
    }
}

fn main_of(repo: &str) -> Turn {
    Turn {
        urgent: true,
        repo: repo.into(),
    }
}

/// A Mac that runs one 8 GB machine at a time, with one running from second 60: its slot.
fn full() -> (Budget, SlotId) {
    let mut budget = open(dedicated());
    let Assigned::Start(grant) = budget.assign_in_turn(secs(60), Size::G8, pull("acme/first")) else {
        panic!("an open Mac starts the first job");
    };
    (budget, grant.slot)
}

/// A job assigned at `at` seconds that has to wait.
fn queue(budget: &mut Budget, at: u64, turn: Turn) -> SlotId {
    match budget.assign_in_turn(secs(at), Size::G8, turn) {
        Assigned::Waits { slot, .. } => slot,
        other => panic!("expected a job that waits, got {other:?}"),
    }
}

/// From second `at` on, each machine ends as soon as it ran a second, until nothing waits: the jobs in the order
/// they started.
fn drain(budget: &mut Budget, mut running: SlotId, mut at: u64) -> Vec<SlotId> {
    let mut order = vec![];
    while budget.waiting() > 0 {
        budget.release(running).unwrap();
        let started = watch(budget, at, at, |_| {});
        assert_eq!(started.len(), 1, "one machine at a time, at second {at}");
        running = started[0].1.slot;
        order.push(running);
        at += 1;
    }
    order
}

#[test]
fn a_main_job_behind_five_pull_request_jobs_starts_first() {
    let (mut budget, running) = full();
    let pulls: Vec<SlotId> = (0..5)
        .map(|_| queue(&mut budget, 60, pull("acme/shop")))
        .collect();
    let main = queue(&mut budget, 60, main_of("acme/shop"));
    let mut expected = vec![main];
    expected.extend(&pulls);
    assert_eq!(
        budget.line(secs(60)),
        expected,
        "the line says so before it happens"
    );
    assert_eq!(drain(&mut budget, running, 61), expected);
}

#[test]
fn a_job_passed_over_for_the_defer_limit_beats_a_fresh_main_job() {
    let limit = DEFER_LIMIT.0 / 1000;
    let (mut budget, running) = full();
    let old = queue(&mut budget, 60, pull("acme/shop"));
    // Seconds before the limit, the main job still goes first.
    watch(&mut budget, 61, 60 + limit - 1, |_| {});
    let early = queue(&mut budget, 60 + limit - 1, main_of("acme/other"));
    assert_eq!(budget.line(secs(60 + limit - 1)), [early, old]);
    // At the limit, the old one goes first, and a main job that comes now goes after it.
    watch(&mut budget, 60 + limit, 60 + limit, |_| {});
    let fresh = queue(&mut budget, 60 + limit, main_of("acme/other"));
    assert_eq!(budget.line(secs(60 + limit)), [old, early, fresh]);
    assert_eq!(drain(&mut budget, running, 61 + limit), [old, early, fresh]);
}

#[test]
fn two_repositories_take_turns() {
    let (mut budget, running) = full();
    let a: Vec<SlotId> = (0..3).map(|_| queue(&mut budget, 60, pull("acme/a"))).collect();
    let b: Vec<SlotId> = (0..3).map(|_| queue(&mut budget, 60, pull("acme/b"))).collect();
    assert_eq!(
        drain(&mut budget, running, 61),
        [a[0], b[0], a[1], b[1], a[2], b[2]]
    );
    // The same among main jobs, and a repository that just started a job goes after one that did not.
    let (mut budget, running) = full();
    let first = queue(&mut budget, 60, main_of("acme/first"));
    let other = queue(&mut budget, 60, main_of("acme/other"));
    assert_eq!(drain(&mut budget, running, 61), [other, first]);
}

#[test]
fn a_job_that_started_on_its_reservation_counts_for_its_repositorys_turn() {
    let mut budget = open(dedicated());
    let grant = budget.claim(secs(60), Size::G8).expect("an open Mac reserves");
    budget.confirm_in_turn(grant.slot, &pull("acme/busy")).unwrap();
    let busy = queue(&mut budget, 60, pull("acme/busy"));
    let quiet = queue(&mut budget, 60, pull("acme/quiet"));
    assert_eq!(budget.line(secs(60)), [quiet, busy]);
    assert_eq!(drain(&mut budget, grant.slot, 61), [quiet, busy]);
}

#[test]
fn an_empty_line_starts_nothing_and_holds_nobody_up() {
    let mut budget = open(dedicated());
    assert_eq!(budget.line(secs(60)), []);
    assert_eq!(watch(&mut budget, 61, 70, |_| {}), []);
    assert_eq!(budget.status(secs(70)).waiting, []);
    assert!(budget.claim(secs(70), Size::G2).is_ok());
    // A job that went away leaves the line empty again.
    let (mut budget, running) = full();
    let gone = queue(&mut budget, 60, main_of("acme/shop"));
    assert_eq!(budget.withdraw(gone), Ok(Size::G8));
    assert_eq!(
        budget.withdraw(gone).map_err(|e| e.0),
        Err(gone),
        "it is not there twice"
    );
    assert_eq!(budget.line(secs(60)), []);
    budget.release(running).unwrap();
    assert_eq!(watch(&mut budget, 61, 61, |_| {}), []);
    assert!(budget.claim(secs(61), Size::G8).is_ok());
}

#[test]
fn the_status_names_the_waiting_jobs_in_the_order_they_start() {
    let (mut budget, _) = full();
    let pull_job = queue(&mut budget, 60, pull("acme/shop"));
    let main = queue(&mut budget, 60, main_of("acme/shop"));
    let waiting = budget.status(secs(60)).waiting;
    assert_eq!(
        waiting.iter().map(|w| w.slot).collect::<Vec<_>>(),
        [main, pull_job]
    );
    assert_eq!(
        waiting[1].why,
        Some(cf_budget::Refusal::AssignedFirst { waiting: 1 })
    );
}

#[derive(Clone, Debug)]
struct Arrival {
    gap: u64,
    run: u64,
    urgent: bool,
    repo: u8,
}

fn arrivals() -> impl Strategy<Value = Vec<Arrival>> {
    prop::collection::vec(
        (0u64..90, 1u64..200, any::<bool>(), 0u8..3).prop_map(|(gap, run, urgent, repo)| Arrival {
            gap,
            run,
            urgent,
            repo,
        }),
        1..16,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// On a Mac that runs one job at a time, whatever comes: every job starts; none starts while a job that waited
    /// the defer limit and came before it still waits; none that is not urgent starts while an urgent one waits
    /// that has not waited the limit, unless an overdue one goes; and no urgent job passes an overdue one.
    #[test]
    fn nobody_waits_for_ever_and_main_jobs_go_first(jobs in arrivals()) {
        let limit = DEFER_LIMIT;
        let mut budget = open(dedicated());
        // (slot, arrived at, urgent, run time)
        let mut waiting: Vec<(SlotId, Millis, bool, u64)> = vec![];
        let mut running: Option<(SlotId, u64)> = None;
        let mut next = 0;
        let mut at = 61u64;
        let mut arrive_at = 61 + jobs[0].gap;
        let mut started = 0;
        while started < jobs.len() {
            prop_assert!(at < 61 + 100_000, "the line never drained");
            if let Some((slot, until)) = running
                && at >= until
            {
                budget.release(slot).unwrap();
                running = None;
            }
            while next < jobs.len() && arrive_at <= at {
                let job = &jobs[next];
                let turn = Turn { urgent: job.urgent, repo: format!("acme/{}", job.repo) };
                match budget.assign_in_turn(secs(at), Size::G8, turn) {
                    Assigned::Start(grant) => {
                        prop_assert!(running.is_none() && waiting.is_empty());
                        running = Some((grant.slot, at + job.run));
                        started += 1;
                    }
                    Assigned::Waits { slot, .. } => waiting.push((slot, Millis::secs(at), job.urgent, job.run)),
                    Assigned::Never(why) => prop_assert!(false, "an 8 GB job fits this Mac: {why:?}"),
                }
                next += 1;
                if let Some(job) = jobs.get(next) {
                    arrive_at += job.gap;
                }
            }
            let now = secs(at);
            for grant in budget.observe(now, Some(calm(now))).started {
                let index = waiting.iter().position(|w| w.0 == grant.slot).expect("a job that waited");
                let (slot, arrived, urgent, run) = waiting.remove(index);
                let overdue = |since: Millis| now.elapsed.since(since) >= limit;
                for &(_, other_arrived, other_urgent, _) in &waiting {
                    if overdue(other_arrived) {
                        prop_assert!(
                            overdue(arrived) && arrived <= other_arrived,
                            "{slot:?} started while a job overdue since before it waited"
                        );
                    } else if other_urgent && !urgent {
                        prop_assert!(overdue(arrived), "{slot:?} passed an urgent job");
                    }
                }
                prop_assert!(running.is_none());
                running = Some((slot, at + run));
                started += 1;
            }
            at += 1;
        }
        prop_assert!(waiting.is_empty());
    }
}
