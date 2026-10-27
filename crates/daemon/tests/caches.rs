//! Jobs' caches: one per repository, written back only by a successful run of the repository's own workflow
//! on its default branch whose disk was let go cleanly, under a budget the least recently used cache leaves first.

mod support;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use cf_daemon::GithubJob;
use cf_daemon::caches::{
    Cache, CacheCopy, CacheStore, DAEMON_SILENT, DISK_BYTES, WorkflowRun, evictions, key, refusal, spoiled,
};
use cf_guest_proto::{CacheEnd, CacheFound};
use proptest::prelude::*;
use support::{cache_folder, head};
use tempfile::TempDir;

const REPO: &str = "codefusion-cc/shop";

fn github(event: &str, workflow: &str, result: Option<&str>) -> GithubJob {
    GithubJob {
        repo: REPO.into(),
        job: "stack".into(),
        workflow: workflow.into(),
        run_id: 7,
        event: event.into(),
        queued_at: None,
        started_at: None,
        result: result.map(str::to_string),
        completed_at: None,
        inferred: false,
    }
}

fn on_main() -> GithubJob {
    github(
        "push",
        "codefusion-cc/shop/.github/workflows/ci.yml@refs/heads/main",
        Some("succeeded"),
    )
}

/// What GitHub's REST API says of a push's run on main.
fn main_run() -> WorkflowRun {
    WorkflowRun {
        head_branch: Some("main".into()),
        head_repository: Some(REPO.into()),
        event: "push".into(),
        head_sha: Some("c".repeat(40)),
        on_default_branch: true,
    }
}

/// A job without Docker whose disk the guest let go cleanly.
fn clean(found: CacheFound) -> Option<CacheEnd> {
    Some(CacheEnd {
        found,
        clean: true,
        docker: None,
    })
}

#[test]
fn a_successful_push_to_the_default_branch_with_a_clean_disk_is_kept() {
    assert_eq!(
        refusal(
            REPO,
            Some("main"),
            Some(&on_main()),
            Some(&main_run()),
            clean(CacheFound::Fresh)
        ),
        None
    );
    for event in ["schedule", "workflow_dispatch"] {
        let job = GithubJob {
            event: event.into(),
            ..on_main()
        };
        assert_eq!(
            refusal(
                REPO,
                Some("main"),
                Some(&job),
                Some(&WorkflowRun {
                    event: event.into(),
                    ..main_run()
                }),
                clean(CacheFound::Reused)
            ),
            None,
            "{event}"
        );
    }
    let spelled = github(
        "push",
        "CodeFusion-CC/Shop/.github/workflows/ci.yml@refs/heads/main",
        Some("success"),
    );
    assert_eq!(
        refusal(
            REPO,
            Some("main"),
            Some(&spelled),
            Some(&WorkflowRun {
                head_repository: Some("CodeFusion-CC/Shop".into()),
                ..main_run()
            }),
            clean(CacheFound::Reused)
        ),
        None,
        "GitHub's names ignore case"
    );
}

