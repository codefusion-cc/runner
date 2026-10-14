//! Docker jobs (`cf-docker`): their machines get a daemon, and a Mac runs at most `docker_max_jobs` of them at once
//! (1 unless set): promised, waiting in line or running, until the machine is gone. A job GitHub assigns without a
//! promise runs even past the cap, as any assigned job does.

mod support;

use cf_daemon::names::job_name;
use cf_daemon::testing::pass;
use cf_daemon::{Claim, DOCKER_FULL, EventKind, Slots};
use cf_fake_github::JobState;
use cf_scaleset::Scaler as _;
use cf_vmhost::End;
use support::world::{SIZE, World};
use support::*;

fn docker_of_last_start(w: &World) -> bool {
    w.parts
        .vms
        .started()
        .last()
        .expect("a machine was started")
        .docker
}

#[tokio::test]
async fn a_docker_jobs_machine_gets_a_daemon_and_another_does_not() {
    let mut w = World::new(3);
    let plain = w.start().await;
    assert!(!docker_of_last_start(&w), "{plain} asked for no Docker");

    let ticket = w.m.reserve_docker(SIZE).expect("room for a Docker job");
    let Claim::Start(ticket) = w.m.claim(SIZE, Some(ticket)) else {
        panic!("the promise is claimed");
    };
    w.start_on(ticket).await.expect("the machine starts");
    assert!(docker_of_last_start(&w));
    assert_eq!(
        w.parts.store.load().iter().filter(|record| record.docker).count(),
        1,
        "the record says which machine has Docker"
    );
}

#[tokio::test]
async fn one_docker_job_at_a_time_while_other_jobs_still_fit() {
    let w = World::new(3);
    assert_eq!(w.m.docker_room(), 1);
    let first = w.m.reserve_docker(SIZE).expect("the first");
    assert_eq!(w.m.docker_room(), 0);
    assert_eq!(
        w.m.reserve_docker(SIZE).map_err(|refusal| refusal.0),
        Err(DOCKER_FULL.to_string())
    );
    assert!(w.m.reserve(SIZE).is_ok(), "a job without Docker still fits");

    w.m.release(first);
    assert_eq!(w.m.docker_room(), 1, "a promise given back frees the place");
    assert!(w.m.reserve_docker(SIZE).is_ok());
}

#[tokio::test]
async fn a_docker_machine_holds_its_place_until_it_is_gone() {
    let mut w = World::new(3);
    let ticket = w.m.reserve_docker(SIZE).unwrap();
    let Claim::Start(ticket) = w.m.claim(SIZE, Some(ticket)) else {
        panic!("the promise is claimed");
    };
    let runner = w.start_on(ticket).await.unwrap();
    pass(&w.m, 2);
    assert_eq!(w.m.docker_room(), 0, "a running Docker job holds the place");

    w.parts.vms.end(job_name(&runner), End::Ok);
    w.github(EventKind::Completed, &runner, "succeeded");
    w.assert_nothing_left();
    assert_eq!(w.m.docker_room(), 1, "the place is free once the machine is gone");
}

#[tokio::test]
async fn a_docker_job_github_assigned_past_the_cap_still_runs_with_its_daemon() {
    let mut w = World::new(3);
    let _promised = w.m.reserve_docker(SIZE).unwrap();
    let Claim::Start(ticket) = w.m.claim(SIZE, None) else {
        panic!("an assigned job starts where memory allows");
    };
    w.m.mark_docker(ticket);
    w.start_on(ticket).await.unwrap();
    assert!(docker_of_last_start(&w));
    assert_eq!(w.m.docker_room(), 0);
}

#[tokio::test]
async fn a_docker_machine_found_after_a_restart_still_holds_the_place() {
    let mut w = World::new(3);
    let ticket = w.m.reserve_docker(SIZE).unwrap();
    let Claim::Start(ticket) = w.m.claim(SIZE, Some(ticket)) else {
        panic!("the promise is claimed");
    };
    let runner = w.start_on(ticket).await.unwrap();
    w.parts.vms.ready(job_name(&runner));
    pass(&w.m, 2);
    w.restart();
    pass(&w.m, 1);
    assert!(w.up(&runner), "the machine was adopted");
    assert_eq!(w.m.docker_room(), 0);
    assert!(w.m.reserve_docker(SIZE).is_err());
}

#[tokio::test]
async fn a_docker_scale_set_acquires_one_job_and_offers_no_more_while_it_runs() {
    let mut h = harness(3).await;
    h.scaler = scaler_with(
        &Parts {
            client: &h.client,
            set: &h.set,
            session: &h.session,
            slots: &h.slots,
            hosts: &h.hosts,
            roster: &h.roster,
            clock: &h.clock,
            events: &h.events,
        },
        REPO,
        true,
    );
    let ids: Vec<i64> = (1..=2)
        .map(|i| {
            h.fake
                .queue_job(REPO, &["self-hosted", "cf-2g"], &format!("stack {i}"))
        })
        .collect();
    let message = next_messages(&h, 2, 5).await;
    h.scaler.scale(Some(&message)).await.unwrap();
    assert_eq!(h.fake.job_state(ids[0]), Some(JobState::Acquired(h.set.id)));
    assert_eq!(
        h.fake.job_state(ids[1]),
        Some(JobState::Available),
        "the second waits at GitHub for the Docker place"
    );
    assert_eq!(h.scaler.capacity(), 1, "the promise, and nothing more to offer");

    h.scaler.scale(Some(&stats(1))).await.unwrap();
    let started = h.parts.vms.started();
    assert_eq!(started.len(), 1);
    assert!(started[0].docker);
    assert_eq!(
        h.scaler.capacity(),
        1,
        "the running job, and still nothing to offer"
    );
}
