//! The administrator's side and one session's calls, against the fake GitHub.

mod support;

use std::sync::atomic::Ordering;
use std::time::Duration;

use cf_fake_github::JobState;
use cf_fake_github::httpd::Reply;
use cf_scaleset::{Error, Message};
use support::*;

#[tokio::test]
async fn a_scale_set_is_created_once_and_found_again_with_its_labels_set() {
    let h = harness().await;
    let first = h
        .client
        .ensure_scale_set("mac-a--cf-2g", &labels(&["Self-Hosted", "CF-2G"]))
        .await
        .unwrap();
    let again = h
        .client
        .ensure_scale_set("mac-a--cf-2g", &labels(&["cf-2g", "self-hosted"]))
        .await
        .unwrap();
    assert_eq!(first.id, again.id);
    assert_eq!(h.fake.scale_sets().len(), 1);
    assert_eq!(h.fake.scale_sets()[0].labels, vec!["cf-2g", "self-hosted"]);
    // The labels changed: the set is updated, not duplicated.
    let changed = h
        .client
        .ensure_scale_set("mac-a--cf-2g", &labels(&["self-hosted", "cf-4g"]))
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
        .generate_jit_config(set.id, "mac-a-j0000001", "_work")
        .await
        .unwrap();
    assert_eq!(jit.runner.name, "mac-a-j0000001");
    assert!(!jit.encoded.expose().is_empty());
    assert_eq!(h.fake.runners(), vec!["mac-a-j0000001"]);
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
        .generate_jit_config(set.id, "mac-a-jabc", "_work")
        .await
        .unwrap();
    h.fake.start_job("mac-a-jabc");
    h.fake.complete_job("mac-a-jabc", "failed");
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
    assert_eq!(seen.started[0].runner_name, "mac-a-jabc");
    assert_eq!(seen.completed[0].result, "failed");
    assert_eq!(seen.completed[0].runner_name, "mac-a-jabc");
    assert_eq!(seen.completed[0].workflow_run_id, 7000 + request);
    assert!(seen.completed[0].queue_time.is_some());
    let _ = jit;
}

