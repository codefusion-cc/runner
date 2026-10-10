//! The long poll against the fake GitHub: what a listener does with messages, and with everything that can go
//! wrong between the Runner and GitHub. The fake is the only thing replaced; the GitHub App client, the HTTP
//! retries and the scale set client are the real ones.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use cf_fake_github::Options;
use cf_fake_github::httpd::Reply;
use cf_ghapp::RetryPolicy;
use cf_scaleset::{Backoff, Error, ScaleSet, listen, shutdown, supervise};
use serde_json::json;
use support::recorder::{Recorder, Seen};
use support::*;
use tokio::sync::Notify;

fn polls(h: &Harness) -> usize {
    h.fake
        .requests()
        .iter()
        .filter(|r| r.line.starts_with("GET /svc") && r.line.ends_with("/messages"))
        .count()
}

fn poll_line(set: &ScaleSet) -> String {
    format!("GET /svc/_apis/runtime/runnerscalesets/{}/messages", set.id)
}

fn job_message(id: i64, request: i64) -> Reply {
    let item = json!({"messageType": "JobAvailable", "runnerRequestId": request, "repositoryName": "app", "ownerName": "org",
        "jobDisplayName": "build", "workflowRunId": 9, "requestLabels": ["self-hosted", "cf-2g"]});
    Reply::json(
        &json!({"messageId": id, "messageType": "RunnerScaleSetJobMessages", "body": json!([item]).to_string(),
        "statistics": {"totalAvailableJobs": 1}}),
    )
}

// A message is handled first and acknowledged after: while the scaler is busy with it, the service still has it.
#[tokio::test]
async fn a_message_is_acknowledged_only_after_the_scaler_handled_it() {
    let h = harness().await;
    let (_, session) = with_session(&h).await;
    let gate = Arc::new(Notify::new());
    let mut scaler = Recorder::new(1);
    scaler.gate = Some(gate.clone());
    let seen = scaler.clone();
    let (_trigger, stop) = shutdown();
    let task = tokio::spawn({
        let session = session.clone();
        async move { listen(&session, &mut scaler, stop).await }
    });
    h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    seen.arrived.notified().await;
    assert_eq!(
        h.fake.scale_sets()[0].queued_messages,
        1,
        "acknowledged before it was handled"
    );
    gate.notify_one();
    until("the acknowledgement", || {
        h.fake.scale_sets()[0].queued_messages == 0
    })
    .await;
    assert!(
        matches!(seen.seen.lock().unwrap()[0], Seen::Initial(_)),
        "the counts of the session come first"
    );
    assert_eq!(seen.messages().len(), 1);
    task.abort();
}

#[tokio::test]
async fn a_message_delivered_twice_is_handled_once_and_acknowledged_again() {
    let h = harness().await;
    let (set, session) = with_session(&h).await;
    let mut scaler = Recorder::new(1);
    let seen = scaler.clone();
    let (trigger, stop) = shutdown();
    let task = tokio::spawn({
        let session = session.clone();
        async move { listen(&session, &mut scaler, stop).await }
    });
    h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    until("the first delivery", || seen.messages().len() == 1).await;
    let id = seen.messages()[0].id;
    h.fake.fail(&poll_line(&set), job_message(id, 1), 1);
    until("the second delivery", || {
        h.fake.count(&format!(
            "DELETE /svc/_apis/runtime/runnerscalesets/{}/messages/{id}",
            set.id
        )) == 2
    })
    .await;
    trigger.trigger();
    assert!(task.await.unwrap().is_ok());
    assert_eq!(seen.messages().len(), 1, "the scaler saw the same message twice");
}

