//! The scaler's behaviour, ported case by case from the Go manager's scaler tests (HANDOVER.md has the mapping),
//! plus what the HTTP edge adds: what GitHub is left holding when this machine refuses or loses a job.

mod support;

use std::time::Duration;

use cf_daemon::testing::{pass, settings_for};
use cf_daemon::{Claim, EventKind, IDLE_GRACE, JobInfo, PROMISE_TTL, Runner, Slots, Ticket};
use cf_fake_github::httpd::Reply;
use cf_fake_github::{JobState, Options};
use cf_scaleset::{JobBase, JobCompleted, JobStarted, Message, Scaler as _, Statistics};
use support::*;

fn names(h: &Harness) -> Vec<String> {
    h.hosts.started().into_iter().map(|(n, _, _)| n).collect()
}

fn started(runner: &str, job: JobBase) -> Message {
    Message {
        started: vec![JobStarted {
            job,
            runner_name: runner.into(),
        }],
        ..Message::default()
    }
}

fn completed(runner: &str, result: &str, job: JobBase) -> JobCompleted {
    JobCompleted {
        job,
        result: result.into(),
        runner_name: runner.into(),
    }
}

#[tokio::test]
async fn initial_statistics_start_one_runner_per_assigned_job() {
    let mut h = harness(3).await;
    let capacity = h.scaler.scale(Some(&stats(2))).await.unwrap();
    let started = h.hosts.started();
    assert_eq!(started.len(), 2);
    for (name, runner_id, _) in &started {
        assert!(name.starts_with("mac-b-j"), "{name} does not say the machine");
        assert_ne!(*runner_id, 0);
    }
    let mut registered = h.fake.runners();
    registered.sort();
    let mut expected = names(&h);
    expected.sort();
    assert_eq!(registered, expected, "every machine has a registration at GitHub");
    assert_eq!(capacity, 3, "2 runners and 1 free slot");
    assert_eq!(h.slots.claimed(), 2);
}

#[tokio::test]
async fn redelivered_statistics_start_nothing_more() {
    let mut h = harness(3).await;
    h.scaler.scale(Some(&stats(2))).await.unwrap();
    h.scaler.scale(Some(&stats(2))).await.unwrap();
    h.scaler.scale(None).await.unwrap();
    assert_eq!(h.hosts.started().len(), 2);
    assert_eq!(h.fake.runners().len(), 2);
}

#[tokio::test]
async fn assigned_jobs_beyond_the_machine_wait_for_a_slot() {
    let mut h = harness(2).await;
    h.scaler.scale(Some(&stats(5))).await.unwrap();
    assert_eq!(h.hosts.started().len(), 2, "a machine of 2 starts 2");
    // An Actions runner finishes and its machine is gone: the next quiet poll starts a waiting job.
    let first = names(&h)[0].clone();
    h.hosts.gone(&first);
    h.scaler.scale(None).await.unwrap();
    assert_eq!(h.hosts.started().len(), 3);
}

#[tokio::test]
async fn acquires_only_what_fits() {
    let mut h = harness(2).await;
    let ids: Vec<i64> = (1..=3)
        .map(|i| {
            h.fake
                .queue_job(REPO, &["self-hosted", "cf-2g"], &format!("job {i}"))
        })
        .collect();
    let message = next_messages(&h, 3, 5).await;
    assert_eq!(message.available.len(), 3);
    h.scaler.scale(Some(&message)).await.unwrap();
    assert_eq!(h.fake.job_state(ids[0]), Some(JobState::Acquired(h.set.id)));
    assert_eq!(h.fake.job_state(ids[1]), Some(JobState::Acquired(h.set.id)));
    assert_eq!(
        h.fake.job_state(ids[2]),
        Some(JobState::Available),
        "the third waits at GitHub for a free machine"
    );
    assert!(h.slots.reserve(SIZE).is_err(), "acquired jobs hold their slots");
    assert_eq!(
        h.scaler.capacity(),
        2,
        "the promised slots count, and no free one is left to offer"
    );
}

#[tokio::test]
async fn jobs_offered_twice_in_one_message_are_acquired_once() {
    let mut h = harness(3).await;
    let id = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "job");
    let mut message = next_message(&h, 3).await;
    message.available.push(message.available[0].clone());
    h.scaler.scale(Some(&message)).await.unwrap();
    assert_eq!(h.fake.job_state(id), Some(JobState::Acquired(h.set.id)));
    assert_eq!(h.slots.used(), 1, "one job, one slot");
    assert!(
        h.slots.released().is_empty(),
        "no second slot was promised and given back"
    );
    assert_eq!(h.fake.count("acquirejobs"), 1);
}

#[tokio::test]
async fn jobs_taken_elsewhere_free_their_slots() {
    let mut h = harness(3).await;
    let ids: Vec<i64> = (1..=3)
        .map(|i| {
            h.fake
                .queue_job(REPO, &["self-hosted", "cf-2g"], &format!("job {i}"))
        })
        .collect();
    let message = next_messages(&h, 3, 3).await;
    // Another machine's Actions runner takes two of them between the offer and the acquire.
    h.fake.take_job(ids[1]);
    h.fake.take_job(ids[2]);
    h.scaler.scale(Some(&message)).await.unwrap();
    assert_eq!(h.fake.job_state(ids[0]), Some(JobState::Acquired(h.set.id)));
    assert_eq!(h.slots.used(), 1, "only the job this machine got holds a slot");
    assert_eq!(h.slots.released().len(), 2);
}

#[tokio::test]
async fn an_acquire_failure_gives_the_slots_back_and_fails_the_poll() {
    let mut h = harness(2).await;
    for i in 1..=2 {
        h.fake
            .queue_job(REPO, &["self-hosted", "cf-2g"], &format!("job {i}"));
    }
    let message = next_messages(&h, 2, 2).await;
    h.fake.fail("acquirejobs", Reply::text(503, "down"), 10);
    let err = h
        .scaler
        .scale(Some(&message))
        .await
        .expect_err("the message must come back");
    assert_eq!(err.status(), Some(503));
    assert_eq!(h.slots.used(), 0, "slots of a failed acquire are free again");
}

#[tokio::test]
async fn a_paused_machine_acquires_nothing_and_an_assigned_job_waits_in_line() {
    let mut h = harness(2).await;
    h.slots.set_open(false);
    let id = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "job");
    let message = next_message(&h, 1).await;
    h.scaler.scale(Some(&message)).await.unwrap();
    assert_eq!(
        h.fake.count("acquirejobs"),
        0,
        "a closed machine does not even ask"
    );
    assert_eq!(
        h.fake.job_state(id),
        Some(JobState::Available),
        "the refused job stays offered at GitHub"
    );
    assert_eq!(h.slots.used(), 0);
    // A job GitHub assigned all the same holds a place in line: a hold keeps every job out.
    let capacity = h.scaler.scale(Some(&stats(1))).await.unwrap();
    assert!(
        h.hosts.started().is_empty(),
        "a machine was started on a paused Mac"
    );
    assert_eq!(h.machines.snapshot().waiting, 1);
    assert_eq!(capacity, 1, "the job in line counts, no free slot is offered");
    // Asked again, the same job does not take a second place.
    h.scaler.scale(None).await.unwrap();
    assert_eq!(h.machines.snapshot().waiting, 1);
    h.slots.set_open(true);
    let nudges = h.parts.nudges.load(std::sync::atomic::Ordering::Relaxed);
    pass(&h.machines, 1);
    assert_eq!(
        h.parts.nudges.load(std::sync::atomic::Ordering::Relaxed),
        nudges + 1,
        "the scalers are told that the job's turn came"
    );
    h.scaler.scale(None).await.unwrap();
    assert_eq!(h.hosts.started().len(), 1);
    assert_eq!((h.machines.snapshot().waiting, h.slots.used()), (0, 1));
}

