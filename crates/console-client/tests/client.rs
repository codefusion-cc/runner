//! The client against the fake console and against consoles that go wrong: what it sends, what it refuses to send, and
//! what each failure is called.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use cf_console_client::{
    Client, Error, MAX_BODY_BYTES, MAX_JOB_EVENTS, MAX_RESULTS, MAX_RUNNERS, MAX_SCALE_SETS, Outbox, enroll,
    is_machine_name,
};
use cf_fake_console::FakeConsole;
use cf_fake_github::httpd::{Handler, Reply, Server};
use cf_ghapp::Http;
use cf_secret::Secret;
use serde_json::{Value, json};
use support::*;

/// A server that answers every call with one reply and counts the calls: a console that went wrong.
async fn answering(reply: Reply) -> (Server, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let handler: Handler = Arc::new(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
        let reply = reply.clone();
        Box::pin(async move { reply })
    });
    (Server::start(handler).await, calls)
}

fn client_for(base: &str) -> Client {
    Client::new(base, Secret::new(format!("cfr_{}", "A".repeat(43))), http())
}

#[tokio::test]
async fn enrolling_gives_the_token_and_name_and_sends_the_contracts_fields() {
    let fake = FakeConsole::start().await;
    fake.add_code("code-a");
    let enrolled = enroll(&http(), &fake.url(), &hello("code-a", "mac-a"))
        .await
        .expect("enrolled");
    assert_eq!(enrolled.name, "mac-a");
    assert_eq!(enrolled.token.expose(), &fake.tokens()[0]);
    assert_eq!(
        fake.enrollments(),
        vec![json!({
            "protocol": 1, "code": "code-a", "machine": "mac-a", "hostname": "Build-Mac-2.local", "model": "Apple M5",
            "version": "abc1234", "cpus": 10, "memoryBytes": 17179869184u64,
        })]
    );
}

#[tokio::test]
async fn enrolling_names_what_went_wrong() {
    let fake = FakeConsole::start().await;
    fake.add_code_for("for-a", "mac-a");
    fake.add_code("spent");
    // A code the console does not know.
    assert_eq!(
        enroll(&http(), &fake.url(), &hello("nope", "mac-a"))
            .await
            .unwrap_err(),
        Error::Unauthorized
    );
    // A code for another Mac is a refusal with the console's words.
    match enroll(&http(), &fake.url(), &hello("for-a", "mac-b"))
        .await
        .unwrap_err()
    {
        Error::Refused { status: 403, message } => {
            assert!(message.starts_with("This code is for mac-a"), "{message}")
        }
        other => panic!("{other:?}"),
    }
    enroll(&http(), &fake.url(), &hello("for-a", "mac-a"))
        .await
        .expect("the wrong Mac spent nothing");
    // A code whose answer was lost is gone: the second try meets a spent code.
    fake.accept_then_drop_next(1);
    assert!(matches!(
        enroll(&http(), &fake.url(), &hello("spent", "mac-b")).await,
        Err(Error::Unavailable(_))
    ));
    assert_eq!(
        enroll(&http(), &fake.url(), &hello("spent", "mac-b"))
            .await
            .unwrap_err(),
        Error::Unauthorized
    );
}

#[tokio::test]
async fn enrolling_refuses_what_the_console_would_without_sending_it() {
    let fake = FakeConsole::start().await;
    fake.add_code("code");
    for (label, mut enrol) in [
        ("capital", hello("code", "Mac-A")),
        ("25 characters", hello("code", &"a".repeat(25))),
        ("empty name", hello("code", "")),
        ("empty code", hello("", "mac-a")),
        ("201-character code", hello(&"c".repeat(201), "mac-a")),
        ("no cpus", hello("code", "mac-a")),
        ("4097 cpus", hello("code", "mac-a")),
    ] {
        match label {
            "no cpus" => enrol.cpus = 0,
            "4097 cpus" => enrol.cpus = 4097,
            _ => {}
        }
        let got = enroll(&http(), &fake.url(), &enrol).await;
        assert!(matches!(got, Err(Error::Protocol(_))), "{label}: {got:?}");
    }
    assert_eq!(fake.calls(), 0, "nothing was sent");
    assert!(is_machine_name("a") && is_machine_name(&"a".repeat(24)) && is_machine_name("a-b0"));
    assert!(
        !is_machine_name("")
            && !is_machine_name(&"a".repeat(25))
            && !is_machine_name("a--b")
            && !is_machine_name("A")
    );
}

