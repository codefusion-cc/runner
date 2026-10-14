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

// Probe J2 and the Mac mini at 18:57:52: a job assigned to a scale set whose Actions runner never took it came back
// after five minutes as `JobCompleted` `canceled`, and was handed out again.
#[tokio::test]
async fn an_assignment_no_actions_runner_takes_within_five_minutes_is_taken_back() {
    let h = harness_with(honest(), fast(), Duration::from_secs(5)).await;
    let (set, session) = with_session(&h).await;
    session.poll(0, 1).await.unwrap();
    session.poll(0, 1).await.unwrap();
    let job = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    let assigned = session.poll(0, 0).await.unwrap().expect("the assignment");
    assert_eq!(assigned.assigned.len(), 1);
    session.poll(assigned.id, 0).await.unwrap();
    let jit = h
        .client
        .generate_jit_config(set.id, "mac-a-late", "_work")
        .await
        .unwrap();

    // The queue's clock is the wall clock plus what passed: a margin wide enough for a slow machine.
    h.fake
        .pass(cf_fake_github::TAKE_BACK_AFTER - Duration::from_secs(30));
    assert_eq!(h.fake.job_state(job), Some(JobState::Acquired(set.id)), "not yet");
    h.fake.pass(Duration::from_secs(30));
    assert_eq!(
        h.fake.job_state(job),
        Some(JobState::Available),
        "back in the queue: the polls offer no room for it"
    );
    let taken_back = session
        .poll(assigned.id, 0)
        .await
        .unwrap()
        .expect("the take-back");
    assert_eq!(taken_back.completed.len(), 1);
    assert_eq!(taken_back.completed[0].result, "canceled");
    assert_eq!(
        taken_back.completed[0].runner_name, "",
        "no Actions runner had it"
    );

    h.fake.start_job(&jit.runner.name);
    assert_eq!(
        h.fake.job_state(job),
        Some(JobState::Available),
        "an Actions runner that comes late gets nothing"
    );
}

// The REST view is a snapshot. The fake can answer one read behind, never from before the registration's answer.
#[tokio::test]
async fn a_stale_rest_view_answers_what_the_read_before_saw() {
    let h = harness_with(honest(), fast(), Duration::from_secs(5)).await;
    let (set, session) = with_session(&h).await;
    session.poll(0, 1).await.unwrap();
    session.poll(0, 1).await.unwrap();
    h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    let jit = h
        .client
        .generate_jit_config(set.id, "mac-a-stale", "_work")
        .await
        .unwrap();
    let id = jit.runner.id;
    h.fake.start_job(&jit.runner.name);
    h.fake.set_rest_stale(true);
    assert_eq!(
        h.client.runner_busy(id).await.unwrap(),
        Some(false),
        "as registered"
    );
    assert_eq!(h.client.runner_busy(id).await.unwrap(), Some(true));
    h.fake.complete_job(&jit.runner.name, "succeeded");
    assert_eq!(
        h.client.runner_busy(id).await.unwrap(),
        Some(true),
        "one read behind"
    );
    assert_eq!(h.client.runner_busy(id).await.unwrap(), None);
    h.fake.set_rest_stale(false);
    assert_eq!(h.client.runner_busy(id).await.unwrap(), None);
    assert_eq!(
        h.client.runner_busy(9999).await.unwrap(),
        None,
        "never registered"
    );
}

#[tokio::test]
async fn a_run_says_the_branch_its_code_came_from_not_its_workflows() {
    let h = harness_with(honest(), fast(), Duration::from_secs(5)).await;
    let main = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    let pull = h
        .fake
        .queue_job_at(REPO, &["self-hosted", "cf-2g"], "build", "refs/pull/4/merge");
    let called = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    // A reusable workflow at main, called from a workflow of another branch: its code is that branch's.
    h.fake.set_run_head(called, "feature");
    let run = |request| h.fake.run_id_of(request).unwrap();

    let read = h.client.workflow_run(run(main)).await.unwrap().expect("a run");
    assert_eq!(
        (
            read.head_branch.as_deref(),
            read.head_repository.as_deref(),
            read.event.as_str()
        ),
        (Some("main"), Some(REPO), "push")
    );
    let read = h.client.workflow_run(run(pull)).await.unwrap().expect("a run");
    assert_eq!(
        (read.head_branch.as_deref(), read.event.as_str()),
        (Some("feature"), "pull_request")
    );
    let read = h.client.workflow_run(run(called)).await.unwrap().expect("a run");
    assert_eq!(read.head_branch.as_deref(), Some("feature"));
    assert_eq!(h.client.workflow_run(424_242).await.unwrap(), None, "no such run");
}
