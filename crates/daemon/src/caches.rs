//! Jobs' caches: one disk per repository with npm's cache, pnpm's store and a Docker job's daemon data (images,
//! build cache), which spares a job the downloads, pulls and builds the one before it made.
//!
//! Every job gets a copy of its repository's cache that shares every block until written (APFS's clonefile, Linux's
//! reflink; a host without either keeps no cache), and writes only that copy, so jobs that run at once never write
//! the same file. Once the job is over the copy becomes the repository's cache only when everything says it may
//! ([`refusal`]): GitHub's word that the job succeeded, on a push, a schedule or a dispatch, of a workflow of the
//! repository itself on its default branch, and the guest's word that the disk was let go cleanly (after a Docker
//! job's daemon stopped by itself). Anything else, a pull request first of all, is thrown away. So a cache holds what
//! the default branch downloaded and built and nothing a pull request, a fork or another repository left; the guest
//! keeps only packages and Docker's data on it (`cf_supervisor::cache`).
//!
//! The caches together stay under a budget: the one used longest ago goes first ([`evictions`]).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cf_guest_proto::{CacheEnd, CacheFound};
use serde::{Deserialize, Serialize};

use crate::records::GithubJob;
pub use cf_ghapp::WorkflowRun;

/// The size of a cache disk as the guest sees it. The file is sparse: the host holds only what was written.
pub const DISK_BYTES: u64 = 20 << 30;
/// The events kept for the console: the newest ones.
pub const EVENTS_KEPT: usize = cf_console_client::MAX_JOB_CACHE_EVENTS;

/// A scale set's [`CacheRepo`] as the newest look at GitHub found it, shared by its state and whatever starts its
/// machines: a repository made public since gives its next machine no cache.
#[derive(Debug, Clone, Default)]
pub struct CacheTerms(Arc<Mutex<Option<CacheRepo>>>);

impl CacheTerms {
    pub fn new(terms: Option<CacheRepo>) -> CacheTerms {
        CacheTerms(Arc::new(Mutex::new(terms)))
    }

    pub fn get(&self) -> Option<CacheRepo> {
        self.0.lock().expect("cache terms").clone()
    }

    pub fn set(&self, terms: Option<CacheRepo>) {
        *self.0.lock().expect("cache terms") = terms;
    }
}

impl PartialEq for CacheTerms {
    fn eq(&self, other: &CacheTerms) -> bool {
        self.get() == other.get()
    }
}

impl Eq for CacheTerms {}

/// A repository's cache key: GitHub's number for it, which a rename keeps and a repository made again under the same
/// name does not get. The cache's own record names it too, and is held against the job's before the cache is used
/// ([`CacheStore::prepare`]).
pub fn key(repo_id: u64) -> String {
    format!("repo-{repo_id}")
}

/// What a repository's jobs need to get its cache: GitHub's number for it and its default branch, whose pushes write
/// the cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheRepo {
    /// GitHub's number for the repository.
    pub id: u64,
    /// Its default branch when GitHub was last asked.
    pub default_branch: String,
}

impl CacheRepo {
    /// The terms of a repository GitHub listed, or `None` when its jobs get no cache: a public one, since anyone may
    /// open a pull request there and the Runner gives a machine its disk before GitHub says which job it runs, and one
    /// GitHub did not name a number or a default branch for. What was not read counts as public.
    pub fn of(repo: &cf_ghapp::Repo) -> Option<CacheRepo> {
        let default_branch = repo.default_branch.clone().filter(|branch| !branch.is_empty())?;
        (repo.private && repo.id != 0).then_some(CacheRepo {
            id: repo.id,
            default_branch,
        })
    }
}

/// Why a writer's copy is refused when its Docker daemon could not run on it: the one refusal after which the cache
/// it came from may be dropped ([`CacheStore::settle`]). Every other check passed by then, so only a job that could
/// have written the cache can drop it.
pub const DAEMON_SILENT: &str = "the daemon did not answer at the job's end";

/// The events of a run whose job may write its repository's cache.
const WRITERS: [&str; 3] = ["push", "schedule", "workflow_dispatch"];

