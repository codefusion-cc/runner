//! Room made for a larger size, on the mini of 2026-10-13: a cf-6g job waited for hours while cf-4g and cf-2g jobs
//! took every slot that freed, and GitHub took it back every five minutes. Every number here is the mini's: with
//! one cf-4g machine holding 4109 MiB, its other programs left 5 GiB available, 9229 MiB without the machine. That
//! carries a cf-2g machine beside the cf-4g one, a cf-6g one only on its own (and never beside it: 7 vCPUs of 6), and
//! never a cf-8g one.

mod support;

use cf_budget::{GIB, Size};
use cf_daemon::names::job_name;
use cf_daemon::testing::pass;
use cf_daemon::{Claim, EventKind, Slots, Ticket};
use cf_vmhost::End;
use support::Log;
use support::world::{MINI_HELD, MINI_PEAK, World, mini, mini_with_one};

const HELD: u64 = MINI_HELD;
const AVAILABLE: u64 = 5 * GIB;

/// The mini with a cf-4g machine running.
async fn mini_with_a_cf4g() -> (World, String) {
    let (w, runner) = mini_with_one(MINI_PEAK, Some(HELD)).await;
    w.m.with_host(|host| host.set(|r| r.available_bytes = AVAILABLE));
    pass(&w.m, 1);
    (w, runner)
}

/// The cf-4g machine's job ends, and the Mac gets its memory back.
fn ends(w: &World, runner: &str) {
    w.parts.vms.end(job_name(runner), End::Ok);
    w.github(EventKind::Completed, runner, "succeeded");
    w.m.with_host(|host| host.set(|r| r.available_bytes = AVAILABLE + HELD));
    pass(&w.m, 2);
}

fn waits(claim: Claim) -> (Ticket, String) {
    match claim {
        Claim::Waits { ticket, why } => (ticket, why),
        other => panic!("expected a job that waits, got {other:?}"),
    }
}

/// A scale set of this size polls once a second for this many seconds: what it offered GitHub at each.
fn polls(w: &World, size: Size, seconds: u64) -> Vec<usize> {
    (0..seconds)
        .map(|_| {
            pass(&w.m, 1);
            w.m.open_slots(size)
        })
        .collect()
}

#[tokio::test]
async fn a_cf6g_that_waits_holds_back_the_cf2g_jobs_that_keep_coming_for_the_second_slot() {
    let log = Log::start();
    let (w, cf4g) = mini_with_a_cf4g().await;
    assert_eq!(
        w.m.open_slots(Size::G2),
        1,
        "a cf-2g machine fits beside the cf-4g one"
    );

    let (cf6g, why) = waits(w.m.claim(Size::G6, None));
    assert_eq!(why, "cpus");
    pass(&w.m, 1);
    assert_eq!(
        log.lines("smaller ones keep taking"),
        [
            "a job of this size waits for room the smaller ones keep taking: none of them starts until it does size=\"cf-6g\" limit_min=30"
        ]
    );
    // For the next quarter of an hour cf-2g jobs keep coming: none gets the second slot, and no poll offers it.
    let mut held_back = Vec::new();
    for _ in 0..15 {
        assert_eq!(w.m.reserve(Size::G2).map_err(|e| e.0), Err("making-room".into()));
        let (ticket, why) = waits(w.m.claim(Size::G2, None));
        assert_eq!(why, "making-room");
        held_back.push(ticket);
        assert!(polls(&w, Size::G2, 60).iter().all(|offered| *offered == 0));
        assert!(!held_back.iter().any(|ticket| w.m.ready(*ticket)));
    }
    assert!(!w.m.ready(cf6g));
    assert_eq!(w.m.snapshot().room(Size::G2).why, Some("making-room"));

    // The cf-4g job ends: the cf-6g one starts. No cf-2g machine fits beside it on what the Mac has left, so they
    // wait for room as any job does, no longer for room made.
    ends(&w, &cf4g);
    assert!(w.m.ready(cf6g), "the cf-6g job got the room");
    assert_eq!(
        log.lines("the room is made"),
        ["the room is made: a machine of this size starts, and the others may again size=\"cf-6g\""]
    );
    assert!(!held_back.iter().any(|ticket| w.m.ready(*ticket)));
    assert_ne!(w.m.snapshot().room(Size::G2).why, Some("making-room"));
}

