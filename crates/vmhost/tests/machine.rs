//! A job's state machine against the Runner's waits and the ways a job goes wrong.

use cf_guest_proto::{GuestEnd, GuestMessage, OomEvent, PROTOCOL, Process, StopReason, Summary, Usage};
use cf_vmhost::{Action, End, Event, Limits, Machine, Stage};
use proptest::prelude::*;
use std::time::Duration;

fn s(seconds: u64) -> Duration {
    Duration::from_secs(seconds)
}

fn limits() -> Limits {
    Limits::for_deadline(s(6 * 3600))
}

fn usage(memory: u64) -> Usage {
    Usage {
        memory_bytes: memory,
        memory_peak_bytes: memory,
        cpu_usec: 1,
        disk_bytes: 2,
        pids: 3,
    }
}

fn summary(end: GuestEnd, exit_code: Option<i32>, oom: bool) -> Summary {
    let oom = oom.then(|| OomEvent {
        process: Process {
            pid: 9,
            name: "node".into(),
            memory_bytes: 1,
        },
        limit_bytes: 2,
        largest: vec![],
    });
    Summary {
        end,
        exit_code,
        signal: None,
        usage: usage(5),
        disk_peak_bytes: 7,
        oom,
        wall_ms: 11,
    }
}

fn exited(code: i32) -> Event {
    Event::Guest(GuestMessage::Exited(summary(GuestEnd::Exited, Some(code), false)))
}

fn hello() -> Event {
    Event::Guest(GuestMessage::Hello {
        protocol: PROTOCOL,
        kernel: "6.18".into(),
    })
}

fn ready() -> Event {
    Event::Guest(GuestMessage::Ready { boot_ms: 700 })
}

/// A machine whose workload started at second 1.
fn running(limits: Limits) -> Machine {
    let (mut machine, first) = Machine::new(limits, s(0));
    assert_eq!(first, [Action::Publish, Action::Boot]);
    assert_eq!(machine.handle(Event::Connected, s(1)), [Action::SendStart]);
    assert_eq!(machine.handle(hello(), s(1)), []);
    assert_eq!(machine.handle(ready(), s(1)), [Action::Publish]);
    assert_eq!(machine.stage(), Stage::Running);
    machine
}

fn end_of(machine: &Machine) -> Option<End> {
    machine.ended(0).map(|ended| ended.end)
}

#[test]
fn a_job_that_runs_and_ends_is_ok_and_waits_for_its_machine() {
    let mut machine = running(limits());
    assert_eq!(machine.boot_ms(), Some(1000));
    assert_eq!(
        machine.handle(Event::Guest(GuestMessage::Heartbeat(usage(10))), s(3)),
        []
    );
    assert_eq!(machine.usage(), Some(&usage(10)));
    assert_eq!(
        machine.handle(exited(0), s(5)),
        [Action::CloseChannel, Action::Publish]
    );
    assert_eq!((machine.stage(), end_of(&machine)), (Stage::Done, Some(End::Ok)));
    // We closed the channel ourselves; that is no lost guest.
    assert_eq!(machine.handle(Event::ChannelClosed, s(5)), []);
    assert_eq!(
        machine.handle(Event::VmExited("the guest powered off".into()), s(6)),
        [Action::Finish]
    );
    assert_eq!((machine.stage(), end_of(&machine)), (Stage::Ended, Some(End::Ok)));
    assert_eq!(machine.ended(42).unwrap().summary.unwrap().disk_peak_bytes, 7);
    assert_eq!(machine.handle(Event::Tick, s(9999)), []);
}

#[test]
fn the_end_is_the_stop_before_the_memory_before_the_exit_code() {
    for (given, expected) in [
        (summary(GuestEnd::Exited, Some(0), false), End::Ok),
        (summary(GuestEnd::Exited, Some(1), false), End::Failed),
        (summary(GuestEnd::Exited, None, false), End::Failed),
        (summary(GuestEnd::Exited, Some(137), true), End::Oom),
        (summary(GuestEnd::Exited, Some(0), true), End::Oom),
        (
            summary(GuestEnd::Stopped(StopReason::EvictedMemory), Some(137), true),
            End::EvictedMemory,
        ),
        (
            summary(GuestEnd::Stopped(StopReason::EvictedDisk), Some(0), false),
            End::EvictedDisk,
        ),
        (
            summary(GuestEnd::Stopped(StopReason::Timeout), None, false),
            End::Timeout,
        ),
        (
            summary(GuestEnd::Stopped(StopReason::HostSlept), None, false),
            End::HostSlept,
        ),
        (
            summary(GuestEnd::Stopped(StopReason::Cancelled), Some(143), false),
            End::Cancelled,
        ),
    ] {
        assert_eq!(End::of_summary(&given), expected, "{given:?}");
    }
}

