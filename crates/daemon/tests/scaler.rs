//! The scaler's behaviour, ported case by case from the Go manager's scaler tests (HANDOVER.md has the mapping),
//! plus what the HTTP edge adds: what GitHub is left holding when this machine refuses or loses a job.

mod support;

use std::time::Duration;

use cf_daemon::{EventKind, IDLE_GRACE, JobInfo, PROMISE_TTL, Runner, Slots};
use cf_fake_github::JobState;
use cf_fake_github::httpd::Reply;
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
    h.roster.exited(&first, &*h.slots).expect("known runner");
    h.scaler.scale(None).await.unwrap();
    assert_eq!(h.hosts.started().len(), 3);
}

#[tokio::test]
async fn acquires_only_what_fits() {
    let mut h = harness(2).await;
    let ids: Vec<i64> = (1..=3)
        .map(|i| {
            h.fake
                .queue_job(REPO, &["self-hosted", "cf-1g"], &format!("job {i}"))
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
    assert!(h.slots.reserve(1).is_err(), "acquired jobs hold their slots");
    assert_eq!(
        h.scaler.capacity(),
        2,
        "the promised slots count, and no free one is left to offer"
    );
}

#[tokio::test]
async fn jobs_offered_twice_in_one_message_are_acquired_once() {
    let mut h = harness(3).await;
    let id = h.fake.queue_job(REPO, &["self-hosted", "cf-1g"], "job");
    let mut message = next_message(&h, 3).await;
    message.available.push(message.available[0].clone());
    h.scaler.scale(Some(&message)).await.unwrap();
    assert_eq!(h.fake.job_state(id), Some(JobState::Acquired(h.set.id)));
    assert_eq!(h.slots.used_gb(), 1, "one job, one slot");
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
                .queue_job(REPO, &["self-hosted", "cf-1g"], &format!("job {i}"))
        })
        .collect();
    let message = next_messages(&h, 3, 3).await;
    // Another machine's Actions runner takes two of them between the offer and the acquire.
    h.fake.take_job(ids[1]);
    h.fake.take_job(ids[2]);
    h.scaler.scale(Some(&message)).await.unwrap();
    assert_eq!(h.fake.job_state(ids[0]), Some(JobState::Acquired(h.set.id)));
    assert_eq!(h.slots.used_gb(), 1, "only the job this machine got holds a slot");
    assert_eq!(h.slots.released().len(), 2);
}

#[tokio::test]
async fn an_acquire_failure_gives_the_slots_back_and_fails_the_poll() {
    let mut h = harness(2).await;
    for i in 1..=2 {
        h.fake
            .queue_job(REPO, &["self-hosted", "cf-1g"], &format!("job {i}"));
    }
    let message = next_messages(&h, 2, 2).await;
    h.fake.fail("acquirejobs", Reply::text(503, "down"), 10);
    let err = h
        .scaler
        .scale(Some(&message))
        .await
        .expect_err("the message must come back");
    assert_eq!(err.status(), Some(503));
    assert_eq!(h.slots.used_gb(), 0, "slots of a failed acquire are free again");
}