/// Why a job's copy does not become its repository's cache; `None` when it does. Every condition must be known and
/// hold, so what is unknown refuses: GitHub's result, the event, the workflow's ref, the default branch, the run's
/// branch, the guest's word on the disk.
///
/// The workflow's ref is where its file is, not where the code it runs came from: a reusable workflow at the default
/// branch that another branch's workflow calls runs that branch's code. So the run itself (`run`, which GitHub's REST
/// API says) must have checked out the default branch of this repository.
pub fn refusal(
    repo: &str,
    default_branch: Option<&str>,
    github: Option<&GithubJob>,
    run: Option<&WorkflowRun>,
    end: Option<CacheEnd>,
) -> Option<&'static str> {
    let Some(github) = github else {
        return Some("GitHub did not say how the job ended");
    };
    if !matches!(github.result.as_deref(), Some("succeeded" | "success")) {
        return Some("the job did not succeed");
    }
    if let Some(why) = job_refusal(repo, default_branch, github) {
        return Some(why);
    }
    // Known and not empty: `job_refusal` refused otherwise.
    let branch = default_branch.unwrap_or_default();
    let Some(run) = run else {
        return Some("GitHub's run was not read");
    };
    if run.head_branch.as_deref() != Some(branch) {
        return Some("the run's code is not the default branch's");
    }
    // A tag's run names the tag as its branch: only its commit says whose code it ran.
    if !run.on_default_branch {
        return Some("the run's commit is not on the default branch");
    }
    if !run
        .head_repository
        .as_deref()
        .is_some_and(|head| head.eq_ignore_ascii_case(repo))
    {
        return Some("the run's code is another repository's");
    }
    if run.event != github.event {
        return Some("the run's event is not the job's");
    }
    let Some(end) = end else {
        return Some("the machine said nothing of the cache");
    };
    if end.docker == Some(false) {
        return Some(DAEMON_SILENT);
    }
    if !end.clean {
        return Some("the disk was not let go cleanly");
    }
    None
}

/// Whether GitHub's run is worth reading for a job's copy: what GitHub said of the job, as far as it is known while
/// the job runs, does not refuse it already ([`job_refusal`]). A pull request's job, the most common, then costs no
/// call.
pub fn worth_reading(repo: &str, default_branch: Option<&str>, github: &GithubJob) -> bool {
    job_refusal(repo, default_branch, github).is_none()
}

/// What refuses a job's copy from what GitHub said of the job before it ended: its event, its workflow's repository
/// and ref, the default branch. The run itself and the job's end are [`refusal`]'s.
fn job_refusal(repo: &str, default_branch: Option<&str>, github: &GithubJob) -> Option<&'static str> {
    if !WRITERS.contains(&github.event.as_str()) {
        return Some("only a push, a schedule or a dispatch writes the cache");
    }
    // `owner/name/.github/workflows/file.yml@refs/heads/main`: the repository and the ref the workflow ran at.
    // Split at the first `@`: a branch's name may hold one, and the ref is then never just the default branch.
    let Some((path, git_ref)) = github.workflow.split_once('@') else {
        return Some("the job's workflow names no ref");
    };
    let own = path
        .get(..repo.len() + 1)
        .is_some_and(|start| start.eq_ignore_ascii_case(&format!("{repo}/")));
    if !own {
        return Some("the workflow is another repository's");
    }
    let Some(branch) = default_branch.filter(|branch| !branch.is_empty()) else {
        return Some("the repository's default branch is not known");
    };
    if git_ref != format!("refs/heads/{branch}") {
        return Some("the run is not on the default branch");
    }
    None
}

/// Whether a repository's cache is to go after this job: a Docker job found it as it was and its daemon could not
/// run on it to the end. A next job then starts from an empty one.
pub fn spoiled(end: Option<CacheEnd>) -> bool {
    end.is_some_and(|end| end.found == CacheFound::Reused && end.docker == Some(false))
}

/// A repository's cache as the store keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cache {
    /// `owner/name`, as the last job that wrote it spelled it.
    pub repo: String,
    /// GitHub's number for the repository.
    pub repo_id: u64,
    /// What it holds of the host's disk, as the store last found it; not written down.
    #[serde(skip)]
    pub bytes: u64,
    /// When a job's copy last became it, in seconds since 1970.
    pub written_at: i64,
    /// When a job last got a copy of it.
    pub used_at: i64,
}

