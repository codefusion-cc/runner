//! Jobs GitHub holds while a scale set here has room. GitHub hands a queued job to a scale set only when it looks at
//! its queue: when a job is queued, ends or is cancelled, or by itself at moments nobody can predict, which can be
//! many minutes apart. No call to the scale set service makes it look every time (`docs/github-protocol.md`, probes
//! I, J and L). An Actions runner registered outside the scale set that comes online did, within seconds (probe N).
//! So a job that waits while its scale set's polls show room gets an Actions runner of its own: registered for its
//! repository, outside the scale set, with exactly the scale set's labels (probe M). Its arrival makes GitHub look,
//! and GitHub hands the job to the scale set, as in probe N, or to it, since it has the job's labels. Whichever of the
//! two gets the job runs it; the other waits for nothing and asks for its registration's removal, which GitHub
//! answers 204 only for one that ran no job (`docs/job-state.md`).
//!
//! Only reckoning here; the service reads the queue, starts and retires the Actions runners.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

/// The scale set ID of an Actions runner of its own: GitHub numbers its scale sets from 1.
pub const OWN: i64 = 0;

/// How long a job waits, while its scale set here has had room for as long, before it counts as held. GitHub hands
/// a job to a scale set with room within a poll of its next look, and a poll takes some 50 s.
pub const HELD_AFTER: Duration = Duration::from_secs(2 * 60);

/// How often a scale set gets an Actions runner of its own at most.
pub const OWN_EVERY: Duration = Duration::from_secs(5 * 60);

/// How long an Actions runner of its own waits for a job before it asks for its registration's removal. A job
/// assigned to another host's scale set reads as queued until that host's Actions runner takes it, and GitHub takes
/// it back from a scale set that did not within five minutes: it is then this one's to take.
pub const OWN_WAIT: Duration = Duration::from_secs(5 * 60);

/// The most scale sets with held jobs that are kept: those the report can name.
pub use cf_console_client::MAX_HELD_JOBS as MAX_HELD;

/// A scale set this host serves, as one look at its repository's queue saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetView {
    /// The scale set's name.
    pub name: String,
    /// `owner/name`.
    pub repo: String,
    /// Its labels.
    pub labels: Vec<String>,
    /// Since when every poll of its listener offered GitHub room beyond the jobs GitHub counts as assigned to it;
    /// none while the last did not.
    pub room_since: Option<SystemTime>,
    /// The jobs GitHub assigned to it that no Actions runner of it runs yet: GitHub's REST view still calls them
    /// queued.
    pub assigned_unstarted: usize,
    /// The jobs of its labels the REST view calls queued: when each was queued, in seconds since 1970, and its ID,
    /// the oldest first.
    pub queued: Vec<(i64, i64)>,
}

impl SetView {
    /// The queued jobs no scale set of this host was handed: GitHub holds them, or another host has them. GitHub
    /// hands out a scale set's jobs oldest first, so those assigned here are taken to be the oldest.
    pub fn unassigned(&self) -> &[(i64, i64)] {
        &self.queued[self.assigned_unstarted.min(self.queued.len())..]
    }
}

/// The jobs GitHub holds of one scale set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldSet {
    /// The scale set's name.
    pub name: String,
    /// `owner/name`.
    pub repo: String,
    /// Its labels.
    pub labels: Vec<String>,
    /// How many jobs.
    pub jobs: usize,
    /// The oldest job's ID.
    pub oldest: i64,
    /// When the oldest was queued.
    pub queued: SystemTime,
    /// How long the scale set has had room.
    pub room: Duration,
}

/// The scale sets with jobs GitHub holds: a queued job no scale set here was handed waited [`HELD_AFTER`], and every
/// poll of the scale set offered room for as long. The longest waits first, [`MAX_HELD`] at most.
pub fn held(now: SystemTime, sets: &[SetView]) -> Vec<HeldSet> {
    let waited = |queued_at: i64| now.duration_since(crate::time_of(queued_at)).unwrap_or_default();
    let mut held: Vec<HeldSet> = sets
        .iter()
        .filter_map(|set| {
            let room = now.duration_since(set.room_since?).ok()?;
            if room < HELD_AFTER {
                return None;
            }
            // The oldest first: those that waited long enough lead.
            let unassigned = set.unassigned();
            let jobs = unassigned.partition_point(|&(queued_at, _)| waited(queued_at) >= HELD_AFTER);
            let &(queued_at, oldest) = unassigned[..jobs].first()?;
            Some(HeldSet {
                name: set.name.clone(),
                repo: set.repo.clone(),
                labels: set.labels.clone(),
                jobs,
                oldest,
                queued: crate::time_of(queued_at),
                room,
            })
        })
        .collect();
    held.sort_by(|a, b| a.queued.cmp(&b.queued).then_with(|| a.name.cmp(&b.name)));
    held.truncate(MAX_HELD);
    held
}

