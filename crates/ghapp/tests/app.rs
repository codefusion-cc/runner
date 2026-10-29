//! The GitHub App client against a fake GitHub at the HTTP edge. The Go manager's `ghapp_test.go` is the model;
//! `// Go:` comments name the test each one ports, `// added:` the ones the Go code had no test for.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cf_fake_github::harness::{TestClock, elsewhere, fast};
use cf_fake_github::httpd::Reply;
use cf_fake_github::{FakeGithub, Options, test_key};
use cf_ghapp::{
    AppCredentials, AppKey, Clock, Error, GitHubApp, Http, Installation, JOBS_PAGE, MAX_RUN_SETS,
    RUNS_LISTED, RetryPolicy, WAITING_RUNS, Waiting,
};
use cf_secret::Secret;
use cf_workflows::{LabelSet, MAX_LABEL_BYTES, MAX_SET_LABELS};

struct Harness {
    fake: FakeGithub,
    app: GitHubApp,
    clock: Arc<TestClock>,
}

/// The largest workflow file the tests' caller reads, as the Runner's own limit is.
const FILE_LIMIT: u64 = 512 * 1024;

async fn harness_with(retry: RetryPolicy, timeout: Duration) -> Harness {
    let fake = FakeGithub::start(Options::default()).await;
    fake.add_repo("org/app", true);
    fake.add_repo("org/other", false);
    let clock = Arc::new(TestClock::default());
    let http = Http::new(timeout, retry).with_max_body(64 * 1024);
    let app = cf_fake_github::harness::app(&fake, http, clock.clone());
    Harness { fake, app, clock }
}

async fn harness() -> Harness {
    harness_with(fast(), Duration::from_secs(10)).await
}

fn claims_of(authorization: &str) -> serde_json::Value {
    let jwt = authorization.strip_prefix("Bearer ").expect("bearer");
    let payload = jwt.split('.').nth(1).expect("payload");
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).expect("base64")).expect("json")
}

// Go: TestNewRejectsBadKey
#[test]
fn a_key_that_is_not_pem_is_refused_without_echoing_it() {
    let err = AppKey::from_pem(&Secret::new("not a key SECRETMARKER".into()))
        .unwrap_err()
        .to_string();
    assert!(!err.contains("SECRETMARKER"), "{err}");
    let truncated = test_key().private_pem[..200].to_string();
    assert!(AppKey::from_pem(&Secret::new(truncated)).is_err());
    let wrong_label = test_key().private_pem.replace("RSA PRIVATE KEY", "CERTIFICATE");
    assert!(AppKey::from_pem(&Secret::new(wrong_label)).is_err());
}

// added: the key and the tokens never show
#[tokio::test]
async fn the_key_and_tokens_never_show_in_debug_output_or_errors() {
    let h = harness().await;
    let key = AppKey::from_pem(&Secret::new(test_key().private_pem.clone())).unwrap();
    assert_eq!(format!("{key:?}"), "AppKey(..)");
    let body_line = test_key().private_pem.lines().nth(2).unwrap().to_string();
    let shown = format!(
        "{:?} {:?}",
        h.app,
        AppCredentials {
            client_id: "x".into(),
            key
        }
    );
    assert!(!shown.contains(&body_line));
    let token = h.app.installation_token(1).await.unwrap();
    let error = h.app.installation_token(99).await.unwrap_err();
    for text in [error.to_string(), format!("{error:?}")] {
        assert!(
            !text.contains(token.expose().as_str()) && !text.contains(&body_line),
            "{text}"
        );
    }
    assert_eq!(format!("{token:?}"), "Secret(..)");
    let jwt = h.app.jwt().unwrap();
    assert!(!format!("{jwt:?} {:?}", h.app).contains(jwt.expose().as_str()));
}

// Go: TestJWTIsWhatGitHubAccepts
#[tokio::test]
async fn the_jwt_is_what_github_accepts() {
    let h = harness().await;
    h.app
        .installations()
        .await
        .expect("the fake checks signature, issuer, iat and exp as GitHub does");
    let seen = h
        .fake
        .requests()
        .into_iter()
        .find(|r| r.line == "GET /app/installations")
        .unwrap();
    let claims = claims_of(&seen.authorization.unwrap());
    assert_eq!(claims["iss"], "Iv23test");
    let (iat, exp) = (claims["iat"].as_i64().unwrap(), claims["exp"].as_i64().unwrap());
    assert!(
        exp - iat <= 600,
        "GitHub refuses tokens that live longer than ten minutes"
    );
    let now = h
        .clock
        .now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!(
        iat < now,
        "issued in the past: a clock a little ahead of GitHub's makes a token issued now invalid"
    );
}

// Go: TestInstallations
#[tokio::test]
async fn installations_are_listed_with_the_apps_own_token() {
    let h = harness().await;
    assert_eq!(h.app.installations().await.unwrap(), vec![Installation { id: 1 }]);
}

// Go: TestTokenIsReusedUntilNearExpiry
#[tokio::test]
async fn a_token_is_reused_until_five_minutes_before_it_expires() {
    let h = harness().await;
    let first = h.app.installation_token(1).await.unwrap();
    h.clock.advance(Duration::from_secs(54 * 60));
    h.fake.set_clock_offset(54 * 60);
    let again = h.app.installation_token(1).await.unwrap();
    assert_eq!(
        again.expose(),
        first.expose(),
        "replaced a token with 6 minutes left"
    );
    assert_eq!(h.fake.count("POST /app/installations/1/access_tokens"), 1);

    h.clock.advance(Duration::from_secs(2 * 60));
    h.fake.set_clock_offset(56 * 60);
    let fresh = h.app.installation_token(1).await.unwrap();
    assert_ne!(fresh.expose(), first.expose(), "kept a token with 4 minutes left");
}

