//! How a Runner installed from a release (`host-setup.sh install --version`, no build folder, no toolchain) updates
//! itself: to the commit the console's `update-to` names, from that commit's release of `codefusion-cc/runner`. The
//! service decides when, as for a build ([`cf_daemon::Service`]): the release is fetched and checked while jobs
//! run, put in place once no machine stands, and the program that starts next counts its starts and goes back to
//! the one before when it does not stay up ([`crate::update::guard`]).
//!
//! A release is taken only when it is the release workflow's, of a commit main's history has after the running one,
//! and its program for this host's architecture has the SHA-256 the release's `SHA256SUMS` names and says it is of
//! that commit. Plain `update` (main's head) is a build's: it is not offered here.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use cf_daemon::update::{NO_HEAD, Pending, SelfUpdate, UpdateFrom, is_commit};
use cf_ghapp::{Compared, Release, Releases, RetryPolicy, is_whole_commit};

use crate::commands::VERSION;
use crate::update::{Installer, cut, running_commit};

/// The repository the Runner's releases are in.
pub const REPO: &str = "codefusion-cc/runner";
/// The branch whose history decides which commit is newer.
const BRANCH: &str = "main";
/// Who makes the Runner's releases: the release workflow, with its own token. A release anyone else made is not
/// taken.
pub const PUBLISHER: &str = "github-actions[bot]";
/// The list of the release's files and their SHA-256.
const SUMS: &str = "SHA256SUMS";
/// The longest `SHA256SUMS` read: it names three files.
const MAX_SUMS: u64 = 64 * 1024;
/// The largest program taken: the Runner with its guest and cloud-hypervisor is some tens of megabytes.
const MAX_PROGRAM: u64 = 512 * 1024 * 1024;
/// How long a release that is not out is waited for: the console sends a commit once its CI is green, and the
/// release workflow publishes it minutes after, from what it built while CI ran; as long as a build may take.
pub const RELEASE_WAIT: Duration = Duration::from_secs(45 * 60);
/// How often GitHub is asked for it meanwhile: GitHub answers 60 reads an hour to a host without a token.
pub const ASK_EVERY: Duration = Duration::from_secs(3 * 60);
/// Of those looks, every how many also reads the newest releases, for a tag of another package version.
const PAGE_EVERY: u32 = 5;
/// The name a release's program is fetched to beside the installed one.
const DOWNLOAD: &str = "download";

/// The asset of the Runner's release for this host, or `None` where the release has none.
pub fn asset_for_this_host() -> Option<&'static str> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    match std::env::consts::ARCH {
        "x86_64" => Some("codefusion-runner-linux-amd64"),
        "aarch64" => Some("codefusion-runner-linux-arm64"),
        _ => None,
    }
}