#[tokio::test]
async fn without_a_larger_job_waiting_the_cf2g_jobs_take_the_second_slot() {
    let (w, _) = mini_with_a_cf4g().await;
    assert!(matches!(w.m.claim(Size::G2, None), Claim::Start(_)));
}

#[tokio::test]
async fn a_size_whose_polls_found_no_room_for_ten_minutes_is_offered_a_place_in_line_and_a_size_the_mac_never_runs_is_not()
 {
    let log = Log::start();
    let (w, _) = mini_with_a_cf4g().await;
    // The first poll that found no room was at second 1: the one ten minutes later offers a place.
    let offered = polls(&w, Size::G6, 601);
    assert!(offered[..600].iter().all(|offered| *offered == 0), "{offered:?}");
    assert_eq!(offered[600], 1, "ten minutes without room");
    assert!(
        w.m.offers_place(Size::G6),
        "what it offers is a place in line, no room"
    );
    assert_eq!(
        log.lines("a place in line"),
        [
            "no machine of this size fits now, but a job of it gets a place in line: the smaller ones wait for it size=\"cf-6g\""
        ]
    );
    // It is offered on, and the log said it once; the cf-2g jobs still start while no cf-6g one is assigned.
    assert!(polls(&w, Size::G6, 120).iter().all(|offered| *offered == 1));
    assert_eq!(log.lines("a place in line").len(), 1);
    assert_eq!(w.m.open_slots(Size::G2), 1);
    assert!(!w.m.offers_place(Size::G2), "a cf-2g machine fits: that is room");

    // A cf-8g machine needs 10432 MiB without the Mac's own machines; it has 9229.
    assert!(polls(&w, Size::G8, 900).iter().all(|offered| *offered == 0));

    // A paused Mac offers no place.
    w.m.pause(true);
    assert_eq!(polls(&w, Size::G6, 5), [0; 5]);
    w.m.pause(false);
}

#[tokio::test]
async fn a_place_in_line_rings_the_bells_once_and_not_at_every_look() {
    let (w, _) = mini_with_a_cf4g().await;
    polls(&w, Size::G6, 599);
    let before = w.nudges();
    pass(&w.m, 10);
    assert_eq!(w.nudges(), before + 1, "the polls that offer none are told once");
    assert_eq!(w.m.open_slots(Size::G6), 1);
    polls(&w, Size::G6, 120);
    assert_eq!(w.nudges(), before + 1);
}

#[tokio::test]
async fn a_job_github_takes_back_keeps_its_place_and_the_room_is_given_up_when_it_does_not_come_again() {
    let log = Log::start();
    let (w, _) = mini_with_a_cf4g().await;
    let (first, _) = waits(w.m.claim(Size::G6, None));
    pass(&w.m, 1);
    assert_eq!(
        w.m.open_slots(Size::G6),
        0,
        "its job waits in line: no second is asked for"
    );

    // Five minutes on, GitHub takes the job back: the scale set offers its place, and the cf-2g jobs wait on.
    pass(&w.m, 300);
    w.m.withdraw(first);
    pass(&w.m, 1);
    assert_eq!(w.m.open_slots(Size::G6), 1);
    let (cf2g, why) = waits(w.m.claim(Size::G2, None));
    assert_eq!(why, "making-room");
    // GitHub hands it here again within the minute.
    pass(&w.m, 50);
    let (again, _) = waits(w.m.claim(Size::G6, None));
    pass(&w.m, 1);
    assert_eq!(w.m.open_slots(Size::G6), 0);

    // Taken back once more, and this time it does not come again: after two minutes the others start.
    pass(&w.m, 300);
    w.m.withdraw(again);
    assert!(polls(&w, Size::G6, 119).iter().all(|offered| *offered == 1));
    assert_eq!(log.lines("did not hand it here again"), Vec::<String>::new());
    pass(&w.m, 1);
    assert_eq!(
        log.lines("did not hand it here again"),
        [
            "no more room is made for this size: the others start again size=\"cf-6g\" why=\"GitHub took the job back and did not hand it here again\""
        ]
    );
    assert!(w.m.ready(cf2g), "the cf-2g job that waited starts");
}

