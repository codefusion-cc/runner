//! The fake console against protocol 1 as the console's own code reads it (`worker/runners/protocol.ts` and
//! `agent.ts`): every bound at its maximum and one past it, the order of the checks, and the statuses. These tests
//! send raw JSON written from the contract, so they check the fake and, through it, what the client may send.

mod support;

use cf_fake_console::MAX_BODY;
use serde_json::{Value, json};
use support::*;

/// Sends `body` as a report and asserts the status; a refused report leaves nothing in the fake.
async fn report_gets(w: &World, label: &str, body: &Value, status: u16) {
    let before = w.fake.reports().len();
    let got = post_json(w, "report", body).await;
    assert_eq!(got.status, status, "{label}: {}", got.error());
    assert!(
        got.content_type.starts_with("application/json"),
        "{label}: every answer is JSON"
    );
    if status == 200 {
        assert_eq!(w.fake.reports().len(), before + 1, "{label}");
    } else {
        assert_eq!(
            w.fake.reports().len(),
            before,
            "{label}: a refused report is not kept"
        );
        assert!(
            got.error().starts_with("Not a report this console reads: ") || status != 400,
            "{label}: {}",
            got.error()
        );
    }
}

fn with(f: impl FnOnce(&mut Value)) -> Value {
    let mut report = report_json("mac-a");
    f(&mut report);
    report
}

#[tokio::test]
async fn lists_are_bounded_at_500_events_50_results_64_runners_500_scale_sets_30_labels() {
    let w = world().await;
    let result = |i: usize| json!({"id": format!("c{i}"), "ok": true, "message": "done", "at": "2026-10-10T08:00:00Z"});
    let runner = |i: usize| json!({"name": format!("mac-a-j{i}"), "scaleSet": 4, "state": "busy", "started": "2026-10-10T08:00:00Z"});
    let scale_set =
        |i: usize| json!({"repo": "org/app", "name": format!("s{i}"), "id": i, "labels": [], "capacity": 1});
    for (label, field, make, max) in [
        (
            "jobs",
            "jobs",
            Box::new(event_json) as Box<dyn Fn(usize) -> Value>,
            500usize,
        ),
        ("results", "results", Box::new(result), 50),
    ] {
        let list = |n: usize| with(|r| r[field] = Value::Array((0..n).map(&make).collect()));
        report_gets(&w, &format!("{label} at the maximum"), &list(max), 200).await;
        report_gets(&w, &format!("{label} one past it"), &list(max + 1), 400).await;
        report_gets(&w, &format!("{label} empty"), &list(0), 200).await;
    }
    for (label, make, max) in [(
        "runners",
        Box::new(runner) as Box<dyn Fn(usize) -> Value>,
        64usize,
    )] {
        let list = |n: usize| with(|r| r["status"]["runners"] = Value::Array((0..n).map(&make).collect()));
        report_gets(&w, &format!("{label} at the maximum"), &list(max), 200).await;
        report_gets(&w, &format!("{label} one past it"), &list(max + 1), 400).await;
    }
    let sets = |n: usize| with(|r| r["status"]["scaleSets"] = Value::Array((0..n).map(scale_set).collect()));
    report_gets(&w, "scale sets at the maximum", &sets(500), 200).await;
    report_gets(&w, "scale sets one past it", &sets(501), 400).await;
    let labels = |n: usize| {
        with(|r| {
            r["status"]["scaleSets"][0]["labels"] =
                Value::Array((0..n).map(|i| json!(format!("l{i}"))).collect())
        })
    };
    report_gets(&w, "30 labels", &labels(30), 200).await;
    report_gets(&w, "31 labels", &labels(31), 400).await;
}

