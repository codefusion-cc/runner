//! The scale set routes (`github/scaleset/*`) end to end: the real client against the fake console, which asks the
//! fake GitHub for the machine. Written from the console's contract: the body of every route, the refusals and their
//! codes, what the client refuses unsent, what it sends once, answers that break the contract, and the console's scope
//! (another machine's scale set or Actions runner, the switch, a removed machine, the rate).

mod support;

use std::time::Duration;

use cf_console_client::{
    Client, Error, NAME_TAKEN, NOT_YOURS, RUNNER_BUSY, Removal, SESSION_EXISTS, SESSION_LOST,
};
use cf_fake_console::{FakeConsole, RATE_WINDOW_S, SESSIONS_PER_WINDOW, SETS_PER_WINDOW};
use cf_fake_github::httpd::Reply;
use cf_fake_github::{FakeGithub, Options};
use cf_ghapp::Http;
use cf_harness::{enrolled, labels};
use serde_json::{Value, json};

const REPO: &str = "org/app";
const SET: &str = "mac-a--cf-2g";

struct Rig {
    github: FakeGithub,
    console: FakeConsole,
    client: Client,
}

impl Rig {
    /// The bodies the console got on `scaleset/<route>`, in order.
    fn sent(&self, route: &str) -> Vec<Value> {
        let route = format!("scaleset/{route}");
        self.console
            .github_calls()
            .into_iter()
            .filter(|c| c.route == route)
            .map(|c| c.body)
            .collect()
    }

    async fn set(&self) -> i64 {
        self.client
            .scaleset_create("mac-a", REPO, SET, &labels(&["self-hosted", "cf-2g"]))
            .await
            .expect("scale set")
            .id
    }
}

async fn rig_with(http: Http) -> Rig {
    let github = FakeGithub::start(Options::default()).await;
    github.add_repo(REPO, true);
    let console = FakeConsole::start().await;
    console.serve_github(github.clone());
    let client = enrolled(&console, "mac-a", http).await;
    Rig {
        github,
        console,
        client,
    }
}

async fn rig() -> Rig {
    rig_with(support::http()).await
}

fn refusal(status: u16, code: &str) -> Reply {
    Reply::status_json(
        status,
        &json!({"error": format!("refused: {code}"), "code": code}),
    )
}

fn opened() -> Value {
    json!({
        "sessionId": "5f0c2a7e-1",
        "messageQueueUrl": "https://pipelines.actions.githubusercontent.com/q/messages",
        "messageQueueAccessToken": "queue-token-of-the-answer",
        "serviceUrl": "https://pipelines.actions.githubusercontent.com/q",
        "statistics": {
            "totalAvailableJobs": 1, "totalAcquiredJobs": 0, "totalAssignedJobs": 0, "totalRunningJobs": 0,
            "totalRegisteredRunners": 0, "totalBusyRunners": 0, "totalIdleRunners": 0
        }
    })
}

// ---- the contract ----

