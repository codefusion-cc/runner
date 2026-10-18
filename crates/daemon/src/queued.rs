//! The label sets of queued jobs that no workflow on a repository's default branch names: a pull request's new
//! workflow or labels. Each is served for a while after a job of it was last seen queued or running, and only so
//! many are: what a pull request can make the Runner serve is bounded.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime};

use cf_budget::Size;
use cf_workflows::LabelSet;

/// GitHub fails a job no Actions runner took within 24 hours: a set seen no later than that may still have one
/// waiting, though its repository's queue could not be read since.
pub(crate) const LONGEST_WAIT: Duration = Duration::from_secs(24 * 60 * 60);

/// A label set queued jobs asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueuedSet {
    /// `owner/name`.
    pub repo: String,
    pub set: LabelSet,
    pub size: Size,
    /// When a job of it was last seen queued or running.
    pub last_seen: SystemTime,
    /// The newest job of it seen: when it was queued (seconds since 1970) and its ID.
    pub newest: (i64, i64),
}

/// How long and how many sets of queued jobs are served.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Bounds {
    /// How long a set is served after a job of it was last seen.
    pub keep: Duration,
    /// The most sets of one repository.
    pub per_repo: usize,
    /// The most sets of the host.
    pub per_host: usize,
}

/// What a selection is made from beside the sets: the state of the last look.
pub(crate) struct Look<'a> {
    pub now: SystemTime,
    /// The repositories the Runner serves.
    pub served: &'a BTreeSet<String>,
    /// The repositories whose queue (or workflows) could not be read: nothing is concluded about them, so their
    /// sets are kept for as long as a job may wait at GitHub, [`LONGEST_WAIT`].
    pub unread: &'a BTreeSet<String>,
    /// The sets the default branches' workflows name: those are served as theirs.
    pub main: &'a BTreeSet<String>,
}

/// The sets of queued jobs, by scale set name.
#[derive(Debug, Default)]
pub(crate) struct Queued {
    sets: BTreeMap<String, QueuedSet>,
}

impl Queued {
    /// A job of `set` was seen: the set is served from now on, as long as the bounds let it.
    pub fn seen(&mut self, name: String, set: QueuedSet) {
        self.sets
            .entry(name)
            .and_modify(|known| {
                known.last_seen = known.last_seen.max(set.last_seen);
                known.newest = known.newest.max(set.newest);
            })
            .or_insert(set);
    }

    /// Keeps the sets that are served: of a repository the Runner serves, not one main names, seen within
    /// [`Bounds::keep`] (or of a repository that could not be read), and within the bounds, the newest first. The
    /// rest are forgotten. Says what the bounds left out.
    pub fn select(&mut self, look: &Look, bounds: Bounds) -> Vec<String> {
        self.sets.retain(|name, q| {
            let of_ours = look.served.contains(&q.repo) || look.unread.contains(&q.repo);
            // A clock that went back reads as just seen.
            let since = look.now.duration_since(q.last_seen).unwrap_or_default();
            let keep = if look.unread.contains(&q.repo) {
                bounds.keep.max(LONGEST_WAIT)
            } else {
                bounds.keep
            };
            of_ours && !look.main.contains(name) && since <= keep
        });
        let mut order: Vec<(&String, &QueuedSet)> = self.sets.iter().collect();
        order.sort_by(|(a_name, a), (b_name, b)| {
            b.last_seen
                .cmp(&a.last_seen)
                .then(b.newest.cmp(&a.newest))
                .then(a_name.cmp(b_name))
        });
        let mut per_repo: BTreeMap<&str, usize> = BTreeMap::new();
        let mut kept = 0usize;
        let mut over_repo: BTreeMap<String, usize> = BTreeMap::new();
        let mut over_host: BTreeMap<String, usize> = BTreeMap::new();
        let mut dropped: Vec<String> = Vec::new();
        for (name, q) in order {
            let of_repo = per_repo.entry(q.repo.as_str()).or_default();
            if *of_repo >= bounds.per_repo {
                *over_repo.entry(q.repo.clone()).or_default() += 1;
            } else if kept >= bounds.per_host {
                *over_host.entry(q.repo.clone()).or_default() += 1;
            } else {
                *of_repo += 1;
                kept += 1;
                continue;
            }
            dropped.push(name.clone());
        }
        for name in dropped {
            self.sets.remove(&name);
        }
        let mut problems: Vec<String> = over_repo
            .into_iter()
            .map(|(repo, n)| {
                format!(
                    "{repo}: {n} label sets of queued jobs are not served, at most {} a repository are",
                    bounds.per_repo
                )
            })
            .collect();
        problems.extend(over_host.into_iter().map(|(repo, n)| {
            format!(
                "{repo}: {n} label sets of queued jobs are not served, at most {} a host are",
                bounds.per_host
            )
        }));
        problems
    }