#[tokio::test]
async fn a_mac_someone_works_on_acquires_nothing_but_runs_what_it_has() {
    use cf_budget::{Mode, Settings};
    let mut settings = Settings::for_host(cf_daemon::testing::TEST_MAC, Mode::Workstation);
    settings.vcpus = [1; 4];
    // The fake Mac's person has only just stopped typing.
    let mut h = harness_on(settings, Options::default()).await;
    let id = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "job");
    let message = next_message(&h, 1).await;
    h.scaler.scale(Some(&message)).await.unwrap();
    assert_eq!(
        h.fake.count("acquirejobs"),
        0,
        "no job is asked for while the person is there"
    );
    assert_eq!(h.fake.job_state(id), Some(JobState::Available));
    assert_eq!(h.slots.state(), (false, "person-there".to_string()));
    // A job that is this Mac's already still gets its Actions runner.
    let capacity = h.scaler.scale(Some(&stats(1))).await.unwrap();
    assert_eq!(
        h.hosts.started().len(),
        1,
        "an assigned job was left without a runner"
    );
    assert_eq!(capacity, 1, "just its runner, no free slot offered");
    let held = h.machines.snapshot().budget.slots;
    assert_eq!(
        held.iter().map(|slot| slot.origin.name()).collect::<Vec<_>>(),
        ["assigned-in-use"]
    );
}

#[tokio::test]
async fn a_job_whose_size_the_settings_rule_out_is_said_to_never_fit_and_holds_nothing() {
    let mut settings = settings_for(2);
    // No room for even one machine of the scale set's size, whatever the Mac reads.
    settings.away.cap_bytes = cf_budget::GIB;
    settings.in_use = settings.away;
    let mut h = harness_on(settings, Options::default()).await;
    let never = h.scaler.never_handle();
    let id = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "job");
    let message = next_message(&h, 1).await;
    let capacity = h.scaler.scale(Some(&message)).await.unwrap();
    assert_eq!(
        h.fake.count("acquirejobs"),
        0,
        "a job that cannot fit is not asked for"
    );
    assert_eq!(h.fake.job_state(id), Some(JobState::Available));
    assert_eq!(capacity, 0);
    assert_eq!(*never.lock().unwrap(), None, "nothing is assigned yet");
    // GitHub assigns one all the same.
    h.scaler.scale(Some(&stats(1))).await.unwrap();
    h.scaler.scale(None).await.unwrap();
    assert!(h.hosts.started().is_empty());
    assert!(
        h.fake.runners().is_empty(),
        "an Actions runner was registered for a job that cannot run"
    );
    let snapshot = h.machines.snapshot();
    assert_eq!((snapshot.waiting, snapshot.budget.slots.len()), (0, 0));
    assert_eq!(never.lock().unwrap().as_deref(), Some("full"));
    // The job went away: the reason goes with it.
    h.scaler.scale(Some(&stats(0))).await.unwrap();
    assert_eq!(*never.lock().unwrap(), None);
}

#[tokio::test]
async fn a_failed_start_unregisters_and_frees_the_slot() {
    let mut h = harness(1).await;
    h.hosts.fail_start(1);
    let err = h
        .scaler
        .scale(Some(&stats(1)))
        .await
        .expect_err("a failed start fails the poll");
    assert!(err.to_string().contains("did not start"), "{err}");
    assert!(h.fake.runners().is_empty(), "the registration was left behind");
    assert_eq!(h.slots.used(), 0);
    h.scaler.scale(None).await.unwrap();
    assert_eq!(
        h.hosts.started().len(),
        1,
        "the slot of the failed start was given back"
    );
}

#[tokio::test]
async fn a_registration_failure_starts_no_machine() {
    let mut h = harness(1).await;
    h.fake.fail("generatejitconfig", Reply::text(500, "boom"), 10);
    let err = h
        .scaler
        .scale(Some(&stats(1)))
        .await
        .expect_err("a failed registration fails the poll");
    assert_eq!(err.status(), Some(500));
    assert!(h.hosts.started().is_empty());
    assert_eq!(h.slots.used(), 0, "slot held after a failed registration");
}

#[tokio::test]
async fn a_taken_runner_name_is_registered_again_under_another() {
    let mut h = harness(1).await;
    h.fake.fail("generatejitconfig", Reply::text(409, "exists"), 1);
    h.scaler.scale(Some(&stats(1))).await.unwrap();
    assert_eq!(h.hosts.started().len(), 1);
    assert_eq!(h.fake.runners().len(), 1);
}

#[tokio::test]
async fn a_completed_job_is_not_replaced_and_its_runner_is_left_to_exit() {
    let mut h = harness(2).await;
    h.scaler.scale(Some(&stats(1))).await.unwrap();
    let name = names(&h)[0].clone();
    let busy = Message {
        statistics: Some(Statistics {
            total_assigned_jobs: 1,
            total_running_jobs: 1,
            ..Default::default()
        }),
        ..started(&name, JobBase::default())
    };
    h.scaler.scale(Some(&busy)).await.unwrap();
    assert!(h.roster.get(&name).unwrap().busy);
    let done = Message {
        completed: vec![completed(&name, "succeeded", JobBase::default())],
        statistics: Some(Statistics::default()),
        ..Message::default()
    };
    h.clock.advance(IDLE_GRACE * 2);
    h.scaler.scale(Some(&done)).await.unwrap();
    assert_eq!(h.hosts.started().len(), 1, "one job, one runner");
    assert!(
        h.hosts.retired().is_empty(),
        "a finishing runner is not removed as surplus"
    );
    assert_eq!(h.fake.runners(), vec![name]);
}

