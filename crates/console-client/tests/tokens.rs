//! GitHub tokens through the console, end to end: the real App client on [`ConsoleTokens`], against the fake console
//! (`github/{read,registration,jit}`, its switch per machine, its kept tokens and limits) and the fake GitHub that
//! makes and takes the tokens. Written from the console's contract and the ways it can fail: an outage before and
//! after the last token ends, a switch that is off or turned off, answers that break the contract, renewal, and many
//! callers at once.

mod support;

use std::sync::Arc;
use std::time::Duration;

use cf_console_client::{
    Client, ConsoleTokens, END_MARGIN, FAILED_WAIT, GATE_RECHECK, GATE_RECHECK_ALONE, MAX_TOKEN_LIFE, enroll,
};
use cf_fake_console::FakeConsole;
use cf_fake_github::harness::{REPO, TestClock, fast, labels};
use cf_fake_github::httpd::Reply;
use cf_fake_github::{FakeGithub, Options, test_key};
use cf_ghapp::{AppKey, Error as GitHubError, GitHubApp, Http, KeyTokens, TokenSource};
use cf_secret::Secret;
use serde_json::json;

const MACHINE: &str = "mac-a";
const RUNNER: &str = "mac-a-j0a1b2c3";

struct Rig {
    github: FakeGithub,
    console: FakeConsole,
    clock: Arc<TestClock>,
    app: GitHubApp,
    /// The machine's own token at the console.
    machine_token: Secret<String>,
}

impl Rig {
    /// Moves time on for the machine, the console and GitHub alike.
    fn advance(&self, by: Duration) {
        self.clock.advance(by);
        let offset = self.github.now_epoch() - epoch_now() + by.as_secs() as i64;
        self.github.set_clock_offset(offset);
    }

    /// How many `github/<route>` calls reached the console's routes.
    fn calls(&self, route: &str) -> usize {
        self.console
            .github_calls()
            .iter()
            .filter(|c| c.route == route)
            .count()
    }

    /// Installation tokens the local key asked GitHub for.
    fn key_tokens(&self) -> usize {
        self.github.count("/access_tokens")
    }
}

fn epoch_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
}

/// A machine `mac-a` enrolled with a console that holds a Runners App on the fake GitHub; the console's switch for it
/// `on` or not, and the App's own key on the machine or not.
async fn rig(on: bool, key: bool) -> Rig {
    rig_with(on, key, fast()).await
}

async fn rig_with(on: bool, key: bool, retry: cf_ghapp::RetryPolicy) -> Rig {
    let github = FakeGithub::start(Options::default()).await;
    github.add_repo(REPO, true);
    github.set_variable(REPO, "RUNS_ON", "mac");
    let console = FakeConsole::start().await;
    console.serve_github(github.clone());
    console.add_code("code-a");
    let enrolled = enroll(
        &support::http(),
        &console.url(),
        &support::hello("code-a", MACHINE),
    )
    .await
    .expect("enrol");
    assert!(console.set_broker(MACHINE, on));
    let clock = Arc::new(TestClock::default());
    let http = Http::new(Duration::from_secs(5), retry);
    let fallback = key.then(|| {
        let pem = AppKey::from_pem(&Secret::new(test_key().private_pem.clone())).expect("key");
        Arc::new(KeyTokens::new(
            "Iv23test",
            pem,
            &github.url(),
            http.clone(),
            clock.clone(),
        )) as Arc<dyn TokenSource>
    });
    let machine_token = enrolled.token.clone();
    let client = Client::new(&console.url(), enrolled.token, http.clone());
    let tokens = Arc::new(ConsoleTokens::new(
        client,
        MACHINE,
        fallback,
        clock.clone(),
        clock.clone(),
    ));
    let app = GitHubApp::with_source(tokens, &github.url(), http);
    Rig {
        github,
        console,
        clock,
        app,
        machine_token,
    }
}

fn status_of(error: &GitHubError) -> Option<u16> {
    match error {
        GitHubError::Status(s) => Some(s.status),
        _ => None,
    }
}

// ---- the contract ----

#[tokio::test]
async fn read_tokens_from_the_console_read_github_and_are_asked_for_once() {
    let rig = rig(true, false).await;
    let installations = rig.app.installations().await.expect("installations");
    assert_eq!(installations.iter().map(|i| i.id).collect::<Vec<_>>(), [1]);
    let repos = rig.app.repositories(1).await.expect("repositories");
    assert_eq!(
        repos.iter().map(|r| r.full_name.as_str()).collect::<Vec<_>>(),
        [REPO]
    );
    let variables = rig.app.variables(1, REPO).await.expect("variables");
    assert_eq!(variables.get("RUNS_ON").map(String::as_str), Some("mac"));
    assert_eq!(
        rig.calls("read"),
        1,
        "one read serves every call while it is fresh"
    );
    assert_eq!(rig.key_tokens(), 0);
    let sent = &rig.console.github_calls()[0];
    assert_eq!(sent.machine, MACHINE);
    assert_eq!(sent.body, json!({"protocol": 1}));
}