#[tokio::test]
async fn room_is_made_for_half_an_hour_at_most_and_then_for_none_for_as_long() {
    let log = Log::start();
    let (w, _) = mini_with_a_cf4g().await;
    // GitHub takes the job back every five minutes and hands it here again at once.
    let (mut cf6g, _) = waits(w.m.claim(Size::G6, None));
    for _ in 0..5 {
        pass(&w.m, 300);
        w.m.withdraw(cf6g);
        pass(&w.m, 1);
        assert_eq!(w.m.open_slots(Size::G6), 1);
        let rung = w.nudges();
        cf6g = waits(w.m.claim(Size::G6, None)).0;
        pass(&w.m, 1);
        assert_eq!(
            w.nudges(),
            rung + 1,
            "the polls that offer its place hear that it is taken"
        );
    }
    pass(&w.m, 30 * 60 - 5 * 302 - 1);
    assert_eq!(w.m.snapshot().room(Size::G2).why, Some("making-room"));
    pass(&w.m, 1);
    assert_eq!(
        log.lines("half an hour"),
        [
            "no more room is made for this size: the others start again size=\"cf-6g\" why=\"room was made for half an hour and is still not there\""
        ]
    );
    // The job keeps its place until GitHub takes it back once more; then its scale set offers none, and the cf-2g
    // jobs start, for half an hour.
    pass(&w.m, 299);
    w.m.withdraw(cf6g);
    pass(&w.m, 1);
    assert!(matches!(w.m.claim(Size::G2, None), Claim::Start(_)));
    let offered = polls(&w, Size::G6, 30 * 60 - 301);
    assert!(offered.iter().all(|offered| *offered == 0));
    // Then a place is offered again.
    assert_eq!(polls(&w, Size::G6, 5), [1; 5]);
}

#[tokio::test]
async fn an_empty_mac_whose_other_programs_hold_the_room_stops_making_it() {
    let log = Log::start();
    let (w, cf4g) = mini_with_a_cf4g().await;
    let (cf6g, _) = waits(w.m.claim(Size::G6, None));
    // The cf-4g job ends, but what else runs on the Mac takes the memory it gave back.
    w.parts.vms.end(job_name(&cf4g), End::Ok);
    w.github(EventKind::Completed, &cf4g, "succeeded");
    pass(&w.m, 2);
    assert!(!w.m.ready(cf6g));
    pass(&w.m, 118);
    assert_eq!(log.lines("holds the memory"), Vec::<String>::new());
    pass(&w.m, 2);
    assert_eq!(log.lines("holds the memory").len(), 1);
    // Once GitHub takes the job back, its scale set offers no place, and the cf-2g jobs start.
    w.m.withdraw(cf6g);
    pass(&w.m, 1);
    assert_eq!(w.m.open_slots(Size::G6), 0);
    assert!(matches!(w.m.claim(Size::G2, None), Claim::Start(_)));
}

#[tokio::test]
async fn no_room_is_made_for_a_size_the_mac_already_runs_or_while_it_takes_no_jobs() {
    let log = Log::start();
    let (w, _) = mini_with_a_cf4g().await;
    // A second cf-4g job waits for the memory the first holds: the room comes back when the first ends.
    let (second, why) = waits(w.m.claim(Size::G4, None));
    assert_eq!(why, "memory");
    pass(&w.m, 1);
    assert_ne!(w.m.snapshot().room(Size::G2).why, Some("making-room"));
    w.m.withdraw(second);

    // A paused Mac makes no room: what keeps the job out is the pause.
    w.m.pause(true);
    let (cf6g, why) = waits(w.m.claim(Size::G6, None));
    assert_eq!(why, "paused");
    pass(&w.m, 5);
    w.m.withdraw(cf6g);
    w.m.pause(false);
    pass(&w.m, 1);
    assert_eq!(log.lines("smaller ones keep taking"), Vec::<String>::new());
    assert!(matches!(w.m.claim(Size::G2, None), Claim::Start(_)));
}

