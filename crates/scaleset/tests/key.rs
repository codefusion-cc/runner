//! A host that still holds the Runners App's key makes its scale set calls itself, as before the console made them:
//! its own registration token and admin token, straight to the fake GitHub, and never through the console.

mod support;

use std::time::Duration;

use cf_fake_github::JobState;
use cf_fake_github::httpd::Reply;
use cf_ghapp::Http;
use cf_scaleset::ScaleSetClient;
use support::*;

/// The scale set client of `machine` for [`REPO`], signing with the test key.
fn keyed(h: &Harness, machine: &str) -> ScaleSetClient {
    let http = Http::new(Duration::from_secs(10), fast());
    let tokens = key_tokens(&h.fake, http.clone(), h.clock.clone());
    let app = key_app(&h.fake, &tokens, http);
    ScaleSetClient::with_key(
        app,
        tokens,
        machine,
        1,
        REPO,
        &format!("https://github.com/{REPO}"),
        Duration::from_secs(10),
    )
}

/// The scale set calls that reached the console.
fn through_console(h: &Harness) -> usize {
    h.console
        .github_calls()
        .iter()
        .filter(|call| call.route.starts_with("scaleset/"))
        .count()
}

#[tokio::test]
async fn a_host_that_holds_the_key_makes_its_scale_sets_sessions_and_runners_itself() {
    let h = harness().await;
    let client = keyed(&h, MACHINE);
    assert!(client.holds_key());
    assert!(!h.client.holds_key(), "the console's client holds none");
    let set = client
        .ensure_scale_set(&format!("{MACHINE}--cf-2g"), &labels(&["self-hosted", "cf-2g"]))
        .await
        .expect("a scale set");
    let session = client.open_session(set.id).await.expect("a session");
    let request = h.fake.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    let message = session.poll(0, 2).await.expect("a poll").expect("the offer");
    assert_eq!(message.available[0].runner_request_id, request);
    assert_eq!(
        session.acquire_jobs(&[request]).await.expect("acquired"),
        vec![request]
    );
    assert_eq!(h.fake.job_state(request), Some(JobState::Acquired(set.id)));
    let name = format!("{MACHINE}-j0000001");
    let jit = client.generate_jit_config(set.id, &name).await.expect("a runner");
    assert_eq!(h.fake.runners(), vec![name.clone()]);
    assert!(client.remove_runner(jit.runner.id).await.expect("removed"));
    assert!(h.fake.runners().is_empty());
    session.close().await.expect("closed");
    assert!(client.delete_scale_set(set.id).await.expect("deleted"));
    assert!(registrations(&h.fake) >= 1, "its own registration token");
    assert!(
        !admin_requests_from_machines(&h.fake).is_empty(),
        "its own admin token"
    );
    assert_eq!(through_console(&h), 0, "nothing through the console");
}

#[tokio::test]
async fn a_host_that_holds_the_key_lists_only_its_own_scale_sets() {
    let h = harness().await;
    let other = keyed(&h, "mac-ab");
    let labels = labels(&["self-hosted", "cf-2g"]);
    other
        .ensure_scale_set("mac-ab--cf-2g", &labels)
        .await
        .expect("the other's");
    let mine = keyed(&h, MACHINE);
    mine.ensure_scale_set(&format!("{MACHINE}--cf-2g"), &labels)
        .await
        .expect("mine");
    let names: Vec<String> = mine
        .list_scale_sets()
        .await
        .expect("listed")
        .into_iter()
        .map(|set| set.name)
        .collect();
    assert_eq!(names, vec![format!("{MACHINE}--cf-2g")]);
}

#[tokio::test]
async fn an_admin_token_github_refuses_is_replaced_once() {
    let h = harness().await;
    let client = keyed(&h, MACHINE);
    client.list_scale_sets().await.expect("listed");
    let before = registrations(&h.fake);
    h.fake.fail("runnerscalesets", Reply::text(401, "expired"), 1);
    client
        .list_scale_sets()
        .await
        .expect("listed with a new admin token");
    assert_eq!(
        registrations(&h.fake),
        before + 2,
        "one registration token, one admin token"
    );
    h.fake.fail("runnerscalesets", Reply::text(401, "expired"), 5);
    assert_eq!(
        client.list_scale_sets().await.expect_err("refused").status(),
        Some(401)
    );
    assert_eq!(
        registrations(&h.fake),
        before + 4,
        "one new admin token and no more"
    );
}