#[tokio::test]
async fn a_scaler_that_fails_leaves_the_message_with_the_service_and_it_comes_again() {
    let h = harness().await;
    let (_, session) = with_session(&h).await;
    h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    let mut scaler = Recorder::new(1);
    let first_id = {
        let m = session.poll(0, 1).await.unwrap().unwrap();
        m.id
    };
    scaler.fail_on.lock().unwrap().push(first_id);
    let (_t, stop) = shutdown();
    let error = listen(&session, &mut scaler, stop.clone()).await.unwrap_err();
    assert!(matches!(error, Error::Scaler(_)), "{error:?}");
    assert_eq!(
        h.fake.scale_sets()[0].queued_messages,
        1,
        "a message the scaler refused was acknowledged"
    );
    // The next session (here: the same one, listening again) gets it again, and this time it goes through.
    let (trigger, stop) = shutdown();
    let seen = scaler.clone();
    let task = tokio::spawn({
        let session = session.clone();
        async move { listen(&session, &mut scaler, stop).await }
    });
    until("the redelivery", || seen.messages().len() == 1).await;
    until("the acknowledgement", || {
        h.fake.scale_sets()[0].queued_messages == 0
    })
    .await;
    trigger.trigger();
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test]
async fn a_body_that_is_not_a_list_of_jobs_is_acknowledged_and_its_counts_still_count() {
    let h = harness().await;
    let (set, session) = with_session(&h).await;
    let poison = Reply::json(
        &json!({"messageId": 1, "messageType": "RunnerScaleSetJobMessages", "body": "<html>bad gateway",
        "statistics": {"totalAssignedJobs": 3}}),
    );
    h.fake.fail(&poll_line(&set), poison, 1);
    let mut scaler = Recorder::new(1);
    let seen = scaler.clone();
    let (trigger, stop) = shutdown();
    let task = tokio::spawn({
        let session = session.clone();
        async move { listen(&session, &mut scaler, stop).await }
    });
    until("the poison message", || seen.messages().len() == 1).await;
    let m = &seen.messages()[0];
    assert_eq!(
        (
            m.skipped,
            m.statistics.unwrap().total_assigned_jobs,
            m.available.len()
        ),
        (1, 3, 0)
    );
    until("its acknowledgement", || h.fake.count("DELETE") == 1).await;
    trigger.trigger();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn items_that_are_not_understood_are_left_out_one_by_one() {
    let h = harness().await;
    let (set, session) = with_session(&h).await;
    let items = json!([
        {"messageType": "JobAvailable", "runnerRequestId": 5, "repositoryName": "app", "ownerName": "org", "queueTime": "yesterday-ish"},
        {"messageType": "JobAvailable", "runnerRequestId": "five"},
        {"messageType": "JobAvailable"},
        {"messageType": "SomethingNew", "runnerRequestId": 6},
        {"nothing": "typed"},
        "a string",
        {"messageType": "JobCompleted", "runnerRequestId": 5, "result": "succeeded", "runnerName": "air-j1"},
    ]);
    h.fake.fail(
        &poll_line(&set),
        Reply::json(
            &json!({"messageId": 1, "messageType": "RunnerScaleSetJobMessages", "body": items.to_string()}),
        ),
        1,
    );
    let m = session.poll(0, 1).await.unwrap().unwrap();
    assert_eq!(m.available.len(), 1, "the one with a bad date is still a job");
    assert!(m.available[0].queue_time.is_none());
    assert_eq!(m.completed.len(), 1);
    assert_eq!(m.skipped, 5);
}

// A session lost in the middle of a poll: the next poll finds none, and the worker opens a new one.
#[tokio::test]
async fn a_session_lost_mid_poll_is_opened_again_and_the_next_job_still_arrives() {
    let h = harness().await;
    let set = h
        .client
        .ensure_scale_set("air--cf-2g", &labels(&["self-hosted", "cf-2g"]))
        .await
        .unwrap();
    let scaler = Recorder::new(1);
    let seen = scaler.clone();
    let (trigger, stop) = shutdown();
    let failures = Arc::new(AtomicUsize::new(0));
    let task = tokio::spawn({
        let (client, stop, failures, scaler) =
            (h.client.clone(), stop.clone(), failures.clone(), scaler.clone());
        async move {
            supervise(
                stop.clone(),
                Backoff::new(
                    Duration::from_millis(10),
                    Duration::from_millis(40),
                    Duration::from_secs(60),
                ),
                move || {
                    let (client, stop, mut scaler) = (client.clone(), stop.clone(), scaler.clone());
                    async move {
                        let session = client.open_session(set.id, "air").await?;
                        let result = listen(&session, &mut scaler, stop).await;
                        let _ = session.close().await;
                        result
                    }
                },
                |error, _| {
                    assert!(matches!(error, Error::SessionLost(_)), "{error:?}");
                    failures.fetch_add(1, Ordering::SeqCst);
                },
            )
            .await;
        }
    });
    until("the first poll", || polls(&h) >= 1).await;
    h.fake.lose_session(set.id);
    until("the second session", || {
        h.fake.scale_sets()[0].has_session && failures.load(Ordering::SeqCst) == 1
    })
    .await;
    h.fake
        .queue_job(REPO, &["self-hosted", "cf-2g"], "after the loss");
    until("the job", || {
        seen.messages().iter().any(|m| m.available.len() == 1)
    })
    .await;
    trigger.trigger();
    task.await.unwrap();
    assert_eq!(failures.load(Ordering::SeqCst), 1);
    assert!(
        !h.fake.scale_sets()[0].has_session,
        "a stopped worker leaves its session open"
    );
}

#[tokio::test]
async fn a_token_that_expires_during_a_poll_is_replaced_and_the_wait_goes_on() {
    let h = harness().await;
    let (set, session) = with_session(&h).await;
    let first_token = {
        let polling = tokio::spawn({
            let session = session.clone();
            async move { session.poll(0, 1).await }
        });
        until("the poll to be in flight", || polls(&h) == 1).await;
        h.fake.expire_session_token(set.id);
        h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "late");
        let message = polling.await.unwrap().unwrap().expect("the job still arrives");
        assert_eq!(message.available.len(), 1);
        h.fake
            .requests()
            .iter()
            .find(|r| r.line == poll_line(&set))
            .unwrap()
            .authorization
            .clone()
            .unwrap()
    };
    assert_eq!(h.fake.count("PATCH"), 1, "one refresh");
    let last = h
        .fake
        .requests()
        .iter()
        .rev()
        .find(|r| r.line == poll_line(&set))
        .unwrap()
        .authorization
        .clone()
        .unwrap();
    assert_ne!(first_token, last, "the poll went on with the old token");
}

