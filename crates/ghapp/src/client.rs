use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use cf_secret::Secret;
use cf_workflows::LabelSet;
use reqwest::Method;
use reqwest::header::ETAG;
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::error::Error;
use crate::http::{Http, Response};
use crate::rest::{Installed, MAX_PAGES, Rest, decode};
use crate::source::TokenSource;

/// GitHub's REST API.
pub const API: &str = "https://api.github.com";

/// One account the App is installed on.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Installation {
    /// The installation's ID.
    pub id: i64,
}

/// A repository the App may serve.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Repo {
    /// `owner/name`.
    pub full_name: String,
    /// Changes with every push, so workflows need reading again only when it does.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub pushed_at: Option<OffsetDateTime>,
    /// The repository's page, which the scale set client takes as its scope.
    #[serde(default)]
    pub html_url: String,
    /// Whether the repository is private.
    #[serde(default)]
    pub private: bool,
    /// Whether it is archived.
    #[serde(default)]
    pub archived: bool,
    /// Whether it is disabled.
    #[serde(default)]
    pub disabled: bool,
}

/// How many runs' jobs one look reads, newest first.
pub const WAITING_RUNS: usize = 10;
/// How many jobs of one run are read: one page.
pub const JOBS_PAGE: usize = 100;
/// The most distinct label sets kept of one run.
pub const MAX_RUN_SETS: usize = 16;
/// The longest ETag kept to ask with again.
const MAX_ETAG_BYTES: usize = 256;

/// A job GitHub has not finished, of a label set these Actions runners serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitingJob {
    /// Its ID.
    pub id: i64,
    /// The set its labels ask for ([`LabelSet::served`]).
    pub set: LabelSet,
    /// When it was queued, in seconds since 1970.
    pub created: i64,
    /// Whether its run is a pull request's from another repository, a fork: on a public repository, a stranger's
    /// code. A run that names no head repository counts as one.
    pub from_fork: bool,
    /// The run's jobs of this set that wait for an Actions runner (`queued`): when each was queued, in seconds since
    /// 1970, and its ID, the oldest first. A job GitHub assigned to a scale set stays `queued` until its Actions
    /// runner takes it.
    pub queued: Vec<(i64, i64)>,
}

/// The runner group every repository has. A repository's own Actions runners and the Runner's scale sets are in it.
pub const DEFAULT_RUNNER_GROUP: i64 = 1;

/// An Actions runner registered for one job outside any scale set ([`GitHubApp::generate_jit_config`]).
#[derive(Debug)]
pub struct PlainJit {
    /// GitHub's ID for it.
    pub id: i64,
    /// What the Actions runner starts with. A credential: it never reaches a log.
    pub encoded: Secret<String>,
}

/// What one look at a repository's queue found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Waiting {
    /// The jobs.
    pub jobs: Vec<WaitingJob>,
    /// What the bounds left out.
    pub left_out: Vec<String>,
}

/// A run in a listing of queued or running runs.
#[derive(Clone)]
struct Run {
    id: i64,
    /// When it last changed: its jobs did not change while this did not.
    updated: String,
    from_fork: bool,
}

/// A page of a listing of runs: how many there are, and the newest.
struct Listing {
    total: usize,
    runs: Vec<Run>,
}

/// A page of a repository's runs, read as a [`Listing`].
fn read_runs(repo: &str, response: &Response) -> Result<Listing, Error> {
    #[derive(Deserialize)]
    struct Head {
        full_name: String,
    }
    #[derive(Deserialize)]
    struct RunJson {
        id: i64,
        #[serde(default)]
        updated_at: String,
        head_repository: Option<Head>,
    }
    #[derive(Deserialize)]
    struct Runs {
        total_count: usize,
        workflow_runs: Vec<RunJson>,
    }
    let page: Runs = decode("workflow runs", response)?;
    Ok(Listing {
        total: page.total_count,
        runs: page
            .workflow_runs
            .into_iter()
            .map(|r| Run {
                id: r.id,
                updated: r.updated_at,
                from_fork: !r
                    .head_repository
                    .is_some_and(|head| head.full_name.eq_ignore_ascii_case(repo)),
            })
            .collect(),
    })
}

/// What a run's jobs read as: the newest unfinished job of each distinct label set served, with the set's queued
/// ones, at most [`MAX_RUN_SETS`] of them, and what the bounds left out.
struct RunRead {
    jobs: Vec<WaitingJob>,
    left_out: Vec<String>,
}