#[tokio::test]
async fn a_closed_machine_acquires_nothing_but_runs_what_it_has() {
    let mut h = harness(2).await;
    h.slots.set_open(false, "in use");
    let id = h.fake.queue_job(REPO, &["self-hosted", "cf-1g"], "job");
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
    assert_eq!(h.slots.used_gb(), 0);
    // A job assigned before the machine closed still gets its Actions runner.
    let capacity = h.scaler.scale(Some(&stats(1))).await.unwrap();
    assert_eq!(
        h.hosts.started().len(),
        1,
        "an assigned job was left without a runner"
    );
    assert_eq!(capacity, 1, "just its runner, no free slot offered");
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
    assert!(err.to_string().contains("no machine"), "{err}");
    assert!(h.fake.runners().is_empty(), "the registration was left behind");
    assert_eq!(h.slots.used_gb(), 0);
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
    assert_eq!(h.slots.used_gb(), 0, "slot held after a failed registration");
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
        h.hosts.stopped().is_empty(),
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
        h.hosts.stopped().is_empty(),
        "retired runners still inside their grace period"
    );

    h.clock.advance(IDLE_GRACE);
    let (stuck_name, _, stuck_id) = {
        let (n, id, t) = all
            .iter()
            .skip(1)
            .map(|(n, id, t)| (n.clone(), *id, *t))
            .next()
            .unwrap();
        (n, t, id)
    };
    h.fake.fail(
        &format!("DELETE /svc/_apis/distributedtask/pools/0/agents/{stuck_id}"),
        Reply::text(400, "job still running"),
        10,
    );
    h.scaler.scale(None).await.unwrap();
    let removed = h.hosts.stopped();
    assert_eq!(
        removed.len(),
        1,
        "only the idle runner that could be unregistered goes: {removed:?}"
    );
    assert_ne!(removed[0], stuck_name);
    assert_ne!(removed[0], busy);
    for kept in [&busy, &"adopted".to_string(), &stuck_name] {
        assert!(
            h.roster.get(kept).is_some(),
            "{kept}: busy, adopted and not-yet-removable runners must stay"
        );
    }
    assert_eq!(
        h.slots.released().len(),
        1,
        "the retired runner's slot is back, once"
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
    let id = h.fake.queue_job(REPO, &["self-hosted", "cf-1g"], "job");
    let offered = next_message(&h, 1).await;
    h.scaler.scale(Some(&offered)).await.unwrap();
    let assigned = next_message(&h, 1).await;
    assert_eq!(assigned.statistics.unwrap().assigned(), 1);
    h.scaler.scale(Some(&assigned)).await.unwrap();
    assert_eq!(h.hosts.started().len(), 1);
    // The job is cancelled before the Actions runner took it.
    h.fake.cancel_job(id);
    let cancelled = next_message(&h, 1).await;
    h.scaler.scale(Some(&cancelled)).await.unwrap();
    assert!(
        h.hosts.stopped().is_empty(),
        "inside the grace period the runner may still be wanted"
    );
    h.clock.advance(IDLE_GRACE);
    h.scaler.scale(None).await.unwrap();
    assert_eq!(h.hosts.stopped().len(), 1);
    assert!(h.fake.runners().is_empty());
    assert_eq!(h.slots.used_gb(), 0);
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
        vec!["mac-b-j0000000".to_string()],
        "only the runner this machine no longer has is asked about"
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
    h.clock.advance(IDLE_GRACE * 3);
    h.scaler.scale(Some(&stats(0))).await.unwrap();
    assert_eq!(
        h.hosts.stopped(),
        vec![idle],
        "only the idle runner goes; the finished one exits by itself"
    );
    assert!(h.roster.get(&finished).is_some());
}

// added: what the scaler owes the machine's slots

/// One job offered and acquired, and the slot promised to it.
async fn acquired_job(h: &mut Harness) -> (i64, cf_daemon::Ticket) {
    let id = h.fake.queue_job(REPO, &["self-hosted", "cf-1g"], "job");
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
    assert_eq!(h.slots.used_gb(), 1, "the promise still stands");

    h.clock.advance(Duration::from_secs(10));
    assert_eq!(
        h.scaler.scale(None).await.unwrap(),
        1,
        "a promise nobody will claim is no longer offered to the service as room"
    );
    assert_eq!(h.slots.used_gb(), 0);
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
    assert_eq!(h.slots.used_gb(), 1);
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
    let id = h.fake.queue_job(REPO, &["self-hosted", "cf-1g"], "job");
    let message = next_message(&h, 2).await;
    h.fake.fail("acquirejobs", Reply::Hang, 1);
    let given_up = tokio::time::timeout(Duration::from_millis(200), h.scaler.scale(Some(&message))).await;
    assert!(given_up.is_err(), "the acquire was to hang");
    assert_eq!(h.fake.job_state(id), Some(JobState::Available));
    assert_eq!(h.slots.used_gb(), 1);
    assert_eq!(
        h.scaler.capacity(),
        2,
        "the scaler still knows the promise it made"
    );
    h.clock.advance(PROMISE_TTL);
    h.scaler.scale(None).await.unwrap();
    assert_eq!(
        h.slots.used_gb(),
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
        h.hosts.stopped().is_empty(),
        "the runner of a job still assigned was retired"
    );
    assert_eq!(
        h.roster.active(h.set.id),
        1,
        "the second job still has its runner"
    );

    h.scaler.scale(Some(&done(&second))).await.unwrap();
    assert_eq!(h.hosts.started().len(), 2);
    assert!(h.hosts.stopped().is_empty());

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
        h.hosts.stopped().is_empty(),
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
    h.clock.advance(IDLE_GRACE);
    h.hosts.fail_stop(1);
    h.scaler.scale(Some(&stats(0))).await.unwrap();
    assert_eq!(h.fake.runners(), Vec::<String>::new(), "its registration is gone");
    assert_eq!(
        h.slots.held(),
        vec![ticket],
        "the slot of a machine that may still stand was given to the next job"
    );
    assert_eq!(h.roster.active(h.set.id), 0, "it waits for no job any more");
    assert!(
        h.roster.get(&name).is_some(),
        "whoever watches the machines still finds it"
    );

    // A new job gets its own Actions runner and slot beside it, and the watcher's word frees the old one.
    h.scaler.scale(Some(&stats(1))).await.unwrap();
    assert_eq!(h.hosts.started().len(), 2);
    assert_eq!(h.slots.used_gb(), 2);
    h.roster.exited(&name, &*h.slots);
    assert_eq!(h.slots.used_gb(), 1);
}