#[test]
fn a_guest_not_ready_in_20_seconds_is_booted_once_more_and_then_given_up() {
    let (mut machine, _) = Machine::new(limits(), s(0));
    assert_eq!(machine.handle(Event::Connected, s(2)), [Action::SendStart]);
    assert_eq!(machine.handle(Event::Tick, Duration::from_millis(19_999)), []);
    assert_eq!(
        machine.handle(Event::Tick, s(20)),
        [Action::Boot, Action::Publish]
    );
    assert_eq!((machine.stage(), machine.boots()), (Stage::Booting, 2));
    // The second machine has its own 20 seconds.
    assert_eq!(machine.handle(Event::Tick, s(39)), []);
    assert_eq!(
        machine.handle(Event::Tick, s(40)),
        [Action::StopVm, Action::Publish]
    );
    assert_eq!(
        (machine.stage(), end_of(&machine)),
        (Stage::Killing, Some(End::BootFailed))
    );
    assert_eq!(
        machine.handle(Event::VmExited("stopped by the host".into()), s(41)),
        [Action::Finish]
    );
    assert_eq!(end_of(&machine), Some(End::BootFailed));
}

#[test]
fn the_second_boot_can_succeed_and_its_boot_time_is_its_own() {
    let (mut machine, _) = Machine::new(limits(), s(0));
    assert_eq!(
        machine.handle(Event::VmExited("crashed".into()), s(3)),
        [Action::Boot, Action::Publish]
    );
    machine.handle(Event::Connected, s(4));
    assert_eq!(machine.handle(ready(), s(5)), [Action::Publish]);
    assert_eq!((machine.boots(), machine.boot_ms()), (2, Some(2000)));
}

#[test]
fn a_machine_that_is_gone_at_the_last_try_ends_the_job_at_once() {
    let (mut machine, _) = Machine::new(limits(), s(0));
    assert_eq!(
        machine.handle(Event::VmExited("crashed".into()), s(3)),
        [Action::Boot, Action::Publish]
    );
    assert_eq!(
        machine.handle(Event::VmExited("crashed again".into()), s(5)),
        [Action::Finish]
    );
    let ended = machine.ended(0).unwrap();
    assert_eq!(
        (ended.end, ended.message.as_deref()),
        (End::BootFailed, Some("crashed again"))
    );
}

#[test]
fn a_machine_that_cannot_start_twice_ends_at_once_with_nothing_to_stop() {
    let (mut machine, _) = Machine::new(limits(), s(0));
    assert_eq!(
        machine.handle(Event::StartFailed("no memory".into()), s(0)),
        [Action::Boot, Action::Publish]
    );
    assert_eq!(
        machine.handle(Event::StartFailed("no memory".into()), s(0)),
        [Action::Finish]
    );
    let ended = machine.ended(0).unwrap();
    assert_eq!(
        (ended.end, ended.message.as_deref()),
        (End::BootFailed, Some("no memory"))
    );
}

#[test]
fn a_guest_that_says_why_it_failed_or_speaks_another_protocol_is_not_booted_again() {
    let (mut machine, _) = Machine::new(limits(), s(0));
    let failed = Event::Guest(GuestMessage::Failed {
        stage: "network".into(),
        message: "no address".into(),
    });
    assert_eq!(machine.handle(failed, s(3)), [Action::StopVm, Action::Publish]);
    assert_eq!(
        machine.ended(0).unwrap().message.unwrap(),
        "the guest failed at network: no address"
    );
    assert_eq!((machine.boots(), end_of(&machine)), (1, Some(End::BootFailed)));

    let (mut machine, _) = Machine::new(limits(), s(0));
    let other = Event::Guest(GuestMessage::Hello {
        protocol: PROTOCOL + 1,
        kernel: String::new(),
    });
    assert_eq!(machine.handle(other, s(1)), [Action::StopVm, Action::Publish]);
    assert_eq!(end_of(&machine), Some(End::BootFailed));
}

