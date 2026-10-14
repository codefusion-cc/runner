//! Docker jobs' caches: one per repository, written back only by a successful run of the repository's own workflow
//! on its default branch whose disk was let go cleanly, under a budget the least recently used cache leaves first.

mod support;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use cf_daemon::GithubJob;
use cf_daemon::caches::{Cache, CacheCopy, CacheStore, DISK_BYTES, evictions, key, refusal, spoiled};
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

fn clean(found: CacheFound) -> Option<CacheEnd> {
    Some(CacheEnd {
        found,
        clean: true,
        daemon: true,
    })
}

#[test]
fn a_successful_push_to_the_default_branch_with_a_clean_disk_is_kept() {
    assert_eq!(
        refusal(REPO, Some("main"), Some(&on_main()), clean(CacheFound::Fresh)),
        None
    );
    for event in ["schedule", "workflow_dispatch"] {
        let job = GithubJob {
            event: event.into(),
            ..on_main()
        };
        assert_eq!(
            refusal(REPO, Some("main"), Some(&job), clean(CacheFound::Reused)),
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
        refusal(REPO, Some("main"), Some(&spelled), clean(CacheFound::Reused)),
        None,
        "GitHub's names ignore case"
    );
}

#[test]
fn each_condition_alone_refuses() {
    let end = clean(CacheFound::Reused);
    let with = |job: GithubJob| refusal(REPO, Some("main"), Some(&job), end);
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
        refusal(REPO, Some("main"), None, end).is_some(),
        "no word from GitHub"
    );
    assert!(
        refusal(REPO, None, Some(&on_main()), end).is_some(),
        "no default branch known"
    );
    assert!(
        refusal(REPO, Some(""), Some(&on_main()), end).is_some(),
        "an empty default branch"
    );
    assert!(
        refusal(REPO, Some("develop"), Some(&on_main()), end).is_some(),
        "main is not the default"
    );
    assert!(
        refusal(REPO, Some("main"), Some(&on_main()), None).is_some(),
        "no word from the guest"
    );
    for (clean, daemon) in [(false, true), (true, false), (false, false)] {
        let end = Some(CacheEnd {
            found: CacheFound::Reused,
            clean,
            daemon,
        });
        assert!(
            refusal(REPO, Some("main"), Some(&on_main()), end).is_some(),
            "{clean} {daemon}"
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
        daemon in any::<bool>(),
    ) {
        let job = github(&event, &format!("{repo}/.github/workflows/x.yml@refs/heads/{branch}"), Some(&result));
        let end = Some(CacheEnd { found: CacheFound::Reused, clean, daemon });
        if refusal(REPO, Some("main"), Some(&job), end).is_none() {
            prop_assert!(["push", "schedule", "workflow_dispatch"].contains(&event.as_str()));
            prop_assert_eq!(branch, "main");
            prop_assert!(repo.eq_ignore_ascii_case(REPO));
            prop_assert!(result == "succeeded" || result == "success");
            prop_assert!(clean && daemon);
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
    let end = |found, daemon| {
        Some(CacheEnd {
            found,
            clean: true,
            daemon,
        })
    };
    assert!(spoiled(end(CacheFound::Reused, false)));
    assert!(!spoiled(end(CacheFound::Reused, true)));
    assert!(
        !spoiled(end(CacheFound::Fresh, false)),
        "a fresh one held nothing to blame"
    );
    assert!(!spoiled(end(CacheFound::Replaced, false)));
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
        daemon: false,
    });
    caches.settle(REPO, &second, Some("the daemon did not answer"), 40);
    assert!(caches.list().is_empty());
    assert_eq!(kinds(&caches), ["kept", "dropped"]);
    let third = caches.prepare("job3", REPO, Some("main"), 50).unwrap();
    assert!(empty(&third.path));
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