/// The caches to delete, least recently used first, so that the rest fit `budget` bytes. A cache a job uses now is
/// never one of them.
pub fn evictions(caches: &[(String, Cache)], budget: u64, in_use: &BTreeSet<String>) -> Vec<String> {
    let mut total: u64 = caches.iter().map(|(_, cache)| cache.bytes).sum();
    let mut oldest: Vec<&(String, Cache)> = caches.iter().collect();
    oldest.sort_by_key(|(key, cache)| (cache.used_at, key.clone()));
    let mut out = Vec::new();
    for (key, cache) in oldest {
        if total <= budget {
            break;
        }
        if in_use.contains(key) {
            continue;
        }
        total = total.saturating_sub(cache.bytes);
        out.push(key.clone());
    }
    out
}

/// What happened to a cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheEventKind {
    /// A job's copy became its repository's cache.
    Kept,
    /// A job's copy was thrown away, and the cache stays as it was.
    Discarded,
    /// The guest found a disk that would not mount and started an empty one.
    Replaced,
    /// The cache went: its daemon could not run on it.
    Dropped,
    /// The cache went: the caches were over the budget.
    Evicted,
}

impl CacheEventKind {
    /// Its name, as the console shows it.
    pub fn name(self) -> &'static str {
        match self {
            CacheEventKind::Kept => "kept",
            CacheEventKind::Discarded => "discarded",
            CacheEventKind::Replaced => "replaced",
            CacheEventKind::Dropped => "dropped",
            CacheEventKind::Evicted => "evicted",
        }
    }
}

/// What happened to a cache, for the console and the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEvent {
    /// When, in seconds since 1970.
    pub at: i64,
    /// `owner/name`.
    pub repo: String,
    pub kind: CacheEventKind,
    /// Why, for `discarded` and `dropped`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

/// The caches of one host, in a folder of their own: `<key>.img` and `<key>.json` per repository, and `jobs/<job>.img`
/// for the copies jobs run with. Only the store changes the folder, so it reads it once and keeps what it found.
pub struct CacheStore {
    dir: PathBuf,
    budget: u64,
    events: VecDeque<CacheEvent>,
    /// The caches with their keys, by key.
    listing: Vec<(String, Cache)>,
    /// The copies machines hold now, with their repositories' keys: those caches are never evicted.
    copies: BTreeMap<PathBuf, String>,
}

/// A job's copy, kept with the job's record: what [`refusal`] needs of it besides GitHub's word.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheCopy {
    /// The repository's key.
    pub key: String,
    /// GitHub's number for the repository.
    #[serde(default)]
    pub repo_id: u64,
    /// The copy the machine got.
    pub path: PathBuf,
    /// When the cache it was copied from was written; `None` for an empty copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<i64>,
    /// The repository's default branch when the job was given its machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    /// What the guest said of the disk at the job's end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<CacheEnd>,
    /// What GitHub said of the job's run, read while the job ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<WorkflowRun>,
}

