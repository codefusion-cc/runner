//! Each way a slot can go, step by step: what the daemon is told to do and how the slot ends.

use cf_budget::{EvictionCause, Millis};
use cf_lifecycle::{Effect, End, Event, Limits, Notice, Phase, Saved, Slot, Timer};

const MEMORY: Event = Event::Evict(EvictionCause::Memory);
const DISK: Event = Event::Evict(EvictionCause::Disk);

fn s(seconds: u64) -> Millis {
    Millis::secs(seconds)
}

fn ended(end: End, ran_job: bool) -> Effect {
    Effect::Ended { end, ran_job }
}

/// A slot taken through `events`, one a second from second 1. Returns it with the effects of the last event.
fn after(events: &[Event]) -> (Slot, Vec<Effect>) {
    let mut slot = Slot::reserved(Limits::default());
    let mut last = Vec::new();
    for (second, event) in events.iter().enumerate() {
        last = slot.on(s(second as u64 + 1), *event);
    }
    (slot, last)
}

fn starting() -> Slot {
    after(&[Event::Assigned]).0
}

fn idle() -> Slot {
    after(&[Event::Assigned, Event::VmReady]).0
}

fn running() -> Slot {
    after(&[Event::Assigned, Event::VmReady, Event::JobStarted]).0
}

fn finishing() -> Slot {
    after(&[
        Event::Assigned,
        Event::VmReady,
        Event::JobStarted,
        Event::JobCompleted,
    ])
    .0
}

#[test]
fn a_job_from_reservation_to_release() {
    let mut slot = Slot::reserved(Limits::default());
    assert_eq!(
        (slot.phase(), slot.holds_memory(), slot.next_deadline()),
        (Phase::Reserved, true, None)
    );
    assert_eq!(slot.on(s(1), Event::Assigned), [Effect::StartVm]);
    assert_eq!(
        (slot.phase(), slot.timers(s(1))),
        (Phase::Starting, vec![(Timer::Start, s(20))])
    );
    assert_eq!(slot.on(s(3), Event::VmReady), [Effect::StartSucceeded]);
    assert_eq!(
        slot.timers(s(3)),
        [(Timer::Idle, s(120)), (Timer::Heartbeat, s(60))]
    );
    assert_eq!(slot.on(s(9), Event::JobStarted), []);
    assert_eq!(
        (slot.phase(), slot.timers(s(9))),
        (
            Phase::Running,
            vec![(Timer::Heartbeat, s(60)), (Timer::Job, s(21_600))]
        )
    );
    assert_eq!(slot.on(s(500), Event::Heartbeat), []);
    assert_eq!(slot.on(s(540), Event::JobCompleted), []);
    assert_eq!(
        (slot.phase(), slot.timers(s(540))),
        (Phase::Finishing, vec![(Timer::Finish, s(120))])
    );
    assert!(slot.holds_memory(), "the VM is still there");
    assert_eq!(
        slot.on(s(542), Event::VmExited),
        [Effect::Unregister, Effect::Release, ended(End::Ok, true)]
    );
    assert_eq!(
        (slot.phase(), slot.holds_memory(), slot.next_deadline()),
        (Phase::Released, false, None)
    );
    assert_eq!(slot.timers(s(542)), []);
}

#[test]
fn a_reservation_without_a_job_gives_its_memory_back_and_starts_nothing() {
    for event in [Event::Lost, Event::Stop, MEMORY, DISK] {
        let (slot, effects) = after(&[event]);
        assert_eq!(effects, [Effect::Release, ended(End::Lost, false)], "{event:?}");
        assert_eq!(slot.phase(), Phase::Released);
    }
    // The budget took the reservation back itself: there is nothing left to give back.
    let (slot, effects) = after(&[Event::Expired]);
    assert_eq!(effects, [ended(End::Lost, false)]);
    assert!(!slot.holds_memory());
    // Nothing else moves a reservation, and time does not either: its two minutes are the budget's.
    for event in [
        Event::VmReady,
        Event::JobStarted,
        Event::Heartbeat,
        Event::OutOfMemory,
        Event::JobCompleted,
        Event::VmExited,
        Event::Tick,
        Event::Slept(s(9000)),
    ] {
        let mut slot = Slot::reserved(Limits::default());
        assert_eq!(slot.on(s(100_000), event), [], "{event:?}");
        assert_eq!(slot.phase(), Phase::Reserved);
    }
}