#[tokio::test]
async fn surplus_runners_are_retired_after_the_grace_period() {
    let mut h = harness(4).await;
    h.scaler.scale(Some(&stats(3))).await.unwrap();
    let all = h.hosts.started();
    let busy = all[0].0.clone();
    h.roster.mark_busy(
        REPO,
        h.set.id,
        &busy,
        JobInfo {
            repo: REPO.into(),
            name: "j".into(),
            workflow: String::new(),
            run_id: 1,
            started: h.clock_now(),
        },
    );
    h.roster.adopt(Runner {
        name: "adopted".into(),
        handle: "adopted".into(),
        runner_id: 99,
        repo: REPO.into(),
        scale_set_id: h.set.id,
        ticket: None,
        started: h.clock_now() - Duration::from_secs(3600),
        busy: false,
        adopted: true,
        done: None,
        job: None,
    });
    // The two idle jobs were cancelled: only the busy Actions runner's job is assigned.
    h.scaler.scale(Some(&stats(1))).await.unwrap();
    assert!(
        h.hosts.retired().is_empty(),
        "retired runners still inside their grace period"
    );

    // Only a machine that came up asks for its removal.
    h.hosts.boot_all();
    h.clock.advance(IDLE_GRACE);
    h.scaler.scale(None).await.unwrap();
    let mut idle: Vec<String> = all.iter().skip(1).map(|(n, ..)| n.clone()).collect();
    idle.sort();
    let retired = |h: &Harness| {
        let mut retired = h.hosts.retired();
        retired.sort();
        retired
    };
    assert_eq!(
        retired(&h),
        idle,
        "the idle runners retire, the busy and the adopted one stay"
    );
    // Retiring is asking: nothing is stopped and nothing changes at GitHub until the sweep has GitHub's answer.
    assert_eq!(h.fake.runners().len(), 3);
    h.scaler.scale(None).await.unwrap();
    assert_eq!(
        retired(&h),
        idle,
        "a runner that retires is not retired again, and counts"
    );
    assert_eq!(h.slots.used(), 3, "a slot went back before its machine was gone");
    // GitHub removed one at the sweep's request; the other's removal GitHub has not answered.
    h.machines.answered(&idle[0], cf_daemon::Answer::Removed);
    for kept in [&busy, &"adopted".to_string(), &idle[1]] {
        assert!(h.roster.get(kept).is_some(), "{kept} must stay");
    }
    pass(&h.machines, 1);
    assert_eq!(
        h.slots.used(),
        2,
        "the retired runner's slot is back once its machine is gone"
    );
    assert!(
        h.slots.released().is_empty(),
        "the scaler gave back a slot a machine held"
    );
}

#[tokio::test]
async fn a_job_cancelled_after_its_runner_was_registered_leaves_a_surplus_that_goes() {
    let mut h = harness_with(
        2,
        cf_fake_github::Options {
            auto_assign: true,
            ..Default::default()
        },
    )
    .await;
    // GitHub hands a job out at its queue event, against room two polls showed it.
    h.fake.set_poll_timeout(std::time::Duration::from_millis(100));
    for _ in 0..2 {
        assert_eq!(h.session.poll(0, 1).await.expect("poll"), None);
    }
    let id = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "job");
    let assigned = next_message(&h, 1).await;
    assert_eq!(assigned.statistics.unwrap().assigned(), 1);
    h.scaler.scale(Some(&assigned)).await.unwrap();
    assert_eq!(h.hosts.started().len(), 1);
    // The job is cancelled before the Actions runner took it.
    h.fake.cancel_job(id);
    let cancelled = next_message(&h, 1).await;
    h.scaler.scale(Some(&cancelled)).await.unwrap();
    assert!(
        h.hosts.retired().is_empty(),
        "inside the grace period the runner may still be wanted"
    );
    // Only a machine that came up asks for its removal.
    h.hosts.boot_all();
    h.clock.advance(IDLE_GRACE);
    h.scaler.scale(None).await.unwrap();
    let name = h.hosts.started()[0].0.clone();
    assert_eq!(h.hosts.retired(), vec![name.clone()]);
    assert_eq!(
        h.machines
            .registrations_to_remove()
            .iter()
            .map(|r| r.runner.clone())
            .collect::<Vec<_>>(),
        vec![name.clone()],
        "the sweep removes its registration"
    );
    h.machines.answered(&name, cf_daemon::Answer::Removed);
    pass(&h.machines, 1);
    assert_eq!(h.slots.used(), 0);
}

#[tokio::test]
async fn job_events_name_the_repository_job_and_run() {
    let mut h = harness(2).await;
    h.scaler.scale(Some(&stats(1))).await.unwrap();
    let name = names(&h)[0].clone();
    let job = JobBase {
        owner_name: "org".into(),
        repository_name: "app".into(),
        job_display_name: "test (2/4)".into(),
        job_workflow_ref: "org/app/.github/workflows/ci.yml@refs/heads/main".into(),
        workflow_run_id: 77,
        event_name: "push".into(),
        ..JobBase::default()
    };
    h.scaler.scale(Some(&started(&name, job.clone()))).await.unwrap();
    let done = Message {
        completed: vec![
            completed(&name, "failed", job.clone()),
            completed("", "canceled", job),
        ],
        ..Message::default()
    };
    h.scaler.scale(Some(&done)).await.unwrap();
    let events = h.events.jobs();
    assert_eq!(
        events.len(),
        2,
        "started and completed, none for a job no runner of this machine had: {events:?}"
    );
    assert_eq!(
        (events[0].kind, events[0].runner.as_str(), events[0].repo.as_str()),
        (EventKind::Started, name.as_str(), REPO)
    );
    assert_eq!(
        (
            events[0].job.as_str(),
            events[0].run_id,
            events[0].event.as_str(),
            events[0].result.as_str()
        ),
        ("test (2/4)", 77, "push", "")
    );
    assert_eq!(
        (events[1].kind, events[1].result.as_str()),
        (EventKind::Completed, "failed")
    );
    let runner = h.roster.get(&name).unwrap();
    let job = runner.job.expect("the runner carries its job");
    assert_eq!(
        (job.repo.as_str(), job.name.as_str(), job.run_id),
        (REPO, "test (2/4)", 77)
    );
    assert!(runner.done.is_some());
    assert_eq!(
        h.scaler.statistics_handle().lock().unwrap().unwrap().assigned(),
        1,
        "the counts are the service's own: a completion without counts changes none of them"
    );
}

#[tokio::test]
async fn jobs_without_a_listener_still_run() {
    let mut h = harness(1).await;
    h.scaler.scale(Some(&stats(1))).await.unwrap();
    let name = names(&h)[0].clone();
    h.scaler
        .scale(Some(&started(&name, JobBase::default())))
        .await
        .unwrap();
    assert!(h.roster.get(&name).unwrap().busy);
}