// Go: TestTokenErrors
#[tokio::test]
async fn token_errors_are_results_and_are_not_cached() {
    let h = harness().await;
    h.fake.fail(
        "access_tokens",
        Reply::status_json(401, &serde_json::json!({"message": "Bad credentials"})),
        1,
    );
    let err = h.app.installation_token(1).await.unwrap_err();
    assert_eq!(err.status(), Some(401));
    assert!(err.to_string().contains("Bad credentials"), "{err}");

    h.fake.fail(
        "access_tokens",
        Reply::status_json(
            201,
            &serde_json::json!({"token": "", "expires_at": "2030-01-01T00:00:00Z"}),
        ),
        1,
    );
    assert!(
        matches!(h.app.installation_token(1).await, Err(Error::Decode { .. })),
        "accepted an empty token"
    );

    let body = br#"{"token":"abc","expires_at":"2030-01-01T00:00:00Z"}"#.to_vec();
    h.fake.fail(
        "access_tokens",
        Reply::Truncated {
            status: 201,
            body,
            sent: 10,
        },
        3,
    );
    assert!(
        matches!(h.app.installation_token(1).await, Err(Error::Transport(_))),
        "accepted a cut-off response"
    );

    h.fake.fail(
        "access_tokens",
        Reply::status_json(201, &serde_json::json!({"token": "abc"})),
        1,
    );
    assert!(
        matches!(h.app.installation_token(1).await, Err(Error::Decode { .. })),
        "a token without expires_at"
    );

    // None of the failures is cached: the next call asks again and gets a token.
    let before = h.fake.count("access_tokens");
    h.app.installation_token(1).await.expect("recovers");
    assert_eq!(h.fake.count("access_tokens"), before + 1);
}

// Go: TestRepositoriesFollowsPages
#[tokio::test]
async fn repositories_follow_next_pages() {
    let h = harness().await;
    let url = h.fake.url();
    let page = |name: &str| {
        Reply::json(&serde_json::json!({"repositories": [{"full_name": name, "private": true}]}))
    };
    let link = format!("<{url}/installation/repositories?page=2>; rel=\"next\", <{url}/x>; rel=\"last\"");
    h.fake.fail(
        "GET /installation/repositories",
        page("org/one").with_header("link", &link),
        1,
    );
    h.fake.fail("GET /installation/repositories", page("org/two"), 1);
    let repos = h.app.repositories(1).await.unwrap();
    assert_eq!(
        repos.iter().map(|r| r.full_name.as_str()).collect::<Vec<_>>(),
        ["org/one", "org/two"]
    );
    assert!(repos[0].private);
}

// Go: TestPagesNeverLeaveTheAPI
#[tokio::test]
async fn a_next_page_on_another_host_is_never_followed_with_the_token() {
    let h = harness().await;
    let (evil, visits) = elsewhere().await;
    let link = format!("<{}/installation/repositories?page=2>; rel=\"next\"", evil.url());
    let page =
        Reply::json(&serde_json::json!({"repositories": [{"full_name": "a/b"}]})).with_header("link", &link);
    h.fake.fail("GET /installation/repositories", page, 1);
    let repos = h.app.repositories(1).await.unwrap();
    assert_eq!(repos.len(), 1);
    assert!(
        visits.load(Ordering::SeqCst) == 0,
        "followed a next page to another host with the token"
    );
}

#[tokio::test]
async fn a_listing_that_names_a_next_page_for_ever_is_refused() {
    let h = harness().await;
    let link = format!("<{}/app/installations?page=2>; rel=\"next\"", h.fake.url());
    let page =
        Reply::json(&serde_json::json!([{"id": 1, "account": {"login": "org"}}])).with_header("link", &link);
    h.fake.fail("GET /app/installations", page, 150);
    let error = h.app.installations().await.unwrap_err();
    assert!(matches!(error, Error::Refused(_)), "{error:?}");
    assert_eq!(h.fake.count("GET /app/installations"), 100);
}

// Go: TestWorkflowFiles
#[tokio::test]
async fn workflow_files_are_fetched_only_when_their_content_changed() {
    let h = harness().await;
    h.fake.set_workflow(
        "org/app",
        "ci.yml",
        &format!("jobs: {{}}\n{}", "# long line\n".repeat(20)),
    );
    h.fake.set_workflow("org/app", "release.yaml", "on: push\n");
    h.fake.set_workflow("org/app", "README.md", "not a workflow");
    let files = h.app.workflow_files(1, "org/app", FILE_LIMIT).await.unwrap();
    assert_eq!(files.len(), 2, "only workflow files: {:?}", files.keys());
    assert!(String::from_utf8_lossy(&files["ci.yml"]).starts_with("jobs: {}\n# long line"));
    assert_eq!(files["release.yaml"], b"on: push\n");
    assert_eq!(h.fake.count("git/blobs"), 2);

    assert_eq!(
        h.app
            .workflow_files(1, "org/app", FILE_LIMIT)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        h.fake.count("git/blobs"),
        2,
        "an unchanged file was fetched again"
    );

    h.fake.set_workflow("org/app", "release.yaml", "changed\n");
    let files = h.app.workflow_files(1, "org/app", FILE_LIMIT).await.unwrap();
    assert_eq!(files["release.yaml"], b"changed\n");
    assert_eq!(h.fake.count("git/blobs"), 3, "only the changed file is fetched");
}

// Go: TestWorkflowFiles (no folder, and a failing API)
#[tokio::test]
async fn no_workflow_folder_is_empty_and_a_failing_api_is_an_error() {
    let h = harness().await;
    assert!(
        h.app
            .workflow_files(1, "org/other", FILE_LIMIT)
            .await
            .unwrap()
            .is_empty()
    );
    h.fake.set_workflow("org/app", "ci.yml", "jobs: {}\n");
    h.fake
        .fail("contents/.github/workflows", Reply::text(502, "boom"), 10);
    let err = h.app.workflow_files(1, "org/app", FILE_LIMIT).await.unwrap_err();
    assert_eq!(
        err.status(),
        Some(502),
        "a 502 read as no workflows would drop the repository's runners"
    );
}

// Go: TestWorkflowFilesRejectsUnknownEncoding
#[tokio::test]
async fn content_in_an_unknown_encoding_is_refused() {
    let h = harness().await;
    h.fake.set_workflow("org/app", "ci.yml", "jobs: {}\n");
    h.fake.fail(
        "git/blobs",
        Reply::json(&serde_json::json!({"content": "jobs: {}", "encoding": "utf-8"})),
        1,
    );
    assert!(matches!(
        h.app.workflow_files(1, "org/app", FILE_LIMIT).await,
        Err(Error::Decode { .. })
    ));
}

