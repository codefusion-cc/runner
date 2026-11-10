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

/// `merged_config` of the script, sourced: the configuration's text now, the last run's settings, the keys the script
/// owns and the settings of this run.
fn merged(now: &str, last: &str, owned: &str, settings: &str) -> Result<serde_json::Value, String> {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../scripts/host-setup.sh");
    let output = Command::new("bash")
        .args([
            "-c",
            "source \"$1\" && merged_config \"$2\" \"$3\" \"$4\" \"$5\"",
            "bash",
            script,
            now,
            last,
            owned,
            settings,
        ])
        .output()
        .expect("bash runs");
    if output.status.success() {
        Ok(serde_json::from_slice(&output.stdout).expect("JSON"))
    } else {
        Err(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}

const OWNED: &str = r#"{"machine": "lon1", "class": null, "state_dir": "/s", "build_folder": null, "image": "i1",
    "console": "https://c", "github_api": null, "use": "dedicated"}"#;

#[test]
fn a_first_configuration_is_the_owned_keys_without_nulls_and_the_settings() {
    assert_eq!(
        merged("{}", "{}", OWNED, r#"{"ci_memory_gb": 10.6, "ci_cpus": 6}"#),
        Ok(serde_json::json!({
            "machine": "lon1", "state_dir": "/s", "image": "i1", "console": "https://c", "use": "dedicated",
            "ci_memory_gb": 10.6, "ci_cpus": 6
        }))
    );
}

#[test]
fn keys_the_runner_or_the_console_set_stay_and_settings_left_out_go() {
    let now = r#"{"machine": "lon1", "state_dir": "/s", "image": "i0", "console": "https://c", "use": "dedicated",
        "build_folder": "/home/r/runner", "github_api": "http://127.0.0.1:1", "class": "trusted",
        "max_jobs": 2, "ci_memory_gb": 10, "ci_cpus": 6, "vcpus": {"cf-4g": 2}}"#;
    let last = r#"{"ci_memory_gb": 10, "ci_cpus": 6, "vcpus": {"cf-4g": 2}}"#;
    assert_eq!(
        merged(now, last, OWNED, r#"{"ci_memory_gb": 10.6}"#),
        Ok(serde_json::json!({
            "machine": "lon1", "state_dir": "/s", "image": "i1", "console": "https://c", "use": "dedicated",
            "max_jobs": 2, "ci_memory_gb": 10.6
        })),
        "the console's max_jobs stays; ci_cpus and vcpus, set by the last run alone, go; an owned key now null goes"
    );
}

#[test]
fn settings_that_name_an_owned_key_or_are_no_object_are_refused() {
    let refused = merged(
        "{}",
        "{}",
        OWNED,
        r#"{"use": "workstation", "machine": "x", "ci_cpus": 6}"#,
    )
    .expect_err("refused");
    assert!(
        refused.contains("--settings: machine, use only this script sets"),
        "{refused}"
    );
    let refused = merged(
        "{}",
        "{}",
        OWNED,
        r#"{"trusted": {}, "app_dir": "/x", "ci_cpus": 6}"#,
    )
    .expect_err("refused");
    assert!(
        refused.contains("--settings: app_dir, trusted is not set this way"),
        "{refused}"
    );
    for settings in ["[]", "6", "null", r#""ci_cpus""#] {
        let refused = merged("{}", "{}", OWNED, settings).expect_err(settings);
        assert!(
            refused.contains("--settings: no JSON object"),
            "{settings}: {refused}"
        );
    }
}

/// The script run with `args`, as the user the tests run as: everything an argument can be wrong about is refused
/// before it asks for root.
fn setup(args: &[&str]) -> (bool, String) {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../scripts/host-setup.sh");
    let output = Command::new("bash")
        .arg(script)
        .args(args)
        .output()
        .expect("bash runs");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[test]
fn each_step_takes_only_its_own_options() {
    let this = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    let base = ["--machine", "lon1", "--console", "https://console.example"];
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (
            vec!["install", "--code-file", "/root/code"],
            "install takes no code; enroll does",
        ),
        (
            vec!["enroll", "--machine", "lon1"],
            "enroll takes --machine from the configuration",
        ),
        (
            vec!["enroll", "--binary", this],
            "enroll takes --binary from the configuration",
        ),
        (
            vec!["enroll", "--code-file", "/root/code", "--dry-run"],
            "--dry-run: install's alone",
        ),
        (
            [&["install"][..], &base, &["--check"]].concat(),
            "--check: only where both steps run",
        ),
        (
            [&base[..], &["--code-file", "/c", "--dry-run"]].concat(),
            "--dry-run: install's alone",
        ),
        (
            [&["install"][..], &base, &["--binary", this]].concat(),
            "--sha256: the Runner's SHA-256",
        ),
        (
            [
                &["install"][..],
                &base,
                &["--binary", this, "--sha256", &SHA[..63]],
            ]
            .concat(),
            "--sha256: the Runner's SHA-256",
        ),
        (
            [
                &["install"][..],
                &base,
                &[
                    "--binary",
                    this,
                    "--version",
                    "0.1.0-446125c0a1b2",
                    "--sha256",
                    SHA,
                ],
            ]
            .concat(),
            "--binary or --version, not both",
        ),
        (
            [
                &["install"][..],
                &base,
                &["--binary", "/nowhere/runner", "--sha256", SHA],
            ]
            .concat(),
            "--binary: /nowhere/runner is no file",
        ),
        (
            [
                &["install"][..],
                &base,
                &["--version", "0.1.0+446125c0a1b2", "--sha256", SHA],
            ]
            .concat(),
            "--version: a release's version",
        ),
        (
            [
                &["install"][..],
                &base,
                &[
                    "--version",
                    "0.1.0-446125c0a1b2",
                    "--sha256",
                    SHA,
                    "--ref",
                    "abcdef0",
                ],
            ]
            .concat(),
            "--ref builds the Runner",
        ),
        (
            [&["install"][..], &base, &["--sha256", SHA]].concat(),
            "--sha256 goes with --binary or --version",
        ),
        (
            [&["install"][..], &base, &["--settings", "/nowhere.json"]].concat(),
            "--settings: /nowhere.json is no file",
        ),
        (base.to_vec(), "--code-file is needed (or --check)"),
        (vec!["enroll", "--home", "relative"], "--home: an absolute path"),
        (vec!["install", "--machine"], "--machine needs a value"),
    ];
    for (args, said) in cases {
        let (ok, stderr) = setup(&args);
        assert!(!ok, "{args:?} passed");
        assert!(stderr.contains(said), "{args:?}: {stderr}");
    }
}

#[test]
fn right_options_get_as_far_as_asking_for_root() {
    if euid_is_root() {
        return;
    }
    let this = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    let base = ["--machine", "lon1", "--console", "https://console.example"];
    for args in [
        [
            &["install"][..],
            &base,
            &["--binary", this, "--sha256", SHA, "--settings", this, "--dry-run"],
        ]
        .concat(),
        [
            &["install"][..],
            &base,
            &["--version", "0.1.0-446125c0a1b2", "--sha256", SHA],
        ]
        .concat(),
        [
            &base[..],
            &["--version", "0.1.0-446125c0a1b2", "--sha256", SHA, "--check"],
        ]
        .concat(),
        vec!["enroll", "--code-file", "/nowhere/code", "--home", "/home/x"],
    ] {
        let (ok, stderr) = setup(&args);
        assert!(!ok, "{args:?} passed");
        assert!(stderr.contains("run it as root"), "{args:?}: {stderr}");
    }
}

fn euid_is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|output| output.stdout.trim_ascii() == b"0")
}
