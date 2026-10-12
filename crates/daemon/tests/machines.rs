//! The machines' side by itself, on the real budget and lifecycle with vm-hosts and a Mac a test scripts: every way
//! a machine ends and what is reported of it, two jobs that want the last slot, a job that can never fit, and a
//! daemon that starts again at every step of the loop.

mod support;

use std::sync::{Arc, Barrier};
use std::time::Duration;

use cf_budget::{GIB, MIB, Millis, Pressure, Size};
use cf_daemon::names::job_name;
use cf_daemon::testing::pass;
use cf_daemon::{Claim, EndKind, EventKind, Events, Hosts, JobEvent, RESULT_WAIT, Slots, Ticket};
use cf_guest_proto::StopReason;
use cf_vmhost::{End, OutOfMemory};
use support::Log;
use support::world::{REPO, SET, SIZE, World, ids};

// --- the Mac's clock ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_clock_that_moves_on_while_the_mac_is_read_closes_nothing() {
    // The Air, 2026-10-11: a look whose reading crossed a millisecond was thrown away as one of another moment,
    // and each time the Mac was closed for another calm minute.
    let w = World::new(2);
    w.m.with_host(|host| host.read_takes(Millis(1), Millis::ZERO));
    pass(&w.m, 61);
    assert!(matches!(w.m.claim(SIZE, None), Claim::Start(_)));
}

#[test]
fn the_log_says_when_the_mac_opens_and_closes_and_why_and_says_it_once() {
    let mut w = World::new(2);
    let log = Log::start();
    // A Runner that has just started: a minute of looks that find nothing wrong, then the Mac opens.
    w.restart();
    pass(&w.m, 60);
    assert_eq!(
        log.lines("the Mac is"),
        ["the Mac is closed: a new job waits why=\"calming\" opens_in_s=60"]
    );
    pass(&w.m, 1);
    w.m.with_host(|host| host.set(|r| r.pressure = Pressure::Warning));
    pass(&w.m, 3);
    w.m.with_host(|host| host.set(|r| r.pressure = Pressure::Normal));
    pass(&w.m, 61);
    // What someone set closes it too, and ends with no calm time after.
    w.m.pause(true);
    pass(&w.m, 2);
    w.m.pause(false);
    pass(&w.m, 2);
    assert_eq!(
        log.lines("the Mac is")[1..],
        [
            "the Mac is open: a new job gets a machine",
            "the Mac is closed: a new job waits why=\"memory-pressure\"",
            "the Mac is closed: a new job waits why=\"calming\" opens_in_s=60",
            "the Mac is open: a new job gets a machine",
            "the Mac is closed: a new job waits why=\"paused\"",
            "the Mac is open: a new job gets a machine",
        ]
    );
}

#[tokio::test]
async fn a_machine_that_stands_is_not_taken_for_one_nobody_can_ask_about_memory() {
    let mut w = World::new(1);
    let runner = w.running().await;
    let job = job_name(&runner).to_string();
    pass(&w.m, 1);
    // GitHub's word that a job is over comes while its machine still stands: its end will say the rest.
    assert_eq!(w.m.out_of_memory(&job).await, OutOfMemory::NotSoFar);
    w.parts.vms.out_of_memory(&job);
    assert!(matches!(w.m.out_of_memory(&job).await, OutOfMemory::Yes(_)));
    w.parts.vms.hang(&job);
    let silent = w.m.out_of_memory(&job).await;
    assert_eq!(
        silent,
        OutOfMemory::Unknown {
            reason: "the vm-host does not answer".into()
        }
    );
}

// --- every end ---------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_job_that_succeeds_is_reported_once_with_what_was_measured_and_leaves_nothing() {
    let mut w = World::new(2);
    let runner = w.start().await;
    let job = job_name(&runner).to_string();
    assert_eq!(w.parts.vms.standing(), [job.as_str()]);
    assert_eq!(w.slots(), 1);
    w.github(EventKind::Started, &runner, "");
    w.parts.vms.usage(&job, 700 * MIB, 3 * MIB);
    pass(&w.m, 5);
    assert_eq!(ids(&w.reports()), [format!("{runner}/started")]);

    w.parts.vms.end(&job, End::Ok);
    w.github(EventKind::Completed, &runner, "succeeded");
    pass(&w.m, 2);

    let reports = w.reports();
    assert_eq!(
        ids(&reports),
        [format!("{runner}/started"), format!("{runner}/completed")]
    );
    let done = &reports[1];
    assert_eq!(done.end, Some(EndKind::Success));
    assert_eq!(done.github.result.as_deref(), Some("succeeded"));
    assert_eq!(done.github.run_id, 42);
    assert!(!done.out_of_memory);
    assert_eq!(done.machine.size, "cf-2g");
    assert_eq!(done.machine.memory_bytes, 2 * GIB);
    assert_eq!(done.machine.limit_bytes, 2 * GIB - 256 * MIB);
    assert_eq!(done.machine.cpus, 1);
    assert_eq!(done.machine.origin, "assigned");
    assert_eq!(done.machine.boot_ms, Some(1200));
    assert_eq!(done.machine.memory_peak_bytes, Some(700 * MIB));
    assert_eq!(done.machine.wall_ms, Some(4200));
    assert_eq!(w.slots(), 0, "the slot went back with the machine");
    assert_eq!(
        w.registered.len(),
        1,
        "the registration is the service's to remove"
    );
    w.assert_nothing_left();
    // Nothing is said twice, however long the daemon goes on.
    pass(&w.m, 300);
    assert_eq!(w.reports().len(), 2);
}

#[tokio::test]
async fn a_failed_and_a_cancelled_job_are_reported_as_github_names_them() {
    let mut w = World::new(2);
    let failed = w.running().await;
    let cancelled = w.running().await;
    w.parts.vms.end(job_name(&failed), End::Failed);
    w.github(EventKind::Completed, &failed, "failed");
    // GitHub cancels: its word comes first, the Actions runner ends its job and the machine goes by itself.
    w.github(EventKind::Completed, &cancelled, "canceled");
    pass(&w.m, 1);
    assert_eq!(w.ends(), [(failed.clone(), EndKind::Failure)]);
    w.parts.vms.end(job_name(&cancelled), End::Ok);
    pass(&w.m, 2);
    assert_eq!(
        w.ends(),
        [(failed, EndKind::Failure), (cancelled, EndKind::Cancelled)]
    );
    w.assert_nothing_left();
}

