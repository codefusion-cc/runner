//! The administrator's side and one session's calls, through the fake console to the fake GitHub.

mod support;

use std::sync::atomic::Ordering;

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
    assert!(polls.iter().all(|r| !r.admin), "the session's own token");

    assert_eq!(session.acquire_jobs(&[request]).await.unwrap(), vec![request]);
    assert_eq!(h.fake.job_state(request), Some(JobState::Acquired(set.id)));

    let jit = h
        .client
        .generate_jit_config(set.id, "mac-a-j0000001")
        .await
        .unwrap();
    assert_eq!(jit.runner.name, "mac-a-j0000001");
    assert!(!jit.encoded.expose().is_empty());
    assert_eq!(h.fake.runners(), vec!["mac-a-j0000001"]);
    assert!(
        !format!("{jit:?}").contains(jit.encoded.expose()),
        "the configuration is a credential"
    );

    assert!(h.client.remove_runner(jit.runner.id).await.unwrap());
    assert!(h.fake.runners().is_empty());
    assert!(
        !h.client.remove_runner(jit.runner.id).await.unwrap(),
        "already gone"
    );
    assert_eq!(registrations(&h.fake), 0);
    assert_eq!(admin_requests_from_machines(&h.fake), Vec::<String>::new());
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
        .generate_jit_config(set.id, "mac-a-j0000abc")
        .await
        .unwrap();
    h.fake.start_job("mac-a-j0000abc");
    h.fake.complete_job("mac-a-j0000abc", "failed");
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
    assert_eq!(seen.started[0].runner_name, "mac-a-j0000abc");
    assert_eq!(seen.completed[0].result, "failed");
    assert_eq!(seen.completed[0].runner_name, "mac-a-j0000abc");
    assert_eq!(seen.completed[0].workflow_run_id, 7000 + request);
    assert!(seen.completed[0].queue_time.is_some());
    let _ = jit;
}

#[tokio::test]
async fn a_second_session_of_the_machine_is_refused_until_the_first_is_closed() {
    let h = harness().await;
    let (set, first) = with_session(&h).await;
    assert_eq!(
        h.client.open_session(set.id).await.unwrap_err(),
        Error::SessionExists
    );
    first.close().await.unwrap();
    assert!(!h.fake.scale_sets()[0].has_session);
    h.client.open_session(set.id).await.unwrap();
}

#[tokio::test]
async fn acquiring_goes_to_the_service_address_with_the_session_token_only() {
    let h = harness().await;
    let (set, session) = with_session(&h).await;
    let request = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    session.poll(0, 1).await.unwrap().expect("the offer");
    assert_eq!(session.acquire_jobs(&[request]).await.unwrap(), vec![request]);
    let acquired = h
        .fake
        .requests()
        .into_iter()
        .filter(|r| r.line.ends_with("/acquirejobs"))
        .collect::<Vec<_>>();
    assert_eq!(acquired.len(), 1);
    assert_eq!(
        acquired[0].line,
        format!("POST /svc/_apis/runtime/runnerscalesets/{}/acquirejobs", set.id),
        "at the serviceUrl the console named"
    );
    assert!(!acquired[0].admin && !acquired[0].from_console);
}

#[tokio::test]
async fn the_machine_never_asks_for_a_registration_or_holds_an_admin_token() {
    let h = harness().await;
    let (set, session) = with_session(&h).await;
    let request = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    session.poll(0, 1).await.unwrap().expect("the offer");
    session.acquire_jobs(&[request]).await.unwrap();
    let jit = h
        .client
        .generate_jit_config(set.id, "mac-a-j00000f1")
        .await
        .unwrap();
    h.fake.start_job(&jit.runner.name);
    h.fake.complete_job(&jit.runner.name, "succeeded");
    h.client.remove_runner_named("mac-a-j00000f1").await.unwrap();
    session.close().await.unwrap();
    assert!(h.client.delete_scale_set(set.id).await.unwrap());

    assert_eq!(
        registrations(&h.fake),
        0,
        "nothing asked GitHub for a registration over HTTP"
    );
    assert_eq!(admin_requests_from_machines(&h.fake), Vec::<String>::new());
    let console_calls = h.console.github_calls();
    assert!(
        console_calls.iter().all(|c| c.route != "registration"),
        "{console_calls:?}"
    );
    let kinds = h
        .console
        .scaleset_audit()
        .into_iter()
        .map(|row| (row.kind, row.outcome))
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            ("set-create", "ok"),
            ("session-open", "ok"),
            ("jit", "ok"),
            ("remove", "ok"),
            ("session-close", "ok"),
            ("set-delete", "ok"),
        ]
    );
}