#[tokio::test]
async fn an_enrolment_answer_that_is_not_the_contracts_is_a_protocol_error_that_quotes_nothing() {
    let wrong = [
        (
            "a token of another shape",
            json!({"token": "cfr_short", "name": "mac-a"}),
        ),
        (
            "another name",
            json!({"token": format!("cfr_{}", "B".repeat(43)), "name": "mac-b"}),
        ),
        ("no name", json!({"token": format!("cfr_{}", "B".repeat(43))})),
        ("a list", json!([1, 2])),
        ("a token that is a number", json!({"token": 7, "name": "mac-a"})),
    ];
    for (label, answer) in wrong {
        let (server, _) = answering(Reply::json(&answer)).await;
        let got = enroll(&http(), &server.url(), &hello("code", "mac-a"))
            .await
            .unwrap_err();
        let Error::Protocol(why) = &got else {
            panic!("{label}: {got:?}")
        };
        assert!(!why.contains("cfr_") && !why.contains("short"), "{label}: {why}");
    }
}

#[tokio::test]
async fn a_report_gets_the_answer_and_arrives_as_the_contract_has_it() {
    let w = world().await;
    w.fake.pause(true);
    let id = w
        .fake
        .queue_command("stop-runner", Some(json!({"runner": "mac-a-j1"})));
    let mut report = report("mac-a");
    report.jobs.push(event(1));
    report.status.runners.push(cf_console_client::RunnerStatus {
        name: "mac-a-j1".into(),
        scale_set: 4,
        state: "busy".into(),
        started: "2026-10-10T08:00:00Z".into(),
        job: Some(cf_console_client::RunnerJob {
            repo: "org/app".into(),
            name: "build".into(),
            workflow: ".github/workflows/ci.yml".into(),
            run_id: 7,
            started: "2026-10-10T08:00:00Z".into(),
        }),
        usage: Some(cf_console_client::RunnerUsage {
            cpu_percent: 12.5,
            memory_bytes: 1 << 30,
        }),
    });
    report.results.push(cf_console_client::CommandResult {
        id: "old".into(),
        ok: true,
        message: "done".into(),
        at: "2026-10-10T08:00:00Z".into(),
    });
    let answer = w.client.report(&report).await.expect("answer");
    assert!(answer.paused);
    assert_eq!(answer.report_every, 5, "a command waits");
    assert_eq!(answer.commands.len(), 1);
    assert_eq!(answer.commands[0].id, id);
    assert_eq!(answer.commands[0].kind, "stop-runner");
    assert_eq!(answer.commands[0].args, Some(json!({"runner": "mac-a-j1"})));
    let sent = &w.fake.reports()[0];
    assert_eq!(sent["protocol"], 1);
    assert_eq!(sent["status"]["machine"], "mac-a");
    assert_eq!(sent["status"]["maxJobs"], 2);
    assert_eq!(sent["status"]["scaleSets"][0]["name"], "mac-a--app-1a2b");
    assert_eq!(sent["status"]["host"]["freeMemoryPercent"], 60.0);
    assert_eq!(sent["status"]["runners"][0]["scaleSet"], 4);
    assert_eq!(sent["status"]["runners"][0]["job"]["runId"], 7);
    assert_eq!(sent["status"]["runners"][0]["usage"]["memoryBytes"], 1 << 30);
    assert_eq!(sent["activity"]["cpuPercent"], 25.0);
    assert_eq!(sent["results"][0]["id"], "old");
    // The Runner's own fields travel with the event; `outOfMemory` is left out when false, as the contract says.
    assert_eq!(sent["jobs"][0]["id"], "e0001");
    assert_eq!(sent["jobs"][0]["runId"], 1001);
    assert_eq!(sent["jobs"][0]["end"], "timeout");
    assert_eq!(sent["jobs"][0]["machine"]["memoryBytes"], 4u64 << 30);
    assert_eq!(sent["jobs"][0]["machine"]["bootMs"], 2100);
    assert!(sent["jobs"][0].get("outOfMemory").is_none() && sent["jobs"][0].get("startedAt").is_none());
    // Fields left as `None` are left out, not written as null.
    assert!(sent["status"].get("reason").is_none() && sent["status"].get("update").is_none());
    assert!(sent.get("settings").is_none() && sent.get("image").is_none());
}