// added: what an answer may cost and where a request may go

#[tokio::test]
async fn an_answer_longer_than_the_limit_is_refused_and_not_asked_for_again() {
    let h = harness().await;
    let long = "x".repeat(70 * 1024);
    h.fake.fail(
        "GET /installation/repositories",
        Reply::json(&serde_json::json!({"repositories": [], "padding": long})),
        5,
    );
    let err = h.app.repositories(1).await.unwrap_err();
    assert!(matches!(err, Error::Refused(_)), "{err:?}");
    assert!(!err.is_transient());
    assert!(err.to_string().contains("65536 bytes"), "{err}");
    assert_eq!(h.fake.count("GET /installation/repositories"), 1);
}

#[tokio::test]
async fn an_answer_that_does_not_say_its_length_is_cut_off_at_the_limit() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // A server that never stops sending: no Content-Length, the body runs until the connection closes.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let sent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = sent.clone();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 4096];
        let _ = stream.read(&mut request).await;
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n")
            .await;
        let chunk = [b' '; 8192];
        while stream.write_all(&chunk).await.is_ok() {
            counter.fetch_add(chunk.len(), Ordering::SeqCst);
        }
    });
    let key = AppKey::from_pem(&Secret::new(test_key().private_pem.clone())).unwrap();
    let app = GitHubApp::new(
        "Iv23test",
        key,
        &format!("http://{address}"),
        Http::new(Duration::from_secs(5), fast()).with_max_body(64 * 1024),
        Arc::new(TestClock::default()),
    );
    let started = std::time::Instant::now();
    let err = app.installations().await.unwrap_err();
    assert!(matches!(err, Error::Refused(_)), "{err:?}");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "read until the time limit instead of stopping at the size limit"
    );
    assert!(sent.load(Ordering::SeqCst) < 8 * 1024 * 1024);
}

#[tokio::test]
async fn a_redirect_to_another_host_is_not_followed() {
    let h = harness().await;
    let (evil, visits) = elsewhere().await;
    h.fake.fail(
        "GET /installation/repositories",
        Reply::empty(302).with_header("location", &format!("{}/installation/repositories", evil.url())),
        1,
    );
    let err = h.app.repositories(1).await.unwrap_err();
    assert_eq!(err.status(), Some(302), "{err:?}");
    assert!(
        visits.load(Ordering::SeqCst) == 0,
        "a redirect sent the request to another host"
    );
}

#[tokio::test]
async fn a_redirect_on_the_same_host_is_followed() {
    // What a renamed repository answers with.
    let h = harness().await;
    h.fake.fail(
        "GET /installation/repositories",
        Reply::empty(301).with_header(
            "location",
            &format!("{}/installation/repositories?moved=1", h.fake.url()),
        ),
        1,
    );
    assert_eq!(h.app.repositories(1).await.unwrap().len(), 2);
    assert_eq!(h.fake.count("GET /installation/repositories"), 2);
}

#[tokio::test]
async fn a_token_never_goes_out_over_plain_http() {
    let h = harness().await;
    let key = AppKey::from_pem(&Secret::new(test_key().private_pem.clone())).unwrap();
    // A name that is not this machine: nothing is even looked up.
    let app = GitHubApp::new(
        "Iv23test",
        key,
        "http://api.github.example",
        Http::new(Duration::from_secs(5), fast()),
        h.clock.clone(),
    );
    let err = app.installations().await.unwrap_err();
    assert!(matches!(err, Error::Refused(_)), "{err:?}");
    assert!(err.to_string().contains("api.github.example"), "{err}");
    for (address, safe) in [
        ("https://api.github.com/x", true),
        ("http://127.0.0.1:8080/x", true),
        ("http://[::1]:8080/x", true),
        ("http://localhost/x", true),
        ("http://api.github.com/x", false),
        ("http://127.0.0.1.example.com/x", false),
        ("http://localhost.example.com/x", false),
        ("ftp://127.0.0.1/x", false),
    ] {
        assert_eq!(
            cf_ghapp::is_safe_address(&address.parse().unwrap()),
            safe,
            "{address}"
        );
    }
}

#[tokio::test]
async fn a_workflow_file_too_long_to_be_one_is_not_fetched() {
    let h = harness().await;
    h.fake.set_workflow("org/app", "ci.yml", "jobs: {}\n");
    h.fake
        .set_workflow("org/app", "huge.yml", &"# padding\n".repeat(60 * 1024));
    let files = h.app.workflow_files(1, "org/app", FILE_LIMIT).await.unwrap();
    assert_eq!(files.keys().collect::<Vec<_>>(), ["ci.yml"]);
    assert_eq!(h.fake.count("git/blobs"), 1, "the long file was fetched");
}

