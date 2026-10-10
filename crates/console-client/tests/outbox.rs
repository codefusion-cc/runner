//! The outbox on its own: what it keeps, what it refuses, what a restart and a damaged file do to it.

mod support;

use std::os::unix::fs::PermissionsExt as _;

use cf_console_client::{JobEvent, OUTBOX_CAPACITY, Outbox, SENT_MEMORY};
use support::{event, ids};

fn open(dir: &tempfile::TempDir) -> Outbox {
    Outbox::open(dir.path().join("outbox.json"), OUTBOX_CAPACITY)
}

#[test]
fn events_wait_oldest_first_and_stay_until_they_are_marked_sent() {
    let dir = tempfile::tempdir().expect("dir");
    let outbox = open(&dir);
    assert!(outbox.is_empty());
    for n in 0..5 {
        assert!(outbox.push(event(n)));
    }
    assert_eq!(outbox.len(), 5);
    assert_eq!(ids(&outbox.batch(3)), ["e0000", "e0001", "e0002"]);
    assert_eq!(
        ids(&outbox.batch(3)),
        ["e0000", "e0001", "e0002"],
        "looking removes nothing"
    );
    assert_eq!(outbox.batch(0), vec![]);
    assert_eq!(outbox.batch(99).len(), 5);
    outbox.sent(["e0001", "e0000"]);
    assert_eq!(ids(&outbox.batch(99)), ["e0002", "e0003", "e0004"]);
    outbox.sent(Vec::<String>::new());
    assert_eq!(outbox.len(), 3);
    assert_eq!(outbox.dropped(), 0);
}

#[test]
fn an_id_that_waits_or_was_sent_recently_is_not_queued_again() {
    let dir = tempfile::tempdir().expect("dir");
    let outbox = open(&dir);
    assert!(outbox.push(event(1)));
    assert!(!outbox.push(event(1)), "it waits already");
    let same_id_other_content = JobEvent {
        runner: "mac-a-other".into(),
        ..event(1)
    };
    assert!(
        !outbox.push(same_id_other_content),
        "the id names the event, whatever else it says"
    );
    assert_eq!(outbox.len(), 1);
    outbox.sent(["e0001"]);
    assert!(!outbox.push(event(1)), "the console has it");
    assert!(outbox.is_empty());
    outbox.sent(["never-queued"]);
    assert!(
        !outbox.push(JobEvent {
            id: "never-queued".into(),
            ..event(2)
        }),
        "sent covers an event that was dropped meanwhile"
    );
}

#[test]
fn the_last_1000_sent_ids_are_remembered_and_not_one_more() {
    let dir = tempfile::tempdir().expect("dir");
    let outbox = open(&dir);
    let all: Vec<String> = (0..=SENT_MEMORY).map(|n| format!("old{n}")).collect();
    for chunk in all.chunks(100) {
        outbox.sent(chunk);
    }
    let again = |n: usize| {
        outbox.push(JobEvent {
            id: format!("old{n}"),
            ..event(n)
        })
    };
    assert!(again(0), "1001 ids went by: the first is forgotten");
    assert!(!again(1), "the 1000th from the end is still known");
    assert!(!again(SENT_MEMORY));
    assert_eq!(SENT_MEMORY, 1000);
}

#[test]
fn over_the_capacity_the_oldest_is_dropped_and_counted() {
    let dir = tempfile::tempdir().expect("dir");
    let outbox = open(&dir);
    for n in 0..700 {
        assert!(outbox.push(event(n)), "{n}");
    }
    assert_eq!(outbox.len(), 500);
    assert_eq!(outbox.dropped(), 200);
    let waiting = outbox.batch(usize::MAX);
    assert_eq!(waiting.first().map(|e| e.id.as_str()), Some("e0200"));
    assert_eq!(waiting.last().map(|e| e.id.as_str()), Some("e0699"));
    // A dropped event is gone, not sent: it may come again.
    assert!(outbox.push(event(0)));
    assert_eq!((outbox.len(), outbox.dropped()), (500, 201));
    // A capacity of 0 is 1.
    let one = Outbox::open(dir.path().join("one.json"), 0);
    assert!(one.push(event(1)) && one.push(event(2)));
    assert_eq!(
        (ids(&one.batch(9)), one.dropped()),
        (vec!["e0002".to_string()], 1)
    );
}

#[test]
fn a_restart_keeps_what_waits_what_was_sent_and_what_was_dropped() {
    let dir = tempfile::tempdir().expect("dir");
    {
        let outbox = Outbox::open(dir.path().join("outbox.json"), 3);
        for n in 0..6 {
            outbox.push(event(n));
        }
        outbox.sent(["e0004"]);
    }
    let outbox = Outbox::open(dir.path().join("outbox.json"), 3);
    assert_eq!(ids(&outbox.batch(9)), ["e0003", "e0005"]);
    assert_eq!(outbox.dropped(), 3);
    assert!(!outbox.push(event(4)), "sent before the restart");
    assert!(!outbox.push(event(3)), "waiting before the restart");
    assert!(outbox.push(event(9)));
    // The event comes back whole.
    assert_eq!(outbox.batch(1)[0], event(3));
    // A smaller capacity at the next start drops the oldest and counts them.
    let smaller = Outbox::open(dir.path().join("outbox.json"), 2);
    assert_eq!(
        (ids(&smaller.batch(9)), smaller.dropped()),
        (vec!["e0005".to_string(), "e0009".to_string()], 4)
    );
}