#[tokio::test]
async fn a_machine_put_away_after_its_job_was_over_does_not_change_how_the_job_ended() {
    let mut w = World::new(1);
    let runner = w.running().await;
    w.github(EventKind::Completed, &runner, "succeeded");
    // The Actions runner does not exit by itself: the daemon stops the machine when the time to finish is over.
    pass(&w.m, 200);
    assert_eq!(
        w.parts.vms.stopped().iter().map(|s| s.1).collect::<Vec<_>>(),
        [StopReason::Cancelled]
    );
    assert_eq!(w.ends(), [(runner, EndKind::Success)]);
    w.assert_nothing_left();
}

#[tokio::test]
async fn a_job_github_says_nothing_of_is_reported_with_what_the_mac_knows_after_the_wait() {
    let mut w = World::new(1);
    let runner = w.running().await;
    w.parts.vms.end(job_name(&runner), End::Ok);
    pass(&w.m, RESULT_WAIT.as_secs() - 1);
    assert_eq!(w.ends(), [], "GitHub's word may still come");
    assert_eq!(w.slots(), 0, "the slot does not wait for it");
    pass(&w.m, 3);
    assert_eq!(w.ends(), [(runner.clone(), EndKind::Finished)]);
    // Its word after that changes nothing and is not a second report.
    w.github(EventKind::Completed, &runner, "succeeded");
    pass(&w.m, 5);
    assert_eq!(w.ends().len(), 1);
    w.assert_nothing_left();
}

#[tokio::test]
async fn a_job_the_vm_host_ends_at_its_deadline_timed_out_whatever_github_calls_it() {
    let mut w = World::new(1);
    let runner = w.running().await;
    w.parts.vms.end(job_name(&runner), End::Timeout);
    w.github(EventKind::Completed, &runner, "failed");
    pass(&w.m, 2);
    assert_eq!(w.ends(), [(runner, EndKind::Timeout)]);
    w.assert_nothing_left();
}

#[tokio::test]
async fn a_vm_host_that_misses_its_own_deadline_is_stopped_with_grace_and_then_killed() {
    let mut w = World::new(1);
    let runner = w.running().await;
    let job = job_name(&runner).to_string();
    w.parts.vms.ignore_stops(true);
    let vm = cf_vmhost::Limits::for_deadline(Duration::from_secs(6 * 3600));
    // The vm-host's own longest life passes without a word of an end.
    pass(&w.m, vm.longest_life().as_secs() + 59);
    assert_eq!(w.parts.vms.stopped(), [], "its own time is not over");
    pass(&w.m, 3);
    assert_eq!(
        w.parts.vms.stopped(),
        [(job.clone(), StopReason::Timeout, vm.deadline_grace)]
    );
    assert_eq!(w.parts.vms.killed(), Vec::<String>::new(), "the grace runs");
    assert_eq!(w.slots(), 1, "its memory is held while it stands");
    pass(&w.m, 60);
    assert_eq!(w.parts.vms.killed(), [job]);
    pass(&w.m, RESULT_WAIT.as_secs() + 2);
    assert_eq!(w.ends(), [(runner, EndKind::Timeout)]);
    w.assert_nothing_left();
}

#[tokio::test]
async fn a_guest_that_is_lost_a_vm_host_that_is_killed_and_one_that_hangs_all_end_as_guest_lost() {
    let mut w = World::new(3);
    let lost = w.running().await;
    let killed = w.running().await;
    let hangs = w.running().await;
    w.parts.vms.end(job_name(&lost), End::GuestLost);
    w.parts.vms.die(job_name(&killed));
    w.parts.vms.hang(job_name(&hangs));
    pass(&w.m, 2);
    assert_eq!(w.slots(), 1, "the two that are gone gave their memory back");
    assert_eq!(
        w.parts.vms.files(),
        [job_name(&hangs).to_string()],
        "what the dead left is removed"
    );
    // A vm-host that answers nothing for a minute is ended by force.
    pass(&w.m, 55);
    assert_eq!(w.parts.vms.killed(), Vec::<String>::new());
    pass(&w.m, 10);
    assert_eq!(w.parts.vms.killed(), [job_name(&hangs).to_string()]);
    for runner in [&lost, &killed, &hangs] {
        w.github(EventKind::Completed, runner, "failed");
    }
    pass(&w.m, 2);
    let mut ends = w.ends();
    ends.sort();
    assert_eq!(
        ends,
        [
            (lost, EndKind::GuestLost),
            (killed, EndKind::GuestLost),
            (hangs, EndKind::GuestLost)
        ]
    );
    w.assert_nothing_left();
}

#[tokio::test]
async fn a_job_that_lost_a_process_for_memory_says_so() {
    let mut w = World::new(1);
    let runner = w.running().await;
    let job = job_name(&runner).to_string();
    w.parts.vms.out_of_memory(&job);
    pass(&w.m, 1);
    assert!(!w.m.was_out_of_memory(&runner), "asked of machines that are gone");
    w.parts.vms.end(&job, End::Oom);
    w.github(EventKind::Completed, &runner, "failed");
    pass(&w.m, 2);
    let reports = w.reports();
    assert_eq!(reports[1].end, Some(EndKind::Failure));
    assert!(reports[1].out_of_memory);
    assert!(w.m.was_out_of_memory(&runner));
    w.assert_nothing_left();
}

#[tokio::test]
async fn when_the_mac_runs_out_of_memory_the_newest_job_is_stopped_and_only_one_at_a_time() {
    let mut w = World::new(3);
    let first = w.running().await;
    pass(&w.m, 1);
    let second = w.running().await;
    pass(&w.m, 1);
    let third = w.running().await;
    for runner in [&first, &second, &third] {
        w.parts.vms.usage(job_name(runner), 900 * MIB, MIB);
    }
    pass(&w.m, 2);
    let critical = w.m.settings().critical_for;
    w.m.with_host(|host| host.set(|r| r.pressure = Pressure::Critical));
    pass(&w.m, critical.0 / 1000 - 1);
    assert_eq!(w.parts.vms.stopped(), [], "a moment of pressure stops nothing");
    pass(&w.m, 3);
    assert_eq!(
        w.parts
            .vms
            .stopped()
            .iter()
            .map(|s| (s.0.as_str(), s.1))
            .collect::<Vec<_>>(),
        [(job_name(&third), StopReason::EvictedMemory)]
    );
    assert_eq!(w.ends(), [], "GitHub's word of the job is waited for");
    // The pressure stays: the next one goes only after the time between stopped jobs.
    let every = w.m.settings().evict_every;
    pass(&w.m, every.0 / 1000 - 5);
    assert_eq!(w.parts.vms.stopped().len(), 1);
    pass(&w.m, 8);
    assert_eq!(w.parts.vms.stopped().len(), 2);
    assert_eq!(w.parts.vms.stopped()[1].0, job_name(&second));
    w.m.with_host(|host| host.set(|r| r.pressure = Pressure::Normal));
    w.github(EventKind::Completed, &third, "failed");
    w.github(EventKind::Completed, &second, "failed");
    pass(&w.m, 2);
    assert_eq!(
        w.ends(),
        [(third, EndKind::EvictedMemory), (second, EndKind::EvictedMemory)]
    );
    assert_eq!(w.parts.vms.standing(), [job_name(&first).to_string()]);
    assert_eq!(w.slots(), 1);
}

