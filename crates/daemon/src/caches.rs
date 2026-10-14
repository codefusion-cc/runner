//! Docker jobs' caches: one disk per repository with its daemon's data (images, build cache), which spares a job
//! the pulls and builds the one before it made.
//!
//! A Docker job gets a copy of its repository's cache that shares every block until written (APFS's clonefile,
//! Linux's reflink; a host without either keeps no cache), and writes only that copy. Once the job is over the copy
//! becomes the repository's cache only when everything says it may ([`refusal`]): GitHub's word that the job succeeded,
//! on a push, a schedule or a dispatch, of a workflow of the repository itself on its default branch, and the guest's
//! word that the daemon stopped by itself and the disk was let go. Anything else, a pull request first of all, is
//! thrown away. So a cache holds what the default branch built and nothing a pull request, a fork or another
//! repository left.
//!
//! The caches together stay under a budget: the one used longest ago goes first ([`evictions`]).

use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use cf_guest_proto::{CacheEnd, CacheFound};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::records::GithubJob;

/// The size of a cache disk as the guest sees it. The file is sparse: the host holds only what was written.
pub const DISK_BYTES: u64 = 20 << 30;
/// The events kept for the console: the newest ones.
pub const EVENTS_KEPT: usize = 32;

/// A repository's cache key. GitHub's names ignore case, so one repository has one key however a workflow or a
/// listing spells it; two repositories never share one, and the cache's own record names its repository, which is
/// held against the job's before the cache is used ([`CacheStore::prepare`]).
pub fn key(repo: &str) -> String {
    let digest = Sha256::digest(repo.to_lowercase().as_bytes());
    digest.iter().take(16).map(|byte| format!("{byte:02x}")).collect()
}

/// The events of a run whose job may write its repository's cache.
const WRITERS: [&str; 3] = ["push", "schedule", "workflow_dispatch"];

/// Why a job's copy does not become its repository's cache; `None` when it does. Every condition must be known and
/// hold, so what is unknown refuses: GitHub's result, the event, the workflow's ref, the default branch, the guest's
/// word on the disk.
pub fn refusal(
    repo: &str,
    default_branch: Option<&str>,
    github: Option<&GithubJob>,
    end: Option<CacheEnd>,
) -> Option<&'static str> {
    let Some(github) = github else {
        return Some("GitHub did not say how the job ended");
    };
    if !matches!(github.result.as_deref(), Some("succeeded" | "success")) {
        return Some("the job did not succeed");
    }
    if !WRITERS.contains(&github.event.as_str()) {
        return Some("only a push, a schedule or a dispatch writes the cache");
    }
    // `owner/name/.github/workflows/file.yml@refs/heads/main`: the repository and the ref the workflow ran at.
    let Some((path, git_ref)) = github.workflow.rsplit_once('@') else {
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
    let Some(end) = end else {
        return Some("the machine said nothing of the cache");
    };
    if !end.daemon {
        return Some("the daemon did not answer at the job's end");
    }
    if !end.clean {
        return Some("the daemon did not stop by itself, or the disk was not let go");
    }
    None
}

/// Whether a repository's cache is to go after this job: the job found it as it was and its daemon could not run
/// on it to the end. A next job then starts from an empty one.
pub fn spoiled(end: Option<CacheEnd>) -> bool {
    end.is_some_and(|end| end.found == CacheFound::Reused && !end.daemon)
}

/// A repository's cache as the store keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cache {
    /// `owner/name`, as the first job that wrote it spelled it.
    pub repo: String,
    /// What it holds of the host's disk.
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

/// What happened to a cache, for the console and the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEvent {
    /// When, in seconds since 1970.
    pub at: i64,
    /// `owner/name`.
    pub repo: String,
    /// `kept`, `discarded`, `replaced` (the guest found a disk that would not mount and started an empty one),
    /// `dropped` (its daemon could not run on it) or `evicted` (over the budget).
    pub kind: String,
    /// Why, for `discarded` and `dropped`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

/// The caches of one host, in a folder of their own: `<key>.img` and `<key>.json` per repository, and `jobs/<job>.img`
/// for the copies jobs run with.
pub struct CacheStore {
    dir: PathBuf,
    budget: u64,
    events: VecDeque<CacheEvent>,
}

/// The record of a job's copy, kept with the job's own record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheCopy {
    /// The repository's key.
    pub key: String,
    /// The copy the machine got.
    pub path: PathBuf,
}

fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

impl CacheStore {
    /// A store in `dir` that keeps at most `budget` bytes, or none at all when the host cannot share blocks between
    /// files there (a copy of a cache would then cost its whole size, every job).
    pub fn open(dir: &Path, budget: u64) -> Option<CacheStore> {
        if budget == 0 {
            return None;
        }
        private_dir(&dir.join("jobs")).ok()?;
        let probe = dir.join(".clone-probe");
        let copy = dir.join(".clone-probe.copy");
        let _ = fs::remove_file(&copy);
        fs::write(&probe, b"probe").ok()?;
        let shares = cf_vmhost::scratch::clone(&probe, &copy).is_ok();
        let _ = (fs::remove_file(&probe), fs::remove_file(&copy));
        if !shares {
            tracing::info!(dir = %dir.display(), "no Docker cache: this file system cannot share blocks between files");
            return None;
        }
        Some(CacheStore {
            dir: dir.to_path_buf(),
            budget,
            events: VecDeque::new(),
        })
    }