#[tokio::test]
async fn texts_are_cut_by_the_console_up_to_four_times_their_bound_and_refused_past_it() {
    let w = world().await;
    type Case<'a> = (&'a str, usize, Box<dyn Fn(usize) -> Value>);
    let cases: Vec<Case> = vec![
        (
            "runner name",
            100,
            Box::new(|n| {
                with(
                    |r| r["status"]["runners"] = json!([{"name": "x".repeat(n), "scaleSet": 4, "state": "busy", "started": "2026-10-10T08:00:00Z"}]),
                )
            }),
        ),
        (
            "runner state",
            20,
            Box::new(|n| {
                with(
                    |r| r["status"]["runners"] = json!([{"name": "n", "scaleSet": 4, "state": "x".repeat(n), "started": "2026-10-10T08:00:00Z"}]),
                )
            }),
        ),
        (
            "status reason",
            300,
            Box::new(|n| with(|r| r["status"]["reason"] = json!("x".repeat(n)))),
        ),
        (
            "host error",
            300,
            Box::new(|n| with(|r| r["status"]["hostError"] = json!("x".repeat(n)))),
        ),
        (
            "discovery error",
            1000,
            Box::new(|n| with(|r| r["status"]["discoveryError"] = json!("x".repeat(n)))),
        ),
        (
            "scale set error",
            1000,
            Box::new(|n| with(|r| r["status"]["scaleSets"][0]["error"] = json!("x".repeat(n)))),
        ),
        (
            "version",
            64,
            Box::new(|n| with(|r| r["status"]["version"] = json!("x".repeat(n)))),
        ),
        (
            "hostname",
            255,
            Box::new(|n| with(|r| r["hostname"] = json!("x".repeat(n)))),
        ),
        (
            "model",
            200,
            Box::new(|n| with(|r| r["model"] = json!("x".repeat(n)))),
        ),
        (
            "event repo",
            200,
            Box::new(|n| {
                with(|r| {
                    r["jobs"] =
                        json!([{"kind": "started", "runner": "r", "at": "2026-10-10", "repo": "x".repeat(n)}])
                })
            }),
        ),
        (
            "event job",
            300,
            Box::new(|n| {
                with(|r| {
                    r["jobs"] =
                        json!([{"kind": "started", "runner": "r", "at": "2026-10-10", "job": "x".repeat(n)}])
                })
            }),
        ),
        (
            "event workflow",
            400,
            Box::new(|n| {
                with(
                    |r| r["jobs"] = json!([{"kind": "started", "runner": "r", "at": "2026-10-10", "workflow": "x".repeat(n)}]),
                )
            }),
        ),
        (
            "event kind of trigger",
            50,
            Box::new(|n| {
                with(|r| {
                    r["jobs"] = json!([{"kind": "started", "runner": "r", "at": "2026-10-10", "event": "x".repeat(n)}])
                })
            }),
        ),
        (
            "event result",
            30,
            Box::new(|n| {
                with(
                    |r| r["jobs"] = json!([{"kind": "completed", "runner": "r", "at": "2026-10-10", "result": "x".repeat(n)}]),
                )
            }),
        ),
        (
            "result message",
            1000,
            Box::new(|n| {
                with(|r| {
                    r["results"] =
                        json!([{"id": "c", "ok": true, "message": "x".repeat(n), "at": "2026-10-10"}])
                })
            }),
        ),
    ];
    for (label, bound, make) in cases {
        report_gets(&w, &format!("{label}: at the bound"), &make(bound), 200).await;
        report_gets(
            &w,
            &format!("{label}: at four times the bound (the console cuts it)"),
            &make(bound * 4),
            200,
        )
        .await;
        report_gets(
            &w,
            &format!("{label}: one past four times the bound"),
            &make(bound * 4 + 1),
            400,
        )
        .await;
    }
}

#[tokio::test]
async fn the_runner_name_of_a_job_event_is_one_to_100_units_with_no_cutting() {
    let w = world().await;
    let named = |name: String| {
        with(|r| r["jobs"] = json!([{"kind": "started", "runner": name, "at": "2026-10-10T08:00:00Z"}]))
    };
    report_gets(&w, "empty", &named(String::new()), 400).await;
    report_gets(&w, "one", &named("a".into()), 200).await;
    report_gets(&w, "100", &named("a".repeat(100)), 200).await;
    report_gets(&w, "101", &named("a".repeat(101)), 400).await;
    // Length counts UTF-16 units, as JavaScript does: a rocket is two.
    report_gets(&w, "50 rockets are 100 units", &named("🚀".repeat(50)), 200).await;
    report_gets(&w, "51 rockets are 102 units", &named("🚀".repeat(51)), 400).await;
    report_gets(
        &w,
        "100 two-byte letters are 100 units",
        &named("ä".repeat(100)),
        200,
    )
    .await;
    // The runner must be a string, and the id of a result 1 to 64.
    report_gets(
        &w,
        "a number",
        &with(|r| r["jobs"] = json!([{"kind": "started", "runner": 7, "at": "2026-10-10"}])),
        400,
    )
    .await;
    let result = |id: String| {
        with(|r| r["results"] = json!([{"id": id, "ok": true, "message": "", "at": "2026-10-10T08:00:00Z"}]))
    };
    report_gets(&w, "result id empty", &result(String::new()), 400).await;
    report_gets(&w, "result id 64", &result("c".repeat(64)), 200).await;
    report_gets(&w, "result id 65", &result("c".repeat(65)), 400).await;
}