#[test]
fn a_guest_silent_for_60_seconds_is_lost_and_a_heartbeat_keeps_it() {
    let mut machine = running(limits());
    assert_eq!(machine.handle(Event::Tick, s(60)), []);
    machine.handle(Event::Guest(GuestMessage::Heartbeat(usage(1))), s(60));
    assert_eq!(machine.handle(Event::Tick, s(119)), []);
    assert_eq!(
        machine.handle(Event::Tick, s(120)),
        [Action::StopVm, Action::Publish]
    );
    assert_eq!(
        (machine.stage(), end_of(&machine)),
        (Stage::Killing, Some(End::GuestLost))
    );
    // A machine that does not obey the stop is left behind after ten seconds.
    assert_eq!(machine.handle(Event::Tick, s(129)), []);
    assert_eq!(machine.handle(Event::Tick, s(130)), [Action::Finish]);
    assert_eq!(end_of(&machine), Some(End::GuestLost));
}

#[test]
fn a_machine_or_channel_that_goes_mid_job_is_a_lost_guest() {
    let mut machine = running(limits());
    assert_eq!(
        machine.handle(Event::VmExited("the process died".into()), s(9)),
        [Action::Finish]
    );
    assert_eq!(end_of(&machine), Some(End::GuestLost));

    let mut machine = running(limits());
    assert_eq!(
        machine.handle(Event::ChannelClosed, s(9)),
        [Action::StopVm, Action::Publish]
    );
    assert_eq!(end_of(&machine), Some(End::GuestLost));
    // A summary that arrives after the job was given up does not rewrite its end.
    assert_eq!(machine.handle(exited(0), s(9)), []);
    assert_eq!(end_of(&machine), Some(End::GuestLost));
}

#[test]
fn the_deadline_counts_from_the_workloads_start_and_gives_the_job_20_seconds() {
    let mut machine = running(Limits::for_deadline(s(100)));
    machine.handle(Event::Guest(GuestMessage::Heartbeat(usage(1))), s(100));
    assert_eq!(machine.handle(Event::Tick, s(100)), []);
    assert_eq!(
        machine.handle(Event::Tick, s(101)),
        [
            Action::SendStop {
                reason: StopReason::Timeout,
                grace: s(20)
            },
            Action::Publish
        ]
    );
    assert_eq!(machine.stage(), Stage::Stopping);
    machine.handle(Event::Guest(GuestMessage::Heartbeat(usage(1))), s(120));
    // 20 seconds of grace and 10 for the guest to say how it went.
    assert_eq!(machine.handle(Event::Tick, s(130)), []);
    assert_eq!(
        machine.handle(Event::Tick, s(131)),
        [Action::StopVm, Action::Publish]
    );
    assert_eq!(end_of(&machine), Some(End::Timeout));
}

#[test]
fn a_stop_keeps_its_first_reason_and_a_second_one_only_makes_it_sooner() {
    let mut machine = running(limits());
    let evict = Event::StopAsked {
        reason: StopReason::EvictedMemory,
        grace: s(20),
    };
    assert_eq!(
        machine.handle(evict, s(10)),
        [
            Action::SendStop {
                reason: StopReason::EvictedMemory,
                grace: s(20)
            },
            Action::Publish
        ]
    );
    let cancel = Event::StopAsked {
        reason: StopReason::Cancelled,
        grace: s(0),
    };
    assert_eq!(
        machine.handle(cancel, s(11)),
        [
            Action::SendStop {
                reason: StopReason::Cancelled,
                grace: s(0)
            },
            Action::Publish
        ]
    );
    assert_eq!(machine.handle(Event::Tick, s(20)), []);
    assert_eq!(
        machine.handle(Event::Tick, s(21)),
        [Action::StopVm, Action::Publish]
    );
    assert_eq!(end_of(&machine), Some(End::EvictedMemory));

    // The guest's own account wins while it comes in time.
    let mut machine = running(limits());
    machine.handle(
        Event::StopAsked {
            reason: StopReason::EvictedDisk,
            grace: s(20),
        },
        s(10),
    );
    let told = Event::Guest(GuestMessage::Exited(summary(
        GuestEnd::Stopped(StopReason::EvictedDisk),
        Some(143),
        false,
    )));
    assert_eq!(
        machine.handle(told, s(12)),
        [Action::CloseChannel, Action::Publish]
    );
    assert_eq!(end_of(&machine), Some(End::EvictedDisk));

    // A machine that dies while it is being stopped ends as what it was stopped for.
    let mut machine = running(limits());
    machine.handle(
        Event::StopAsked {
            reason: StopReason::HostSlept,
            grace: s(20),
        },
        s(10),
    );
    assert_eq!(
        machine.handle(Event::VmExited("gone".into()), s(11)),
        [Action::Finish]
    );
    assert_eq!(end_of(&machine), Some(End::HostSlept));
}

