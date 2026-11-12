//! A public repository's releases against a fake GitHub at the HTTP edge: what a release lists, how two commits stand,
//! and a release's file fetched whole, hashed, begun again after a try that broke off, and refused when it cannot be
//! kept.

use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cf_fake_github::httpd::{Handler, Reply, Request, Server};
use cf_ghapp::{Compared, Error, Releases, RetryPolicy, Sink};
use sha2::{Digest as _, Sha256};

const REPO: &str = "codefusion-cc/runner";

/// A server that answers each request by `reply` and keeps them all.
struct Fake {
    server: Server,
    requests: Arc<Mutex<Vec<Request>>>,
}

fn fake(reply: impl Fn(&Request, usize) -> Reply + Send + Sync + 'static) -> Fake {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    let handler: Handler = Arc::new(move |request: Request| {
        let reply = {
            let mut seen = seen.lock().unwrap();
            seen.push(request.clone());
            reply(&request, seen.len())
        };
        Box::pin(async move { reply })
    });
    Fake {
        server: futures_block(Server::start(handler)),
        requests,
    }
}

/// Server::start needs the test's runtime; it is already in one.
fn futures_block<F: std::future::Future>(future: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

impl Fake {
    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().iter().map(Request::line).collect()
    }
}

fn quick() -> RetryPolicy {
    RetryPolicy {
        retries: 3,
        base: Duration::from_millis(10),
        cap: Duration::from_millis(50),
        max_retry_after: Duration::from_secs(5),
    }
}

fn releases(fake: &Fake) -> Releases {
    Releases::new(&fake.server.url(), REPO, quick()).expect("releases")
}

fn file(body: &[u8]) -> Reply {
    Reply::Response {
        status: 200,
        headers: vec![("content-type".into(), "application/octet-stream".into())],
        body: body.to_vec(),
    }
}

/// The program a release would carry: a megabyte that is not all the same byte.
fn program() -> Vec<u8> {
    (0..1_048_576u32).map(|n| (n % 251) as u8).collect()
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// A file in a folder of the test's own.
struct Target {
    _folder: tempfile::TempDir,
    path: std::path::PathBuf,
    file: std::fs::File,
}

fn target() -> Target {
    let folder = tempfile::tempdir().expect("folder");
    let path = folder.path().join("download");
    let file = std::fs::File::create(&path).expect("file");
    Target {
        _folder: folder,
        path,
        file,
    }
}

static NO_STOP: AtomicBool = AtomicBool::new(false);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_is_fetched_whole_and_hashed() {
    let bytes = program();
    let body = bytes.clone();
    let fake = fake(move |_, _| file(&body));
    let mut target = target();
    let url = format!(
        "{}/download/runner-0.1.0-aaaaaaaaaaaa/codefusion-runner-linux-arm64",
        fake.server.url()
    );
    let got = releases(&fake)
        .download(&url, &mut target.file, 2 << 20, &NO_STOP)
        .await
        .expect("fetched");
    assert_eq!(got.bytes, bytes.len() as u64);
    assert_eq!(got.sha256, sha256(&bytes));
    assert_eq!(std::fs::read(&target.path).expect("the file"), bytes);
    assert_eq!(fake.requests().len(), 1);
}

/// GitHub sends a release's file on to its storage, on another host: followed, and no token goes anywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_sent_on_to_another_host_is_followed() {
    let bytes = program();
    let body = bytes.clone();
    let storage = fake(move |_, _| file(&body));
    let to = format!("{}/objects/1", storage.server.url());
    let github = fake(move |_, _| Reply::empty(302).with_header("location", &to));
    let mut target = target();
    let got = releases(&github)
        .download(
            &format!("{}/download/x", github.server.url()),
            &mut target.file,
            2 << 20,
            &NO_STOP,
        )
        .await
        .expect("fetched");
    assert_eq!(got.sha256, sha256(&bytes));
    assert_eq!(storage.requests(), ["GET /objects/1"]);
    let sent = storage.requests.lock().unwrap()[0].clone();
    assert_eq!(sent.header("authorization"), None);
}

/// A download that breaks off half way begins again from the first byte: the file holds the program once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interrupted_download_begins_again_and_holds_the_file_once() {
    let bytes = program();
    let body = bytes.clone();
    let fake = fake(move |_, n| {
        if n == 1 {
            Reply::Truncated {
                status: 200,
                body: body.clone(),
                sent: body.len() / 2,
            }
        } else {
            file(&body)
        }
    });
    let mut target = target();
    let got = releases(&fake)
        .download(
            &format!("{}/f", fake.server.url()),
            &mut target.file,
            2 << 20,
            &NO_STOP,
        )
        .await
        .expect("fetched on the second try");
    assert_eq!(fake.requests().len(), 2);
    assert_eq!(got.bytes, bytes.len() as u64);
    assert_eq!(got.sha256, sha256(&bytes));
    assert_eq!(std::fs::read(&target.path).expect("the file"), bytes);
}

