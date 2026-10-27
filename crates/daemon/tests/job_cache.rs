//! Jobs' caches: every job runs with a copy of its repository's cache, which becomes the cache only after a
//! successful run of the repository's own workflow on its default branch whose disk the guest let go cleanly. Jobs
//! that run at once each write a copy of their own.

mod support;

use cf_daemon::caches::WorkflowRun;
use cf_daemon::names::job_name;
use cf_daemon::testing::pass;
use cf_daemon::{Claim, EventKind, Slots};
use cf_vmhost::End;
use support::world::{SIZE, World};
use support::*;

const ON_MAIN: &str = "org/app/.github/workflows/ci.yml@refs/heads/main";

/// A job without Docker: its Actions runner's name.
async fn job(w: &mut World) -> String {
    w.start().await
}

/// A Docker job on a machine of its own, started with a promise: its Actions runner's name.
async fn docker_job(w: &mut World) -> String {
    let ticket = w.m.reserve_docker(SIZE).expect("room for a Docker job");
    let Claim::Start(ticket) = w.m.claim(SIZE, Some(ticket)) else {
        panic!("the promise is claimed");
    };
    w.start_on(ticket).await.expect("the machine starts")
}

fn copy_of_last_start(w: &World) -> std::path::PathBuf {
    w.parts
        .vms
        .started()
        .last()
        .unwrap()
        .cache
        .clone()
        .expect("a copy of the cache")
}

/// What GitHub's REST API says of the run of a job whose workflow is at `workflow`: the branch of that ref, or a
/// pull request's head.
fn run_of(event: &str, workflow: &str) -> WorkflowRun {
    WorkflowRun {
        head_branch: Some(
            workflow
                .rsplit_once("@refs/heads/")
                .map_or("feature", |(_, branch)| branch)
                .into(),
        ),
        head_repository: Some(REPO.into()),
        event: event.into(),
    }
}

/// Whether a job of this event and workflow may write the cache, as far as GitHub's word on the job goes.
fn writes(event: &str, workflow: &str) -> bool {
    ["push", "schedule", "workflow_dispatch"].contains(&event) && workflow == ON_MAIN
}

/// What the sweep does while a job runs: reads its run, as GitHub says it.
fn read_run(w: &World, runner: &str, run: WorkflowRun) {
    let wanted = w.m.runs_to_read();
    assert_eq!(
        wanted,
        [(job_name(runner).to_string(), REPO.to_string(), 42)],
        "the run to read"
    );
    w.m.run_read(job_name(runner), run);
    assert!(w.m.runs_to_read().is_empty(), "read once");
}

/// The job ends: its machine first, then GitHub's word on it.
fn ends(w: &mut World, runner: &str, result: &str, event: &str, workflow: &str) {
    ends_with_run(w, runner, result, event, workflow, Some(run_of(event, workflow)));
}

/// [`ends`] with what the sweep read of the run, if anything.
fn ends_with_run(
    w: &mut World,
    runner: &str,
    result: &str,
    event: &str,
    workflow: &str,
    run: Option<WorkflowRun>,
) {
    w.github_run(EventKind::Started, runner, "", event, workflow);
    if !writes(event, workflow) {
        // A run GitHub refused already, a pull request's say, is never read.
        assert!(
            w.m.runs_to_read().is_empty(),
            "{event} {workflow}: nothing to read"
        );
    } else if let Some(run) = run {
        read_run(w, runner, run);
    }
    w.parts.vms.end(job_name(runner), End::Ok);
    pass(&w.m, 2);
    w.github_run(EventKind::Completed, runner, result, event, workflow);
    w.assert_nothing_left();
}

