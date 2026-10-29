//! `scripts/host-setup.sh` reads GitHub's API for the newest image. GitHub's rate limit answers a fresh host with a 403
//! or 429: the host waits as the answer says and asks again, and sends the token it is given.

use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cf_fake_github::httpd::{Handler, Reply, Request, Server};

/// A server that gives `replies` in turn (the last one again once they run out) and keeps each request.
struct Api {
    server: Server,
    requests: Arc<Mutex<Vec<Request>>>,
    _runtime: tokio::runtime::Runtime,
}

fn api(replies: Vec<Reply>) -> Api {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("a runtime");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    let replies = Arc::new(replies);
    let handler: Handler = Arc::new(move |request: Request| {
        let mut seen = seen.lock().unwrap();
        seen.push(request);
        let reply = replies[(seen.len() - 1).min(replies.len() - 1)].clone();
        Box::pin(async move { reply })
    });
    let server = runtime.block_on(Server::start(handler));
    Api {
        server,
        requests,
        _runtime: runtime,
    }
}

impl Api {
    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

/// `github_get` of the script, sourced, against `url`, with `env` and nothing of the test's own GitHub token.
fn github_get(url: &str, env: &[(&str, &str)]) -> Output {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../scripts/host-setup.sh");
    let mut command = Command::new("bash");
    command
        .args(["-c", "source \"$1\" && github_get \"$2\"", "bash", script, url])
        .env_remove("GITHUB_TOKEN");
    for (name, value) in env {
        command.env(name, value);
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("bash runs");
    // A wait the script should have cut short ends the test, not the suite.
    let deadline = Instant::now() + Duration::from_secs(60);
    while child.try_wait().expect("bash's status").is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("github_get still waits after a minute");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child.wait_with_output().expect("bash's output")
}

fn rate_limited(status: u16) -> Reply {
    Reply::status_json(
        status,
        &serde_json::json!({ "message": "API rate limit exceeded" }),
    )
}

fn releases() -> Reply {
    Reply::json(&serde_json::json!([{ "tag_name": "image-2026.10.29-1", "draft": false }]))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

#[test]
fn a_403_with_retry_after_is_asked_again_after_that_wait() {
    let api = api(vec![
        rate_limited(403).with_header("Retry-After", "1"),
        releases(),
    ]);
    let started = Instant::now();
    let out = github_get(&format!("{}/releases", api.server.url()), &[]);
    assert!(out.status.success(), "{out:?}");
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).expect("the body alone");
    assert_eq!(body[0]["tag_name"], "image-2026.10.29-1");
    assert_eq!(api.requests().len(), 2);
    assert!(started.elapsed() >= Duration::from_secs(1), "it waited");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("GitHub answered 403, asking again in 1s"),
        "{out:?}"
    );
}

#[test]
fn a_429_without_retry_after_waits_until_the_rate_limit_resets() {
    // Further off than the 2 s of a first wait without it.
    let reset = (now() + 4).to_string();
    let api = api(vec![
        rate_limited(429)
            .with_header("x-ratelimit-remaining", "0")
            .with_header("x-ratelimit-reset", &reset),
        releases(),
    ]);
    let started = Instant::now();
    let out = github_get(&format!("{}/releases", api.server.url()), &[]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(api.requests().len(), 2);
    assert!(
        started.elapsed() >= Duration::from_secs(3),
        "it waited for the reset"
    );
}

#[test]
fn the_token_goes_with_every_request_and_none_without_one() {
    let api_with = api(vec![
        rate_limited(403).with_header("Retry-After", "1"),
        releases(),
    ]);
    let out = github_get(
        &format!("{}/releases", api_with.server.url()),
        &[("GITHUB_TOKEN", "ghs_test0000")],
    );
    assert!(out.status.success(), "{out:?}");
    let auth: Vec<_> = api_with
        .requests()
        .iter()
        .map(|r| r.header("authorization").map(str::to_owned))
        .collect();
    assert_eq!(
        auth,
        [
            Some("Bearer ghs_test0000".to_owned()),
            Some("Bearer ghs_test0000".to_owned())
        ]
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("ghs_test0000"),
        "never said"
    );

    let api_without = api(vec![releases()]);
    let out = github_get(&format!("{}/releases", api_without.server.url()), &[]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(api_without.requests()[0].header("authorization"), None);
}

#[test]
fn any_other_failure_ends_at_once() {
    let api = api(vec![Reply::status_json(
        404,
        &serde_json::json!({ "message": "Not Found" }),
    )]);
    let out = github_get(&format!("{}/releases/tags/none", api.server.url()), &[]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty(), "no body of a failure");
    assert_eq!(api.requests().len(), 1);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("GitHub answered 404"),
        "{out:?}"
    );
}

#[test]
fn a_rate_limit_that_lasts_ends_after_the_tries() {
    let api = api(vec![rate_limited(403).with_header("Retry-After", "1")]);
    let out = github_get(
        &format!("{}/releases", api.server.url()),
        &[("GITHUB_TRIES", "2")],
    );
    assert!(!out.status.success());
    assert_eq!(api.requests().len(), 2);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("GitHub answered 403 for"),
        "{out:?}"
    );
}

#[test]
fn a_long_wait_is_cut_to_the_most_a_host_waits() {
    let api = api(vec![
        rate_limited(403).with_header("Retry-After", "3600"),
        releases(),
    ]);
    let started = Instant::now();
    let out = github_get(
        &format!("{}/releases", api.server.url()),
        &[("GITHUB_WAIT_MAX", "1")],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(started.elapsed() < Duration::from_secs(30), "not the hour");
}

#[test]
fn a_server_error_or_no_answer_is_asked_again() {
    let api = api(vec![Reply::empty(503), releases()]);
    let out = github_get(&format!("{}/releases", api.server.url()), &[]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(api.requests().len(), 2);

    // A port nothing listens on: asked twice, then the failure.
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let out = github_get(&format!("http://{closed}/releases"), &[("GITHUB_TRIES", "2")]);
    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("GitHub answered 000, asking again in 2s"), "{said}");
    assert!(said.contains("GitHub answered 000 for"), "{said}");
}
