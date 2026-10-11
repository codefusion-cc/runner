//! Random interleavings of pushes, deliveries that work, deliveries that fail, and restarts, against the fake console,
//! with the rule written from the outbox's promise: every event pushed is delivered, waiting or counted as dropped, and
//! one the console has acknowledged is never sent again. Then two threads pushing while a third delivers.

mod support;

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use cf_console_client::{Outbox, REPORT_BATCH};
use proptest::prelude::*;
use support::*;

#[derive(Debug, Clone)]
enum Op {
    Push(usize),
    Deliver,
    /// The console is down, or a call is cut before it reaches the console.
    DeliverRefused,
    /// The console takes the report and the answer is lost.
    DeliverLost,
    /// The console says 401.
    DeliverUnauthorized,
    Reopen,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0usize..14).prop_map(Op::Push),
        3 => Just(Op::Deliver),
        1 => Just(Op::DeliverRefused),
        1 => Just(Op::DeliverLost),
        1 => Just(Op::DeliverUnauthorized),
        1 => Just(Op::Reopen),
    ]
}

const CAPACITY: usize = 6;

fn name(n: usize) -> String {
    format!("e{n:04}")
}

async fn run(ops: Vec<Op>) {
    let w = world().await;
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("outbox.json");
    let mut outbox = Outbox::open(&path, CAPACITY);

    // The model, written from the promise and not from the code.
    let mut waiting: VecDeque<String> = VecDeque::new();
    let mut taken: HashSet<String> = HashSet::new(); // acknowledged by the console, and so remembered
    let mut dropped = 0u64;
    let mut accepted = 0usize;
    let mut acked = 0usize;
    let mut seen = 0usize; // how many events the fake had when the last delivery began

    for op in ops {
        match op {
            Op::Push(n) => {
                let id = name(n);
                let fresh = !waiting.contains(&id) && !taken.contains(&id);
                assert_eq!(outbox.push(event(n)), fresh, "push {id}");
                if fresh {
                    accepted += 1;
                    if waiting.len() == CAPACITY {
                        waiting.pop_front();
                        dropped += 1;
                    }
                    waiting.push_back(id);
                }
            }
            Op::Reopen => outbox = Outbox::open(&path, CAPACITY),
            deliver => {
                match deliver {
                    Op::DeliverRefused => w.fake.fail_next(1, 503),
                    Op::DeliverLost => w.fake.accept_then_drop_next(1),
                    Op::DeliverUnauthorized => w.fake.fail_next(1, 401),
                    _ => {}
                }
                let batch: Vec<String> = waiting.iter().take(REPORT_BATCH).cloned().collect();
                let result = w.client.deliver(&outbox, report("mac-a")).await;
                let arrived: Vec<String> = w.fake.job_event_ids().split_off(seen);
                match deliver {
                    Op::Deliver => {
                        result.expect("a working console answers");
                        assert_eq!(arrived, batch, "oldest first");
                        for id in &batch {
                            taken.insert(id.clone());
                        }
                        acked += batch.len();
                        waiting.drain(..batch.len());
                    }
                    Op::DeliverLost => {
                        assert!(result.is_err());
                        assert_eq!(arrived, batch, "the console took it");
                    }
                    _ => {
                        assert!(result.is_err());
                        assert!(arrived.is_empty(), "the console did not take it");
                    }
                }
                // Whatever the console was sent, it was not something it had acknowledged before.
                for id in &arrived {
                    assert!(
                        !taken.contains(id) || batch.contains(id),
                        "{id} was sent after it was acknowledged"
                    );
                }
                seen = w.fake.job_events().len();
            }
        }
        let held: Vec<String> = outbox.batch(usize::MAX).into_iter().map(|e| e.id).collect();
        assert_eq!(held, Vec::from(waiting.clone()), "what waits");
        assert_eq!(outbox.dropped(), dropped);
    }

    // Everything accepted is acknowledged, waiting or dropped.
    assert_eq!(accepted, acked + waiting.len() + dropped as usize);
    // Draining what is left delivers all of it once more, nothing already acknowledged.
    let before = w.fake.job_event_ids().len();
    // The capacity is 6 and a batch takes them all, so a few rounds always drain it; a console
    // that never gets an event marked sent must fail here and not loop for ever.
    for _ in 0..10 {
        if outbox.is_empty() {
            break;
        }
        w.client.deliver(&outbox, report("mac-a")).await.expect("drained");
    }
    assert!(outbox.is_empty(), "the outbox drains");
    let rest: Vec<String> = w.fake.job_event_ids().split_off(before);
    assert_eq!(rest, Vec::from(waiting));
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 40, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn nothing_is_lost_nothing_acknowledged_is_sent_again_and_the_count_adds_up(ops in proptest::collection::vec(op(), 1..45)) {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(run(ops));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_threads_pushing_while_one_delivers_lose_and_double_nothing() {
    const THREADS: usize = 3;
    const EACH: usize = 250;
    let w = world().await;
    let dir = tempfile::tempdir().expect("dir");
    let outbox = Arc::new(Outbox::open(dir.path().join("outbox.json"), 2000));
    let pushers: Vec<_> = (0..THREADS)
        .map(|t| {
            let outbox = outbox.clone();
            std::thread::spawn(move || {
                let mut accepted = 0;
                for i in 0..EACH {
                    // Ids 0..50 are pushed by every thread: only one push of each is the first.
                    let n = if i % 5 == 0 { i / 5 } else { 1000 * (t + 1) + i };
                    if outbox.push(event(n)) {
                        accepted += 1;
                    }
                    if i % 17 == 0 {
                        std::thread::yield_now();
                    }
                }
                accepted
            })
        })
        .collect();
    let mut rounds = 0;
    while pushers.iter().any(|p| !p.is_finished()) || !outbox.is_empty() {
        rounds += 1;
        assert!(rounds < 3_000, "the outbox drains");
        w.client
            .deliver(&outbox, report("mac-a"))
            .await
            .expect("delivered");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let accepted: usize = pushers.into_iter().map(|p| p.join().expect("thread")).sum();
    let ids = w.fake.job_event_ids();
    let distinct: HashSet<&String> = ids.iter().collect();
    assert_eq!(ids.len(), distinct.len(), "nothing arrived twice");
    assert_eq!(ids.len(), accepted, "everything accepted arrived");
    assert_eq!(
        accepted,
        50 + THREADS * (EACH - 50),
        "the shared ids were taken once"
    );
    assert_eq!(outbox.dropped(), 0);
    // Each thread's own events arrived in the order it pushed them.
    for t in 0..THREADS {
        let own: Vec<usize> = ids
            .iter()
            .filter_map(|id| id[1..].parse::<usize>().ok())
            .filter(|n| *n >= 1000 * (t + 1) && *n < 1000 * (t + 2))
            .collect();
        assert!(
            own.windows(2).all(|pair| pair[0] < pair[1]),
            "thread {t}: {own:?}"
        );
    }
}