#[tokio::test]
async fn when_the_disk_runs_out_the_job_with_the_most_on_it_is_stopped() {
    let mut w = World::new(2);
    let small = w.running().await;
    let large = w.running().await;
    w.parts.vms.usage(job_name(&small), 100 * MIB, GIB);
    w.parts.vms.usage(job_name(&large), 100 * MIB, 30 * GIB);
    pass(&w.m, 2);
    let below = w.m.settings().disk_evict_bytes - 1;
    w.m.with_host(|host| host.set(|r| r.disk_free_bytes = below));
    pass(&w.m, 2);
    assert_eq!(
        w.parts
            .vms
            .stopped()
            .iter()
            .map(|s| (s.0.as_str(), s.1))
            .collect::<Vec<_>>(),
        [(job_name(&large), StopReason::EvictedDisk)]
    );
    w.github(EventKind::Completed, &large, "failed");
    pass(&w.m, 2);
    assert_eq!(w.ends(), [(large, EndKind::EvictedDisk)]);
    assert_eq!(w.parts.vms.standing(), [job_name(&small).to_string()]);
}

#[tokio::test]
async fn a_mac_that_slept_long_ends_its_jobs_as_host_slept() {
    let mut w = World::new(1);
    let runner = w.running().await;
    w.m.with_host(|host| host.sleep(Millis::mins(30)));
    pass(&w.m, 2);
    assert_eq!(
        w.parts.vms.stopped().iter().map(|s| s.1).collect::<Vec<_>>(),
        [StopReason::HostSlept]
    );
    pass(&w.m, RESULT_WAIT.as_secs() + 2);
    assert_eq!(w.ends(), [(runner, EndKind::HostSlept)]);
    w.assert_nothing_left();
}

#[tokio::test]
async fn a_stop_for_every_machine_ends_jobs_as_stopped_and_a_drained_macs_cancelled_vm_host_as_host_shutdown()
{
    let mut w = World::new(2);
    let stopped = w.running().await;
    w.m.stop_all();
    pass(&w.m, 2);
    w.github(EventKind::Completed, &stopped, "canceled");
    pass(&w.m, 2);
    assert_eq!(w.ends(), [(stopped.clone(), EndKind::Stopped)]);

    // The Mac shuts down: the daemon drains and the system ends the vm-host, which calls that cancelled.
    let shut = w.running().await;
    w.m.drain();
    assert!(matches!(w.m.claim(SIZE, None), Claim::Waits { why, .. } if why == "draining"));
    w.parts.vms.end(job_name(&shut), End::Cancelled);
    pass(&w.m, RESULT_WAIT.as_secs() + 2);
    assert_eq!(
        w.ends(),
        [(stopped, EndKind::Stopped), (shut, EndKind::HostShutdown)]
    );
}

#[tokio::test]
async fn a_machine_that_never_comes_up_is_killed_frees_its_slot_and_three_in_a_row_close_the_mac() {
    let mut w = World::new(1);
    w.parts.vms.slow_boot(true);
    let start = cf_daemon::MachineConfig::new("mac-b", "x", "start-1", Duration::from_secs(6 * 3600))
        .limits
        .start;
    for round in 1..=3 {
        let runner = w.start().await;
        let job = job_name(&runner).to_string();
        pass(&w.m, start.0 / 1000 - 2);
        assert_eq!(w.parts.vms.standing(), [job.as_str()], "the start's time runs");
        assert_eq!(w.slots(), 1);
        pass(&w.m, 4);
        assert_eq!(w.parts.vms.killed().len(), round);
        assert_eq!(w.slots(), 0);
        assert_eq!(w.parts.vms.files(), Vec::<String>::new());
    }
    // No job ran on any of them: nothing to report, three registrations to remove.
    assert_eq!(w.reports(), []);
    assert_eq!(w.m.registrations_to_remove().len(), 3);
    assert_eq!(w.m.state(), (false, "start-failures".to_string()));
    assert!(matches!(w.m.claim(SIZE, None), Claim::Waits { why, .. } if why == "start-failures"));
}

#[tokio::test]
async fn a_slot_taken_back_for_room_before_its_machine_started_is_not_the_one_try_of_a_closed_mac() {
    let mut w = World::new(1);
    w.parts.vms.slow_boot(true);
    let start = cf_daemon::MachineConfig::new("mac-b", "x", "start-1", Duration::from_secs(6 * 3600))
        .limits
        .start;
    for _ in 1..=3 {
        w.start().await;
        pass(&w.m, start.0 / 1000 + 2);
    }
    // The one try after each wait fails too, and the wait doubles: 30 s to 15 minutes.
    for wait in [30, 60, 120, 240, 480] {
        pass(&w.m, wait);
        w.start().await;
        pass(&w.m, start.0 / 1000 + 2);
        assert_eq!(w.m.state(), (false, "start-failures".to_string()));
    }
    // The next try gets its slot, and the Mac runs out of memory before the machine is started.
    pass(&w.m, 900);
    let Claim::Start(_) = w.m.claim(SIZE, None) else {
        panic!("the one try");
    };
    assert_eq!(
        w.m.state(),
        (false, "start-failures".to_string()),
        "one try, not two"
    );
    let critical = w.m.settings().critical_for;
    w.m.with_host(|host| host.set(|r| r.pressure = Pressure::Critical));
    pass(&w.m, critical.0 / 1000 + 3);
    assert_eq!(w.slots(), 0, "the slot was taken back");
    w.m.with_host(|host| host.set(|r| r.pressure = Pressure::Normal));
    // The Mac is calm again long before a quarter of an hour is over. No start was tried, so none is awaited and
    // none failed: the try is still to be had.
    pass(&w.m, 300);
    assert_eq!(w.m.state(), (true, String::new()));
    assert!(matches!(w.m.claim(SIZE, None), Claim::Start(_)));
}