// added: many callers refused at once
#[tokio::test]
async fn callers_refused_together_share_one_new_token() {
    let h = harness().await;
    h.app.installation_token(1).await.unwrap();
    h.fake.fail(
        "GET /installation/repositories",
        Reply::After(
            Duration::from_millis(100),
            Box::new(Reply::text(401, r#"{"message":"Bad credentials"}"#)),
        ),
        8,
    );
    let calls: Vec<_> = (0..8)
        .map(|_| {
            let app = h.app.clone();
            tokio::spawn(async move { app.repositories(1).await })
        })
        .collect();
    for call in calls {
        assert_eq!(call.await.unwrap().unwrap().len(), 2);
    }
    assert_eq!(
        h.fake.count("access_tokens"),
        2,
        "each refused caller bought a token of its own"
    );
}

// not ported: Go's TestLatestCommit, TestTarball and TestLocate* test the source lookups (commit, tarball, moved
// repositories). The Runner updates from signed releases and never reads a repository's source, so there is no
// counterpart. This is the repository-installation lookup they shared.
#[tokio::test]
async fn the_installation_of_a_repository_is_found_and_a_missing_one_is_a_404() {
    let h = harness().await;
    assert_eq!(h.app.repo_installation("org/app").await.unwrap(), 1);
    assert_eq!(
        h.app.repo_installation("org/gone").await.unwrap_err().status(),
        Some(404)
    );
}

// added: variables
#[tokio::test]
async fn variables_are_read_and_a_missing_permission_is_said() {
    let h = harness().await;
    h.fake.set_variable("org/app", "CI_RUNNER", "cf-4g");
    h.fake.set_variable("org/app", "CI_RUNNER_HEAVY", "cf-8g");
    let vars = h.app.variables(1, "org/app").await.unwrap();
    assert_eq!(vars.get("CI_RUNNER").map(String::as_str), Some("cf-4g"));
    assert_eq!(vars.len(), 2);

    let denied = serde_json::json!({"message": "Resource not accessible by integration"});
    h.fake.fail(
        "GET /repos/org/app/actions/variables",
        Reply::status_json(403, &denied),
        1,
    );
    let err = h.app.variables(1, "org/app").await.unwrap_err();
    assert_eq!(err.status(), Some(403));
    assert!(
        err.to_string().contains("not accessible"),
        "the caller must be able to say what is missing: {err}"
    );

    // Organization variables are best effort.
    h.fake.fail("organization-variables", Reply::text(403, "no"), 1);
    assert_eq!(h.app.variables(1, "org/app").await.unwrap().len(), 2);
}

// added: registration
#[tokio::test]
async fn a_registration_token_is_asked_with_the_installation_token() {
    let h = harness().await;
    let token = h.app.registration_token(1, "org/app").await.unwrap();
    assert!(token.expose().starts_with("AREG_"));
    assert_eq!(
        h.app
            .registration_token(1, "org/gone")
            .await
            .unwrap_err()
            .status(),
        Some(404)
    );
}

// added: 5xx is tried again, a refusal is not
#[tokio::test]
async fn server_errors_are_tried_again_and_refusals_are_not() {
    let h = harness().await;
    h.fake.fail("GET /app/installations", Reply::text(503, "down"), 1);
    h.app.installations().await.expect("one 503 is retried");
    assert_eq!(h.fake.count("GET /app/installations"), 2);

    h.fake
        .fail("GET /app/installations", Reply::text(500, "down"), 10);
    let err = h.app.installations().await.unwrap_err();
    assert_eq!(err.status(), Some(500));
    assert!(err.is_transient());
    assert_eq!(
        h.fake.count("GET /app/installations"),
        2 + 3,
        "one try and two retries"
    );

    for status in [401u16, 403, 404, 409] {
        let h = harness().await;
        h.fake
            .fail("GET /app/installations", Reply::text(status, "no"), 10);
        let err = h.app.installations().await.unwrap_err();
        assert_eq!(err.status(), Some(status));
        assert!(!err.is_transient(), "{status}");
        assert_eq!(
            h.fake.count("GET /app/installations"),
            1,
            "{status} was tried again"
        );
    }
}

// added: 429 with Retry-After
#[tokio::test]
async fn retry_after_is_waited_out_when_short_and_reported_when_long() {
    let h = harness().await;
    h.fake.fail(
        "GET /app/installations",
        Reply::text(429, "slow down").with_header("retry-after", "1"),
        1,
    );
    let started = Instant::now();
    h.app
        .installations()
        .await
        .expect("a short Retry-After is waited out");
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "did not wait the Retry-After: {:?}",
        started.elapsed()
    );
    assert_eq!(h.fake.count("GET /app/installations"), 2);

    h.fake.fail(
        "GET /app/installations",
        Reply::text(429, "slow down").with_header("retry-after", "3600"),
        1,
    );
    let started = Instant::now();
    let err = h.app.installations().await.unwrap_err();
    assert_eq!(err.status(), Some(429));
    assert_eq!(
        err.retry_after(),
        Some(Duration::from_secs(3600)),
        "the caller must learn the wait"
    );
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "waited out an hour"
    );
    assert!(err.is_transient());

    // A 403 is a refusal unless GitHub says when to come back (a secondary rate limit).
    h.fake.fail(
        "GET /app/installations",
        Reply::text(403, "limit").with_header("retry-after", "0"),
        1,
    );
    h.app
        .installations()
        .await
        .expect("403 with Retry-After is a rate limit");
}

// added: slow, cut-off and dropped responses
#[tokio::test]
async fn a_slow_cut_off_or_dropped_response_is_a_network_error() {
    let h = harness_with(fast(), Duration::from_millis(300)).await;
    h.fake.fail(
        "GET /app/installations",
        Reply::After(Duration::from_secs(2), Box::new(Reply::empty(200))),
        10,
    );
    let started = Instant::now();
    assert!(matches!(h.app.installations().await, Err(Error::Transport(_))));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the timeout is not applied"
    );

    let h = harness().await;
    let body = br#"[{"id":1},{"id":2}]"#.to_vec();
    h.fake.fail(
        "GET /app/installations",
        Reply::Truncated {
            status: 200,
            body,
            sent: 5,
        },
        10,
    );
    assert!(
        matches!(h.app.installations().await, Err(Error::Transport(_))),
        "a cut-off list read as a short list"
    );
    h.fake.fail("GET /app/installations", Reply::Drop, 10);
    // The Truncated faults above matched first and are used up by now or still queued; clear both with calls.
    for _ in 0..4 {
        let _ = h.app.installations().await;
    }
    h.fake.fail("GET /app/installations", Reply::Drop, 10);
    assert!(matches!(h.app.installations().await, Err(Error::Transport(_))));
}

#[tokio::test]
async fn an_answer_of_the_wrong_shape_keeps_its_text_out_of_the_error() {
    let h = harness().await;
    // serde names the value it met where it expected another: here that value is the token.
    for body in [
        r#""ghs_leaked""#,
        r#"{"token": {"ghs_leaked": 1}}"#,
        r#"{"token": "ghs_leaked""#,
    ] {
        h.fake.fail(
            "POST /app/installations/1/access_tokens",
            Reply::text(201, body),
            1,
        );
        let error = h.app.installation_token(1).await.unwrap_err();
        assert!(matches!(error, Error::Decode { .. }), "{error:?}");
        let said = format!("{error} {error:?}");
        assert!(!said.contains("ghs_leaked"), "{said}");
        assert!(
            said.contains("line 1"),
            "where the answer went wrong is gone too: {said}"
        );
    }
}