#[tokio::test]
async fn every_route_sends_the_contracts_body_and_reads_its_answer() {
    let rig = rig().await;
    assert_eq!(rig.client.scaleset_list(REPO).await.unwrap(), vec![]);
    let entry = rig
        .client
        .scaleset_create("mac-a", REPO, SET, &labels(&["self-hosted", "CF-2G"]))
        .await
        .unwrap();
    assert_eq!(
        (entry.name.as_str(), entry.labels.clone()),
        (SET, labels(&["cf-2g", "self-hosted"]))
    );
    assert_eq!(rig.client.scaleset_list(REPO).await.unwrap(), vec![entry.clone()]);
    let id = entry.id;

    let session = rig.client.scaleset_session_open(REPO, id).await.unwrap();
    assert!(
        session.queue_url.starts_with(&rig.github.url()),
        "{}",
        session.queue_url
    );
    assert_eq!(session.service_url, format!("{}/svc", rig.github.url()));
    assert!(!session.queue_token.expose().is_empty());
    assert!(!format!("{session:?}").contains(session.queue_token.expose()));
    let refreshed = rig
        .client
        .scaleset_session_refresh(REPO, id, &session.session_id)
        .await
        .unwrap();
    assert_ne!(refreshed.queue_token.expose(), session.queue_token.expose());
    assert!(
        rig.client
            .scaleset_session_close(REPO, id, &session.session_id)
            .await
            .unwrap()
    );
    assert!(
        !rig.client
            .scaleset_session_close(REPO, id, &session.session_id)
            .await
            .unwrap(),
        "gone"
    );

    let jit = rig
        .client
        .scaleset_jit("mac-a", REPO, id, "mac-a-j0000001")
        .await
        .unwrap();
    assert!(jit.runner_id > 0 && !jit.encoded.expose().is_empty());
    assert!(
        rig.client
            .scaleset_remove("mac-a", REPO, Removal::Name("mac-a-j0000001"))
            .await
            .unwrap()
    );
    assert!(
        !rig.client
            .scaleset_remove("mac-a", REPO, Removal::Name("mac-a-j0000001"))
            .await
            .unwrap()
    );
    let jit = rig
        .client
        .scaleset_jit("mac-a", REPO, id, "mac-a-j0000002")
        .await
        .unwrap();
    assert!(
        rig.client
            .scaleset_remove("mac-a", REPO, Removal::Id(jit.runner_id))
            .await
            .unwrap()
    );
    assert!(
        !rig.client
            .scaleset_remove("mac-a", REPO, Removal::Id(jit.runner_id))
            .await
            .unwrap()
    );
    assert!(rig.client.scaleset_delete(REPO, id).await.unwrap());

    assert_eq!(rig.sent("list")[0], json!({"protocol": 1, "repo": REPO}));
    assert_eq!(
        rig.sent("create")[0],
        json!({"protocol": 1, "repo": REPO, "name": SET, "labels": ["self-hosted", "CF-2G"]})
    );
    assert_eq!(
        rig.sent("session/open"),
        vec![json!({"protocol": 1, "repo": REPO, "setId": id})]
    );
    assert_eq!(
        rig.sent("session/refresh"),
        vec![json!({"protocol": 1, "repo": REPO, "setId": id, "sessionId": session.session_id})]
    );
    assert_eq!(
        rig.sent("jit")[0],
        json!({"protocol": 1, "repo": REPO, "setId": id, "name": "mac-a-j0000001"})
    );
    assert_eq!(
        rig.sent("remove")[..2],
        [
            json!({"protocol": 1, "repo": REPO, "name": "mac-a-j0000001"}),
            json!({"protocol": 1, "repo": REPO, "name": "mac-a-j0000001"})
        ]
    );
    assert_eq!(
        rig.sent("remove")[2],
        json!({"protocol": 1, "repo": REPO, "runnerId": jit.runner_id})
    );
    assert_eq!(
        rig.sent("delete"),
        vec![json!({"protocol": 1, "repo": REPO, "setId": id})]
    );
    assert_eq!(
        rig.github.count("/registration-token") + rig.github.count("/runner-registration"),
        0
    );
}

#[tokio::test]
async fn a_refresh_without_a_queue_keeps_the_sessions_own() {
    let rig = rig().await;
    let id = rig.set().await;
    let session = rig.client.scaleset_session_open(REPO, id).await.unwrap();
    rig.console.answer_next(Reply::json(&json!({
        "messageQueueUrl": "",
        "messageQueueAccessToken": "fresh",
        "serviceUrl": "https://pipelines.actions.githubusercontent.com/q"
    })));
    let refreshed = rig
        .client
        .scaleset_session_refresh(REPO, id, &session.session_id)
        .await
        .unwrap();
    assert_eq!(refreshed.queue_url, None);
}

// ---- refusals ----