#[tokio::test]
async fn a_vm_host_that_does_not_die_keeps_its_memory_counted_and_closes_the_mac_until_it_is_gone() {
    let mut w = World::new(2);
    let runner = w.running().await;
    let job = job_name(&runner).to_string();
    w.parts.vms.ignore_stops(true);
    w.parts.vms.unkillable(&job, true);
    w.m.stop_all();
    pass(&w.m, 120);
    assert!(w.parts.vms.killed().len() > 1, "the kill is tried again");
    assert_eq!(w.m.snapshot().stuck, [job.as_str()]);
    assert_eq!(w.slots(), 1, "memory that is still held is still counted");
    assert_eq!(w.m.state(), (false, "start-failures".to_string()));
    assert_eq!(
        w.m.registrations_to_remove().len(),
        0,
        "its registration stays while it stands"
    );
    w.parts.vms.unkillable(&job, false);
    pass(&w.m, 15);
    assert_eq!(w.m.snapshot().stuck, Vec::<String>::new());
    assert_eq!(w.slots(), 0);
    pass(&w.m, RESULT_WAIT.as_secs());
    assert_eq!(w.ends(), [(runner, EndKind::Stopped)]);
    w.assert_nothing_left();
}

#[tokio::test]
async fn an_idle_machine_that_is_retired_reports_no_job() {
    let mut w = World::new(1);
    let runner = w.start().await;
    pass(&w.m, 3);
    w.m.stop(job_name(&runner)).await.expect("stop");
    pass(&w.m, 2);
    assert_eq!(w.reports(), []);
    w.assert_nothing_left();
}

// --- slots -------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn two_jobs_that_want_the_last_slot_get_it_one_after_the_other_in_the_order_they_came() {
    let mut w = World::new(1);
    // Eight scalers ask in the same instant.
    let barrier = Arc::new(Barrier::new(8));
    let claims: Vec<Claim> = std::thread::scope(|scope| {
        let asks: Vec<_> = (0..8)
            .map(|_| {
                let (m, barrier) = (w.m.clone(), barrier.clone());
                scope.spawn(move || {
                    barrier.wait();
                    m.claim(SIZE, None)
                })
            })
            .collect();
        asks.into_iter().map(|ask| ask.join().expect("claim")).collect()
    });
    let starts: Vec<Ticket> = claims
        .iter()
        .filter_map(|claim| match claim {
            Claim::Start(ticket) => Some(*ticket),
            _ => None,
        })
        .collect();
    let mut waits: Vec<Ticket> = claims
        .iter()
        .filter_map(|claim| match claim {
            Claim::Waits { ticket, why } => {
                // The first in line waits for room, the others for the ones before them.
                assert!(why == "full" || why == "assigned-first", "{why}");
                Some(*ticket)
            }
            _ => None,
        })
        .collect();
    assert_eq!((starts.len(), waits.len()), (1, 7), "one slot, one start");
    waits.sort();

    let mut runner = w.start_on(starts[0]).await.expect("start");
    for turn in 0..7 {
        pass(&w.m, 5);
        assert!(
            waits.iter().all(|t| !w.m.ready(*t)),
            "nobody starts while the slot is taken"
        );
        assert_eq!(w.parts.vms.standing().len(), 1);
        // A machine may not be started on a place in line whose turn has not come.
        let early = w.start_on(waits[turn]).await;
        assert!(early.is_err(), "started without a grant");
        let nudges = w.parts.nudges.load(std::sync::atomic::Ordering::Relaxed);
        w.parts.vms.end(job_name(&runner), End::Ok);
        pass(&w.m, 3);
        let ready: Vec<Ticket> = waits.iter().copied().filter(|t| w.m.ready(*t)).collect();
        assert_eq!(ready, [waits[turn]], "the one that waited longest, and only it");
        assert!(
            w.parts.nudges.load(std::sync::atomic::Ordering::Relaxed) > nudges,
            "the scalers are told"
        );
        runner = w.start_on(waits[turn]).await.expect("its turn");
        assert_eq!(w.parts.vms.standing(), [job_name(&runner).to_string()]);
    }
    w.parts.vms.end(job_name(&runner), End::Ok);
    pass(&w.m, 3);
    w.assert_nothing_left();
}

#[tokio::test]
async fn a_job_that_can_never_fit_is_refused_at_once_and_holds_nothing() {
    let w = World::new(2);
    // Room for two of 2 GB: a machine of 8 GB fits no state of this Mac.
    let Claim::Never(why) = w.m.claim(Size::G8, None) else {
        panic!("a job that can never fit must not wait");
    };
    assert_eq!(why, "full");
    assert_eq!(w.slots(), 0);
    assert_eq!(w.m.snapshot().waiting, 0);
    assert_eq!(w.m.open_slots(Size::G8), 0);
    assert_eq!(w.m.open_slots(SIZE), 2, "the jobs that fit are not in its way");
}

#[tokio::test]
async fn a_place_in_line_that_is_given_up_is_nobodys_and_a_machines_slot_cannot_be_given_back_from_outside() {
    let mut w = World::new(1);
    let Claim::Start(ticket) = w.m.claim(SIZE, None) else {
        panic!("room");
    };
    let runner = w.start_on(ticket).await.expect("start");
    let Claim::Waits { ticket: waiting, .. } = w.m.claim(SIZE, None) else {
        panic!("full");
    };
    // The scaler's bookkeeping going wrong may not free memory a machine holds.
    w.m.release(ticket);
    w.m.withdraw(ticket);
    assert_eq!(w.slots(), 1);
    assert_eq!(w.m.open_slots(SIZE), 0);
    w.m.withdraw(waiting);
    assert_eq!(w.m.snapshot().waiting, 0);
    w.parts.vms.end(job_name(&runner), End::Ok);
    pass(&w.m, 5);
    assert!(!w.m.ready(waiting), "a place that was given up never gets a turn");
    assert_eq!(w.m.open_slots(SIZE), 1);
    w.assert_nothing_left();
}