/// One that breaks off every time ends after the tries, as a network fault.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_download_that_always_breaks_off_ends_after_the_tries() {
    let body = program();
    let fake = fake(move |_, _| Reply::Truncated {
        status: 200,
        body: body.clone(),
        sent: 1000,
    });
    let mut target = target();
    let error = releases(&fake)
        .download(
            &format!("{}/f", fake.server.url()),
            &mut target.file,
            2 << 20,
            &NO_STOP,
        )
        .await
        .expect_err("broke off");
    assert!(
        matches!(&error, Error::Transport(why) if why.contains("ended early")),
        "{error:?}"
    );
    assert_eq!(fake.requests().len(), 4, "the first try and three more");
}

/// GitHub's rate limit and its server errors are waited out; a file that is not there is not asked for again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_429_or_a_5xx_is_asked_again_and_a_404_is_not() {
    for (first, asked) in [
        (Reply::empty(429).with_header("retry-after", "1"), 2),
        (Reply::empty(502), 2),
        (Reply::empty(503), 2),
    ] {
        let bytes = program();
        let body = bytes.clone();
        let first = Mutex::new(Some(first));
        let fake = fake(move |_, _| first.lock().unwrap().take().unwrap_or_else(|| file(&body)));
        let mut target = target();
        let got = releases(&fake)
            .download(
                &format!("{}/f", fake.server.url()),
                &mut target.file,
                2 << 20,
                &NO_STOP,
            )
            .await
            .expect("fetched after the wait");
        assert_eq!(got.sha256, sha256(&bytes));
        assert_eq!(fake.requests().len(), asked);
    }
    let fake = fake(|_, _| Reply::status_json(404, &serde_json::json!({"message": "Not Found"})));
    let mut target = target();
    let error = releases(&fake)
        .download(
            &format!("{}/f", fake.server.url()),
            &mut target.file,
            2 << 20,
            &NO_STOP,
        )
        .await
        .expect_err("not there");
    assert_eq!(error.status(), Some(404));
    assert_eq!(fake.requests().len(), 1);
}

/// A sink with room for `room` bytes, as a disk that fills up.
struct Full {
    written: Vec<u8>,
    room: usize,
    restarts: usize,
}