    /// Forgets the sets `keep` says no to: ones the host's settings or policy no longer serve.
    pub fn retain(&mut self, mut keep: impl FnMut(&QueuedSet) -> bool) {
        self.sets.retain(|_, q| keep(q));
    }

    /// The sets served, by name.
    pub fn sets(&self) -> &BTreeMap<String, QueuedSet> {
        &self.sets
    }

    /// Forgets every set.
    pub fn clear(&mut self) {
        self.sets.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const BOUNDS: Bounds = Bounds {
        keep: Duration::from_secs(30 * 60),
        per_repo: 8,
        per_host: 32,
    };

    fn at(minutes: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000 + minutes * 60)
    }

    fn set(repo: &str, label: &str, seen: SystemTime, newest: i64) -> (String, QueuedSet) {
        let labels = LabelSet::served(["self-hosted", "cf-2g", label]).expect("served");
        (
            format!("{repo}/{label}"),
            QueuedSet {
                repo: repo.into(),
                set: labels,
                size: Size::G2,
                last_seen: seen,
                newest: (newest, newest),
            },
        )
    }

    fn select(queued: &mut Queued, now: SystemTime, served: &[&str], unread: &[&str]) -> Vec<String> {
        let served: BTreeSet<String> = served.iter().map(|s| s.to_string()).collect();
        let unread: BTreeSet<String> = unread.iter().map(|s| s.to_string()).collect();
        let look = Look {
            now,
            served: &served,
            unread: &unread,
            main: &BTreeSet::from(["org/app/main".to_string()]),
        };
        queued.select(&look, BOUNDS)
    }

    fn names(queued: &Queued) -> Vec<&str> {
        queued.sets().keys().map(String::as_str).collect()
    }

    #[test]
    fn a_set_is_kept_30_minutes_after_it_was_last_seen_and_not_31() {
        let mut queued = Queued::default();
        let (name, s) = set("org/app", "a", at(0), 1);
        queued.seen(name, s);
        assert!(select(&mut queued, at(30), &["org/app"], &[]).is_empty());
        assert_eq!(names(&queued), ["org/app/a"]);
        assert!(select(&mut queued, at(31), &["org/app"], &[]).is_empty());
        assert!(names(&queued).is_empty());

        // Seen again later, the time starts again; a sighting older than one known does not set it back.
        let (name, s) = set("org/app", "a", at(0), 1);
        queued.seen(name.clone(), s.clone());
        queued.seen(
            name.clone(),
            QueuedSet {
                last_seen: at(20),
                ..s.clone()
            },
        );
        queued.seen(
            name,
            QueuedSet {
                last_seen: at(5),
                ..s
            },
        );
        select(&mut queued, at(50), &["org/app"], &[]);
        assert_eq!(names(&queued), ["org/app/a"]);
        select(&mut queued, at(51), &["org/app"], &[]);
        assert!(names(&queued).is_empty());
    }

    #[test]
    fn a_set_of_a_repository_that_could_not_be_read_is_kept_as_long_as_a_job_may_wait() {
        let mut queued = Queued::default();
        let (name, s) = set("org/app", "a", at(0), 1);
        queued.seen(name, s);
        select(&mut queued, at(600), &["org/app"], &["org/app"]);
        assert_eq!(names(&queued), ["org/app/a"]);
        // Nor when its installation's repositories could not be listed, so it is not among the served.
        select(&mut queued, at(24 * 60), &[], &["org/app"]);
        assert_eq!(names(&queued), ["org/app/a"]);
        // No job it was seen for can still wait after 24 hours.
        select(&mut queued, at(24 * 60 + 1), &["org/app"], &["org/app"]);
        assert!(names(&queued).is_empty());
    }

    #[test]
    fn a_set_the_host_no_longer_serves_is_forgotten() {
        let mut queued = Queued::default();
        for (name, s) in [set("org/app", "a", at(0), 1), set("org/app", "b", at(0), 2)] {
            queued.seen(name, s);
        }
        queued.retain(|q| !q.set.contains("b"));
        assert_eq!(names(&queued), ["org/app/a"]);
    }

    #[test]
    fn a_set_of_a_repository_no_longer_served_or_one_main_names_goes() {
        let mut queued = Queued::default();
        for (name, s) in [set("org/app", "a", at(0), 1), set("org/app", "main", at(0), 2)] {
            queued.seen(name, s);
        }
        select(&mut queued, at(1), &["org/app"], &[]);
        assert_eq!(names(&queued), ["org/app/a"], "main's own set is main's");
        select(&mut queued, at(1), &["org/other"], &[]);
        assert!(names(&queued).is_empty());
    }