#[tokio::test]
async fn a_job_handed_here_again_once_the_room_is_there_ends_the_making_at_once() {
    for promised in [false, true] {
        let (w, cf4g) = mini_with_a_cf4g().await;
        let (cf6g, _) = waits(w.m.claim(Size::G6, None));
        pass(&w.m, 300);
        w.m.withdraw(cf6g);
        ends(&w, &cf4g);
        // GitHub hands the job here again, or the scale set promises it the slot first: either way it starts, and
        // the next cf-2g job is not held back.
        if promised {
            assert!(w.m.reserve(Size::G6).is_ok());
        } else {
            assert!(matches!(w.m.claim(Size::G6, None), Claim::Start(_)));
        }
        assert_ne!(
            w.m.snapshot().room(Size::G2).why,
            Some("making-room"),
            "promised: {promised}"
        );
    }
}

#[tokio::test]
async fn a_job_behind_one_a_hold_keeps_out_has_no_room_made_for_it() {
    let log = Log::start();
    let (w, _) = mini_with_a_cf4g().await;
    w.m.pause(true);
    let (_, why) = waits(w.m.claim(Size::G2, None));
    assert_eq!(why, "paused");
    // The cf-6g job waits behind the cf-2g one, which waits for the pause, not for room.
    let (_, why) = waits(w.m.claim(Size::G6, None));
    assert_eq!(why, "assigned-first");
    pass(&w.m, 2);
    assert_eq!(log.lines("smaller ones keep taking"), Vec::<String>::new());
}

#[tokio::test]
async fn room_made_for_a_size_the_settings_then_rule_out_is_given_up_at_once() {
    let log = Log::start();
    let (w, _) = mini_with_a_cf4g().await;
    let _ = waits(w.m.claim(Size::G6, None));
    pass(&w.m, 1);
    assert_eq!(w.m.snapshot().room(Size::G2).why, Some("making-room"));
    let mut settings = mini();
    settings.away.cap_bytes = 5 * GIB;
    settings.in_use.cap_bytes = settings.in_use.cap_bytes.min(5 * GIB);
    w.m.set_settings(settings).expect("settings");
    pass(&w.m, 1);
    assert_ne!(w.m.snapshot().room(Size::G2).why, Some("making-room"));
    assert_eq!(
        w.m.open_slots(Size::G6),
        0,
        "no place for a size the Mac never runs"
    );
    assert_eq!(
        log.lines("no more room is made"),
        [
            "no more room is made for this size: the others start again size=\"cf-6g\" why=\"the settings no longer run this size\""
        ]
    );
}

#[tokio::test]
async fn the_size_room_is_made_for_is_promised_the_room_before_the_jobs_held_back_for_it() {
    let (w, cf4g) = mini_with_a_cf4g().await;
    let (cf6g, _) = waits(w.m.claim(Size::G6, None));
    pass(&w.m, 1);
    let (cf2g, why) = waits(w.m.claim(Size::G2, None));
    assert_eq!(why, "making-room");
    pass(&w.m, 60);
    w.m.withdraw(cf6g);
    ends(&w, &cf4g);
    // GitHub offers the cf-6g job again: its scale set takes it, ahead of the cf-2g job that waits for it.
    assert_eq!(w.m.open_slots(Size::G6), 1);
    assert!(w.m.reserve(Size::G6).is_ok());
    assert!(!w.m.ready(cf2g));
    assert_ne!(w.m.snapshot().room(Size::G2).why, Some("making-room"));
}

/// A Google Compute Engine host of the CI pool: 15.6 GB and 8 CPUs, given to CI, one of many.
fn gcp() -> World {
    World::on(cf_budget::Settings::for_headless(
        cf_budget::HostFacts {
            total_bytes: 15_600 * cf_budget::MIB,
            cores: 8,
        },
        8,
    ))
}

/// How long each cf-4g job of the steady load runs.
const JOB_S: u64 = 10 * 60;