#[tokio::test]
async fn a_token_the_service_keeps_refusing_is_an_error_after_one_refresh() {
    let h = harness().await;
    let (set, session) = with_session(&h).await;
    h.fake.fail(
        &poll_line(&set),
        Reply::status_json(401, &json!({"message": "no"})),
        10,
    );
    let error = session.poll(0, 1).await.unwrap_err();
    assert_eq!(error.status(), Some(401));
    assert_eq!(h.fake.count("PATCH"), 1);
    assert_eq!(polls(&h), 2);
}

#[tokio::test]
async fn a_refresh_for_a_session_the_service_forgot_is_a_lost_session() {
    let h = harness().await;
    let (set, session) = with_session(&h).await;
    h.fake.expire_session_token(set.id);
    h.fake.lose_session(set.id);
    // 404 on the poll itself: no queue.
    assert!(matches!(
        session.poll(0, 1).await.unwrap_err(),
        Error::SessionLost(_)
    ));
    // 401 first, then the refresh finds no session.
    let (set2, session2) = {
        let set = h
            .client
            .ensure_scale_set("air--cf-4g", &labels(&["self-hosted", "cf-4g"]))
            .await
            .unwrap();
        let session = h.client.open_session(set.id, "air").await.unwrap();
        (set, session)
    };
    h.fake
        .fail(&poll_line(&set2), Reply::status_json(401, &json!({})), 1);
    h.fake.fail(
        &format!("PATCH /svc/_apis/runtime/runnerscalesets/{}/sessions", set2.id),
        Reply::status_json(404, &json!({})),
        1,
    );
    // The fault matcher is a substring, so the PATCH with the session id in its path matches.
    assert!(matches!(
        session2.poll(0, 1).await.unwrap_err(),
        Error::SessionLost(_)
    ));
}

#[tokio::test]
async fn every_status_the_poll_can_meet_is_a_result() {
    for (status, kind) in [
        (403u16, "refused"),
        (404, "lost"),
        (409, "conflict"),
        (400, "bad"),
        (418, "teapot"),
    ] {
        let h = harness().await;
        let (set, session) = with_session(&h).await;
        h.fake.fail(
            &poll_line(&set),
            Reply::status_json(status, &json!({"message": "x"})),
            1,
        );
        let error = session.poll(0, 1).await.unwrap_err();
        match status {
            404 => assert!(matches!(error, Error::SessionLost(_)), "{kind}: {error:?}"),
            _ => assert_eq!(error.status(), Some(status), "{kind}: {error:?}"),
        }
        assert_eq!(polls(&h), 1, "{kind}: a refusal is not tried again");
        assert!(!error.to_string().is_empty());
    }
}