#[tokio::test]
async fn an_admin_token_the_service_stops_taking_is_replaced_by_the_console_out_of_sight() {
    let h = harness().await;
    h.client.list_scale_sets().await.unwrap();
    h.fake.revoke_admin_tokens();
    let (a, b) = tokio::join!(h.client.list_scale_sets(), h.client.list_scale_sets());
    a.unwrap();
    b.unwrap();
    assert_eq!(admin_requests_from_machines(&h.fake), Vec::<String>::new());
}

#[tokio::test]
async fn a_service_address_that_is_not_githubs_is_refused_by_the_console_and_no_token_goes_there() {
    let h = harness().await;
    let (other, visits) = elsewhere().await;
    h.fake.send_the_service_to(&format!("{}/svc", other.url()));
    let error = h.client.list_scale_sets().await.unwrap_err();
    assert!(
        matches!(error, Error::Api(cf_ghapp::Error::Transport(_))),
        "{error:?}"
    );
    assert_eq!(
        visits.load(Ordering::SeqCst),
        0,
        "the admin token went to another host"
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
    let error = h.client.open_session(set.id).await.unwrap_err();
    assert!(
        matches!(error, Error::Api(cf_ghapp::Error::Refused(_))),
        "{error:?}"
    );
    assert!(!error.is_transient(), "a refusal is not tried again");
    assert_eq!(visits.load(Ordering::SeqCst), 0);
    assert!(
        !h.fake.scale_sets()[0].has_session,
        "the refused session was left open at the service"
    );
}

/// A console answer as the console writes one for `scaleset/session/open`, with `queue` and `service` as addresses.
fn opened(queue: &str, service: &str) -> Reply {
    Reply::json(&serde_json::json!({
        "sessionId": "5f0c-1",
        "messageQueueUrl": queue,
        "messageQueueAccessToken": "queue-token-not-for-elsewhere",
        "serviceUrl": service,
        "statistics": {
            "totalAvailableJobs": 0, "totalAcquiredJobs": 0, "totalAssignedJobs": 0, "totalRunningJobs": 0,
            "totalRegisteredRunners": 0, "totalBusyRunners": 0, "totalIdleRunners": 0
        }
    }))
}

#[tokio::test]
async fn a_console_answer_naming_another_host_is_refused_and_the_session_token_goes_nowhere() {
    let h = harness().await;
    let set = h
        .client
        .ensure_scale_set("mac-a--cf-2g", &labels(&["self-hosted", "cf-2g"]))
        .await
        .unwrap();
    let (other, visits) = elsewhere().await;
    let service = format!("{}/svc", h.fake.url());
    for (queue, service) in [
        (format!("{}/messages", other.url()), service.clone()),
        (format!("{service}/messages"), format!("{}/svc", other.url())),
        (
            "https://actions.githubusercontent.com.example.org/messages".to_string(),
            service.clone(),
        ),
        (
            format!("{service}/messages"),
            "http://pipelines.actions.githubusercontent.com/svc".into(),
        ),
    ] {
        h.console.answer_next(opened(&queue, &service));
        let error = h.client.open_session(set.id).await.unwrap_err();
        assert!(
            matches!(error, Error::Api(cf_ghapp::Error::Refused(_))),
            "{queue} {service}: {error:?}"
        );
        assert!(!format!("{error:?}").contains("queue-token-not-for-elsewhere"));
    }
    assert_eq!(
        visits.load(Ordering::SeqCst),
        0,
        "a session token went to another host"
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
async fn the_consoles_refusals_keep_their_meaning() {
    let h = harness().await;
    let set = h
        .client
        .ensure_scale_set("mac-a--cf-2g", &labels(&["self-hosted", "cf-2g"]))
        .await
        .unwrap();
    let refusal = |status: u16, code: &str| {
        Reply::status_json(
            status,
            &serde_json::json!({"error": format!("refused: {code}"), "code": code}),
        )
    };

    h.console.answer_next(refusal(409, "session_exists"));
    assert_eq!(
        h.client.open_session(set.id).await.unwrap_err(),
        Error::SessionExists
    );

    h.console.answer_next(refusal(403, "not_yours"));
    let error = h.client.delete_scale_set(set.id).await.unwrap_err();
    assert_eq!(
        (error.status(), error.is_transient()),
        (Some(403), false),
        "{error:?}"
    );

    h.console.answer_next(refusal(422, "runner_busy"));
    let error = h.client.remove_runner(77).await.unwrap_err();
    assert_eq!(
        (error.status(), error.is_transient()),
        (Some(422), false),
        "{error:?}"
    );

    // A wait longer than the client waits out by itself comes back with the error.
    h.console
        .answer_next(refusal(503, "github_busy").with_header("retry-after", "60"));
    let error = h.client.list_scale_sets().await.unwrap_err();
    assert_eq!(error.status(), Some(503), "{error:?}");
    assert_eq!(error.retry_after(), Some(std::time::Duration::from_secs(60)));
    assert!(error.is_transient());

    h.console.answer_next(Reply::status_json(
        403,
        &serde_json::json!({"error": "GitHub through the console is off", "code": "broker_off"}),
    ));
    let error = h.client.list_scale_sets().await.unwrap_err();
    assert_eq!(
        (error.status(), error.is_transient()),
        (Some(403), false),
        "{error:?}"
    );

    h.console.answer_next(Reply::status_json(
        404,
        &serde_json::json!({"error": "Not found"}),
    ));
    // A console without the route is older than this Runner, not a set that is gone: the host lists them itself
    // (tests/older_console.rs).
    let sets = h.client.list_scale_sets().await.expect("listed by the host");
    assert_eq!(sets.len(), 1, "{sets:?}");
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
    // serde names the value it met where it expected another: here that value is a token.
    h.console
        .answer_next(Reply::json(&serde_json::json!({"scaleSets": "token-leaked"})));
    let error = h.client.list_scale_sets().await.unwrap_err();
    assert!(
        matches!(error, Error::Api(cf_ghapp::Error::Decode { .. })),
        "{error:?}"
    );
    let said = format!("{error} {error:?}");
    assert!(!said.contains("token-leaked"), "{said}");
}

#[tokio::test]
async fn a_configuration_that_does_not_name_its_runner_is_refused_and_its_registration_removed() {
    let h = harness().await;
    let (set, _session) = with_session(&h).await;
    // The service has the Actions runner; its answers leave out which one it is, or the configuration.
    for answer in [
        serde_json::json!({"encodedJITConfig": "abc"}),
        serde_json::json!({"runner": null, "encodedJITConfig": "abc"}),
        serde_json::json!({"runner": {"id": 0, "name": "mac-a-j0000c0f"}, "encodedJITConfig": "abc"}),
        serde_json::json!({"runner": {"id": 7, "name": "mac-a-j0000c0f"}, "encodedJITConfig": ""}),
    ] {
        h.client
            .generate_jit_config(set.id, "mac-a-j0000c0f")
            .await
            .expect("registered");
        assert_eq!(h.fake.runners(), vec!["mac-a-j0000c0f".to_string()]);
        h.fake.fail("generatejitconfig", Reply::json(&answer), 1);
        let error = h
            .client
            .generate_jit_config(set.id, "mac-a-j0000c0f")
            .await
            .unwrap_err();
        assert_eq!(error.status(), None, "{answer}: {error:?}");
        assert_eq!(h.fake.runners(), Vec::<String>::new(), "{answer}");
    }
    // The console's own answer may be what is wrong.
    for answer in [
        serde_json::json!({"runnerId": 0, "encodedJitConfig": "abc"}),
        serde_json::json!({"runnerId": 9, "encodedJitConfig": ""}),
        serde_json::json!({"encodedJitConfig": "abc"}),
    ] {
        h.console.answer_next(Reply::json(&answer));
        let error = h
            .client
            .generate_jit_config(set.id, "mac-a-j0000c0f")
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::Api(cf_ghapp::Error::Decode { .. })),
            "{answer}: {error:?}"
        );
    }
}

#[tokio::test]
async fn a_registration_whose_answer_is_lost_leaves_no_runner_without_a_machine() {
    let h = harness().await;
    let (set, _session) = with_session(&h).await;
    let jits = |h: &Harness| {
        h.console
            .github_calls()
            .iter()
            .filter(|c| c.route == "scaleset/jit")
            .count()
    };

    // The service registers the Actions runner and the answer is lost: the console says GitHub is busy, the call is
    // not sent again (it may have registered one), and the registration is looked for and removed.
    h.fake.lose_answer("generatejitconfig", 1);
    let before = jits(&h);
    let error = h
        .client
        .generate_jit_config(set.id, "mac-a-j000105e")
        .await
        .unwrap_err();
    assert!(error.is_transient(), "{error:?}");
    assert_eq!(jits(&h) - before, 1, "sent once");
    assert_eq!(h.fake.runners(), Vec::<String>::new());

    // A name the service already has is taken: whatever holds it has no machine.
    h.client
        .generate_jit_config(set.id, "mac-a-j00007a4")
        .await
        .unwrap();
    let error = h
        .client
        .generate_jit_config(set.id, "mac-a-j00007a4")
        .await
        .unwrap_err();
    assert_eq!(error.status(), Some(409), "{error:?}");
    assert_eq!(h.fake.runners(), Vec::<String>::new());

    // Another refusal of GitHub's (the console answers 409 without `name_taken`) registered nothing.
    let lookups = h.fake.count("GET /svc/_apis/distributedtask/pools/0/agents");
    assert!(lookups > 0);
    h.fake.fail(
        "generatejitconfig",
        Reply::status_json(403, &serde_json::json!({"message": "no"})),
        1,
    );
    let error = h
        .client
        .generate_jit_config(set.id, "mac-a-j0000403")
        .await
        .unwrap_err();
    assert_eq!(
        (error.status(), error.is_transient()),
        (Some(409), false),
        "{error:?}"
    );
    assert_eq!(
        h.fake.count("GET /svc/_apis/distributedtask/pools/0/agents"),
        lookups
    );

    // Nor did the console's own limit, which refused the call before GitHub was asked.
    h.console.answer_next(
        Reply::status_json(
            429,
            &serde_json::json!({"error": "Too many", "code": "rate_limited"}),
        )
        .with_header("retry-after", "300"),
    );
    let error = h
        .client
        .generate_jit_config(set.id, "mac-a-j0000429")
        .await
        .unwrap_err();
    assert_eq!(error.status(), Some(429), "{error:?}");
    assert_eq!(
        h.fake.count("GET /svc/_apis/distributedtask/pools/0/agents"),
        lookups
    );

    // Nor a 409 that says when to ask again: the console's answer while no trusted job waits, which made nothing.
    let removes = |h: &Harness| {
        h.console
            .github_calls()
            .iter()
            .filter(|c| c.route == "scaleset/remove")
            .count()
    };
    let before = removes(&h);
    h.console.answer_next(
        Reply::status_json(
            409,
            &serde_json::json!({"error": "No trusted job is queued", "code": "no_trusted_job"}),
        )
        .with_header("retry-after", "15"),
    );
    let error = h
        .client
        .generate_jit_config(set.id, "mac-a-j0000409")
        .await
        .unwrap_err();
    assert_eq!(
        (error.status(), error.retry_after()),
        (Some(409), Some(std::time::Duration::from_secs(15))),
        "{error:?}"
    );
    assert_eq!(removes(&h), before, "nothing was made, so nothing is looked for");
}

/// A trusted host's Actions runner outside any scale set: the console's 409 with a wait (`no_trusted_job`) made
/// nothing, so nothing is looked for; a 409 without one is a name GitHub has, whose registration may be the one just
/// made, so it is removed.
#[tokio::test]
async fn a_plain_registration_that_waits_for_a_trusted_job_leaves_nothing_to_remove() {
    let h = harness().await;
    let removes = |h: &Harness| {
        h.console
            .github_calls()
            .iter()
            .filter(|c| c.route == "scaleset/remove")
            .count()
    };
    let trusted = labels(&["trusted"]);
    h.console.answer_next(
        Reply::status_json(
            409,
            &serde_json::json!({"error": "No trusted job is queued", "code": "no_trusted_job"}),
        )
        .with_header("retry-after", "15"),
    );
    let error = h
        .client
        .generate_plain_jit_config("mac-a-j0000002", &trusted)
        .await
        .unwrap_err();
    assert_eq!(
        (error.status(), error.retry_after()),
        (Some(409), Some(std::time::Duration::from_secs(15))),
        "{error:?}"
    );
    assert_eq!(removes(&h), 0, "nothing was made, so nothing is looked for");

    // Past the wait, a 409 without one is a name already taken.
    h.clock.advance(std::time::Duration::from_secs(15));
    h.console.answer_next(Reply::status_json(
        409,
        &serde_json::json!({"error": "Taken", "code": "name_taken"}),
    ));
    let error = h
        .client
        .generate_plain_jit_config("mac-a-j0000003", &trusted)
        .await
        .unwrap_err();
    assert_eq!(
        (error.status(), error.retry_after()),
        (Some(409), None),
        "{error:?}"
    );
    assert_eq!(removes(&h), 1, "a name taken may be this call's own registration");
}

#[tokio::test]
async fn removing_by_name_or_id_says_whether_it_was_there() {
    let h = harness().await;
    let (set, _session) = with_session(&h).await;
    let jit = h
        .client
        .generate_jit_config(set.id, "mac-a-j0000aaa")
        .await
        .unwrap();
    assert!(!h.client.remove_runner_named("mac-a-j0000bbb").await.unwrap());
    assert!(h.client.remove_runner_named("mac-a-j0000aaa").await.unwrap());
    assert!(!h.client.remove_runner(jit.runner.id).await.unwrap());
    // Another machine's Actions runner is refused before anything is sent.
    let error = h.client.remove_runner_named("mac-b-j0000aaa").await.unwrap_err();
    assert_eq!(error.status(), None, "refused unsent: {error:?}");
}