#[test]
fn a_stop_during_the_boot_ends_the_job_without_another_boot() {
    let (mut machine, _) = Machine::new(limits(), s(0));
    assert_eq!(
        machine.handle(
            Event::StopAsked {
                reason: StopReason::Cancelled,
                grace: s(20)
            },
            s(1)
        ),
        [Action::StopVm, Action::Publish]
    );
    assert_eq!(
        machine.handle(Event::VmExited("stopped".into()), s(2)),
        [Action::Finish]
    );
    assert_eq!((machine.boots(), end_of(&machine)), (1, Some(End::Cancelled)));
}

#[test]
fn a_machine_still_up_two_minutes_after_the_job_is_stopped_and_the_end_stays() {
    let mut machine = running(limits());
    machine.handle(exited(3), s(10));
    assert_eq!(machine.handle(Event::Tick, s(129)), []);
    assert_eq!(
        machine.handle(Event::Tick, s(130)),
        [Action::StopVm, Action::Publish]
    );
    assert_eq!(machine.handle(Event::Tick, s(140)), [Action::Finish]);
    assert_eq!(end_of(&machine), Some(End::Failed));
}

#[test]
fn only_the_first_out_of_memory_stop_is_the_jobs_and_usage_is_written_every_ten_seconds() {
    let mut machine = running(limits());
    let oom = |name: &str| {
        GuestMessage::Oom(OomEvent {
            process: Process {
                pid: 1,
                name: name.into(),
                memory_bytes: 1,
            },
            limit_bytes: 2,
            largest: vec![],
        })
    };
    assert_eq!(machine.handle(Event::Guest(oom("node")), s(2)), [Action::Publish]);
    assert_eq!(machine.handle(Event::Guest(oom("esbuild")), s(3)), []);
    assert_eq!(machine.oom().unwrap().process.name, "node");
    let beat = || Event::Guest(GuestMessage::Heartbeat(usage(1)));
    assert_eq!(machine.handle(beat(), s(9)), []);
    assert_eq!(machine.handle(beat(), s(11)), [Action::Publish]);
    assert_eq!(machine.handle(beat(), s(13)), []);
    assert_eq!(machine.handle(beat(), s(21)), [Action::Publish]);
}