#[tokio::test]
async fn a_start_that_fails_leaves_a_registration_to_remove_and_no_machine() {
    let mut w = World::new(1);
    let Claim::Start(ticket) = w.m.claim(SIZE, None) else {
        panic!("room");
    };
    w.parts.vms.fail_start(1);
    let error = w.start_on(ticket).await.expect_err("the vm-host does not start");
    assert_eq!(error.0, "the vm-host did not start");
    assert_eq!(w.parts.vms.standing(), Vec::<String>::new());
    assert_eq!(w.m.registrations_to_remove().len(), 1);
    assert_eq!(w.m.registrations_to_remove()[0].runner_id, Some(1));
    // The slot is still the scaler's, which gives it back.
    assert_eq!(w.slots(), 1);
    w.m.release(ticket);
    w.assert_nothing_left();
}

// --- a daemon that starts again ----------------------------------------------------------------------------------

#[tokio::test]
async fn a_registration_that_was_only_intended_when_the_daemon_died_is_removed() {
    let mut w = World::new(1);
    let runner = w.name();
    w.m.registering(&runner, REPO, SET);
    // GitHub may have registered it or not: nobody heard the answer.
    w.registered.insert(runner.clone());
    assert_eq!(
        w.m.registrations_to_remove(),
        [],
        "it is being registered right now"
    );
    w.restart();
    let stale = w.m.registrations_to_remove();
    assert_eq!(stale.len(), 1);
    assert_eq!(
        (stale[0].runner.as_str(), stale[0].runner_id),
        (runner.as_str(), None)
    );
    assert_eq!((stale[0].repo.as_str(), stale[0].scale_set_id), (REPO, SET));
    w.assert_nothing_left();
}

#[tokio::test]
async fn a_registration_github_refused_leaves_no_trace_and_one_nobody_heard_of_is_removed_by_name() {
    let mut w = World::new(1);
    let refused = w.name();
    w.m.registering(&refused, REPO, SET);
    w.m.not_registered(&refused);
    let unsure = w.name();
    w.m.registering(&unsure, REPO, SET);
    w.m.registration_unsure(&unsure);
    assert_eq!(
        w.m.registrations_to_remove()
            .iter()
            .map(|r| r.runner.clone())
            .collect::<Vec<_>>(),
        [unsure]
    );
    w.assert_nothing_left();
}

#[tokio::test]
async fn a_daemon_that_died_before_its_vm_host_existed_ends_the_job_as_daemon_restart() {
    let mut w = World::new(1);
    let runner = w.start().await;
    w.parts.vms.forget(job_name(&runner));
    w.restart();
    assert_eq!(w.slots(), 0, "no machine, no memory");
    assert_eq!(w.m.registrations_to_remove().len(), 1);
    assert_eq!(w.m.registrations_to_remove()[0].runner_id, Some(1));
    // GitHub had given the job to it already.
    w.github(EventKind::Started, &runner, "");
    w.github(EventKind::Completed, &runner, "failed");
    pass(&w.m, 2);
    assert_eq!(w.ends(), [(runner, EndKind::DaemonRestart)]);
    w.assert_nothing_left();
}

#[tokio::test]
async fn a_machine_that_boots_or_runs_while_the_daemon_starts_again_is_adopted_and_its_job_goes_on() {
    let mut w = World::new(2);
    w.parts.vms.slow_boot(true);
    let booting = w.start().await;
    w.parts.vms.slow_boot(false);
    let runs = w.running().await;
    pass(&w.m, 2);

    w.restart();
    // Counted before anything else is decided.
    let snapshot = w.m.snapshot();
    assert_eq!(
        snapshot
            .budget
            .slots
            .iter()
            .map(|s| s.origin.name())
            .collect::<Vec<_>>(),
        ["adopted", "adopted"]
    );
    assert_eq!(w.m.open_slots(SIZE), 0);
    assert_eq!(w.parts.vms.standing().len(), 2, "nothing was stopped");
    assert_eq!(w.parts.vms.killed(), Vec::<String>::new());
    let adopted: Vec<String> = w
        .parts
        .roster
        .runners(REPO, SET)
        .into_iter()
        .map(|r| r.name)
        .collect();
    assert_eq!(adopted, [booting.clone(), runs.clone()]);
    assert!(w.parts.roster.runners(REPO, SET).iter().all(|r| r.adopted));
    pass(&w.m, 61);
    let Claim::Waits { ticket: third, why } = w.m.claim(SIZE, None) else {
        panic!("adopted machines hold their memory");
    };
    assert_eq!(why, "full");
    w.m.withdraw(third);

    w.parts.vms.ready(job_name(&booting));
    pass(&w.m, 1);
    w.github(EventKind::Started, &booting, "");
    for runner in [&booting, &runs] {
        w.parts.vms.end(job_name(runner), End::Ok);
        w.github(EventKind::Completed, runner, "succeeded");
    }
    pass(&w.m, 2);
    let mut reported = ids(&w.reports());
    reported.sort();
    assert_eq!(
        reported,
        [
            format!("{booting}/completed"),
            format!("{booting}/started"),
            format!("{runs}/completed"),
            format!("{runs}/started")
        ],
        "each start and each end once, across the two daemons"
    );
    assert!(
        w.reports()
            .iter()
            .filter_map(|r| r.end)
            .all(|end| end == EndKind::Success)
    );
    w.assert_nothing_left();
}

#[tokio::test]
async fn machines_that_ended_while_no_daemon_ran_are_ended_with_what_their_vm_host_wrote() {
    let mut w = World::new(3);
    let finished = w.running().await;
    let killed = w.running().await;
    let silent = w.running().await;
    w.parts.vms.end(job_name(&finished), End::Ok);
    w.parts.vms.die(job_name(&killed));
    w.parts.vms.hang(job_name(&silent));

    w.restart();
    assert_eq!(
        w.parts.vms.killed(),
        [job_name(&silent).to_string()],
        "a silent vm-host is ended first"
    );
    assert_eq!(w.parts.vms.standing(), Vec::<String>::new());
    assert_eq!(
        w.parts.vms.files(),
        Vec::<String>::new(),
        "disk, socket and state are removed"
    );
    assert_eq!(w.slots(), 0);
    assert_eq!(w.m.registrations_to_remove().len(), 3);
    w.github(EventKind::Completed, &finished, "succeeded");
    pass(&w.m, RESULT_WAIT.as_secs() + 2);
    let mut ends = w.ends();
    ends.sort();
    assert_eq!(
        ends,
        [
            (finished, EndKind::Success),
            (killed, EndKind::GuestLost),
            (silent, EndKind::GuestLost)
        ]
    );
    w.assert_nothing_left();
}