#[tokio::test]
async fn a_registration_token_comes_from_the_console_and_is_its_kept_one() {
    let rig = rig(true, false).await;
    let first = rig.app.registration_token(1, REPO).await.expect("registration");
    let second = rig.app.registration_token(1, REPO).await.expect("registration");
    assert!(
        first.expose().starts_with("AREG_"),
        "a registration token of the fake GitHub"
    );
    assert_eq!(first, second, "the console hands out the token it keeps");
    assert_eq!(
        rig.calls("registration"),
        2,
        "not cached here: the console keeps it"
    );
    assert_eq!(
        rig.console.github_calls()[0].body,
        json!({"protocol": 1, "repo": REPO})
    );
}

#[tokio::test]
async fn a_one_job_runner_comes_from_the_console_with_exactly_its_name_and_labels() {
    let rig = rig(true, false).await;
    let wanted = labels(&["self-hosted", "macOS", "cf.size-m"]);
    let jit = rig
        .app
        .generate_jit_config(1, REPO, RUNNER, &wanted)
        .await
        .expect("jit");
    assert!(jit.id > 0);
    let call = &rig.console.github_calls()[0];
    assert_eq!(call.route, "jit");
    assert_eq!(
        call.body,
        json!({"protocol": 1, "repo": REPO, "name": RUNNER, "labels": wanted})
    );
    assert_eq!(call.status, 200);
}

#[tokio::test]
async fn a_runner_name_that_is_not_the_machines_is_refused_unsent() {
    let rig = rig(true, false).await;
    for name in [
        "mac-b-j0a1b2c3",
        "mac-a-j0A1B2C3",
        "mac-a-j0a1b2c",
        "mac-a",
        "mac-a-j0a1b2c3x",
        "mäc-a-j0a1b2c3",
    ] {
        let error = rig
            .app
            .generate_jit_config(1, REPO, name, &labels(&["self-hosted"]))
            .await
            .expect_err(name);
        assert!(matches!(error, GitHubError::Refused(_)), "{name}: {error}");
    }
    assert_eq!(rig.console.calls(), 1, "only the enrolment reached the console");
}

#[tokio::test]
async fn requests_the_console_would_refuse_are_not_sent() {
    let rig = rig(true, false).await;
    let long = format!("org/{}", "a".repeat(101));
    for repo in [
        "org",
        "org/..",
        "org/.",
        "-org/app",
        "org/a b",
        long.as_str(),
        "org/app/x",
    ] {
        assert!(rig.app.registration_token(1, repo).await.is_err(), "{repo}");
    }
    let too_many: Vec<String> = (0..21).map(|n| format!("l{n}")).collect();
    for bad in [vec![], too_many, labels(&["a b"]), labels(&[&"x".repeat(101)])] {
        assert!(
            rig.app.generate_jit_config(1, REPO, RUNNER, &bad).await.is_err(),
            "{bad:?}"
        );
    }
    assert_eq!(rig.console.calls(), 1, "only the enrolment reached the console");
}

// ---- renewal and many callers ----

#[tokio::test]
async fn a_read_token_is_renewed_after_half_its_life_and_never_used_near_its_end() {
    let rig = rig(true, false).await;
    let first = rig.app.installation_token(1).await.expect("token");
    rig.advance(Duration::from_secs(29 * 60));
    assert_eq!(rig.app.installation_token(1).await.expect("token"), first);
    assert_eq!(rig.calls("read"), 1, "fresh until half its life");
    rig.advance(Duration::from_secs(2 * 60));
    let second = rig.app.installation_token(1).await.expect("token");
    assert_ne!(second, first, "renewed past half its life");
    assert_eq!(rig.calls("read"), 2);
    assert!(rig.app.repositories(1).await.is_ok(), "the new token reads");
}

#[tokio::test]
async fn many_callers_at_once_share_one_request() {
    let rig = rig(true, false).await;
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..24 {
        let app = rig.app.clone();
        set.spawn(async move { app.installation_token(1).await });
    }
    let mut seen = std::collections::HashSet::new();
    while let Some(done) = set.join_next().await {
        seen.insert(done.expect("task").expect("token").expose().clone());
    }
    assert_eq!(seen.len(), 1);
    assert_eq!(rig.calls("read"), 1, "the first asks, the rest wait for it");

    // Due but still usable: one caller renews, the others go on with the old token meanwhile.
    rig.advance(Duration::from_secs(31 * 60));
    for _ in 0..24 {
        let app = rig.app.clone();
        set.spawn(async move { app.installation_token(1).await });
    }
    while let Some(done) = set.join_next().await {
        done.expect("task").expect("token");
    }
    assert_eq!(rig.calls("read"), 2, "one renewal for all of them");
}

// ---- outages ----