#[test]
fn a_vm_that_does_not_get_ready_is_killed_and_started_once_more() {
    let mut slot = starting();
    assert_eq!(slot.on(Millis(20_999), Event::Tick), []);
    assert_eq!(slot.on(s(21), Event::Tick), [Effect::StartFailed, Effect::KillVm]);
    assert_eq!(slot.phase(), Phase::Retrying);
    // No second VM while the first one's process may be there.
    assert_eq!(slot.on(s(22), Event::Tick), []);
    assert_eq!(slot.on(s(22), Event::Assigned), []);
    assert_eq!(slot.on(s(23), Event::VmExited), [Effect::StartVm]);
    assert_eq!(
        (slot.phase(), slot.timers(s(23))),
        (Phase::Starting, vec![(Timer::Start, s(20))])
    );
    // The second one fails too: that was the retry.
    assert_eq!(slot.on(s(43), Event::Tick), [Effect::StartFailed, Effect::KillVm]);
    assert_eq!(slot.phase(), Phase::Killing);
    assert!(slot.holds_memory(), "not given back while the process may live");
    assert_eq!(
        slot.on(s(44), Event::VmExited),
        [
            Effect::Unregister,
            Effect::Release,
            ended(End::StartFailed, false)
        ]
    );
}

#[test]
fn a_retry_that_is_no_longer_wanted_starts_no_second_vm() {
    // The first VM was killed and its process is still going: whatever ends the slot now also ends the retry.
    let cases = [
        (MEMORY, End::EvictedMemory),
        (DISK, End::EvictedDisk),
        (Event::Stop, End::Stopped),
        (Event::Slept(Millis::mins(5).plus(Millis(1))), End::HostSlept),
    ];
    for (event, end) in cases {
        let mut slot = starting();
        slot.on(s(21), Event::Tick);
        assert_eq!(slot.phase(), Phase::Retrying);
        assert_eq!(slot.on(s(22), event), [], "{event:?}: the kill was sent already");
        assert_eq!(slot.phase(), Phase::Killing, "{event:?}");
        assert!(slot.holds_memory());
        assert_eq!(
            slot.on(s(23), Event::VmExited),
            [Effect::Unregister, Effect::Release, ended(end, false)],
            "{event:?}"
        );
    }
    // A sleep of five minutes or less changes nothing: the second VM is started when the first is gone.
    for nap in [Millis(1), Millis::secs(30), Millis::mins(5)] {
        let mut slot = starting();
        slot.on(s(21), Event::Tick);
        assert_eq!(slot.on(s(22), Event::Slept(nap)), []);
        assert_eq!(slot.phase(), Phase::Retrying, "{nap:?}");
        assert_eq!(slot.on(s(23), Event::VmExited), [Effect::StartVm]);
    }
}

#[test]
fn a_vm_that_dies_while_booting_counts_as_a_failed_start() {
    let mut slot = starting();
    assert_eq!(
        slot.on(s(2), Event::VmExited),
        [Effect::StartFailed, Effect::StartVm]
    );
    assert_eq!(slot.phase(), Phase::Starting);
    assert_eq!(
        slot.on(s(3), Event::VmExited),
        [
            Effect::StartFailed,
            Effect::Unregister,
            Effect::Release,
            ended(End::StartFailed, false)
        ]
    );
    // The retry that works is a normal slot from there.
    let mut slot = starting();
    slot.on(s(2), Event::VmExited);
    assert_eq!(slot.on(s(5), Event::VmReady), [Effect::StartSucceeded]);
    assert_eq!(slot.phase(), Phase::Idle);
}

