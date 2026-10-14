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

// Docker jobs' caches: a job runs with a copy of its repository's cache, which becomes the cache only after a
// successful run of the repository's own workflow on its default branch whose disk the guest let go cleanly.

const ON_MAIN: &str = "org/app/.github/workflows/ci.yml@refs/heads/main";

/// A Docker job on a machine of its own, started with a promise: its Actions runner's name.
async fn docker_job(w: &mut World) -> String {
    let ticket = w.m.reserve_docker(SIZE).expect("room for a Docker job");
    let Claim::Start(ticket) = w.m.claim(SIZE, Some(ticket)) else {
        panic!("the promise is claimed");
    };
    w.start_on(ticket).await.expect("the machine starts")
}

fn copy_of_last_start(w: &World) -> std::path::PathBuf {
    w.parts.vms.started().last().unwrap().docker_cache.clone().expect("a copy of the cache")
}

/// The job ends: its machine first, then GitHub's word on it.
fn ends(w: &mut World, runner: &str, result: &str, event: &str, workflow: &str) {
    w.github_run(EventKind::Started, runner, "", event, workflow);
    w.parts.vms.end(job_name(runner), End::Ok);
    pass(&w.m, 2);
    w.github_run(EventKind::Completed, runner, result, event, workflow);
    w.assert_nothing_left();
}

fn head(path: &std::path::Path) -> Vec<u8> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    std::fs::File::open(path).unwrap().take(64).read_to_end(&mut bytes).unwrap();
    bytes
}

fn kept_repos(dir: &std::path::Path) -> Vec<String> {
    cf_daemon::caches::CacheStore::open(dir, u64::MAX)
        .map(|caches| caches.list().into_iter().map(|(_, cache)| cache.repo).collect())
        .unwrap_or_default()
}

/// A Mac that keeps caches in a folder of the test's own, or none where the folder's file system cannot share
/// blocks between files.
fn caching() -> Option<(tempfile::TempDir, World)> {
    let dir = tempfile::tempdir().unwrap();
    cf_daemon::caches::CacheStore::open(dir.path(), 1 << 40)?;
    let w = World::with_docker_cache(3, dir.path(), 100 << 30);
    Some((dir, w))
}

#[tokio::test]
async fn a_successful_push_to_main_keeps_its_copy_and_the_next_docker_job_starts_from_it() {
    let Some((dir, mut w)) = caching() else { return };
    let runner = docker_job(&mut w).await;
    let copy = copy_of_last_start(&w);
    std::fs::write(&copy, b"what main built").unwrap();
    ends(&mut w, &runner, "succeeded", "push", ON_MAIN);
    assert_eq!(kept_repos(dir.path()), [REPO]);
    assert!(!copy.exists(), "the copy became the cache");

    let next = docker_job(&mut w).await;
    assert_eq!(head(&copy_of_last_start(&w)), b"what main built");
    ends(&mut w, &next, "failed", "push", ON_MAIN);
}

#[tokio::test]
async fn a_pull_request_a_failure_or_an_unclean_disk_never_writes_the_cache() {
    let Some((dir, mut w)) = caching() else { return };
    let first = docker_job(&mut w).await;
    std::fs::write(copy_of_last_start(&w), b"main").unwrap();
    ends(&mut w, &first, "succeeded", "push", ON_MAIN);

    for (result, event, workflow) in [
        ("succeeded", "pull_request", "org/app/.github/workflows/ci.yml@refs/pull/3/merge"),
        ("succeeded", "push", "org/app/.github/workflows/ci.yml@refs/heads/feature"),
        ("failed", "push", ON_MAIN),
        ("canceled", "push", ON_MAIN),
        ("succeeded", "push", "other/tools/.github/workflows/build.yml@refs/heads/main"),
    ] {
        let runner = docker_job(&mut w).await;
        std::fs::write(copy_of_last_start(&w), event.as_bytes()).unwrap();
        ends(&mut w, &runner, result, event, workflow);
        let next = docker_job(&mut w).await;
        assert_eq!(head(&copy_of_last_start(&w)), b"main", "{result} {event} {workflow}");
        ends(&mut w, &next, "failed", "push", ON_MAIN);
    }

    // The guest could not let the disk go: not kept either.
    w.parts.vms.cache_end(cf_guest_proto::CacheEnd {
        found: cf_guest_proto::CacheFound::Reused,
        clean: false,
        daemon: true,
    });
    let runner = docker_job(&mut w).await;
    std::fs::write(copy_of_last_start(&w), b"unclean").unwrap();
    ends(&mut w, &runner, "succeeded", "push", ON_MAIN);
    assert_eq!(kept_repos(dir.path()), [REPO]);
    let next = docker_job(&mut w).await;
    assert_eq!(head(&copy_of_last_start(&w)), b"main");
    ends(&mut w, &next, "failed", "push", ON_MAIN);
}

#[tokio::test]
async fn a_job_github_never_says_the_end_of_does_not_write_the_cache() {
    let Some((dir, mut w)) = caching() else { return };
    let runner = docker_job(&mut w).await;
    let copy = copy_of_last_start(&w);
    std::fs::write(&copy, b"unsaid").unwrap();
    w.github_run(EventKind::Started, &runner, "", "push", ON_MAIN);
    w.parts.vms.end(job_name(&runner), End::Ok);
    pass(&w.m, 2);
    w.assert_nothing_left();
    assert!(kept_repos(dir.path()).is_empty());
    assert!(!copy.exists(), "the copy is thrown away");
}

#[tokio::test]
async fn a_cache_copy_outlives_a_restart_of_the_runner_and_is_settled_after() {
    let Some((dir, _)) = caching() else { return };
    // Records on disk, as the service keeps them: what a restart reads back is what was written.
    let records = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(cf_daemon::DiskStore::new(records.path()).unwrap());
    let mut w = World::keeping(cf_daemon::testing::settings_for(3), store);
    w.parts.docker_cache = Some((dir.path().to_path_buf(), 100 << 30));
    w.m = w.parts.machines(None);
    let runner = docker_job(&mut w).await;
    std::fs::write(copy_of_last_start(&w), b"across a restart").unwrap();
    w.parts.vms.ready(job_name(&runner));
    pass(&w.m, 2);
    w.github_run(EventKind::Started, &runner, "", "push", ON_MAIN);
    w.restart();
    w.parts.vms.end(job_name(&runner), End::Ok);
    pass(&w.m, 2);
    w.github_run(EventKind::Completed, &runner, "succeeded", "push", ON_MAIN);
    w.assert_nothing_left();
    assert_eq!(kept_repos(dir.path()), [REPO]);
}

#[tokio::test]
async fn a_machine_that_does_not_start_leaves_no_copy_and_a_plain_job_gets_none() {
    let Some((dir, mut w)) = caching() else { return };
    w.parts.vms.fail_start(1);
    let ticket = w.m.reserve_docker(SIZE).unwrap();
    let Claim::Start(ticket) = w.m.claim(SIZE, Some(ticket)) else {
        panic!("the promise is claimed");
    };
    assert!(w.start_on(ticket).await.is_err());
    let jobs: Vec<_> = std::fs::read_dir(dir.path().join("jobs")).unwrap().collect();
    assert!(jobs.is_empty(), "no copy is left");
    let plain = w.start().await;
    assert_eq!(w.parts.vms.started().last().unwrap().docker_cache, None, "{plain} has no Docker");
}