#[tokio::test]
async fn an_unknown_event_kind_refuses_the_whole_report_and_keeps_none_of_its_events() {
    let w = world().await;
    let body = with(|r| {
        r["jobs"] = json!([
            {"kind": "started", "runner": "mac-a-j1", "at": "2026-10-10T08:00:00Z"},
            {"kind": "queued", "runner": "mac-a-j2", "at": "2026-10-10T08:00:00Z"},
        ])
    });
    report_gets(&w, "queued", &body, 400).await;
    for kind in ["", "Started", "COMPLETED", "start"] {
        let body = with(|r| r["jobs"] = json!([{"kind": kind, "runner": "r", "at": "2026-10-10"}]));
        report_gets(&w, &format!("kind {kind:?}"), &body, 400).await;
    }
    assert!(
        w.fake.job_events().is_empty(),
        "nothing of a refused report is kept"
    );
}

#[tokio::test]
async fn the_body_may_be_512_kib_and_not_a_byte_more() {
    let w = world().await;
    let padded = |extra: usize| {
        let mut body = report_json("mac-a");
        body["pad"] = json!("");
        let base = serde_json::to_vec(&body).expect("json").len();
        body["pad"] = json!("x".repeat(MAX_BODY + extra - base));
        let bytes = serde_json::to_vec(&body).expect("json");
        assert_eq!(bytes.len(), MAX_BODY + extra);
        bytes
    };
    let send = |bytes: Vec<u8>| async {
        raw(
            &w.fake.url(),
            "POST",
            "/agent/runners/report",
            Some(&format!("Bearer {}", w.token)),
            Some("application/json"),
            bytes,
        )
        .await
    };
    assert_eq!(
        send(padded(0)).await.status,
        200,
        "exactly 512 KiB, with an unknown field the console drops"
    );
    let over = send(padded(1)).await;
    assert_eq!(over.status, 413);
    assert_eq!(over.error(), "Request too large");
    assert_eq!(w.fake.reports().len(), 1);
    // The same bound on a connect request, which reads its body before it looks at the code.
    w.fake.add_code("code-b");
    let mut hello = hello_json("code-b", "mac-b");
    hello["pad"] = json!("x".repeat(MAX_BODY));
    assert_eq!(post_hello(&w.fake, &hello).await.status, 413);
}

#[tokio::test]
async fn a_token_that_is_missing_malformed_or_unknown_is_a_401_before_the_body_is_looked_at() {
    let w = world().await;
    let good = format!("Bearer {}", w.token);
    let unknown = format!("Bearer cfr_{}", "A".repeat(43));
    let attempts: Vec<(&str, Option<String>)> = vec![
        ("no header", None),
        ("empty", Some(String::new())),
        ("not a bearer", Some(w.token.clone())),
        ("lower case scheme", Some(format!("bearer {}", w.token))),
        ("one character short", Some(good[..good.len() - 1].to_string())),
        ("one character long", Some(format!("{good}A"))),
        ("wrong prefix", Some(format!("Bearer cfx_{}", &w.token[4..]))),
        (
            "a character outside the alphabet",
            Some(format!("Bearer cfr_{}!", "A".repeat(42))),
        ),
        ("well formed, unknown", Some(unknown)),
    ];
    for (label, header) in attempts {
        // A body that would be refused for its type and its size: the token comes first.
        let got = raw(
            &w.fake.url(),
            "POST",
            "/agent/runners/report",
            header.as_deref(),
            Some("text/plain"),
            vec![b' '; MAX_BODY + 1],
        )
        .await;
        assert_eq!(got.status, 401, "{label}");
        assert_eq!(got.error(), "Unknown token", "{label}");
    }
    assert_eq!(
        raw(&w.fake.url(), "POST", "/agent/runners/leave", None, None, vec![])
            .await
            .status,
        401
    );
    assert!(w.fake.reports().is_empty());
}