#[tokio::test]
async fn retries_a_429_and_a_5xx_of_the_console() {
    for status in [429, 500, 502, 503] {
        let rig = rig(true, false).await;
        let before = rig.console.calls();
        rig.console.fail_next(2, status);
        rig.app
            .installations()
            .await
            .unwrap_or_else(|e| panic!("{status}: {e}"));
        assert_eq!(
            rig.console.calls() - before,
            3,
            "{status}: two failures and the answer"
        );
    }
}

#[tokio::test]
async fn a_console_outage_while_the_token_still_works_changes_nothing_until_it_ends() {
    let rig = rig(true, false).await;
    let token = rig.app.installation_token(1).await.expect("token");
    rig.console.down(true);
    rig.advance(Duration::from_secs(31 * 60));
    assert_eq!(rig.app.installation_token(1).await.expect("the kept one"), token);
    assert!(rig.app.repositories(1).await.is_ok(), "it still reads GitHub");
    let tried = rig.console.calls();
    for _ in 0..10 {
        assert_eq!(rig.app.installation_token(1).await.expect("the kept one"), token);
    }
    assert_eq!(
        rig.console.calls(),
        tried,
        "a failed renewal waits before the console is asked again"
    );
    rig.advance(FAILED_WAIT);
    rig.console.down(false);
    assert_ne!(
        rig.app.installation_token(1).await.expect("renewed"),
        token,
        "asked again after the wait"
    );
}

#[tokio::test]
async fn an_outage_past_the_tokens_end_is_a_clear_error_and_asks_at_a_measured_pace() {
    let rig = rig(true, false).await;
    rig.app.installation_token(1).await.expect("token");
    rig.console.down(true);
    // Past END_MARGIN before the end: the token is not used any more.
    rig.advance(MAX_TOKEN_LIFE - END_MARGIN);
    let error = rig.app.installations().await.expect_err("no token left");
    assert!(error.is_transient(), "an outage can end: {error}");
    let said = error.to_string();
    assert!(
        said.contains("no GitHub token") && said.contains("console"),
        "{said}"
    );
    let tried = rig.console.calls();
    for _ in 0..20 {
        assert!(rig.app.installation_token(1).await.is_err());
    }
    assert_eq!(rig.console.calls(), tried, "no storm of requests while it waits");
    rig.console.down(false);
    rig.advance(FAILED_WAIT);
    assert!(rig.app.repositories(1).await.is_ok(), "back once the console is");
}

#[tokio::test]
async fn the_wait_after_failures_doubles() {
    let rig = rig_with(true, false, support::no_retry()).await;
    rig.console.down(true);
    assert!(rig.app.installations().await.is_err());
    let second = Duration::from_secs(1);
    for wait in [5, 10, 20, 40, 80, 160, 300, 300] {
        let before = rig.console.calls();
        rig.advance(Duration::from_secs(wait) - second);
        assert!(rig.app.installations().await.is_err());
        assert_eq!(rig.console.calls(), before, "not asked again before {wait} s");
        rig.advance(second);
        assert!(rig.app.installations().await.is_err());
        assert_eq!(rig.console.calls(), before + 1, "asked again after {wait} s");
    }
    assert_eq!(FAILED_WAIT, Duration::from_secs(5));
}

#[tokio::test]
async fn a_console_that_dropped_the_machine_says_enroll_again() {
    let rig = rig(true, false).await;
    let fresh = Client::new(
        &rig.console.url(),
        Secret::new(format!("cfr_{}", "A".repeat(43))),
        Http::new(Duration::from_secs(5), fast()),
    );
    let tokens = ConsoleTokens::new(fresh, MACHINE, None, rig.clock.clone(), rig.clock.clone());
    let error = tokens.installations().await.expect_err("unknown token");
    assert_eq!(status_of(&error), Some(401));
    assert!(error.to_string().contains("enroll it again"), "{error}");
}

// ---- the console's switch ----

#[tokio::test]
async fn with_the_switch_off_the_local_key_serves_and_the_console_is_asked_again_later() {
    let rig = rig(false, true).await;
    assert_eq!(rig.app.installations().await.expect("from the key").len(), 1);
    assert!(rig.app.repositories(1).await.is_ok());
    assert!(rig.app.registration_token(1, REPO).await.is_ok());
    assert_eq!(rig.calls("read"), 1, "refused once");
    assert!(rig.key_tokens() >= 1, "the key made the token");
    assert_eq!(
        rig.calls("registration"),
        0,
        "not asked while the console said off"
    );
    rig.console.set_broker(MACHINE, true);
    rig.advance(GATE_RECHECK - Duration::from_secs(1));
    rig.app.installations().await.expect("still the key");
    assert_eq!(rig.calls("read"), 1);
    rig.advance(Duration::from_secs(2));
    let keyed = rig.key_tokens();
    rig.app.repositories(1).await.expect("from the console again");
    assert_eq!(rig.calls("read"), 2);
    assert_eq!(rig.key_tokens(), keyed, "the console's token served");
}

