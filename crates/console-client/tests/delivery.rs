//! The outbox, the client and the console together: events survive a console that is away, a lost answer, a refusal and
//! a restart, and the console is not sent what it took.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use cf_console_client::{Client, Error, JobEvent, OUTBOX_CAPACITY, Outbox, REPORT_BATCH};
use cf_fake_github::httpd::{Handler, Reply, Server};
use cf_ghapp::Http;
use cf_secret::Secret;
use serde_json::json;
use support::*;

fn outbox(dir: &tempfile::TempDir) -> Outbox {
    Outbox::open(dir.path().join("outbox.json"), OUTBOX_CAPACITY)
}

fn report_sizes(w: &World) -> Vec<usize> {
    w.fake
        .reports()
        .iter()
        .map(|r| r["jobs"].as_array().map_or(0, Vec::len))
        .collect()
}

#[tokio::test]
async fn a_console_that_is_away_for_700_events_gets_the_newest_500_oldest_first_in_batches_of_100_each_once()
{
    let w = world().await;
    let dir = tempfile::tempdir().expect("dir");
    let outbox = outbox(&dir);
    for n in 0..700 {
        outbox.push(event(n));
    }
    assert_eq!((outbox.len(), outbox.dropped()), (500, 200));
    w.fake.down(true);
    for _ in 0..3 {
        let got = w.client.deliver(&outbox, report("mac-a")).await;
        assert!(matches!(got, Err(Error::Unavailable(_))), "{got:?}");
    }
    assert_eq!(
        (outbox.len(), w.fake.job_events().len()),
        (500, 0),
        "nothing left the outbox, nothing arrived"
    );
    w.fake.down(false);
    for round in 1..=5 {
        w.client
            .deliver(&outbox, report("mac-a"))
            .await
            .expect("delivered");
        assert_eq!(outbox.len(), 500 - 100 * round);
    }
    assert_eq!(report_sizes(&w), vec![100; 5]);
    let expected: Vec<String> = (200..700).map(|n| format!("e{n:04}")).collect();
    assert_eq!(
        w.fake.job_event_ids(),
        expected,
        "oldest first, nothing twice, nothing lost but the dropped"
    );
    // With nothing waiting a report still goes, with no events.
    w.client
        .deliver(&outbox, report("mac-a"))
        .await
        .expect("delivered");
    assert_eq!(report_sizes(&w).last(), Some(&0));
    assert_eq!(REPORT_BATCH, 100);
}

#[tokio::test]
async fn a_report_the_console_took_whose_answer_was_lost_is_sent_again_with_the_same_ids() {
    let w = world().await;
    let dir = tempfile::tempdir().expect("dir");
    let outbox = outbox(&dir);
    for n in 0..150 {
        outbox.push(event(n));
    }
    w.fake.accept_then_drop_next(1);
    assert!(matches!(
        w.client.deliver(&outbox, report("mac-a")).await,
        Err(Error::Unavailable(_))
    ));
    assert_eq!(outbox.len(), 150, "nothing is marked on a lost answer");
    assert_eq!(w.fake.job_events().len(), 100, "the console took them");
    w.client
        .deliver(&outbox, report("mac-a"))
        .await
        .expect("delivered");
    w.client
        .deliver(&outbox, report("mac-a"))
        .await
        .expect("delivered");
    let ids = w.fake.job_event_ids();
    assert_eq!(ids.len(), 250);
    assert_eq!(ids[..100], ids[100..200], "the same events with the same ids");
    let rest: Vec<String> = (100..150).map(|n| format!("e{n:04}")).collect();
    assert_eq!(ids[200..], rest[..], "then the rest");
    assert!(outbox.is_empty());
    // After an answer, none goes again.
    w.client
        .deliver(&outbox, report("mac-a"))
        .await
        .expect("delivered");
    assert_eq!(w.fake.job_event_ids().len(), 250);
}

#[tokio::test]
async fn a_retry_inside_the_http_client_sends_the_batch_again_and_marks_it_once() {
    let w = world_with(Http::new(Duration::from_secs(5), fast_retry())).await;
    let dir = tempfile::tempdir().expect("dir");
    let outbox = outbox(&dir);
    for n in 0..3 {
        outbox.push(event(n));
    }
    w.fake.accept_then_drop_next(1);
    w.client
        .deliver(&outbox, report("mac-a"))
        .await
        .expect("the retry works");
    assert_eq!(
        w.fake.job_event_ids(),
        ["e0000", "e0001", "e0002", "e0000", "e0001", "e0002"]
    );
    assert!(outbox.is_empty());
}

#[tokio::test]
async fn an_id_pushed_twice_arrives_once() {
    let w = world().await;
    let dir = tempfile::tempdir().expect("dir");
    let outbox = outbox(&dir);
    assert!(outbox.push(event(1)));
    assert!(!outbox.push(event(1)));
    w.client
        .deliver(&outbox, report("mac-a"))
        .await
        .expect("delivered");
    assert!(!outbox.push(event(1)), "and not again once it was taken");
    w.client
        .deliver(&outbox, report("mac-a"))
        .await
        .expect("delivered");
    assert_eq!(w.fake.job_event_ids(), ["e0001"]);
}