    #[test]
    fn a_repository_keeps_its_8_newest_sets_and_the_ninth_is_said() {
        let mut queued = Queued::default();
        for n in 0..8 {
            let (name, s) = set("org/app", &format!("s{n}"), at(0), n);
            queued.seen(name, s);
        }
        assert!(select(&mut queued, at(1), &["org/app"], &[]).is_empty());
        assert_eq!(queued.sets().len(), 8);
        let (name, s) = set("org/app", "s8", at(0), 8);
        queued.seen(name, s);
        let problems = select(&mut queued, at(1), &["org/app"], &[]);
        assert_eq!(
            problems,
            ["org/app: 1 label sets of queued jobs are not served, at most 8 a repository are"]
        );
        assert_eq!(queued.sets().len(), 8);
        assert!(
            !queued.sets().contains_key("org/app/s0"),
            "the oldest job's set goes"
        );
        // Seen later beats a newer job seen earlier.
        let (name, s) = set("org/app", "s0", at(1), 0);
        queued.seen(name, s);
        select(&mut queued, at(1), &["org/app"], &[]);
        assert!(queued.sets().contains_key("org/app/s0"));
        assert!(!queued.sets().contains_key("org/app/s1"));
    }

    #[test]
    fn the_host_keeps_its_32_newest_sets_and_the_33rd_is_said() {
        let mut queued = Queued::default();
        let repos: Vec<String> = (0..5).map(|r| format!("org/r{r}")).collect();
        let served: Vec<&str> = repos.iter().map(String::as_str).collect();
        for n in 0..32 {
            let (name, s) = set(&repos[n as usize / 8], &format!("s{n}"), at(0), n);
            queued.seen(name, s);
        }
        assert!(select(&mut queued, at(1), &served, &[]).is_empty());
        assert_eq!(queued.sets().len(), 32);
        let (name, s) = set("org/r4", "s32", at(0), 32);
        queued.seen(name, s);
        let problems = select(&mut queued, at(1), &served, &[]);
        assert_eq!(
            problems,
            ["org/r0: 1 label sets of queued jobs are not served, at most 32 a host are"]
        );
        assert_eq!(queued.sets().len(), 32);
        assert!(queued.sets().contains_key("org/r4/s32"));
        assert!(!queued.sets().contains_key("org/r0/s0"));
    }

    #[test]
    fn a_clock_that_went_back_keeps_a_set() {
        let mut queued = Queued::default();
        let (name, s) = set("org/app", "a", at(100), 1);
        queued.seen(name, s);
        select(&mut queued, at(0), &["org/app"], &[]);
        assert_eq!(names(&queued), ["org/app/a"]);
    }

    proptest! {
        // added: whatever was seen, the bounds hold, the newest are kept, and a second selection changes nothing.
        #[test]
        fn a_selection_keeps_the_bounds_and_the_newest(
            seen in prop::collection::vec((0usize..6, 0usize..20, 0u64..90, 0i64..50), 0..120),
            now in 0u64..120,
        ) {
            let repos: Vec<String> = (0..6).map(|r| format!("org/r{r}")).collect();
            let served: Vec<&str> = repos.iter().take(5).map(String::as_str).collect();
            let mut queued = Queued::default();
            for (repo, label, minute, newest) in &seen {
                let (name, s) = set(&repos[*repo], &format!("s{label}"), at(*minute), *newest);
                queued.seen(name, s);
            }
            let before: BTreeMap<String, QueuedSet> = queued.sets().clone();
            let problems = select(&mut queued, at(now), &served, &[]);
            let kept = queued.sets();
            prop_assert!(kept.len() <= BOUNDS.per_host);
            for repo in &repos {
                prop_assert!(kept.values().filter(|q| &q.repo == repo).count() <= BOUNDS.per_repo);
            }
            for q in kept.values() {
                prop_assert!(served.contains(&q.repo.as_str()));
                prop_assert!(at(now).duration_since(q.last_seen).unwrap_or_default() <= BOUNDS.keep);
            }
            // Nothing that could be served is left out while a less recent one of its repository is kept.
            for (name, q) in &before {
                let eligible = served.contains(&q.repo.as_str())
                    && at(now).duration_since(q.last_seen).unwrap_or_default() <= BOUNDS.keep;
                if eligible && !kept.contains_key(name) {
                    prop_assert!(!problems.is_empty());
                    for k in kept.values().filter(|k| k.repo == q.repo) {
                        prop_assert!((k.last_seen, k.newest) >= (q.last_seen, q.newest));
                    }
                }
            }
            let again = kept.clone();
            prop_assert!(select(&mut queued, at(now), &served, &[]).is_empty());
            prop_assert_eq!(queued.sets(), &again);
        }
    }
}