/// Whether `version` is a package version, three numbers: `0.1.0`.
fn is_package_version(version: &str) -> bool {
    version.split('.').count() == 3
        && version
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// The tag of a release of version `version` (`0.1.0`) at the commit `short` (12 characters).
pub fn tag_of(version: &str, short: &str) -> String {
    format!("runner-{version}-{short}")
}

/// The update of a Runner that came from a release.
pub struct ReleaseUpdate {
    /// Where the program fetched waits, and how it is put in place.
    pub installer: Installer,
    /// The running version.
    pub running: String,
    /// The repository's releases.
    releases: Releases,
    /// The file of a release this host runs.
    asset: String,
    /// The package version whose tag is asked for first.
    package: String,
    /// How long a release that is not out is waited for, asked for every so often.
    wait: (Duration, Duration),
    /// Set as the Runner ends: a download or a wait under way ends with it. The service runs one update at a time.
    stop: AtomicBool,
}

impl ReleaseUpdate {
    /// The update of the running program from the releases at `github_api`, on a host the Runner's release has a
    /// program for; `None` elsewhere.
    pub fn of_this_program(installer: Installer, github_api: &str) -> Option<ReleaseUpdate> {
        let asset = asset_for_this_host()?;
        let retry = RetryPolicy {
            retries: 5,
            base: Duration::from_secs(2),
            cap: Duration::from_secs(60),
            max_retry_after: Duration::from_secs(15 * 60),
        };
        match ReleaseUpdate::new(installer, github_api, asset, retry) {
            Ok(update) => Some(update),
            Err(why) => {
                tracing::warn!(%why, "the Runner cannot update from its releases");
                None
            }
        }
    }

    /// The update from the releases at `github_api`, taking the file `asset`, each call tried again as `retry` says.
    pub fn new(
        installer: Installer,
        github_api: &str,
        asset: &str,
        retry: RetryPolicy,
    ) -> Result<ReleaseUpdate, String> {
        Ok(ReleaseUpdate {
            installer,
            running: VERSION.into(),
            releases: Releases::new(github_api, REPO, retry).map_err(|error| error.to_string())?,
            asset: asset.to_string(),
            package: env!("CARGO_PKG_VERSION").into(),
            wait: (RELEASE_WAIT, ASK_EVERY),
            stop: AtomicBool::new(false),
        })
    }

    /// Waits at most `wait` for a release that is not out, asking every `every`.
    #[cfg(test)]
    pub fn wait_for(&mut self, wait: Duration, every: Duration) {
        self.wait = (wait, every);
    }

    /// The file a release's program is fetched to.
    fn download(&self) -> PathBuf {
        self.installer.installed.with_extension(DOWNLOAD)
    }

    /// The release of the commit `short`, its tag that of the running package's version or, after a version
    /// changed, any `runner-<version>-<short>` of the newest releases (read at the first look and every
    /// [`PAGE_EVERY`]th). Waits for one that is not out yet.
    async fn release_of(&self, short: &str) -> Result<Release, String> {
        let (wait, every) = self.wait;
        let began = Instant::now();
        let mut look = 0u32;
        loop {
            let last = began.elapsed() + every > wait || self.stop.load(Ordering::SeqCst);
            if let Some(release) = self
                .look_for(short, last || look.is_multiple_of(PAGE_EVERY))
                .await?
            {
                return Ok(release);
            }
            if last {
                return Err(format!(
                    "{short} has no release of {REPO} (asked for {}); nothing was downloaded",
                    cf_daemon::update::spoken(began.elapsed())
                ));
            }
            tracing::info!(commit = short, "its release is not out yet; asking again");
            cf_ghapp::sleep_unless(&self.stop, every).await;
            look += 1;
        }
    }

    async fn look_for(&self, short: &str, with_page: bool) -> Result<Option<Release>, String> {
        let unread = |error: cf_ghapp::Error| {
            format!("the releases of {REPO} could not be read: {error}; nothing was downloaded")
        };
        if let Some(release) = self
            .releases
            .by_tag(&tag_of(&self.package, short))
            .await
            .map_err(unread)?
        {
            return Ok(Some(release));
        }
        if !with_page {
            return Ok(None);
        }
        let ends = format!("-{short}");
        let page = self.releases.page(1).await.map_err(unread)?;
        Ok(page.into_iter().find(|release| {
            release
                .tag_name
                .strip_prefix("runner-")
                .and_then(|rest| rest.strip_suffix(&ends))
                .is_some_and(is_package_version)
        }))
    }

    /// Finds the release of `name`, checks it is newer on main than `running`, and fetches and checks its program
    /// into [`ReleaseUpdate::download`]. Answers whether it fetched one: not when `running` is that commit already.
    async fn fetch(&self, name: &str, short: &str, running: &str) -> Result<bool, String> {
        let release = self.release_of(short).await?;
        let tag = &release.tag_name;
        let not = "nothing was downloaded";
        if release.draft {
            return Err(format!("the release {tag} is a draft; {not}"));
        }
        let made_by = release
            .author
            .as_ref()
            .map_or("nobody", |author| author.login.as_str());
        if made_by != PUBLISHER {
            return Err(format!(
                "the release {tag} was made by {}, not by the release workflow; {not}",
                cut(made_by, 40)
            ));
        }
        let full = &release.target_commitish;
        if !is_whole_commit(full) || !full.starts_with(name) {
            return Err(format!(
                "the release {tag} names the commit `{}`, not {name}; {not}",
                cut(full, 40)
            ));
        }
        let github = |error: cf_ghapp::Error| {
            format!("GitHub could not say how {short} stands on {BRANCH}: {error}; {not}")
        };
        let head = self.releases.branch_head(BRANCH).await.map_err(github)?;
        match self.releases.compare(full, &head).await.map_err(github)? {
            Compared::Ahead | Compared::Identical => {}
            Compared::Behind | Compared::Diverged => {
                return Err(format!("{short} is not on {BRANCH} (at {}); {not}", &head[..12]));
            }
        }
        match self.releases.compare(running, full).await.map_err(github)? {
            Compared::Ahead => {}
            Compared::Identical => return Ok(false),
            Compared::Behind => {
                return Err(format!(
                    "{short} is not newer than {running}, which runs: a Runner is not taken back; {not}"
                ));
            }
            Compared::Diverged => {
                return Err(format!(
                    "{short} does not have {running}, which runs and is not on {BRANCH}; update it by hand; {not}"
                ));
            }
        }
        let not = "nothing was installed";
        let Some(sums) = release.asset(SUMS) else {
            return Err(format!("the release {tag} has no {SUMS}; {not}"));
        };
        let Some(program) = release.asset(&self.asset) else {
            return Err(format!(
                "the release {tag} has no {} for this host's architecture; {not}",
                self.asset
            ));
        };
        let fetched = |what: &str, error: cf_ghapp::Error| {
            format!("{what} of {tag} could not be fetched: {error}; {not}")
        };
        let mut listed = Vec::new();
        self.releases
            .download(&sums.browser_download_url, &mut listed, MAX_SUMS, &self.stop)
            .await
            .map_err(|error| fetched(SUMS, error))?;
        let want = cf_image::parse_sums(&listed)
            .map_err(|error| format!("{SUMS} of {tag} is no list of SHA-256s: {error}; {not}"))?
            .remove(&self.asset)
            .ok_or_else(|| format!("{SUMS} of {tag} names no SHA-256 of {}; {not}", self.asset))?;
        let path = self.download();
        let mut file = new_file(&path).map_err(|error| format!("{}: {error}; {not}", path.display()))?;
        let got = self
            .releases
            .download(&program.browser_download_url, &mut file, MAX_PROGRAM, &self.stop)
            .await
            .map_err(|error| fetched(&self.asset, error))?;
        drop(file);
        let got = got.sha256;
        if got != want {
            return Err(format!(
                "{} of {tag} has the SHA-256 {got}, not the {want} its {SUMS} names; {not}",
                self.asset
            ));
        }
        Ok(true)
    }
}

impl SelfUpdate for ReleaseUpdate {
    fn source(&self) -> String {
        format!("https://github.com/{REPO} releases")
    }

    fn build(&self, commit: Option<&str>) -> Result<String, String> {
        let running = running_commit(&self.running)?;
        let Some(name) = commit else {
            return Err(NO_HEAD.into());
        };
        let not = "nothing was downloaded";
        if !is_commit(name) {
            return Err(format!("`{}` is no commit's name; {not}", cut(name, 40)));
        }
        if name.len() < 12 {
            return Err(format!(
                "{name} is too short to name a release: send 12 characters of it or more; {not}"
            ));
        }
        let short = &name[..12];
        if short == running {
            return Ok(self.running.clone());
        }
        let path = self.download();
        let fetched = block_on(self.fetch(name, short, running));
        let offered = fetched.and_then(|downloaded| {
            if !downloaded {
                return Ok(self.running.clone());
            }
            make_runnable(&path)
                .map_err(|error| format!("{}: {error}", path.display()))
                .and_then(|()| self.installer.offer(&path, short, &self.asset))
        });
        let _ = std::fs::remove_file(&path);
        offered
    }

    fn install(&self, pending: &Pending) -> Result<(), String> {
        self.installer.install(pending)
    }

    fn updates_from(&self) -> UpdateFrom {
        UpdateFrom::Releases
    }

    fn end(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Runs `future` to its end from a thread that may wait: the service's runtime when the thread is one of its
/// blocking ones, else a runtime of its own.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.block_on(future),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(future),
    }
}

/// A new file for this user alone, in place of any left before.
fn new_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let _ = std::fs::remove_file(path);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

fn make_runnable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};

    use cf_daemon::update::{MAX_STARTS, UpdateFile};
    use cf_fake_github::httpd::{Handler, Reply, Request, Server};
    use sha2::{Digest as _, Sha256};

    use super::*;
    use crate::update::{Started, guard};

    const ASSET: &str = "codefusion-runner-linux-arm64";
    const OTHER: &str = "codefusion-runner-linux-amd64";
    const RUNNING: &str = "aaaaaaaaaaaa";
    const RUNNING_FULL: &str = "aaaaaaaaaaaa0000000000000000000000000000";
    const NEW: &str = "bbbbbbbbbbbb1111111111111111111111111111";
    const HEAD: &str = "cccccccccccc2222222222222222222222222222";

    fn fits(_: &Path) -> Result<(), String> {
        Ok(())
    }

    /// A program that says it is of `short`, as the Runner's `version` does.
    fn program(short: &str) -> Vec<u8> {
        format!("#!/bin/sh\necho \"codefusion-runner 0.1.0+{short}\"\n").into_bytes()
    }

    fn sum(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    /// What the fake GitHub holds and answers.
    #[derive(Default)]
    struct Hub {
        releases: Vec<serde_json::Value>,
        files: HashMap<String, Vec<u8>>,
        compared: HashMap<(String, String), &'static str>,
        /// Answers given before the usual one, by path, in turn.
        faults: HashMap<String, VecDeque<Reply>>,
        seen: Vec<String>,
    }

    struct Bench {
        _folder: tempfile::TempDir,
        hub: Arc<Mutex<Hub>>,
        _server: Server,
        runtime: tokio::runtime::Runtime,
        update: ReleaseUpdate,
    }

    fn reply_for(hub: &mut Hub, request: &Request, base: &str) -> Reply {
        let path = request.path.clone();
        hub.seen.push(path.clone());
        if let Some(reply) = hub.faults.get_mut(&path).and_then(VecDeque::pop_front) {
            return reply;
        }
        let repo = format!("/repos/{REPO}/");
        if let Some(rest) = path.strip_prefix(&repo) {
            if let Some(tag) = rest.strip_prefix("releases/tags/") {
                return hub
                    .releases
                    .iter()
                    .find(|release| release["tag_name"] == tag)
                    .map_or_else(
                        || Reply::status_json(404, &serde_json::json!({"message": "Not Found"})),
                        |release| Reply::json(&with_urls(release, base)),
                    );
            }
            if rest == "releases" {
                let all: Vec<_> = hub
                    .releases
                    .iter()
                    .map(|release| with_urls(release, base))
                    .collect();
                return Reply::json(&serde_json::Value::Array(all));
            }
            if rest == "git/ref/heads/main" {
                return Reply::json(&serde_json::json!({"ref": "refs/heads/main", "object": {"sha": HEAD}}));
            }
            if let Some((base, head)) = rest
                .strip_prefix("compare/")
                .and_then(|pair| pair.split_once("..."))
            {
                let status = hub
                    .compared
                    .get(&(base.to_string(), head.to_string()))
                    .copied()
                    .unwrap_or("diverged");
                return Reply::json(&serde_json::json!({"status": status}));
            }
        }
        match hub.files.get(&path) {
            Some(bytes) => Reply::Response {
                status: 200,
                headers: vec![],
                body: bytes.clone(),
            },
            None => Reply::empty(404),
        }
    }

    /// A release as GitHub lists it, its files' addresses on this server.
    fn with_urls(release: &serde_json::Value, base: &str) -> serde_json::Value {
        let mut release = release.clone();
        let tag = release["tag_name"].as_str().unwrap_or_default().to_string();
        if let Some(assets) = release["assets"].as_array_mut() {
            for asset in assets {
                let name = asset["name"].as_str().unwrap_or_default().to_string();
                asset["browser_download_url"] = format!("{base}/download/{tag}/{name}").into();
            }
        }
        release
    }

    fn release(tag: &str, commit: &str, files: &[(&str, usize)]) -> serde_json::Value {
        serde_json::json!({
            "tag_name": tag,
            "target_commitish": commit,
            "draft": false,
            "author": {"login": PUBLISHER},
            "assets": files.iter().map(|(name, size)| serde_json::json!({"name": name, "size": size, "browser_download_url": ""})).collect::<Vec<_>>(),
        })
    }

    fn quick() -> RetryPolicy {
        RetryPolicy {
            retries: 3,
            base: Duration::from_millis(10),
            cap: Duration::from_millis(50),
            max_retry_after: Duration::from_secs(5),
        }
    }

    /// GitHub with the release of `NEW` (its program for both architectures and their SHA256SUMS), main at `HEAD`
    /// past `NEW` past the running commit, and a host whose installed program runs `RUNNING`.
    fn bench() -> Bench {
        let folder = tempfile::tempdir().expect("folder");
        let state = folder.path().canonicalize().expect("folder");
        std::fs::create_dir_all(state.join("bin")).expect("folder");
        let installed = state.join("bin/codefusion-runner");
        std::fs::write(&installed, "the one that runs").expect("program");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a runtime");
        let hub = Arc::new(Mutex::new(Hub::default()));
        let answers = hub.clone();
        let base = Arc::new(Mutex::new(String::new()));
        let at = base.clone();
        let handler: Handler = Arc::new(move |request: Request| {
            let base = at.lock().expect("base").clone();
            let reply = reply_for(&mut answers.lock().expect("hub"), &request, &base);
            Box::pin(async move { reply })
        });
        let server = runtime.block_on(Server::start(handler));
        *base.lock().expect("base") = server.url();
        let installer = Installer {
            installed,
            records: state.join("run"),
            state_dir: state,
            fit: fits,
        };
        let mut update = ReleaseUpdate::new(installer, &server.url(), ASSET, quick()).expect("an update");
        update.wait_for(Duration::ZERO, Duration::ZERO);
        update.running = format!("0.1.0+{RUNNING}");
        let bench = Bench {
            _folder: folder,
            hub,
            _server: server,
            runtime,
            update,
        };
        bench.publish(NEW, &program(&NEW[..12]), &program(&NEW[..12]));
        {
            let mut hub = bench.hub.lock().expect("hub");
            hub.compared.insert((NEW.into(), HEAD.into()), "ahead");
            hub.compared.insert((RUNNING.into(), NEW.into()), "ahead");
        }
        bench
    }

    impl Bench {
        fn tag(commit: &str) -> String {
            tag_of(env!("CARGO_PKG_VERSION"), &commit[..12])
        }

        /// The release of `commit` with these programs for this host's architecture and the other, and the
        /// SHA256SUMS of both.
        fn publish(&self, commit: &str, ours: &[u8], other: &[u8]) {
            let tag = Bench::tag(commit);
            let sums = format!(
                "{}  {ASSET}\n{}  {OTHER}\n{}  host-setup.sh\n",
                sum(ours),
                sum(other),
                sum(b"script")
            );
            let mut hub = self.hub.lock().expect("hub");
            hub.releases.retain(|release| release["tag_name"] != tag.as_str());
            hub.releases.insert(
                0,
                release(
                    &tag,
                    commit,
                    &[(ASSET, ours.len()), (OTHER, other.len()), (SUMS, sums.len())],
                ),
            );
            hub.files
                .insert(format!("/download/{tag}/{ASSET}"), ours.to_vec());
            hub.files
                .insert(format!("/download/{tag}/{OTHER}"), other.to_vec());
            hub.files
                .insert(format!("/download/{tag}/{SUMS}"), sums.into_bytes());
        }

        fn change_release(&self, commit: &str, change: impl FnOnce(&mut serde_json::Value)) {
            let tag = Bench::tag(commit);
            let mut hub = self.hub.lock().expect("hub");
            let release = hub
                .releases
                .iter_mut()
                .find(|release| release["tag_name"] == tag.as_str())
                .expect("the release");
            change(release);
        }

        fn fault(&self, path: &str, replies: Vec<Reply>) {
            self.hub
                .lock()
                .expect("hub")
                .faults
                .insert(path.to_string(), replies.into());
        }

        fn file_path(commit: &str, file: &str) -> String {
            format!("/download/{}/{file}", Bench::tag(commit))
        }

        fn seen(&self) -> Vec<String> {
            self.hub.lock().expect("hub").seen.clone()
        }

        fn downloads(&self) -> Vec<String> {
            self.seen()
                .into_iter()
                .filter(|path| path.starts_with("/download/"))
                .collect()
        }

        /// What the host's bin folder holds besides the program that runs.
        fn left(&self) -> Vec<String> {
            let mut names: Vec<String> =
                std::fs::read_dir(self.update.installer.installed.parent().expect("bin"))
                    .expect("bin")
                    .flatten()
                    .map(|entry| entry.file_name().into_string().expect("a name"))
                    .filter(|name| name != "codefusion-runner")
                    .collect();
            names.sort();
            names
        }

        fn installed(&self) -> Vec<u8> {
            std::fs::read(&self.update.installer.installed).expect("the program")
        }

        fn pending(&self, to: &str) -> Pending {
            Pending {
                id: "cmd-1".into(),
                from: self.update.running.clone(),
                to: to.into(),
                starts: 0,
                failed: None,
            }
        }
    }

    /// What a refused update leaves: the program that runs, nothing beside it, nothing written down.
    fn untouched(bench: &Bench) {
        assert_eq!(bench.installed(), b"the one that runs");
        assert_eq!(bench.left(), Vec::<String>::new());
        assert_eq!(
            UpdateFile::in_state(&bench.update.installer.state_dir).read(),
            Ok(None)
        );
    }

    #[test]
    fn a_newer_commit_of_main_is_fetched_checked_and_installed_and_rolled_back_when_it_does_not_stay_up() {
        let bench = bench();
        let version = format!("0.1.0+{}", &NEW[..12]);
        assert_eq!(bench.update.build(Some(NEW)), Ok(version.clone()));
        assert_eq!(
            bench.left(),
            ["codefusion-runner.next"],
            "the download is gone, the program checked waits"
        );
        assert_eq!(
            bench.downloads(),
            [Bench::file_path(NEW, SUMS), Bench::file_path(NEW, ASSET)],
            "only this host's program"
        );
        assert_eq!(
            bench.installed(),
            b"the one that runs",
            "nothing in place before the install"
        );

        let pending = bench.pending(&version);
        bench.update.install(&pending).expect("installed");
        assert_eq!(bench.installed(), program(&NEW[..12]));
        assert_eq!(bench.left(), ["codefusion-runner.prev"]);
        let installed = &bench.update.installer.installed;
        let state = &bench.update.installer.state_dir;
        // The new program fails its health check: it does not stay up, start after start.
        for _ in 0..MAX_STARTS {
            assert!(matches!(
                guard(state, installed, &version),
                Ok(Started::Trying(_))
            ));
        }
        let said = guard(state, installed, &version).expect_err("taken back");
        assert!(said.contains("is back in place"), "{said}");
        assert_eq!(bench.installed(), b"the one that runs");
        match guard(state, installed, &pending.from) {
            Ok(Started::Failed { id, message }) => {
                assert_eq!(id, "cmd-1");
                assert!(
                    message.contains("did not stay up and was taken back"),
                    "{message}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_program_whose_sha256_is_not_the_one_listed_is_not_offered() {
        let bench = bench();
        let listed = program(&NEW[..12]);
        bench.publish(NEW, &listed, b"other");
        // The release's file is another than the one its list names.
        let mut altered = listed.clone();
        altered.extend_from_slice(b"# and one more line\n");
        bench
            .hub
            .lock()
            .expect("hub")
            .files
            .insert(Bench::file_path(NEW, ASSET), altered.clone());
        let said = bench.update.build(Some(NEW)).expect_err("refused");
        assert_eq!(
            said,
            format!(
                "{ASSET} of {} has the SHA-256 {}, not the {} its SHA256SUMS names; nothing was installed",
                Bench::tag(NEW),
                sum(&altered),
                sum(&listed)
            )
        );
        untouched(&bench);
    }

    #[test]
    fn a_release_without_this_hosts_program_or_its_list_is_refused_before_any_download() {
        let bench = bench();
        bench.change_release(NEW, |release| {
            release["assets"]
                .as_array_mut()
                .expect("assets")
                .retain(|asset| asset["name"] != ASSET);
        });
        assert_eq!(
            bench.update.build(Some(NEW)),
            Err(format!(
                "the release {} has no {ASSET} for this host's architecture; nothing was installed",
                Bench::tag(NEW)
            ))
        );
        let bench2 = self::bench();
        bench2.change_release(NEW, |release| {
            release["assets"]
                .as_array_mut()
                .expect("assets")
                .retain(|asset| asset["name"] != SUMS);
        });
        assert_eq!(
            bench2.update.build(Some(NEW)),
            Err(format!(
                "the release {} has no SHA256SUMS; nothing was installed",
                Bench::tag(NEW)
            ))
        );
        for bench in [bench, bench2] {
            assert_eq!(bench.downloads(), Vec::<String>::new());
            untouched(&bench);
        }
    }

    #[test]
    fn a_list_that_does_not_name_this_hosts_program_once_is_refused() {
        let bench = bench();
        let ours = program(&NEW[..12]);
        let tag = Bench::tag(NEW);
        for (list, malformed) in [
            (format!("{}  {OTHER}\n", sum(b"x")), false),
            (String::new(), false),
            (
                format!("{}  {ASSET}\n{}  {ASSET}\n", sum(&ours), sum(&ours)),
                true,
            ),
            (format!("{}  {ASSET}\n", &sum(&ours)[..63]), true),
            (format!("{}  {ASSET}\n", sum(&ours).to_uppercase()), true),
            (format!("{} *{ASSET}\n", sum(&ours)), true),
            (
                format!("{}  {OTHER}\nnot a line\n{}  {ASSET}\n", sum(b"x"), sum(&ours)),
                true,
            ),
        ] {
            bench
                .hub
                .lock()
                .expect("hub")
                .files
                .insert(Bench::file_path(NEW, SUMS), list.clone().into_bytes());
            let said = bench.update.build(Some(NEW)).expect_err("refused");
            if malformed {
                assert!(
                    said.starts_with(&format!("SHA256SUMS of {tag} is no list of SHA-256s: ")),
                    "{list:?}: {said}"
                );
                assert!(said.ends_with("; nothing was installed"), "{said}");
            } else {
                assert_eq!(
                    said,
                    format!("SHA256SUMS of {tag} names no SHA-256 of {ASSET}; nothing was installed"),
                    "{list:?}"
                );
            }
            untouched(&bench);
        }
    }

    #[test]
    fn a_commit_with_no_release_is_refused_and_one_that_comes_out_while_it_waits_is_taken() {
        let mut bench = bench();
        let none = "dddddddddddd3333333333333333333333333333";
        let said = bench.update.build(Some(none)).expect_err("no release");
        assert!(
            said.starts_with(&format!("dddddddddddd has no release of {REPO} (asked for ")),
            "{said}"
        );
        assert!(said.ends_with("; nothing was downloaded"), "{said}");
        assert_eq!(
            bench.seen(),
            [
                format!("/repos/{REPO}/releases/tags/{}", Bench::tag(none)),
                format!("/repos/{REPO}/releases")
            ],
            "the tag of this version, then the newest releases"
        );
        untouched(&bench);

        // Out after two looks: waited for.
        bench
            .update
            .wait_for(Duration::from_secs(5), Duration::from_millis(100));
        let tags = format!("/repos/{REPO}/releases/tags/{}", Bench::tag(NEW));
        let not_found = Reply::status_json(404, &serde_json::json!({"message": "Not Found"}));
        bench.fault(&tags, vec![not_found.clone(), not_found]);
        bench.fault(
            &format!("/repos/{REPO}/releases"),
            vec![
                Reply::json(&serde_json::json!([])),
                Reply::json(&serde_json::json!([])),
            ],
        );
        assert_eq!(bench.update.build(Some(NEW)), Ok(format!("0.1.0+{}", &NEW[..12])));
        let seen = bench.seen();
        let count = |path: &str| seen.iter().filter(|seen| *seen == path).count();
        assert_eq!((count(&tags), count(&format!("/repos/{REPO}/releases"))), (3, 2));
    }

    /// While it waits, a look reads the tag alone; the newest releases only every few looks, and at the last.
    #[test]
    fn a_wait_reads_the_newest_releases_only_now_and_then() {
        let mut bench = bench();
        bench
            .update
            .wait_for(Duration::from_millis(600), Duration::from_millis(40));
        let none = "dddddddddddd3333333333333333333333333333";
        bench.update.build(Some(none)).expect_err("no release");
        let seen = bench.seen();
        let tags = seen
            .iter()
            .filter(|path| path.contains("/releases/tags/"))
            .count();
        let mut page_after = Vec::new();
        let mut look = 0usize;
        for path in &seen {
            if path.contains("/releases/tags/") {
                look += 1;
            } else {
                page_after.push(look);
            }
        }
        assert!(tags >= 10, "{seen:?}");
        let mut wanted: Vec<usize> = (1..=tags)
            .filter(|look| (look - 1) % PAGE_EVERY as usize == 0)
            .collect();
        if !wanted.contains(&tags) {
            wanted.push(tags);
        }
        assert_eq!(page_after, wanted, "{seen:?}");
    }

    #[test]
    fn a_release_of_another_package_version_is_found_among_the_newest() {
        let bench = bench();
        bench.change_release(NEW, |release| {
            release["tag_name"] = format!("runner-0.2.0-{}", &NEW[..12]).into();
        });
        {
            let mut hub = bench.hub.lock().expect("hub");
            for file in [ASSET, SUMS] {
                let bytes = hub.files.remove(&Bench::file_path(NEW, file)).expect("a file");
                hub.files
                    .insert(format!("/download/runner-0.2.0-{}/{file}", &NEW[..12]), bytes);
            }
            // Tags that only look like it are passed over.
            for tag in [
                format!("runner-0.2-{}", &NEW[..12]),
                format!("image-0.2.0-{}", &NEW[..12]),
            ] {
                let mut fake = release(&tag, NEW, &[]);
                fake["assets"] = serde_json::json!([]);
                hub.releases.insert(0, fake);
            }
        }
        assert_eq!(bench.update.build(Some(NEW)), Ok(format!("0.1.0+{}", &NEW[..12])));
    }

    #[test]
    fn github_s_rate_limit_and_server_errors_are_waited_out() {
        let bench = bench();
        let tags = format!("/repos/{REPO}/releases/tags/{}", Bench::tag(NEW));
        bench.fault(
            &tags,
            vec![
                Reply::empty(429).with_header("retry-after", "1"),
                Reply::empty(500),
            ],
        );
        bench.fault(
            &format!("/repos/{REPO}/compare/{NEW}...{HEAD}"),
            vec![Reply::empty(503)],
        );
        bench.fault(
            &format!("/repos/{REPO}/git/ref/heads/main"),
            vec![Reply::empty(502)],
        );
        bench.fault(
            &Bench::file_path(NEW, ASSET),
            vec![Reply::empty(503), Reply::empty(429)],
        );
        assert_eq!(bench.update.build(Some(NEW)), Ok(format!("0.1.0+{}", &NEW[..12])));
        assert_eq!(bench.seen().iter().filter(|path| **path == tags).count(), 3);

        // One that lasts ends the update, said as GitHub's.
        let bench = self::bench();
        bench.fault(&tags, vec![Reply::empty(503); 10]);
        let said = bench.update.build(Some(NEW)).expect_err("GitHub is down");
        assert!(
            said.starts_with(&format!("the releases of {REPO} could not be read: ")),
            "{said}"
        );
        assert!(said.contains("503"), "{said}");
        untouched(&bench);
    }

    #[test]
    fn an_interrupted_download_begins_again_and_one_that_breaks_off_every_time_installs_nothing() {
        let bench = bench();
        let ours = program(&NEW[..12]);
        let cut_short = || Reply::Truncated {
            status: 200,
            body: ours.clone(),
            sent: ours.len() / 2,
        };
        bench.fault(&Bench::file_path(NEW, ASSET), vec![cut_short()]);
        assert_eq!(bench.update.build(Some(NEW)), Ok(format!("0.1.0+{}", &NEW[..12])));
        assert_eq!(
            std::fs::read(bench.update.installer.next()).expect("offered"),
            ours
        );

        let bench = self::bench();
        bench.fault(&Bench::file_path(NEW, ASSET), vec![cut_short(); 10]);
        let said = bench.update.build(Some(NEW)).expect_err("broke off");
        assert!(
            said.starts_with(&format!("{ASSET} of {} could not be fetched: ", Bench::tag(NEW))),
            "{said}"
        );
        assert!(said.ends_with("; nothing was installed"), "{said}");
        untouched(&bench);
    }

    #[test]
    fn a_program_that_does_not_run_here_or_says_another_commit_is_not_offered() {
        // Another architecture's program under this one's name: it checks out, and does not run.
        let bench = bench();
        let elf_of_another = b"\x7fELF\x02\x01\x01\x00not for this machine".to_vec();
        bench.publish(NEW, &elf_of_another, b"other");
        let said = bench.update.build(Some(NEW)).expect_err("does not run");
        assert!(
            said.starts_with(&format!("{ASSET} does not run on this host: ")),
            "{said}"
        );
        untouched(&bench);

        let bench = self::bench();
        bench.publish(NEW, &program("eeeeeeeeeeee"), b"other");
        let said = bench.update.build(Some(NEW)).expect_err("another commit");
        assert_eq!(
            said,
            format!(
                "{ASSET} says it is `codefusion-runner 0.1.0+eeeeeeeeeeee`, not of {}",
                &NEW[..12]
            )
        );
        untouched(&bench);
    }

    #[test]
    fn a_commit_older_than_the_running_one_or_off_main_is_refused_before_any_download() {
        for (pair, status, said) in [
            (
                (RUNNING, NEW),
                "behind",
                format!(
                    "bbbbbbbbbbbb is not newer than {RUNNING}, which runs: a Runner is not taken back; nothing was downloaded"
                ),
            ),
            (
                (RUNNING, NEW),
                "diverged",
                format!(
                    "bbbbbbbbbbbb does not have {RUNNING}, which runs and is not on main; update it by hand; nothing was downloaded"
                ),
            ),
            (
                (NEW, HEAD),
                "behind",
                "bbbbbbbbbbbb is not on main (at cccccccccccc); nothing was downloaded".into(),
            ),
            (
                (NEW, HEAD),
                "diverged",
                "bbbbbbbbbbbb is not on main (at cccccccccccc); nothing was downloaded".into(),
            ),
        ] {
            let bench = bench();
            bench
                .hub
                .lock()
                .expect("hub")
                .compared
                .insert((pair.0.into(), pair.1.into()), status);
            assert_eq!(bench.update.build(Some(NEW)), Err(said), "{pair:?} {status}");
            assert_eq!(bench.downloads(), Vec::<String>::new());
            untouched(&bench);
        }
    }

    #[test]
    fn a_release_not_made_by_the_workflow_or_of_another_commit_is_refused() {
        let tag = Bench::tag(NEW);
        for (change, said) in [
            (
                serde_json::json!({"author": {"login": "someone"}}),
                format!(
                    "the release {tag} was made by someone, not by the release workflow; nothing was downloaded"
                ),
            ),
            (
                serde_json::json!({"author": null}),
                format!(
                    "the release {tag} was made by nobody, not by the release workflow; nothing was downloaded"
                ),
            ),
            (
                serde_json::json!({"draft": true}),
                format!("the release {tag} is a draft; nothing was downloaded"),
            ),
            (
                serde_json::json!({"target_commitish": "main"}),
                format!("the release {tag} names the commit `main`, not {NEW}; nothing was downloaded"),
            ),
            (
                serde_json::json!({"target_commitish": "bbbbbbbbbbbb9999999999999999999999999999"}),
                format!(
                    "the release {tag} names the commit `bbbbbbbbbbbb9999999999999999999999999999`, not {NEW}; nothing \
                     was downloaded"
                ),
            ),
        ] {
            let bench = bench();
            bench.change_release(NEW, |release| {
                for (key, value) in change.as_object().expect("an object") {
                    release[key] = value.clone();
                }
            });
            assert_eq!(bench.update.build(Some(NEW)), Err(said));
            assert_eq!(bench.downloads(), Vec::<String>::new());
            untouched(&bench);
        }
    }

    #[test]
    fn main_s_head_is_not_taken_and_names_that_cannot_name_a_release_ask_nothing() {
        let bench = bench();
        assert_eq!(bench.update.build(None), Err(NO_HEAD.into()));
        assert_eq!(bench.update.updates_from(), UpdateFrom::Releases);
        assert_eq!(
            bench.update.build(Some("bbbbbbb")),
            Err("bbbbbbb is too short to name a release: send 12 characters of it or more; nothing was downloaded".into())
        );
        for name in ["main", "--x", "BBBBBBBBBBBB", "bbbbbbbbbbbb~1"] {
            let said = bench.update.build(Some(name)).expect_err(name);
            assert!(
                said.ends_with("is no commit's name; nothing was downloaded"),
                "{name}: {said}"
            );
        }
        // The commit that runs: nothing to do.
        assert_eq!(
            bench.update.build(Some(RUNNING_FULL)),
            Ok(bench.update.running.clone())
        );
        assert_eq!(bench.seen(), Vec::<String>::new());
        let mut bench = bench;
        bench.update.running = "0.1.0+unknown".into();
        assert!(
            bench
                .update
                .build(Some(NEW))
                .expect_err("no commit")
                .contains("names no commit")
        );
        untouched(&bench);
    }

    /// The service runs one update at a time; a second fetch at once is refused all the same, and fetches nothing.
    /// The Runner ends while a release is waited for: the wait ends with it.
    #[test]
    fn a_wait_for_a_release_ends_when_the_runner_ends() {
        let mut bench = bench();
        bench
            .update
            .wait_for(Duration::from_secs(600), Duration::from_secs(60));
        let began = Instant::now();
        std::thread::scope(|scope| {
            let waits = scope.spawn(|| {
                bench
                    .update
                    .build(Some("dddddddddddd3333333333333333333333333333"))
            });
            while bench.seen().len() < 2 {
                std::thread::sleep(Duration::from_millis(10));
            }
            bench.update.end();
            assert!(
                waits
                    .join()
                    .expect("the wait")
                    .expect_err("ended")
                    .contains("has no release")
            );
        });
        assert!(began.elapsed() < Duration::from_secs(10), "{:?}", began.elapsed());
    }

    /// The service fetches on a thread of its runtime's blocking pool, whose runtime the download uses.
    #[test]
    fn a_fetch_on_the_service_s_blocking_pool_uses_its_runtime() {
        let Bench {
            _folder,
            hub: _hub,
            _server,
            runtime,
            update,
        } = bench();
        let update = Arc::new(update);
        let fetching = update.clone();
        let said = runtime
            .block_on(runtime.spawn_blocking(move || fetching.build(Some(NEW))))
            .expect("the thread");
        assert_eq!(said, Ok(format!("0.1.0+{}", &NEW[..12])));
    }

    #[test]
    fn this_host_takes_its_own_architecture_s_program() {
        let asset = asset_for_this_host();
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => assert_eq!(asset, Some(OTHER)),
            ("linux", "aarch64") => assert_eq!(asset, Some(ASSET)),
            _ => assert_eq!(asset, None, "no release is made for this system"),
        }
    }
}