#[tokio::test]
async fn after_the_mac_started_again_every_job_of_before_ended_as_host_shutdown() {
    let mut w = World::new(2);
    let ran = w.running().await;
    let idle = w.start().await;
    w.reboot();
    assert_eq!(w.slots(), 0);
    assert_eq!(w.parts.vms.files(), Vec::<String>::new());
    pass(&w.m, RESULT_WAIT.as_secs() + 2);
    // The one without a job has nothing to report.
    assert_eq!(w.ends(), [(ran, EndKind::HostShutdown)]);
    assert_eq!(w.m.registrations_to_remove().len(), 2);
    let _ = idle;
    w.assert_nothing_left();
}

#[tokio::test]
async fn a_machine_nobody_has_a_record_of_is_ended_and_removed() {
    let mut w = World::new(1);
    let runner = w.start().await;
    w.parts.store.remove(job_name(&runner));
    w.restart();
    assert_eq!(w.parts.vms.killed(), [job_name(&runner).to_string()]);
    assert_eq!(w.parts.vms.standing(), Vec::<String>::new());
    assert_eq!(w.parts.vms.files(), Vec::<String>::new());
    assert_eq!(w.slots(), 0);
}

#[tokio::test]
async fn a_daemon_that_dies_between_a_machines_end_and_its_report_reports_it_once_after_it_starts_again() {
    let mut w = World::new(1);
    let runner = w.running().await;
    w.parts.vms.end(job_name(&runner), End::Failed);
    pass(&w.m, 2);
    assert_eq!(w.ends(), [], "GitHub's word is waited for");
    w.restart();
    w.restart();
    w.github(EventKind::Completed, &runner, "failed");
    pass(&w.m, 2);
    w.restart();
    pass(&w.m, RESULT_WAIT.as_secs() + 2);
    assert_eq!(w.ends(), [(runner.clone(), EndKind::Failure)]);
    // Reported, and the registration still at GitHub: it is still named for removal after another start.
    assert_eq!(w.m.registrations_to_remove().len(), 1);
    w.restart();
    assert_eq!(w.m.registrations_to_remove().len(), 1);
    w.assert_nothing_left();
    w.restart();
    assert_eq!(w.m.registrations_to_remove(), []);
    assert_eq!(w.ends().len(), 1);
}

/// One job's whole life, step by step, and whatever the world does next when the daemon is not there to see it.
#[tokio::test]
async fn a_daemon_that_dies_at_any_step_of_a_jobs_life_leaves_nothing_and_reports_the_job_once() {
    // The steps a job takes; the daemon dies after step `crash` and starts again.
    const STEPS: usize = 7;
    for crash in 0..=STEPS {
        for vm_survives in [true, false] {
            let mut w = World::new(1);
            let mut runner = None;
            let mut job_started = false;
            for step in 1..=STEPS {
                if step == crash + 1 {
                    if !vm_survives {
                        for job in w.parts.vms.standing() {
                            w.parts.vms.die(&job);
                        }
                    }
                    w.restart();
                    if step == 1 {
                        // A daemon that just started watches the Mac for a calm minute before it takes a job.
                        assert!(
                            matches!(w.m.claim(SIZE, None), Claim::Waits { ticket, .. } if { w.m.withdraw(ticket); true })
                        );
                        pass(&w.m, 61);
                    }
                }
                let alive = runner
                    .as_deref()
                    .is_some_and(|r: &str| w.parts.vms.standing().contains(&job_name(r).to_string()));
                match step {
                    1 => runner = Some(w.start().await),
                    2 => pass(&w.m, 3),
                    3 => {
                        // GitHub hands the job over only to an Actions runner that is there.
                        if alive {
                            w.github(EventKind::Started, runner.as_deref().expect("runner"), "");
                            job_started = true;
                        }
                    }
                    4 => pass(&w.m, 30),
                    5 => {
                        if alive {
                            w.parts
                                .vms
                                .end(job_name(runner.as_deref().expect("runner")), End::Ok);
                        }
                    }
                    6 => {
                        if job_started {
                            let result = if alive { "succeeded" } else { "failed" };
                            w.github(EventKind::Completed, runner.as_deref().expect("runner"), result);
                        }
                    }
                    _ => pass(&w.m, 2),
                }
                assert!(
                    w.parts.vms.standing().len() <= w.slots(),
                    "a machine without a grant"
                );
            }
            pass(&w.m, RESULT_WAIT.as_secs() + 5);
            let runner = runner.expect("runner");
            let expected: Vec<String> = if job_started {
                vec![format!("{runner}/started"), format!("{runner}/completed")]
            } else {
                vec![]
            };
            assert_eq!(
                ids(&w.reports()),
                expected,
                "crash after step {crash}, vm-host survives: {vm_survives}"
            );
            w.assert_nothing_left();
        }
    }
}

#[tokio::test]
async fn a_stop_in_the_moment_the_mac_wakes_is_not_put_off_by_the_time_it_slept() {
    let mut w = World::new(1);
    let runner = w.start().await;
    let job = job_name(&runner).to_string();
    pass(&w.m, 2);
    w.parts.vms.hang(&job);
    // The Mac sleeps for a few minutes and wakes; before the daemon's next look at it, the scaler retires the
    // machine. The time to its forced end runs from now, not from now plus the sleep.
    w.m.with_host(|host| host.sleep(Millis::mins(4)));
    w.m.stop(&job).await.expect("stop");
    pass(&w.m, 40);
    assert_eq!(w.parts.vms.killed(), [job.as_str()]);
    assert_eq!(w.slots(), 0);
    w.assert_nothing_left();
}

#[test]
fn a_pause_and_a_drain_tell_the_polls_in_flight_once() {
    let w = World::new(2);
    let before = w.parts.nudges.load(std::sync::atomic::Ordering::Relaxed);
    w.m.pause(true);
    w.m.pause(true);
    assert_eq!(
        w.parts.nudges.load(std::sync::atomic::Ordering::Relaxed),
        before + 1
    );
    w.m.pause(false);
    assert_eq!(
        w.parts.nudges.load(std::sync::atomic::Ordering::Relaxed),
        before + 2
    );
    w.m.drain();
    w.m.drain();
    assert_eq!(
        w.parts.nudges.load(std::sync::atomic::Ordering::Relaxed),
        before + 3
    );
}