#[test]
fn a_guest_that_never_stops_talking_still_meets_every_wait() {
    // No tick at all: whoever feeds the machine may have nothing but the guest's chatter to feed it.
    let limits = Limits {
        publish: s(3600),
        ..Limits::for_deadline(s(5))
    };
    let beat = || Event::Guest(GuestMessage::Heartbeat(usage(1)));
    let mut machine = running(limits);
    assert_eq!(machine.handle(beat(), s(5)), []);
    assert_eq!(
        machine.handle(beat(), s(6)),
        [
            Action::SendStop {
                reason: StopReason::Timeout,
                grace: s(20)
            },
            Action::Publish
        ]
    );
    assert_eq!(machine.handle(beat(), s(35)), []);
    assert_eq!(machine.handle(beat(), s(36)), [Action::StopVm, Action::Publish]);
    assert_eq!(
        (machine.stage(), end_of(&machine)),
        (Stage::Killing, Some(End::Timeout))
    );
    assert_eq!(machine.handle(beat(), s(45)), []);
    assert_eq!(machine.handle(beat(), s(46)), [Action::Finish]);

    // A boot is no different: a guest that says hello for ever is not ready.
    let (mut machine, _) = Machine::new(limits, s(0));
    assert_eq!(machine.handle(hello(), s(19)), []);
    assert_eq!(machine.handle(hello(), s(20)), [Action::Boot, Action::Publish]);
    assert_eq!(machine.handle(Event::Connected, s(39)), [Action::SendStart]);
    assert_eq!(machine.handle(hello(), s(40)), [Action::StopVm, Action::Publish]);
    assert_eq!(end_of(&machine), Some(End::BootFailed));

    // And a stop asked of a job whose guest talks on: the grace, the slack, then by force.
    let mut machine = running(Limits {
        publish: s(3600),
        ..Limits::for_deadline(s(3600))
    });
    machine.handle(
        Event::StopAsked {
            reason: StopReason::EvictedMemory,
            grace: s(3),
        },
        s(10),
    );
    assert_eq!(machine.handle(beat(), s(22)), []);
    assert_eq!(machine.handle(beat(), s(23)), [Action::StopVm, Action::Publish]);
    assert_eq!(end_of(&machine), Some(End::EvictedMemory));
}

#[test]
fn the_longest_a_job_runs_and_a_vm_host_lives_is_what_the_limits_say() {
    let limits = Limits::for_deadline(s(5));
    // Told at the deadline, 20 s to end by itself, 10 s for the guest to say how it went.
    assert_eq!(limits.longest_run(), s(35));
    // Then a machine that stays up after its guest's account (120 s) and one that ignores the forced stop (10 s).
    assert_eq!(limits.longest_life(), s(165));

    // A job that ignores everything, as does its machine: ready at second 1.
    let ready_at = s(1);
    let mut machine = running(limits);
    let before = |wait: Duration| ready_at + wait - Duration::from_millis(1);
    assert_eq!(machine.handle(Event::Tick, before(s(5))), []);
    assert_eq!(machine.handle(Event::Tick, ready_at + s(5)).len(), 2);
    assert_eq!(machine.stage(), Stage::Stopping);
    assert_eq!(machine.handle(Event::Tick, before(limits.longest_run())), []);
    assert_eq!(
        machine.handle(Event::Tick, ready_at + limits.longest_run()),
        [Action::StopVm, Action::Publish]
    );
    assert_eq!(
        machine.handle(Event::Tick, before(limits.longest_run() + limits.kill)),
        []
    );
    assert_eq!(
        machine.handle(Event::Tick, ready_at + limits.longest_run() + limits.kill),
        [Action::Finish]
    );
    assert_eq!(end_of(&machine), Some(End::Timeout));

    // The slowest end there is: the guest gives its account at the last moment and its machine never goes.
    let mut machine = running(limits);
    machine.handle(Event::Tick, ready_at + s(5));
    let last_moment = before(limits.longest_run());
    assert_eq!(
        machine.handle(
            Event::Guest(GuestMessage::Exited(summary(
                GuestEnd::Stopped(StopReason::Timeout),
                None,
                false
            ))),
            last_moment
        ),
        [Action::CloseChannel, Action::Publish]
    );
    let mut now = last_moment;
    while machine.stage() != Stage::Ended {
        now += Duration::from_millis(500);
        machine.handle(Event::Tick, now);
    }
    assert!(
        now <= ready_at + limits.longest_life(),
        "ended {now:?} after the vm-host's start"
    );
    assert_eq!(end_of(&machine), Some(End::Timeout));
}