#[tokio::test]
async fn a_report_the_console_would_refuse_is_not_sent() {
    let w = world().await;
    let calls = w.fake.calls();
    let runner = |i: usize| cf_console_client::RunnerStatus {
        name: format!("r{i}"),
        scale_set: 1,
        state: "idle".into(),
        started: "2026-10-10T08:00:00Z".into(),
        job: None,
        usage: None,
    };
    let scale_set = |i: usize| cf_console_client::ScaleSetStatus {
        repo: "org/app".into(),
        name: format!("s{i}"),
        id: i as i64,
        labels: vec![],
        capacity: 1,
        waiting: 0,
        error: None,
        error_at: None,
    };
    let mut cases: Vec<(&str, cf_console_client::Report)> = Vec::new();
    for (label, name) in [
        ("a capital", "Mac-A"),
        ("an empty name", ""),
        ("25 characters", &"a".repeat(25)[..]),
        ("a slash", "../etc"),
    ] {
        cases.push((label, report(name)));
    }
    let mut r = report("mac-a");
    r.jobs = (0..=MAX_JOB_EVENTS).map(event).collect();
    cases.push(("501 events", r));
    let mut r = report("mac-a");
    r.results = (0..=MAX_RESULTS)
        .map(|i| cf_console_client::CommandResult {
            id: format!("c{i}"),
            ok: true,
            message: String::new(),
            at: "2026-10-10".into(),
        })
        .collect();
    cases.push(("51 results", r));
    let mut r = report("mac-a");
    r.status.runners = (0..=MAX_RUNNERS).map(runner).collect();
    cases.push(("65 Actions runners", r));
    let mut r = report("mac-a");
    r.status.scale_sets = (0..=MAX_SCALE_SETS).map(scale_set).collect();
    cases.push(("501 scale sets", r));
    let mut r = report("mac-a");
    r.jobs = vec![cf_console_client::JobEvent {
        runner: String::new(),
        ..event(1)
    }];
    cases.push(("an event without an Actions runner", r));
    let mut r = report("mac-a");
    r.jobs = vec![cf_console_client::JobEvent {
        runner: "r".repeat(101),
        ..event(1)
    }];
    cases.push(("an Actions runner name of 101", r));
    let mut r = report("mac-a");
    r.jobs = vec![cf_console_client::JobEvent {
        runner: "🚀".repeat(51),
        ..event(1)
    }];
    cases.push(("51 rockets are 102 units", r));
    let mut r = report("mac-a");
    r.results = vec![cf_console_client::CommandResult {
        id: String::new(),
        ok: true,
        message: String::new(),
        at: "2026-10-10".into(),
    }];
    cases.push(("a result without an id", r));
    let mut r = report("mac-a");
    r.results = vec![cf_console_client::CommandResult {
        id: "c".repeat(65),
        ok: true,
        message: String::new(),
        at: "2026-10-10".into(),
    }];
    cases.push(("a result id of 65", r));
    for (label, report) in cases {
        let got = w.client.report(&report).await;
        assert!(matches!(got, Err(Error::Protocol(_))), "{label}: {got:?}");
    }
    assert_eq!(w.fake.calls(), calls, "nothing was sent");
}