/// A page of a run's jobs, read as a [`RunRead`]. A job of labels these Actions runners do not serve is left to
/// others: it is not kept, and needs no word.
fn read_jobs(run: &Run, response: &Response) -> Result<RunRead, Error> {
    #[derive(Deserialize)]
    struct Job {
        id: i64,
        status: String,
        labels: Vec<String>,
        created_at: String,
    }
    #[derive(Deserialize)]
    struct Jobs {
        total_count: usize,
        jobs: Vec<Job>,
    }
    let page: Jobs = decode("workflow run jobs", response)?;
    let mut left_out = Vec::new();
    if page.total_count > page.jobs.len() {
        left_out.push(format!(
            "run {}: {} jobs, the first {} read",
            run.id,
            page.total_count,
            page.jobs.len()
        ));
    }
    let mut sets: BTreeMap<LabelSet, WaitingJob> = BTreeMap::new();
    for job in page.jobs.into_iter().filter(|j| j.status != "completed") {
        let Some(set) = LabelSet::served(&job.labels) else {
            continue;
        };
        let created = OffsetDateTime::parse(&job.created_at, &Rfc3339)
            .map_err(|e| Error::Decode {
                what: "workflow run jobs".into(),
                why: format!("created_at: {e}"),
            })?
            .unix_timestamp();
        let kept = sets.entry(set.clone()).or_insert_with(|| WaitingJob {
            id: job.id,
            set,
            created,
            from_fork: run.from_fork,
            queued: Vec::new(),
        });
        if job.id > kept.id {
            (kept.id, kept.created) = (job.id, created);
        }
        if job.status == "queued" {
            kept.queued.push((created, job.id));
        }
    }
    for job in sets.values_mut() {
        job.queued.sort_unstable();
    }
    let mut jobs: Vec<WaitingJob> = sets.into_values().collect();
    jobs.sort_by_key(|job| std::cmp::Reverse(job.id));
    if jobs.len() > MAX_RUN_SETS {
        left_out.push(format!(
            "run {}: {} label sets, the newest {MAX_RUN_SETS} kept",
            run.id,
            jobs.len()
        ));
        jobs.truncate(MAX_RUN_SETS);
    }
    Ok(RunRead { jobs, left_out })
}

/// An answer's ETag, when it has one short enough to keep; empty when not.
fn etag_of(response: &Response) -> String {
    response
        .headers
        .get(ETAG)
        .and_then(|tag| tag.to_str().ok())
        .filter(|tag| tag.len() <= MAX_ETAG_BYTES)
        .unwrap_or_default()
        .to_string()
}

/// A repository's workflow file contents by blob hash.
type Blobs = HashMap<String, Arc<Vec<u8>>>;

/// What a repository's queue last read as: what is needed to ask GitHub whether it changed, and what it was.
#[derive(Default)]
struct QueueCache {
    /// Each listing's last page by path, with its ETag.
    listings: HashMap<String, (String, Arc<Listing>)>,
    /// Each run's jobs by run ID, as of the run's `updated_at`.
    runs: HashMap<i64, RunJobs>,
}

/// A run's jobs as last read.
struct RunJobs {
    /// The run's `updated_at` when they were read.
    updated: String,
    /// The ETag of the answer they were read from; empty when it had none.
    etag: String,
    read: Arc<RunRead>,
}

struct Inner {
    rest: Rest,
    source: Arc<dyn TokenSource>,
    /// Each repository's current workflow files by blob hash.
    blobs: Mutex<HashMap<String, Blobs>>,
    /// What each repository's queue last read as.
    queue: Mutex<HashMap<String, QueueCache>>,
}

/// A GitHub App's REST client. Cheap to clone; safe to share. Its tokens come from a [`TokenSource`]: the CodeFusion
/// Console. It keeps workflow files by their blob hash, so polling repositories
/// costs little.
#[derive(Clone)]
pub struct GitHubApp {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for GitHubApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubApp")
            .field("api", &self.inner.rest.api)
            .finish_non_exhaustive()
    }
}