#[tokio::test]
async fn refusals_keep_their_status_their_code_and_their_wait() {
    let rig = rig().await;
    let id = rig.set().await;
    for (status, code) in [
        (404, SESSION_LOST),
        (409, SESSION_EXISTS),
        (409, NAME_TAKEN),
        (422, RUNNER_BUSY),
        (403, NOT_YOURS),
        (502, "bad_address"),
    ] {
        rig.console.answer_next(refusal(status, code));
        match rig.client.scaleset_delete(REPO, id).await.unwrap_err() {
            Error::Refused {
                status: got,
                code: Some(got_code),
                ..
            } => {
                assert_eq!((got, got_code.as_str()), (status, code))
            }
            other => panic!("{status} {code}: {other:?}"),
        }
    }
    // A name or a session taken stays that refusal with its code, even when it says when to ask again: only a 409
    // without either code and with a wait is the console's word that nothing was made.
    for code in [SESSION_EXISTS, NAME_TAKEN] {
        rig.console
            .answer_next(refusal(409, code).with_header("retry-after", "15"));
        let error = rig.client.scaleset_delete(REPO, id).await.unwrap_err();
        assert_eq!(error.code(), Some(code), "{error:?}");
        assert!(!error.is_transient(), "{error:?}");
    }
    rig.console
        .answer_next(refusal(409, "no_trusted_job").with_header("retry-after", "15"));
    let error = rig.client.scaleset_delete(REPO, id).await.unwrap_err();
    assert_eq!(error.retry_after(), Some(Duration::from_secs(15)), "{error:?}");
    // A code that is not one is left out; the refusal stays.
    rig.console.answer_next(refusal(409, "Not A Code!"));
    assert!(matches!(
        rig.client.scaleset_delete(REPO, id).await.unwrap_err(),
        Error::Refused {
            status: 409,
            code: None,
            ..
        }
    ));
    for status in [429, 503] {
        rig.console
            .answer_next(refusal(status, "rate_limited").with_header("retry-after", "120"));
        let error = rig.client.scaleset_list(REPO).await.unwrap_err();
        assert_eq!(
            error.retry_after(),
            Some(Duration::from_secs(120)),
            "{status}: {error:?}"
        );
        assert!(error.is_transient());
    }
    rig.console
        .answer_next(Reply::status_json(401, &json!({"error": "Unknown token"})));
    assert_eq!(
        rig.client.scaleset_list(REPO).await.unwrap_err(),
        Error::Unauthorized
    );
    rig.console.answer_next(Reply::status_json(
        403,
        &json!({"error": "Blocked", "code": "other"}),
    ));
    let error = rig.client.scaleset_list(REPO).await.unwrap_err();
    assert!(!error.is_transient(), "{error:?}");
    rig.console.answer_next(refusal(403, "broker_off"));
    assert!(matches!(
        rig.client.scaleset_list(REPO).await.unwrap_err(),
        Error::GithubOff(_)
    ));
}