#[tokio::test]
async fn server_errors_are_tried_again_a_few_times_and_then_reported() {
    let h = harness().await;
    let (set, session) = with_session(&h).await;
    h.fake.fail(&poll_line(&set), Reply::text(503, "unavailable"), 2);
    assert!(
        session.poll(0, 1).await.unwrap().is_none(),
        "two failures, the third try works"
    );
    assert_eq!(polls(&h), 3);
    h.fake.fail(&poll_line(&set), Reply::text(502, "bad gateway"), 10);
    let error = session.poll(0, 1).await.unwrap_err();
    assert_eq!(error.status(), Some(502));
    assert!(error.is_transient());
}

#[tokio::test]
async fn too_many_requests_waits_out_a_short_retry_after_and_reports_a_long_one() {
    let h = harness().await;
    let (set, session) = with_session(&h).await;
    h.fake.fail(
        &poll_line(&set),
        Reply::text(429, "slow down").with_header("retry-after", "1"),
        1,
    );
    let started = Instant::now();
    assert!(session.poll(0, 1).await.unwrap().is_none());
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "the wait was not waited out: {:?}",
        started.elapsed()
    );

    h.fake.fail(
        &poll_line(&set),
        Reply::text(429, "slow down").with_header("retry-after", "120"),
        1,
    );
    let started = Instant::now();
    let error = session.poll(0, 1).await.unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "a two minute wait was waited out inside the call"
    );
    assert_eq!(error.retry_after(), Some(Duration::from_secs(120)));
    // The worker's wait is never shorter than what GitHub asked for.
    let mut backoff = Backoff::new(
        Duration::from_millis(10),
        Duration::from_millis(40),
        Duration::from_secs(60),
    );
    assert!(backoff.next_wait(Duration::ZERO, &error, 0.0) >= Duration::from_secs(120));
}

#[tokio::test]
async fn a_dropped_connection_a_hung_one_and_a_cut_body_are_transport_errors() {
    let h = harness_with(
        Options::default(),
        RetryPolicy { retries: 1, ..fast() },
        Duration::from_millis(400),
    )
    .await;
    let (set, session) = with_session(&h).await;
    let line = poll_line(&set);
    h.fake.fail(&line, Reply::Drop, 2);
    let error = session.poll(0, 1).await.unwrap_err();
    assert!(
        matches!(error, Error::Api(cf_ghapp::Error::Transport(_))),
        "{error:?}"
    );

    h.fake.fail(&line, Reply::Hang, 2);
    let started = Instant::now();
    let error = session.poll(0, 1).await.unwrap_err();
    assert!(
        matches!(error, Error::Api(cf_ghapp::Error::Transport(_))),
        "{error:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );

    let body = job_message(1, 1);
    let Reply::Response { body, .. } = body else {
        unreachable!()
    };
    h.fake.fail(
        &line,
        Reply::Truncated {
            status: 200,
            sent: body.len() / 2,
            body,
        },
        2,
    );
    let error = session.poll(0, 1).await.unwrap_err();
    assert!(
        matches!(&error, Error::Api(cf_ghapp::Error::Transport(why)) if why.contains("ended early")),
        "{error:?}"
    );
    assert!(error.is_transient());
}

#[tokio::test]
async fn a_slow_answer_inside_the_timeout_is_an_answer() {
    let h = harness().await;
    let (set, session) = with_session(&h).await;
    h.fake.fail(
        &poll_line(&set),
        Reply::After(Duration::from_millis(600), Box::new(job_message(1, 1))),
        1,
    );
    let message = session.poll(0, 1).await.unwrap().unwrap();
    assert_eq!(message.available.len(), 1);
}

#[tokio::test]
async fn an_answer_that_is_not_json_is_a_decode_error_and_the_message_is_not_acknowledged() {
    let h = harness().await;
    let (set, session) = with_session(&h).await;
    h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    h.fake.fail(
        &poll_line(&set),
        Reply::text(200, "<html>captive portal</html>"),
        1,
    );
    let mut scaler = Recorder::new(1);
    let (_t, stop) = shutdown();
    let error = listen(&session, &mut scaler, stop).await.unwrap_err();
    assert!(
        matches!(error, Error::Api(cf_ghapp::Error::Decode { .. })),
        "{error:?}"
    );
    assert_eq!(h.fake.count("DELETE"), 0);
    assert_eq!(h.fake.scale_sets()[0].queued_messages, 1);
}

#[tokio::test]
async fn shutdown_ends_a_wait_at_once() {
    let h = harness_with(
        Options {
            poll_timeout: Duration::from_secs(30),
            ..Options::default()
        },
        fast(),
        Duration::from_secs(60),
    )
    .await;
    let (_, session) = with_session(&h).await;
    let mut scaler = Recorder::new(1);
    let (trigger, stop) = shutdown();
    let task = tokio::spawn({
        let session = session.clone();
        async move { listen(&session, &mut scaler, stop).await }
    });
    until("the poll", || polls(&h) == 1).await;
    let started = Instant::now();
    trigger.trigger();
    assert!(task.await.unwrap().is_ok());
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn shutdown_during_a_message_finishes_and_acknowledges_it_first() {
    let h = harness().await;
    let (_, session) = with_session(&h).await;
    let gate = Arc::new(Notify::new());
    let mut scaler = Recorder::new(1);
    scaler.gate = Some(gate.clone());
    let seen = scaler.clone();
    let (trigger, stop) = shutdown();
    let mut task = tokio::spawn({
        let session = session.clone();
        async move { listen(&session, &mut scaler, stop).await }
    });
    h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    seen.arrived.notified().await;
    trigger.trigger();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut task)
            .await
            .is_err(),
        "stopped with a message in the scaler's hands"
    );
    gate.notify_one();
    assert!(task.await.unwrap().is_ok());
    assert_eq!(seen.messages().len(), 1, "the message in hand was dropped");
    assert_eq!(
        h.fake.scale_sets()[0].queued_messages,
        0,
        "and it was not acknowledged"
    );
}

