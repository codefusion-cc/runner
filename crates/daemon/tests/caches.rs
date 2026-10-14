//! Docker jobs' caches: one per repository, written back only by a successful run of the repository's own workflow
//! on its default branch whose disk was let go cleanly, under a budget the least recently used cache leaves first.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use cf_daemon::GithubJob;
use cf_daemon::caches::{Cache, CacheStore, DISK_BYTES, evictions, key, refusal, spoiled};
use cf_guest_proto::{CacheEnd, CacheFound};
use proptest::prelude::*;

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

/// A store in a folder of its own, or none where the file system cannot share blocks (as on most Linux CI disks).
fn store(dir: &Path, budget: u64) -> Option<CacheStore> {
    let store = CacheStore::open(dir, budget);
    if store.is_none() {
        eprintln!("skipped: {} cannot share blocks between files", dir.display());
    }
    store
}

/// The first 64 KB of a copy: all a test needs, of a file that may be 20 GB of holes.
fn head(path: &Path) -> Vec<u8> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    fs::File::open(path)
        .unwrap()
        .take(1 << 16)
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
}

fn empty(path: &Path) -> bool {
    head(path).iter().all(|byte| *byte == 0)
}

fn ended(found: CacheFound) -> Option<CacheEnd> {
    clean(found)
}

#[test]
fn a_kept_copy_is_what_the_next_job_of_the_repository_starts_from() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut caches) = store(dir.path(), 100 << 30) else {
        return;
    };
    let none = BTreeSet::new();
    let first = caches.prepare("job1", REPO, 10).expect("a copy");
    assert_eq!(
        fs::metadata(&first.path).unwrap().len(),
        DISK_BYTES,
        "a first one is empty, of the full size"
    );
    assert!(empty(&first.path));
    fs::write(&first.path, b"layers of the first job").unwrap();
    caches.settle(REPO, &first, None, ended(CacheFound::Fresh), &none, 20);
    assert!(!first.path.exists(), "the copy became the cache");
    let kept = caches.list();
    assert_eq!(kept.len(), 1);
    assert_eq!((kept[0].1.repo.as_str(), kept[0].1.written_at), (REPO, 20));

    let second = caches.prepare("job2", "CodeFusion-CC/Shop", 30).expect("a copy");
    assert_eq!(
        head(&second.path),
        b"layers of the first job",
        "the same repository, any case"
    );
    let other = caches.prepare("job3", "codefusion-cc/blog", 30).expect("a copy");
    assert!(empty(&other.path), "another repository never gets it");
}

#[test]
fn a_refused_copy_is_thrown_away_and_the_cache_stays_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut caches) = store(dir.path(), 100 << 30) else {
        return;
    };
    let none = BTreeSet::new();
    let first = caches.prepare("job1", REPO, 10).unwrap();
    fs::write(&first.path, b"main's layers").unwrap();
    caches.settle(REPO, &first, None, ended(CacheFound::Fresh), &none, 20);

    let pull_request = caches.prepare("job2", REPO, 30).unwrap();
    fs::write(&pull_request.path, b"a pull request's layers").unwrap();
    caches.settle(
        REPO,
        &pull_request,
        Some("only a push"),
        ended(CacheFound::Reused),
        &none,
        40,
    );
    assert!(!pull_request.path.exists());
    let next = caches.prepare("job3", REPO, 50).unwrap();
    assert_eq!(head(&next.path), b"main's layers");
    let events = caches.events();
    let kinds: Vec<&str> = events.iter().map(|event| event.kind.as_str()).collect();
    assert_eq!(kinds, ["kept", "discarded"]);
    assert_eq!(events[1].why.as_deref(), Some("only a push"));
}

#[test]
fn a_cache_its_daemon_could_not_run_on_goes_and_the_next_job_starts_empty() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut caches) = store(dir.path(), 100 << 30) else {
        return;
    };
    let none = BTreeSet::new();
    let first = caches.prepare("job1", REPO, 10).unwrap();
    fs::write(&first.path, b"broken").unwrap();
    caches.settle(REPO, &first, None, ended(CacheFound::Fresh), &none, 20);
    let second = caches.prepare("job2", REPO, 30).unwrap();
    let dead = Some(CacheEnd {
        found: CacheFound::Reused,
        clean: true,
        daemon: false,
    });
    caches.settle(REPO, &second, Some("the daemon did not answer"), dead, &none, 40);
    assert!(caches.list().is_empty());
    assert_eq!(caches.events().last().unwrap().kind, "dropped");
    let third = caches.prepare("job3", REPO, 50).unwrap();
    assert!(empty(&third.path));
}

#[test]
fn a_cache_whose_record_names_another_repository_is_never_used() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut caches) = store(dir.path(), 100 << 30) else {
        return;
    };
    let none = BTreeSet::new();
    let first = caches.prepare("job1", REPO, 10).unwrap();
    fs::write(&first.path, b"the shop's layers").unwrap();
    caches.settle(REPO, &first, None, ended(CacheFound::Fresh), &none, 20);
    // The record says another repository, under this repository's key: as if two names shared a key.
    let record = dir.path().join(format!("{}.json", key(REPO)));
    let text = fs::read_to_string(&record).unwrap().replace(REPO, "someone/else");
    fs::write(&record, text).unwrap();
    let copy = caches.prepare("job2", REPO, 30).unwrap();
    assert!(empty(&copy.path));
    assert!(caches.list().is_empty(), "the stray cache is deleted, not kept");
}

#[test]
fn over_the_budget_the_one_used_longest_ago_goes_and_one_in_use_stays() {
    let dir = tempfile::tempdir().unwrap();
    // Each cache holds about 1 MB on disk; the budget fits two.
    let Some(mut caches) = store(dir.path(), (5 << 20) / 2) else {
        return;
    };
    let none = BTreeSet::new();
    let keep = |caches: &mut CacheStore, repo: &str, job: &str, at: i64, busy: &BTreeSet<String>| {
        let copy = caches.prepare(job, repo, at).unwrap();
        fs::write(&copy.path, vec![7u8; 1 << 20]).unwrap();
        caches.settle(repo, &copy, None, ended(CacheFound::Fresh), busy, at);
    };
    keep(&mut caches, "o/a", "j1", 10, &none);
    keep(&mut caches, "o/b", "j2", 20, &none);
    let busy: BTreeSet<String> = [key("o/a")].into();
    keep(&mut caches, "o/c", "j3", 30, &busy);
    let left: Vec<String> = caches.list().into_iter().map(|(_, cache)| cache.repo).collect();
    assert_eq!(left.len(), 2);
    assert!(
        left.contains(&"o/a".to_string()),
        "in use, so it stays though oldest: {left:?}"
    );
    assert!(left.contains(&"o/c".to_string()));
    assert_eq!(caches.events().last().unwrap().kind, "evicted");
    assert_eq!(caches.events().last().unwrap().repo, "o/b");
}

#[test]
fn copies_nothing_knows_are_swept_and_known_ones_stay() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut caches) = store(dir.path(), 100 << 30) else {
        return;
    };
    let known = caches.prepare("job1", REPO, 10).unwrap();
    let stray = caches.prepare("job2", REPO, 10).unwrap();
    caches.sweep(&[known.path.clone()].into());
    assert!(known.path.exists());
    assert!(!stray.path.exists());
}

#[test]
fn no_budget_no_store() {
    let dir = tempfile::tempdir().unwrap();
    assert!(CacheStore::open(dir.path(), 0).is_none());
}