#[tokio::test]
async fn the_content_type_the_method_and_the_path_are_judged_in_the_consoles_order() {
    let w = world().await;
    let auth = format!("Bearer {}", w.token);
    let body = serde_json::to_vec(&report_json("mac-a")).expect("json");
    let call = |method: &'static str,
                path: &'static str,
                token: Option<&str>,
                kind: Option<&'static str>,
                body: Vec<u8>| {
        let url = w.fake.url();
        let token = token.map(str::to_string);
        async move { raw(&url, method, path, token.as_deref(), kind, body).await }
    };
    let got = call(
        "POST",
        "/agent/runners/report",
        Some(&auth),
        Some("text/plain"),
        body.clone(),
    )
    .await;
    assert_eq!((got.status, got.error().as_str()), (415, "Expected JSON"));
    assert_eq!(
        call("POST", "/agent/runners/report", Some(&auth), None, body.clone())
            .await
            .status,
        415
    );
    // `startsWith('application/json')`: a charset is fine, a different case is not.
    assert_eq!(
        call(
            "POST",
            "/agent/runners/report",
            Some(&auth),
            Some("application/json; charset=utf-8"),
            body.clone()
        )
        .await
        .status,
        200
    );
    assert_eq!(
        call(
            "POST",
            "/agent/runners/report",
            Some(&auth),
            Some("Application/JSON"),
            body.clone()
        )
        .await
        .status,
        415
    );
    for (label, bytes) in [
        ("not JSON", b"{\"protocol\":".to_vec()),
        ("empty", vec![]),
        ("a BOM and then nothing", b"\xef\xbb\xbf".to_vec()),
    ] {
        let got = call(
            "POST",
            "/agent/runners/report",
            Some(&auth),
            Some("application/json"),
            bytes,
        )
        .await;
        assert_eq!(
            (got.status, got.error().as_str()),
            (400, "Expected a JSON object"),
            "{label}"
        );
    }
    // `null`, a list and a number are JSON; the schema refuses them.
    for text in ["null", "[]", "7", "\"report\""] {
        let got = call(
            "POST",
            "/agent/runners/report",
            Some(&auth),
            Some("application/json"),
            text.as_bytes().to_vec(),
        )
        .await;
        assert_eq!(got.status, 400, "{text}");
        assert!(
            got.error().starts_with("Not a report this console reads: body: "),
            "{}",
            got.error()
        );
    }
    // A byte order mark is dropped, as TextDecoder drops it.
    let mut with_bom = b"\xef\xbb\xbf".to_vec();
    with_bom.extend_from_slice(&body);
    assert_eq!(
        call(
            "POST",
            "/agent/runners/report",
            Some(&auth),
            Some("application/json"),
            with_bom
        )
        .await
        .status,
        200
    );
    // The method is judged before the path, the path before the token.
    for method in ["GET", "PUT", "DELETE"] {
        let got = call(
            method,
            "/agent/runners/elsewhere",
            Some(&auth),
            Some("application/json"),
            vec![],
        )
        .await;
        assert_eq!((got.status, got.error().as_str()), (405, "Use POST"), "{method}");
    }
    let got = call("POST", "/agent/runners/elsewhere", None, None, vec![]).await;
    assert_eq!((got.status, got.error().as_str()), (404, "Not found"));
    assert_eq!(
        call(
            "POST",
            "/agent/runners/report/",
            Some(&auth),
            Some("application/json"),
            body
        )
        .await
        .status,
        404
    );
}

#[tokio::test]
async fn protocol_must_be_the_number_one() {
    let w = world().await;
    for (label, value) in [
        ("2", json!(2)),
        ("0", json!(0)),
        ("a string", json!("1")),
        ("null", Value::Null),
        ("true", json!(true)),
    ] {
        report_gets(
            &w,
            &format!("protocol {label}"),
            &with(|r| r["protocol"] = value),
            400,
        )
        .await;
    }
    report_gets(
        &w,
        "protocol missing",
        &with(|r| {
            r.as_object_mut().expect("object").remove("protocol");
        }),
        400,
    )
    .await;
    report_gets(
        &w,
        "protocol 1.0 is the number one in JSON",
        &with(|r| r["protocol"] = json!(1.0)),
        200,
    )
    .await;
    w.fake.add_code("code-b");
    for (label, value) in [("2", json!(2)), ("missing", Value::Null)] {
        let mut hello = hello_json("code-b", "mac-b");
        hello["protocol"] = value;
        if label == "missing" {
            hello.as_object_mut().expect("object").remove("protocol");
        }
        assert_eq!(
            post_hello(&w.fake, &hello).await.status,
            400,
            "enrol with protocol {label}"
        );
    }
}