#[tokio::test]
async fn a_second_session_of_the_machine_is_refused_until_the_first_is_closed() {
    let h = harness().await;
    let (set, first) = with_session(&h).await;
    assert_eq!(
        h.client.open_session(set.id, "mac-a").await.unwrap_err(),
        Error::SessionExists
    );
    first.close().await.unwrap();
    assert!(!h.fake.scale_sets()[0].has_session);
    h.client.open_session(set.id, "mac-a").await.unwrap();
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

#[tokio::test]
async fn a_clock_that_runs_ahead_keeps_the_admin_token() {
    let h = harness_with(
        cf_fake_github::Options {
            admin_token_ttl: Duration::from_secs(1000),
            ..Default::default()
        },
        fast(),
        Duration::from_secs(10),
    )
    .await;
    // This machine's clock is half an hour ahead of GitHub's: read by it, every admin token ran out long ago.
    h.fake.set_clock_offset(-1800);
    for _ in 0..3 {
        h.client.list_scale_sets().await.unwrap();
    }
    assert_eq!(
        h.fake.count("POST /actions/runner-registration"),
        1,
        "a token good for a quarter of an hour was thrown away"
    );
}

#[tokio::test]
async fn callers_refused_together_share_one_new_admin_token() {
    let h = harness().await;
    h.client.list_scale_sets().await.unwrap();
    h.fake.revoke_admin_tokens();
    let (a, b, c, d) = tokio::join!(
        h.client.list_scale_sets(),
        h.client.list_scale_sets(),
        h.client.list_scale_sets(),
        h.client.list_scale_sets(),
    );
    for answer in [a, b, c, d] {
        answer.unwrap();
    }
    assert_eq!(
        h.fake.count("POST /actions/runner-registration"),
        2,
        "each refused caller registered again and threw the others' token away"
    );
}

#[tokio::test]
async fn a_registration_that_names_another_host_is_refused_and_no_token_goes_there() {
    let h = harness().await;
    let (other, visits) = elsewhere().await;
    h.fake.send_the_service_to(&format!("{}/svc", other.url()));
    let error = h.client.list_scale_sets().await.unwrap_err();
    assert!(
        matches!(error, Error::Api(cf_ghapp::Error::Refused(_))),
        "{error:?}"
    );
    assert!(!error.is_transient(), "a refusal is not tried again");
    assert_eq!(
        visits.load(Ordering::SeqCst),
        0,
        "the admin token went to another host"
    );
    // The same goes for a host on the internet that only looks like GitHub's.
    h.fake
        .send_the_service_to("https://actions.githubusercontent.com.example.org/svc");
    let error = h.client.list_scale_sets().await.unwrap_err();
    assert!(
        matches!(error, Error::Api(cf_ghapp::Error::Refused(_))),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_session_whose_queue_is_on_another_host_is_refused_and_closed() {
    let h = harness().await;
    let set = h
        .client
        .ensure_scale_set("mac-a--cf-2g", &labels(&["self-hosted", "cf-2g"]))
        .await
        .unwrap();
    let (other, visits) = elsewhere().await;
    h.fake.send_the_queue_to(&format!("{}/messages", other.url()));
    let error = h.client.open_session(set.id, "mac-a").await.unwrap_err();
    assert!(
        matches!(error, Error::Api(cf_ghapp::Error::Refused(_))),
        "{error:?}"
    );
    assert_eq!(visits.load(Ordering::SeqCst), 0);
    assert!(
        !h.fake.scale_sets()[0].has_session,
        "the refused session was left open at the service"
    );
}

#[tokio::test]
async fn a_refreshed_session_cannot_move_its_queue_to_another_host() {
    let h = harness().await;
    let (set, session) = with_session(&h).await;
    let (other, visits) = elsewhere().await;
    h.fake.send_the_queue_to(&format!("{}/messages", other.url()));
    h.fake.expire_session_token(set.id);
    h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    let error = session.poll(0, 1).await.unwrap_err();
    assert!(
        matches!(error, Error::Api(cf_ghapp::Error::Refused(_))),
        "{error:?}"
    );
    assert_eq!(
        visits.load(Ordering::SeqCst),
        0,
        "the session token went to another host"
    );
}

#[tokio::test]
async fn a_job_named_twice_in_the_answer_is_acquired_once_and_a_stranger_not_at_all() {
    let h = harness().await;
    let (set, session) = with_session(&h).await;
    let request = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    session.poll(0, 1).await.unwrap().expect("the offer");
    h.fake.fail(
        &format!("POST /svc/_apis/runtime/runnerscalesets/{}/acquirejobs", set.id),
        Reply::json(&serde_json::json!({"count": 3, "value": [request, 4242, request]})),
        1,
    );
    assert_eq!(session.acquire_jobs(&[request]).await.unwrap(), vec![request]);
}

#[tokio::test]
async fn an_answer_of_the_wrong_shape_keeps_its_text_out_of_the_error() {
    let h = harness().await;
    // serde names the value it met where it expected another: here that value is the service's token.
    h.fake.fail(
        "POST /actions/runner-registration",
        Reply::text(200, r#""admin-token-leaked""#),
        1,
    );
    let error = h
        .client
        .ensure_scale_set("mac-a--cf-2g", &labels(&["self-hosted", "cf-2g"]))
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Api(cf_ghapp::Error::Decode { .. })),
        "{error:?}"
    );
    let said = format!("{error} {error:?}");
    assert!(!said.contains("admin-token-leaked"), "{said}");
}

#[tokio::test]
async fn a_configuration_that_does_not_name_its_runner_is_refused_and_its_registration_removed() {
    let h = harness().await;
    let (set, _session) = with_session(&h).await;
    // The service has the Actions runner; its answers leave out which one it is, or the configuration.
    for answer in [
        serde_json::json!({"encodedJITConfig": "abc"}),
        serde_json::json!({"runner": null, "encodedJITConfig": "abc"}),
        serde_json::json!({"runner": {"id": 0, "name": "mac-a-cf-2g-who"}, "encodedJITConfig": "abc"}),
        serde_json::json!({"runner": {"id": 7, "name": "mac-a-cf-2g-who"}, "encodedJITConfig": ""}),
    ] {
        h.client
            .generate_jit_config(set.id, "mac-a-cf-2g-who", "_work")
            .await
            .expect("registered");
        assert_eq!(h.fake.runners(), vec!["mac-a-cf-2g-who".to_string()]);
        h.fake.fail("generatejitconfig", Reply::json(&answer), 1);
        let error = h
            .client
            .generate_jit_config(set.id, "mac-a-cf-2g-who", "_work")
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::Api(cf_ghapp::Error::Decode { .. })),
            "{answer}: {error:?}"
        );
        assert_eq!(h.fake.runners(), Vec::<String>::new(), "{answer}");
    }
}