#[test]
fn an_answer_shown_for_debugging_shows_none_of_its_body() {
    let answer = cf_ghapp::Response {
        status: reqwest::StatusCode::CREATED,
        headers: reqwest::header::HeaderMap::new(),
        body: r#"{"token":"ghs_leaked"}"#.into(),
    };
    let shown = format!("{answer:?}");
    assert!(!shown.contains("ghs_leaked"), "{shown}");
    assert!(shown.contains("201"), "{shown}");
}

#[tokio::test]
async fn a_workflow_file_larger_than_its_listing_said_is_left_out() {
    let h = harness().await;
    h.fake.set_workflow("org/app", "ci.yml", "on: push\n");
    let long = base64::engine::general_purpose::STANDARD.encode("x".repeat(100));
    h.fake.fail(
        "GET /repos/org/app/git/blobs",
        Reply::json(&serde_json::json!({"content": long, "encoding": "base64"})),
        1,
    );
    let files = h.app.workflow_files(1, "org/app", 64).await.unwrap();
    assert!(files.is_empty(), "{:?}", files.keys());
    // By the listing alone it fits, and what fits is read.
    let files = h.app.workflow_files(1, "org/app", 64).await.unwrap();
    assert_eq!(files["ci.yml"], b"on: push\n");
}

// added: malformed bodies are results
#[tokio::test]
async fn malformed_json_is_a_decode_error_not_a_panic() {
    let h = harness().await;
    for body in [
        "",
        "not json",
        "{",
        "[1,2",
        r#"{"repositories": 5}"#,
        r#"{"repositories":[{"full_name":5}]}"#,
    ] {
        h.fake
            .fail("GET /installation/repositories", Reply::text(200, body), 1);
        assert!(
            matches!(h.app.repositories(1).await, Err(Error::Decode { .. })),
            "{body:?}"
        );
    }
}

// added: clock skew on the JWT
#[tokio::test]
async fn a_machine_clock_that_is_wrong_still_gets_a_token_github_accepts() {
    // Each of these is far enough off that GitHub refuses the first token: issued in its future, or already expired.
    for offset in [-1200i64, -120, 700, 3600] {
        let h = harness().await;
        h.fake.set_clock_offset(offset);
        h.app
            .installations()
            .await
            .unwrap_or_else(|e| panic!("clock {offset}s off: {e}"));
        let skew = h.app.clock_skew_secs();
        assert!((skew - offset).abs() <= 2, "learned {skew}s for {offset}s");
        // Learned once: the next call signs right.
        let before = h.fake.count("GET /app/installations");
        h.app.installations().await.unwrap();
        assert_eq!(h.fake.count("GET /app/installations"), before + 1);
    }
    // A little off is inside the margins and needs no correction.
    let h = harness().await;
    h.fake.set_clock_offset(30);
    h.app.installations().await.unwrap();
    assert_eq!(h.fake.count("GET /app/installations"), 1);
}

// added: a 401 that is not the clock
#[tokio::test]
async fn a_401_that_is_not_about_the_clock_is_not_tried_forever() {
    let h = harness().await;
    h.fake.set_clock_offset(3600);
    // A key GitHub does not know: every token is refused, even one signed by GitHub's time.
    let other = AppKey::from_pem(&Secret::new(test_key().private_pem.clone())).unwrap();
    let wrong = GitHubApp::new(
        "Iv23someoneelse",
        other,
        &h.fake.url(),
        Http::new(Duration::from_secs(5), fast()),
        h.clock.clone(),
    );
    let err = wrong.installations().await.unwrap_err();
    assert_eq!(err.status(), Some(401));
    assert!(
        h.fake.count("GET /app/installations") <= 2,
        "{}",
        h.fake.count("GET /app/installations")
    );
}