#[test]
fn each_condition_alone_refuses() {
    let end = clean(CacheFound::Reused);
    let run = main_run();
    let with = |job: GithubJob| refusal(REPO, Some("main"), Some(&job), Some(&run), end);
    for event in [
        "pull_request",
        "pull_request_target",
        "merge_group",
        "release",
        "workflow_run",
        "",
    ] {
        assert!(
            with(GithubJob {
                event: event.into(),
                ..on_main()
            })
            .is_some(),
            "{event}"
        );
    }
    for result in [
        None,
        Some("failed"),
        Some("canceled"),
        Some("cancelled"),
        Some("skipped"),
        Some(""),
    ] {
        assert!(
            with(GithubJob {
                result: result.map(str::to_string),
                ..on_main()
            })
            .is_some(),
            "{result:?}"
        );
    }
    for workflow in [
        "codefusion-cc/shop/.github/workflows/ci.yml@refs/heads/feature",
        "codefusion-cc/shop/.github/workflows/ci.yml@refs/pull/4/merge",
        "codefusion-cc/shop/.github/workflows/ci.yml@refs/tags/main",
        "codefusion-cc/shop/.github/workflows/ci.yml@refs/heads/main2",
        "codefusion-cc/shop/.github/workflows/ci.yml",
        "codefusion-cc/shop-fork/.github/workflows/ci.yml@refs/heads/main",
        "codefusion-cc/sho/.github/workflows/ci.yml@refs/heads/main",
        "other/shop/.github/workflows/ci.yml@refs/heads/main",
        "codefusion-cc/tools/.github/workflows/ci.yml@refs/heads/main",
        "",
    ] {
        assert!(
            with(GithubJob {
                workflow: workflow.into(),
                ..on_main()
            })
            .is_some(),
            "{workflow}"
        );
    }
    assert!(
        refusal(REPO, Some("main"), None, Some(&run), end).is_some(),
        "no word from GitHub"
    );
    assert!(
        refusal(REPO, None, Some(&on_main()), Some(&run), end).is_some(),
        "no default branch known"
    );
    assert!(
        refusal(REPO, Some(""), Some(&on_main()), Some(&run), end).is_some(),
        "an empty default branch"
    );
    assert!(
        refusal(REPO, Some("develop"), Some(&on_main()), Some(&run), end).is_some(),
        "main is not the default"
    );
    assert!(
        refusal(REPO, Some("main"), Some(&on_main()), Some(&run), None).is_some(),
        "no word from the guest"
    );
    let on = |run: Option<&WorkflowRun>| refusal(REPO, Some("main"), Some(&on_main()), run, end);
    assert!(on(None).is_some(), "the run was not read");
    for head_branch in [None, Some("feature"), Some("Main"), Some("main2"), Some("")] {
        let run = WorkflowRun {
            head_branch: head_branch.map(str::to_string),
            ..main_run()
        };
        assert!(
            on(Some(&run)).is_some(),
            "a reusable workflow at main run from {head_branch:?}"
        );
    }
    // A tag named `main` that calls main's reusable workflow: GitHub names the tag as the run's branch.
    let tag = WorkflowRun {
        on_default_branch: false,
        ..main_run()
    };
    assert_eq!(
        on(Some(&tag)),
        Some("the run's commit is not on the default branch"),
        "a tag named like the default branch"
    );
    for head_repository in [
        None,
        Some("someone/shop"),
        Some("codefusion-cc/shop-fork"),
        Some(""),
    ] {
        let run = WorkflowRun {
            head_repository: head_repository.map(str::to_string),
            ..main_run()
        };
        assert!(on(Some(&run)).is_some(), "code of {head_repository:?}");
    }
    for event in ["pull_request", "workflow_call", ""] {
        let run = WorkflowRun {
            event: event.into(),
            ..main_run()
        };
        assert!(on(Some(&run)).is_some(), "the run's event {event}");
    }
    // A branch whose name holds `@` and a ref: its workflow's ref would end in the default branch.
    let tricked = GithubJob {
        workflow: "codefusion-cc/shop/.github/workflows/ci.yml@refs/heads/x@refs/heads/main".into(),
        ..on_main()
    };
    assert!(
        refusal(REPO, Some("main"), Some(&tricked), Some(&run), end).is_some(),
        "two refs"
    );
    let tricked_run = WorkflowRun {
        head_branch: Some("x@refs/heads/main".into()),
        ..main_run()
    };
    assert!(
        refusal(REPO, Some("main"), Some(&tricked), Some(&tricked_run), end).is_some(),
        "two refs, and the run's own branch"
    );
    for (clean, docker) in [
        (false, None),
        (false, Some(true)),
        (true, Some(false)),
        (false, Some(false)),
    ] {
        let end = Some(CacheEnd {
            found: CacheFound::Reused,
            clean,
            docker,
        });
        assert!(
            refusal(REPO, Some("main"), Some(&on_main()), Some(&run), end).is_some(),
            "{clean} {docker:?}"
        );
    }
    // A clean disk is kept with or without Docker, as long as a Docker job's daemon answered.
    for docker in [None, Some(true)] {
        let end = Some(CacheEnd {
            found: CacheFound::Reused,
            clean: true,
            docker,
        });
        assert_eq!(
            refusal(REPO, Some("main"), Some(&on_main()), Some(&run), end),
            None,
            "{docker:?}"
        );
    }
}