#[tokio::test]
async fn a_job_that_ran_out_of_memory_is_reported_as_such() {
    let asked = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
    let mut h = harness(2).await;
    let log = asked.clone();
    h.scaler = h.scaler.with_out_of_memory_check(std::sync::Arc::new(move |r| {
        log.lock().unwrap().push(r.to_string());
        false
    }));
    h.scaler.scale(Some(&stats(2))).await.unwrap();
    let (killed, fine) = (names(&h)[0].clone(), names(&h)[1].clone());
    h.hosts.mark_out_of_memory(&killed);
    let job = JobBase {
        owner_name: "org".into(),
        repository_name: "app".into(),
        job_display_name: "build".into(),
        workflow_run_id: 38058951666,
        ..JobBase::default()
    };
    let done = Message {
        completed: vec![
            completed(&killed, "failed", job.clone()),
            completed(&fine, "failed", job.clone()),
            completed("", "canceled", job.clone()),
            completed("mac-b-j0000000", "failed", job),
        ],
        ..Message::default()
    };
    h.scaler.scale(Some(&done)).await.unwrap();
    let events = h.events.jobs();
    assert_eq!(events.len(), 3, "the cancelled job has no runner and no event");
    assert!(events[0].out_of_memory && events[0].runner == killed);
    assert!(!events[1].out_of_memory && !events[2].out_of_memory);
    let oom = h.events.out_of_memory();
    assert_eq!(oom.len(), 1);
    assert_eq!(
        (oom[0].runner.as_str(), oom[0].run_id, oom[0].job.as_str()),
        (killed.as_str(), 38058951666, "build")
    );
    assert_eq!(
        *asked.lock().unwrap(),
        vec![fine.clone(), "mac-b-j0000000".to_string()],
        "a machine that still runs cannot rule it out and one that is gone cannot say: whoever watches machines go is asked"
    );
}

#[tokio::test]
async fn a_host_that_does_not_answer_is_not_out_of_memory() {
    let mut h = harness(1).await;
    h.scaler.scale(Some(&stats(1))).await.unwrap();
    let name = names(&h)[0].clone();
    h.hosts.mark_out_of_memory(&name);
    h.hosts.leave_out_of_memory_unanswered();
    let done = Message {
        completed: vec![completed(&name, "failed", JobBase::default())],
        ..Message::default()
    };
    h.scaler.scale(Some(&done)).await.unwrap();
    let events = h.events.jobs();
    assert_eq!(events.len(), 1);
    assert!(!events[0].out_of_memory);
    assert!(h.events.out_of_memory().is_empty());
}

#[tokio::test]
async fn a_job_whose_machine_is_gone_is_still_out_of_memory() {
    let mut h = harness(1).await;
    h.scaler = h
        .scaler
        .with_out_of_memory_check(std::sync::Arc::new(|r| r == "mac-b-gone"));
    let done = Message {
        completed: vec![
            completed("mac-b-gone", "failed", JobBase::default()),
            completed("mac-b-other", "failed", JobBase::default()),
        ],
        ..Message::default()
    };
    h.scaler.scale(Some(&done)).await.unwrap();
    let events = h.events.jobs();
    assert_eq!(events.len(), 2);
    assert!(events[0].out_of_memory && !events[1].out_of_memory);
    assert_eq!(h.events.out_of_memory().len(), 1);
}

#[tokio::test]
async fn a_runner_whose_job_is_done_is_never_retired_as_surplus() {
    let mut h = harness(3).await;
    h.scaler.scale(Some(&stats(2))).await.unwrap();
    let (finished, idle) = (names(&h)[0].clone(), names(&h)[1].clone());
    let done = Message {
        completed: vec![completed(&finished, "succeeded", JobBase::default())],
        statistics: Some(Statistics {
            total_assigned_jobs: 1,
            ..Statistics::default()
        }),
        ..Message::default()
    };
    h.scaler.scale(Some(&done)).await.unwrap();
    // Both assigned jobs are gone from the counts, long past the grace period.
    // Only a machine that came up asks for its removal.
    h.hosts.boot_all();
    h.clock.advance(IDLE_GRACE * 3);
    h.scaler.scale(Some(&stats(0))).await.unwrap();
    assert_eq!(
        h.hosts.retired(),
        vec![idle],
        "only the idle runner goes; the finished one exits by itself"
    );
    assert!(h.roster.get(&finished).is_some());
}

// added: what the scaler owes the machine's slots

/// One job offered and acquired, and the slot promised to it.
async fn acquired_job(h: &mut Harness) -> (i64, cf_daemon::Ticket) {
    let id = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "job");
    let message = next_message(h, 2).await;
    h.scaler.scale(Some(&message)).await.unwrap();
    assert_eq!(h.fake.job_state(id), Some(JobState::Acquired(h.set.id)));
    (id, *h.slots.held().last().expect("a promised slot"))
}

#[tokio::test]
async fn a_slot_promised_to_a_job_that_is_never_assigned_goes_back_after_two_minutes() {
    let mut h = harness(2).await;
    acquired_job(&mut h).await;
    assert_eq!(h.scaler.capacity(), 2, "the promise and one free slot");

    h.clock.advance(PROMISE_TTL - Duration::from_secs(5));
    assert_eq!(h.scaler.scale(None).await.unwrap(), 2);
    assert_eq!(h.slots.used(), 1, "the promise still stands");

    h.clock.advance(Duration::from_secs(10));
    assert_eq!(
        h.scaler.scale(None).await.unwrap(),
        1,
        "a promise nobody will claim is no longer offered to the service as room"
    );
    assert_eq!(h.slots.used(), 0);
    assert_eq!(h.slots.released().len(), 1);
    assert!(h.hosts.started().is_empty());
}

#[tokio::test]
async fn a_job_assigned_late_still_runs_in_the_slot_promised_to_it() {
    let mut h = harness(1).await;
    let (_, promised) = acquired_job(&mut h).await;
    // No poll came back for three minutes; the next one brings the assignment.
    h.clock.advance(PROMISE_TTL + Duration::from_secs(60));
    h.scaler.scale(Some(&stats(1))).await.unwrap();
    let started = h.hosts.started();
    assert_eq!(started.len(), 1);
    assert_eq!(
        started[0].2, promised,
        "the runner took another slot than the promised one"
    );
    assert!(h.slots.released().is_empty());
    assert_eq!(h.slots.used(), 1);
}

#[tokio::test]
async fn a_scaler_that_goes_away_gives_its_promises_back_and_leaves_its_runners_their_slots() {
    let mut h = harness(3).await;
    h.scaler.scale(Some(&stats(1))).await.unwrap();
    acquired_job(&mut h).await;
    assert_eq!(h.slots.held().len(), 2);
    let running = h.hosts.started()[0].2;
    drop(h.scaler);
    assert_eq!(
        h.slots.held(),
        vec![running],
        "a runner's slot is its machine's, not the scaler's"
    );
}

#[tokio::test]
async fn an_acquire_that_is_given_up_on_does_not_lose_the_slot_it_promised() {
    let mut h = harness(2).await;
    let id = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "job");
    let message = next_message(&h, 2).await;
    h.fake.fail("acquirejobs", Reply::Hang, 1);
    let given_up = tokio::time::timeout(Duration::from_millis(200), h.scaler.scale(Some(&message))).await;
    assert!(given_up.is_err(), "the acquire was to hang");
    assert_eq!(h.fake.job_state(id), Some(JobState::Available));
    assert_eq!(h.slots.used(), 1);
    assert_eq!(
        h.scaler.capacity(),
        2,
        "the scaler still knows the promise it made"
    );
    h.clock.advance(PROMISE_TTL);
    h.scaler.scale(None).await.unwrap();
    assert_eq!(
        h.slots.used(),
        0,
        "the slot of the abandoned acquire is lost to the machine"
    );
}

