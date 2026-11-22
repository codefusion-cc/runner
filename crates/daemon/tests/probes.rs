//! `Probes`, what a host keeps of the console's game servers between reports: when a round is due, what waits for
//! the console, and what an answer without probes ends.

use std::time::{Duration, Instant};

use cf_console_client::{Answer, MAX_PROBE_RESULTS, MAX_PROBE_TARGETS, ProbeResult, ProbeTarget};
use cf_daemon::probes::{Probes, targets_of};

fn target(name: &str) -> ProbeTarget {
    ProbeTarget {
        name: name.into(),
        host: "na.example.net".into(),
        port: 2457,
        kind: "a2s".into(),
    }
}

fn answer(targets: Vec<ProbeTarget>, every: Option<u64>) -> Answer {
    Answer {
        paused: false,
        report_every: 60,
        commands: vec![],
        make_room_for: None,
        probes: targets,
        probe_every_seconds: every,
    }
}

fn result(target: &str, at: &str) -> ProbeResult {
    ProbeResult {
        target: target.into(),
        ok: true,
        at: at.into(),
        latency_ms: Some(12),
        world_name: Some("Heimforge NA".into()),
        players: Some(0),
        max: Some(10),
        error: None,
    }
}

#[test]
fn a_round_is_due_first_and_then_every_interval_never_two_at_once() {
    let probes = Probes::default();
    let t0 = Instant::now();
    let on = answer(vec![target("a")], Some(120));
    assert_eq!(probes.answered(&on, t0), Some(vec![target("a")]));
    // One runs: none starts beside it, however late.
    assert_eq!(probes.answered(&on, t0 + Duration::from_secs(500)), None);
    probes.finished(vec![]);
    assert_eq!(probes.answered(&on, t0 + Duration::from_secs(119)), None);
    assert_eq!(
        probes.answered(&on, t0 + Duration::from_secs(120)),
        Some(vec![target("a")])
    );
}

#[test]
fn the_interval_is_held_to_its_bounds() {
    let t0 = Instant::now();
    for (asked, every) in [(Some(0), 30), (Some(1), 30), (None, 120), (Some(1_000_000), 3600)] {
        let probes = Probes::default();
        let on = answer(vec![target("a")], asked);
        assert!(probes.answered(&on, t0).is_some());
        probes.finished(vec![]);
        assert_eq!(
            probes.answered(&on, t0 + Duration::from_secs(every - 1)),
            None,
            "{asked:?}"
        );
        assert!(
            probes.answered(&on, t0 + Duration::from_secs(every)).is_some(),
            "{asked:?}"
        );
    }
}

#[test]
fn results_wait_until_sent_and_a_newer_one_stays() {
    let probes = Probes::default();
    let on = answer(vec![target("a"), target("b")], None);
    probes.answered(&on, Instant::now());
    probes.finished(vec![result("a", "1"), result("b", "1"), result("stranger", "1")]);
    let sent = probes.pending();
    assert_eq!(
        sent,
        vec![result("a", "1"), result("b", "1")],
        "only targets it was asked about"
    );
    // A newer look at a ends while the report is on its way: it is the one that stays.
    probes.answered(&on, Instant::now() + Duration::from_secs(200));
    probes.finished(vec![result("a", "2")]);
    probes.sent(&sent);
    assert_eq!(probes.pending(), vec![result("a", "2")]);
}

#[test]
fn an_answer_without_probes_drops_what_waits() {
    let probes = Probes::default();
    probes.answered(&answer(vec![target("a")], None), Instant::now());
    probes.finished(vec![result("a", "1")]);
    assert_eq!(probes.answered(&answer(vec![], None), Instant::now()), None);
    assert_eq!(probes.pending(), vec![]);
    // A round that ends after it was turned off keeps nothing either.
    let probes = Probes::default();
    probes.answered(&answer(vec![target("a")], None), Instant::now());
    probes.answered(&answer(vec![], None), Instant::now());
    probes.finished(vec![result("a", "1")]);
    assert_eq!(probes.pending(), vec![]);
}

#[test]
fn at_most_the_newest_results_wait() {
    let probes = Probes::default();
    let names: Vec<String> = (0..MAX_PROBE_RESULTS + 4).map(|i| format!("t{i}")).collect();
    // More targets than a host looks at are cut by targets_of; a result per target is all that waits.
    let on = answer(names.iter().map(|n| target(n)).collect(), None);
    assert_eq!(targets_of(&on).len(), MAX_PROBE_TARGETS);
    probes.answered(&on, Instant::now());
    probes.finished(names.iter().map(|n| result(n, "1")).collect());
    assert!(probes.pending().len() <= MAX_PROBE_RESULTS);
    assert!(
        probes
            .pending()
            .iter()
            .all(|r| names[..MAX_PROBE_TARGETS].contains(&r.target))
    );
}

#[test]
fn only_targets_a_host_may_ask_are_kept() {
    let bad = [
        ProbeTarget {
            kind: "a2s-player".into(),
            ..target("kind")
        },
        ProbeTarget {
            kind: "A2S".into(),
            ..target("case")
        },
        ProbeTarget {
            port: 0,
            ..target("port")
        },
        ProbeTarget {
            host: "na.example.net; rm -rf /".into(),
            ..target("host")
        },
        ProbeTarget {
            host: String::new(),
            ..target("nohost")
        },
        ProbeTarget {
            host: "x".repeat(254),
            ..target("longhost")
        },
        target(""),
        target("two words"),
        target("bell\u{7}"),
        target(&"n".repeat(65)),
    ];
    let mut all = bad.to_vec();
    all.push(target(&"n".repeat(64)));
    all.push(target("ok"));
    all.push(ProbeTarget {
        port: 9,
        ..target("ok")
    });
    let kept = targets_of(&answer(all, None));
    assert_eq!(
        kept,
        vec![target(&"n".repeat(64)), target("ok")],
        "the first of a name, and nothing malformed"
    );
}