#[tokio::test]
async fn every_bound_at_its_maximum_is_sent_and_accepted() {
    let w = world().await;
    let mut r = report("mac-a");
    r.jobs = (0..MAX_JOB_EVENTS).map(event).collect();
    r.results = (0..MAX_RESULTS)
        .map(|i| cf_console_client::CommandResult {
            id: format!("c{i}"),
            ok: true,
            message: "m".repeat(1000),
            at: "2026-10-10".into(),
        })
        .collect();
    r.status.runners = (0..MAX_RUNNERS)
        .map(|i| cf_console_client::RunnerStatus {
            name: format!("r{i}"),
            scale_set: 1,
            state: "idle".into(),
            started: "2026-10-10T08:00:00Z".into(),
            job: None,
            usage: None,
        })
        .collect();
    r.status.scale_sets = (0..MAX_SCALE_SETS)
        .map(|i| cf_console_client::ScaleSetStatus {
            repo: "org/app".into(),
            name: format!("s{i}"),
            id: i as i64,
            labels: (0..30).map(|l| format!("l{l}")).collect(),
            capacity: 1,
            waiting: 0,
            error: None,
            error_at: None,
        })
        .collect();
    w.client.report(&r).await.expect("everything at its maximum");
    assert_eq!(w.fake.job_events().len(), 500);
}

#[tokio::test]
async fn a_body_of_exactly_512_kib_is_sent_and_one_byte_more_is_not() {
    let w = world().await;
    // The body is the report with `"protocol":1` added. Events with every text as long as the console takes it
    // (four times its bound) fill most of it, and one more event takes up the rest, a byte at a time.
    let body_len = |r: &cf_console_client::Report| {
        let mut value = serde_json::to_value(r).expect("json");
        value["protocol"] = json!(1);
        serde_json::to_vec(&value).expect("json").len()
    };
    let full = |n: usize| cf_console_client::JobEvent {
        repo: Some("r".repeat(800)),
        job: Some("j".repeat(1200)),
        workflow: Some("w".repeat(1600)),
        event: Some("e".repeat(200)),
        result: Some("x".repeat(120)),
        machine: None,
        ..event(n)
    };
    let mut r = report("mac-a");
    loop {
        r.jobs.push(full(r.jobs.len()));
        if body_len(&r) > MAX_BODY_BYTES {
            r.jobs.pop();
            break;
        }
    }
    let empty = Some(String::new());
    let last = cf_console_client::JobEvent {
        repo: empty.clone(),
        job: empty.clone(),
        workflow: empty.clone(),
        event: empty.clone(),
        result: empty,
        machine: None,
        ..event(r.jobs.len())
    };
    r.jobs.push(last);
    let mut left = MAX_BODY_BYTES - body_len(&r);
    let at = r.jobs.len() - 1;
    for (field, cap) in [(0, 800), (1, 1200), (2, 1600), (3, 200), (4, 120)] {
        let add = left.min(cap);
        left -= add;
        let text = Some("p".repeat(add));
        let e = &mut r.jobs[at];
        match field {
            0 => e.repo = text,
            1 => e.job = text,
            2 => e.workflow = text,
            3 => e.event = text,
            _ => e.result = text,
        }
    }
    assert_eq!(left, 0, "the padding fits");
    assert_eq!(body_len(&r), MAX_BODY_BYTES);
    assert!(r.jobs.len() < MAX_JOB_EVENTS);
    w.client.report(&r).await.expect("exactly 512 KiB");
    assert_eq!(w.fake.reports().len(), 1);
    let calls = w.fake.calls();
    r.jobs[at].job = Some(format!("{}j", r.jobs[at].job.as_deref().unwrap_or("")));
    assert_eq!(body_len(&r), MAX_BODY_BYTES + 1);
    assert!(matches!(w.client.report(&r).await, Err(Error::Protocol(_))));
    assert_eq!(w.fake.calls(), calls);
}