#[tokio::test]
async fn a_completed_job_without_new_counts_gets_no_second_runner() {
    let mut h = harness(3).await;
    h.scaler.scale(Some(&stats(2))).await.unwrap();
    let (first, second) = (names(&h)[0].clone(), names(&h)[1].clone());
    let done = |runner: &str| Message {
        completed: vec![completed(runner, "succeeded", JobBase::default())],
        ..Message::default()
    };

    h.scaler.scale(Some(&done(&first))).await.unwrap();
    assert_eq!(
        h.hosts.started().len(),
        2,
        "a runner started for a job that is over"
    );

    // The same completion again, one without an Actions runner and one of an Actions runner nobody knows take nothing
    // off: the second job keeps its Actions runner however long it waits.
    h.clock.advance(IDLE_GRACE + Duration::from_secs(1));
    for runner in [first.as_str(), "", "mac-b-stranger"] {
        h.scaler.scale(Some(&done(runner))).await.unwrap();
    }
    assert_eq!(h.hosts.started().len(), 2);
    assert!(
        h.hosts.retired().is_empty(),
        "the runner of a job still assigned was retired"
    );
    assert_eq!(
        h.roster.active(REPO, h.set.id),
        1,
        "the second job still has its runner"
    );

    h.scaler.scale(Some(&done(&second))).await.unwrap();
    assert_eq!(h.hosts.started().len(), 2);
    assert!(h.hosts.retired().is_empty());

    // Only the service's own counts bring new Actions runners.
    h.scaler.scale(Some(&stats(1))).await.unwrap();
    assert_eq!(h.hosts.started().len(), 3);
}

#[tokio::test]
async fn a_completion_after_counts_that_left_the_job_out_takes_nothing_more_off() {
    let mut h = harness(2).await;
    h.scaler.scale(Some(&stats(1))).await.unwrap();
    let name = names(&h)[0].clone();
    // The service's counts already left the job out; its completion comes after, without counts.
    h.scaler.scale(Some(&stats(0))).await.unwrap();
    let done = Message {
        completed: vec![completed(&name, "succeeded", JobBase::default())],
        ..Message::default()
    };
    h.scaler.scale(Some(&done)).await.unwrap();
    assert_eq!(h.hosts.started().len(), 1, "a count below zero started runners");
    h.scaler.scale(Some(&stats(1))).await.unwrap();
    assert_eq!(h.hosts.started().len(), 2);
}

#[tokio::test]
async fn a_message_that_names_a_runner_of_another_scale_set_leaves_that_runner_alone() {
    let mut h = harness(2).await;
    h.scaler.scale(Some(&stats(1))).await.unwrap();
    // Whoever may register Actions runners in this scale set's repository chooses their names, also the name of an
    // Actions runner this machine has for another repository.
    h.roster.add(Runner {
        name: "mac-b-theirs".into(),
        handle: "vm-theirs".into(),
        runner_id: 9,
        repo: REPO.into(),
        scale_set_id: h.set.id + 1,
        ticket: None,
        started: h.clock_now(),
        busy: false,
        adopted: false,
        done: None,
        job: None,
    });
    h.scaler
        .scale(Some(&started("mac-b-theirs", JobBase::default())))
        .await
        .unwrap();
    let done = Message {
        completed: vec![completed("mac-b-theirs", "succeeded", JobBase::default())],
        ..Message::default()
    };
    h.clock.advance(IDLE_GRACE + Duration::from_secs(1));
    h.scaler.scale(Some(&done)).await.unwrap();

    let theirs = h.roster.get("mac-b-theirs").unwrap();
    assert!(!theirs.busy && theirs.job.is_none(), "marked busy: {theirs:?}");
    assert_eq!(theirs.done, None, "marked done");
    assert!(
        h.hosts.retired().is_empty(),
        "the completion counted against this scale set's own job"
    );
}

#[tokio::test]
async fn a_host_that_does_not_answer_leaves_the_verdict_to_who_watched_the_machine() {
    let mut h = harness(2).await;
    h.scaler.scale(Some(&stats(2))).await.unwrap();
    let (killed, fine) = (names(&h)[0].clone(), names(&h)[1].clone());
    let watched = killed.clone();
    h.scaler = h
        .scaler
        .with_out_of_memory_check(std::sync::Arc::new(move |r| r == watched));
    h.hosts.leave_out_of_memory_unanswered();
    let done = Message {
        completed: vec![
            completed(&killed, "failed", JobBase::default()),
            completed(&fine, "failed", JobBase::default()),
        ],
        ..Message::default()
    };
    h.scaler.scale(Some(&done)).await.unwrap();
    let events = h.events.jobs();
    assert_eq!(
        (events[0].out_of_memory, events[1].out_of_memory),
        (true, false),
        "a job that ran out of memory was reported as an ordinary failure"
    );
    assert_eq!(h.events.out_of_memory().len(), 1);
}

#[tokio::test]
async fn a_surplus_machine_that_does_not_stop_keeps_its_slot() {
    let mut h = harness(2).await;
    h.scaler.scale(Some(&stats(1))).await.unwrap();
    let (name, _, ticket) = h.hosts.started()[0].clone();
    // Only a machine that came up asks for its removal.
    h.hosts.boot_all();
    h.clock.advance(IDLE_GRACE);
    h.scaler.scale(Some(&stats(0))).await.unwrap();
    assert_eq!(h.hosts.retired(), vec![name.clone()]);
    // GitHub removed its registration at the sweep's request; the machine is told to stop and still stands.
    h.machines.answered(&name, cf_daemon::Answer::Removed);
    assert_eq!(
        h.slots.held(),
        vec![ticket],
        "the slot of a machine that may still stand was given to the next job"
    );
    assert_eq!(h.roster.active(REPO, h.set.id), 0, "it waits for no job any more");
    assert!(
        h.roster.get(&name).is_some(),
        "whoever watches the machines still finds it"
    );

    // A new job gets its own Actions runner and slot beside it, and the watcher's word frees the old one.
    h.scaler.scale(Some(&stats(1))).await.unwrap();
    assert_eq!(h.hosts.started().len(), 2);
    assert_eq!(h.slots.used(), 2);
    h.hosts.gone(&name);
    assert_eq!(h.slots.used(), 1);
}

#[tokio::test]
async fn only_a_vm_host_that_cannot_say_is_worth_a_warning_about_memory() {
    let mut h = harness(2).await;
    h.scaler.scale(Some(&stats(2))).await.unwrap();
    let names = names(&h);
    let log = support::Log::start();
    // The usual order: GitHub's word of the end comes while the machine still stands.
    let done = Message {
        completed: vec![completed(&names[0], "succeeded", JobBase::default())],
        ..Message::default()
    };
    h.scaler.scale(Some(&done)).await.unwrap();
    assert_eq!(log.lines("out of memory"), Vec::<String>::new());
    h.hosts.leave_out_of_memory_unanswered();
    let done = Message {
        completed: vec![completed(&names[1], "succeeded", JobBase::default())],
        ..Message::default()
    };
    h.scaler.scale(Some(&done)).await.unwrap();
    assert_eq!(
        log.lines("out of memory"),
        [format!(
            "the job's vm-host could not say whether the job ran out of memory; what the Runner recorded of its \
             machine is used runner={} reason=the vm-host does not answer recorded=false",
            names[1]
        )]
    );
}