impl CacheStore {
    /// A store in `dir` that keeps at most `budget` bytes. None at all when the budget is 0, which deletes what the
    /// folder held, or when the host cannot share blocks between files there (a copy of a cache would then cost its
    /// whole size, every job).
    pub fn open(dir: &Path, budget: u64) -> Option<CacheStore> {
        if budget == 0 {
            match fs::remove_dir_all(dir) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    tracing::warn!(dir = %dir.display(), %error, "the job caches were not deleted");
                }
                _ => {}
            }
            return None;
        }
        cf_secret::private_dir(&dir.join("jobs")).ok()?;
        let probe = dir.join(".clone-probe");
        let copy = dir.join(".clone-probe.copy");
        let _ = fs::remove_file(&copy);
        let probed = fs::write(&probe, b"probe").and_then(|()| cf_vmhost::scratch::clone(&probe, &copy));
        let _ = (fs::remove_file(&probe), fs::remove_file(&copy));
        if let Err(error) = probed {
            // No job gets a copy now, so the copies a Runner before left are nobody's: their room comes back.
            let _ = fs::remove_dir_all(dir.join("jobs"));
            tracing::info!(dir = %dir.display(), %error, "no job cache: blocks cannot be shared between files here");
            return None;
        }
        let mut store = CacheStore {
            dir: dir.to_path_buf(),
            budget,
            events: VecDeque::new(),
            listing: Vec::new(),
            copies: BTreeMap::new(),
        };
        store.listing = store.read_all();
        store.drop_strays();
        Some(store)
    }

    fn image(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.img"))
    }

    fn record(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    fn bytes(&self, key: &str) -> Option<u64> {
        Some(fs::metadata(self.image(key)).ok()?.blocks() * 512)
    }

    fn read_all(&self) -> Vec<(String, Cache)> {
        let mut out: Vec<(String, Cache)> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let key = name.strip_suffix(".json")?.to_string();
                let mut cache: Cache = serde_json::from_slice(&fs::read(self.record(&key)).ok()?).ok()?;
                cache.bytes = self.bytes(&key)?;
                Some((key, cache))
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Deletes the images no readable record names, and the records that cannot be read: a Runner stopped between
    /// keeping one and writing its record, could not write it, or wrote what this one does not read. Nothing would
    /// count or evict them.
    fn drop_strays(&self) {
        for entry in fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(key) = name.strip_suffix(".img").or_else(|| name.strip_suffix(".json")) else {
                continue;
            };
            if !self.listing.iter().any(|(found, _)| found == key) {
                tracing::info!(%key, "a cache or record without the other is deleted");
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    fn write(&self, key: &str, cache: &Cache) {
        let written = serde_json::to_vec(cache)
            .map_err(std::io::Error::other)
            .and_then(|json| cf_secret::replace_private(&self.record(key), &json));
        if let Err(error) = written {
            tracing::warn!(%key, %error, "a cache's record was not written");
        }
    }

    fn event(&mut self, at: i64, repo: &str, kind: CacheEventKind, why: Option<&str>) {
        tracing::info!(%repo, kind = kind.name(), why = why.unwrap_or(""), "job cache");
        self.events.push_back(CacheEvent {
            at,
            repo: repo.to_string(),
            kind,
            why: why.map(str::to_string),
        });
        while self.events.len() > EVENTS_KEPT {
            self.events.pop_front();
        }
    }

    /// The copy a job of `repo` runs with, named after the job: its repository's cache shared block by block, or an
    /// empty sparse file the guest makes a file system on. `None` when no copy could be made; the job then keeps its
    /// packages and its daemon's data on its own disk.
    pub fn prepare(&mut self, job: &str, repo: &str, terms: &CacheRepo, now: i64) -> Option<CacheCopy> {
        let key = key(terms.id);
        let path = self.dir.join("jobs").join(format!("{job}.img"));
        let _ = fs::remove_file(&path);
        // A copy may grow to the whole disk whatever it starts as: none without room for that.
        let room = free_bytes(&self.dir);
        if room.is_none_or(|room| room < DISK_BYTES) {
            tracing::info!(%repo, %job, room, "no cache for this job: too little disk for a copy");
            return None;
        }
        let found = self.listing.iter().position(|(found, _)| *found == key);
        let mut from = None;
        if let Some(at) = found {
            if self.listing[at].1.repo_id == terms.id {
                match cf_vmhost::scratch::clone(&self.image(&key), &path) {
                    Ok(()) => {
                        self.listing[at].1.used_at = now;
                        self.write(&key, &self.listing[at].1);
                        from = Some(self.listing[at].1.written_at);
                    }
                    Err(error) => {
                        // A cache that cannot be copied is never used again: the job starts an empty one.
                        tracing::warn!(%repo, %job, %error, "the cache could not be copied; it goes");
                        let _ = fs::remove_file(&path);
                        self.forget(&key);
                    }
                }
            } else {
                // Not this repository's, whatever its key: it is neither used nor kept.
                self.forget(&key);
            }
        }
        let made = match from {
            Some(_) => Ok(()),
            None => fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .and_then(|file| file.set_len(DISK_BYTES)),
        };
        match made {
            Ok(()) => {
                self.copies.insert(path.clone(), key.clone());
                Some(CacheCopy {
                    key,
                    repo_id: terms.id,
                    path,
                    from,
                    default_branch: Some(terms.default_branch.clone()),
                    end: None,
                    run: None,
                })
            }
            Err(error) => {
                tracing::warn!(%repo, %job, %error, "no cache for this job");
                let _ = fs::remove_file(&path);
                None
            }
        }
    }

    fn forget(&mut self, key: &str) {
        let _ = fs::remove_file(self.image(key));
        let _ = fs::remove_file(self.record(key));
        self.listing.retain(|(found, _)| found != key);
    }

    /// Deletes a copy that no job ran with: its machine never started.
    pub fn discard(&mut self, copy: &CacheCopy) {
        let _ = fs::remove_file(&copy.path);
        self.copies.remove(&copy.path);
    }

    /// Once a job is over: its copy becomes its repository's cache when `why_not` (see [`refusal`]) is `None`, and is
    /// deleted otherwise. A cache its daemon could not run on is dropped. Then the caches are brought under the
    /// budget.
    pub fn settle(&mut self, repo: &str, copy: &CacheCopy, why_not: Option<&str>, now: i64) {
        self.copies.remove(&copy.path);
        if copy.end.is_some_and(|end| end.found == CacheFound::Replaced) {
            self.event(now, repo, CacheEventKind::Replaced, None);
        }
        match why_not {
            None => match fs::rename(&copy.path, self.image(&copy.key)) {
                Ok(()) => {
                    let cache = Cache {
                        repo: repo.to_string(),
                        repo_id: copy.repo_id,
                        bytes: self.bytes(&copy.key).unwrap_or(0),
                        written_at: now,
                        used_at: now,
                    };
                    self.write(&copy.key, &cache);
                    self.listing.retain(|(found, _)| *found != copy.key);
                    let at = self.listing.partition_point(|(found, _)| *found < copy.key);
                    self.listing.insert(at, (copy.key.clone(), cache));
                    self.event(now, repo, CacheEventKind::Kept, None);
                }
                Err(error) => {
                    let _ = fs::remove_file(&copy.path);
                    tracing::warn!(%repo, %error, "a job's copy did not become the cache");
                }
            },
            Some(why) => {
                let _ = fs::remove_file(&copy.path);
                // Only a writer drops it, and only the cache it started from: a newer one was not tried.
                let current = self
                    .listing
                    .iter()
                    .find(|(key, _)| *key == copy.key)
                    .map(|(_, cache)| cache.written_at);
                if spoiled(copy.end) && why == DAEMON_SILENT && copy.from.is_some() && current == copy.from {
                    self.forget(&copy.key);
                    self.event(now, repo, CacheEventKind::Dropped, Some(why));
                } else {
                    self.event(now, repo, CacheEventKind::Discarded, Some(why));
                }
            }
        }
        self.enforce(now);
    }

    fn enforce(&mut self, now: i64) {
        let in_use: BTreeSet<String> = self.copies.values().cloned().collect();
        let gone = evictions(&self.listing, self.budget, &in_use);
        let gone: Vec<(String, String)> = self
            .listing
            .iter()
            .filter(|(key, _)| gone.contains(key))
            .map(|(key, cache)| (key.clone(), cache.repo.clone()))
            .collect();
        for (key, repo) in gone {
            self.forget(&key);
            self.event(now, &repo, CacheEventKind::Evicted, None);
        }
    }

    /// Every cache with its key, by key.
    pub fn list(&self) -> &[(String, Cache)] {
        &self.listing
    }

    /// The newest events, oldest first.
    pub fn events(&self) -> impl Iterator<Item = &CacheEvent> {
        self.events.iter()
    }

    /// The copies machines still hold, `known`, when the Runner starts: they are in use. Every other copy is one a
    /// Runner before left when it stopped, and is deleted.
    pub fn sweep<'a>(&mut self, known: impl IntoIterator<Item = &'a CacheCopy>) {
        self.copies = known
            .into_iter()
            .map(|copy| (copy.path.clone(), copy.key.clone()))
            .collect();
        for entry in fs::read_dir(self.dir.join("jobs"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = entry.path();
            if !self.copies.contains_key(&path) {
                let _ = fs::remove_file(path);
            }
        }
    }
}

/// What an unprivileged process may still write on the file system of `dir`, in bytes; `None` when it cannot be read.
fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt as _;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `path` ends with a NUL and `stats` is a statvfs this call may write.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statvfs returned 0, so it wrote the whole struct.
    let stats = unsafe { stats.assume_init() };
    #[allow(
        clippy::useless_conversion,
        reason = "the field types differ between macOS and Linux"
    )]
    Some(u64::from(stats.f_bavail).saturating_mul(u64::from(stats.f_frsize)))
}