#[tokio::test]
async fn with_the_switch_off_and_no_key_every_call_says_why() {
    let rig = rig(false, false).await;
    let error = rig.app.installations().await.expect_err("nothing serves");
    assert_eq!(status_of(&error), Some(403));
    assert!(!error.is_transient());
    assert!(error.to_string().contains("off for mac-a"), "{error}");
    let error = rig
        .app
        .registration_token(1, REPO)
        .await
        .expect_err("nothing serves");
    assert_eq!(status_of(&error), Some(403));
    assert_eq!(
        rig.console.github_calls().len(),
        1,
        "asked again only after a while"
    );
    rig.console.set_broker(MACHINE, true);
    rig.advance(GATE_RECHECK_ALONE);
    assert!(rig.app.installations().await.is_ok(), "on again");
}

#[tokio::test]
async fn a_switch_turned_off_meanwhile_moves_the_calls_to_the_key() {
    let rig = rig(true, true).await;
    assert!(rig.app.repositories(1).await.is_ok());
    assert_eq!(rig.key_tokens(), 0);
    // Turning it off revokes the read tokens at GitHub: the next call gets a 401, drops them, asks, hears 403.
    rig.console.set_broker(MACHINE, false);
    assert!(
        rig.app.repositories(1).await.is_ok(),
        "the key took over in the same call"
    );
    assert_eq!(rig.calls("read"), 2);
    assert!(rig.key_tokens() >= 1);
    // Writes go to the key too, without asking the console again.
    rig.app.registration_token(1, REPO).await.expect("from the key");
    rig.app
        .generate_jit_config(1, REPO, RUNNER, &labels(&["self-hosted"]))
        .await
        .expect("from the key");
    assert_eq!(rig.calls("registration") + rig.calls("jit"), 0);
}

#[tokio::test]
async fn a_runner_asked_for_with_the_switch_off_comes_from_the_key() {
    let rig = rig(false, true).await;
    let jit = rig
        .app
        .generate_jit_config(1, REPO, RUNNER, &labels(&["self-hosted"]))
        .await
        .expect("from the key");
    assert!(jit.id > 0);
    assert_eq!(rig.calls("jit"), 1, "the console refused it before asking GitHub");
    assert_eq!(rig.console.github_calls()[0].status, 403);
    rig.app
        .generate_jit_config(1, REPO, "mac-a-j0a1b2c4", &labels(&["self-hosted"]))
        .await
        .expect("from the key");
    assert_eq!(rig.calls("jit"), 1, "not asked again while it said off");
}

#[tokio::test]
async fn only_the_switchs_code_means_off_another_403_is_an_error_with_its_words() {
    // Something in front of the console, or the console refusing the call itself: a 403 without `broker_off`.
    let refusals = [
        Reply::status_json(403, &json!({"error": "Blocked by the firewall rule 7"})),
        Reply::status_json(
            403,
            &json!({"error": "Blocked by the firewall rule 7", "code": "other"}),
        ),
        Reply::Response {
            status: 403,
            headers: vec![("content-type".into(), "text/html".into())],
            body: b"<html>Access denied</html>".to_vec(),
        },
    ];
    for refusal in refusals {
        let rig = rig(true, true).await;
        rig.console.answer_next(refusal.clone());
        let error = rig.app.installations().await.expect_err("refused");
        assert_eq!(status_of(&error), Some(403), "{error}");
        assert!(!error.is_transient());
        let said = error.to_string();
        assert!(
            said.contains("Blocked by the firewall rule 7") || said.contains("no message from the console"),
            "{said}"
        );
        assert_eq!(rig.key_tokens(), 0, "the local key is not turned to: {said}");
        for (route, call) in [("registration", 0), ("jit", 1)] {
            rig.console.answer_next(refusal.clone());
            let error = match call {
                0 => rig.app.registration_token(1, REPO).await.map(|_| ()),
                _ => rig
                    .app
                    .generate_jit_config(1, REPO, RUNNER, &labels(&["self-hosted"]))
                    .await
                    .map(|_| ()),
            }
            .expect_err(route);
            assert_eq!(status_of(&error), Some(403), "{route}: {error}");
        }
        assert_eq!(rig.key_tokens(), 0);
        assert!(rig.github.count("/generate-jitconfig") == 0 && rig.github.count("/registration-token") == 0);
        // Not taken as the switch: the console is asked again, after the wait a failure gets.
        rig.advance(FAILED_WAIT);
        assert!(rig.app.repositories(1).await.is_ok(), "the console serves again");
        assert_eq!(rig.key_tokens(), 0);
    }
}