impl GitHubApp {
    /// A client whose tokens come from `source` (the console's). `api` is the REST API root the tokens are for.
    pub fn with_source(source: Arc<dyn TokenSource>, api: &str, http: Http) -> GitHubApp {
        GitHubApp {
            inner: Arc::new(Inner {
                rest: Rest::new(api, http),
                source,
                blobs: Mutex::new(HashMap::new()),
                queue: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The REST API root.
    pub fn api(&self) -> &str {
        &self.inner.rest.api
    }

    /// The HTTP client the calls go through.
    pub fn http(&self) -> &Http {
        &self.inner.rest.http
    }

    /// GitHub's clock minus this machine's, in seconds, as the token source learned it (0 when it did not).
    pub fn clock_skew_secs(&self) -> i64 {
        self.inner.source.clock_skew_secs()
    }

    /// GitHub's time as this machine knows it, in seconds since the epoch: its own clock plus that offset. What
    /// the expiry of a token GitHub issued is compared with.
    pub fn now_epoch(&self) -> i64 {
        self.inner.source.now_epoch()
    }

    /// A call authenticated with an installation token; a 401 drops the token and tries once with a new one.
    async fn as_installation(
        &self,
        installation: i64,
        repo: Option<&str>,
        method: Method,
        path: &str,
    ) -> Result<Response, Error> {
        self.as_installation_with(installation, repo, method, path, None)
            .await
    }

    /// [`Self::as_installation`] with `If-None-Match`.
    async fn as_installation_with(
        &self,
        installation: i64,
        repo: Option<&str>,
        method: Method,
        path: &str,
        etag: Option<&str>,
    ) -> Result<Response, Error> {
        self.inner
            .rest
            .as_installation(
                Installed {
                    source: &*self.inner.source,
                    installation,
                    repo,
                },
                method,
                path,
                etag,
                None,
            )
            .await
    }

    /// Every page of an installation's listing from `first` on. A listing of more than [`MAX_PAGES`] pages is
    /// [`Error::Refused`]: one that names a next page for ever is not read for ever.
    async fn pages(
        &self,
        installation: i64,
        repo: Option<&str>,
        first: &str,
    ) -> Result<Vec<Response>, Error> {
        let mut pages = vec![];
        let mut next = Some(first.to_string());
        while let Some(page) = next {
            if pages.len() == MAX_PAGES {
                return Err(Rest::too_many_pages(first));
            }
            let response = self
                .as_installation(installation, repo, Method::GET, &page)
                .await?;
            next = self.inner.rest.next_page(&response);
            pages.push(response);
        }
        Ok(pages)
    }

    /// Lists every account the App is installed on, as the token source has them.
    pub async fn installations(&self) -> Result<Vec<Installation>, Error> {
        self.inner.source.installations().await
    }

    /// A token that reads an installation's repositories, as the token source keeps it.
    pub async fn installation_token(&self, installation: i64) -> Result<Secret<String>, Error> {
        self.inner.source.token(installation, None).await
    }

    /// Registers an Actions runner for one job with a repository, outside any scale set, in the repository's default
    /// runner group: GitHub hands it any queued job of the repository whose labels it has. It carries exactly
    /// `labels`; GitHub adds none (probe L, `docs/github-protocol.md`).
    pub async fn generate_jit_config(
        &self,
        installation: i64,
        repo: &str,
        name: &str,
        labels: &[String],
    ) -> Result<PlainJit, Error> {
        self.inner
            .source
            .generate_jit_config(installation, repo, name, labels)
            .await
    }

    /// Whether a registered Actions runner of a repository has a job, by GitHub's own record. `None` when GitHub no
    /// longer has it: a one-job Actions runner unregisters itself once its job is over.
    pub async fn runner_busy(
        &self,
        installation: i64,
        repo: &str,
        runner_id: i64,
    ) -> Result<Option<bool>, Error> {
        #[derive(Deserialize)]
        struct RunnerState {
            busy: bool,
        }
        let path = format!("/repos/{repo}/actions/runners/{runner_id}");
        match self
            .as_installation(installation, Some(repo), Method::GET, &path)
            .await
        {
            Ok(response) => Ok(Some(decode::<RunnerState>("Actions runner", &response)?.busy)),
            Err(Error::Status(s)) if s.status == 404 => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Lists the repositories an installation gives the App.
    pub async fn repositories(&self, installation: i64) -> Result<Vec<Repo>, Error> {
        #[derive(Deserialize)]
        struct Page {
            repositories: Vec<Repo>,
        }
        let mut all = vec![];
        for page in self
            .pages(installation, None, "/installation/repositories?per_page=100")
            .await?
        {
            all.extend(decode::<Page>("repositories", &page)?.repositories);
        }
        Ok(all)
    }

    /// The workflow files on the repository's default branch by file name; none when it has no `.github/workflows`
    /// folder. A file is fetched only when its content changed since the last call, and one over `max_file_bytes`
    /// (what the reader of the files would refuse anyway) not at all. Any other failure is an error, not "no
    /// workflows": that would drop the repository's Actions runners.
    pub async fn workflow_files(
        &self,
        installation: i64,
        repo: &str,
        max_file_bytes: u64,
    ) -> Result<BTreeMap<String, Vec<u8>>, Error> {
        #[derive(Deserialize)]
        struct Entry {
            name: String,
            #[serde(rename = "type")]
            kind: String,
            sha: String,
            #[serde(default)]
            size: u64,
        }
        #[derive(Deserialize)]
        struct Blob {
            content: String,
            encoding: String,
        }
        let listing = match self
            .as_installation(
                installation,
                Some(repo),
                Method::GET,
                &format!("/repos/{repo}/contents/.github/workflows"),
            )
            .await
        {
            Ok(r) => r,
            Err(Error::Status(s)) if s.status == 404 => {
                self.inner.blobs.lock().expect("blobs").remove(repo);
                return Ok(BTreeMap::new());
            }
            Err(e) => return Err(e),
        };
        let entries: Vec<Entry> = decode("workflow listing", &listing)?;
        let known = self
            .inner
            .blobs
            .lock()
            .expect("blobs")
            .get(repo)
            .cloned()
            .unwrap_or_default();
        let mut files = BTreeMap::new();
        let mut current = HashMap::new();
        for e in entries {
            let yaml = e.name.ends_with(".yml") || e.name.ends_with(".yaml");
            if e.kind != "file" || !yaml || e.size > max_file_bytes {
                continue;
            }
            let content = match known.get(&e.sha) {
                Some(c) => c.clone(),
                None => {
                    let response = self
                        .as_installation(
                            installation,
                            Some(repo),
                            Method::GET,
                            &format!("/repos/{repo}/git/blobs/{}", e.sha),
                        )
                        .await?;
                    let blob: Blob = decode("workflow file", &response)?;
                    if blob.encoding != "base64" {
                        return Err(Error::Decode {
                            what: format!("{repo}/{}", e.name),
                            why: format!("unexpected encoding {:?}", blob.encoding),
                        });
                    }
                    let clean: String = blob.content.chars().filter(|c| !c.is_whitespace()).collect();
                    let bytes = STANDARD.decode(clean).map_err(|err| Error::Decode {
                        what: format!("{repo}/{}", e.name),
                        why: err.to_string(),
                    })?;
                    // The listing's size is a claim; what counts is what came.
                    if bytes.len() as u64 > max_file_bytes {
                        continue;
                    }
                    Arc::new(bytes)
                }
            };
            current.insert(e.sha.clone(), content.clone());
            files.insert(e.name, content.as_ref().clone());
        }
        self.inner
            .blobs
            .lock()
            .expect("blobs")
            .insert(repo.to_string(), current);
        Ok(files)
    }

    /// The repository's Actions variables, the organization's that reach it included. Needs the App's `Variables`
    /// permission: without it GitHub answers 403 (or 404), which comes back as the error so the caller can say
    /// what is missing. Organization variables are best effort: a refusal there leaves them out.
    pub async fn variables(&self, installation: i64, repo: &str) -> Result<BTreeMap<String, String>, Error> {
        #[derive(Deserialize)]
        struct Variable {
            name: String,
            value: String,
        }
        #[derive(Deserialize)]
        struct Page {
            variables: Vec<Variable>,
        }
        let mut vars = BTreeMap::new();
        for (path, required) in [
            (
                format!("/repos/{repo}/actions/organization-variables?per_page=30"),
                false,
            ),
            (format!("/repos/{repo}/actions/variables?per_page=30"), true),
        ] {
            let pages = match self.pages(installation, Some(repo), &path).await {
                Ok(pages) => pages,
                Err(Error::Status(s)) if !required && (s.status == 403 || s.status == 404) => continue,
                Err(e) => return Err(e),
            };
            for page in &pages {
                for v in decode::<Page>("variables", page)?.variables {
                    vars.insert(v.name, v.value);
                }
            }
        }
        Ok(vars)
    }

    /// The jobs of a repository GitHub has not finished, from its queued and running workflow runs, of the label
    /// sets these Actions runners serve ([`LabelSet::served`]): the newest [`WAITING_RUNS`] runs, one page of each
    /// run's jobs, and of each run the newest job of each distinct set, at most [`MAX_RUN_SETS`].
    /// [`Waiting::left_out`] says what the bounds left out.
    ///
    /// The listings name the ETag of their last answer, so a queue that did not change costs 304s, which do not
    /// count against the rate limit. The jobs of a run that did not change since they were read (its `updated_at`)
    /// are not asked for again; those of one that did are asked for with their ETag. Needs the App's `Actions: read`
    /// permission: without it GitHub answers 403 (or 404), which comes back as the error. Any failure is an error,
    /// not an empty queue; only a run gone between its listing and the read of its jobs is left out.
    pub async fn waiting_jobs(&self, installation: i64, repo: &str) -> Result<Waiting, Error> {
        // Taken out while the look lasts: a look that fails forgets it, and the next one asks without ETags.
        let mut last = self
            .inner
            .queue
            .lock()
            .expect("queue")
            .remove(repo)
            .unwrap_or_default();
        let listing_of = |status: &str| {
            format!(
                "/repos/{repo}/actions/runs?status={status}&per_page={WAITING_RUNS}&exclude_pull_requests=true"
            )
        };
        let (queued, running) = (listing_of("queued"), listing_of("in_progress"));
        let (queued_page, running_page) = tokio::join!(
            self.listing(installation, repo, &queued, &last),
            self.listing(installation, repo, &running, &last)
        );
        let mut now = QueueCache::default();
        let mut total = 0;
        let mut runs = Vec::new();
        for (path, (tag, page)) in [(queued, queued_page?), (running, running_page?)] {
            total += page.total;
            runs.extend(page.runs.iter().cloned());
            if !tag.is_empty() {
                now.listings.insert(path, (tag, page));
            }
        }
        runs.sort_unstable_by_key(|run| std::cmp::Reverse(run.id));
        runs.dedup_by_key(|run| run.id);
        let mut waiting = Waiting::default();
        if total.max(runs.len()) > WAITING_RUNS {
            waiting.left_out.push(format!(
                "{} runs waiting, the newest {WAITING_RUNS} read",
                total.max(runs.len())
            ));
            runs.truncate(WAITING_RUNS);
        }
        for run in runs {
            let jobs = match last.runs.remove(&run.id) {
                Some(known) if !run.updated.is_empty() && known.updated == run.updated => known,
                known => match self.run_jobs(installation, repo, &run, known).await {
                    Ok(jobs) => jobs,
                    Err(Error::Status(s)) if s.status == 404 => continue,
                    Err(e) => return Err(e),
                },
            };
            waiting.jobs.extend(jobs.read.jobs.iter().cloned());
            waiting.left_out.extend(jobs.read.left_out.iter().cloned());
            now.runs.insert(run.id, jobs);
        }
        self.inner
            .queue
            .lock()
            .expect("queue")
            .insert(repo.to_string(), now);
        Ok(waiting)
    }

    /// Forgets what the queues of the repositories `keep` turns down last read as.
    pub fn forget_queues(&self, keep: impl Fn(&str) -> bool) {
        self.inner
            .queue
            .lock()
            .expect("queue")
            .retain(|repo, _| keep(repo));
    }

    /// A page of a listing of runs and its ETag: GitHub's, or the last one when GitHub says it is unchanged.
    async fn listing(
        &self,
        installation: i64,
        repo: &str,
        path: &str,
        last: &QueueCache,
    ) -> Result<(String, Arc<Listing>), Error> {
        let known = last.listings.get(path);
        match self
            .get_if(installation, repo, path, known.map(|(tag, _)| tag.as_str()))
            .await?
        {
            Some(response) => Ok((etag_of(&response), Arc::new(read_runs(repo, &response)?))),
            None => Ok(known.cloned().expect("a 304 answers a known ETag")),
        }
    }

    /// A run's jobs: GitHub's, or the last ones when GitHub says they are unchanged.
    async fn run_jobs(
        &self,
        installation: i64,
        repo: &str,
        run: &Run,
        known: Option<RunJobs>,
    ) -> Result<RunJobs, Error> {
        let path = format!(
            "/repos/{repo}/actions/runs/{}/jobs?filter=latest&per_page={JOBS_PAGE}",
            run.id
        );
        let etag = known
            .as_ref()
            .map(|k| k.etag.as_str())
            .filter(|tag| !tag.is_empty());
        let (etag, read) = match self.get_if(installation, repo, &path, etag).await? {
            Some(response) => (etag_of(&response), Arc::new(read_jobs(run, &response)?)),
            None => {
                let known = known.expect("a 304 answers a known ETag");
                (known.etag, known.read)
            }
        };
        Ok(RunJobs {
            updated: run.updated.clone(),
            etag,
            read,
        })
    }

    /// A GET as an installation that names the ETag of the answer known: `None` when GitHub says it is unchanged.
    async fn get_if(
        &self,
        installation: i64,
        repo: &str,
        path: &str,
        etag: Option<&str>,
    ) -> Result<Option<Response>, Error> {
        match self
            .as_installation_with(installation, Some(repo), Method::GET, path, etag)
            .await
        {
            Ok(response) => Ok(Some(response)),
            Err(Error::Status(s)) if s.status == 304 && etag.is_some() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The App's installation that covers a repository.
    pub async fn repo_installation(&self, repo: &str) -> Result<i64, Error> {
        self.inner.source.repo_installation(repo).await
    }
}