#[tokio::test]
async fn a_401_a_refusal_and_a_page_that_is_not_the_console_mark_nothing_sent() {
    let w = world().await;
    let dir = tempfile::tempdir().expect("dir");
    let outbox = outbox(&dir);
    for n in 0..5 {
        outbox.push(event(n));
    }
    w.fake.fail_next(1, 401);
    assert_eq!(
        w.client.deliver(&outbox, report("mac-a")).await.unwrap_err(),
        Error::Unauthorized
    );
    w.fake.fail_next(1, 400);
    assert!(matches!(
        w.client.deliver(&outbox, report("mac-a")).await,
        Err(Error::Refused { status: 400, .. })
    ));
    // A token the console does not know.
    let stranger = Client::new(
        &w.fake.url(),
        Secret::new(format!("cfr_{}", "Q".repeat(43))),
        http(),
    );
    assert_eq!(
        stranger.deliver(&outbox, report("mac-a")).await.unwrap_err(),
        Error::Unauthorized
    );
    // A 200 that is a sign-in page: not the console's answer, so the events stay (a resend costs nothing, a loss does).
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let handler: Handler = Arc::new(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Reply::text(200, "<html>Sign in</html>") })
    });
    let page = Server::start(handler).await;
    let client = Client::new(&page.url(), Secret::new(w.token.clone()), http());
    assert!(matches!(
        client.deliver(&outbox, report("mac-a")).await,
        Err(Error::Protocol(_))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(outbox.len(), 5);
    assert!(w.fake.job_events().is_empty());
    // And the same outbox still delivers to the real console.
    w.client
        .deliver(&outbox, report("mac-a"))
        .await
        .expect("delivered");
    assert_eq!(w.fake.job_event_ids().len(), 5);
    assert!(outbox.is_empty());
}

#[tokio::test]
async fn a_report_refused_before_sending_marks_nothing_and_a_poisoned_caller_report_cannot_take_the_events() {
    let w = world().await;
    let dir = tempfile::tempdir().expect("dir");
    let outbox = outbox(&dir);
    outbox.push(event(1));
    let calls = w.fake.calls();
    let got = w.client.deliver(&outbox, report("Not A Name")).await;
    assert!(matches!(got, Err(Error::Protocol(_))));
    assert_eq!((outbox.len(), w.fake.calls()), (1, calls));
}

#[tokio::test]
async fn the_reports_own_jobs_are_replaced_by_the_outboxs() {
    let w = world().await;
    let dir = tempfile::tempdir().expect("dir");
    let outbox = outbox(&dir);
    outbox.push(event(1));
    let mut r = report("mac-a");
    r.jobs = vec![event(99), event(98)];
    w.client.deliver(&outbox, r).await.expect("delivered");
    assert_eq!(w.fake.job_event_ids(), ["e0001"]);
}

#[tokio::test]
async fn a_restart_between_the_send_and_the_next_one_loses_and_repeats_nothing_the_console_acknowledged() {
    let w = world().await;
    let dir = tempfile::tempdir().expect("dir");
    {
        let first = outbox(&dir);
        for n in 0..5 {
            first.push(event(n));
        }
        w.client
            .deliver(&first, report("mac-a"))
            .await
            .expect("delivered");
        first.push(event(10));
        first.push(event(11));
    }
    let second = outbox(&dir);
    assert!(!second.push(event(3)), "taken before the restart");
    assert_eq!(second.len(), 2);
    w.client
        .deliver(&second, report("mac-a"))
        .await
        .expect("delivered");
    assert_eq!(
        w.fake.job_event_ids(),
        ["e0000", "e0001", "e0002", "e0003", "e0004", "e0010", "e0011"]
    );
}

#[tokio::test]
async fn a_file_damaged_while_the_console_was_away_costs_the_events_and_not_the_reports() {
    let w = world().await;
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("outbox.json");
    {
        let outbox = Outbox::open(&path, 10);
        outbox.push(event(1));
    }
    std::fs::write(&path, b"{\"waiting\": [").expect("damage");
    let outbox = Outbox::open(&path, 10);
    w.client
        .deliver(&outbox, report("mac-a"))
        .await
        .expect("delivered");
    assert_eq!(report_sizes(&w), vec![0]);
}

#[tokio::test]
async fn two_deliveries_at_once_take_turns_and_send_no_event_twice() {
    let w = world().await;
    let dir = tempfile::tempdir().expect("dir");
    let outbox = outbox(&dir);
    for n in 0..150 {
        outbox.push(event(n));
    }
    let (a, b) = tokio::join!(
        w.client.deliver(&outbox, report("mac-a")),
        w.client.deliver(&outbox, report("mac-a"))
    );
    a.expect("first");
    b.expect("second");
    let mut ids = w.fake.job_event_ids();
    assert_eq!(ids.len(), 150, "{:?}", report_sizes(&w));
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 150);
    assert!(outbox.is_empty());
}

#[tokio::test]
async fn commands_and_pauses_come_back_with_the_delivery() {
    let w = world().await;
    let dir = tempfile::tempdir().expect("dir");
    let outbox = outbox(&dir);
    w.fake.pause(true);
    let id = w.fake.queue_command("update", None);
    let answer = w
        .client
        .deliver(&outbox, report("mac-a"))
        .await
        .expect("delivered");
    assert!(answer.paused);
    assert_eq!(
        (
            answer.commands[0].id.as_str(),
            answer.commands[0].kind.as_str(),
            &answer.commands[0].args
        ),
        (id.as_str(), "update", &None)
    );
    let event_json: JobEvent = serde_json::from_value(json!({"id": "x", "kind": "completed", "runner": "r", "at": "2026-10-10T08:00:00Z", "outOfMemory": true})).expect("event");
    assert!(event_json.out_of_memory);
    outbox.push(event_json);
    w.client
        .deliver(&outbox, report("mac-a"))
        .await
        .expect("delivered");
    assert_eq!(w.fake.job_events()[0]["outOfMemory"], true);
    assert_eq!(w.fake.job_events()[0]["kind"], "completed");
}