#[tokio::test]
async fn fields_the_console_defaults_take_absence_but_not_null_and_nullish_ones_take_both() {
    let w = world().await;
    report_gets(
        &w,
        "hostname absent",
        &with(|r| {
            r.as_object_mut().expect("o").remove("hostname");
        }),
        200,
    )
    .await;
    report_gets(&w, "hostname null", &with(|r| r["hostname"] = Value::Null), 400).await;
    report_gets(
        &w,
        "activity absent",
        &with(|r| {
            r.as_object_mut().expect("o").remove("activity");
        }),
        200,
    )
    .await;
    report_gets(&w, "activity null", &with(|r| r["activity"] = Value::Null), 400).await;
    report_gets(&w, "settings null", &with(|r| r["settings"] = Value::Null), 200).await;
    report_gets(
        &w,
        "settings absent",
        &with(|r| {
            r.as_object_mut().expect("o").remove("settings");
        }),
        200,
    )
    .await;
    report_gets(&w, "host null", &with(|r| r["status"]["host"] = Value::Null), 200).await;
    report_gets(&w, "jobs null", &with(|r| r["jobs"] = Value::Null), 200).await;
    report_gets(
        &w,
        "jobs absent",
        &with(|r| {
            r.as_object_mut().expect("o").remove("jobs");
        }),
        200,
    )
    .await;
    report_gets(
        &w,
        "status absent",
        &with(|r| {
            r.as_object_mut().expect("o").remove("status");
        }),
        400,
    )
    .await;
    report_gets(
        &w,
        "status open absent",
        &with(|r| {
            r["status"].as_object_mut().expect("o").remove("open");
        }),
        400,
    )
    .await;
    report_gets(
        &w,
        "paused null",
        &with(|r| r["status"]["paused"] = Value::Null),
        400,
    )
    .await;
    report_gets(
        &w,
        "outOfMemory is any value",
        &with(|r| {
            r["jobs"] =
                json!([{"kind": "completed", "runner": "r", "at": "2026-10-10", "outOfMemory": "yes"}])
        }),
        200,
    )
    .await;
    report_gets(
        &w,
        "unknown fields are dropped",
        &with(|r| {
            r["new"] = json!({"a": 1});
            r["status"]["new"] = json!([1]);
        }),
        200,
    )
    .await;
}

#[tokio::test]
async fn numbers_and_times_are_bounded() {
    let w = world().await;
    let free = |v: Value| with(|r| r["status"]["host"]["freeMemoryPercent"] = v);
    report_gets(&w, "free memory 0", &free(json!(0)), 200).await;
    report_gets(&w, "free memory 100", &free(json!(100)), 200).await;
    report_gets(&w, "free memory 101", &free(json!(101)), 400).await;
    report_gets(&w, "free memory -1", &free(json!(-1)), 400).await;
    report_gets(&w, "free memory a string", &free(json!("60")), 400).await;
    let max_jobs = |v: Value| with(|r| r["status"]["maxJobs"] = v);
    report_gets(&w, "max jobs 1000000", &max_jobs(json!(1_000_000)), 200).await;
    report_gets(&w, "max jobs 1000001", &max_jobs(json!(1_000_001)), 400).await;
    report_gets(&w, "max jobs 1.5", &max_jobs(json!(1.5)), 400).await;
    report_gets(&w, "max jobs -1", &max_jobs(json!(-1)), 400).await;
    let pressure = |v: Value| with(|r| r["status"]["host"]["memoryPressure"] = v);
    report_gets(&w, "pressure 8", &pressure(json!(8)), 200).await;
    report_gets(&w, "pressure 9", &pressure(json!(9)), 400).await;
    let cpu = |v: Value| with(|r| r["activity"]["cpuPercent"] = v);
    report_gets(&w, "cpu -1", &cpu(json!(-1)), 200).await;
    report_gets(&w, "cpu 100", &cpu(json!(100)), 200).await;
    report_gets(&w, "cpu 100.5", &cpu(json!(100.5)), 400).await;
    let bytes = |v: Value| with(|r| r["status"]["host"]["diskFreeBytes"] = v);
    report_gets(&w, "bytes 2^53", &bytes(json!(9_007_199_254_740_992u64)), 200).await;
    report_gets(&w, "bytes 2^53 + 2", &bytes(json!(9_007_199_254_740_994u64)), 400).await;
    for (label, at, status) in [
        ("an ISO time", "2026-10-10T08:00:00Z", 200),
        (
            "with milliseconds and an offset",
            "2026-10-10T08:00:00.123+02:00",
            200,
        ),
        ("a date alone", "2026-10-10", 200),
        ("a day no calendar has", "2026-02-30T08:00:00Z", 400),
        ("not a time", "yesterday", 400),
        ("empty", "", 400),
        (
            "over 64 characters",
            &format!("2026-10-10T08:00:00.{}Z", "1".repeat(64)),
            400,
        ),
    ] {
        let body = with(|r| r["status"]["updatedAt"] = json!(at));
        report_gets(&w, &format!("updatedAt: {label}"), &body, status).await;
    }
    // A time the console reads as none (`optionalTime`) is not an error, but a missing required one is.
    report_gets(
        &w,
        "a bad boot time reads as none",
        &with(|r| r["status"]["host"]["bootTime"] = json!("whenever")),
        200,
    )
    .await;
    report_gets(
        &w,
        "a bad event time",
        &with(|r| r["jobs"] = json!([{"kind": "started", "runner": "r", "at": "later"}])),
        400,
    )
    .await;
    report_gets(
        &w,
        "a missing event time",
        &with(|r| r["jobs"] = json!([{"kind": "started", "runner": "r"}])),
        400,
    )
    .await;
}