#[tokio::test]
async fn each_failure_has_its_name() {
    let w = world().await;
    let report = report("mac-a");
    w.fake.fail_next(1, 401);
    assert_eq!(w.client.report(&report).await.unwrap_err(), Error::Unauthorized);
    for status in [429, 500, 502, 503] {
        w.fake.fail_next(1, status);
        let got = w.client.report(&report).await.unwrap_err();
        assert!(
            matches!(got, Error::Unavailable(_)) && got.is_transient(),
            "{status}: {got:?}"
        );
    }
    for status in [400, 403, 404, 413, 415] {
        w.fake.fail_next(1, status);
        match w.client.report(&report).await.unwrap_err() {
            Error::Refused { status: got, message } => {
                assert_eq!((got, message.as_str()), (status, "The console fails on purpose"))
            }
            other => panic!("{status}: {other:?}"),
        }
    }
    w.fake.down(true);
    assert!(matches!(
        w.client.report(&report).await,
        Err(Error::Unavailable(_))
    ));
    w.fake.down(false);
    w.fake.drop_before_next(1);
    assert!(matches!(
        w.client.report(&report).await,
        Err(Error::Unavailable(_))
    ));
    w.fake.accept_then_drop_next(1);
    assert!(matches!(
        w.client.report(&report).await,
        Err(Error::Unavailable(_))
    ));
    w.client.report(&report).await.expect("works again");
}

#[tokio::test]
async fn retries_inside_the_http_client_resend_the_same_body() {
    let w = world_with(Http::new(Duration::from_secs(5), fast_retry())).await;
    w.fake.fail_next(2, 503);
    let mut r = report("mac-a");
    r.jobs = vec![event(1)];
    w.client.report(&r).await.expect("the third try works");
    assert_eq!(w.fake.reports().len(), 1);
    assert_eq!(w.fake.calls(), 1 + 3, "the enrolment and three tries");
    // Three failures in a row exhaust two retries.
    w.fake.fail_next(3, 503);
    assert!(matches!(w.client.report(&r).await, Err(Error::Unavailable(_))));
}