#[tokio::test]
async fn a_machine_lost_under_its_job_is_counted_until_github_lets_the_job_go() {
    let mut w = World::new(3);
    // One dies before any job came to it: GitHub counts nothing for it.
    let idle = w.start().await;
    w.parts.vms.die(job_name(&idle));
    // Two die under their jobs.
    let (first, second) = (w.start().await, w.start().await);
    w.github(EventKind::Started, &first, "");
    w.github(EventKind::Started, &second, "");
    w.parts.vms.die(job_name(&first));
    w.parts.vms.die(job_name(&second));
    pass(&w.m, 2);
    assert_eq!(w.parts.vms.standing(), Vec::<String>::new());
    assert_eq!(w.m.lost_with_job(REPO, SET), 2, "GitHub still counts both jobs");
    assert_eq!(w.m.lost_with_job(REPO, SET + 1), 0, "and for this scale set only");
    assert_eq!(
        w.m.lost_with_job("org/other", SET),
        0,
        "not for another repository's scale set that has the same number"
    );
    // GitHub says how the first ended; the registration of the second is gone, and its job with it.
    w.github(EventKind::Completed, &first, "failed");
    assert_eq!(w.m.lost_with_job(REPO, SET), 1);
    w.registered.remove(&second);
    w.m.unregistered(&second);
    assert_eq!(w.m.lost_with_job(REPO, SET), 0);
    w.assert_nothing_left();
}

#[tokio::test]
async fn a_machine_that_dies_before_it_was_seen_up_but_took_a_job_is_lost_and_not_a_failed_start() {
    let mut w = World::new(2);
    w.parts.vms.slow_boot(true);
    let runner = w.start().await;
    // The vm-host dies between two looks; GitHub's word that the job started arrives after.
    w.parts.vms.die(job_name(&runner));
    pass(&w.m, 2);
    w.github(EventKind::Started, &runner, "");
    w.github(EventKind::Completed, &runner, "failed");
    pass(&w.m, 1);
    assert_eq!(w.ends(), [(runner, EndKind::GuestLost)]);
    w.assert_nothing_left();
}

#[tokio::test]
async fn a_job_githubs_word_names_before_the_vm_host_wrote_anything_runs_to_its_end() {
    let mut w = World::new(2);
    w.parts.vms.late_state(true);
    let runner = w.start().await;
    // GitHub's word that the job started is here before the first look finds anything of the vm-host.
    w.github(EventKind::Started, &runner, "");
    pass(&w.m, 3);
    assert_eq!(
        w.m.snapshot()
            .machines
            .iter()
            .map(|m| m.state)
            .collect::<Vec<_>>(),
        ["running"],
        "a vm-host that has not written yet is no machine that is gone"
    );
    assert_eq!(w.parts.vms.killed(), Vec::<String>::new());
    w.parts.vms.written(job_name(&runner));
    pass(&w.m, 2);
    w.parts.vms.end(job_name(&runner), End::Ok);
    w.github(EventKind::Completed, &runner, "succeeded");
    pass(&w.m, 2);
    assert_eq!(w.ends(), [(runner, EndKind::Success)]);
    w.assert_nothing_left();
}

#[tokio::test]
async fn a_vm_host_that_never_writes_anything_under_a_job_is_ended_as_one_that_hangs() {
    let mut w = World::new(2);
    w.parts.vms.late_state(true);
    let runner = w.start().await;
    w.github(EventKind::Started, &runner, "");
    // The time a vm-host may be silent, and no more: no wait without an end.
    pass(&w.m, 59);
    assert_eq!(w.ends(), []);
    pass(&w.m, 30);
    w.github(EventKind::Completed, &runner, "failed");
    pass(&w.m, 30);
    assert_eq!(w.ends(), [(runner, EndKind::GuestLost)]);
    w.assert_nothing_left();
}

#[tokio::test]
async fn a_look_at_a_vm_host_that_does_not_answer_keeps_nobody_else_waiting() {
    let mut w = World::new(2);
    let runner = w.running().await;
    let held = w.parts.vms.hold_looks();
    let (m, ticking) = (w.m.clone(), w.m.clone());
    let tick = std::thread::spawn(move || ticking.tick());
    assert!(
        held.waiting(Duration::from_secs(10)),
        "the tick looks at the vm-host"
    );
    // What the listeners ask and tell meanwhile: a slot, the Mac's state, GitHub's word of a job.
    let (asked, answered) = std::sync::mpsc::channel();
    let name = runner.clone();
    let asking = std::thread::spawn(move || {
        let state = m.state();
        let claim = m.claim(SIZE, None);
        m.job(&JobEvent {
            kind: EventKind::Completed,
            runner: name,
            repo: REPO.into(),
            job: "build".into(),
            workflow: "ci.yml".into(),
            run_id: 42,
            event: "push".into(),
            queued_at: None,
            started_at: None,
            at: time::OffsetDateTime::UNIX_EPOCH,
            result: "succeeded".into(),
            out_of_memory: false,
        });
        let _ = asked.send((state, claim));
    });
    let got = answered.recv_timeout(Duration::from_secs(5));
    held.let_go();
    tick.join().expect("the tick");
    asking.join().expect("the asking");
    let (state, claim) = got.expect("an answer while the look still waits");
    assert_eq!(state, (true, String::new()));
    assert!(matches!(claim, Claim::Start(_)), "{claim:?}");
    // The look that waited is still told to the machine, and the word that came meanwhile is kept.
    w.parts.vms.end(job_name(&runner), End::Ok);
    pass(&w.m, 2);
    assert_eq!(w.ends(), [(runner, EndKind::Success)]);
}

#[tokio::test]
async fn a_vm_host_that_hangs_before_it_wrote_anything_is_ended_by_the_runner_that_starts_next() {
    let mut w = World::new(1);
    w.parts.vms.late_state(true);
    let runner = w.start().await;
    assert_eq!(
        w.parts.vms.standing(),
        [job_name(&runner)],
        "its process is there"
    );
    // The Runner dies; the one that starts finds a record and no state, and a process only the record names.
    w.restart();
    assert_eq!(
        w.parts.vms.standing(),
        Vec::<String>::new(),
        "the vm-host of before is ended, not left to boot a machine nobody counts"
    );
    assert_eq!(w.slots(), 0);
    w.github(EventKind::Started, &runner, "");
    w.github(EventKind::Completed, &runner, "failed");
    pass(&w.m, 2);
    assert_eq!(w.ends(), [(runner, EndKind::DaemonRestart)]);
    w.assert_nothing_left();
}