#[tokio::test]
async fn the_system_is_macos_or_linux_and_anything_else_is_read_as_none_and_refused_never() {
    let w = world().await;
    let read_as = |os: &Value| -> Option<&'static str> {
        match os.as_str() {
            Some("macos") => Some("macos"),
            Some("linux") => Some("linux"),
            _ => None,
        }
    };
    let values = [
        json!("linux"),
        json!("macos"),
        json!("windows"),
        json!("Linux"),
        json!(""),
        json!(7),
        json!(["linux"]),
        Value::Null,
    ];
    // A report that names no system the console knows keeps the one it has.
    let mut kept = w.fake.os_of("mac-a");
    for (n, os) in values.iter().enumerate() {
        let code = format!("code-{n}");
        w.fake.add_code(&code);
        let mut hello = hello_json(&code, &format!("host-{n}"));
        hello["os"] = os.clone();
        let got = post_hello(&w.fake, &hello).await;
        assert_eq!(got.status, 200, "connect with os {os}: {}", got.error());
        assert_eq!(
            w.fake.os_of(&format!("host-{n}")).as_deref(),
            read_as(os),
            "connect with os {os}"
        );
        report_gets(&w, &format!("os {os}"), &with(|r| r["os"] = os.clone()), 200).await;
        if let Some(said) = read_as(os) {
            kept = Some(said.to_string());
        }
        assert_eq!(w.fake.os_of("mac-a"), kept, "report with os {os}");
    }
    assert_eq!(kept.as_deref(), Some("macos"), "the last system named");
    // Left out, as an older Runner does: the system said before stays.
    report_gets(&w, "no os", &report_json("mac-a"), 200).await;
    assert_eq!(w.fake.os_of("mac-a").as_deref(), Some("macos"));
}

#[tokio::test]
async fn the_machine_name_is_a_pattern_and_at_most_24_characters() {
    let fake = cf_fake_console::FakeConsole::start().await;
    for (n, (name, ok)) in [
        ("a", true),
        ("mac-a", true),
        ("0", true),
        ("a-b-c-d", true),
        (&"a".repeat(24)[..], true),
        (&"a".repeat(25)[..], false),
        ("", false),
        ("Mac-a", false),
        ("mac_a", false),
        ("-mac", false),
        ("mac-", false),
        ("mac--a", false),
        ("mac a", false),
        ("mäc", false),
        ("mac-a\n", false),
        ("../etc", false),
        ("air'; DROP TABLE runner_machines; --", false),
    ]
    .into_iter()
    .enumerate()
    {
        let w = world().await;
        report_gets(
            &w,
            &format!("report from {name:?}"),
            &with(|r| r["status"]["machine"] = json!(name)),
            if ok { 200 } else { 400 },
        )
        .await;
        let code = format!("code-{n}");
        fake.add_code(&code);
        let got = post_hello(&fake, &hello_json(&code, name)).await;
        assert_eq!(got.status == 200, ok, "enrol as {name:?}: {}", got.error());
        assert!(ok || got.status == 400, "{name:?}");
    }
}