// added: a token GitHub revoked early
#[tokio::test]
async fn a_revoked_installation_token_is_replaced_once() {
    let h = harness().await;
    h.app.installation_token(1).await.unwrap();
    h.fake.fail(
        "GET /installation/repositories",
        Reply::text(401, r#"{"message":"Bad credentials"}"#),
        1,
    );
    assert_eq!(h.app.repositories(1).await.unwrap().len(), 2);
    assert_eq!(h.fake.count("access_tokens"), 2);

    h.fake
        .fail("GET /installation/repositories", Reply::text(401, "no"), 5);
    assert_eq!(h.app.repositories(1).await.unwrap_err().status(), Some(401));
    assert_eq!(h.fake.count("access_tokens"), 3, "one new token and no more");
}

// added: one token request for many callers
#[tokio::test]
async fn concurrent_callers_share_one_token_request() {
    let h = harness().await;
    let calls: Vec<_> = (0..16)
        .map(|_| {
            let app = h.app.clone();
            tokio::spawn(async move { app.installation_token(1).await })
        })
        .collect();
    let mut values = std::collections::HashSet::new();
    for call in calls {
        values.insert(call.await.unwrap().unwrap().expose().clone());
    }
    assert_eq!(values.len(), 1);
    assert_eq!(h.fake.count("access_tokens"), 1);
}

// added: where the key is read from
#[cfg(unix)]
#[test]
fn credentials_load_from_a_folder_and_a_key_others_can_read_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("app.json"),
        r#"{"client_id":"Iv23abc","other":1}"#,
    )
    .unwrap();
    let pem = dir.path().join("app.pem");
    std::fs::write(&pem, &test_key().private_pem).unwrap();
    std::fs::set_permissions(&pem, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = AppCredentials::load(dir.path()).unwrap_err().to_string();
    assert!(err.contains("0600") && err.contains("app.pem"), "{err}");
    assert!(!err.contains("BEGIN"));

    std::fs::set_permissions(&pem, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(AppCredentials::load(dir.path()).unwrap().client_id, "Iv23abc");

    std::fs::write(dir.path().join("app.json"), r#"{"client_id":""}"#).unwrap();
    assert!(
        AppCredentials::load(dir.path())
            .unwrap_err()
            .to_string()
            .contains("client_id")
    );
    std::fs::write(dir.path().join("app.json"), "{").unwrap();
    assert!(AppCredentials::load(dir.path()).is_err());
    std::fs::remove_file(&pem).unwrap();
    std::fs::write(dir.path().join("app.json"), r#"{"client_id":"Iv23abc"}"#).unwrap();
    assert!(AppCredentials::load(dir.path()).is_err());
}

/// The labels of what [`GitHubApp::waiting_jobs`] read, by job, sorted for comparing.
fn waiting(read: &Waiting) -> Vec<(i64, String)> {
    let mut jobs: Vec<(i64, String)> = read.jobs.iter().map(|j| (j.id, j.set.key())).collect();
    jobs.sort();
    jobs
}

/// A set's key, as the set of these labels reads it.
fn key(labels: &[&str]) -> String {
    LabelSet::new(labels).key()
}

// added: the jobs GitHub has not finished, from the REST view of the runs that are queued or under way.
#[tokio::test]
async fn waiting_jobs_are_the_unfinished_jobs_of_queued_and_running_runs() {
    let h = harness().await;
    let queued = h.fake.queue_job("org/app", &["self-hosted", "CF-2g"], "build");
    // A run under way: its first job runs elsewhere, its second waits for it.
    let run = h.fake.queue_run(
        "org/app",
        &[(&["ubuntu-latest"], "lint"), (&["self-hosted", "cf-4g"], "test")],
        "refs/pull/7/merge",
    );
    h.fake.take_job(run[0]);
    let over = h.fake.queue_job("org/app", &["self-hosted", "cf-8g"], "old");
    h.fake.end_job(over, "success");
    // A run under way whose first job is done: the done one is not waiting.
    let half = h.fake.queue_run(
        "org/app",
        &[
            (&["self-hosted", "cf-16g"], "done"),
            (&["self-hosted", "cf-2g"], "next"),
        ],
        "refs/heads/main",
    );
    h.fake.end_job(half[0], "success");
    h.fake
        .queue_job("org/other", &["self-hosted", "cf-6g"], "elsewhere");

    let read = h.app.waiting_jobs(1, "org/app").await.unwrap();
    assert_eq!(
        waiting(&read),
        [
            (queued, key(&["self-hosted", "cf-2g"])),
            (run[1], key(&["self-hosted", "cf-4g"])),
            (half[1], key(&["self-hosted", "cf-2g"])),
        ],
        "the sets served, as a set reads them; a job of GitHub's own labels, a finished job and another \
         repository's are not there"
    );
    assert!(read.jobs.iter().all(|j| !j.from_fork), "no run here is a fork's");
    assert!(read.left_out.is_empty(), "{:?}", read.left_out);
    assert!(
        read.jobs.iter().all(|j| j.created > 1_700_000_000),
        "when each was queued"
    );

    // Nothing waits: nothing, and no error.
    let h = harness().await;
    let read = h.app.waiting_jobs(1, "org/app").await.unwrap();
    assert!(read.jobs.is_empty() && read.left_out.is_empty());
}

// added: a look that finds nothing changed costs nothing of the rate limit: the listings name the ETag of their last
// answer, GitHub answers 304, a run that did not change since its jobs were read is not asked about again, and the
// last result stands.
#[tokio::test]
async fn an_unchanged_queue_is_answered_304_and_the_last_result_stands() {
    let h = harness().await;
    let first = h.fake.queue_job("org/app", &["self-hosted", "cf-2g"], "build");
    let before = h.app.waiting_jobs(1, "org/app").await.unwrap();
    assert_eq!(h.fake.not_modified(), 0);
    let requests = h.fake.requests().len();

    let again = h.app.waiting_jobs(1, "org/app").await.unwrap();
    let asked = h.fake.requests().len() - requests;
    assert_eq!(asked, 2, "the two listings, not the run's jobs");
    assert_eq!(h.fake.not_modified(), 2, "both 304");
    assert_eq!(waiting(&again), waiting(&before));
    assert_eq!(waiting(&again), [(first, key(&["self-hosted", "cf-2g"]))]);

    // A change is read: the new run in full, the old one still from its 304.
    let second = h.fake.queue_job("org/app", &["self-hosted", "cf-4g"], "more");
    let read = h.app.waiting_jobs(1, "org/app").await.unwrap();
    assert_eq!(
        waiting(&read),
        [
            (first, key(&["self-hosted", "cf-2g"])),
            (second, key(&["self-hosted", "cf-4g"]))
        ]
    );
    // A run that ends leaves.
    h.fake.end_job(first, "cancelled");
    let read = h.app.waiting_jobs(1, "org/app").await.unwrap();
    assert_eq!(waiting(&read), [(second, key(&["self-hosted", "cf-4g"]))]);

    // A run under way whose job ends is read again: the job that ended is gone, the one that waits stays.
    let run = h.fake.queue_run(
        "org/app",
        &[(&["self-hosted", "cf-6g"], "a"), (&["self-hosted", "cf-8g"], "b")],
        "refs/heads/main",
    );
    h.app.waiting_jobs(1, "org/app").await.unwrap();
    h.fake.end_job(run[0], "success");
    let read = h.app.waiting_jobs(1, "org/app").await.unwrap();
    assert_eq!(
        waiting(&read),
        [
            (second, key(&["self-hosted", "cf-4g"])),
            (run[1], key(&["self-hosted", "cf-8g"]))
        ]
    );

    // A queue forgotten is asked about without ETags.
    let h = harness().await;
    h.fake.queue_job("org/app", &["self-hosted", "cf-2g"], "build");
    h.app.waiting_jobs(1, "org/app").await.unwrap();
    h.app.forget_queues(|repo| repo != "org/app");
    h.app.waiting_jobs(1, "org/app").await.unwrap();
    assert_eq!(h.fake.not_modified(), 0);
    h.app.waiting_jobs(1, "org/app").await.unwrap();
    assert_eq!(h.fake.not_modified(), 2, "kept again once read");
}

// added: the App without "Actions: read" is refused, and the caller hears it to say what is missing.
#[tokio::test]
async fn a_missing_actions_permission_is_the_error() {
    let h = harness().await;
    h.fake.queue_job("org/app", &["self-hosted", "cf-2g"], "build");
    let denied = serde_json::json!({"message": "Resource not accessible by integration"});
    h.fake.fail("/actions/runs", Reply::status_json(403, &denied), 1);
    let err = h.app.waiting_jobs(1, "org/app").await.unwrap_err();
    assert_eq!(err.status(), Some(403));
    assert!(err.to_string().contains("not accessible"), "{err}");
    // On a private repository GitHub says 404 instead.
    h.fake.fail(
        "/actions/runs",
        Reply::status_json(404, &serde_json::json!({"message": "Not Found"})),
        1,
    );
    assert_eq!(
        h.app.waiting_jobs(1, "org/app").await.unwrap_err().status(),
        Some(404)
    );
    // And a refusal of a run's jobs is the error too, not a run without jobs.
    h.fake.fail("/jobs", Reply::status_json(403, &denied), 1);
    assert_eq!(
        h.app.waiting_jobs(1, "org/app").await.unwrap_err().status(),
        Some(403)
    );
    // A server error is an error, not an empty queue.
    h.fake.fail("/jobs", Reply::text(502, "bad gateway"), 10);
    assert!(h.app.waiting_jobs(1, "org/app").await.is_err());
}

// added: a run that went between its listing and the read of its jobs (deleted) is left out, not an error.
#[tokio::test]
async fn a_run_gone_before_its_jobs_were_read_is_left_out() {
    let h = harness().await;
    let gone = h.fake.queue_job("org/app", &["self-hosted", "cf-2g"], "a");
    let kept = h.fake.queue_job("org/app", &["self-hosted", "cf-4g"], "b");
    h.fake.fail(
        &format!("/runs/{}/jobs", 7000 + gone),
        Reply::status_json(404, &serde_json::json!({"message": "Not Found"})),
        1,
    );
    let read = h.app.waiting_jobs(1, "org/app").await.unwrap();
    assert_eq!(waiting(&read), [(kept, key(&["self-hosted", "cf-4g"]))]);
}

// added: what a busy or hostile repository can make the Runner read and keep is bounded: the runs read, the jobs of
// one, the labels of one job, the distinct sets kept of one run. What is left out is said.
#[tokio::test]
async fn the_runs_jobs_and_labels_read_are_bounded_and_what_is_left_out_is_said() {
    let h = harness().await;
    let mut ids = Vec::new();
    for n in 0..(WAITING_RUNS + 5) {
        ids.push(
            h.fake
                .queue_job("org/app", &["self-hosted", &format!("l{n}")], "build"),
        );
    }
    let before = h.fake.count("/jobs");
    let read = h.app.waiting_jobs(1, "org/app").await.unwrap();
    assert_eq!(
        h.fake.count("/jobs") - before,
        WAITING_RUNS,
        "only so many runs' jobs are read"
    );
    let oldest: Vec<i64> = ids[..WAITING_RUNS].to_vec();
    let mut got: Vec<i64> = read.jobs.iter().map(|j| j.id).collect();
    got.sort();
    assert_eq!(got, oldest, "the oldest runs: their jobs waited longest");
    assert_eq!(
        read.left_out,
        [format!(
            "{} runs waiting, the oldest {WAITING_RUNS} read",
            WAITING_RUNS + 5
        )]
    );
    let listings: Vec<String> = h
        .fake
        .requests()
        .into_iter()
        .filter(|r| r.line.ends_with("/actions/runs"))
        .filter_map(|r| r.query.into_iter().find(|(name, _)| name == "per_page"))
        .map(|(_, n)| n)
        .collect();
    assert_eq!(
        listings,
        [RUNS_LISTED.to_string(), RUNS_LISTED.to_string()],
        "each listing asks for as many runs as one page holds, so the oldest are among them"
    );

    // One run of more jobs than a page, of more distinct sets than are kept.
    let h = harness().await;
    let names: Vec<String> = (0..JOBS_PAGE + 20).map(|n| format!("l{n}")).collect();
    let jobs: Vec<Vec<&str>> = names.iter().map(|n| vec!["self-hosted", n.as_str()]).collect();
    let jobs: Vec<(&[&str], &str)> = jobs.iter().map(|labels| (labels.as_slice(), "matrix")).collect();
    h.fake.queue_run("org/app", &jobs, "refs/heads/main");
    let read = h.app.waiting_jobs(1, "org/app").await.unwrap();
    assert_eq!(read.jobs.len(), MAX_RUN_SETS, "distinct sets kept of one run");
    assert!(read.left_out.len() >= 2, "{:?}", read.left_out);

    // Labels written in another case or with spaces around are the same set.
    let h = harness().await;
    let ids = h.fake.queue_run(
        "org/app",
        &[
            (&["Self-Hosted", "CF-2g"], "a"),
            (&[" self-hosted", "cf-2g "], "b"),
            (&["self-hosted", "cf-2g"], "c"),
        ],
        "refs/heads/main",
    );
    let read = h.app.waiting_jobs(1, "org/app").await.unwrap();
    assert_eq!(read.jobs.iter().map(|j| j.id).collect::<Vec<_>>(), [ids[2]]);

    // A matrix of one set is one set, however many jobs.
    let h = harness().await;
    let jobs: Vec<(&[&str], &str)> = (0..50)
        .map(|_| (&["self-hosted", "cf-2g"][..], "matrix"))
        .collect();
    let ids = h.fake.queue_run("org/app", &jobs, "refs/heads/main");
    let read = h.app.waiting_jobs(1, "org/app").await.unwrap();
    assert_eq!(read.jobs.len(), 1);
    assert_eq!(
        read.jobs[0].id,
        *ids.iter().max().unwrap(),
        "the newest job of the set stands for it"
    );
    assert!(read.left_out.is_empty(), "{:?}", read.left_out);

    // A job of too many or too long labels is no set served: it is not kept, and needs no word.
    let h = harness().await;
    let mut many: Vec<String> = (0..MAX_SET_LABELS).map(|n| format!("l{n}")).collect();
    many.push("self-hosted".into());
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    let long = "x".repeat(MAX_LABEL_BYTES + 1);
    h.fake.queue_job("org/app", &many, "many");
    h.fake.queue_job("org/app", &["self-hosted", &long], "long");
    let fine = h.fake.queue_job("org/app", &["self-hosted", "cf-2g"], "fine");
    let read = h.app.waiting_jobs(1, "org/app").await.unwrap();
    assert_eq!(read.jobs.iter().map(|j| j.id).collect::<Vec<_>>(), [fine]);
    assert!(read.left_out.is_empty(), "{:?}", read.left_out);
}

// added: a run of a pull request from a fork says so: on a public repository its jobs are a stranger's code.
#[tokio::test]
async fn a_job_of_a_forks_pull_request_says_so() {
    let h = harness().await;
    let own = h.fake.queue_job("org/app", &["self-hosted", "cf-2g"], "own");
    let stranger = h.fake.queue_job("org/app", &["self-hosted", "cf-4g"], "stranger");
    h.fake.from_fork(stranger);
    let read = h.app.waiting_jobs(1, "org/app").await.unwrap();
    let forks: Vec<(i64, bool)> = read.jobs.iter().map(|j| (j.id, j.from_fork)).collect();
    assert_eq!(forks, [(own, false), (stranger, true)], "the oldest run first");
}

// added: an answer over the body limit is refused, never held, and a slow one gives up at the client's timeout.
#[tokio::test]
async fn a_giant_or_slow_answer_is_an_error() {
    let h = harness().await;
    // The harness reads at most 64 KiB: a run whose jobs name labels of 40 KiB each is over it.
    let big = "x".repeat(40 * 1024);
    h.fake.queue_run(
        "org/app",
        &[(&["self-hosted", &big], "a"), (&["self-hosted", &big], "b")],
        "refs/heads/main",
    );
    assert!(matches!(
        h.app.waiting_jobs(1, "org/app").await,
        Err(Error::Refused(_))
    ));

    let h = harness_with(fast(), Duration::from_millis(300)).await;
    h.fake.queue_job("org/app", &["self-hosted", "cf-2g"], "build");
    h.fake.fail(
        "/jobs",
        Reply::After(Duration::from_secs(5), Box::new(Reply::empty(200))),
        10,
    );
    let started = Instant::now();
    assert!(matches!(
        h.app.waiting_jobs(1, "org/app").await,
        Err(Error::Transport(_))
    ));
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
}

// added: a set's queued jobs, the oldest first, beside its newest unfinished one: a job under way and a finished one
// are not queued, and another set's queued job is its own.
#[tokio::test]
async fn a_sets_queued_jobs_are_named_the_oldest_first() {
    let h = harness().await;
    let run = h.fake.queue_run(
        "org/app",
        &[
            (&["self-hosted", "cf-2g"], "a"),
            (&["self-hosted", "cf-2g"], "b"),
            (&["self-hosted", "cf-2g"], "c"),
            (&["self-hosted", "cf-2g"], "d"),
            (&["self-hosted", "cf-4g"], "e"),
        ],
        "refs/heads/main",
    );
    h.fake.take_job(run[1]);
    h.fake.end_job(run[3], "success");
    let queued = |read: &cf_ghapp::Waiting, labels: &[&str]| -> Vec<i64> {
        let job = read
            .jobs
            .iter()
            .find(|j| j.set.key() == key(labels))
            .expect("the set");
        assert!(
            job.queued.windows(2).all(|w| w[0] < w[1]),
            "the oldest first: {:?}",
            job.queued
        );
        job.queued.iter().map(|&(_, id)| id).collect()
    };
    let read = h.app.waiting_jobs(1, "org/app").await.unwrap();
    assert_eq!(queued(&read, &["self-hosted", "cf-2g"]), [run[0], run[2]]);
    assert_eq!(queued(&read, &["self-hosted", "cf-4g"]), [run[4]]);

    // Nothing queued: the set waits on a job under way, and names none.
    h.fake.take_job(run[0]);
    h.fake.take_job(run[2]);
    let read = h.app.waiting_jobs(1, "org/app").await.unwrap();
    assert_eq!(queued(&read, &["self-hosted", "cf-2g"]), Vec::<i64>::new());
}

// added: an Actions runner for one job outside any scale set is registered with exactly the labels asked for; a name
// taken is refused with 409 and registers nothing more.
#[tokio::test]
async fn a_plain_actions_runner_is_registered_with_exactly_its_labels() {
    let h = harness().await;
    let labels = vec!["self-hosted".to_string(), "cf-2g".to_string()];
    let jit = h
        .app
        .generate_jit_config(1, "org/app", "mac-b-1", &labels)
        .await
        .unwrap();
    assert!(jit.id > 0);
    assert!(!jit.encoded.expose().is_empty());
    assert_eq!(
        h.fake.plain_runners(),
        [("mac-b-1".to_string(), "org/app".to_string(), labels.clone())]
    );
    let again = h
        .app
        .generate_jit_config(1, "org/app", "mac-b-1", &labels)
        .await
        .unwrap_err();
    assert_eq!(again.status(), Some(409));
    assert_eq!(h.fake.plain_runners().len(), 1);
    // Sent once: GitHub may have registered it although its answer failed, and a second try would meet the name.
    let path = "POST /repos/org/app/actions/runners/generate-jitconfig";
    let sent = h.fake.count(path);
    h.fake.fail(path, Reply::text(502, "bad gateway"), 1);
    let failed = h
        .app
        .generate_jit_config(1, "org/app", "mac-b-3", &labels)
        .await
        .unwrap_err();
    assert_eq!(failed.status(), Some(502));
    assert_eq!(h.fake.count(path), sent + 1);
    let unknown = h
        .app
        .generate_jit_config(1, "org/none", "mac-b-2", &labels)
        .await
        .unwrap_err();
    assert_eq!(unknown.status(), Some(404));
}