/// Whether a queued job asks for only some of a scale set's `labels`: `queued` are the repositories and label sets of
/// the jobs queued now. An Actions runner of the set's own carries all of them, so GitHub could give it such a job of
/// its `repo`, which wants a machine of another size.
pub fn narrower_waits<'a>(
    repo: &str,
    labels: &[String],
    queued: impl IntoIterator<Item = (&'a str, &'a [String])>,
) -> bool {
    queued.into_iter().any(|(other_repo, other)| {
        other_repo == repo && other.len() < labels.len() && other.iter().all(|label| labels.contains(label))
    })
}

/// The Actions runners of their own that scale sets got: when each scale set last got one, and its name.
#[derive(Debug, Default)]
pub struct OwnRunners {
    last: BTreeMap<String, (SystemTime, String)>,
}

impl OwnRunners {
    /// Whether the scale set may get an Actions runner of its own now: none in the last [`OWN_EVERY`], and the last
    /// one it got no longer `stands`.
    pub fn due(&self, now: SystemTime, set: &str, stands: impl Fn(&str) -> bool) -> bool {
        self.last.get(set).is_none_or(|(at, runner)| {
            now.duration_since(*at).is_ok_and(|since| since >= OWN_EVERY) && !stands(runner)
        })
    }

    /// The scale set got one now, or tried to: `runner` is its name, empty when none was started.
    pub fn tried(&mut self, now: SystemTime, set: &str, runner: String) {
        self.last.insert(set.to_string(), (now, runner));
    }

    /// The scale set the Actions runner of this name was started for, if this Runner started it.
    pub fn set_of(&self, runner: &str) -> Option<&str> {
        self.last
            .iter()
            .find(|(_, (_, name))| name == runner)
            .map(|(set, _)| set.as_str())
    }

    /// Forgets the scale sets that are not served any more.
    pub fn retain(&mut self, served: impl Fn(&str) -> bool) {
        self.last.retain(|set, _| served(set));
    }
}