#[tokio::test]
async fn the_log_names_a_jobs_start_by_githubs_time_and_says_how_late_its_word_came() {
    let mut h = harness(2).await;
    h.scaler.scale(Some(&stats(2))).await.unwrap();
    let names = names(&h);
    let log = support::Log::start();
    let now = time::OffsetDateTime::from(cf_ghapp::Clock::now(&*h.clock));
    let job = JobBase {
        job_display_name: "build".into(),
        runner_assign_time: Some(now - time::Duration::seconds(41)),
        ..JobBase::default()
    };
    h.scaler.scale(Some(&started(&names[0], job))).await.unwrap();
    // GitHub names no time: the start counts as now.
    h.scaler
        .scale(Some(&started(&names[1], JobBase::default())))
        .await
        .unwrap();
    let at = |at: time::OffsetDateTime| cf_daemon::iso(at.unix_timestamp());
    assert_eq!(
        log.lines("job started"),
        [
            format!(
                "job started runner={} job=build at={} heard_after_s=41",
                names[0],
                at(now - time::Duration::seconds(41))
            ),
            format!(
                "job started runner={} job= at={} heard_after_s=0",
                names[1],
                at(now)
            ),
        ]
    );
}

// --- two repositories whose scale sets have the same number --------------------------------------------------------

/// An Actions runner this Mac has for another repository's scale set, which GitHub gave the number this one has:
/// GitHub counts scale sets within a repository. The Air and the mini, 2026-10-12: four repositories' scale sets
/// were number 6 on one and number 7 on the other.
fn of_another_repository(h: &Harness, name: &str, ticket: Option<Ticket>, age: Duration) -> Runner {
    Runner {
        name: name.into(),
        handle: name.into(),
        runner_id: 9,
        repo: "org/other".into(),
        scale_set_id: h.set.id,
        ticket,
        started: h.clock_now() - age,
        busy: ticket.is_some(),
        adopted: false,
        done: None,
        job: None,
    }
}

#[tokio::test]
async fn another_repositorys_runner_under_the_same_number_is_not_offered_to_github_as_room() {
    let mut h = harness(1).await;
    let Claim::Start(ticket) = h.machines.claim(SIZE, None) else {
        panic!("the Mac has a slot");
    };
    h.roster.add(of_another_repository(
        &h,
        "mac-b-theirs",
        Some(ticket),
        Duration::ZERO,
    ));
    let capacity = h.scaler.scale(Some(&stats(0))).await.unwrap();
    assert_eq!(
        capacity, 0,
        "the Mac is full: GitHub must not be told this scale set takes a job"
    );
}

#[tokio::test]
async fn a_job_assigned_while_another_repository_has_a_runner_under_the_same_number_gets_its_own() {
    let mut h = harness(2).await;
    let Claim::Start(ticket) = h.machines.claim(SIZE, None) else {
        panic!("the Mac has a slot");
    };
    h.roster.add(of_another_repository(
        &h,
        "mac-b-theirs",
        Some(ticket),
        Duration::ZERO,
    ));
    let capacity = h.scaler.scale(Some(&stats(1))).await.unwrap();
    assert_eq!(
        h.hosts.started().len(),
        1,
        "the assigned job has no Actions runner"
    );
    assert_eq!(capacity, 1, "its own Actions runner, and no room beside the two");
    assert_eq!(h.roster.active(REPO, h.set.id), 1);
    assert_eq!(h.roster.active("org/other", h.set.id), 1);
}

#[tokio::test]
async fn another_repositorys_idle_runner_under_the_same_number_is_not_retired_as_surplus() {
    let mut h = harness(2).await;
    h.roster.add(of_another_repository(
        &h,
        "mac-b-theirs",
        None,
        IDLE_GRACE + Duration::from_secs(1),
    ));
    h.scaler.scale(Some(&stats(0))).await.unwrap();
    assert!(h.hosts.retired().is_empty(), "{:?}", h.hosts.retired());
    assert_eq!(h.roster.active("org/other", h.set.id), 1);
    // Nor does a word about a runner of that name in this repository touch it.
    assert!(!h.roster.mark_done(REPO, h.set.id, "mac-b-theirs", h.clock_now()));
}

// --- who goes first: the main branch and deploys, repositories in turn, nobody for ever -----------------------------

const OTHER: &str = "org/other";

/// A job as GitHub assigns it: no request ID, no times (`the_real_services_assigned_job_has_no_request_id_and_no_times`).
fn job_at(repo: &str, run: i64, git_ref: &str, event: &str) -> JobBase {
    let (owner, name) = repo.split_once('/').expect("owner/name");
    JobBase {
        owner_name: owner.into(),
        repository_name: name.into(),
        job_display_name: "check".into(),
        job_workflow_ref: format!("{repo}/.github/workflows/ci.yml@{git_ref}"),
        workflow_run_id: run,
        event_name: event.into(),
        request_labels: vec!["self-hosted".into(), "cf-2g".into()],
        ..JobBase::default()
    }
}

fn pull_job(repo: &str, run: i64) -> JobBase {
    job_at(repo, run, &format!("refs/pull/{run}/merge"), "pull_request")
}

fn main_job(repo: &str, run: i64) -> JobBase {
    job_at(repo, run, "refs/heads/main", "push")
}

fn counts(assigned: i64) -> Option<Statistics> {
    Some(Statistics {
        total_assigned_jobs: assigned,
        ..Statistics::default()
    })
}

/// GitHub assigns these jobs to the scale set, which has `assigned` jobs with them.
fn assigned(jobs: Vec<JobBase>, assigned: i64) -> Message {
    Message {
        assigned: jobs,
        statistics: counts(assigned),
        ..Message::default()
    }
}

fn started_repos(h: &Harness) -> Vec<String> {
    h.hosts.started_repos()
}

/// The newest Actions runner started.
fn newest(h: &Harness) -> String {
    names(h).last().expect("a runner").clone()
}

/// The Actions runner's job is done, `left` jobs stay assigned to its scale set, and its machine goes: the room it
/// held goes to the first in line.
async fn finish(hosts: &Spy, scaler: &mut TestScaler, runner: &str, job: JobBase, left: i64) {
    let done = Message {
        completed: vec![completed(runner, "succeeded", job)],
        statistics: counts(left),
        ..Message::default()
    };
    scaler.scale(Some(&done)).await.unwrap();
    hosts.gone(runner);
}

/// A Mac of one machine, running a pull request job of [`REPO`]: its Actions runner.
async fn busy_mac() -> (Harness, String) {
    let mut h = harness(1).await;
    let first = pull_job(REPO, 1);
    h.scaler
        .scale(Some(&assigned(vec![first.clone()], 1)))
        .await
        .unwrap();
    let runner = newest(&h);
    h.hosts.boot_all();
    h.scaler.scale(Some(&started(&runner, first))).await.unwrap();
    (h, runner)
}

