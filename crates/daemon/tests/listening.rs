//! The scaler under the real listener, long polls and all: what GitHub does when this machine will not take a job.

mod support;

use std::time::Duration;

use cf_fake_github::{JobState, Options};
use cf_harness::until;
use cf_scaleset::{listen, shutdown};
use support::*;

#[tokio::test]
async fn a_job_the_closed_machine_refused_waits_at_github_and_is_taken_once_the_machine_opens() {
    let h = harness_with(
        2,
        Options {
            auto_assign: true,
            ..Options::default()
        },
    )
    .await;
    h.fake.set_poll_timeout(Duration::from_millis(150));
    h.slots.set_open(false);
    let id = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");

    let (trigger, stop) = shutdown();
    let (session, mut scaler) = (h.session.clone(), h.scaler);
    let task = tokio::spawn(async move { listen(&session, &mut scaler, stop).await });

    let polls_with_room = |room: &str| {
        h.fake
            .requests()
            .iter()
            .filter(|r| r.max_capacity.as_deref() == Some(room))
            .count()
    };
    until("polls that offer no room", || polls_with_room("0") >= 2).await;
    assert_eq!(
        h.fake.job_state(id),
        Some(JobState::Available),
        "GitHub must not hand a closed machine a job"
    );
    assert!(h.hosts.started().is_empty());
    assert_eq!(h.fake.count("acquirejobs"), 0);

    h.slots.set_open(true);
    until("the machine to offer its free slot twice", || {
        polls_with_room("1") >= 2
    })
    .await;
    // GitHub hands the job over at its next look at the queue, which no poll brings about.
    h.fake.github_looks();
    until("a runner for the job", || h.hosts.started().len() == 1).await;
    assert_eq!(h.fake.runners().len(), 1);

    trigger.trigger();
    task.await.unwrap().unwrap();
}

/// The listener is stopped the moment the offer is acknowledged: the job is acquired and its assignment, which the
/// service sends next, has not been read. The promised slot goes back with the scaler, so nothing holds memory
/// while nobody looks after it, and the job is not lost: it stays this scale set's at GitHub, and whoever listens
/// next reads the assignment and starts its Actions runner.
#[tokio::test]
async fn a_stop_before_the_assignment_gives_the_promise_back_and_the_next_listener_starts_the_job() {
    let h = harness(2).await;
    let id = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    let (trigger, stop) = shutdown();
    // Asked to stop just before the service handles the acknowledgement, so the listener sees it at its next
    // look, before any other message.
    h.fake.before("DELETE /svc", move |_| trigger.trigger());
    let (session, mut scaler) = (h.session.clone(), h.scaler);
    tokio::spawn(async move { listen(&session, &mut scaler, stop).await })
        .await
        .unwrap()
        .expect("a clean stop");

    assert_eq!(h.fake.job_state(id), Some(JobState::Acquired(h.set.id)));
    assert_eq!(h.fake.count("DELETE /svc"), 1, "the offer is acknowledged, once");
    assert!(h.hosts.started().is_empty(), "the assignment was not read");
    assert_eq!(
        h.slots.released().len(),
        1,
        "the promise went back with the scaler"
    );
    assert_eq!(h.slots.used(), 0);

    let mut scaler = scaler_of(&Parts {
        client: &h.client,
        set: &h.set,
        session: &h.session,
        slots: &h.slots,
        hosts: &h.hosts,
        roster: &h.roster,
        clock: &h.clock,
        events: &h.events,
    });
    let (trigger, stop) = shutdown();
    let session = h.session.clone();
    let task = tokio::spawn(async move { listen(&session, &mut scaler, stop).await });
    until("an Actions runner for the acquired job", || {
        h.hosts.started().len() == 1
    })
    .await;
    assert_eq!((h.slots.claimed(), h.slots.used()), (1, 1));
    trigger.trigger();
    task.await.unwrap().expect("a clean stop");
}

/// The listener is stopped after the assignment was handled: the Actions runner stands, and its slot stays held
/// when the scaler is gone, until its machine is.
#[tokio::test]
async fn a_stop_after_the_assignment_leaves_the_actions_runner_its_slot() {
    let h = harness(2).await;
    let id = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    let (trigger, stop) = shutdown();
    let (session, mut scaler) = (h.session.clone(), h.scaler);
    let task = tokio::spawn(async move { listen(&session, &mut scaler, stop).await });

    until("an Actions runner for the job", || h.hosts.started().len() == 1).await;
    until("the offer and the assignment to be acknowledged", || {
        h.fake.count("DELETE /svc") >= 2
    })
    .await;
    trigger.trigger();
    task.await.unwrap().expect("a clean stop");
    assert_eq!(h.fake.job_state(id), Some(JobState::Acquired(h.set.id)));
    assert_eq!((h.slots.claimed(), h.slots.used()), (1, 1));
    assert!(
        h.slots.released().is_empty(),
        "the slot of a standing machine is not given back"
    );
}