#[test]
fn the_number_of_starts_is_a_limit() {
    for starts in [1, 2, 4] {
        let mut slot = Slot::reserved(Limits {
            starts,
            ..Limits::default()
        });
        let mut started = slot
            .on(s(0), Event::Assigned)
            .iter()
            .filter(|e| **e == Effect::StartVm)
            .count();
        for second in 1..20 {
            started += slot
                .on(s(second), Event::VmExited)
                .iter()
                .filter(|e| **e == Effect::StartVm)
                .count();
        }
        assert_eq!((started, slot.phase()), (starts as usize, Phase::Released));
    }
}

#[test]
fn a_process_that_survives_sigkill_is_killed_again_and_its_memory_kept() {
    let mut slot = idle();
    slot.on(s(100), Event::Heartbeat);
    assert_eq!(slot.on(Millis(121_999), Event::Tick), []);
    assert_eq!(
        slot.on(s(122), Event::Tick),
        [Effect::StopVm { grace: Millis::ZERO }]
    );
    assert_eq!(
        (slot.phase(), slot.timers(s(122))),
        (Phase::Stopping, vec![(Timer::Stop, s(10))])
    );
    assert_eq!(slot.on(Millis(131_999), Event::Tick), []);
    assert_eq!(slot.on(s(132), Event::Tick), [Effect::KillVm]);
    assert_eq!(
        (slot.phase(), slot.timers(s(132))),
        (Phase::Killing, vec![(Timer::Kill, s(10))])
    );
    for round in 1..50 {
        assert_eq!(
            slot.on(s(132 + 10 * round), Event::Tick),
            [Effect::KillVm, Effect::Stuck]
        );
        assert_eq!(slot.on(s(133 + 10 * round), Event::Tick), []);
        assert!(slot.holds_memory() && slot.phase() == Phase::Killing);
    }
    assert_eq!(
        slot.on(s(9000), Event::VmExited),
        [Effect::Unregister, Effect::Release, ended(End::NoJob, false)]
    );
}

#[test]
fn a_guest_that_goes_silent_is_killed() {
    for mut slot in [idle(), running()] {
        let ran_job = slot.phase() == Phase::Running;
        slot.on(s(50), Event::Heartbeat);
        assert_eq!(slot.on(Millis(109_999), Event::Tick), []);
        assert_eq!(slot.on(s(110), Event::Tick), [Effect::KillVm]);
        assert_eq!(slot.phase(), Phase::Killing);
        assert_eq!(
            slot.on(s(111), Event::VmExited),
            [
                Effect::Unregister,
                Effect::Release,
                ended(End::GuestLost, ran_job)
            ]
        );
    }
    // Heartbeats keep it alive for as long as they come.
    let mut slot = running();
    for second in (10..5000).step_by(59) {
        assert_eq!(slot.on(s(second), Event::Heartbeat), []);
        assert_eq!(slot.on(s(second + 58), Event::Tick), []);
    }
    assert_eq!(slot.phase(), Phase::Running);
}

#[test]
fn a_vm_that_dies_under_a_job_is_guest_lost() {
    let mut slot = running();
    slot.on(s(5), Event::OutOfMemory);
    assert_eq!(
        slot.on(s(6), Event::VmExited),
        [Effect::Unregister, Effect::Release, ended(End::GuestLost, true)]
    );
    let mut slot = idle();
    assert_eq!(
        slot.on(s(6), Event::VmExited),
        [Effect::Unregister, Effect::Release, ended(End::GuestLost, false)]
    );
}

#[test]
fn a_job_is_stopped_after_six_hours_with_time_to_upload() {
    let mut slot = running();
    let started = 3;
    for minute in 1..360 {
        slot.on(s(started + minute * 60 - 1), Event::Heartbeat);
        assert_eq!(
            slot.on(s(started + minute * 60), Event::Tick),
            [],
            "minute {minute}"
        );
    }
    slot.on(s(started + 21_590), Event::Heartbeat);
    assert_eq!(slot.on(Millis((started + 21_600) * 1000 - 1), Event::Tick), []);
    assert_eq!(
        slot.on(s(started + 21_600), Event::Tick),
        [Effect::Tell(Notice::TimedOut), Effect::StopVm { grace: s(20) }]
    );
    assert_eq!(slot.timers(s(started + 21_600)), [(Timer::Stop, s(30))]);
    assert_eq!(slot.on(s(started + 21_629), Event::Tick), []);
    assert_eq!(slot.on(s(started + 21_630), Event::Tick), [Effect::KillVm]);
    assert_eq!(
        slot.on(s(started + 21_631), Event::VmExited),
        [Effect::Unregister, Effect::Release, ended(End::Timeout, true)]
    );
}

