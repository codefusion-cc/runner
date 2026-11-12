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

/// `runs_newer` of the script, sourced, against the repository's API at `api`: the Runner that runs at `running`,
/// the release asked for at `wanted`. Its answer and what it said on standard error.
fn runs_newer(api: &str, running: &str, wanted: &str, env: &[(&str, &str)]) -> (String, String) {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../scripts/host-setup.sh");
    let mut command = Command::new("bash");
    command
        .args([
            "-c",
            "source \"$1\" && runs_newer \"$2\" \"$3\" \"$4\"",
            "bash",
            script,
            api,
            running,
            wanted,
        ])
        .env_remove("GITHUB_TOKEN");
    for (name, value) in env {
        command.env(name, value);
    }
    let out = command.output().expect("bash runs");
    assert!(out.status.success(), "it always answers: {out:?}");
    (
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

const RUNS: &str = "bbbbbbbbbbbb";
const ASKED: &str = "aaaaaaaaaaaa";
const MAIN: &str = "cccccccccccc0000000000000000000000000000";

fn compared(status: &str) -> Reply {
    Reply::json(&serde_json::json!({ "status": status, "commits": [] }))
}

fn main_at() -> Reply {
    Reply::json(&serde_json::json!({ "ref": "refs/heads/main", "object": { "sha": MAIN, "type": "commit" } }))
}

fn paths(api: &Api) -> Vec<String> {
    api.requests().iter().map(|r| r.path.clone()).collect()
}

/// A Runner that updated itself past the release a run asks for, to a commit main has, stays: the version is a
/// minimum.
#[test]
fn a_runner_newer_on_main_than_the_version_asked_for_stays() {
    for main_says in ["ahead", "identical"] {
        let api = api(vec![compared("ahead"), main_at(), compared(main_says)]);
        let (said, _) = runs_newer(&api.server.url(), RUNS, ASKED, &[]);
        assert_eq!(said, "keep", "main is {main_says} of it");
        assert_eq!(
            paths(&api),
            [
                format!("/compare/{ASKED}...{RUNS}"),
                "/git/ref/heads/main".to_string(),
                format!("/compare/{RUNS}...{MAIN}"),
            ]
        );
    }
}

#[test]
fn a_runner_older_than_the_version_asked_for_or_off_main_takes_it() {
    for (answers, asked) in [
        (vec![compared("behind")], 1),
        (vec![compared("diverged")], 1),
        // Newer than the one asked for, but on a branch beside main, or a tag named main.
        (vec![compared("ahead"), main_at(), compared("diverged")], 3),
        (vec![compared("ahead"), main_at(), compared("behind")], 3),
    ] {
        let api = api(answers);
        let (said, _) = runs_newer(&api.server.url(), RUNS, ASKED, &[]);
        assert_eq!(said, "install");
        assert_eq!(api.requests().len(), asked);
    }
}

/// The same commit with other bytes, or a program that names no commit: the release asked for goes in, and GitHub is
/// not asked.
#[test]
fn a_runner_of_no_commit_or_of_the_one_asked_for_takes_it_without_asking_github() {
    let api = api(vec![compared("ahead")]);
    for running in [
        ASKED,
        "",
        "unknown",
        "bbbbbbbbbbb",
        "bbbbbbbbbbbbb",
        "BBBBBBBBBBBB",
        "bbbbbbbbbbb-",
    ] {
        let (said, _) = runs_newer(&api.server.url(), running, ASKED, &[]);
        assert_eq!(said, "install", "{running:?}");
    }
    assert_eq!(api.requests().len(), 0);
}

/// GitHub's rate limit and server errors are waited out as every read of the script waits them out.
#[test]
fn a_429_or_a_5xx_is_asked_again_before_it_decides() {
    let api = api(vec![
        rate_limited(429).with_header("Retry-After", "1"),
        compared("ahead"),
        Reply::empty(502),
        main_at(),
        Reply::empty(503),
        compared("ahead"),
    ]);
    let (said, stderr) = runs_newer(&api.server.url(), RUNS, ASKED, &[]);
    assert_eq!(said, "keep");
    assert_eq!(api.requests().len(), 6);
    assert!(
        stderr.contains("GitHub answered 429, asking again in 1s"),
        "{stderr}"
    );
}

/// When GitHub cannot say, at any of the three reads, the Runner that runs stays.
#[test]
fn what_github_cannot_say_keeps_the_runner_that_runs() {
    let not_found = Reply::status_json(404, &serde_json::json!({ "message": "Not Found" }));
    for answers in [
        vec![Reply::empty(503)],
        vec![not_found.clone()],
        vec![Reply::text(200, "<html>a proxy</html>")],
        vec![compared("sideways")],
        vec![compared("ahead"), Reply::empty(503)],
        vec![
            compared("ahead"),
            Reply::json(&serde_json::json!({ "object": { "sha": "main" } })),
        ],
        vec![compared("ahead"), main_at(), Reply::empty(503)],
        vec![compared("ahead"), main_at(), not_found],
    ] {
        let api = api(answers.clone());
        let (said, _) = runs_newer(
            &api.server.url(),
            RUNS,
            ASKED,
            &[("GITHUB_TRIES", "2"), ("GITHUB_WAIT_MAX", "1")],
        );
        assert_eq!(said, "unknown", "{answers:?}");
    }
}

/// A ref that names no whole commit, such as a tag's or a short name, is not compared with: main is not known.
#[test]
fn a_main_that_names_no_whole_commit_is_not_compared_with() {
    for sha in ["main", &MAIN[..12], &MAIN.to_uppercase(), ""] {
        let api = api(vec![
            compared("ahead"),
            Reply::json(&serde_json::json!({ "object": { "sha": sha } })),
            compared("ahead"),
        ]);
        let (said, _) = runs_newer(&api.server.url(), RUNS, ASKED, &[]);
        assert_eq!(said, "unknown", "{sha:?}");
        assert_eq!(api.requests().len(), 2, "{sha:?}");
    }
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

const OWNED: &str = r#"{"machine": "lon1", "class": null, "sandbox": null, "engine": null, "state_dir": "/s", "build_folder": null, "image": "i1",
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
fn a_host_moved_from_containers_to_machines_loses_its_engine() {
    let container = OWNED.replace(
        r#""sandbox": null, "engine": null"#,
        r#""sandbox": "container", "engine": "/run/user/1001/podman/podman.sock""#,
    );
    let now = merged("{}", "{}", &container, "{}").expect("a container host");
    assert_eq!(
        (&now["sandbox"], &now["engine"]),
        (
            &serde_json::json!("container"),
            &serde_json::json!("/run/user/1001/podman/podman.sock")
        )
    );
    let vm = merged(&now.to_string(), "{}", OWNED, "{}").expect("a machine host");
    assert!(vm.get("sandbox").is_none() && vm.get("engine").is_none(), "{vm}");
    let refused = merged("{}", "{}", OWNED, r#"{"engine": "/x.sock"}"#).expect_err("refused");
    assert!(
        refused.contains("--settings: engine only this script sets"),
        "{refused}"
    );
}

/// `kept_image` of the script, sourced: the image an enrolled host keeps of the one configured, for this prefix.
fn kept(configured: &str, prefix: &str) -> String {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../scripts/host-setup.sh");
    let output = Command::new("bash")
        .args([
            "-c",
            "source \"$1\" && kept_image \"$2\" \"$3\"",
            "bash",
            script,
            configured,
            prefix,
        ])
        .output()
        .expect("bash runs");
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).expect("UTF-8")
}

#[test]
fn an_enrolled_host_keeps_its_image_only_while_its_sandbox_runs_that_kind() {
    assert_eq!(
        kept("amd64-image-2026.11.12-1", "amd64-image-"),
        "amd64-image-2026.11.12-1\n"
    );
    assert_eq!(
        kept("container-image-2026.11.12-1", "container-image-"),
        "container-image-2026.11.12-1\n"
    );
    assert_eq!(
        kept("amd64-image-2026.11.12-1", "amd64-container-image-"),
        "",
        "moved to containers"
    );
    assert_eq!(
        kept("container-image-2026.11.12-1", "image-"),
        "",
        "moved to machines"
    );
    assert_eq!(kept("", "image-"), "", "none configured");
}

/// `modules_wanted` of the script, sourced: what a container host still needs of `modules`, given its boot list's
/// text (none: no file) and the modules loaded now.
fn wanted(boot_list: Option<&str>, loaded: &[&str], modules: &[&str]) -> String {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../scripts/host-setup.sh");
    let dir = tempfile::tempdir().expect("a folder");
    let list = dir.path().join("codefusion-runner.conf");
    if let Some(text) = boot_list {
        std::fs::write(&list, text).expect("the boot list");
    }
    let sys = dir.path().join("module");
    for module in loaded {
        std::fs::create_dir_all(sys.join(module)).expect("a loaded module");
    }
    let output = Command::new("bash")
        .args([
            "-c",
            "source \"$1\" && shift && modules_wanted \"$@\"",
            "bash",
            script,
        ])
        .arg(&list)
        .arg(&sys)
        .args(modules)
        .output()
        .expect("bash runs");
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).expect("UTF-8")
}

#[test]
fn a_container_host_loads_the_firewalls_modules_now_and_at_every_boot() {
    let modules = ["nf_tables", "nft_ct"];
    assert_eq!(
        wanted(None, &[], &modules),
        "boot nf_tables\nnow nf_tables\nboot nft_ct\nnow nft_ct\n",
        "a new host"
    );
    assert_eq!(
        wanted(Some("nf_tables\nnft_ct\n"), &["nf_tables", "nft_ct"], &modules),
        "",
        "a host that has them"
    );
    assert_eq!(
        wanted(Some("nf_tables\n"), &["nf_tables"], &modules),
        "boot nft_ct\nnow nft_ct\n",
        "a host with one of them"
    );
    assert_eq!(
        wanted(Some("# nft_ct\nnft_ct_extra\n"), &["nf_tables"], &["nft_ct"]),
        "boot nft_ct\nnow nft_ct\n",
        "a comment or a longer name is not the module"
    );
    assert_eq!(wanted(None, &[], &[]), "", "no modules asked for");
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
        (
            vec!["enroll", "--sandbox", "container"],
            "enroll takes --sandbox from the configuration",
        ),
        (
            [&["install"][..], &base, &["--sandbox", "chroot"]].concat(),
            "--sandbox: vm or container",
        ),
        (
            [
                &["install"][..],
                &base,
                &["--sandbox", "container", "--class", "trusted"],
            ]
            .concat(),
            "--sandbox: a trusted host runs its jobs in machines",
        ),
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
        [
            &["install"][..],
            &base,
            &[
                "--sandbox",
                "container",
                "--version",
                "0.1.0-446125c0a1b2",
                "--sha256",
                SHA,
            ],
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

/// `scripts/release-gate.sh` against the repository's API at `api`, for `commit`: its status, standard output and
/// standard error.
fn release_gate(api: &str, commit: &str, env: &[(&str, &str)]) -> (Option<i32>, String, String) {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../scripts/release-gate.sh");
    let mut command = Command::new("bash");
    command.args([script, api, commit]).env_remove("GITHUB_TOKEN");
    for (name, value) in env {
        command.env(name, value);
    }
    let out = command.output().expect("bash runs");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

const PUSHED: &str = "dddddddddddd0000000000000000000000000000";

/// A run of a workflow as GitHub lists it, of the push of `PUSHED` to main unless changed.
fn run(id: u64, number: u64, status: &str, conclusion: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "id": id, "run_number": number, "run_attempt": 1, "status": status, "conclusion": conclusion,
        "head_sha": PUSHED, "event": "push", "head_branch": "main",
    })
}

fn runs(list: &[serde_json::Value]) -> Reply {
    Reply::json(&serde_json::json!({ "total_count": list.len(), "workflow_runs": list }))
}

fn green(id: u64) -> Reply {
    runs(&[run(id, 1, "completed", Some("success"))])
}

/// The reads before the runs: main's ref, and main has the commit.
fn on_main() -> Vec<Reply> {
    vec![main_at(), compared("ahead")]
}

fn gate_says(answers: Vec<Reply>) -> (String, usize) {
    let api = api(answers);
    let (status, said, stderr) = release_gate(&api.server.url(), PUSHED, &[]);
    assert_eq!(status, Some(0), "{said} {stderr}");
    (said, api.requests().len())
}

/// A push whose CI, Image when it made one, and Runner build are green gets its release, with that build's programs.
#[test]
fn a_green_push_of_main_is_released_with_its_build() {
    let api = api([on_main(), vec![green(10), runs(&[]), green(30)]].concat());
    let (status, said, stderr) = release_gate(&api.server.url(), PUSHED, &[]);
    assert_eq!((status, said.as_str()), (Some(0), "publish 30"), "{stderr}");
    let requests = api.requests();
    let asked: Vec<&str> = requests.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(
        asked,
        [
            "/git/ref/heads/main",
            &format!("/compare/{PUSHED}...{MAIN}"),
            "/actions/workflows/ci.yml/runs",
            "/actions/workflows/image.yml/runs",
            "/actions/workflows/runner-build.yml/runs",
        ]
    );
    for request in &requests[2..] {
        for pair in [("head_sha", PUSHED), ("event", "push"), ("branch", "main")] {
            assert!(
                request.query.contains(&(pair.0.to_string(), pair.1.to_string())),
                "{:?}",
                request.query
            );
        }
    }
    // Main at the commit itself, and an Image run that is green.
    let (said, _) = gate_says(vec![
        main_at(),
        compared("identical"),
        green(10),
        green(20),
        green(30),
    ]);
    assert_eq!(said, "publish 30");
}

/// The red commit that went out before: a run that ended other than green keeps its commit's release back, as does
/// a commit main does not have.
#[test]
fn a_push_with_a_red_run_or_off_main_gets_no_release() {
    let red = |conclusion| runs(&[run(9, 1, "completed", Some(conclusion))]);
    for (answers, starts) in [
        (vec![main_at(), compared("behind")], "skip "),
        (vec![main_at(), compared("diverged")], "skip "),
        (
            [on_main(), vec![red("failure")]].concat(),
            "skip CI run 9 ended failure",
        ),
        (
            [on_main(), vec![red("cancelled")]].concat(),
            "skip CI run 9 ended cancelled",
        ),
        (
            [on_main(), vec![red("timed_out")]].concat(),
            "skip CI run 9 ended timed_out",
        ),
        (
            [on_main(), vec![green(10), red("failure")]].concat(),
            "skip Image run 9",
        ),
        (
            [on_main(), vec![green(10), runs(&[]), red("failure")]].concat(),
            "skip Runner build run 9",
        ),
    ] {
        let (said, _) = gate_says(answers);
        assert!(said.starts_with(starts), "{said}");
    }
}

/// A run not ended, or not started, makes it wait: the workflow_run of the run that ends starts the check again.
#[test]
fn a_push_whose_runs_have_not_ended_waits() {
    let going = |status| runs(&[run(9, 1, status, None)]);
    for (answers, says) in [
        (
            [on_main(), vec![going("in_progress")]].concat(),
            "wait CI run 9 is in_progress",
        ),
        (
            [on_main(), vec![going("queued")]].concat(),
            "wait CI run 9 is queued",
        ),
        (
            [on_main(), vec![runs(&[])]].concat(),
            "wait CI has not run for this push",
        ),
        (
            [on_main(), vec![green(10), going("in_progress")]].concat(),
            "wait Image run 9 is in_progress",
        ),
        (
            [on_main(), vec![green(10), runs(&[]), runs(&[])]].concat(),
            "wait Runner build has not run for this push",
        ),
    ] {
        let (said, _) = gate_says(answers);
        assert_eq!(said, says);
    }
}

/// The newest run of a workflow counts, as on the console: a re-run that went green releases, a newer red one does
/// not; runs of other commits, events or branches in the answer are none of this push's.
#[test]
fn the_newest_run_of_the_push_counts() {
    let old_red = run(9, 4, "completed", Some("failure"));
    let new_green = run(10, 5, "completed", Some("success"));
    let mut rerun_green = run(9, 4, "completed", Some("success"));
    rerun_green["run_attempt"] = 2.into();
    let mut first_attempt_red = run(9, 4, "completed", Some("failure"));
    first_attempt_red["run_attempt"] = 1.into();
    for (list, says) in [
        (vec![old_red.clone(), new_green.clone()], "publish 30"),
        (vec![new_green.clone(), old_red.clone()], "publish 30"),
        (vec![first_attempt_red, rerun_green], "publish 30"),
        (
            vec![run(11, 6, "completed", Some("failure")), new_green.clone()],
            "skip CI run 11 ended failure",
        ),
    ] {
        let (said, _) = gate_says([on_main(), vec![runs(&list), runs(&[]), green(30)]].concat());
        assert_eq!(said, says, "{list:?}");
    }
    for (field, value) in [
        ("head_sha", "eeeeeeeeeeee0000000000000000000000000000"),
        ("event", "pull_request"),
        ("head_branch", "main-copy"),
    ] {
        let mut other = run(12, 7, "completed", Some("success"));
        other[field] = value.into();
        let (said, _) = gate_says([on_main(), vec![runs(&[other])]].concat());
        assert_eq!(said, "wait CI has not run for this push", "{field}");
    }
}

/// GitHub's rate limit and server errors are waited out; when GitHub cannot say, the gate fails rather than release.
#[test]
fn the_gate_waits_out_429_and_5xx_and_fails_when_github_cannot_say() {
    let (said, asked) = gate_says(vec![
        rate_limited(429).with_header("Retry-After", "1"),
        main_at(),
        Reply::empty(502),
        compared("ahead"),
        green(10),
        Reply::empty(503),
        runs(&[]),
        green(30),
    ]);
    assert_eq!((said.as_str(), asked), ("publish 30", 8));

    let limits = [("GITHUB_TRIES", "2"), ("GITHUB_WAIT_MAX", "1")];
    for answers in [
        vec![Reply::empty(503)],
        vec![Reply::json(&serde_json::json!({ "object": { "sha": "main" } }))],
        vec![main_at(), compared("sideways")],
        vec![main_at(), Reply::status_json(404, &serde_json::json!({ "message": "Not Found" }))],
        [on_main(), vec![Reply::empty(503)]].concat(),
        [on_main(), vec![Reply::text(200, "<html>a proxy</html>")]].concat(),
        [on_main(), vec![runs(&[serde_json::json!({ "id": "x", "run_number": 1, "run_attempt": 1, "status": "completed", "conclusion": "success", "head_sha": PUSHED, "event": "push", "head_branch": "main" })])]].concat(),
        [on_main(), vec![green(10), runs(&[]), Reply::empty(500)]].concat(),
    ] {
        let api = api(answers.clone());
        let (status, said, _) = release_gate(&api.server.url(), PUSHED, &limits);
        assert_ne!(status, Some(0), "{answers:?}");
        assert!(!said.starts_with("publish"), "{said}");
    }
}

#[test]
fn the_gate_takes_only_a_whole_commit() {
    let api = api(vec![main_at()]);
    for commit in [
        "",
        "main",
        &PUSHED[..12],
        &PUSHED.to_uppercase(),
        &format!("{PUSHED}0"),
        "../../x",
    ] {
        let (status, said, _) = release_gate(&api.server.url(), commit, &[]);
        assert!(status.is_some_and(|code| code != 0), "{commit:?}");
        assert_eq!(said, "", "{commit:?}");
    }
    assert_eq!(api.requests().len(), 0);
}