#[tokio::test]
async fn what_the_console_would_refuse_is_refused_unsent() {
    let rig = rig().await;
    let id = rig.set().await;
    let before = rig.console.calls();
    let c = &rig.client;
    let long = format!("mac-a--{}", "a".repeat(94));
    let refused = [
        c.scaleset_list("org").await.map(|_| ()),
        c.scaleset_list("org/a b").await.map(|_| ()),
        c.scaleset_create("mac-a", REPO, "mac-b--cf-2g", &labels(&["x"]))
            .await
            .map(|_| ()),
        c.scaleset_create("mac-a", REPO, "mac-a--", &labels(&["x"]))
            .await
            .map(|_| ()),
        c.scaleset_create("mac-a", REPO, "mac-a--CF", &labels(&["x"]))
            .await
            .map(|_| ()),
        c.scaleset_create("mac-a", REPO, &long, &labels(&["x"]))
            .await
            .map(|_| ()),
        c.scaleset_create("mac", REPO, "mac-a--cf", &labels(&["x"]))
            .await
            .map(|_| ()),
        c.scaleset_create("mac-a", REPO, SET, &[]).await.map(|_| ()),
        c.scaleset_create(
            "mac-a",
            REPO,
            SET,
            &(0..21).map(|n| format!("l{n}")).collect::<Vec<_>>(),
        )
        .await
        .map(|_| ()),
        c.scaleset_create("mac-a", REPO, SET, &labels(&["a b"]))
            .await
            .map(|_| ()),
        c.scaleset_delete(REPO, 0).await.map(|_| ()),
        c.scaleset_delete(REPO, 9_007_199_254_740_992).await.map(|_| ()),
        c.scaleset_session_open(REPO, -1).await.map(|_| ()),
        c.scaleset_session_refresh(REPO, id, "a_b").await.map(|_| ()),
        c.scaleset_session_refresh(REPO, id, "").await.map(|_| ()),
        c.scaleset_session_close(REPO, id, &"a".repeat(101))
            .await
            .map(|_| ()),
        c.scaleset_jit("mac-a", REPO, id, "mac-b-j0000001")
            .await
            .map(|_| ()),
        c.scaleset_jit("mac-a", REPO, id, "mac-a-j000000G")
            .await
            .map(|_| ()),
        c.scaleset_jit("mac-a", REPO, id, "mac-a-j00000001")
            .await
            .map(|_| ()),
        c.scaleset_remove("mac-a", REPO, Removal::Name("mac-b-j0000001"))
            .await
            .map(|_| ()),
        c.scaleset_remove("mac-a", REPO, Removal::Id(0)).await.map(|_| ()),
    ];
    for (n, result) in refused.into_iter().enumerate() {
        assert!(matches!(result, Err(Error::Protocol(_))), "{n}: {result:?}");
    }
    assert_eq!(rig.console.calls(), before, "nothing reached the console");
}

#[tokio::test]
async fn a_session_and_a_runner_are_asked_for_once_the_rest_again() {
    let rig = rig_with(Http::new(Duration::from_secs(5), support::fast_retry())).await;
    let id = rig.set().await;
    rig.console.fail_next(1, 503);
    rig.client.scaleset_list(REPO).await.expect("tried again");
    for status in [500, 502, 503] {
        let before = rig.console.calls();
        rig.console.fail_next(1, status);
        assert!(
            rig.client
                .scaleset_session_open(REPO, id)
                .await
                .unwrap_err()
                .is_transient()
        );
        assert_eq!(rig.console.calls() - before, 1, "open {status}: sent once");
        let before = rig.console.calls();
        rig.console.fail_next(1, status);
        assert!(
            rig.client
                .scaleset_jit("mac-a", REPO, id, "mac-a-j0000001")
                .await
                .unwrap_err()
                .is_transient()
        );
        assert_eq!(rig.console.calls() - before, 1, "jit {status}: sent once");
    }
}

// ---- answers that break the contract ----