#[tokio::test]
async fn an_answer_that_is_not_the_contracts_is_a_protocol_error() {
    let report = report("mac-a");
    let page = Reply::text(200, "<html><body>Sign in to continue</body></html>");
    let wrong = [
        ("a sign-in page", page),
        ("an empty 200", Reply::empty(200)),
        (
            "no commands",
            Reply::json(&json!({"paused": false, "reportEvery": 60})),
        ),
        (
            "a report interval of 0",
            Reply::json(&json!({"paused": false, "reportEvery": 0, "commands": []})),
        ),
        (
            "a negative interval",
            Reply::json(&json!({"paused": false, "reportEvery": -5, "commands": []})),
        ),
        (
            "a command without an id",
            Reply::json(&json!({"paused": false, "reportEvery": 5, "commands": [{"kind": "restart"}]})),
        ),
        ("a list", Reply::json(&json!([]))),
        (
            "a redirect",
            Reply::empty(302).with_header("location", "https://elsewhere.example/sign-in"),
        ),
        ("a 204", Reply::empty(204)),
    ];
    for (label, reply) in wrong {
        let (server, calls) = answering(reply).await;
        let got = client_for(&server.url()).report(&report).await.unwrap_err();
        assert!(matches!(got, Error::Protocol(_)), "{label}: {got:?}");
        assert!(
            !got.to_string().contains("Sign in"),
            "{label}: the answer is not quoted: {got}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "{label}");
    }
}

#[tokio::test]
async fn an_html_error_page_is_a_refusal_without_a_message_not_a_crash() {
    let (server, _) = answering(Reply::text(403, "<html>Access denied</html>")).await;
    match client_for(&server.url())
        .report(&report("mac-a"))
        .await
        .unwrap_err()
    {
        Error::Refused { status: 403, message } => assert_eq!(message, "no message from the console"),
        other => panic!("{other:?}"),
    }
    let (server, _) = answering(Reply::status_json(
        400,
        &json!({"error": format!("bad\u{7}{}", "x".repeat(900))}),
    ))
    .await;
    match client_for(&server.url())
        .report(&report("mac-a"))
        .await
        .unwrap_err()
    {
        // The status error keeps 512 bytes of the body, which cuts the JSON: no message to read.
        Error::Refused { status: 400, message } => assert!(message.len() <= 300, "{message}"),
        other => panic!("{other:?}"),
    }
    let (server, _) = answering(Reply::status_json(
        409,
        &json!({"error": format!("bad\u{7} thing{}", "x".repeat(400))}),
    ))
    .await;
    match client_for(&server.url())
        .report(&report("mac-a"))
        .await
        .unwrap_err()
    {
        Error::Refused { status: 409, message } => {
            assert!(message.len() <= 300 && !message.contains('\u{7}'), "{message}")
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_console_that_hangs_a_cut_answer_and_a_closed_port_are_unavailable_and_nothing_panics() {
    let report = report("mac-a");
    let short = Http::new(Duration::from_millis(300), no_retry());
    let hang = Client::new(
        &answering(Reply::Hang).await.0.url(),
        Secret::new(format!("cfr_{}", "A".repeat(43))),
        short,
    );
    assert!(matches!(hang.report(&report).await, Err(Error::Unavailable(_))));
    let body =
        serde_json::to_vec(&json!({"paused": false, "reportEvery": 60, "commands": []})).expect("json");
    let (server, _) = answering(Reply::Truncated {
        status: 200,
        body,
        sent: 10,
    })
    .await;
    assert!(matches!(
        client_for(&server.url()).report(&report).await,
        Err(Error::Unavailable(_))
    ));
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        format!("http://{}", listener.local_addr().expect("addr"))
    };
    assert!(matches!(
        client_for(&closed).report(&report).await,
        Err(Error::Unavailable(_))
    ));
}

#[tokio::test]
async fn an_address_the_token_may_not_go_to_and_a_token_that_cannot_be_the_consoles_send_nothing() {
    let report = report("mac-a");
    let secret = Secret::new(format!("cfr_{}", "A".repeat(43)));
    // Plain HTTP to a host that is not this machine.
    let got = Client::new("http://console.example.invalid", secret.clone(), http())
        .report(&report)
        .await;
    assert!(matches!(got, Err(Error::Protocol(_))), "{got:?}");
    let got = Client::new("not a url", secret, http()).report(&report).await;
    assert!(matches!(got, Err(Error::Protocol(_))), "{got:?}");
    let w = world().await;
    let calls = w.fake.calls();
    for token in [
        "",
        "cfr_short",
        "x".repeat(47).as_str(),
        "cfr_\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n",
    ] {
        let got = Client::new(&w.fake.url(), Secret::new(token.to_string()), http())
            .report(&report)
            .await;
        assert_eq!(got.unwrap_err(), Error::Unauthorized, "{token:?}");
    }
    assert_eq!(w.fake.calls(), calls);
    // A well formed token the console does not know is the console's 401.
    let unknown = Client::new(
        &w.fake.url(),
        Secret::new(format!("cfr_{}", "Z".repeat(43))),
        http(),
    );
    assert_eq!(unknown.report(&report).await.unwrap_err(), Error::Unauthorized);
    assert_eq!(w.fake.calls(), calls + 1);
}

#[tokio::test]
async fn leaving_ends_the_token() {
    let w = world().await;
    w.client.leave().await.expect("left");
    assert_eq!(w.fake.left(), vec!["mac-a".to_string()]);
    assert_eq!(
        w.client.report(&report("mac-a")).await.unwrap_err(),
        Error::Unauthorized
    );
    assert_eq!(w.client.leave().await.unwrap_err(), Error::Unauthorized);
    let (server, _) = answering(Reply::json(&json!({"ok": false}))).await;
    assert!(matches!(
        client_for(&server.url()).leave().await,
        Err(Error::Protocol(_))
    ));
    let (server, _) = answering(Reply::text(200, "<html>")).await;
    assert!(matches!(
        client_for(&server.url()).leave().await,
        Err(Error::Protocol(_))
    ));
}

#[tokio::test]
async fn the_token_and_the_code_show_in_no_debug_output_and_no_error() {
    let w = world().await;
    let token = w.token.clone();
    let mut shown: Vec<String> = vec![format!("{:?}", w.client), format!("{:#?}", w.client)];
    let enrolment = hello("code-secret-123", "mac-b");
    shown.push(format!("{enrolment:?}"));
    w.fake.add_code("code-secret-123");
    let enrolled = enroll(&http(), &w.fake.url(), &enrolment)
        .await
        .expect("enrolled");
    let second_token = enrolled.token.expose().clone();
    shown.push(format!("{enrolled:?}"));
    let report = report("mac-a");
    for fault in 0..8 {
        match fault {
            0 => w.fake.fail_next(1, 401),
            1 => w.fake.fail_next(1, 400),
            2 => w.fake.fail_next(1, 503),
            3 => w.fake.down(true),
            4 => w.fake.drop_before_next(1),
            5 => w.fake.accept_then_drop_next(1),
            _ => {}
        }
        if let Err(error) = w.client.report(&report).await {
            shown.push(format!("{error}"));
            shown.push(format!("{error:?}"));
        }
        w.fake.down(false);
    }
    let mut bad = report.clone();
    bad.status.machine = "NOT VALID".into();
    shown.push(format!("{:?}", w.client.report(&bad).await.unwrap_err()));
    shown.push(format!(
        "{:?}",
        enroll(&http(), &w.fake.url(), &hello("wrong", "mac-b"))
            .await
            .unwrap_err()
    ));
    let (server, _) = answering(Reply::json(&json!({"token": token, "name": "other"}))).await;
    shown.push(format!(
        "{:?}",
        enroll(&http(), &server.url(), &hello("code", "mac-b"))
            .await
            .unwrap_err()
    ));
    assert!(shown.len() > 10);
    for text in shown {
        for secret in [&token, &second_token, &"code-secret-123".to_string()] {
            assert!(!text.contains(secret.as_str()), "{text}");
        }
    }
    let _: Value = json!(null);
}

#[tokio::test]
async fn events_with_empty_very_long_and_unicode_names_reach_the_console_as_the_console_keeps_them() {
    let w = world().await;
    let outbox_dir = tempfile::tempdir().expect("dir");
    let outbox = Outbox::open(outbox_dir.path().join("outbox.json"), 500);
    let mut cases = Vec::new();
    for (n, (job, workflow)) in [
        ("", ""),
        ("ビルド 🚀 ünïcödé", "ワークフロー/ci.yml 🚀"),
        ("j", "w"),
        (&"j".repeat(300), &"w".repeat(400)),
        (&"🚀".repeat(150), &"é".repeat(400)),
    ]
    .into_iter()
    .enumerate()
    {
        let e = cf_console_client::JobEvent {
            job: Some(job.into()),
            workflow: Some(workflow.into()),
            ..event(n)
        };
        assert!(outbox.push(e.clone()));
        cases.push(e);
    }
    // Longer than the console keeps: cut on the way into the outbox, so the report stays small and the console never
    // has to refuse it.
    let long = cf_console_client::JobEvent {
        job: Some("j".repeat(100_000)),
        workflow: Some("w".repeat(100_000)),
        ..event(9)
    };
    assert!(outbox.push(long));
    w.client
        .deliver(&outbox, report("mac-a"))
        .await
        .expect("delivered");
    let seen = w.fake.job_events();
    assert_eq!(seen.len(), 6);
    for (sent, seen) in cases.iter().zip(&seen) {
        assert_eq!(seen["job"].as_str(), sent.job.as_deref());
        assert_eq!(seen["workflow"].as_str(), sent.workflow.as_deref());
    }
    assert_eq!(seen[5]["job"].as_str().map(|s| s.chars().count()), Some(300));
    assert_eq!(seen[5]["workflow"].as_str().map(|s| s.chars().count()), Some(400));
}