proptest! {
    #[test]
    fn nothing_but_a_run_of_the_default_branch_is_ever_kept(
        event in prop_oneof![Just("push".to_string()), Just("pull_request".to_string()), "[a-z_]{0,20}"],
        branch in prop_oneof![Just("main".to_string()), "[a-z/-]{0,12}"],
        repo in prop_oneof![Just(REPO.to_string()), Just("CODEFUSION-CC/SHOP".to_string()), "[a-z-]{1,8}/[a-z-]{1,8}"],
        result in prop_oneof![Just("succeeded".to_string()), "[a-z]{0,10}"],
        clean in any::<bool>(),
        docker in prop_oneof![Just(None), Just(Some(true)), Just(Some(false))],
        head in prop_oneof![Just("main".to_string()), "[a-z@/-]{0,12}"],
        read in any::<bool>(),
        on_default_branch in any::<bool>(),
    ) {
        let job = github(&event, &format!("{repo}/.github/workflows/x.yml@refs/heads/{branch}"), Some(&result));
        let end = Some(CacheEnd { found: CacheFound::Reused, clean, docker });
        let run = WorkflowRun {
            head_branch: Some(head.clone()),
            head_repository: Some(repo.clone()),
            event: event.clone(),
            head_sha: None,
            on_default_branch,
        };
        if refusal(REPO, Some("main"), Some(&job), read.then_some(&run), end).is_none() {
            prop_assert!(read);
            prop_assert!(on_default_branch);
            prop_assert_eq!(head, "main");
            prop_assert!(["push", "schedule", "workflow_dispatch"].contains(&event.as_str()));
            prop_assert_eq!(branch, "main");
            prop_assert!(repo.eq_ignore_ascii_case(REPO));
            prop_assert!(result == "succeeded" || result == "success");
            prop_assert!(clean && docker != Some(false));
        }
    }

    #[test]
    fn one_repository_one_key_two_repositories_two(a in "[A-Za-z0-9-]{1,10}/[A-Za-z0-9._-]{1,10}", b in "[A-Za-z0-9-]{1,10}/[A-Za-z0-9._-]{1,10}") {
        prop_assert_eq!(key(&a), key(&a.to_uppercase()));
        prop_assert_eq!(key(&a), key(&a.to_lowercase()));
        if !a.eq_ignore_ascii_case(&b) {
            prop_assert_ne!(key(&a), key(&b));
        }
        prop_assert_eq!(key(&a).len(), 32);
        prop_assert!(key(&a).bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}

#[test]
fn a_cache_is_dropped_only_when_its_daemon_could_not_run_on_it() {
    let end = |found, docker| {
        Some(CacheEnd {
            found,
            clean: true,
            docker,
        })
    };
    assert!(spoiled(end(CacheFound::Reused, Some(false))));
    assert!(!spoiled(end(CacheFound::Reused, Some(true))));
    assert!(
        !spoiled(end(CacheFound::Reused, None)),
        "a job without Docker has no daemon to fail"
    );
    assert!(
        !spoiled(end(CacheFound::Fresh, Some(false))),
        "a fresh one held nothing to blame"
    );
    assert!(!spoiled(end(CacheFound::Replaced, Some(false))));
    assert!(!spoiled(None));
}

fn cache(bytes: u64, used_at: i64) -> Cache {
    Cache {
        repo: "r".into(),
        bytes,
        written_at: 0,
        used_at,
    }
}

#[test]
fn the_least_recently_used_go_first_until_the_rest_fit_and_none_in_use() {
    let caches = vec![
        ("a".to_string(), cache(10, 3)),
        ("b".to_string(), cache(10, 1)),
        ("c".to_string(), cache(10, 2)),
    ];
    let none = BTreeSet::new();
    assert_eq!(evictions(&caches, 30, &none), Vec::<String>::new());
    assert_eq!(evictions(&caches, 29, &none), ["b"]);
    assert_eq!(evictions(&caches, 10, &none), ["b", "c"]);
    assert_eq!(evictions(&caches, 0, &none), ["b", "c", "a"]);
    let busy: BTreeSet<String> = ["b".to_string()].into();
    assert_eq!(
        evictions(&caches, 10, &busy),
        ["c", "a"],
        "b is in use; the rest go"
    );
    assert_eq!(evictions(&[], 0, &none), Vec::<String>::new());
}

proptest! {
    #[test]
    fn what_stays_fits_unless_only_caches_in_use_are_left(
        sizes in proptest::collection::vec((0u64..100, 0i64..50, any::<bool>()), 0..12),
        budget in 0u64..400,
    ) {
        let caches: Vec<(String, Cache)> =
            sizes.iter().enumerate().map(|(i, (bytes, used, _))| (format!("k{i:02}"), cache(*bytes, *used))).collect();
        let in_use: BTreeSet<String> =
            sizes.iter().enumerate().filter(|(_, (.., busy))| *busy).map(|(i, _)| format!("k{i:02}")).collect();
        let gone = evictions(&caches, budget, &in_use);
        prop_assert!(gone.iter().all(|key| !in_use.contains(key)));
        let left: u64 = caches.iter().filter(|(key, _)| !gone.contains(key)).map(|(_, c)| c.bytes).sum();
        let busy: u64 = caches.iter().filter(|(key, _)| in_use.contains(key)).map(|(_, c)| c.bytes).sum();
        prop_assert!(left <= budget.max(busy));
        // Nothing went that was used more recently than one that stayed and could have gone.
        for (key, kept) in caches.iter().filter(|(key, _)| !gone.contains(key) && !in_use.contains(key)) {
            for went in caches.iter().filter(|(k, _)| gone.contains(k)) {
                prop_assert!((went.1.used_at, &went.0) <= (kept.used_at, key));
            }
        }
    }
}

/// A store keeping `budget` bytes in a folder of the test's own, or none where its file system cannot share blocks.
fn store(budget: u64) -> Option<(TempDir, CacheStore)> {
    let dir = cache_folder()?;
    let caches = CacheStore::open(dir.path(), budget).expect("a store");
    Some((dir, caches))
}

fn empty(path: &Path) -> bool {
    head(path).iter().all(|byte| *byte == 0)
}

/// A job of `repo` that writes `bytes` to its copy, ends cleanly and is kept.
fn keep(caches: &mut CacheStore, repo: &str, job: &str, at: i64, bytes: &[u8]) -> CacheCopy {
    let mut copy = caches.prepare(job, repo, Some("main"), at).expect("a copy");
    fs::write(&copy.path, bytes).unwrap();
    copy.end = clean(CacheFound::Fresh);
    caches.settle(repo, &copy, None, at);
    copy
}

fn kinds(caches: &CacheStore) -> Vec<&'static str> {
    caches.events().map(|event| event.kind.name()).collect()
}

#[test]
fn a_kept_copy_is_what_the_next_job_of_the_repository_starts_from() {
    let Some((_dir, mut caches)) = store(100 << 30) else {
        return;
    };
    let first = caches.prepare("job1", REPO, Some("main"), 10).expect("a copy");
    assert_eq!(
        fs::metadata(&first.path).unwrap().len(),
        DISK_BYTES,
        "a first one is empty, of the full size"
    );
    assert!(empty(&first.path));
    assert_eq!(first.default_branch.as_deref(), Some("main"));
    fs::write(&first.path, b"layers of the first job").unwrap();
    caches.settle(
        REPO,
        &CacheCopy {
            end: clean(CacheFound::Fresh),
            ..first.clone()
        },
        None,
        20,
    );
    assert!(!first.path.exists(), "the copy became the cache");
    let kept = caches.list();
    assert_eq!(kept.len(), 1);
    assert_eq!((kept[0].1.repo.as_str(), kept[0].1.written_at), (REPO, 20));
    assert!(kept[0].1.bytes > 0, "its size on the host's disk");

    let second = caches
        .prepare("job2", "CodeFusion-CC/Shop", None, 30)
        .expect("a copy");
    assert_eq!(
        head(&second.path),
        b"layers of the first job",
        "the same repository, any case"
    );
    assert_eq!(caches.list()[0].1.used_at, 30);
    let other = caches
        .prepare("job3", "codefusion-cc/blog", None, 30)
        .expect("a copy");
    assert!(empty(&other.path), "another repository never gets it");
}

#[test]
fn a_refused_copy_is_thrown_away_and_the_cache_stays_as_it_was() {
    let Some((_dir, mut caches)) = store(100 << 30) else {
        return;
    };
    keep(&mut caches, REPO, "job1", 20, b"main's layers");
    let mut pull_request = caches.prepare("job2", REPO, Some("main"), 30).unwrap();
    fs::write(&pull_request.path, b"a pull request's layers").unwrap();
    pull_request.end = clean(CacheFound::Reused);
    caches.settle(REPO, &pull_request, Some("only a push"), 40);
    assert!(!pull_request.path.exists());
    let next = caches.prepare("job3", REPO, Some("main"), 50).unwrap();
    assert_eq!(head(&next.path), b"main's layers");
    assert_eq!(kinds(&caches), ["kept", "discarded"]);
    assert_eq!(
        caches.events().last().unwrap().why.as_deref(),
        Some("only a push")
    );
}

#[test]
fn a_cache_its_daemon_could_not_run_on_goes_and_the_next_job_starts_empty() {
    let Some((_dir, mut caches)) = store(100 << 30) else {
        return;
    };
    keep(&mut caches, REPO, "job1", 20, b"broken");
    let mut second = caches.prepare("job2", REPO, Some("main"), 30).unwrap();
    second.end = Some(CacheEnd {
        found: CacheFound::Reused,
        clean: true,
        docker: Some(false),
    });
    caches.settle(REPO, &second, Some(DAEMON_SILENT), 40);
    assert!(caches.list().is_empty());
    assert_eq!(kinds(&caches), ["kept", "dropped"]);
    let third = caches.prepare("job3", REPO, Some("main"), 50).unwrap();
    assert!(empty(&third.path));
}

#[test]
fn only_a_writer_drops_a_cache_and_only_the_one_it_started_from() {
    let Some((_dir, mut caches)) = store(100 << 30) else {
        return;
    };
    let silent = Some(CacheEnd {
        found: CacheFound::Reused,
        clean: true,
        docker: Some(false),
    });
    keep(&mut caches, REPO, "job1", 20, b"v1");
    // A pull request kills its own daemon: refused for its event, the cache stays.
    let mut pull = caches.prepare("pull", REPO, Some("main"), 30).unwrap();
    pull.end = silent;
    caches.settle(
        REPO,
        &pull,
        Some("only a push, a schedule or a dispatch writes the cache"),
        31,
    );
    assert_eq!(caches.list().len(), 1, "a pull request drops nothing");
    // A writer started from v1 while another kept v2: v2 was never tried, and stays.
    let mut late = caches.prepare("late", REPO, Some("main"), 40).unwrap();
    late.end = silent;
    keep(&mut caches, REPO, "newer", 50, b"v2");
    caches.settle(REPO, &late, Some(DAEMON_SILENT), 60);
    assert_eq!(caches.list().len(), 1, "the newer cache stays");
    let next = caches.prepare("next", REPO, Some("main"), 70).unwrap();
    assert_eq!(head(&next.path), b"v2");
    assert_eq!(kinds(&caches), ["kept", "discarded", "kept", "discarded"]);
}

#[test]
fn a_disk_the_guest_replaced_is_told() {
    let Some((_dir, mut caches)) = store(100 << 30) else {
        return;
    };
    let mut copy = caches.prepare("job1", REPO, Some("main"), 10).unwrap();
    copy.end = clean(CacheFound::Replaced);
    caches.settle(REPO, &copy, Some("only a push"), 20);
    assert_eq!(kinds(&caches), ["replaced", "discarded"]);
}

#[test]
fn a_cache_whose_record_names_another_repository_is_never_used() {
    let Some((dir, mut caches)) = store(100 << 30) else {
        return;
    };
    keep(&mut caches, REPO, "job1", 20, b"the shop's layers");
    // The record says another repository, under this repository's key: as if two names shared a key.
    let record = dir.path().join(format!("{}.json", key(REPO)));
    let text = fs::read_to_string(&record).unwrap().replace(REPO, "someone/else");
    fs::write(&record, text).unwrap();
    let mut caches = CacheStore::open(dir.path(), 100 << 30).unwrap();
    let copy = caches.prepare("job2", REPO, Some("main"), 30).unwrap();
    assert!(empty(&copy.path));
    assert!(caches.list().is_empty(), "the stray cache is deleted, not kept");
    assert!(!record.exists());
}

#[test]
fn over_the_budget_the_one_used_longest_ago_goes_and_one_in_use_stays() {
    // Each cache holds about 1 MB on disk; the budget fits two.
    let Some((_dir, mut caches)) = store((5 << 20) / 2) else {
        return;
    };
    let layers = vec![7u8; 1 << 20];
    keep(&mut caches, "o/a", "j1", 10, &layers);
    keep(&mut caches, "o/b", "j2", 20, &layers);
    // A job runs with a copy of o/a, which stays the one used longest ago.
    let busy = caches.prepare("busy", "o/a", Some("main"), 15).unwrap();
    keep(&mut caches, "o/c", "j3", 30, &layers);
    let left: Vec<&str> = caches
        .list()
        .iter()
        .map(|(_, cache)| cache.repo.as_str())
        .collect();
    assert!(
        left.contains(&"o/a") && left.contains(&"o/c") && left.len() == 2,
        "in use, so o/a stays though oldest: {left:?}"
    );
    let last = caches.events().last().unwrap();
    assert_eq!((last.kind.name(), last.repo.as_str()), ("evicted", "o/b"));

    // Once the job is over its cache may go like any other.
    caches.settle("o/a", &busy, Some("only a push"), 40);
    keep(&mut caches, "o/d", "j4", 50, &layers);
    let left: Vec<&str> = caches
        .list()
        .iter()
        .map(|(_, cache)| cache.repo.as_str())
        .collect();
    assert!(
        left.contains(&"o/c") && left.contains(&"o/d") && left.len() == 2,
        "{left:?}"
    );
}

#[test]
fn a_copy_whose_machine_never_started_is_deleted_and_holds_its_cache_no_longer() {
    let Some((_dir, mut caches)) = store((5 << 20) / 2) else {
        return;
    };
    let layers = vec![7u8; 1 << 20];
    keep(&mut caches, "o/a", "j1", 10, &layers);
    let never = caches.prepare("never", "o/a", Some("main"), 10).unwrap();
    caches.discard(&never);
    assert!(!never.path.exists());
    keep(&mut caches, "o/b", "j2", 20, &layers);
    keep(&mut caches, "o/c", "j3", 30, &layers);
    let last = caches.events().last().unwrap();
    assert_eq!((last.kind.name(), last.repo.as_str()), ("evicted", "o/a"));
}

#[test]
fn copies_nothing_knows_are_swept_and_known_ones_stay_in_use() {
    let Some((dir, mut caches)) = store((5 << 20) / 2) else {
        return;
    };
    let layers = vec![7u8; 1 << 20];
    keep(&mut caches, "o/a", "j1", 10, &layers);
    let known = caches.prepare("job1", "o/a", Some("main"), 10).unwrap();
    let stray = caches.prepare("job2", "o/b", Some("main"), 10).unwrap();
    // A Runner that starts again knows its copies from its records.
    let mut caches = CacheStore::open(dir.path(), (5 << 20) / 2).unwrap();
    caches.sweep([&known]);
    assert!(known.path.exists());
    assert!(!stray.path.exists());
    keep(&mut caches, "o/b", "j2", 20, &layers);
    keep(&mut caches, "o/c", "j3", 30, &layers);
    let last = caches.events().last().unwrap();
    assert_eq!(
        (last.kind.name(), last.repo.as_str()),
        ("evicted", "o/b"),
        "o/a is in use by the known copy"
    );
}

#[test]
fn a_store_reads_back_what_it_kept() {
    let Some((dir, mut caches)) = store(100 << 30) else {
        return;
    };
    keep(&mut caches, REPO, "job1", 20, b"kept");
    let again = CacheStore::open(dir.path(), 100 << 30).unwrap();
    assert_eq!(again.list(), caches.list());
}

#[test]
fn no_budget_no_store_and_no_caches_left() {
    let Some((dir, mut caches)) = store(100 << 30) else {
        return;
    };
    let kept = keep(&mut caches, REPO, "job1", 20, b"kept");
    assert!(CacheStore::open(dir.path(), 0).is_none());
    assert!(!dir.path().join(format!("{}.img", kept.key)).exists());
    assert!(!dir.path().exists(), "the folder is gone with what it held");
}

#[test]
fn only_a_job_github_has_not_refused_already_has_its_run_read() {
    assert!(cf_daemon::caches::worth_reading(REPO, Some("main"), &on_main()));
    let refused = [
        github(
            "pull_request",
            "codefusion-cc/shop/.github/workflows/ci.yml@refs/pull/3/merge",
            None,
        ),
        github(
            "push",
            "codefusion-cc/shop/.github/workflows/ci.yml@refs/heads/feature",
            None,
        ),
        github(
            "push",
            "other/tools/.github/workflows/ci.yml@refs/heads/main",
            None,
        ),
    ];
    for job in &refused {
        assert!(
            !cf_daemon::caches::worth_reading(REPO, Some("main"), job),
            "{}",
            job.workflow
        );
    }
    assert!(
        !cf_daemon::caches::worth_reading(REPO, None, &on_main()),
        "no default branch known"
    );
    // How the job ends is not known while it runs: a job still running is worth reading.
    let running = github(
        "push",
        "codefusion-cc/shop/.github/workflows/ci.yml@refs/heads/main",
        None,
    );
    assert!(cf_daemon::caches::worth_reading(REPO, Some("main"), &running));
}

#[test]
fn a_store_that_cannot_share_blocks_now_deletes_the_copies_left_behind() {
    let Some(dir) = cache_folder() else { return };
    let mut caches = CacheStore::open(dir.path(), 100 << 30).unwrap();
    let left = caches.prepare("job1", REPO, Some("main"), 10).unwrap();
    fs::write(&left.path, b"a copy").unwrap();
    drop(caches);
    // The probe cannot be written, as on a full disk.
    fs::create_dir(dir.path().join(".clone-probe")).unwrap();
    assert!(CacheStore::open(dir.path(), 100 << 30).is_none());
    assert!(!left.path.exists(), "its room comes back");
    fs::remove_dir(dir.path().join(".clone-probe")).unwrap();
    assert!(
        CacheStore::open(dir.path(), 100 << 30).is_some(),
        "and the store opens once it can"
    );
}

#[test]
fn an_image_no_record_names_is_deleted_when_the_store_opens() {
    let Some(dir) = cache_folder() else { return };
    let mut caches = CacheStore::open(dir.path(), 100 << 30).unwrap();
    keep(&mut caches, REPO, "job1", 10, b"kept");
    drop(caches);
    // A Runner stopped between keeping a copy and writing its record.
    let stray = dir.path().join(format!("{}.img", key("codefusion-cc/other")));
    fs::write(&stray, vec![1; 1 << 20]).unwrap();
    let caches = CacheStore::open(dir.path(), 100 << 30).unwrap();
    assert!(!stray.exists(), "nothing would ever count or evict it");
    assert_eq!(caches.list().len(), 1, "the cache with its record stays");
}

#[test]
fn a_cache_that_cannot_be_copied_goes_and_the_job_starts_empty() {
    use std::os::unix::fs::PermissionsExt as _;
    let Some((dir, mut caches)) = store(100 << 30) else {
        return;
    };
    keep(&mut caches, REPO, "job1", 10, b"kept");
    let image = dir.path().join(format!("{}.img", key(REPO)));
    fs::set_permissions(&image, fs::Permissions::from_mode(0o000)).unwrap();
    let copy = caches
        .prepare("job2", REPO, Some("main"), 20)
        .expect("an empty copy");
    assert_eq!(copy.from, None);
    assert!(empty(&copy.path));
    assert!(caches.list().is_empty(), "never tried again");
    assert!(!image.exists());
}
