//! A console older than its scale set calls answers them a 404 without a code: the Runner makes them itself, with the
//! console's registration token, as it did before the console made them. A Runner never needs a newer console.

mod support;

use cf_fake_github::JobState;
use cf_fake_github::httpd::Reply;
use support::*;

/// What a console from before the scale set calls answers them: the 404 of a route it does not know, with no code.
fn no_scaleset_calls() -> Reply {
    Reply::status_json(404, &serde_json::json!({ "error": "Not found" }))
}

/// The scale set calls that reached the console, by route.
fn scaleset_calls(h: &Harness) -> Vec<String> {
    h.console
        .github_calls()
        .into_iter()
        .filter(|call| call.route.starts_with("scaleset/"))
        .map(|call| call.route)
        .collect()
}

/// The console's registration tokens it handed out.
fn console_registrations(h: &Harness) -> usize {
    h.console
        .github_calls()
        .iter()
        .filter(|call| call.route == "registration" && call.status == 200)
        .count()
}

#[tokio::test]
async fn through_a_console_without_scale_set_calls_the_host_makes_its_scale_sets_sessions_and_runners_itself()
{
    let h = harness().await;
    h.console.scaleset_answer(Some(no_scaleset_calls()));
    let set = h
        .client
        .ensure_scale_set(&format!("{MACHINE}--cf-2g"), &labels(&["self-hosted", "cf-2g"]))
        .await
        .expect("a scale set");
    let session = h.client.open_session(set.id).await.expect("a session");
    let request = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    let message = session.poll(0, 2).await.expect("a poll").expect("the offer");
    assert_eq!(message.available[0].runner_request_id, request);
    assert_eq!(
        session.acquire_jobs(&[request]).await.expect("acquired"),
        vec![request]
    );
    assert_eq!(h.fake.job_state(request), Some(JobState::Acquired(set.id)));
    let name = format!("{MACHINE}-j0000001");
    let jit = h
        .client
        .generate_jit_config(set.id, &name)
        .await
        .expect("a runner");
    assert_eq!(h.fake.runners(), vec![name.clone()]);
    assert!(h.client.remove_runner(jit.runner.id).await.expect("removed"));
    assert!(h.fake.runners().is_empty());
    session.close().await.expect("closed");
    assert!(h.client.delete_scale_set(set.id).await.expect("deleted"));
    assert!(!h.client.holds_key(), "no key on this host");
    assert!(console_registrations(&h) >= 1, "the console's registration token");
    assert!(
        !admin_requests_from_machines(&h.fake).is_empty(),
        "the host's own admin token"
    );
    // The console is taken at its word for a while: one that has the calls now is not asked them at once.
    h.console.scaleset_answer(None);
    h.client.list_scale_sets().await.expect("listed");
    assert_eq!(scaleset_calls(&h), Vec::<String>::new());
}

#[tokio::test]
async fn a_404_with_a_code_is_the_consoles_answer_and_no_sign_of_an_older_console() {
    let h = harness().await;
    h.console.scaleset_answer(Some(Reply::status_json(
        404,
        &serde_json::json!({ "error": "The session is gone", "code": "session_lost" }),
    )));
    let error = h
        .client
        .ensure_scale_set(&format!("{MACHINE}--cf-2g"), &labels(&["self-hosted", "cf-2g"]))
        .await
        .expect_err("refused");
    assert!(matches!(error, cf_scaleset::Error::SessionLost(_)), "{error}");
    assert_eq!(console_registrations(&h), 0, "no registration token asked for");
    assert!(admin_requests_from_machines(&h.fake).is_empty());
}

#[tokio::test]
async fn a_console_that_fails_is_no_older_console() {
    let h = harness().await;
    h.console.scaleset_answer(Some(Reply::status_json(
        500,
        &serde_json::json!({ "error": "boom" }),
    )));
    h.client.list_scale_sets().await.expect_err("the console failed");
    assert_eq!(console_registrations(&h), 0);
    assert!(admin_requests_from_machines(&h.fake).is_empty());
}
