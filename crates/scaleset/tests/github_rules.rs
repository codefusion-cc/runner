//! The fake GitHub may be no kinder than the real one. Each rule the Runner's tests lean on is checked here through
//! the real client, beside what was seen at GitHub (`docs/github-protocol.md` has the probes and the logs).

mod support;

use std::time::Duration;

use cf_fake_github::{JobState, Options};
use support::*;

fn honest() -> Options {
    Options {
        auto_assign: true,
        poll_timeout: Duration::from_millis(150),
        ..Options::default()
    }
}

// Probes E and F, 2026-10-12: a job queued while the polls offered no room stayed queued for a minute of polls with
// room in a session opened after it.
#[tokio::test]
async fn a_session_opening_hands_out_no_queued_job() {
    let h = harness_with(honest(), fast(), Duration::from_secs(5)).await;
    let (set, session) = with_session(&h).await;
    assert_eq!(session.poll(0, 0).await.unwrap(), None);
    let job = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    session.close().await.unwrap();
    let session = h.client.open_session(set.id, "mac-a").await.unwrap();
    for _ in 0..3 {
        assert_eq!(session.poll(0, 1).await.unwrap(), None);
    }
    assert_eq!(h.fake.job_state(job), Some(JobState::Available));
}

// Probe D saw a pending message survive a session's end; one sample does not make it a promise, and the Runner's
// tests run against the GitHub that drops it.
#[tokio::test]
async fn a_session_that_ends_takes_its_pending_messages_with_it() {
    let h = harness_with(honest(), fast(), Duration::from_secs(5)).await;
    let (set, session) = with_session(&h).await;
    session.poll(0, 1).await.unwrap();
    session.poll(0, 1).await.unwrap();
    let job = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    assert_eq!(h.fake.job_state(job), Some(JobState::Acquired(set.id)));
    session.close().await.unwrap();
    let session = h.client.open_session(set.id, "mac-a").await.unwrap();
    assert_eq!(
        session.poll(0, 1).await.unwrap(),
        None,
        "the assignment is gone with the old queue"
    );
    assert_eq!(
        session.initial_statistics().total_assigned_jobs,
        1,
        "the counts still name it"
    );
}

// Every count in the probes said `totalAvailableJobs: 0` while a job waited unassigned.
#[tokio::test]
async fn the_counts_never_name_a_job_github_holds() {
    let h = harness_with(honest(), fast(), Duration::from_secs(5)).await;
    let (set, session) = with_session(&h).await;
    session.poll(0, 0).await.unwrap();
    h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    session.close().await.unwrap();
    let session = h.client.open_session(set.id, "mac-a").await.unwrap();
    assert_eq!(session.initial_statistics().total_available_jobs, 0);
}

// Probe E: a queue event assigned against room of the poll before the newest; probe J1: GitHub assigned against
// room only the newest poll offered. The fake gives the smaller of the two, which neither reading beats.
#[tokio::test]
async fn a_queue_event_assigns_against_the_smaller_room_of_the_last_two_polls() {
    let h = harness_with(honest(), fast(), Duration::from_secs(5)).await;
    let (set, session) = with_session(&h).await;
    session.poll(0, 0).await.unwrap();
    session.poll(0, 1).await.unwrap();
    let first = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    assert_eq!(
        h.fake.job_state(first),
        Some(JobState::Available),
        "the poll before offered none"
    );
    session.poll(0, 1).await.unwrap();
    let second = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "test");
    assert_eq!(
        h.fake.job_state(first),
        Some(JobState::Acquired(set.id)),
        "the oldest first"
    );
    assert_eq!(
        h.fake.job_state(second),
        Some(JobState::Available),
        "room for one"
    );
}

// Probes A, G, I2, J1 and J3: with no event, GitHub sometimes assigned within half a minute of room, sometimes not
// in three. The fake looks only when a test says GitHub does.
#[tokio::test]
async fn polls_with_room_assign_nothing_until_github_looks() {
    let h = harness_with(honest(), fast(), Duration::from_secs(5)).await;
    let (set, session) = with_session(&h).await;
    session.poll(0, 0).await.unwrap();
    let job = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    for _ in 0..3 {
        session.poll(0, 1).await.unwrap();
    }
    assert_eq!(h.fake.job_state(job), Some(JobState::Available));
    h.fake.github_looks();
    assert_eq!(h.fake.job_state(job), Some(JobState::Acquired(set.id)));
}