/// A pool host under a steady cf-4g load: a new cf-4g job is assigned whenever one ends, each runs [`JOB_S`], and
/// the first three started a third of a job apart. A cf-6g job is assigned once the load is steady. Says after how
/// many seconds the cf-6g job got its machine, or none within `within` seconds.
async fn steady_cf4g_and_one_cf6g(w: &mut World, within: u64) -> Option<u64> {
    // (runner, when its job ends)
    let mut running: Vec<(String, u64)> = Vec::new();
    let mut waiting: Vec<Ticket> = Vec::new();
    for at in 0..3 {
        running.push((w.running_sized(Size::G4).await, at * JOB_S / 3 + JOB_S));
        pass(&w.m, JOB_S / 3);
    }
    let mut now = JOB_S;
    let cf6g = match w.m.claim(Size::G6, None) {
        Claim::Waits { ticket, .. } => ticket,
        other => panic!("a full host started the cf-6g job: {other:?}"),
    };
    for second in 0..within {
        pass(&w.m, 1);
        now += 1;
        if w.m.ready(cf6g) {
            return Some(second);
        }
        let (over, on): (Vec<_>, Vec<_>) = running.into_iter().partition(|(_, end)| *end <= now);
        running = on;
        for (runner, _) in over {
            w.parts.vms.end(job_name(&runner), End::Ok);
            w.github(EventKind::Completed, &runner, "succeeded");
            pass(&w.m, 2);
            now += 2;
            // The next cf-4g job is assigned at once.
            match w.m.claim(Size::G4, None) {
                Claim::Start(ticket) => {
                    let runner = w.start_as(ticket, Size::G4).await.expect("starts");
                    w.github(EventKind::Started, &runner, "");
                    running.push((runner, now + JOB_S));
                }
                Claim::Waits { ticket, .. } => waiting.push(ticket),
                other => panic!("{other:?}"),
            }
        }
        let mut still = Vec::new();
        for ticket in waiting {
            if w.m.ready(ticket) {
                let runner = w.start_as(ticket, Size::G4).await.expect("starts");
                w.github(EventKind::Started, &runner, "");
                running.push((runner, now + JOB_S));
            } else {
                still.push(ticket);
            }
        }
        waiting = still;
    }
    None
}

// added: one host of a pool that the console does not name never offers a place in line: it would win the job from
// a host with room, hold back its other sizes for it, and lose it when GitHub takes it back.
#[tokio::test]
async fn a_pool_host_the_console_does_not_name_offers_no_place_and_holds_nothing_back() {
    let log = Log::start();
    let mut w = gcp();
    for _ in 0..3 {
        w.running_sized(Size::G4).await;
    }
    assert!(
        polls(&w, Size::G6, 15 * 60).iter().all(|offered| *offered == 0),
        "a full pool host offered a cf-6g place"
    );
    assert!(!w.m.offers_place(Size::G6));
    assert!(log.lines("a place in line").is_empty());
    assert_ne!(
        w.m.snapshot().room(Size::G2).why,
        Some("making-room"),
        "the other sizes were held back"
    );
    assert!(log.lines("smaller ones keep taking").is_empty(), "room was made");
}

// added: the host the console names makes room under a steady cf-4g load: the cf-6g job starts within one job's
// length, and its place in line is offered at once, since the console waited for it.
#[tokio::test]
async fn the_pool_host_the_console_names_starts_the_cf6g_job_within_one_jobs_length() {
    let mut w = gcp();
    for _ in 0..3 {
        w.running_sized(Size::G4).await;
    }
    assert_eq!(w.m.open_slots(Size::G6), 0);
    w.m.make_room_for(Some(Size::G6));
    assert_eq!(w.m.open_slots(Size::G6), 1, "named, it offers a place at once");
    assert!(w.m.offers_place(Size::G6));
    w.m.make_room_for(None);
    assert_eq!(w.m.open_slots(Size::G6), 0, "no longer named, no place");

    let mut w = gcp();
    w.m.make_room_for(Some(Size::G6));
    let cf6g = steady_cf4g_and_one_cf6g(&mut w, 3 * JOB_S).await;
    let after = cf6g.expect("the cf-6g job never started");
    assert!(
        after <= JOB_S,
        "the cf-6g job waited {after} s, over one job's length"
    );
}