#[tokio::test]
async fn the_capacity_the_scaler_names_goes_to_the_service_with_the_next_wait() {
    let h = harness().await;
    let (_, session) = with_session(&h).await;
    let mut scaler = Recorder::new(3);
    let (trigger, stop) = shutdown();
    let task = tokio::spawn({
        let session = session.clone();
        async move { listen(&session, &mut scaler, stop).await }
    });
    until("two polls", || polls(&h) >= 2).await;
    trigger.trigger();
    task.await.unwrap().unwrap();
    let capacities: Vec<_> = h
        .fake
        .requests()
        .iter()
        .filter_map(|r| r.max_capacity.clone())
        .collect();
    assert!(
        !capacities.is_empty() && capacities.iter().all(|c| c == "3"),
        "{capacities:?}"
    );
}

#[tokio::test]
async fn the_wait_after_a_failure_grows_is_capped_and_starts_over_after_a_good_session() {
    let error = Error::SessionLost("x".into());
    let mut b = Backoff::standard();
    let waits: Vec<u64> = (0..9)
        .map(|_| b.next_wait(Duration::from_secs(1), &error, 1.0).as_secs())
        .collect();
    assert_eq!(waits, vec![5, 10, 20, 40, 80, 160, 300, 300, 300]);
    assert_eq!(
        b.next_wait(Duration::from_secs(301), &error, 1.0),
        Duration::from_secs(5),
        "a session that worked for five minutes"
    );
    let mut b = Backoff::standard();
    assert_eq!(
        b.next_wait(Duration::ZERO, &error, 0.0),
        Duration::from_millis(2500),
        "half at the least"
    );
    assert_eq!(
        b.next_wait(Duration::ZERO, &error, 7.0),
        Duration::from_secs(10),
        "jitter outside 0..1 is clamped"
    );
}

#[tokio::test]
async fn a_worker_waiting_to_try_again_stops_at_once_when_asked() {
    let (trigger, stop) = shutdown();
    let task = tokio::spawn(supervise(
        stop,
        Backoff::new(
            Duration::from_secs(60),
            Duration::from_secs(300),
            Duration::from_secs(300),
        ),
        || async { Err(Error::SessionExists) },
        |_, _| {},
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;
    let started = Instant::now();
    trigger.trigger();
    task.await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
}