    fn image(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.img"))
    }

    fn record(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    fn read(&self, key: &str) -> Option<Cache> {
        let mut cache: Cache = serde_json::from_slice(&fs::read(self.record(key)).ok()?).ok()?;
        cache.bytes = fs::metadata(self.image(key)).ok()?.blocks() * 512;
        Some(cache)
    }

    fn write(&self, key: &str, cache: &Cache) {
        let path = self.record(key);
        let next = path.with_extension("json.next");
        let written = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&next)
            .and_then(|mut file| file.write_all(&serde_json::to_vec(cache).unwrap_or_default()))
            .and_then(|()| fs::rename(&next, &path));
        if let Err(error) = written {
            tracing::warn!(%key, %error, "a cache's record was not written");
        }
    }

    fn event(&mut self, at: i64, repo: &str, kind: &str, why: Option<&str>) {
        tracing::info!(%repo, kind, why = why.unwrap_or(""), "Docker cache");
        self.events.push_back(CacheEvent {
            at,
            repo: repo.to_string(),
            kind: kind.to_string(),
            why: why.map(str::to_string),
        });
        while self.events.len() > EVENTS_KEPT {
            self.events.pop_front();
        }
    }

    /// The copy a Docker job of `repo` runs with, named after the job: its repository's cache shared block by block,
    /// or an empty sparse file the guest makes a file system on. `None` when no copy could be made; the job then
    /// keeps its daemon's data on its own disk.
    pub fn prepare(&mut self, job: &str, repo: &str, now: i64) -> Option<CacheCopy> {
        let key = key(repo);
        let path = self.dir.join("jobs").join(format!("{job}.img"));
        let _ = fs::remove_file(&path);
        let made = match self.read(&key) {
            Some(mut cache) if cache.repo.eq_ignore_ascii_case(repo) => {
                cache.used_at = now;
                self.write(&key, &cache);
                cf_vmhost::scratch::clone(&self.image(&key), &path)
            }
            found => {
                if found.is_some() {
                    // Not this repository's, whatever its key: it is neither used nor kept.
                    self.forget(&key);
                }
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)
                    .and_then(|file| file.set_len(DISK_BYTES))
            }
        };
        match made {
            Ok(()) => Some(CacheCopy { key, path }),
            Err(error) => {
                tracing::warn!(%repo, %job, %error, "no Docker cache for this job");
                let _ = fs::remove_file(&path);
                None
            }
        }
    }

    fn forget(&self, key: &str) {
        let _ = fs::remove_file(self.image(key));
        let _ = fs::remove_file(self.record(key));
    }

    /// Once a job is over: its copy becomes its repository's cache when [`refusal`] finds no reason against it, and is
    /// deleted otherwise. A cache its daemon could not run on is dropped. Then the caches are brought under the
    /// budget, `in_use` being the keys of copies machines hold now.
    pub fn settle(
        &mut self,
        repo: &str,
        copy: &CacheCopy,
        why_not: Option<&str>,
        end: Option<CacheEnd>,
        in_use: &BTreeSet<String>,
        now: i64,
    ) {
        if end.is_some_and(|end| end.found == CacheFound::Replaced) {
            self.event(now, repo, "replaced", None);
        }
        match why_not {
            None => match fs::rename(&copy.path, self.image(&copy.key)) {
                Ok(()) => {
                    let cache = Cache {
                        repo: repo.to_string(),
                        bytes: 0,
                        written_at: now,
                        used_at: now,
                    };
                    self.write(&copy.key, &cache);
                    self.event(now, repo, "kept", None);
                }
                Err(error) => {
                    let _ = fs::remove_file(&copy.path);
                    tracing::warn!(%repo, %error, "a job's copy did not become the cache");
                }
            },
            Some(why) => {
                let _ = fs::remove_file(&copy.path);
                if spoiled(end) {
                    self.forget(&copy.key);
                    self.event(now, repo, "dropped", Some(why));
                } else {
                    self.event(now, repo, "discarded", Some(why));
                }
            }
        }
        self.enforce(in_use, now);
    }

    fn enforce(&mut self, in_use: &BTreeSet<String>, now: i64) {
        let caches = self.list();
        for key in evictions(&caches, self.budget, in_use) {
            let repo = caches
                .iter()
                .find(|(found, _)| *found == key)
                .map(|(_, cache)| cache.repo.clone())
                .unwrap_or_default();
            self.forget(&key);
            self.event(now, &repo, "evicted", None);
        }
    }

    /// Every cache with its key, by key.
    pub fn list(&self) -> Vec<(String, Cache)> {
        let mut out: Vec<(String, Cache)> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let key = name.strip_suffix(".json")?.to_string();
                let cache = self.read(&key)?;
                Some((key, cache))
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// The newest events, oldest first.
    pub fn events(&self) -> Vec<CacheEvent> {
        self.events.iter().cloned().collect()
    }

    /// Deletes the copies of jobs nothing knows any more: those a Runner before left when it stopped.
    pub fn sweep(&self, known: &BTreeSet<PathBuf>) {
        for entry in fs::read_dir(self.dir.join("jobs"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = entry.path();
            if !known.contains(&path) {
                let _ = fs::remove_file(path);
            }
        }
    }
}