/// The repositories whose caches are on disk, as a store that opens the folder finds them.
fn kept_repos(dir: &std::path::Path) -> Vec<String> {
    cf_daemon::caches::CacheStore::open(dir, u64::MAX)
        .map(|caches| {
            caches
                .list()
                .iter()
                .map(|(_, cache)| cache.repo.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// A Mac that keeps caches in a folder of the test's own, or none where the folder's file system cannot share
/// blocks between files.
fn caching() -> Option<(tempfile::TempDir, World)> {
    let dir = cache_folder()?;
    let w = World::with_cache(3, dir.path(), 100 << 30);
    Some((dir, w))
}

#[tokio::test]
async fn a_successful_push_to_main_keeps_its_copy_and_the_next_job_starts_from_it() {
    let Some((dir, mut w)) = caching() else { return };
    let runner = job(&mut w).await;
    let copy = copy_of_last_start(&w);
    std::fs::write(&copy, b"what main built").unwrap();
    ends(&mut w, &runner, "succeeded", "push", ON_MAIN);
    assert_eq!(kept_repos(dir.path()), [REPO]);
    assert!(!copy.exists(), "the copy became the cache");
    // What the console is told: the cache with its size and last write, and what happened.
    let snapshot = w.m.snapshot();
    assert_eq!(snapshot.caches.len(), 1);
    assert_eq!(snapshot.caches[0].repo, REPO);
    assert!(snapshot.caches[0].written_at > 0);
    assert!(snapshot.caches[0].bytes > 0);
    let kinds: Vec<&str> = snapshot
        .cache_events
        .iter()
        .map(|event| event.kind.name())
        .collect();
    assert_eq!(kinds, ["kept"]);

    let next = job(&mut w).await;
    assert_eq!(head(&copy_of_last_start(&w)), b"what main built");
    ends(&mut w, &next, "failed", "push", ON_MAIN);
}

#[tokio::test]
async fn a_pull_request_a_failure_or_an_unclean_disk_never_writes_the_cache() {
    let Some((dir, mut w)) = caching() else { return };
    let first = job(&mut w).await;
    std::fs::write(copy_of_last_start(&w), b"main").unwrap();
    ends(&mut w, &first, "succeeded", "push", ON_MAIN);

    for (result, event, workflow) in [
        (
            "succeeded",
            "pull_request",
            "org/app/.github/workflows/ci.yml@refs/pull/3/merge",
        ),
        (
            "succeeded",
            "push",
            "org/app/.github/workflows/ci.yml@refs/heads/feature",
        ),
        ("failed", "push", ON_MAIN),
        ("canceled", "push", ON_MAIN),
        (
            "succeeded",
            "push",
            "other/tools/.github/workflows/build.yml@refs/heads/main",
        ),
    ] {
        let runner = job(&mut w).await;
        std::fs::write(copy_of_last_start(&w), event.as_bytes()).unwrap();
        ends(&mut w, &runner, result, event, workflow);
        let next = job(&mut w).await;
        assert_eq!(
            head(&copy_of_last_start(&w)),
            b"main",
            "{result} {event} {workflow}"
        );
        ends(&mut w, &next, "failed", "push", ON_MAIN);
    }

    // The guest could not let the disk go: not kept either.
    w.parts.vms.cache_end(cf_guest_proto::CacheEnd {
        found: cf_guest_proto::CacheFound::Reused,
        clean: false,
        docker: None,
    });
    let runner = job(&mut w).await;
    std::fs::write(copy_of_last_start(&w), b"unclean").unwrap();
    ends(&mut w, &runner, "succeeded", "push", ON_MAIN);
    assert_eq!(kept_repos(dir.path()), [REPO]);
    let next = job(&mut w).await;
    assert_eq!(head(&copy_of_last_start(&w)), b"main");
    ends(&mut w, &next, "failed", "push", ON_MAIN);
}

#[tokio::test]
async fn a_job_github_never_says_the_end_of_does_not_write_the_cache() {
    let Some((dir, mut w)) = caching() else { return };
    let runner = job(&mut w).await;
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
    let Some(dir) = cache_folder() else { return };
    // Records on disk, as the service keeps them: what a restart reads back is what was written.
    let records = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(cf_daemon::DiskStore::new(records.path()).unwrap());
    let mut w = World::keeping(cf_daemon::testing::settings_for(3), store);
    w.parts.cache = Some((dir.path().to_path_buf(), 100 << 30));
    w.m = w.parts.machines(None);
    let runner = job(&mut w).await;
    std::fs::write(copy_of_last_start(&w), b"across a restart").unwrap();
    w.parts.vms.ready(job_name(&runner));
    pass(&w.m, 2);
    w.github_run(EventKind::Started, &runner, "", "push", ON_MAIN);
    read_run(&w, &runner, run_of("push", ON_MAIN));
    w.restart();
    w.parts.vms.end(job_name(&runner), End::Ok);
    pass(&w.m, 2);
    w.github_run(EventKind::Completed, &runner, "succeeded", "push", ON_MAIN);
    w.assert_nothing_left();
    assert_eq!(kept_repos(dir.path()), [REPO]);
}

#[tokio::test]
async fn a_machine_that_does_not_start_leaves_no_copy() {
    let Some((dir, mut w)) = caching() else { return };
    w.parts.vms.fail_start(1);
    let Claim::Start(ticket) = w.m.claim(SIZE, None) else {
        panic!("a slot");
    };
    assert!(w.start_on(ticket).await.is_err());
    let jobs: Vec<_> = std::fs::read_dir(dir.path().join("jobs")).unwrap().collect();
    assert!(jobs.is_empty(), "no copy is left");
}

#[tokio::test]
async fn a_job_whose_run_checked_out_another_branch_or_was_never_read_does_not_write_the_cache() {
    let Some((dir, mut w)) = caching() else { return };
    let first = job(&mut w).await;
    std::fs::write(copy_of_last_start(&w), b"main").unwrap();
    ends(&mut w, &first, "succeeded", "push", ON_MAIN);

    // A reusable workflow at main, called by a push to another branch: its workflow's ref is main's, its code is not.
    let called = job(&mut w).await;
    std::fs::write(copy_of_last_start(&w), b"a feature branch").unwrap();
    let feature = WorkflowRun {
        head_branch: Some("feature".into()),
        ..run_of("push", ON_MAIN)
    };
    ends_with_run(&mut w, &called, "succeeded", "push", ON_MAIN, Some(feature));
    // A run the sweep never read: GitHub was away the whole job.
    let unread = job(&mut w).await;
    std::fs::write(copy_of_last_start(&w), b"unread").unwrap();
    ends_with_run(&mut w, &unread, "succeeded", "push", ON_MAIN, None);

    let next = job(&mut w).await;
    assert_eq!(head(&copy_of_last_start(&w)), b"main");
    ends(&mut w, &next, "failed", "push", ON_MAIN);
    assert_eq!(kept_repos(dir.path()), [REPO]);
    let whys: Vec<String> =
        w.m.snapshot()
            .cache_events
            .iter()
            .filter_map(|event| event.why.clone())
            .collect();
    assert!(
        whys.contains(&"the run's code is not the default branch's".to_string()),
        "{whys:?}"
    );
    assert!(
        whys.contains(&"GitHub's run was not read".to_string()),
        "{whys:?}"
    );
}

#[tokio::test]
async fn jobs_that_run_at_once_each_write_a_copy_of_their_own_and_the_cache_is_one_of_them_whole() {
    let Some((dir, mut w)) = caching() else { return };
    let first = job(&mut w).await;
    let first_copy = copy_of_last_start(&w);
    let second = job(&mut w).await;
    let second_copy = copy_of_last_start(&w);
    assert_ne!(first_copy, second_copy, "one copy per job");
    std::fs::write(&first_copy, b"first").unwrap();
    std::fs::write(&second_copy, b"second").unwrap();
    assert_eq!(
        head(&first_copy),
        b"first",
        "the second job's writes never reach the first's copy"
    );
    // The first ends while the second still runs.
    w.github_run(EventKind::Started, &first, "", "push", ON_MAIN);
    read_run(&w, &first, run_of("push", ON_MAIN));
    w.parts.vms.end(job_name(&first), End::Ok);
    pass(&w.m, 2);
    w.github_run(EventKind::Completed, &first, "succeeded", "push", ON_MAIN);
    assert_eq!(kept_repos(dir.path()), [REPO]);
    assert_eq!(
        head(&second_copy),
        b"second",
        "keeping the first's copy leaves the second's alone"
    );
    ends(&mut w, &second, "succeeded", "push", ON_MAIN);
    assert_eq!(kept_repos(dir.path()), [REPO]);
    let next = job(&mut w).await;
    assert_eq!(
        head(&copy_of_last_start(&w)),
        b"second",
        "the last kept copy, whole"
    );
    ends(&mut w, &next, "failed", "push", ON_MAIN);
}

#[tokio::test]
async fn a_docker_job_and_a_job_without_docker_start_from_one_cache() {
    let Some((dir, mut w)) = caching() else { return };
    let builder = docker_job(&mut w).await;
    std::fs::write(copy_of_last_start(&w), b"images and packages").unwrap();
    ends(&mut w, &builder, "succeeded", "push", ON_MAIN);
    let plain = job(&mut w).await;
    assert_eq!(head(&copy_of_last_start(&w)), b"images and packages");
    std::fs::write(copy_of_last_start(&w), b"packages, newer").unwrap();
    ends(&mut w, &plain, "succeeded", "push", ON_MAIN);
    let docker = docker_job(&mut w).await;
    assert_eq!(head(&copy_of_last_start(&w)), b"packages, newer");
    ends(&mut w, &docker, "failed", "push", ON_MAIN);
    assert_eq!(kept_repos(dir.path()), [REPO]);
}

#[tokio::test]
async fn only_a_docker_job_whose_daemon_failed_drops_the_cache() {
    let Some((dir, mut w)) = caching() else { return };
    let first = job(&mut w).await;
    std::fs::write(copy_of_last_start(&w), b"main").unwrap();
    ends(&mut w, &first, "succeeded", "push", ON_MAIN);

    // A job without Docker that failed leaves the cache as it was.
    w.parts.vms.cache_end(cf_guest_proto::CacheEnd {
        found: cf_guest_proto::CacheFound::Reused,
        clean: true,
        docker: None,
    });
    let failed = job(&mut w).await;
    ends(&mut w, &failed, "failed", "push", ON_MAIN);
    assert_eq!(kept_repos(dir.path()), [REPO]);

    // A Docker job whose daemon could not run on it: the cache goes, and the next job starts from an empty one.
    w.parts.vms.cache_end(cf_guest_proto::CacheEnd {
        found: cf_guest_proto::CacheFound::Reused,
        clean: true,
        docker: Some(false),
    });
    let broken = docker_job(&mut w).await;
    ends(&mut w, &broken, "succeeded", "push", ON_MAIN);
    assert!(kept_repos(dir.path()).is_empty());
    let kinds: Vec<&str> =
        w.m.snapshot()
            .cache_events
            .iter()
            .map(|event| event.kind.name())
            .collect();
    assert_eq!(kinds, ["kept", "discarded", "dropped"]);
    let next = job(&mut w).await;
    assert!(head(&copy_of_last_start(&w)).iter().all(|byte| *byte == 0));
    ends(&mut w, &next, "failed", "push", ON_MAIN);
}