#[tokio::test]
async fn a_main_job_behind_five_pull_request_jobs_starts_first() {
    let (mut h, running) = busy_mac().await;
    let mut other = h.another_repository(OTHER).await;
    let pulls: Vec<JobBase> = (10..15).map(|run| pull_job(OTHER, run)).collect();
    other.scale(Some(&assigned(pulls, 5))).await.unwrap();
    h.scaler
        .scale(Some(&assigned(vec![main_job(REPO, 2)], 2)))
        .await
        .unwrap();
    assert_eq!(h.machines.snapshot().waiting, 6);

    finish(&h.hosts, &mut h.scaler, &running, pull_job(REPO, 1), 1).await;
    other.scale(None).await.unwrap();
    h.scaler.scale(None).await.unwrap();
    assert_eq!(
        started_repos(&h),
        [REPO, REPO],
        "the main job came last and went first"
    );
    assert_eq!(h.machines.snapshot().waiting, 5);
}

#[tokio::test]
async fn within_one_scale_set_the_place_of_the_main_job_goes_first() {
    // GitHub hands a scale set's jobs to its Actions runners itself; the place that stands for the main job is the
    // one that starts first.
    let (mut h, running) = busy_mac().await;
    let mut jobs: Vec<JobBase> = (10..15).map(|run| pull_job(REPO, run)).collect();
    jobs.push(main_job(REPO, 2));
    h.scaler.scale(Some(&assigned(jobs, 7))).await.unwrap();
    let line = h.machines.snapshot().budget.waiting;
    assert_eq!(line.len(), 6);
    finish(&h.hosts, &mut h.scaler, &running, pull_job(REPO, 1), 6).await;
    h.scaler.scale(None).await.unwrap();
    let (_, _, ticket) = *h.hosts.started().last().expect("a runner");
    assert_eq!(ticket, line[0].slot, "the line named it first");
    assert_eq!(
        h.slots.held().last(),
        Some(&ticket),
        "and it is the sixth place taken"
    );
}

#[tokio::test]
async fn a_pull_request_job_passed_over_for_the_defer_limit_beats_a_fresh_main_job() {
    let (mut h, running) = busy_mac().await;
    let mut other = h.another_repository(OTHER).await;
    other
        .scale(Some(&assigned(vec![pull_job(OTHER, 10)], 1)))
        .await
        .unwrap();
    pass(&h.machines, cf_budget::DEFER_LIMIT.0 / 1000);
    h.scaler
        .scale(Some(&assigned(vec![main_job(REPO, 2)], 2)))
        .await
        .unwrap();
    finish(&h.hosts, &mut h.scaler, &running, pull_job(REPO, 1), 1).await;
    h.scaler.scale(None).await.unwrap();
    other.scale(None).await.unwrap();
    assert_eq!(started_repos(&h), [REPO, OTHER]);
}

#[tokio::test]
async fn two_repositories_take_turns() {
    let (mut h, running) = busy_mac().await;
    let mut other = h.another_repository(OTHER).await;
    let ours: Vec<JobBase> = (20..23).map(|run| pull_job(REPO, run)).collect();
    let theirs: Vec<JobBase> = (30..33).map(|run| pull_job(OTHER, run)).collect();
    h.scaler.scale(Some(&assigned(ours.clone(), 4))).await.unwrap();
    other.scale(Some(&assigned(theirs.clone(), 3))).await.unwrap();
    // The busy repository's job ran last, so the other goes first; then each in turn.
    let (mut ours, mut theirs) = (ours.into_iter(), theirs.into_iter());
    let (mut left_ours, mut left_theirs) = (3, 3);
    finish(&h.hosts, &mut h.scaler, &running, pull_job(REPO, 1), left_ours).await;
    for _ in 0..6 {
        h.scaler.scale(None).await.unwrap();
        other.scale(None).await.unwrap();
        let runner = newest(&h);
        if started_repos(&h).last().map(String::as_str) == Some(REPO) {
            left_ours -= 1;
            finish(&h.hosts, &mut h.scaler, &runner, ours.next().unwrap(), left_ours).await;
        } else {
            left_theirs -= 1;
            finish(&h.hosts, &mut other, &runner, theirs.next().unwrap(), left_theirs).await;
        }
    }
    assert_eq!(started_repos(&h), [REPO, OTHER, REPO, OTHER, REPO, OTHER, REPO]);
}

#[tokio::test]
async fn an_empty_line_waits_for_nothing() {
    let mut h = harness(1).await;
    let capacity = h.scaler.scale(Some(&assigned(vec![], 0))).await.unwrap();
    assert_eq!(capacity, 1, "the free machine is offered");
    assert!(h.hosts.started().is_empty());
    assert_eq!(h.machines.snapshot().waiting, 0);
    // The first job assigned starts at once.
    h.scaler
        .scale(Some(&assigned(vec![pull_job(REPO, 1)], 1)))
        .await
        .unwrap();
    assert_eq!(h.hosts.started().len(), 1);
}

#[tokio::test]
async fn a_main_job_cancelled_while_it_waits_gives_its_place_back_and_the_next_goes() {
    let (mut h, running) = busy_mac().await;
    let mut other = h.another_repository(OTHER).await;
    other
        .scale(Some(&assigned(vec![pull_job(OTHER, 10)], 1)))
        .await
        .unwrap();
    h.scaler
        .scale(Some(&assigned(vec![main_job(REPO, 2)], 2)))
        .await
        .unwrap();
    // Cancelled before any Actions runner had it: it completes without one.
    let cancelled = Message {
        completed: vec![completed("", "canceled", main_job(REPO, 2))],
        statistics: counts(1),
        ..Message::default()
    };
    h.scaler.scale(Some(&cancelled)).await.unwrap();
    assert_eq!(h.machines.snapshot().waiting, 1);
    finish(&h.hosts, &mut h.scaler, &running, pull_job(REPO, 1), 0).await;
    h.scaler.scale(None).await.unwrap();
    other.scale(None).await.unwrap();
    assert_eq!(started_repos(&h), [REPO, OTHER]);
}

#[tokio::test]
async fn an_assignment_heard_twice_is_one_job() {
    let (mut h, running) = busy_mac().await;
    let mut other = h.another_repository(OTHER).await;
    other
        .scale(Some(&assigned(vec![pull_job(OTHER, 10)], 1)))
        .await
        .unwrap();
    let main = assigned(vec![main_job(REPO, 2)], 2);
    h.scaler.scale(Some(&main)).await.unwrap();
    h.scaler.scale(Some(&main)).await.unwrap();
    assert_eq!(h.machines.snapshot().waiting, 2, "one place for the main job");
    // A pull request job comes after it. Had the main job been counted twice, its place would be the main job's.
    h.scaler
        .scale(Some(&assigned(vec![pull_job(REPO, 3)], 3)))
        .await
        .unwrap();
    finish(&h.hosts, &mut h.scaler, &running, pull_job(REPO, 1), 2).await;
    h.scaler.scale(None).await.unwrap();
    let main_runner = newest(&h);
    finish(&h.hosts, &mut h.scaler, &main_runner, main_job(REPO, 2), 1).await;
    h.scaler.scale(None).await.unwrap();
    other.scale(None).await.unwrap();
    assert_eq!(
        started_repos(&h),
        [REPO, REPO, OTHER],
        "the other repository's pull request job goes before ours: ours started last"
    );
}