#[test]
fn a_job_stopped_for_the_mac_is_told_why_and_keeps_that_end() {
    for (event, cause, end) in [
        (MEMORY, EvictionCause::Memory, End::EvictedMemory),
        (DISK, EvictionCause::Disk, End::EvictedDisk),
    ] {
        let mut slot = running();
        assert_eq!(
            slot.on(s(10), event),
            [
                Effect::Tell(Notice::Evicted(cause)),
                Effect::StopVm { grace: s(20) }
            ]
        );
        // Told once; the job finishing in its last seconds does not make it a normal end.
        assert_eq!(slot.on(s(11), event), []);
        assert_eq!(slot.on(s(12), Event::JobCompleted), []);
        assert_eq!(slot.on(s(12), Event::Stop), []);
        assert_eq!(
            slot.on(s(13), Event::VmExited),
            [Effect::Unregister, Effect::Release, ended(end, true)]
        );
        // Without a job there is nobody to tell and nothing to upload.
        let mut slot = idle();
        assert_eq!(slot.on(s(10), event), [Effect::StopVm { grace: Millis::ZERO }]);
        assert_eq!(
            slot.on(s(11), Event::VmExited),
            [Effect::Unregister, Effect::Release, ended(end, false)]
        );
        // A VM that is still booting is killed outright.
        let mut slot = starting();
        assert_eq!(slot.on(s(2), event), [Effect::KillVm]);
        assert_eq!(
            slot.on(s(3), Event::VmExited),
            [Effect::Unregister, Effect::Release, ended(end, false)]
        );
    }
}

#[test]
fn stop_from_the_console_in_every_phase() {
    let mut slot = starting();
    assert_eq!(slot.on(s(2), Event::Stop), [Effect::KillVm]);
    assert_eq!(
        slot.on(s(3), Event::VmExited),
        [Effect::Unregister, Effect::Release, ended(End::Stopped, false)]
    );
    let mut slot = idle();
    assert_eq!(
        slot.on(s(5), Event::Stop),
        [Effect::StopVm { grace: Millis::ZERO }]
    );
    let mut slot = running();
    assert_eq!(
        slot.on(s(5), Event::Stop),
        [Effect::Tell(Notice::Stopped), Effect::StopVm { grace: s(20) }]
    );
    assert_eq!(
        slot.on(s(6), Event::VmExited),
        [Effect::Unregister, Effect::Release, ended(End::Stopped, true)]
    );
    // After the job's end the VM just goes sooner; the job's end stays what it was.
    let mut slot = finishing();
    assert_eq!(
        slot.on(s(5), Event::Stop),
        [Effect::StopVm { grace: Millis::ZERO }]
    );
    assert_eq!(
        slot.on(s(6), Event::VmExited),
        [Effect::Unregister, Effect::Release, ended(End::Ok, true)]
    );
    // While the first VM of two is being killed: no second one.
    let mut slot = starting();
    slot.on(s(21), Event::Tick);
    assert_eq!(slot.on(s(22), Event::Stop), []);
    assert_eq!(slot.phase(), Phase::Killing);
    assert_eq!(
        slot.on(s(23), Event::VmExited),
        [Effect::Unregister, Effect::Release, ended(End::Stopped, false)]
    );
}

