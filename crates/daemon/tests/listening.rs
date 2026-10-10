//! The scaler under the real listener, long polls and all: what GitHub does when this machine will not take a job.

mod support;

use std::time::Duration;

use cf_fake_github::{JobState, Options};
use cf_scaleset::{listen, shutdown};
use support::*;

async fn until(what: &str, condition: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while !condition() {
        assert!(std::time::Instant::now() < deadline, "gave up waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

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
    h.slots.set_open(false, "in use");
    let id = h.fake.queue_job(REPO, &["self-hosted", "cf-1g"], "build");

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

    h.slots.set_open(true, "");
    until("a runner for the job", || h.hosts.started().len() == 1).await;
    assert_eq!(h.fake.runners().len(), 1);
    assert!(
        polls_with_room("1") >= 1,
        "the machine offers its free slot once it is open"
    );

    trigger.trigger();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn shutdown_leaves_an_acquired_job_with_its_slot_held_and_its_message_acknowledged() {
    let h = harness(2).await;
    let id = h.fake.queue_job(REPO, &["self-hosted", "cf-1g"], "build");
    let (trigger, stop) = shutdown();
    let (session, mut scaler) = (h.session.clone(), h.scaler);
    let task = tokio::spawn(async move { listen(&session, &mut scaler, stop).await });

    until("the job to be acquired", || {
        h.fake.job_state(id) == Some(JobState::Acquired(h.set.id))
    })
    .await;
    until("the offer to be acknowledged", || {
        h.fake.count("DELETE /svc") >= 1
    })
    .await;
    trigger.trigger();
    task.await.unwrap().expect("a clean stop");
    assert_eq!(h.fake.job_state(id), Some(JobState::Acquired(h.set.id)));
    assert_eq!(
        h.slots.used_gb(),
        1,
        "the promise stays until it runs out; the job is not dropped silently"
    );
}