impl Write for Full {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.written.len() + bytes.len() > self.room {
            return Err(std::io::Error::from(std::io::ErrorKind::StorageFull));
        }
        self.written.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Sink for Full {
    fn restart(&mut self) -> std::io::Result<()> {
        self.restarts += 1;
        self.written.clear();
        Ok(())
    }
}

/// A disk that fills up ends the download at once, said as such: asking GitHub again finds no more room.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_disk_ends_the_download_at_once() {
    let body = program();
    let fake = fake(move |_, _| file(&body));
    let mut full = Full {
        written: Vec::new(),
        room: 4096,
        restarts: 0,
    };
    let error = releases(&fake)
        .download(&format!("{}/f", fake.server.url()), &mut full, 2 << 20, &NO_STOP)
        .await
        .expect_err("no room");
    assert!(
        matches!(&error, Error::Local(why) if why.starts_with("no room on this host's disk")),
        "{error:?}"
    );
    assert_eq!((fake.requests().len(), full.restarts), (1, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_over_its_limit_is_refused_and_one_not_over_https_is_not_asked_for() {
    let body = program();
    let fake = fake(move |_, _| file(&body));
    let mut target = target();
    let error = releases(&fake)
        .download(
            &format!("{}/f", fake.server.url()),
            &mut target.file,
            1000,
            &NO_STOP,
        )
        .await
        .expect_err("too long");
    assert!(
        matches!(&error, Error::Refused(why) if why.contains("over 1000 bytes")),
        "{error:?}"
    );
    assert_eq!(fake.requests().len(), 1, "not asked again");

    for url in ["http://example.com/f", "ftp://example.com/f", "no address"] {
        let error = releases(&fake)
            .download(url, &mut target.file, 1000, &NO_STOP)
            .await
            .expect_err(url);
        assert!(matches!(error, Error::Refused(_)), "{url}: {error:?}");
    }
    assert_eq!(fake.requests().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_download_is_stopped_when_the_runner_ends() {
    let body = program();
    let fake = fake(move |_, _| file(&body));
    let mut target = target();
    let stop = AtomicBool::new(true);
    let error = releases(&fake)
        .download(
            &format!("{}/f", fake.server.url()),
            &mut target.file,
            2 << 20,
            &stop,
        )
        .await
        .expect_err("stopped");
    assert_eq!(error, Error::Local("the download was stopped".into()));
    assert_eq!(fake.requests().len(), 1, "not asked again");
}

/// The Runner ends while the connection stalls, or while a download waits to be tried again: it ends at once, not
/// after the read timeout or the wait GitHub asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_download_that_stalls_or_waits_ends_when_the_runner_ends() {
    let body = program();
    for reply in [
        Reply::Stalled {
            body: body.clone(),
            sent: 1000,
        },
        Reply::empty(429).with_header("retry-after", "4"),
    ] {
        let fake = fake(move |_, _| reply.clone());
        let mut target = target();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let began = std::time::Instant::now();
        let ends = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            stopping.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let error = releases(&fake)
            .download(
                &format!("{}/f", fake.server.url()),
                &mut target.file,
                2 << 20,
                &stop,
            )
            .await
            .expect_err("stopped");
        ends.join().unwrap();
        assert_eq!(error, Error::Local("the download was stopped".into()));
        assert!(began.elapsed() < Duration::from_secs(2), "{:?}", began.elapsed());
        assert_eq!(fake.requests().len(), 1);
    }
}

/// GitHub's rate limit used up (403, no Retry-After, `x-ratelimit-remaining: 0`) is waited out until its reset; any
/// other 403, or a reset later than the longest wait, is not asked again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_used_up_rate_limit_is_waited_out_until_its_reset() {
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    let limited = |reset: u64| {
        Reply::status_json(403, &serde_json::json!({"message": "API rate limit exceeded"}))
            .with_header("x-ratelimit-remaining", "0")
            .with_header("x-ratelimit-reset", &reset.to_string())
    };
    let release = serde_json::json!({"tag_name": "runner-0.1.0-aaaaaaaaaaaa", "target_commitish": "x"});
    for (first, asked, found) in [
        (limited(now() + 1), 2, true),
        (limited(now() - 30), 2, true),
        (limited(now() + 3600), 1, false),
        (
            Reply::status_json(403, &serde_json::json!({"message": "Resource not accessible"})),
            1,
            false,
        ),
        (
            Reply::status_json(403, &serde_json::json!({"message": "x"}))
                .with_header("x-ratelimit-remaining", "12")
                .with_header("x-ratelimit-reset", &(now() + 1).to_string()),
            1,
            false,
        ),
    ] {
        let first = Mutex::new(Some(first));
        let ok = Reply::json(&release);
        let fake = fake(move |_, _| first.lock().unwrap().take().unwrap_or_else(|| ok.clone()));
        let began = std::time::Instant::now();
        let got = releases(&fake).by_tag("runner-0.1.0-aaaaaaaaaaaa").await;
        assert_eq!(got.is_ok(), found, "{got:?}");
        if !found {
            assert_eq!(got.expect_err("refused").status(), Some(403));
        }
        assert_eq!(fake.requests().len(), asked);
        assert!(began.elapsed() < Duration::from_secs(4), "{:?}", began.elapsed());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_compare_says_how_two_commits_stand_and_nothing_else() {
    for (status, said) in [
        ("ahead", Ok(Compared::Ahead)),
        ("behind", Ok(Compared::Behind)),
        ("identical", Ok(Compared::Identical)),
        ("diverged", Ok(Compared::Diverged)),
        ("sideways", Err(())),
    ] {
        let fake = fake(move |_, _| Reply::json(&serde_json::json!({ "status": status, "commits": [] })));
        let got = releases(&fake).compare("aaaaaaaaaaaa", "bbbbbbbbbbbb").await;
        assert_eq!(got.map_err(|_| ()), said, "{status}");
        assert_eq!(
            fake.requests(),
            [format!("GET /repos/{REPO}/compare/aaaaaaaaaaaa...bbbbbbbbbbbb")]
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_branch_head_is_read_from_its_ref_and_must_name_a_whole_commit() {
    let head = "a".repeat(40);
    let answer = head.clone();
    let fake = fake(move |_, n| {
        let sha = if n == 1 { answer.clone() } else { "main".into() };
        Reply::json(
            &serde_json::json!({ "ref": "refs/heads/main", "object": { "sha": sha, "type": "commit" } }),
        )
    });
    let releases = releases(&fake);
    assert_eq!(releases.branch_head("main").await, Ok(head));
    assert!(matches!(
        releases.branch_head("main").await,
        Err(Error::Decode { .. })
    ));
    assert_eq!(
        fake.requests()[0],
        format!("GET /repos/{REPO}/git/ref/heads/main")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_of_releases_is_read_with_its_files() {
    let fake = fake(|request, _| {
        assert_eq!(
            (request.query("per_page"), request.query("page")),
            (Some("30"), Some("2"))
        );
        Reply::json(&serde_json::json!([{
            "tag_name": "runner-0.1.0-aaaaaaaaaaaa",
            "target_commitish": "a".repeat(40),
            "draft": false,
            "author": { "login": "github-actions[bot]" },
            "assets": [{ "name": "SHA256SUMS", "size": 300, "browser_download_url": "https://github.com/x/SHA256SUMS" }],
        }]))
    });
    let page = releases(&fake).page(2).await.expect("a page");
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].asset("SHA256SUMS").map(|asset| asset.size), Some(300));
    assert_eq!(page[0].asset("other"), None);
    assert_eq!(
        page[0].author.as_ref().map(|a| a.login.as_str()),
        Some("github-actions[bot]")
    );
}

#[test]
fn a_repository_or_api_that_is_not_plain_is_refused() {
    for repo in ["codefusion-cc", "a/b/c", "/runner", "a/", "a b/c", "a/../b"] {
        assert!(
            Releases::new("https://api.github.com", repo, quick()).is_err(),
            "{repo}"
        );
    }
    assert!(Releases::new("http://api.github.com", REPO, quick()).is_err());
    assert!(Releases::new("https://api.github.com/", REPO, quick()).is_ok());
}
