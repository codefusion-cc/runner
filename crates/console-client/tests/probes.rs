//! The probe fields of protocol 1 as the wire has them: an answer of a console from before them reads as no probes,
//! and a report with no results says nothing of them, so neither side of an older pair sees a change.

use cf_console_client::{Answer, ProbeResult, ProbeTarget, Report};
use serde_json::json;

mod support;

#[test]
fn an_answer_without_probes_asks_for_none() {
    let old: Answer =
        serde_json::from_value(json!({ "paused": false, "reportEvery": 60, "commands": [] })).expect("read");
    assert_eq!((old.probes, old.probe_every_seconds), (vec![], None));
}

#[test]
fn an_answer_with_probes_names_them_as_the_console_sends_them() {
    let answer: Answer = serde_json::from_value(json!({
        "paused": false, "reportEvery": 60, "commands": [],
        "probes": [{ "name": "heimforge/na", "host": "na.example.net", "port": 2457, "kind": "a2s" }],
        "probeEverySeconds": 120,
        "somethingNewer": true,
    }))
    .expect("read");
    assert_eq!(
        answer.probes,
        vec![ProbeTarget {
            name: "heimforge/na".into(),
            host: "na.example.net".into(),
            port: 2457,
            kind: "a2s".into()
        }]
    );
    assert_eq!(answer.probe_every_seconds, Some(120));
}

#[test]
fn a_report_without_results_leaves_the_field_out_and_one_with_them_names_them_in_camel_case() {
    let plain = serde_json::to_value(support::report("mac-a")).expect("write");
    assert!(plain.get("probeResults").is_none(), "{plain}");
    let report = Report {
        probe_results: vec![ProbeResult {
            target: "heimforge/na".into(),
            ok: true,
            at: "2026-11-22T12:00:00Z".into(),
            latency_ms: Some(41),
            world_name: Some("Heimforge NA".into()),
            players: Some(3),
            max: Some(10),
            error: None,
        }],
        ..support::report("mac-a")
    };
    let wire = serde_json::to_value(report).expect("write");
    assert_eq!(
        wire["probeResults"],
        json!([{ "target": "heimforge/na", "ok": true, "at": "2026-11-22T12:00:00Z", "latencyMs": 41, "worldName": "Heimforge NA", "players": 3, "max": 10 }])
    );
}

#[test]
fn a_probe_entry_or_cadence_this_does_not_read_never_makes_the_answer_unread() {
    let answer: Answer = serde_json::from_value(json!({
        "paused": true, "reportEvery": 5, "commands": [{ "id": "c1", "kind": "restart" }],
        "probes": [
            { "name": "no-kind", "host": "na.example.net", "port": 2457 },
            { "name": "big-port", "host": "na.example.net", "port": 70000, "kind": "a2s" },
            { "name": "text-port", "host": "na.example.net", "port": "2457", "kind": "a2s" },
            "not an object",
            { "name": "good", "host": "na.example.net", "port": 2457, "kind": "a2s" },
        ],
        "probeEverySeconds": -1,
    }))
    .expect("read");
    assert!(answer.paused);
    assert_eq!(answer.commands.len(), 1);
    assert_eq!(
        answer.probes.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        vec!["good"]
    );
    assert_eq!(answer.probe_every_seconds, None);

    for (probes, every) in [
        (json!(null), json!(null)),
        (json!({}), json!(1.5)),
        (json!("x"), json!("120")),
    ] {
        let answer: Answer = serde_json::from_value(json!({
            "paused": false, "reportEvery": 60, "commands": [],
            "probes": probes, "probeEverySeconds": every,
        }))
        .expect("read");
        assert_eq!((answer.probes, answer.probe_every_seconds), (vec![], None));
    }
}