#[tokio::test]
async fn answers_that_break_the_contract_are_refused_without_their_tokens() {
    let rig = rig().await;
    let id = rig.set().await;
    let with = |key: &str, value: Value| {
        let mut answer = opened();
        answer[key] = value;
        answer
    };
    let mut no_counts = opened();
    no_counts.as_object_mut().unwrap().remove("statistics");
    // A key a newer console adds is dropped, as the console drops what a newer Runner sends.
    rig.console.answer_next(Reply::json(&with("extra", json!(1))));
    assert!(
        rig.client.scaleset_session_open(REPO, id).await.is_ok(),
        "an extra key is no breach"
    );
    for answer in [
        no_counts,
        with("sessionId", json!("a_b")),
        with("sessionId", json!("")),
        with("messageQueueAccessToken", json!("")),
        with(
            "messageQueueUrl",
            json!("https://pipelines.actions.githubusercontent.com/a b"),
        ),
        with("serviceUrl", json!("")),
        json!("queue-token-of-the-answer"),
    ] {
        rig.console.answer_next(Reply::json(&answer));
        let error = rig.client.scaleset_session_open(REPO, id).await.unwrap_err();
        assert!(matches!(error, Error::Protocol(_)), "{answer}: {error:?}");
        assert!(
            !format!("{error} {error:?}").contains("queue-token-of-the-answer"),
            "{error:?}"
        );
    }
    for answer in [
        json!({"runnerId": 0, "encodedJitConfig": "abc"}),
        json!({"runnerId": 5, "encodedJitConfig": ""}),
        json!({"encodedJitConfig": "abc"}),
        json!({"runnerId": 5, "encodedJitConfig": "a".repeat(64 * 1024 + 1)}),
    ] {
        rig.console.answer_next(Reply::json(&answer));
        let error = rig
            .client
            .scaleset_jit("mac-a", REPO, id, "mac-a-j0000001")
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Protocol(_)), "{error:?}");
    }
    for answer in [
        json!({"scaleSets": [{"id": 0, "name": SET, "labels": []}]}),
        json!({"scaleSets": [{"id": 3, "name": "", "labels": []}]}),
        json!({"scaleSets": {}}),
        json!({}),
    ] {
        rig.console.answer_next(Reply::json(&answer));
        let error = rig.client.scaleset_list(REPO).await.unwrap_err();
        assert!(matches!(error, Error::Protocol(_)), "{answer}: {error:?}");
    }
    rig.console.answer_next(Reply::json(
        &json!({"scaleSet": {"id": 3, "name": "mac-a--other", "labels": []}}),
    ));
    let error = rig
        .client
        .scaleset_create("mac-a", REPO, SET, &labels(&["x"]))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Protocol(_)), "another set: {error:?}");
    for (route, answer) in [
        ("delete", json!({"removed": true})),
        ("delete", json!({"deleted": "yes"})),
    ] {
        rig.console.answer_next(Reply::json(&answer));
        let error = rig.client.scaleset_delete(REPO, id).await.unwrap_err();
        assert!(matches!(error, Error::Protocol(_)), "{route} {answer}: {error:?}");
    }
    rig.console.answer_next(Reply::json(
        &json!({"messageQueueUrl": "", "serviceUrl": "https://x"}),
    ));
    let error = rig
        .client
        .scaleset_session_refresh(REPO, id, "abc")
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Protocol(_)), "no token: {error:?}");
}

// ---- the console's scope ----

#[tokio::test]
async fn another_machines_scale_set_or_runner_is_not_yours() {
    let rig = rig().await;
    let id = rig.set().await;
    let jit = rig
        .client
        .scaleset_jit("mac-a", REPO, id, "mac-a-j0000001")
        .await
        .unwrap();
    let other = enrolled(&rig.console, "mac-b", support::http()).await;
    let not_yours = |result: Result<(), Error>| {
        assert!(
            matches!(&result, Err(Error::Refused { status: 403, code: Some(code), .. }) if code == NOT_YOURS),
            "{result:?}"
        );
    };
    // mac-b lists nothing of mac-a's, and so owns no row of it.
    assert_eq!(other.scaleset_list(REPO).await.unwrap(), vec![]);
    not_yours(other.scaleset_delete(REPO, id).await.map(|_| ()));
    not_yours(other.scaleset_session_open(REPO, id).await.map(|_| ()));
    not_yours(other.scaleset_session_close(REPO, id, "abc").await.map(|_| ()));
    not_yours(
        other
            .scaleset_jit("mac-b", REPO, id, "mac-b-j0000001")
            .await
            .map(|_| ()),
    );
    not_yours(
        other
            .scaleset_remove("mac-b", REPO, Removal::Id(jit.runner_id))
            .await
            .map(|_| ()),
    );
    // A name of another machine's shape, sent anyway by a client that says it is mac-a.
    not_yours(
        other
            .scaleset_create("mac-a", REPO, "mac-a--cf-4g", &labels(&["x"]))
            .await
            .map(|_| ()),
    );
    not_yours(
        other
            .scaleset_remove("mac-a", REPO, Removal::Name("mac-a-j0000001"))
            .await
            .map(|_| ()),
    );
    // A name that only starts like another machine's is its own.
    let mini = enrolled(&rig.console, "mac-a-mini", support::http()).await;
    assert_eq!(
        mini.scaleset_list(REPO).await.unwrap(),
        vec![],
        "mac-a--… is not mac-a-mini's"
    );
    assert_eq!(
        rig.github.runners(),
        vec!["mac-a-j0000001".to_string()],
        "nothing was removed"
    );
    assert!(rig.github.scale_sets()[0].id == id && !rig.github.scale_sets()[0].has_session);
    let refused = rig
        .console
        .scaleset_audit()
        .into_iter()
        .filter(|row| row.outcome == "refused")
        .count();
    assert!(refused >= 5, "refusals are audited: {refused}");
}