#[tokio::test]
async fn offered_jobs_that_do_not_all_fit_are_taken_main_first() {
    let mut h = harness(1).await;
    let labels = ["self-hosted", "cf-2g"];
    let pulls: Vec<i64> = (1..=5)
        .map(|n| {
            h.fake.queue_job_at(
                REPO,
                &labels,
                &format!("check {n}"),
                &format!("refs/pull/{n}/merge"),
            )
        })
        .collect();
    let main = h.fake.queue_job(REPO, &labels, "check");
    let message = next_messages(&h, 6, 1).await;
    h.scaler.scale(Some(&message)).await.unwrap();
    assert_eq!(h.fake.job_state(main), Some(JobState::Acquired(h.set.id)));
    for id in pulls {
        assert_eq!(
            h.fake.job_state(id),
            Some(JobState::Available),
            "it waits at GitHub"
        );
    }
}

#[tokio::test]
async fn an_offered_job_passed_over_for_the_defer_limit_is_taken_before_a_main_job() {
    let mut h = harness(1).await;
    let labels = ["self-hosted", "cf-2g"];
    h.slots.set_open(false);
    let old = h.fake.queue_job_at(REPO, &labels, "check", "refs/pull/1/merge");
    let first = next_message(&h, 1).await;
    h.scaler.scale(Some(&first)).await.unwrap();
    h.clock.advance(Duration::from_millis(cf_budget::DEFER_LIMIT.0));
    h.slots.set_open(true);
    pass(&h.machines, 1);
    let main = h.fake.queue_job(REPO, &labels, "check");
    // The old job's offer was not taken, so it comes again, with the new one's.
    let again = next_messages(&h, 2, 1).await;
    assert_eq!(again.available.len(), 2);
    h.scaler.scale(Some(&again)).await.unwrap();
    assert_eq!(h.fake.job_state(old), Some(JobState::Acquired(h.set.id)));
    assert_eq!(h.fake.job_state(main), Some(JobState::Available));
}

#[tokio::test]
async fn a_main_job_that_went_elsewhere_between_offer_and_acquire_gives_its_slot_back() {
    let mut h = harness(1).await;
    let labels = ["self-hosted", "cf-2g"];
    let pull = h.fake.queue_job_at(REPO, &labels, "check", "refs/pull/1/merge");
    let main = h.fake.queue_job(REPO, &labels, "check");
    let message = next_messages(&h, 2, 1).await;
    h.fake.take_job(main);
    h.scaler.scale(Some(&message)).await.unwrap();
    assert_eq!(h.fake.job_state(main), Some(JobState::TakenElsewhere));
    assert_eq!(
        h.fake.job_state(pull),
        Some(JobState::Available),
        "the job passed over is still offered, not lost"
    );
    assert_eq!((h.slots.used(), h.slots.released().len()), (0, 1));
    // Offered again, it is taken.
    let mut again = message;
    again.available.retain(|offer| offer.runner_request_id == pull);
    h.scaler.scale(Some(&again)).await.unwrap();
    assert_eq!(h.fake.job_state(pull), Some(JobState::Acquired(h.set.id)));
}

#[tokio::test]
async fn a_main_job_offered_twice_is_acquired_once_and_the_next_slot_goes_to_the_next_job() {
    let mut h = harness(2).await;
    let labels = ["self-hosted", "cf-2g"];
    let pull = h.fake.queue_job_at(REPO, &labels, "check", "refs/pull/1/merge");
    let main = h.fake.queue_job(REPO, &labels, "check");
    let mut message = next_messages(&h, 2, 2).await;
    let offered_again = message.available[1].clone();
    message.available.insert(0, offered_again);
    h.scaler.scale(Some(&message)).await.unwrap();
    assert_eq!(h.fake.job_state(main), Some(JobState::Acquired(h.set.id)));
    assert_eq!(h.fake.job_state(pull), Some(JobState::Acquired(h.set.id)));
    assert_eq!((h.slots.used(), h.fake.count("acquirejobs")), (2, 1));
    assert!(h.slots.released().is_empty(), "no slot was promised to the copy");
}

#[tokio::test]
async fn a_place_in_line_takes_the_turn_of_the_job_it_stands_for_when_another_goes() {
    let (mut h, running) = busy_mac().await;
    let mut other = h.another_repository(OTHER).await;
    other
        .scale(Some(&assigned(vec![pull_job(OTHER, 10)], 1)))
        .await
        .unwrap();
    // A pull request job and then a main job wait; the pull request job is cancelled. The newest place goes, so the
    // one left now stands for the main job.
    h.scaler
        .scale(Some(&assigned(vec![pull_job(REPO, 2), main_job(REPO, 3)], 3)))
        .await
        .unwrap();
    let cancelled = Message {
        completed: vec![completed("", "canceled", pull_job(REPO, 2))],
        statistics: counts(2),
        ..Message::default()
    };
    h.scaler.scale(Some(&cancelled)).await.unwrap();
    assert_eq!(h.machines.snapshot().waiting, 2);
    finish(&h.hosts, &mut h.scaler, &running, pull_job(REPO, 1), 1).await;
    h.scaler.scale(None).await.unwrap();
    other.scale(None).await.unwrap();
    assert_eq!(started_repos(&h), [REPO, REPO]);
}

#[tokio::test]
async fn an_offered_job_passed_over_keeps_its_age_however_many_other_offers_come_and_go() {
    let mut h = harness(1).await;
    let labels = ["self-hosted", "cf-2g"];
    h.slots.set_open(false);
    let old = h.fake.queue_job_at(REPO, &labels, "check", "refs/pull/1/merge");
    let first = next_message(&h, 1).await;
    h.scaler.scale(Some(&first)).await.unwrap();
    // A thousand jobs are offered once each, and taken elsewhere, while the old one waits.
    for n in 0..1000 {
        let gone = Message {
            available: vec![JobBase {
                runner_request_id: 100_000 + n,
                ..pull_job(REPO, 100_000 + n)
            }],
            ..first.clone()
        };
        h.scaler.scale(Some(&gone)).await.unwrap();
    }
    h.clock.advance(Duration::from_millis(cf_budget::DEFER_LIMIT.0));
    h.slots.set_open(true);
    pass(&h.machines, 1);
    let main = h.fake.queue_job(REPO, &labels, "check");
    let again = next_messages(&h, 2, 1).await;
    h.scaler.scale(Some(&again)).await.unwrap();
    assert_eq!(h.fake.job_state(old), Some(JobState::Acquired(h.set.id)));
    assert_eq!(h.fake.job_state(main), Some(JobState::Available));
}