#[tokio::test]
async fn a_registration_whose_answer_is_lost_leaves_no_runner_without_a_machine() {
    let h = harness().await;
    let (set, _session) = with_session(&h).await;
    // The service registers the Actions runner, the answer is lost, and the next try meets the name it just took.
    h.fake.lose_answer("generatejitconfig", 1);
    let error = h
        .client
        .generate_jit_config(set.id, "mac-a-cf-2g-lost", "_work")
        .await
        .unwrap_err();
    assert_eq!(error.status(), Some(409), "{error:?}");
    assert_eq!(h.fake.runners(), Vec::<String>::new());

    // Every try loses its answer: the call fails as a network error, and still nothing stays behind.
    h.fake.lose_answer("generatejitconfig", 3);
    let error = h
        .client
        .generate_jit_config(set.id, "mac-a-cf-2g-gone", "_work")
        .await
        .unwrap_err();
    assert!(error.is_transient(), "{error:?}");
    assert_eq!(h.fake.runners(), Vec::<String>::new());

    // A refusal that registered nothing looks for nothing.
    let lookups = h.fake.count("GET /svc/_apis/distributedtask/pools/0/agents");
    h.fake.fail(
        "generatejitconfig",
        Reply::status_json(403, &serde_json::json!({"message": "no"})),
        1,
    );
    let error = h
        .client
        .generate_jit_config(set.id, "mac-a-cf-2g-no", "_work")
        .await
        .unwrap_err();
    assert_eq!(
        (error.status(), error.is_transient()),
        (Some(403), false),
        "{error:?}"
    );
    assert!(lookups > 0);
    assert_eq!(
        h.fake.count("GET /svc/_apis/distributedtask/pools/0/agents"),
        lookups
    );

    // Nor does a limit, which refused the call before anything was made.
    h.fake
        .fail_until_recovered("generatejitconfig", Reply::text(429, "slow down"));
    let error = h
        .client
        .generate_jit_config(set.id, "mac-a-cf-2g-limit", "_work")
        .await
        .unwrap_err();
    assert_eq!(error.status(), Some(429), "{error:?}");
    h.fake.recover("generatejitconfig");
    assert_eq!(
        h.fake.count("GET /svc/_apis/distributedtask/pools/0/agents"),
        lookups
    );

    // Nor a 409 that says when to ask again: the console's answer while no trusted job waits, which made nothing.
    // A 409 without it is a name already taken, whose registration may be the one just made.
    h.fake.fail(
        "generate-jitconfig",
        Reply::status_json(409, &serde_json::json!({"error": "no_trusted_job"}))
            .with_header("Retry-After", "15"),
        1,
    );
    let error = h
        .client
        .generate_plain_jit_config("mac-a-j0000002", &["trusted".to_string()])
        .await
        .unwrap_err();
    assert_eq!(
        (error.status(), error.retry_after()),
        (Some(409), Some(Duration::from_secs(15))),
        "{error:?}"
    );
    assert_eq!(
        h.fake.count("GET /svc/_apis/distributedtask/pools/0/agents"),
        lookups
    );
    h.fake.fail(
        "generate-jitconfig",
        Reply::status_json(409, &serde_json::json!({"message": "taken"})),
        1,
    );
    let error = h
        .client
        .generate_plain_jit_config("mac-a-j0000003", &["trusted".to_string()])
        .await
        .unwrap_err();
    assert_eq!(error.status(), Some(409), "{error:?}");
    assert_eq!(
        h.fake.count("GET /svc/_apis/distributedtask/pools/0/agents"),
        lookups + 1
    );

    // And a name that works is found by its name.
    let jit = h
        .client
        .generate_jit_config(set.id, "mac-a-cf-2g-ok", "_work")
        .await
        .unwrap();
    assert_eq!(
        h.client.runner_by_name("mac-a-cf-2g-ok").await.unwrap(),
        Some(jit.runner.clone())
    );
    assert_eq!(h.client.runner_by_name("mac-a-cf-2g-nobody").await.unwrap(), None);
}
