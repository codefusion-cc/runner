//! Room made for a larger size: a Mac that keeps running smaller jobs would otherwise never have the room a larger
//! one needs, and its jobs would wait at GitHub for good.

mod common;

use cf_budget::{Assigned, Budget, GIB, Grant, Refusal, Size, SlotId};
use common::*;

fn waits(assigned: Assigned) -> (SlotId, Refusal) {
    match assigned {
        Assigned::Waits { slot, why } => (slot, why),
        other => panic!("expected a job that waits, got {other:?}"),
    }
}

fn starts(assigned: Assigned) -> Grant {
    match assigned {
        Assigned::Start(grant) => grant,
        other => panic!("expected a job that starts, got {other:?}"),
    }
}

/// The sizes of the jobs that start at `second`, with `available` bytes on the Mac. Readings come once a second from
/// second 60 on: a gap would make the budget calm down first.
fn started_with(budget: &mut Budget, second: u64, available: u64) -> Vec<Size> {
    watch(budget, second, second, |r| r.available_bytes = available)
        .into_iter()
        .map(|(_, grant)| grant.size)
        .collect()
}

#[test]
fn while_room_is_made_for_a_size_no_other_size_is_granted_and_that_one_still_is() {
    let mut budget = open(dedicated());
    starts(budget.assign(secs(61), Size::G4));
    budget.make_room(Some(Size::G6));
    let making = Err(Refusal::MakingRoom { size: Size::G6 });
    for size in [Size::G2, Size::G4, Size::G8] {
        assert_eq!(budget.fits(secs(61), size), making, "{size:?}");
        assert_eq!(budget.claim(secs(61), size).map(|_| ()), making, "{size:?}");
        assert_eq!(budget.free(secs(61)).of(size), 0, "{size:?}");
    }
    assert_eq!(budget.fits(secs(61), Size::G6), Ok(()));
    assert_eq!(budget.free(secs(61)).of(Size::G6), 1);
    assert_eq!(Refusal::MakingRoom { size: Size::G6 }.name(), "making-room");

    budget.make_room(None);
    assert_eq!(budget.fits(secs(61), Size::G2), Ok(()));
    assert!(budget.claim(secs(61), Size::G2).is_ok());
}

#[test]
fn a_job_of_another_size_that_is_assigned_meanwhile_waits_and_starts_once_room_is_no_longer_made() {
    let mut budget = open(dedicated());
    budget.make_room(Some(Size::G6));
    let (_, why) = waits(budget.assign(secs(61), Size::G2));
    assert_eq!(why, Refusal::MakingRoom { size: Size::G6 });
    assert_eq!(started_with(&mut budget, 61, 16 * GIB), []);
    budget.make_room(None);
    assert_eq!(started_with(&mut budget, 62, 16 * GIB), [Size::G2]);
}

/// Two cf-4g machines take every vCPU of the dedicated Mac. A cf-2g job is assigned and waits, then a cf-6g one;
/// then one cf-4g machine ends.
fn two_running_then_a_small_and_a_large_job_wait(make_room: bool) -> (Budget, Grant) {
    let mut budget = open(dedicated());
    let first = starts(budget.assign(secs(61), Size::G4));
    starts(budget.assign(secs(61), Size::G4));
    waits(budget.assign(secs(62), Size::G2));
    waits(budget.assign(secs(63), Size::G6));
    if make_room {
        budget.make_room(Some(Size::G6));
    }
    (budget, first)
}

#[test]
fn the_size_room_is_made_for_starts_before_a_smaller_job_that_came_first() {
    let (mut budget, first) = two_running_then_a_small_and_a_large_job_wait(true);
    budget.release(first.slot).expect("released");
    // 4 of 8 vCPUs are free: the cf-6g machine takes them, and the cf-2g one finds none left.
    assert_eq!(started_with(&mut budget, 64, 16 * GIB), [Size::G6]);
    assert_eq!(budget.waiting(), 1);

    // Without room made, the job that came first takes the vCPUs and the larger one waits on.
    let (mut budget, first) = two_running_then_a_small_and_a_large_job_wait(false);
    budget.release(first.slot).expect("released");
    assert_eq!(started_with(&mut budget, 64, 16 * GIB), [Size::G2]);
}

#[test]
fn a_smaller_job_that_would_fit_is_held_back_until_the_larger_one_fits() {
    // One cf-4g machine runs with all of it still to come: a cf-2g one needs 8.375 GiB available beside it, a cf-6g
    // one 12.375 GiB. With 10 GiB the small one would fit and the large one not.
    let (mut budget, first) = two_running_then_a_small_and_a_large_job_wait(true);
    budget.release(first.slot).expect("released");
    assert_eq!(started_with(&mut budget, 64, 10 * GIB), []);
    assert_eq!(budget.waiting(), 2);

    let (mut budget, first) = two_running_then_a_small_and_a_large_job_wait(false);
    budget.release(first.slot).expect("released");
    assert_eq!(started_with(&mut budget, 64, 10 * GIB), [Size::G2]);
}