#[tokio::test]
async fn a_runner_name_the_console_does_not_make_for_this_machine_is_its_403_not_the_key() {
    // The console knows this machine by another name than the Runner does (renamed there): its 403 names it.
    let rig = rig(true, true).await;
    let elsewhere = ConsoleTokens::new(
        Client::new(
            &rig.console.url(),
            rig.machine_token.clone(),
            Http::new(Duration::from_secs(5), fast()),
        ),
        "mac-b",
        None,
        rig.clock.clone(),
        rig.clock.clone(),
    );
    let error = elsewhere
        .generate_jit_config(1, REPO, "mac-b-j0a1b2c3", &labels(&["self-hosted"]))
        .await
        .expect_err("not this machine's name");
    assert_eq!(status_of(&error), Some(403), "{error}");
    assert!(
        error.to_string().contains("A runner of mac-a is named mac-a-j"),
        "{error}"
    );
    let call = rig.console.github_calls().pop().expect("a call");
    assert_eq!((call.route.as_str(), call.status), ("jit", 403));
    // Not the switch: the next read still goes to the console.
    assert!(elsewhere.installations().await.is_ok());
    assert_eq!(rig.calls("read"), 1);
}

#[tokio::test]
async fn the_switchs_403_carries_its_code_and_its_words() {
    let rig = rig(false, false).await;
    let error = rig.app.installations().await.expect_err("off");
    assert!(
        error
            .to_string()
            .contains("GitHub through the console is off for mac-a: the owner turns it on"),
        "{error}"
    );
    let raw = reqwest::Client::new()
        .post(format!("{}/agent/runners/github/read", rig.console.url()))
        .bearer_auth(rig.machine_token.expose())
        .header("content-type", "application/json")
        .body(r#"{"protocol":1}"#)
        .send()
        .await
        .expect("answer");
    assert_eq!(raw.status().as_u16(), 403);
    let body: serde_json::Value = serde_json::from_slice(&raw.bytes().await.expect("body")).expect("JSON");
    assert_eq!(body["code"], "broker_off");
}

#[tokio::test]
async fn installation_ids_of_the_other_app_are_mapped_by_repository() {
    let rig = rig(true, true).await;
    rig.console.set_installations(&[(77, "ORG")]);
    let ids: Vec<i64> = rig
        .app
        .installations()
        .await
        .expect("ids")
        .iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(ids, [77]);
    // An ID learned from the key's App, for a repository of the console's installation (owner compared without case).
    assert!(rig.app.variables(1, REPO).await.is_ok());
    assert_eq!(rig.app.repo_installation(REPO).await.expect("installation"), 77);
    // The console's ID once the key serves: looked up by repository, once.
    rig.console.set_broker(MACHINE, false);
    rig.github.revoke_installation_tokens();
    assert!(rig.app.variables(77, REPO).await.is_ok());
    assert!(rig.app.variables(77, REPO).await.is_ok());
    assert_eq!(rig.github.count("/repos/org/app/installation"), 1);
    // A repository no installation of the console covers is said so, not guessed.
    rig.console.set_broker(MACHINE, true);
    rig.advance(GATE_RECHECK);
    let error = rig.app.variables(5, "other/app").await.expect_err("not covered");
    assert_eq!(status_of(&error), Some(404));
}

// ---- answers that break the contract ----

#[tokio::test]
async fn a_malformed_read_answer_is_refused_and_no_token_of_it_is_used() {
    let token = || "ghs_good".to_string();
    let bad = [
        json!({}),
        json!({"installations": {}}),
        json!({"installations": [{"id": 1, "token": token()}]}),
        json!({"installations": [{"id": 1, "token": token(), "expiresInS": 30}]}),
        json!({"installations": [{"id": 1, "token": token(), "expiresInS": -5}]}),
        json!({"installations": [{"id": 0, "token": token(), "expiresInS": 3600}]}),
        json!({"installations": [{"id": 1, "token": token(), "expiresInS": 3600}, {"id": 1, "token": token(), "expiresInS": 3600}]}),
        json!({"installations": [{"id": 1, "token": "", "expiresInS": 3600}]}),
        json!({"installations": [{"id": 1, "token": "ghs bad", "expiresInS": 3600}]}),
        json!({"installations": [{"id": 1, "token": "ghs_\n", "expiresInS": 3600}]}),
        json!({"installations": [{"id": 1, "token": "x".repeat(4097), "expiresInS": 3600}]}),
    ];
    for answer in bad {
        let rig = rig(true, false).await;
        rig.console.answer_next(Reply::json(&answer));
        let error = rig.app.installations().await.expect_err(&answer.to_string());
        assert!(matches!(error, GitHubError::Decode { .. }), "{answer}: {error}");
        assert!(!error.to_string().contains("ghs_good"), "no token in the error");
        rig.advance(FAILED_WAIT);
        assert!(
            rig.app.repositories(1).await.is_ok(),
            "{answer}: a good answer later serves"
        );
    }
}

#[tokio::test]
async fn an_answer_that_is_not_json_is_refused() {
    let rig = rig(true, false).await;
    rig.console.answer_next(Reply::Response {
        status: 200,
        headers: vec![("content-type".into(), "text/html".into())],
        body: b"<html>ghs_secret</html>".to_vec(),
    });
    let error = rig.app.installations().await.expect_err("not JSON");
    assert!(matches!(error, GitHubError::Decode { .. }), "{error}");
    assert!(!error.to_string().contains("ghs_secret"));
}

#[tokio::test]
async fn a_lifetime_over_an_hour_is_held_for_an_hour() {
    let rig = rig(true, false).await;
    rig.console.answer_next(Reply::json(
        &json!({"installations": [{"id": 1, "account": "org", "token": "ghs_long", "expiresInS": 86_400}]}),
    ));
    assert_eq!(
        rig.app.installation_token(1).await.expect("token").expose(),
        "ghs_long"
    );
    rig.console.down(true);
    rig.advance(MAX_TOKEN_LIFE - END_MARGIN);
    assert!(
        rig.app.installation_token(1).await.is_err(),
        "not used past an hour, whatever the console said"
    );
}

#[tokio::test]
async fn an_empty_read_answer_lists_nothing_and_is_asked_again_later() {
    let rig = rig(true, false).await;
    rig.console
        .answer_next(Reply::json(&json!({"installations": []})));
    assert!(rig.app.installations().await.expect("none").is_empty());
    assert_eq!(
        status_of(&rig.app.installation_token(1).await.expect_err("none")),
        Some(404)
    );
    rig.advance(cf_console_client::EMPTY_LIFE);
    assert_eq!(rig.app.installations().await.expect("listed").len(), 1);
}

#[tokio::test]
async fn malformed_registration_and_runner_answers_are_refused() {
    let rig = rig(true, false).await;
    for answer in [
        json!({"token": "AREG_x"}),
        json!({"token": "", "expiresInS": 3600}),
        json!({"token": "AREG_x", "expiresInS": 10}),
    ] {
        rig.console.answer_next(Reply::json(&answer));
        let error = rig
            .app
            .registration_token(1, REPO)
            .await
            .expect_err(&answer.to_string());
        assert!(matches!(error, GitHubError::Decode { .. }), "{answer}: {error}");
    }
    for answer in [
        json!({"runnerId": 0, "encodedJitConfig": "abc"}),
        json!({"runnerId": 5, "encodedJitConfig": ""}),
        json!({"encodedJitConfig": "abc"}),
    ] {
        rig.console.answer_next(Reply::json(&answer));
        let error = rig
            .app
            .generate_jit_config(1, REPO, RUNNER, &labels(&["self-hosted"]))
            .await
            .expect_err(&answer.to_string());
        assert!(matches!(error, GitHubError::Decode { .. }), "{answer}: {error}");
    }
}

// ---- one-job runners are sent once ----

#[tokio::test]
async fn a_runner_is_asked_for_once_even_when_the_answer_fails() {
    let rig = rig(true, false).await;
    for status in [500, 502, 503] {
        let before = rig.console.calls();
        rig.console.fail_next(1, status);
        let error = rig
            .app
            .generate_jit_config(1, REPO, RUNNER, &labels(&["self-hosted"]))
            .await
            .expect_err("failed");
        assert!(
            error.is_transient(),
            "the caller decides whether to ask again: {error}"
        );
        assert!(
            error.to_string().contains(&format!("answered {status}")),
            "{error}"
        );
        assert_eq!(rig.console.calls() - before, 1, "{status}: sent once");
    }
}

#[tokio::test]
async fn the_consoles_runner_limit_is_a_429_whose_wait_is_kept() {
    let rig = rig(true, false).await;
    let other = "org/other";
    rig.github.add_repo(other, true);
    let names = ["mac-a-j0000001", "mac-a-j0000002", "mac-a-j0000003"];
    for name in names {
        rig.app
            .generate_jit_config(1, REPO, name, &labels(&["self-hosted"]))
            .await
            .expect(name);
    }
    let before = rig.console.calls();
    let error = rig
        .app
        .generate_jit_config(1, REPO, "mac-a-j0000004", &labels(&["self-hosted"]))
        .await
        .expect_err("over the limit");
    assert_eq!(status_of(&error), Some(429), "{error}");
    assert!(error.is_transient(), "{error}");
    assert!(error.to_string().contains("five minutes"), "{error}");
    assert_eq!(
        error.retry_after(),
        Some(Duration::from_secs(300)),
        "the console's Retry-After"
    );
    assert_eq!(rig.console.calls() - before, 1, "not tried again");
    // Until the wait passes, the repository's runners are not asked for: a 429 that says what is left.
    rig.advance(Duration::from_secs(299));
    let error = rig
        .app
        .generate_jit_config(1, REPO, "mac-a-j0000005", &labels(&["self-hosted"]))
        .await
        .expect_err("still waiting");
    assert_eq!(status_of(&error), Some(429), "{error}");
    let left = error.retry_after().expect("what is left");
    assert!(
        left > Duration::ZERO && left <= Duration::from_secs(1),
        "{left:?}"
    );
    assert!(error.to_string().contains("not sent"), "{error}");
    assert_eq!(rig.console.calls() - before, 1, "nothing sent while it waits");
    // Another repository is asked for meanwhile.
    rig.app
        .generate_jit_config(1, other, "mac-a-j0000006", &labels(&["self-hosted"]))
        .await
        .expect("another repository");
    rig.advance(Duration::from_secs(1));
    rig.app
        .generate_jit_config(1, REPO, "mac-a-j0000007", &labels(&["self-hosted"]))
        .await
        .expect("asked again once the wait passed");
}

#[tokio::test]
async fn a_runner_the_console_answers_503_with_a_wait_may_exist_and_waits() {
    let rig = rig(true, false).await;
    rig.console.fail_next_waiting(1, 503, 30);
    let error = rig
        .app
        .generate_jit_config(1, REPO, RUNNER, &labels(&["self-hosted"]))
        .await
        .expect_err("GitHub busy");
    // A 503 keeps its status: GitHub may have registered the runner before it failed.
    assert_eq!(status_of(&error), Some(503), "{error}");
    assert_eq!(error.retry_after(), Some(Duration::from_secs(30)));
    let before = rig.console.calls();
    let waiting = rig
        .app
        .generate_jit_config(1, REPO, "mac-a-j0a1b2c4", &labels(&["self-hosted"]))
        .await
        .expect_err("waiting");
    assert_eq!(status_of(&waiting), Some(429), "not sent: nothing made");
    assert_eq!(rig.console.calls(), before);
    rig.advance(Duration::from_secs(30));
    rig.app
        .generate_jit_config(1, REPO, "mac-a-j0a1b2c4", &labels(&["self-hosted"]))
        .await
        .expect("after the wait");
}

#[tokio::test]
async fn a_retry_after_past_an_hour_is_held_to_an_hour() {
    let rig = rig(true, false).await;
    rig.console.answer_next(
        Reply::status_json(429, &json!({"error": "Too many"}))
            .with_header("retry-after", &u64::MAX.to_string()),
    );
    let error = rig
        .app
        .generate_jit_config(1, REPO, RUNNER, &labels(&["self-hosted"]))
        .await
        .expect_err("limited");
    assert_eq!(error.retry_after(), Some(cf_console_client::RETRY_AFTER_MAX));
    rig.advance(cf_console_client::RETRY_AFTER_MAX);
    rig.app
        .generate_jit_config(1, REPO, RUNNER, &labels(&["self-hosted"]))
        .await
        .expect("after an hour");
}

#[tokio::test]
async fn a_read_the_console_answers_with_a_wait_is_not_asked_again_sooner() {
    let rig = rig(true, false).await;
    rig.console.fail_next_waiting(1, 503, 120);
    let error = rig.app.installations().await.expect_err("busy");
    assert!(error.is_transient(), "{error}");
    let asked = rig.console.calls();
    rig.advance(Duration::from_secs(119));
    assert!(rig.app.installations().await.is_err(), "still waiting");
    assert_eq!(
        rig.console.calls(),
        asked,
        "not before its Retry-After, whatever the doubling says"
    );
    rig.advance(Duration::from_secs(1));
    assert!(rig.app.installations().await.is_ok());
}

#[tokio::test]
async fn a_runner_name_github_has_already_is_a_409() {
    let rig = rig(true, false).await;
    rig.app
        .generate_jit_config(1, REPO, RUNNER, &labels(&["self-hosted"]))
        .await
        .expect("first");
    let error = rig
        .app
        .generate_jit_config(1, REPO, RUNNER, &labels(&["self-hosted"]))
        .await
        .expect_err("taken");
    assert_eq!(status_of(&error), Some(409));
}

#[tokio::test]
async fn a_read_token_from_the_console_cannot_register_a_runner() {
    let rig = rig(true, false).await;
    let token = rig.app.installation_token(1).await.expect("token");
    let response = reqwest::Client::new()
        .post(format!(
            "{}/repos/{REPO}/actions/runners/registration-token",
            rig.github.url()
        ))
        .header("authorization", format!("Bearer {}", token.expose()))
        .send()
        .await
        .expect("answer");
    assert_eq!(
        response.status().as_u16(),
        403,
        "GitHub refuses a read token a write"
    );
}

#[tokio::test]
async fn an_empty_read_answer_stands_until_its_whole_life_has_passed() {
    let rig = rig(true, false).await;
    rig.console
        .answer_next(Reply::json(&json!({"installations": []})));
    assert!(rig.app.installations().await.expect("none").is_empty());
    rig.advance(cf_console_client::EMPTY_LIFE - Duration::from_secs(1));
    assert!(rig.app.installations().await.expect("still none").is_empty());
    assert_eq!(rig.calls("read"), 1, "not asked again before its life is over");
}

// ---- tokens GitHub refuses ----

/// GitHub answers the next read of the installation's repositories 401, whatever token it carries.
fn github_refuses_once(rig: &Rig) {
    rig.github.fail(
        "GET /installation/repositories",
        Reply::status_json(401, &json!({"message": "Bad credentials"})),
        1,
    );
}

#[tokio::test]
async fn a_token_github_refuses_is_named_in_the_next_read_and_the_console_replaces_and_revokes_it() {
    let rig = rig(true, false).await;
    let first = rig.app.installation_token(1).await.expect("token");
    github_refuses_once(&rig);
    assert!(
        rig.app.repositories(1).await.is_ok(),
        "the new token serves in the same call"
    );
    let reads: Vec<_> = rig
        .console
        .github_calls()
        .into_iter()
        .filter(|c| c.route == "read")
        .collect();
    assert_eq!(reads.len(), 2, "asked once more, at once");
    assert_eq!(reads[1].body, json!({"protocol": 1, "refused": [first.expose()]}));
    let second = rig.app.installation_token(1).await.expect("token");
    assert_ne!(second, first, "a new token in its place");
    assert!(
        !rig.github.token_works(first.expose()),
        "the refused one is revoked"
    );
    assert!(rig.github.token_works(second.expose()));
    // Named once: the next read, at renewal, names nothing.
    rig.advance(Duration::from_secs(31 * 60));
    rig.app.installation_token(1).await.expect("renewed");
    let last = rig.console.github_calls().pop().expect("a call");
    assert_eq!((last.route.as_str(), last.body), ("read", json!({"protocol": 1})));
}

#[tokio::test]
async fn a_refused_token_is_named_again_after_a_read_that_failed() {
    let rig = rig_with(true, false, support::no_retry()).await;
    let first = rig.app.installation_token(1).await.expect("token");
    rig.console.fail_next(1, 502);
    github_refuses_once(&rig);
    assert!(rig.app.repositories(1).await.is_err(), "the console failed");
    rig.advance(FAILED_WAIT);
    assert!(rig.app.repositories(1).await.is_ok(), "replaced on the next read");
    let last = rig
        .console
        .github_calls()
        .into_iter()
        .rfind(|c| c.route == "read")
        .expect("a read");
    assert_eq!(last.body, json!({"protocol": 1, "refused": [first.expose()]}));
}

#[tokio::test]
async fn a_read_names_at_most_twenty_refused_tokens_the_last_ones_and_none_too_long() {
    let rig = rig(true, false).await;
    let client = Client::new(
        &rig.console.url(),
        rig.machine_token.clone(),
        Http::new(Duration::from_secs(5), support::no_retry()),
    );
    let named = |rig: &Rig| rig.console.github_calls().pop().expect("a call").body["refused"].clone();
    // The console refuses a read that names 21 (400): the last 20 are named.
    let refused: Vec<Secret<String>> = (0..25).map(|n| Secret::new(format!("ghs_{n:02}"))).collect();
    client.github_read(&refused).await.expect("the console took it");
    let last: Vec<String> = (5..25).map(|n| format!("ghs_{n:02}")).collect();
    assert_eq!(named(&rig), json!(last));
    // One of 255 characters is named; one of 256 is not the console's, and would cost the whole read.
    let edge = format!("ghs_{}", "y".repeat(251));
    client
        .github_read(&[Secret::new(edge.clone()), Secret::new("z".repeat(256))])
        .await
        .expect("the console took it");
    assert_eq!(named(&rig), json!([edge]));
}

#[tokio::test]
async fn a_token_github_refuses_that_a_console_hands_out_again_waits_and_is_named_each_time() {
    let rig = rig(true, false).await;
    // A console that hands out the token it keeps whatever the machine names.
    rig.console.ignore_refused(true);
    let token = rig.app.installation_token(1).await.expect("token");
    // GitHub stops taking the token; the console keeps it (40 minutes and more left) and hands it out again.
    rig.github.revoke_token(token.expose());
    for _ in 0..20 {
        let error = rig.app.repositories(1).await.expect_err("refused");
        assert_eq!(status_of(&error), Some(401), "{error}");
        assert!(!error.to_string().contains(token.expose().as_str()));
    }
    assert_eq!(
        rig.calls("read"),
        2,
        "the console is asked once more, not on every call GitHub refuses"
    );
    // After the wait it is asked once more, naming the token again, and waited on again.
    rig.advance(FAILED_WAIT);
    for _ in 0..5 {
        assert_eq!(
            status_of(&rig.app.repositories(1).await.expect_err("refused")),
            Some(401)
        );
    }
    assert_eq!(rig.calls("read"), 3, "once per wait");
    for read in rig.console.github_calls().iter().skip(1) {
        assert_eq!(read.body, json!({"protocol": 1, "refused": [token.expose()]}));
    }
    // Once the console makes a new one (under 40 minutes left of the kept one), it serves.
    rig.advance(Duration::from_secs(21 * 60));
    assert!(rig.app.repositories(1).await.is_ok(), "a new token serves");
}