/// Whether an Actions runner of its own that waits for a job asks for its registration's removal: it waited
/// [`OWN_WAIT`], or the latest read of its scale set's queue found no job it could take. A job it took reads as
/// running, not queued, and GitHub refuses that removal (`docs/job-state.md`).
pub fn own_done(waited: Duration, unassigned: Option<usize>) -> bool {
    waited >= OWN_WAIT || unassigned == Some(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn at(secs_before_now: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(NOW - secs_before_now)
    }

    fn now() -> SystemTime {
        at(0)
    }

    /// A scale set with `count` queued jobs, the oldest (ID 7) queued `waited` seconds ago and the others a second
    /// apart after it, whose polls offered room for `room` seconds.
    fn set(name: &str, count: i64, waited: u64, room: Option<u64>) -> SetView {
        let first = i64::try_from(NOW - waited).unwrap();
        SetView {
            name: name.into(),
            repo: "org/app".into(),
            labels: vec!["self-hosted".into(), "cf-2g".into()],
            room_since: room.map(at),
            assigned_unstarted: 0,
            queued: (0..count).map(|k| (first + k, 7 + k)).collect(),
        }
    }

    #[test]
    fn a_job_is_held_once_it_and_the_room_waited_two_minutes() {
        let held = held(now(), &[set("a", 1, 120, Some(120))]);
        assert_eq!(
            held,
            [HeldSet {
                name: "a".into(),
                repo: "org/app".into(),
                labels: vec!["self-hosted".into(), "cf-2g".into()],
                jobs: 1,
                oldest: 7,
                queued: at(120),
                room: Duration::from_secs(120),
            }]
        );
    }

    #[test]
    fn a_second_short_of_either_wait_is_not_held() {
        assert!(held(now(), &[set("a", 1, 119, Some(600))]).is_empty());
        assert!(held(now(), &[set("a", 1, 600, Some(119))]).is_empty());
    }

    #[test]
    fn no_room_is_nothing_held_however_long_the_job_waits() {
        assert!(held(now(), &[set("a", 3, 24 * 3600, None)]).is_empty());
    }

    #[test]
    fn jobs_assigned_here_and_not_started_are_not_held() {
        let mut busy = set("a", 2, 600, Some(600));
        busy.assigned_unstarted = 2;
        assert!(held(now(), &[busy.clone()]).is_empty());
        // The oldest is taken to be the one assigned here: the other is held, with its own wait.
        busy.assigned_unstarted = 1;
        let one = &held(now(), &[busy.clone()])[0];
        assert_eq!((one.jobs, one.oldest, one.queued), (1, 8, at(599)));
        // More assigned than the REST view calls queued (it lags): nothing, and no underflow.
        busy.assigned_unstarted = 5;
        assert!(held(now(), &[busy.clone()]).is_empty());
        assert!(busy.unassigned().is_empty());
    }

    #[test]
    fn only_jobs_that_waited_two_minutes_count() {
        // Jobs queued 200 s, 199 s, ... ago: those of 120 s and more.
        let many = set("a", 100, 200, Some(600));
        let found = &held(now(), &[many])[0];
        assert_eq!((found.jobs, found.oldest), (81, 7));
        // An assigned job that waits here leaves a newer one that has not waited long enough: nothing.
        let mut newer = set("a", 2, 600, Some(600));
        newer.queued[1].0 = i64::try_from(NOW - 10).unwrap();
        newer.assigned_unstarted = 1;
        assert!(held(now(), &[newer]).is_empty());
    }

    #[test]
    fn nothing_queued_is_nothing_held() {
        assert!(held(now(), &[set("a", 0, 0, Some(600))]).is_empty());
    }

    #[test]
    fn times_in_the_future_hold_nothing() {
        // A job queued by GitHub's clock after ours, or room since a moment ahead: no wait yet.
        let mut ahead = set("a", 1, 0, Some(600));
        ahead.queued[0].0 = i64::try_from(NOW + 300).unwrap();
        assert!(held(now(), &[ahead]).is_empty());
        let mut early = set("a", 1, 600, Some(0));
        early.room_since = Some(now() + Duration::from_secs(60));
        assert!(held(now(), &[early]).is_empty());
    }

    #[test]
    fn the_longest_waits_first_and_the_list_is_bounded() {
        let sets: Vec<SetView> = (0..40u64)
            .map(|k| set(&format!("s{k:02}"), 1, 200 + k, Some(3600)))
            .collect();
        let held = held(now(), &sets);
        assert_eq!(held.len(), MAX_HELD);
        assert_eq!(held[0].name, "s39");
        assert_eq!(held[MAX_HELD - 1].name, "s08");
        assert!(held.windows(2).all(|w| w[0].queued <= w[1].queued));
    }

    #[test]
    fn a_scale_set_gets_one_of_its_own_at_most_every_five_minutes() {
        let mut own = OwnRunners::default();
        let gone = |_: &str| false;
        assert!(own.due(now(), "a", gone));
        own.tried(at(OWN_EVERY.as_secs() - 1), "a", "r1".into());
        assert!(!own.due(now(), "a", gone));
        // Another scale set is not held back by it.
        assert!(own.due(now(), "b", gone));
        own.tried(at(OWN_EVERY.as_secs()), "a", "r1".into());
        assert!(own.due(now(), "a", gone));
        // A clock that went back holds it back.
        own.tried(now() + Duration::from_secs(10), "a", "r1".into());
        assert!(!own.due(now(), "a", gone));
    }

    #[test]
    fn none_while_the_last_one_stands() {
        let mut own = OwnRunners::default();
        own.tried(at(3600), "a", "r1".into());
        assert!(!own.due(now(), "a", |runner| runner == "r1"));
        assert!(own.due(now(), "a", |runner| runner == "r2"));
        // A try that started none waits its five minutes only.
        own.tried(at(3600), "b", String::new());
        assert!(own.due(now(), "b", |runner| runner == "r1"));
    }

    #[test]
    fn the_set_of_a_runner_is_known_until_it_is_no_longer_served() {
        let mut own = OwnRunners::default();
        own.tried(now(), "a", "r1".into());
        own.tried(now(), "b", "r2".into());
        assert_eq!(own.set_of("r2"), Some("b"));
        assert_eq!(own.set_of("r3"), None);
        own.retain(|set| set == "a");
        assert_eq!(own.set_of("r2"), None);
        assert_eq!(own.set_of("r1"), Some("a"));
    }

    #[test]
    fn one_of_its_own_asks_once_nothing_is_left_for_it_or_it_waited_long_enough() {
        let minute = Duration::from_secs(60);
        assert!(!own_done(minute, Some(1)));
        assert!(!own_done(minute, None));
        assert!(own_done(minute, Some(0)));
        assert!(own_done(OWN_WAIT, Some(1)));
        assert!(own_done(OWN_WAIT, None));
        assert!(!own_done(OWN_WAIT - Duration::from_secs(1), None));
    }

    fn labels(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn a_queued_job_with_only_some_of_the_labels_could_go_to_one_of_its_own() {
        let set = labels(&["cf-4g", "self-hosted"]);
        let narrower = |queued: &[(&str, Vec<String>)]| {
            narrower_waits(
                "org/app",
                &set,
                queued.iter().map(|(repo, labels)| (*repo, labels.as_slice())),
            )
        };
        assert!(narrower(&[("org/app", labels(&["self-hosted"]))]));
        assert!(narrower(&[("org/app", labels(&["cf-4g"]))]));
        assert!(narrower(&[
            ("org/app", labels(&["cf-4g", "self-hosted", "docker"])),
            ("org/app", labels(&["self-hosted"]))
        ]));
        assert!(narrower(&[("org/app", vec![])]), "a job without labels");
        assert!(!narrower(&[]), "nothing queued");
        assert!(
            !narrower(&[("org/app", set.clone())]),
            "its own set's jobs are what it is for"
        );
        assert!(
            !narrower(&[("org/app", labels(&["self-hosted", "cf-2g"]))]),
            "another size"
        );
        assert!(
            !narrower(&[("org/app", labels(&["cf-4g", "self-hosted", "docker"]))]),
            "more labels"
        );
        assert!(
            !narrower(&[("org/shop", labels(&["self-hosted"]))]),
            "another repository's: its Actions runners are not this one's"
        );
    }
}