#[test]
fn out_of_memory_is_the_end_only_of_a_job_that_was_running() {
    let mut slot = running();
    assert_eq!(slot.on(s(5), Event::OutOfMemory), []);
    assert_eq!(
        slot.phase(),
        Phase::Running,
        "the step fails by itself; the job goes on to its end"
    );
    slot.on(s(6), Event::JobCompleted);
    assert_eq!(
        slot.on(s(7), Event::VmExited),
        [Effect::Unregister, Effect::Release, ended(End::Oom, true)]
    );
    let mut slot = finishing();
    slot.on(s(6), Event::OutOfMemory);
    assert_eq!(
        slot.on(s(7), Event::VmExited),
        [Effect::Unregister, Effect::Release, ended(End::Ok, true)]
    );
}

#[test]
fn a_guest_that_does_not_power_off_after_its_job_is_stopped() {
    let mut slot = finishing();
    assert_eq!(slot.on(Millis(123_999), Event::Tick), []);
    assert_eq!(
        slot.on(s(124), Event::Tick),
        [Effect::StopVm { grace: Millis::ZERO }]
    );
    assert_eq!(slot.on(s(134), Event::Tick), [Effect::KillVm]);
    assert_eq!(
        slot.on(s(135), Event::VmExited),
        [Effect::Unregister, Effect::Release, ended(End::Ok, true)]
    );
    // Evicting a job that is already over changes nothing about how it ended.
    let mut slot = finishing();
    assert_eq!(slot.on(s(5), MEMORY), [Effect::StopVm { grace: Millis::ZERO }]);
    assert_eq!(
        slot.on(s(6), Event::VmExited),
        [Effect::Unregister, Effect::Release, ended(End::Ok, true)]
    );
}

#[test]
fn a_long_sleep_ends_the_vm_and_a_short_one_only_pauses_the_watchdogs() {
    let long = Event::Slept(Millis(300_001));
    let mut slot = running();
    assert_eq!(slot.on(s(400), long), [Effect::StopVm { grace: Millis::ZERO }]);
    assert_eq!(
        slot.on(s(401), Event::VmExited),
        [Effect::Unregister, Effect::Release, ended(End::HostSlept, true)]
    );
    let mut slot = idle();
    assert_eq!(slot.on(s(400), long), [Effect::StopVm { grace: Millis::ZERO }]);
    let mut slot = starting();
    assert_eq!(slot.on(s(400), long), [Effect::KillVm]);
    assert_eq!(
        slot.on(s(401), Event::VmExited),
        [Effect::Unregister, Effect::Release, ended(End::HostSlept, false)]
    );
    let mut slot = finishing();
    assert_eq!(slot.on(s(400), long), [Effect::StopVm { grace: Millis::ZERO }]);
    assert_eq!(
        slot.on(s(401), Event::VmExited),
        [Effect::Unregister, Effect::Release, ended(End::Ok, true)]
    );
    // Exactly five minutes is not longer than five minutes. Every watchdog is due that much later: the guest
    // slept too.
    let mut slot = running();
    let before = slot.timers(s(3));
    assert_eq!(slot.on(s(303), Event::Slept(s(300))), []);
    assert_eq!((slot.phase(), slot.timers(s(303))), (Phase::Running, before));
    assert_eq!(slot.on(s(362), Event::Tick), []);
    assert_eq!(
        slot.on(s(363), Event::Tick),
        [Effect::KillVm],
        "sixty waking seconds without a heartbeat"
    );
    for mut slot in [starting(), idle(), finishing()] {
        let (phase, before) = (slot.phase(), slot.timers(s(4)));
        assert_eq!(slot.on(s(104), Event::Slept(s(100))), []);
        assert_eq!((slot.phase(), slot.timers(s(104))), (phase, before));
    }
}