#[tokio::test]
async fn enrolment_bounds_and_what_a_refused_request_spends() {
    let fake = cf_fake_console::FakeConsole::start().await;
    fake.add_code("good");
    let hello = |f: &dyn Fn(&mut Value)| {
        let mut body = hello_json("good", "mac-a");
        f(&mut body);
        body
    };
    let refused = [
        ("cpus 0", hello(&|b| b["cpus"] = json!(0))),
        ("cpus 4097", hello(&|b| b["cpus"] = json!(4097))),
        ("cpus 1.5", hello(&|b| b["cpus"] = json!(1.5))),
        (
            "cpus missing",
            hello(&|b| {
                b.as_object_mut().expect("o").remove("cpus");
            }),
        ),
        ("code empty", hello(&|b| b["code"] = json!(""))),
        ("code 201", hello(&|b| b["code"] = json!("c".repeat(201)))),
        ("code a number", hello(&|b| b["code"] = json!(7))),
        ("memory -1", hello(&|b| b["memoryBytes"] = json!(-1))),
        (
            "memory missing",
            hello(&|b| {
                b.as_object_mut().expect("o").remove("memoryBytes");
            }),
        ),
        (
            "hostname 1021",
            hello(&|b| b["hostname"] = json!("h".repeat(1021))),
        ),
        ("hostname null", hello(&|b| b["hostname"] = Value::Null)),
        ("model 801", hello(&|b| b["model"] = json!("m".repeat(801)))),
        ("version 257", hello(&|b| b["version"] = json!("v".repeat(257)))),
    ];
    for (label, body) in refused {
        let got = post_hello(&fake, &body).await;
        assert_eq!(got.status, 400, "{label}");
        assert!(
            got.error()
                .starts_with("Not a connect request this console reads: "),
            "{label}: {}",
            got.error()
        );
    }
    assert!(fake.enrollments().is_empty(), "a malformed request is not kept");
    // None of that spent the code; the extreme valid values still enrol.
    let best = hello(&|b| {
        b["cpus"] = json!(4096);
        b["hostname"] = json!("h".repeat(1020));
        b["model"] = json!("m".repeat(800));
        b["version"] = json!("v".repeat(256));
        b["memoryBytes"] = json!(9_007_199_254_740_992u64);
    });
    let got = post_hello(&fake, &best).await;
    assert_eq!(got.status, 200, "{}", got.error());
    assert_eq!(fake.enrollments().len(), 1);
    // A code of 200 characters, and one with spaces around it that the console trims.
    fake.add_code(&"c".repeat(200));
    assert_eq!(
        post_hello(&fake, &hello_json(&"c".repeat(200), "mac-b"))
            .await
            .status,
        200
    );
    fake.add_code("padded");
    assert_eq!(
        post_hello(&fake, &hello_json("  padded \n", "mac-c"))
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn a_code_is_spent_once_belongs_to_one_mac_and_a_new_code_replaces_the_token() {
    let fake = cf_fake_console::FakeConsole::start().await;
    fake.add_code_for("for-a", "mac-a");
    let wrong = post_hello(&fake, &hello_json("for-a", "mac-b")).await;
    assert_eq!(wrong.status, 403);
    assert!(
        wrong
            .error()
            .starts_with("This code is for mac-a, and this Mac is mac-b."),
        "{}",
        wrong.error()
    );
    let first = post_hello(&fake, &hello_json("for-a", "mac-a")).await;
    assert_eq!(first.status, 200, "the wrong Mac did not spend it");
    let again = post_hello(&fake, &hello_json("for-a", "mac-a")).await;
    assert_eq!(again.status, 401);
    assert!(
        again.error().starts_with("This code isn't valid"),
        "{}",
        again.error()
    );
    assert_eq!(
        post_hello(&fake, &hello_json("never-made", "mac-a")).await.status,
        401
    );
    let old = first.body["token"].as_str().expect("token").to_string();
    assert_eq!(first.body["name"], "mac-a");
    assert!(old.starts_with("cfr_") && old.len() == 47, "{old}");
    // The same Mac enrolling again with a new code keeps its row and retires the old token.
    fake.add_code("again");
    let second = post_hello(&fake, &hello_json("again", "mac-a")).await;
    let fresh = second.body["token"].as_str().expect("token").to_string();
    assert_ne!(old, fresh);
    let report = |token: String| {
        let url = fake.url();
        async move {
            raw(
                &url,
                "POST",
                "/agent/runners/report",
                Some(&format!("Bearer {token}")),
                Some("application/json"),
                serde_json::to_vec(&report_json("mac-a")).expect("json"),
            )
            .await
            .status
        }
    };
    assert_eq!(report(old).await, 401);
    assert_eq!(report(fresh).await, 200);
    assert_eq!(fake.tokens().len(), 2);
    assert_eq!(fake.machines(), vec!["mac-a".to_string()]);
}

#[tokio::test]
async fn answers_carry_pause_the_report_interval_and_commands_until_a_result_names_them() {
    let w = world().await;
    let first = post_json(&w, "report", &report_json("mac-a")).await;
    assert_eq!(
        first.body,
        json!({"paused": false, "reportEvery": 60, "commands": []})
    );
    w.fake.pause(true);
    w.fake.watch(true);
    let id = w.fake.queue_command("settings", Some(json!({"maxJobs": 1})));
    let other = w.fake.queue_command("restart", None);
    let answer = post_json(&w, "report", &report_json("mac-a")).await;
    assert_eq!(
        answer.body,
        json!({"paused": true, "reportEvery": 5, "commands": [
            {"id": id, "kind": "settings", "args": {"maxJobs": 1}},
            {"id": other, "kind": "restart"},
        ]})
    );
    // They come again until a report carries their result; a result for one does not clear the other.
    let again = post_json(&w, "report", &report_json("mac-a")).await;
    assert_eq!(again.body["commands"].as_array().map(Vec::len), Some(2));
    let done = with(|r| {
        r["results"] = json!([{"id": id, "ok": true, "message": "done", "at": "2026-10-10T08:00:00Z"}])
    });
    let after = post_json(&w, "report", &done).await;
    assert_eq!(after.body["commands"], json!([{"id": other, "kind": "restart"}]));
    w.fake.watch(false);
    let result = with(|r| {
        r["results"] = json!([{"id": other, "ok": false, "message": "no", "at": "2026-10-10T08:00:00Z"}])
    });
    let last = post_json(&w, "report", &result).await;
    assert_eq!(
        last.body,
        json!({"paused": true, "reportEvery": 60, "commands": []})
    );
}

#[tokio::test]
async fn leaving_forgets_the_mac_and_its_token() {
    let w = world().await;
    let left = post_json(&w, "leave", &json!({})).await;
    assert_eq!((left.status, left.body), (200, json!({"ok": true})));
    assert_eq!(w.fake.left(), vec!["mac-a".to_string()]);
    assert_eq!(post_json(&w, "report", &report_json("mac-a")).await.status, 401);
    assert_eq!(post_json(&w, "leave", &json!({})).await.status, 401);
}

#[tokio::test]
async fn a_report_naming_another_name_renames_the_mac_unless_it_is_taken() {
    let w = world().await;
    w.fake.add_code("code-b");
    let b = post_hello(&w.fake, &hello_json("code-b", "mac-b")).await;
    assert_eq!(b.status, 200);
    let renamed = post_json(&w, "report", &report_json("mac-a-2")).await;
    assert_eq!(renamed.status, 200);
    let mut names = w.fake.machines();
    names.sort();
    assert_eq!(names, ["mac-a-2", "mac-b"]);
    let taken = post_json(&w, "report", &report_json("mac-b")).await;
    assert_eq!(taken.status, 200);
    let mut names = w.fake.machines();
    names.sort();
    assert_eq!(
        names,
        ["mac-a-2", "mac-b"],
        "the name is taken, so nothing is renamed"
    );
}

#[tokio::test]
async fn faults_come_before_everything_and_a_taken_call_can_lose_its_answer() {
    let w = world().await;
    w.fake.down(true);
    let got = post_json(&w, "report", &report_json("mac-a")).await;
    assert_eq!((got.status, got.error().as_str()), (503, "The console is down"));
    w.fake.down(false);
    w.fake.fail_next(2, 500);
    assert_eq!(post_json(&w, "report", &report_json("mac-a")).await.status, 500);
    assert_eq!(post_json(&w, "report", &report_json("mac-a")).await.status, 500);
    assert_eq!(post_json(&w, "report", &report_json("mac-a")).await.status, 200);
    assert_eq!(w.fake.reports().len(), 1, "a failed call is not kept");
    // The answer of a taken call never arrives, and the call is kept.
    w.fake.accept_then_drop_next(1);
    let url = format!("{}/agent/runners/report", w.fake.url());
    let sent = reqwest::Client::new()
        .post(url)
        .header("authorization", format!("Bearer {}", w.token))
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&report_json("mac-a")).expect("json"))
        .send()
        .await;
    assert!(sent.is_err(), "no answer");
    assert_eq!(w.fake.reports().len(), 2, "the report was taken");
    // A refused call is answered even then, and does not use the fault up.
    w.fake.accept_then_drop_next(1);
    assert_eq!(
        post_json(&w, "report", &with(|r| r["protocol"] = json!(2)))
            .await
            .status,
        400
    );
    assert_eq!(w.fake.calls(), 7);
}