#[test]
fn the_file_is_this_users_alone_and_so_is_its_folder() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("state").join("outbox.json");
    let outbox = Outbox::open(&path, 10);
    outbox.push(event(1));
    let mode = |p: &std::path::Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(path.parent().expect("parent")), 0o700);
    outbox.sent(["e0001"]);
    assert_eq!(mode(&path), 0o600, "after a replacement too");
    let files: Vec<_> = std::fs::read_dir(path.parent().expect("parent"))
        .expect("list")
        .collect();
    assert_eq!(files.len(), 1, "no partial file is left");
}

#[test]
fn a_damaged_file_is_set_aside_and_the_outbox_starts_empty_and_works() {
    for (label, content) in [
        ("garbage", b"\x00\xffnot json".to_vec()),
        (
            "half a file",
            br#"{"waiting":[{"id":"e1","kind":"started","runner""#.to_vec(),
        ),
        ("another JSON", b"[1,2,3]".to_vec()),
        (
            "an event of an unknown kind",
            br#"{"waiting":[{"id":"e1","kind":"queued","runner":"r","at":"x"}]}"#.to_vec(),
        ),
        ("empty", Vec::new()),
    ] {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("outbox.json");
        std::fs::write(&path, &content).expect("seed");
        let outbox = Outbox::open(&path, 10);
        assert_eq!((outbox.len(), outbox.dropped()), (0, 0), "{label}");
        assert_eq!(
            std::fs::read(path.with_extension("corrupt")).expect("set aside"),
            content,
            "{label}"
        );
        assert!(
            !path.exists(),
            "{label}: the damaged file is moved away, not copied, so it is not judged again"
        );
        assert!(outbox.push(event(1)), "{label}");
        let reopened = Outbox::open(&path, 10);
        assert_eq!(
            ids(&reopened.batch(9)),
            ["e0001"],
            "{label}: the new file is whole"
        );
    }
}

#[test]
fn a_path_that_cannot_be_read_as_a_file_does_not_stop_the_outbox() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("outbox.json");
    std::fs::create_dir(&path).expect("a folder where the file should be");
    let outbox = Outbox::open(&path, 10);
    assert!(outbox.push(event(1)), "memory still works");
    assert_eq!(outbox.len(), 1);
}

#[test]
fn a_file_with_duplicates_empty_ids_and_ids_already_sent_is_cleaned() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("outbox.json");
    let mut empty_id = serde_json::to_value(event(7)).expect("json");
    empty_id["id"] = serde_json::json!("");
    let file = serde_json::json!({
        "waiting": [event(1), event(2), event(1), event(3), empty_id, serde_json::to_value(event(5)).expect("json")],
        "sent": ["e0003", "e0003"],
        "dropped": 7,
    });
    std::fs::write(&path, serde_json::to_vec(&file).expect("json")).expect("seed");
    let outbox = Outbox::open(&path, 10);
    assert_eq!(ids(&outbox.batch(9)), ["e0001", "e0002", "e0005"]);
    assert_eq!(outbox.dropped(), 7);
    assert!(!outbox.push(event(3)));
}

#[test]
fn what_the_console_would_refuse_is_never_queued_and_what_it_cuts_is_cut() {
    let dir = tempfile::tempdir().expect("dir");
    let outbox = open(&dir);
    let with = |f: &dyn Fn(&mut JobEvent)| {
        let mut e = event(1);
        f(&mut e);
        e
    };
    for (label, e) in [
        ("no id", with(&|e| e.id = String::new())),
        ("an id of 129", with(&|e| e.id = "i".repeat(129))),
        ("no Actions runner", with(&|e| e.runner = String::new())),
        ("an Actions runner of 101", with(&|e| e.runner = "r".repeat(101))),
        (
            "an Actions runner of 51 rockets",
            with(&|e| e.runner = "🚀".repeat(51)),
        ),
    ] {
        assert!(!outbox.push(e), "{label}");
    }
    assert!(outbox.is_empty());
    assert!(outbox.push(with(&|e| {
        e.id = "i".repeat(128);
        e.runner = "r".repeat(100);
    })));
    assert!(outbox.push(with(&|e| {
        e.id = "other".into();
        e.runner = "🚀".repeat(50);
        e.repo = Some("r".repeat(5000));
        e.job = Some("🚀".repeat(5000));
        e.workflow = Some("w".repeat(5000));
        e.event = Some("e".repeat(5000));
        e.result = Some("x".repeat(5000));
        e.end = Some("e".repeat(5000));
    })));
    let got = &outbox.batch(9)[1];
    let chars = |s: &Option<String>| s.as_deref().map(|s| s.chars().count());
    assert_eq!(
        [
            chars(&got.repo),
            chars(&got.job),
            chars(&got.workflow),
            chars(&got.event),
            chars(&got.result),
            chars(&got.end)
        ],
        [Some(200), Some(300), Some(400), Some(50), Some(30), Some(50)]
    );
    assert_eq!(got.runner, "🚀".repeat(50));
}

#[test]
fn a_file_that_cannot_be_written_costs_the_file_and_not_the_events() {
    let dir = tempfile::tempdir().expect("dir");
    let folder = dir.path().join("state");
    let path = folder.join("outbox.json");
    let outbox = Outbox::open(&path, 10);
    outbox.push(event(1));
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o500)).expect("lock the folder");
    if std::fs::write(folder.join("probe"), b"").is_ok() {
        // This user can write anywhere (root): nothing to show here.
        return;
    }
    assert!(outbox.push(event(2)), "the push works in memory");
    assert_eq!(ids(&outbox.batch(9)), ["e0001", "e0002"]);
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).expect("unlock");
    outbox.sent(["e0001"]);
    let reopened = Outbox::open(&path, 10);
    assert_eq!(
        ids(&reopened.batch(9)),
        ["e0002"],
        "the next change wrote the whole state"
    );
}