#[test]
fn messages_that_come_twice_late_or_not_at_all() {
    // Twice.
    let mut slot = running();
    for event in [
        Event::Assigned,
        Event::VmReady,
        Event::JobStarted,
        Event::Lost,
        Event::Expired,
    ] {
        assert_eq!(slot.on(s(4), event), [], "{event:?}");
        assert_eq!((slot.phase(), slot.holds_memory()), (Phase::Running, true));
    }
    let mut slot = finishing();
    let timers = slot.timers(s(4));
    for event in [
        Event::JobCompleted,
        Event::JobStarted,
        Event::Heartbeat,
        Event::VmReady,
    ] {
        assert_eq!(slot.on(s(60), event), []);
        assert_eq!(
            (slot.phase(), slot.timers(s(4))),
            (Phase::Finishing, timers.clone())
        );
    }
    // Not at all: the job started, so the VM was ready; the job ended, so it had started.
    let mut slot = starting();
    assert_eq!(slot.on(s(2), Event::JobStarted), [Effect::StartSucceeded]);
    assert_eq!(slot.phase(), Phase::Running);
    let mut slot = idle();
    assert_eq!(slot.on(s(3), Event::JobCompleted), []);
    assert_eq!(
        slot.on(s(4), Event::VmExited),
        [Effect::Unregister, Effect::Release, ended(End::Ok, true)]
    );
    // After the end nothing does anything.
    let (mut slot, _) = after(&[Event::Assigned, Event::VmExited, Event::VmExited]);
    let all = [
        Event::Assigned,
        Event::Lost,
        Event::Expired,
        Event::VmReady,
        Event::JobStarted,
        Event::Heartbeat,
        Event::OutOfMemory,
        Event::JobCompleted,
        Event::VmExited,
        MEMORY,
        DISK,
        Event::Stop,
        Event::Slept(s(1000)),
        Event::Tick,
    ];
    let over = slot.clone();
    for event in all {
        assert_eq!(slot.on(s(1_000_000), event), [], "{event:?}");
        assert_eq!(slot, over);
    }
}

#[test]
fn a_restart_goes_on_with_a_live_vm_and_cleans_up_after_a_dead_one() {
    let limits = Limits::default();
    let saved = |slot: &Slot| slot.saved();
    // Alive: the same phase, the watchdogs from now.
    for (slot, timers) in [
        (starting(), vec![(Timer::Start, s(20))]),
        (idle(), vec![(Timer::Idle, s(120)), (Timer::Heartbeat, s(60))]),
        (
            running(),
            vec![(Timer::Heartbeat, s(60)), (Timer::Job, s(21_600))],
        ),
        (finishing(), vec![(Timer::Finish, s(120))]),
    ] {
        let (restored, effects) = Slot::restore(limits, s(900), saved(&slot), true);
        assert_eq!(effects, [], "{:?}", slot.phase());
        assert_eq!((restored.phase(), restored.holds_memory()), (slot.phase(), true));
        assert_eq!(restored.timers(s(900)), timers);
        assert_eq!(restored.saved(), slot.saved());
    }
    // Dead: unregister, report, and no memory to give back, because the new budget never counted it.
    for (slot, effects) in [
        (Slot::reserved(limits), vec![ended(End::Lost, false)]),
        (
            starting(),
            vec![
                Effect::StartFailed,
                Effect::Unregister,
                ended(End::StartFailed, false),
            ],
        ),
        (idle(), vec![Effect::Unregister, ended(End::GuestLost, false)]),
        (running(), vec![Effect::Unregister, ended(End::GuestLost, true)]),
        (finishing(), vec![Effect::Unregister, ended(End::Ok, true)]),
    ] {
        let (restored, done) = Slot::restore(limits, s(900), saved(&slot), false);
        assert_eq!(done, effects, "{:?}", slot.phase());
        assert_eq!(
            (restored.phase(), restored.holds_memory()),
            (Phase::Released, false)
        );
    }
    // A job with an out-of-memory stop keeps it over the restart.
    let mut slot = running();
    slot.on(s(4), Event::OutOfMemory);
    let (mut restored, _) = Slot::restore(limits, s(900), saved(&slot), true);
    restored.on(s(901), Event::JobCompleted);
    assert_eq!(
        restored.on(s(902), Event::VmExited),
        [Effect::Unregister, Effect::Release, ended(End::Oom, true)]
    );
    // Being stopped: asked again. Being killed, or waiting for a second start that cannot come: killed.
    let mut stopping = running();
    stopping.on(s(4), MEMORY);
    let (mut restored, effects) = Slot::restore(limits, s(900), saved(&stopping), true);
    assert_eq!(effects, [Effect::StopVm { grace: Millis::ZERO }]);
    assert_eq!(
        restored.on(s(901), Event::VmExited),
        [
            Effect::Unregister,
            Effect::Release,
            ended(End::EvictedMemory, true)
        ]
    );
    let mut retrying = starting();
    retrying.on(s(21), Event::Tick);
    let (mut restored, effects) = Slot::restore(limits, s(900), saved(&retrying), true);
    assert_eq!(
        (effects, restored.phase()),
        (vec![Effect::KillVm], Phase::Killing)
    );
    assert_eq!(
        restored.on(s(901), Event::VmExited),
        [
            Effect::Unregister,
            Effect::Release,
            ended(End::StartFailed, false)
        ]
    );
    let (restored, effects) = Slot::restore(limits, s(900), saved(&retrying), false);
    assert_eq!(
        (effects, restored.phase()),
        (
            vec![Effect::Unregister, ended(End::StartFailed, false)],
            Phase::Released
        )
    );
    // What is on disk may be wrong: a VM for a reservation is killed, a released slot stays released.
    let odd = Saved {
        phase: Phase::Reserved,
        starts: 0,
        end: None,
        oom: false,
        ran_job: false,
    };
    let (mut restored, effects) = Slot::restore(limits, s(900), odd, true);
    assert_eq!(
        (effects, restored.phase()),
        (vec![Effect::KillVm], Phase::Killing)
    );
    assert_eq!(
        restored.on(s(901), Event::VmExited),
        [Effect::Unregister, Effect::Release, ended(End::GuestLost, false)]
    );
    for alive in [true, false] {
        let (restored, effects) = Slot::restore(
            limits,
            s(900),
            Saved {
                phase: Phase::Released,
                ..odd
            },
            alive,
        );
        assert_eq!(
            (effects, restored.phase(), restored.holds_memory()),
            (vec![], Phase::Released, false)
        );
    }
}

