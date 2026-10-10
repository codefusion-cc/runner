//! The administrator's side and one session's calls, against the fake GitHub.

mod support;

use std::time::Duration;

use cf_fake_github::JobState;
use cf_scaleset::{Error, Message};
use support::*;

#[tokio::test]
async fn a_scale_set_is_created_once_and_found_again_with_its_labels_set() {
    let h = harness().await;
    let first = h
        .client
        .ensure_scale_set("air--cf-2g", &labels(&["Self-Hosted", "CF-2G"]))
        .await
        .unwrap();
    let again = h
        .client
        .ensure_scale_set("air--cf-2g", &labels(&["cf-2g", "self-hosted"]))
        .await
        .unwrap();
    assert_eq!(first.id, again.id);
    assert_eq!(h.fake.scale_sets().len(), 1);
    assert_eq!(h.fake.scale_sets()[0].labels, vec!["cf-2g", "self-hosted"]);
    // The labels changed: the set is updated, not duplicated.
    let changed = h
        .client
        .ensure_scale_set("air--cf-2g", &labels(&["self-hosted", "cf-4g"]))
        .await
        .unwrap();
    assert_eq!(changed.id, first.id);
    assert_eq!(h.fake.scale_sets().len(), 1);
    assert_eq!(h.fake.scale_sets()[0].labels, vec!["cf-4g", "self-hosted"]);
    assert_eq!(h.client.list_scale_sets().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_queued_job_arrives_is_acquired_and_gets_a_one_job_runner() {
    let h = harness().await;
    let (set, session) = with_session(&h).await;
    let request = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");

    let message = session.poll(0, 2).await.unwrap().expect("the offer");
    assert_eq!(message.available.len(), 1);
    assert_eq!(message.available[0].runner_request_id, request);
    assert_eq!(message.available[0].job_display_name, "build");
    assert_eq!(message.available[0].repository(), REPO);
    assert_eq!(message.statistics.unwrap().total_available_jobs, 1);
    let polls = h
        .fake
        .requests()
        .into_iter()
        .filter(|r| r.line.starts_with("GET /svc") && r.line.ends_with("/messages"))
        .collect::<Vec<_>>();
    assert_eq!(polls[0].max_capacity.as_deref(), Some("2"));

    assert_eq!(session.acquire_jobs(&[request]).await.unwrap(), vec![request]);
    assert_eq!(h.fake.job_state(request), Some(JobState::Acquired(set.id)));

    let jit = h
        .client
        .generate_jit_config(set.id, "air-j0000001", "_work")
        .await
        .unwrap();
    assert_eq!(jit.runner.name, "air-j0000001");
    assert!(!jit.encoded.expose().is_empty());
    assert_eq!(h.fake.runners(), vec!["air-j0000001"]);
    assert!(
        !format!("{jit:?}").contains(jit.encoded.expose()),
        "the configuration is a credential"
    );

    h.client.remove_runner(jit.runner.id).await.unwrap();
    assert!(h.fake.runners().is_empty());
    assert!(
        h.client
            .remove_runner(jit.runner.id)
            .await
            .unwrap_err()
            .is_not_found()
    );
}

#[tokio::test]
async fn a_quiet_wait_is_none_and_acknowledging_removes_the_message() {
    let h = harness().await;
    let (_, session) = with_session(&h).await;
    assert!(session.poll(0, 1).await.unwrap().is_none());
    h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "a");
    let message = session.poll(0, 1).await.unwrap().unwrap();
    assert_eq!(h.fake.scale_sets()[0].queued_messages, 1);
    session.acknowledge(message.id).await.unwrap();
    assert_eq!(h.fake.scale_sets()[0].queued_messages, 0);
    assert!(session.poll(message.id, 1).await.unwrap().is_none());
}

#[tokio::test]
async fn jobs_of_other_labels_or_repositories_are_not_offered() {
    let h = harness().await;
    let (_, session) = with_session(&h).await;
    h.fake.add_repo("org/else", true);
    h.fake
        .queue_job("org/else", &["self-hosted", "cf-2g"], "elsewhere");
    h.fake.queue_job(REPO, &["self-hosted", "cf-8g"], "bigger");
    assert!(session.poll(0, 1).await.unwrap().is_none());
}

#[tokio::test]
async fn the_lifecycle_messages_carry_the_runner_and_the_result() {
    let h = harness().await;
    let (set, session) = with_session(&h).await;
    let request = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    session.acquire_jobs(&[request]).await.unwrap();
    let jit = h
        .client
        .generate_jit_config(set.id, "air-jabc", "_work")
        .await
        .unwrap();
    h.fake.start_job("air-jabc");
    h.fake.complete_job("air-jabc", "failed");
    let mut seen = Message::default();
    let mut after = 0;
    for _ in 0..4 {
        let Some(m) = session.poll(after, 1).await.unwrap() else {
            break;
        };
        after = m.id;
        seen.available.extend(m.available);
        seen.assigned.extend(m.assigned);
        seen.started.extend(m.started);
        seen.completed.extend(m.completed);
    }
    assert_eq!(
        (
            seen.available.len(),
            seen.assigned.len(),
            seen.started.len(),
            seen.completed.len()
        ),
        (1, 1, 1, 1)
    );
    assert_eq!(seen.started[0].runner_name, "air-jabc");
    assert_eq!(seen.completed[0].result, "failed");
    assert_eq!(seen.completed[0].runner_name, "air-jabc");
    assert_eq!(seen.completed[0].workflow_run_id, 7000 + request);
    assert!(seen.completed[0].queue_time.is_some());
    let _ = jit;
}

#[tokio::test]
async fn a_second_session_of_the_machine_is_refused_until_the_first_is_closed() {
    let h = harness().await;
    let (set, first) = with_session(&h).await;
    assert_eq!(
        h.client.open_session(set.id, "air").await.unwrap_err(),
        Error::SessionExists
    );
    first.close().await.unwrap();
    assert!(!h.fake.scale_sets()[0].has_session);
    h.client.open_session(set.id, "air").await.unwrap();
}

#[tokio::test]
async fn the_admin_token_is_replaced_when_it_runs_out() {
    let h = harness_with(
        cf_fake_github::Options {
            admin_token_ttl: Duration::from_secs(1000),
            ..Default::default()
        },
        fast(),
        Duration::from_secs(10),
    )
    .await;
    h.client.list_scale_sets().await.unwrap();
    h.client.list_scale_sets().await.unwrap();
    assert_eq!(
        h.fake.count("POST /actions/runner-registration"),
        1,
        "the token is kept"
    );
    // 900 s later it has 100 s left: inside the two minutes it is replaced before it is used.
    h.clock.advance(Duration::from_secs(900));
    h.client.list_scale_sets().await.unwrap();
    assert_eq!(h.fake.count("POST /actions/runner-registration"), 2);
}