#[test]
fn a_machine_that_fills_its_console_is_stopped_and_never_booted_again() {
    let full = Event::ConsoleFull;
    let (mut machine, _) = Machine::new(limits(), s(0));
    assert_eq!(
        machine.handle(full.clone(), s(1)),
        [Action::StopVm, Action::Publish]
    );
    assert_eq!((machine.boots(), end_of(&machine)), (1, Some(End::BootFailed)));
    assert!(machine.ended(0).unwrap().message.unwrap().contains("console"));

    let mut machine = running(limits());
    assert_eq!(
        machine.handle(full.clone(), s(2)),
        [Action::StopVm, Action::Publish]
    );
    assert_eq!(
        (machine.stage(), end_of(&machine)),
        (Stage::Killing, Some(End::GuestLost))
    );
    assert_eq!(machine.handle(full.clone(), s(3)), []);

    // Under a stop the end is the stop's; after the job's own end it stays, and the machine is not waited for.
    let mut machine = running(limits());
    machine.handle(
        Event::StopAsked {
            reason: StopReason::EvictedDisk,
            grace: s(30),
        },
        s(2),
    );
    machine.handle(full.clone(), s(3));
    assert_eq!(end_of(&machine), Some(End::EvictedDisk));

    let mut machine = running(limits());
    machine.handle(exited(0), s(2));
    assert_eq!(machine.handle(full, s(3)), [Action::StopVm, Action::Publish]);
    assert_eq!(
        (machine.stage(), end_of(&machine)),
        (Stage::Killing, Some(End::Ok))
    );
}

fn any_event() -> impl Strategy<Value = Event> {
    let reason = prop_oneof![
        Just(StopReason::Cancelled),
        Just(StopReason::Timeout),
        Just(StopReason::EvictedMemory),
        Just(StopReason::EvictedDisk),
        Just(StopReason::HostSlept)
    ];
    prop_oneof![
        Just(Event::StartFailed("x".into())),
        Just(Event::Connected),
        Just(Event::ChannelClosed),
        Just(Event::VmExited("x".into())),
        Just(Event::Tick),
        Just(Event::ConsoleFull),
        Just(hello()),
        Just(ready()),
        Just(Event::Guest(GuestMessage::Heartbeat(usage(1)))),
        Just(Event::Guest(GuestMessage::Failed {
            stage: "x".into(),
            message: "y".into()
        })),
        (0..3i32, any::<bool>()).prop_map(|(code, oom)| Event::Guest(GuestMessage::Exited(summary(
            GuestEnd::Exited,
            Some(code),
            oom
        )))),
        (reason, 0..40u64).prop_map(|(reason, grace)| Event::StopAsked {
            reason,
            grace: s(grace)
        }),
    ]
}

proptest! {
    /// Whatever happens in whatever order: a job ends once, its end never changes, a machine is booted at most
    /// twice, and when the world goes silent the job still ends, in a time the waits add up to.
    #[test]
    fn every_job_ends_once_and_none_waits_forever(events in prop::collection::vec((any_event(), 0..30_000u64), 0..40)) {
        let limits = Limits::for_deadline(s(300));
        let (mut machine, first) = Machine::new(limits, s(0));
        let mut now = Duration::ZERO;
        let mut boots = first.iter().filter(|action| **action == Action::Boot).count();
        let mut finishes = 0;
        let mut end = None;
        let mut step = |machine: &mut Machine, event: Event, now: Duration| {
            let before = machine.stage();
            let actions = machine.handle(event, now);
            boots += actions.iter().filter(|action| **action == Action::Boot).count();
            finishes += actions.iter().filter(|action| **action == Action::Finish).count();
            prop_assert!(before != Stage::Ended || actions.is_empty(), "an ended job acted: {actions:?}");
            prop_assert!(before == Stage::Booting || !actions.contains(&Action::SendStart), "a task sent twice");
            prop_assert_eq!(actions.contains(&Action::Finish), machine.stage() == Stage::Ended && before != Stage::Ended);
            let ended = machine.ended(0).map(|ended| ended.end);
            if end.is_some() {
                prop_assert_eq!(ended, end, "the end changed");
            }
            end = ended;
            Ok(())
        };
        for (event, later) in events {
            now += Duration::from_millis(later);
            step(&mut machine, event, now)?;
        }
        // Silence: two boots of 20 s, the deadline, its grace and slack, a heartbeat's wait, two minutes of a
        // machine that stays up and ten seconds of one that will not stop.
        let longest = s(2 * 20 + 300 + 20 + 10 + 60 + 120 + 10 + 5);
        let silence_from = now;
        while machine.stage() != Stage::Ended {
            now += s(1);
            prop_assert!(now - silence_from <= longest, "still {:?} after {:?} of silence", machine.stage(), now - silence_from);
            step(&mut machine, Event::Tick, now)?;
        }
        prop_assert_eq!(finishes, 1);
        prop_assert!(boots <= 2, "{} boots", boots);
        prop_assert!(end.is_some());
    }
}
