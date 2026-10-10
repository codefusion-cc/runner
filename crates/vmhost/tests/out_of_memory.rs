//! Whether a job ran out of memory has three answers; what is not known is never told as "no".

use cf_vmhost::{JobState, Layout, OutOfMemory, out_of_memory};
use serde_json::{Value, json};

fn kill() -> Value {
    json!({ "process": { "pid": 41, "name": "node", "memory_bytes": 700 << 20 }, "limit_bytes": 768 << 20, "largest": [] })
}

fn summary(end: Value, exit_code: Option<i32>, oom: Option<Value>) -> Value {
    json!({
        "end": end, "exit_code": exit_code, "signal": null, "disk_peak_bytes": 0, "oom": oom, "wall_ms": 9,
        "usage": { "memory_bytes": 1, "memory_peak_bytes": 2, "cpu_usec": 3, "disk_bytes": 4, "pids": 5 },
    })
}

/// A job's state as its file holds it: with a reported kill or none, and an end or none.
fn state(stage: &str, oom: Option<Value>, end: Option<Value>) -> JobState {
    let mut state = json!({
        "job": "job-1", "pid": 7, "started_at": 1, "stage": stage, "vm_bytes": 1 << 30, "limit_bytes": 768 << 20,
        "cpus": 2, "image": "image-1", "boots": 1,
    });
    if let Some(oom) = oom {
        state["oom"] = oom;
    }
    if let Some(end) = end {
        state["end"] = end;
    }
    serde_json::from_value(state).unwrap()
}

fn ended(end: &str, message: Option<&str>, summary: Option<Value>) -> Option<Value> {
    let mut ended = json!({ "end": end, "at": 2 });
    if let Some(message) = message {
        ended["message"] = json!(message);
    }
    if let Some(summary) = summary {
        ended["summary"] = summary;
    }
    Some(ended)
}

fn reason(answer: OutOfMemory) -> String {
    match answer {
        OutOfMemory::Unknown { reason } => reason,
        other => panic!("{other:?} is not unknown"),
    }
}

fn exited() -> Value {
    json!({ "kind": "exited" })
}

#[test]
fn a_reported_kill_is_yes_whatever_came_after() {
    let name = |answer: OutOfMemory| match answer {
        OutOfMemory::Yes(event) => (event.process.name, event.limit_bytes),
        other => panic!("{other:?}"),
    };
    let node = ("node".to_string(), 768 << 20);
    // Reported and the job ended as out of memory.
    let whole = state(
        "ended",
        Some(kill()),
        ended("oom", None, Some(summary(exited(), Some(137), Some(kill())))),
    );
    assert_eq!(name(whole.out_of_memory()), node);
    // Reported, then the guest was lost before it could say more.
    assert_eq!(
        name(
            state(
                "ended",
                Some(kill()),
                ended("guest-lost", Some("no heartbeat"), None)
            )
            .out_of_memory()
        ),
        node
    );
    // Reported, and the job is still in its grace time.
    assert_eq!(name(state("running", Some(kill()), None).out_of_memory()), node);
    // Only the guest's last account names it (the report on the way was missed).
    let late = state(
        "ended",
        None,
        ended("oom", None, Some(summary(exited(), Some(137), Some(kill())))),
    );
    assert_eq!(name(late.out_of_memory()), node);
    // A job stopped at its deadline that had run out of memory before is still a yes.
    let stopped = json!({ "kind": "stopped", "reason": "timeout" });
    assert_eq!(
        name(
            state(
                "ended",
                Some(kill()),
                ended("timeout", None, Some(summary(stopped, None, None)))
            )
            .out_of_memory()
        ),
        node
    );
}

#[test]
fn no_is_said_only_of_an_end_the_guest_described_or_a_workload_that_never_started() {
    for (end, account) in [
        ("ok", summary(exited(), Some(0), None)),
        ("failed", summary(exited(), Some(1), None)),
        // 137 without a reported kill is a process that was killed by something else.
        ("failed", summary(exited(), Some(137), None)),
        (
            "timeout",
            summary(json!({ "kind": "stopped", "reason": "timeout" }), None, None),
        ),
        (
            "evicted-memory",
            summary(
                json!({ "kind": "stopped", "reason": "evicted-memory" }),
                None,
                None,
            ),
        ),
    ] {
        assert_eq!(
            state("ended", None, ended(end, None, Some(account))).out_of_memory(),
            OutOfMemory::No,
            "{end}"
        );
    }
    assert_eq!(
        state("ended", None, ended("boot-failed", Some("no answer"), None)).out_of_memory(),
        OutOfMemory::No
    );
}