#[test]
fn the_ends_have_the_consoles_names() {
    let names = [
        End::Ok,
        End::Oom,
        End::EvictedMemory,
        End::EvictedDisk,
        End::Timeout,
        End::GuestLost,
        End::HostSlept,
        End::Stopped,
        End::Lost,
        End::StartFailed,
        End::NoJob,
    ]
    .map(End::name);
    assert_eq!(
        names,
        [
            "ok",
            "oom",
            "evicted-memory",
            "evicted-disk",
            "timeout",
            "guest-lost",
            "host-slept",
            "stopped",
            "lost",
            "start-failed",
            "no-job"
        ]
    );
    let limits = Limits::default();
    assert_eq!(
        (limits.start, limits.starts, limits.idle, limits.heartbeat),
        (s(20), 2, s(120), s(60))
    );
    assert_eq!(
        (limits.job, limits.finish, limits.grace, limits.stop),
        (s(21_600), s(120), s(20), s(10))
    );
    assert_eq!((limits.kill_again, limits.long_sleep), (s(10), s(300)));
}

#[test]
fn a_restored_slot_never_starts_a_second_vm() {
    // What a second VM needs to register was never on disk, so a restored VM that does not get ready ends the
    // slot, whether its process dies or its watchdog runs out.
    let limits = Limits::default();
    let (mut restored, _) = Slot::restore(limits, s(900), starting().saved(), true);
    assert_eq!(
        restored.on(s(901), Event::VmExited),
        [
            Effect::StartFailed,
            Effect::Unregister,
            Effect::Release,
            ended(End::StartFailed, false)
        ]
    );
    let (mut restored, _) = Slot::restore(limits, s(900), starting().saved(), true);
    assert_eq!(
        restored.on(s(921), Event::Tick),
        [Effect::StartFailed, Effect::KillVm]
    );
    assert_eq!(restored.phase(), Phase::Killing);
    assert_eq!(
        restored.on(s(922), Event::VmExited),
        [
            Effect::Unregister,
            Effect::Release,
            ended(End::StartFailed, false)
        ]
    );
}