#[tokio::test]
async fn with_the_switch_off_every_route_is_off_and_github_is_not_asked() {
    let rig = rig().await;
    let id = rig.set().await;
    let asked = rig.github.requests().len();
    rig.console.set_broker("mac-a", false);
    for result in [
        rig.client.scaleset_list(REPO).await.map(|_| ()),
        rig.client.scaleset_session_open(REPO, id).await.map(|_| ()),
        rig.client
            .scaleset_jit("mac-a", REPO, id, "mac-a-j0000001")
            .await
            .map(|_| ()),
        rig.client
            .scaleset_remove("mac-a", REPO, Removal::Id(3))
            .await
            .map(|_| ()),
    ] {
        assert!(matches!(result, Err(Error::GithubOff(_))), "{result:?}");
    }
    assert_eq!(rig.github.requests().len(), asked);
}

#[tokio::test]
async fn a_removed_machine_is_unknown() {
    let rig = rig().await;
    let id = rig.set().await;
    rig.client.leave().await.unwrap();
    assert_eq!(
        rig.client.scaleset_list(REPO).await.unwrap_err(),
        Error::Unauthorized
    );
    assert_eq!(
        rig.client.scaleset_delete(REPO, id).await.unwrap_err(),
        Error::Unauthorized
    );
}

#[tokio::test]
async fn a_machine_over_its_rate_waits_to_the_end_of_the_window() {
    let rig = rig().await;
    // The windows are fixed on the console's clock: start this at the first second of one, so no window ends while the
    // calls are counted, however slow the machine running the test.
    rig.github.set_clock_offset(0);
    let into = rig.github.now_epoch().rem_euclid(RATE_WINDOW_S);
    rig.github.set_clock_offset(1 - into);
    // The set made counts in the window.
    let id = rig.set().await;
    for _ in 1..SETS_PER_WINDOW {
        rig.client.scaleset_list(REPO).await.unwrap();
    }
    let error = rig.client.scaleset_list(REPO).await.unwrap_err();
    match &error {
        Error::Busy {
            status: 429,
            retry_after,
            ..
        } => {
            assert!((1..=300).contains(&retry_after.as_secs()), "{retry_after:?}")
        }
        other => panic!("{other:?}"),
    }
    // Refused calls count too, and other buckets are their own.
    assert!(rig.client.scaleset_delete(REPO, id).await.is_err());
    let session = rig.client.scaleset_session_open(REPO, id).await.unwrap();
    for _ in 1..SESSIONS_PER_WINDOW {
        rig.client
            .scaleset_session_refresh(REPO, id, &session.session_id)
            .await
            .unwrap();
    }
    assert!(matches!(
        rig.client
            .scaleset_session_close(REPO, id, &session.session_id)
            .await
            .unwrap_err(),
        Error::Busy { status: 429, .. }
    ));
    // Another machine has its own.
    let other = enrolled(&rig.console, "mac-b", support::http()).await;
    other.scaleset_list(REPO).await.unwrap();
}