#[test]
fn a_guest_that_was_not_heard_to_the_end_is_unknown_and_says_why() {
    let lost = reason(
        state(
            "ended",
            None,
            ended("guest-lost", Some("no heartbeat for 60 s"), None),
        )
        .out_of_memory(),
    );
    assert!(
        lost.contains("guest-lost") && lost.contains("no heartbeat for 60 s"),
        "{lost}"
    );
    // Every end that can come without the guest's account: none of them is a "no".
    for end in [
        "guest-lost",
        "timeout",
        "evicted-memory",
        "evicted-disk",
        "host-slept",
        "cancelled",
        "failed",
        "ok",
        "oom",
    ] {
        let why = reason(state("ended", None, ended(end, None, None)).out_of_memory());
        assert!(why.contains(end), "{why}");
    }
    for stage in ["booting", "running", "stopping", "done", "killing"] {
        let why = reason(state(stage, None, None).out_of_memory());
        assert!(why.contains("no end on record"), "{why}");
    }
}

#[test]
fn a_state_file_that_cannot_be_trusted_is_unknown_never_no() {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::new(dir.path());
    let missing = reason(out_of_memory(&layout, "job-1"));
    assert!(
        missing.contains("could not be read") && missing.contains("job-1.json"),
        "{missing}"
    );

    std::fs::create_dir_all(layout.run()).unwrap();
    let good = serde_json::to_string(&state(
        "ended",
        None,
        ended("ok", None, Some(summary(exited(), Some(0), None))),
    ))
    .unwrap();
    // Cut short at every length, as a crash in the middle of a write would leave it; empty; not JSON; not a state.
    let mut broken: Vec<String> = (0..good.len())
        .step_by(7)
        .map(|length| good[..length].to_string())
        .collect();
    broken.extend(
        [
            "null",
            "{}",
            "[]",
            "not json",
            "{\"job\":\"job-1\"}",
            "\u{0}\u{0}",
        ]
        .map(str::to_string),
    );
    for text in broken {
        std::fs::write(layout.state("job-1"), &text).unwrap();
        let why = reason(out_of_memory(&layout, "job-1"));
        assert!(why.contains("is not a job's state"), "{text:?}: {why}");
    }
    std::fs::write(layout.state("job-1"), [0xff, 0xfe, 0x00]).unwrap();
    assert!(reason(out_of_memory(&layout, "job-1")).contains("could not be read"));

    // Whole again, it answers; under another job's name it does not pass for that job's.
    std::fs::write(layout.state("job-1"), &good).unwrap();
    assert_eq!(out_of_memory(&layout, "job-1"), OutOfMemory::No);
    std::fs::write(layout.state("job-2"), &good).unwrap();
    assert!(reason(out_of_memory(&layout, "job-2")).contains("another job"));
    let with_kill = serde_json::to_string(&state("ended", Some(kill()), ended("oom", None, None))).unwrap();
    std::fs::write(layout.state("job-1"), with_kill).unwrap();
    assert!(matches!(out_of_memory(&layout, "job-1"), OutOfMemory::Yes(event) if event.process.pid == 41));
}

#[test]
fn the_answer_travels_as_json_with_its_reason() {
    let unknown = OutOfMemory::Unknown {
        reason: "the guest was lost".to_string(),
    };
    assert_eq!(
        serde_json::to_value(&unknown).unwrap(),
        json!({ "answer": "unknown", "reason": "the guest was lost" })
    );
    assert_eq!(
        serde_json::to_value(OutOfMemory::No).unwrap(),
        json!({ "answer": "no" })
    );
    let yes = state("running", Some(kill()), None).out_of_memory();
    let sent = serde_json::to_value(&yes).unwrap();
    assert_eq!(
        (sent["answer"].as_str(), sent["process"]["name"].as_str()),
        (Some("yes"), Some("node"))
    );
    assert_eq!(serde_json::from_value::<OutOfMemory>(sent).unwrap(), yes);
}