// --- the room the polls in flight offer ----------------------------------------------------------------------------

fn nudges(w: &World) -> usize {
    w.parts.nudges.load(std::sync::atomic::Ordering::Relaxed)
}

/// The memory the Mac's other programs leave: enough to stay open, too little for one machine of the test's size.
fn memory_for_no_machine(w: &World) -> u64 {
    w.parts.settings.away.floor_bytes + GIB
}

#[test]
fn the_last_slot_one_scale_set_takes_ends_the_polls_that_still_offer_it() {
    // The Air, 2026-10-12: eight scale sets each offered GitHub the one slot that was free, GitHub assigned a job
    // to several, and all but one waited in line for memory while the other Mac had room.
    let w = World::new(2);
    assert_eq!(w.m.open_slots(SIZE), 2, "a listener begins its poll with room");
    let before = nudges(&w);
    assert!(matches!(w.m.claim(SIZE, None), Claim::Start(_)));
    assert_eq!(
        nudges(&w),
        before,
        "a slot is left: every poll's offer still holds"
    );
    assert_eq!(w.m.open_slots(SIZE), 1);
    assert!(matches!(w.m.claim(SIZE, None), Claim::Start(_)));
    assert_eq!(nudges(&w), before + 1, "the last slot went: the polls are told");
    assert_eq!(w.m.open_slots(SIZE), 0);
    // A job GitHub assigned all the same waits in line, and nobody is told twice of room that was gone already.
    assert!(matches!(w.m.claim(SIZE, None), Claim::Waits { .. }));
    assert_eq!(nudges(&w), before + 1);
}

#[test]
fn a_promise_for_the_last_slot_ends_the_polls_that_still_offer_it() {
    let w = World::new(1);
    assert_eq!(w.m.open_slots(SIZE), 1);
    let before = nudges(&w);
    let ticket = w.m.reserve(SIZE).expect("a slot");
    assert_eq!(nudges(&w), before + 1);
    // The promise is made a machine's: nothing more went.
    assert!(matches!(w.m.claim(SIZE, Some(ticket)), Claim::Start(_)));
    assert_eq!(nudges(&w), before + 1);
}

#[test]
fn a_slot_no_poll_was_told_of_ends_no_poll() {
    let w = World::new(1);
    let before = nudges(&w);
    assert!(matches!(w.m.claim(SIZE, None), Claim::Start(_)));
    assert_eq!(nudges(&w), before);
}

#[test]
fn room_the_macs_other_programs_took_ends_the_polls_at_the_next_look_and_then_every_five_seconds_at_most() {
    let w = World::new(2);
    let short = memory_for_no_machine(&w);
    let set = |bytes: u64| w.m.with_host(|host| host.set(|r| r.available_bytes = bytes));
    assert_eq!(w.m.open_slots(SIZE), 2);
    let before = nudges(&w);
    pass(&w.m, 3);
    assert_eq!(nudges(&w), before, "nothing changed: no poll is ended");

    set(short);
    pass(&w.m, 1);
    assert_eq!(nudges(&w), before + 1, "told at the first look that sees it");
    assert_eq!(w.m.open_slots(SIZE), 0);
    pass(&w.m, 10);
    assert_eq!(nudges(&w), before + 1, "and once");

    // The memory comes and goes: a listener hears of room again, and it is gone at the next look.
    set(64 * GIB);
    pass(&w.m, 1);
    assert_eq!(w.m.open_slots(SIZE), 2);
    set(short);
    pass(&w.m, 1);
    assert_eq!(
        nudges(&w),
        before + 2,
        "five seconds after the last time: told at once"
    );
    set(64 * GIB);
    pass(&w.m, 1);
    assert_eq!(w.m.open_slots(SIZE), 2);
    set(short);
    pass(&w.m, 3);
    assert_eq!(
        nudges(&w),
        before + 2,
        "within five seconds of the last time: not yet"
    );
    pass(&w.m, 2);
    assert_eq!(nudges(&w), before + 3, "and then, although nothing changed since");
}

#[test]
fn an_open_mac_with_no_room_for_a_size_says_so_once_and_when_one_fits_again() {
    // The mini, 2026-10-12: open for hours, "taking jobs", and not one job, because its other programs left room
    // for no machine of the only size it serves. Its log said nothing.
    let w = World::new(2);
    let log = Log::start();
    let set = |bytes: u64| w.m.with_host(|host| host.set(|r| r.available_bytes = bytes));
    set(memory_for_no_machine(&w));
    pass(&w.m, 1);
    assert_eq!(w.m.short_of(SIZE), Some("memory"));
    for _ in 0..3 {
        assert_eq!(w.m.open_slots(SIZE), 0);
        pass(&w.m, 30);
    }
    let said = "the Mac is open, but no machine of this size fits now: its jobs stay at GitHub size=\"cf-2g\" why=\"memory\"";
    assert_eq!(log.lines("of this size"), [said]);
    assert_eq!(log.lines("the Mac is closed"), Vec::<String>::new());

    set(64 * GIB);
    pass(&w.m, 1);
    assert_eq!(w.m.short_of(SIZE), None);
    assert_eq!(w.m.open_slots(SIZE), 2);
    assert_eq!(w.m.open_slots(SIZE), 2);
    assert_eq!(
        log.lines("of this size"),
        [said, "a machine of this size fits again size=\"cf-2g\""]
    );
}

#[test]
fn a_mac_its_own_machines_fill_and_a_closed_mac_are_not_said_to_be_short_of_memory() {
    let w = World::new(1);
    let log = Log::start();
    assert!(matches!(w.m.claim(SIZE, None), Claim::Start(_)));
    assert_eq!(w.m.open_slots(SIZE), 0);
    assert_eq!(w.m.short_of(SIZE), None, "full of its own machines");
    w.m.pause(true);
    w.m.with_host(|host| host.set(|r| r.available_bytes = w.parts.settings.away.floor_bytes + GIB));
    pass(&w.m, 1);
    assert_eq!(w.m.open_slots(SIZE), 0);
    assert_eq!(w.m.short_of(SIZE), None, "closed: the Mac says why itself");
    assert_eq!(log.lines("of this size"), Vec::<String>::new());
}
